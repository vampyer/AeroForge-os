// AeroForge NVMe driver (C++20, freestanding), behind the Driver Host Interface.
//
// Polling mode for now: one admin queue pair and one I/O queue pair per
// controller, completions found by watching the phase bit. Interrupts
// (MSI-X) and one queue pair per CPU come later.

#include "dhi.h"

namespace {

// Controller registers (NVMe 1.4, section 3.1).
constexpr uint32_t kRegCap  = 0x00;
constexpr uint32_t kRegCc   = 0x14;
constexpr uint32_t kRegCsts = 0x1C;
constexpr uint32_t kRegAqa  = 0x24;
constexpr uint32_t kRegAsq  = 0x28;
constexpr uint32_t kRegAcq  = 0x30;

constexpr uint8_t kOpCreateIoSq = 0x01;
constexpr uint8_t kOpCreateIoCq = 0x05;
constexpr uint8_t kOpIdentify   = 0x06;
constexpr uint8_t kOpFlush      = 0x00;
constexpr uint8_t kOpWrite      = 0x01;
constexpr uint8_t kOpRead       = 0x02;

constexpr uint32_t kQueueDepth = 64;
constexpr uint32_t kPageSize = 4096;
constexpr int kMaxControllers = 4;

struct Command {
    uint32_t cdw0;
    uint32_t nsid;
    uint64_t reserved;
    uint64_t mptr;
    uint64_t prp1;
    uint64_t prp2;
    uint32_t cdw10, cdw11, cdw12, cdw13, cdw14, cdw15;
};
static_assert(sizeof(Command) == 64);

struct Completion {
    uint32_t result;
    uint32_t reserved;
    uint16_t sq_head;
    uint16_t sq_id;
    uint16_t cid;
    uint16_t status;  // bit 0 = phase
};
static_assert(sizeof(Completion) == 16);

struct Queue {
    dhi_dma sq{}, cq{};
    uint16_t id = 0;
    uint16_t sq_tail = 0;
    uint16_t cq_head = 0;
    uint8_t phase = 1;
    uint16_t next_cid = 0;
};

class Controller {
public:
    int32_t init(const dhi_ops* ops, uint64_t bar0, dhi_block_info* info) {
        ops_ = ops;
        regs_ = static_cast<volatile uint8_t*>(ops->map_mmio(bar0, 0x4000));
        if (regs_ == nullptr) return -1;

        const uint64_t cap = read64(kRegCap);
        stride_ = 4u << ((cap >> 32) & 0xF);
        timeout_ms_ = static_cast<uint32_t>(((cap >> 24) & 0xFF) * 500);
        if (((cap >> 37) & 1) == 0) return fail("controller lacks the NVM command set");

        // Reset: clear CC.EN and wait for the controller to stop.
        write32(kRegCc, read32(kRegCc) & ~1u);
        if (!wait_ready(false)) return fail("timeout disabling controller");

        if (!make_queue(admin_, 0)) return fail("out of DMA memory");
        write32(kRegAqa, ((kQueueDepth - 1) << 16) | (kQueueDepth - 1));
        write64(kRegAsq, admin_.sq.phys);
        write64(kRegAcq, admin_.cq.phys);
        // 64-byte SQ entries, 16-byte CQ entries, 4 KiB pages, NVM command set, enable.
        write32(kRegCc, (6u << 16) | (4u << 20) | 1u);
        if (!wait_ready(true)) return fail("timeout enabling controller");

        dhi_dma page{};
        if (ops_->dma_alloc(kPageSize, &page) != 0) return fail("out of DMA memory");
        int32_t rc = identify(page, info);
        if (rc == 0) rc = create_io_queues();
        ops_->dma_free(&page);
        if (rc != 0) return rc;

        // PRP1 + PRP2 cover two pages, and MDTS may limit us further.
        max_transfer_ = 2 * kPageSize;
        if (mdts_ != 0 && (kPageSize << mdts_) < max_transfer_) max_transfer_ = kPageSize << mdts_;
        info->max_transfer = max_transfer_;
        ops_->log("nvme: controller ready, admin + 1 I/O queue pair (polling)");
        return 0;
    }

    int32_t read(uint64_t lba, uint32_t count, uint64_t buf) { return io(kOpRead, lba, count, buf); }
    int32_t write(uint64_t lba, uint32_t count, uint64_t buf) { return io(kOpWrite, lba, count, buf); }

    // Commit the controller's volatile write cache to media.
    int32_t flush() {
        Command c{};
        c.cdw0 = kOpFlush;
        c.nsid = nsid_;
        return submit(io_, c, nullptr);
    }

private:
    int32_t io(uint8_t opcode, uint64_t lba, uint32_t count, uint64_t buf) {
        if (count == 0 || uint64_t(count) * block_size_ > max_transfer_) return -1;
        if (lba + count > block_count_) return -2;
        Command c{};
        c.cdw0 = opcode;
        c.nsid = nsid_;
        c.prp1 = buf;
        const uint64_t bytes = uint64_t(count) * block_size_;
        const uint64_t first_page_room = kPageSize - (buf & (kPageSize - 1));
        if (bytes > first_page_room) c.prp2 = (buf & ~uint64_t(kPageSize - 1)) + kPageSize;
        c.cdw10 = static_cast<uint32_t>(lba);
        c.cdw11 = static_cast<uint32_t>(lba >> 32);
        c.cdw12 = count - 1;
        return submit(io_, c, nullptr);
    }

private:
    int32_t identify(const dhi_dma& page, dhi_block_info* info) {
        // Controller data structure (CNS 1).
        Command c{};
        c.cdw0 = kOpIdentify;
        c.prp1 = page.phys;
        c.cdw10 = 1;
        if (submit(admin_, c, nullptr) != 0) return fail("identify controller failed");
        const auto* id = static_cast<const uint8_t*>(page.virt);
        copy_trimmed(info->serial, id + 4, 20);
        copy_trimmed(info->model, id + 24, 40);
        copy_trimmed(info->firmware, id + 64, 8);
        mdts_ = id[77];

        // Namespace 1 (CNS 0).
        c = Command{};
        c.cdw0 = kOpIdentify;
        c.nsid = nsid_ = 1;
        c.prp1 = page.phys;
        c.cdw10 = 0;
        if (submit(admin_, c, nullptr) != 0) return fail("identify namespace failed");
        uint64_t nsze = 0;
        for (int i = 7; i >= 0; --i) nsze = (nsze << 8) | id[i];
        const uint8_t format = id[26] & 0xF;
        const uint8_t lbads = id[128 + 4 * format + 2];
        if (nsze == 0 || lbads < 9) return fail("namespace 1 is empty or unformatted");
        block_count_ = nsze;
        block_size_ = 1u << lbads;
        info->block_count = block_count_;
        info->block_size = block_size_;
        return 0;
    }

    int32_t create_io_queues() {
        if (!make_queue(io_, 1)) return fail("out of DMA memory");
        Command c{};
        c.cdw0 = kOpCreateIoCq;
        c.prp1 = io_.cq.phys;
        c.cdw10 = ((kQueueDepth - 1) << 16) | io_.id;
        c.cdw11 = 1;  // physically contiguous, interrupts off
        if (submit(admin_, c, nullptr) != 0) return fail("create I/O completion queue failed");
        c = Command{};
        c.cdw0 = kOpCreateIoSq;
        c.prp1 = io_.sq.phys;
        c.cdw10 = ((kQueueDepth - 1) << 16) | io_.id;
        c.cdw11 = (uint32_t(io_.id) << 16) | 1;  // completes to CQ 1, contiguous
        if (submit(admin_, c, nullptr) != 0) return fail("create I/O submission queue failed");
        return 0;
    }

    bool make_queue(Queue& q, uint16_t id) {
        q = Queue{};
        q.id = id;
        return ops_->dma_alloc(kQueueDepth * sizeof(Command), &q.sq) == 0 &&
               ops_->dma_alloc(kQueueDepth * sizeof(Completion), &q.cq) == 0;
    }

    int32_t submit(Queue& q, Command c, uint32_t* result) {
        const uint16_t cid = q.next_cid++;
        c.cdw0 = (c.cdw0 & 0xFFFF) | (uint32_t(cid) << 16);
        auto* sq = static_cast<volatile Command*>(q.sq.virt);
        copy_command(&sq[q.sq_tail], c);
        q.sq_tail = static_cast<uint16_t>((q.sq_tail + 1) % kQueueDepth);
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        write32(doorbell(q.id, false), q.sq_tail);

        auto* cq = static_cast<volatile Completion*>(q.cq.virt);
        for (uint32_t waited_us = 0;; ++waited_us) {
            const uint16_t status = cq[q.cq_head].status;
            if ((status & 1) == q.phase) {
                if (result) *result = cq[q.cq_head].result;
                q.cq_head = static_cast<uint16_t>((q.cq_head + 1) % kQueueDepth);
                if (q.cq_head == 0) q.phase ^= 1;
                write32(doorbell(q.id, true), q.cq_head);
                return (status >> 1) == 0 ? 0 : -(int32_t)(status >> 1);
            }
            if (waited_us > 1000u * (timeout_ms_ ? timeout_ms_ : 1000)) return -100;
            ops_->delay_us(1);
        }
    }

    bool wait_ready(bool ready) {
        for (uint32_t ms = 0; ms <= (timeout_ms_ ? timeout_ms_ : 5000); ++ms) {
            const uint32_t csts = read32(kRegCsts);
            if (csts & 2) return false;  // controller fatal status
            if (bool(csts & 1) == ready) return true;
            ops_->delay_us(1000);
        }
        return false;
    }

    uint32_t doorbell(uint16_t qid, bool completion) const {
        return 0x1000 + (2 * qid + (completion ? 1 : 0)) * stride_;
    }

    static void copy_command(volatile Command* dst, const Command& src) {
        auto* d = reinterpret_cast<volatile uint32_t*>(dst);
        const auto* s = reinterpret_cast<const uint32_t*>(&src);
        for (unsigned i = 0; i < sizeof(Command) / 4; ++i) d[i] = s[i];
    }

    static void copy_trimmed(char* dst, const uint8_t* src, int n) {
        int len = n;
        while (len > 0 && (src[len - 1] == ' ' || src[len - 1] == 0)) --len;
        for (int i = 0; i < len; ++i) dst[i] = static_cast<char>(src[i]);
        dst[len] = 0;
    }

    int32_t fail(const char* why) {
        ops_->log(why);
        return -1;
    }

    uint32_t read32(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(regs_ + off); }
    void write32(uint32_t off, uint32_t v) { *reinterpret_cast<volatile uint32_t*>(regs_ + off) = v; }
    uint64_t read64(uint32_t off) const { return uint64_t(read32(off)) | (uint64_t(read32(off + 4)) << 32); }
    void write64(uint32_t off, uint64_t v) {
        write32(off, static_cast<uint32_t>(v));
        write32(off + 4, static_cast<uint32_t>(v >> 32));
    }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* regs_ = nullptr;
    uint32_t stride_ = 4;
    uint32_t timeout_ms_ = 0;
    uint8_t mdts_ = 0;
    uint32_t nsid_ = 1;
    uint64_t block_count_ = 0;
    uint32_t block_size_ = 512;
    uint32_t max_transfer_ = 0;
    Queue admin_{}, io_{};
};

constinit Controller g_controllers[kMaxControllers]{};
constinit int g_count = 0;

}  // namespace

extern "C" int32_t aero_nvme_init(const dhi_ops* ops, uint64_t bar0_phys, dhi_block_info* out) {
    if (ops == nullptr || out == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
    if (g_count == kMaxControllers) return -2;
    *out = dhi_block_info{};
    const int32_t rc = g_controllers[g_count].init(ops, bar0_phys, out);
    if (rc != 0) return rc;
    return g_count++;
}

extern "C" int32_t aero_nvme_read(int32_t ctrl, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (ctrl < 0 || ctrl >= g_count) return -1;
    return g_controllers[ctrl].read(lba, count, buf_phys);
}

extern "C" int32_t aero_nvme_write(int32_t ctrl, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (ctrl < 0 || ctrl >= g_count) return -1;
    return g_controllers[ctrl].write(lba, count, buf_phys);
}

extern "C" int32_t aero_nvme_flush(int32_t ctrl) {
    if (ctrl < 0 || ctrl >= g_count) return -1;
    return g_controllers[ctrl].flush();
}
