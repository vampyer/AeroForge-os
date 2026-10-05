// AeroForge AHCI (SATA) driver (C++20, freestanding), behind the Driver Host
// Interface.
//
// One command slot per port: reads, writes and cache flushes. When the kernel
// hands init interrupt sources (its MSI already pointed at them), a thread
// waiting for a command polls briefly, then sleeps until the controller's
// interrupt. The controller has one interrupt for all ports, so each port
// clears its own status bits after a command. NCQ comes later.

#include "dhi.h"

namespace {

// HBA registers (AHCI 1.3.1, section 3.1).
constexpr uint32_t kHbaCap = 0x00;
constexpr uint32_t kHbaGhc = 0x04;
constexpr uint32_t kHbaIs  = 0x08;
constexpr uint32_t kHbaPi  = 0x0C;
constexpr uint32_t kGhcAhciEnable = 1u << 31;
constexpr uint32_t kGhcIrqEnable = 1u << 1;

// Port registers (section 3.3), relative to 0x100 + port * 0x80.
constexpr uint32_t kPxClb  = 0x00;
constexpr uint32_t kPxClbu = 0x04;
constexpr uint32_t kPxFb   = 0x08;
constexpr uint32_t kPxFbu  = 0x0C;
constexpr uint32_t kPxIs   = 0x10;
constexpr uint32_t kPxIe   = 0x14;
constexpr uint32_t kPxCmd  = 0x18;
constexpr uint32_t kPxTfd  = 0x20;
constexpr uint32_t kPxSig  = 0x24;
constexpr uint32_t kPxSsts = 0x28;
constexpr uint32_t kPxSerr = 0x30;
constexpr uint32_t kPxCi   = 0x38;

constexpr uint32_t kCmdStart = 1u << 0;
constexpr uint32_t kCmdFre   = 1u << 4;
constexpr uint32_t kCmdFr    = 1u << 14;
constexpr uint32_t kCmdCr    = 1u << 15;
constexpr uint32_t kIsTaskFileError = 1u << 30;
// Interrupts for a finished command (D2H register FIS, PIO setup FIS, set
// device bits FIS) and for errors (task file, host bus, interface).
constexpr uint32_t kIeCompletion = (1u << 0) | (1u << 1) | (1u << 3);
constexpr uint32_t kIeErrors = (1u << 30) | (1u << 29) | (1u << 28) | (1u << 27);
constexpr uint32_t kTfdErr = 1u << 0;
constexpr uint32_t kTfdBusy = 1u << 7;
constexpr uint32_t kTfdDrq = 1u << 3;

constexpr uint32_t kSigSataDisk = 0x00000101;

constexpr uint8_t kFisRegH2D = 0x27;
constexpr uint8_t kAtaIdentify = 0xEC;
constexpr uint8_t kAtaReadDmaExt = 0x25;
constexpr uint8_t kAtaWriteDmaExt = 0x35;
constexpr uint8_t kAtaFlushCacheExt = 0xEA;
constexpr uint16_t kHeaderWrite = 1u << 6;

constexpr uint32_t kMaxTransfer = 8192;
constexpr uint32_t kSpinUs = 20;  // poll this long before sleeping for the interrupt
constexpr int kMaxDisks = 8;

struct CommandHeader {
    uint16_t flags;  // CFL in bits 0-4, W bit 6
    uint16_t prdtl;
    uint32_t prdbc;
    uint64_t ctba;
    uint32_t reserved[4];
};
static_assert(sizeof(CommandHeader) == 32);

struct Prd {
    uint64_t dba;
    uint32_t reserved;
    uint32_t dbc;  // byte count - 1, bit 31 = interrupt on completion
};

struct CommandTable {
    uint8_t cfis[64];
    uint8_t acmd[16];
    uint8_t reserved[48];
    Prd prdt[1];
};
static_assert(sizeof(CommandTable) == 144);

class Disk {
public:
    bool init(const dhi_ops* ops, volatile uint8_t* abar, uint32_t index, uint32_t irq_source,
              dhi_block_info* info) {
        ops_ = ops;
        abar_ = abar;
        index_ = index;
        port_ = abar + 0x100 + index * 0x80;
        irq_ = irq_source != DHI_NO_IRQ;
        irq_source_ = irq_source;
        stop();
        if (ops_->dma_alloc(1024, &cmd_list_) != 0 || ops_->dma_alloc(256, &fis_) != 0 ||
            ops_->dma_alloc(sizeof(CommandTable), &table_) != 0) {
            ops_->log("ahci: out of DMA memory");
            return false;
        }
        write64(kPxClb, kPxClbu, cmd_list_.phys);
        write64(kPxFb, kPxFbu, fis_.phys);
        write(kPxSerr, 0xFFFFFFFF);
        write(kPxIs, 0xFFFFFFFF);
        write(kPxIe, irq_ ? (kIeCompletion | kIeErrors) : 0);
        auto* header = static_cast<CommandHeader*>(cmd_list_.virt);
        header->ctba = table_.phys;
        start();

        dhi_dma page{};
        if (ops_->dma_alloc(512, &page) != 0) return false;
        bool ok = issue(kAtaIdentify, 0, 0, page.phys, 512);
        if (ok) {
            const auto* id = static_cast<const uint16_t*>(page.virt);
            if ((id[83] & (1u << 10)) == 0) {
                ops_->log("ahci: disk lacks 48-bit LBA, skipped");
                ok = false;
            } else {
                sectors_ = uint64_t(id[100]) | uint64_t(id[101]) << 16 | uint64_t(id[102]) << 32 |
                           uint64_t(id[103]) << 48;
                info->block_count = sectors_;
                info->block_size = 512;
                info->max_transfer = kMaxTransfer;
                ata_string(info->serial, id + 10, 10);
                ata_string(info->model, id + 27, 20);
                ata_string(info->firmware, id + 23, 4);
            }
        }
        ops_->dma_free(&page);
        return ok && sectors_ > 0;
    }

    int32_t read(uint64_t lba, uint32_t count, uint64_t buf) {
        if (count == 0 || count * 512u > kMaxTransfer) return -1;
        if (lba + count > sectors_) return -2;
        return issue(kAtaReadDmaExt, lba, count, buf, count * 512u) ? 0 : -3;
    }

    int32_t write(uint64_t lba, uint32_t count, uint64_t buf) {
        if (count == 0 || count * 512u > kMaxTransfer) return -1;
        if (lba + count > sectors_) return -2;
        return issue(kAtaWriteDmaExt, lba, count, buf, count * 512u) ? 0 : -3;
    }

    int32_t flush() { return issue(kAtaFlushCacheExt, 0, 0, 0, 0) ? 0 : -3; }

private:
    bool issue(uint8_t command, uint64_t lba, uint32_t count, uint64_t buf, uint32_t bytes) {
        // Wait until the device isn't busy.
        for (int i = 0; i < 100000 && (read32(kPxTfd) & (kTfdBusy | kTfdDrq)); ++i) ops_->delay_us(10);

        auto* header = static_cast<CommandHeader*>(cmd_list_.virt);
        header->flags = sizeof(uint32_t) * 5 / 4;  // CFL: 5-dword H2D FIS
        if (command == kAtaWriteDmaExt) header->flags |= kHeaderWrite;
        header->prdtl = bytes > 0 ? 1 : 0;
        header->prdbc = 0;

        auto* t = static_cast<CommandTable*>(table_.virt);
        for (auto& b : t->cfis) b = 0;
        t->cfis[0] = kFisRegH2D;
        t->cfis[1] = 0x80;  // command, not control
        t->cfis[2] = command;
        t->cfis[4] = uint8_t(lba);
        t->cfis[5] = uint8_t(lba >> 8);
        t->cfis[6] = uint8_t(lba >> 16);
        t->cfis[7] = command == kAtaIdentify || command == kAtaFlushCacheExt ? 0 : (1u << 6);  // LBA mode
        t->cfis[8] = uint8_t(lba >> 24);
        t->cfis[9] = uint8_t(lba >> 32);
        t->cfis[10] = uint8_t(lba >> 40);
        t->cfis[12] = uint8_t(count);
        t->cfis[13] = uint8_t(count >> 8);
        t->prdt[0].dba = buf;
        t->prdt[0].reserved = 0;
        t->prdt[0].dbc = bytes - 1;

        clear_irq();
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        write(kPxCi, 1);
        bool error = false;
        for (uint32_t us = 0; us < 5'000'000;) {
            if ((read32(kPxCi) & 1) == 0) break;
            if (read32(kPxIs) & kIsTaskFileError) {
                ops_->log("ahci: task file error");
                error = true;
                break;
            }
            if (irq_ && us >= kSpinUs) {
                // Sleep until the controller interrupts (for this port or
                // another one); each wait counts as its longest, 10 ms.
                ops_->irq_wait(irq_source_);
                us += 10000;
            } else {
                ops_->delay_us(2);
                us += 2;
            }
        }
        const bool busy = (read32(kPxCi) & 1) != 0;
        clear_irq();
        if (error) return false;
        if (busy) {
            ops_->log("ahci: command timed out");
            return false;
        }
        return (read32(kPxTfd) & kTfdErr) == 0;
    }

    // Port status bits first, then this port's bit in the controller's
    // summary, so the controller can interrupt again for the next command.
    void clear_irq() {
        write(kPxIs, 0xFFFFFFFF);
        *reinterpret_cast<volatile uint32_t*>(abar_ + kHbaIs) = 1u << index_;
    }

    void stop() {
        write(kPxCmd, read32(kPxCmd) & ~kCmdStart);
        for (int i = 0; i < 500 && (read32(kPxCmd) & kCmdCr); ++i) ops_->delay_us(1000);
        write(kPxCmd, read32(kPxCmd) & ~kCmdFre);
        for (int i = 0; i < 500 && (read32(kPxCmd) & kCmdFr); ++i) ops_->delay_us(1000);
    }

    void start() {
        for (int i = 0; i < 500 && (read32(kPxCmd) & kCmdCr); ++i) ops_->delay_us(1000);
        write(kPxCmd, read32(kPxCmd) | kCmdFre);
        write(kPxCmd, read32(kPxCmd) | kCmdStart);
    }

    // ATA strings are 16-bit words with the bytes swapped, space padded.
    static void ata_string(char* dst, const uint16_t* words, int n) {
        int len = 0;
        for (int i = 0; i < n; ++i) {
            dst[len++] = static_cast<char>(words[i] >> 8);
            dst[len++] = static_cast<char>(words[i] & 0xFF);
        }
        while (len > 0 && (dst[len - 1] == ' ' || dst[len - 1] == 0)) --len;
        dst[len] = 0;
    }

    uint32_t read32(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(port_ + off); }
    void write(uint32_t off, uint32_t v) { *reinterpret_cast<volatile uint32_t*>(port_ + off) = v; }
    void write64(uint32_t lo, uint32_t hi, uint64_t v) {
        write(lo, static_cast<uint32_t>(v));
        write(hi, static_cast<uint32_t>(v >> 32));
    }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* abar_ = nullptr;
    volatile uint8_t* port_ = nullptr;
    uint32_t index_ = 0;
    bool irq_ = false;
    uint32_t irq_source_ = 0;
    dhi_dma cmd_list_{}, fis_{}, table_{};
    uint64_t sectors_ = 0;
};

constinit Disk g_disks[kMaxDisks]{};
constinit int32_t g_count = 0;

}  // namespace

extern "C" int32_t aero_ahci_init(const dhi_ops* ops, uint64_t abar_phys, uint32_t irq_first,
                                  dhi_block_info* out, int32_t max, int32_t* first_id) {
    if (ops == nullptr || out == nullptr || first_id == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
    auto* abar = static_cast<volatile uint8_t*>(ops->map_mmio(abar_phys, 0x1100));
    if (abar == nullptr) return -1;
    auto reg = [abar](uint32_t off) -> volatile uint32_t& { return *reinterpret_cast<volatile uint32_t*>(abar + off); };

    reg(kHbaGhc) = reg(kHbaGhc) | kGhcAhciEnable;
    const uint32_t ports = reg(kHbaPi);
    const uint32_t slots = ((reg(kHbaCap) >> 8) & 0x1F) + 1;
    (void)slots;  // one slot per port is all we use for now

    *first_id = g_count;
    int32_t found = 0;
    for (uint32_t p = 0; p < 32 && found < max && g_count < kMaxDisks; ++p) {
        if ((ports & (1u << p)) == 0) continue;
        volatile uint8_t* port = abar + 0x100 + p * 0x80;
        const uint32_t ssts = *reinterpret_cast<volatile uint32_t*>(port + kPxSsts);
        if ((ssts & 0xF) != 3 || ((ssts >> 8) & 0xF) != 1) continue;  // no device, or not active
        if (*reinterpret_cast<volatile uint32_t*>(port + kPxSig) != kSigSataDisk) continue;  // e.g. ATAPI CD-ROM
        out[found] = dhi_block_info{};
        const uint32_t irq = irq_first == DHI_NO_IRQ ? DHI_NO_IRQ : irq_first + uint32_t(found);
        if (g_disks[g_count].init(ops, abar, p, irq, &out[found])) {
            ++g_count;
            ++found;
        }
    }
    // Port interrupts are enabled per port in Disk::init; this lets them out.
    if (irq_first != DHI_NO_IRQ) reg(kHbaGhc) = reg(kHbaGhc) | kGhcIrqEnable;
    ops->log(found > 0 ? (irq_first != DHI_NO_IRQ ? "ahci: controller ready, 1 command slot per port"
                                                  : "ahci: controller ready (polling, 1 command slot per port)")
                       : "ahci: controller ready, no SATA disks attached");
    return found;
}

extern "C" int32_t aero_ahci_read(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (disk < 0 || disk >= g_count) return -1;
    return g_disks[disk].read(lba, count, buf_phys);
}

extern "C" int32_t aero_ahci_write(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (disk < 0 || disk >= g_count) return -1;
    return g_disks[disk].write(lba, count, buf_phys);
}

extern "C" int32_t aero_ahci_flush(int32_t disk) {
    if (disk < 0 || disk >= g_count) return -1;
    return g_disks[disk].flush();
}
