// AeroForge Intel Ethernet driver for the e1000 / e1000e family (C++20,
// freestanding), behind the Driver Host Interface.
//
// Covers the 8254x (e1000) and 8257x/I217/I218/I219 (e1000e) controllers with
// legacy descriptors: one receive and one transmit ring, polled by the kernel.
// The newer igb (I210/I211) and igc (I225/I226) chips use different queue
// registers and descriptors and get their own driver next.

#include "dhi.h"

namespace {

// Registers (Intel 8254x / 82574 datasheets).
constexpr uint32_t kCtrl   = 0x0000;
constexpr uint32_t kStatus = 0x0008;
constexpr uint32_t kEerd   = 0x0014;
constexpr uint32_t kImc    = 0x00D8;
constexpr uint32_t kRctl   = 0x0100;
constexpr uint32_t kTctl   = 0x0400;
constexpr uint32_t kTipg   = 0x0410;
constexpr uint32_t kRdbal  = 0x2800;
constexpr uint32_t kRdbah  = 0x2804;
constexpr uint32_t kRdlen  = 0x2808;
constexpr uint32_t kRdh    = 0x2810;
constexpr uint32_t kRdt    = 0x2818;
constexpr uint32_t kTdbal  = 0x3800;
constexpr uint32_t kTdbah  = 0x3804;
constexpr uint32_t kTdlen  = 0x3808;
constexpr uint32_t kTdh    = 0x3810;
constexpr uint32_t kTdt    = 0x3818;
constexpr uint32_t kMta    = 0x5200;
constexpr uint32_t kRal0   = 0x5400;
constexpr uint32_t kRah0   = 0x5404;

constexpr uint32_t kCtrlSlu  = 1u << 6;   // set link up
constexpr uint32_t kCtrlAsde = 1u << 5;   // auto speed detection
constexpr uint32_t kCtrlRst  = 1u << 26;
constexpr uint32_t kCtrlPhyRst = 1u << 31;
constexpr uint32_t kCtrlLrst = 1u << 3;
constexpr uint32_t kStatusLinkUp = 1u << 1;

constexpr uint32_t kRctlEn    = 1u << 1;
constexpr uint32_t kRctlBam   = 1u << 15;  // accept broadcast
constexpr uint32_t kRctlSecrc = 1u << 26;  // strip the Ethernet CRC
constexpr uint32_t kTctlEn    = 1u << 1;
constexpr uint32_t kTctlPsp   = 1u << 3;   // pad short packets

constexpr uint8_t kTxCmdEop  = 1u << 0;
constexpr uint8_t kTxCmdIfcs = 1u << 1;
constexpr uint8_t kTxCmdRs   = 1u << 3;
constexpr uint8_t kDescDone  = 1u << 0;
constexpr uint8_t kRxEop     = 1u << 1;

constexpr int kRxCount = 64;
constexpr int kTxCount = 64;
constexpr uint32_t kBufSize = 2048;

struct RxDesc {
    uint64_t addr;
    uint16_t length;
    uint16_t checksum;
    uint8_t status;
    uint8_t errors;
    uint16_t special;
};
static_assert(sizeof(RxDesc) == 16);

struct TxDesc {
    uint64_t addr;
    uint16_t length;
    uint8_t cso;
    uint8_t cmd;
    uint8_t status;
    uint8_t css;
    uint16_t special;
};
static_assert(sizeof(TxDesc) == 16);

class Nic {
public:
    int32_t init(const dhi_ops* ops, uint64_t mmio_phys, dhi_net_info* info) {
        ops_ = ops;
        regs_ = static_cast<volatile uint8_t*>(ops_->map_mmio(mmio_phys, 0x20000));
        if (regs_ == nullptr) return -1;

        // Reset, then mask every interrupt: we poll.
        write(kImc, 0xFFFFFFFF);
        write(kCtrl, read(kCtrl) | kCtrlRst);
        ops_->delay_us(10000);
        for (int i = 0; i < 1000 && (read(kCtrl) & kCtrlRst); ++i) ops_->delay_us(100);
        write(kImc, 0xFFFFFFFF);
        write(kCtrl, (read(kCtrl) | kCtrlSlu | kCtrlAsde) & ~(kCtrlLrst | kCtrlPhyRst));

        read_mac(info->mac);
        for (int i = 0; i < 6; ++i) mac_[i] = info->mac[i];
        // Program the address filter with our MAC (valid bit 31), clear the multicast table.
        write(kRal0, uint32_t(mac_[0]) | uint32_t(mac_[1]) << 8 | uint32_t(mac_[2]) << 16 | uint32_t(mac_[3]) << 24);
        write(kRah0, uint32_t(mac_[4]) | uint32_t(mac_[5]) << 8 | (1u << 31));
        for (uint32_t i = 0; i < 128; ++i) write(kMta + i * 4, 0);

        if (!setup_rx() || !setup_tx()) return -2;

        // Give the link a moment to come up (auto-negotiation can take a while on real cables).
        for (int i = 0; i < 300 && !(read(kStatus) & kStatusLinkUp); ++i) ops_->delay_us(10000);
        info->link_up = link_up() ? 1 : 0;
        info->speed_mbps = speed();
        return 0;
    }

    int32_t send(const void* data, uint32_t len) {
        if (len == 0 || len > kBufSize) return -1;
        volatile TxDesc& d = tx_[tx_tail_];
        // The slot is free once the NIC has marked it done (all start out done).
        if (!(d.status & kDescDone)) return -2;
        auto* dst = static_cast<uint8_t*>(tx_buf_.virt) + tx_tail_ * kBufSize;
        const auto* src = static_cast<const uint8_t*>(data);
        for (uint32_t i = 0; i < len; ++i) dst[i] = src[i];
        d.addr = tx_buf_.phys + uint64_t(tx_tail_) * kBufSize;
        d.length = uint16_t(len);
        d.cmd = kTxCmdEop | kTxCmdIfcs | kTxCmdRs;
        d.status = 0;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        tx_tail_ = (tx_tail_ + 1) % kTxCount;
        write(kTdt, uint32_t(tx_tail_));
        return 0;
    }

    int32_t recv(void* out, uint32_t max) {
        volatile RxDesc& d = rx_[rx_next_];
        if (!(d.status & kDescDone)) return 0;
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        int32_t n = 0;
        // Frames bigger than one buffer are dropped (no jumbo frames).
        if ((d.status & kRxEop) && d.errors == 0) {
            n = d.length < max ? d.length : int32_t(max);
            const auto* src = static_cast<const uint8_t*>(rx_buf_.virt) + rx_next_ * kBufSize;
            auto* dst = static_cast<uint8_t*>(out);
            for (int32_t i = 0; i < n; ++i) dst[i] = src[i];
        } else {
            n = -1;
        }
        d.status = 0;
        write(kRdt, uint32_t(rx_next_));  // hand the descriptor back
        rx_next_ = (rx_next_ + 1) % kRxCount;
        return n;
    }

    bool link_up() const { return read(kStatus) & kStatusLinkUp; }

    uint32_t speed() const {
        switch ((read(kStatus) >> 6) & 3) {
            case 0: return 10;
            case 1: return 100;
            default: return 1000;
        }
    }

private:
    // QEMU and most firmware leave the MAC in RAL0/RAH0; older 8254x keep it
    // only in the EEPROM.
    void read_mac(uint8_t* mac) {
        const uint32_t lo = read(kRal0), hi = read(kRah0);
        if (hi & (1u << 31)) {
            for (int i = 0; i < 4; ++i) mac[i] = uint8_t(lo >> (8 * i));
            mac[4] = uint8_t(hi);
            mac[5] = uint8_t(hi >> 8);
            return;
        }
        for (uint32_t word = 0; word < 3; ++word) {
            write(kEerd, 1u | (word << 8));
            uint32_t v = 0;
            for (int i = 0; i < 1000 && !((v = read(kEerd)) & (1u << 4)); ++i) ops_->delay_us(10);
            mac[word * 2] = uint8_t(v >> 16);
            mac[word * 2 + 1] = uint8_t(v >> 24);
        }
    }

    bool setup_rx() {
        if (ops_->dma_alloc(kRxCount * sizeof(RxDesc), &rx_ring_) != 0) return false;
        if (ops_->dma_alloc(kRxCount * kBufSize, &rx_buf_) != 0) return false;
        rx_ = static_cast<volatile RxDesc*>(rx_ring_.virt);
        for (int i = 0; i < kRxCount; ++i) {
            rx_[i].addr = rx_buf_.phys + uint64_t(i) * kBufSize;
            rx_[i].status = 0;
        }
        write(kRdbal, uint32_t(rx_ring_.phys));
        write(kRdbah, uint32_t(rx_ring_.phys >> 32));
        write(kRdlen, kRxCount * sizeof(RxDesc));
        write(kRdh, 0);
        write(kRdt, kRxCount - 1);
        write(kRctl, kRctlEn | kRctlBam | kRctlSecrc);  // 2 KiB buffers
        return true;
    }

    bool setup_tx() {
        if (ops_->dma_alloc(kTxCount * sizeof(TxDesc), &tx_ring_) != 0) return false;
        if (ops_->dma_alloc(kTxCount * kBufSize, &tx_buf_) != 0) return false;
        tx_ = static_cast<volatile TxDesc*>(tx_ring_.virt);
        for (int i = 0; i < kTxCount; ++i) tx_[i].status = kDescDone;
        write(kTdbal, uint32_t(tx_ring_.phys));
        write(kTdbah, uint32_t(tx_ring_.phys >> 32));
        write(kTdlen, kTxCount * sizeof(TxDesc));
        write(kTdh, 0);
        write(kTdt, 0);
        write(kTctl, kTctlEn | kTctlPsp | (0x0Fu << 4) | (0x40u << 12));  // collision threshold/distance
        write(kTipg, 0x0060200A);
        return true;
    }

    uint32_t read(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(regs_ + off); }
    void write(uint32_t off, uint32_t v) const { *reinterpret_cast<volatile uint32_t*>(regs_ + off) = v; }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* regs_ = nullptr;
    dhi_dma rx_ring_{}, rx_buf_{}, tx_ring_{}, tx_buf_{};
    volatile RxDesc* rx_ = nullptr;
    volatile TxDesc* tx_ = nullptr;
    int rx_next_ = 0, tx_tail_ = 0;
    uint8_t mac_[6] = {};
};

constexpr int kMaxNics = 4;
constinit Nic g_nics[kMaxNics]{};
constinit int32_t g_count = 0;

// e1000 (8254x) and e1000e (8257x, ICH/PCH I217-I219) device ids this driver knows.
constexpr uint16_t kIds[] = {
    0x100E, 0x100F, 0x1004, 0x1008, 0x1010, 0x1015, 0x1019, 0x101E, 0x1026, 0x1075, 0x1076,  // 8254x
    0x105E, 0x105F, 0x107D, 0x107E, 0x108B, 0x108C, 0x109A, 0x10D3, 0x10F6, 0x150C,          // 8257x
    0x153A, 0x153B, 0x155A, 0x1559, 0x15A0, 0x15A1, 0x15A2, 0x15A3,                          // I217/I218
    0x156F, 0x1570, 0x15B7, 0x15B8, 0x15B9, 0x15BB, 0x15BC, 0x15BD, 0x15BE, 0x15D6, 0x15D7,  // I219
    0x15D8, 0x15E3, 0x15DF, 0x15E0, 0x15E1, 0x15E2, 0x0D4C, 0x0D4D, 0x0D4E, 0x0D4F, 0x0D53,
    0x0D55, 0x15FB, 0x15FC, 0x1A1C, 0x1A1D, 0x1A1E, 0x1A1F, 0x550A, 0x550B, 0x550C, 0x550D,
};

}  // namespace

extern "C" int32_t aero_e1000_supports(uint16_t device_id) {
    for (uint16_t id : kIds)
        if (id == device_id) return 1;
    return 0;
}

extern "C" int32_t aero_e1000_init(const dhi_ops* ops, uint64_t mmio_phys, dhi_net_info* info) {
    if (ops == nullptr || info == nullptr || ops->abi_version != DHI_ABI_VERSION || g_count >= kMaxNics) return -1;
    const int32_t rc = g_nics[g_count].init(ops, mmio_phys, info);
    if (rc < 0) return rc;
    return g_count++;
}

extern "C" int32_t aero_e1000_send(int32_t nic, const void* frame, uint32_t len) {
    if (nic < 0 || nic >= g_count || frame == nullptr) return -1;
    return g_nics[nic].send(frame, len);
}

extern "C" int32_t aero_e1000_recv(int32_t nic, void* frame, uint32_t max) {
    if (nic < 0 || nic >= g_count || frame == nullptr) return -1;
    return g_nics[nic].recv(frame, max);
}

extern "C" int32_t aero_e1000_link(int32_t nic, uint32_t* speed_mbps) {
    if (nic < 0 || nic >= g_count) return 0;
    if (speed_mbps) *speed_mbps = g_nics[nic].speed();
    return g_nics[nic].link_up() ? 1 : 0;
}
