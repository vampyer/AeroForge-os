// AeroForge Intel Ethernet driver for the igb and igc families (C++20,
// freestanding), behind the Driver Host Interface.
//
//   igb: 82576, I350, I210, I211 (1 Gb/s)
//   igc: I225 and I226 (2.5 Gb/s), the chips on most current Intel and AMD boards
//
// Both use the same queue registers and "advanced" descriptors, so one class
// drives them; the differences are the reset bit, the 2.5 Gb/s speed bit and
// the I225 errata handled in init(). One receive and one transmit queue,
// polled by the kernel.

#include "dhi.h"

namespace {

// Registers (Intel 82576 / I210 / I225 datasheets).
constexpr uint32_t kCtrl    = 0x0000;
constexpr uint32_t kStatus  = 0x0008;
constexpr uint32_t kCtrlExt = 0x0018;
constexpr uint32_t kMdic    = 0x0020;
constexpr uint32_t kImc     = 0x1528;  // interrupt mask clear (igb/igc location)
constexpr uint32_t kRctl    = 0x0100;
constexpr uint32_t kTctl    = 0x0400;
constexpr uint32_t kEeer    = 0x0E30;  // EEE control
constexpr uint32_t kIpcnfg  = 0x0E38;  // EEE advertisement (igc)
constexpr uint32_t kMta     = 0x5200;
constexpr uint32_t kRal0    = 0x5400;
constexpr uint32_t kRah0    = 0x5404;
// Queue 0.
constexpr uint32_t kRdbal   = 0xC000;
constexpr uint32_t kRdbah   = 0xC004;
constexpr uint32_t kRdlen   = 0xC008;
constexpr uint32_t kSrrctl  = 0xC00C;
constexpr uint32_t kRdh     = 0xC010;
constexpr uint32_t kRdt     = 0xC018;
constexpr uint32_t kRxdctl  = 0xC028;
constexpr uint32_t kTdbal   = 0xE000;
constexpr uint32_t kTdbah   = 0xE004;
constexpr uint32_t kTdlen   = 0xE008;
constexpr uint32_t kTdh     = 0xE010;
constexpr uint32_t kTdt     = 0xE018;
constexpr uint32_t kTxdctl  = 0xE028;

constexpr uint32_t kCtrlSlu    = 1u << 6;
constexpr uint32_t kCtrlRst    = 1u << 26;  // igb: full reset
constexpr uint32_t kCtrlDevRst = 1u << 29;  // igc: device reset
constexpr uint32_t kStatusLinkUp = 1u << 1;
constexpr uint32_t kStatusSpeed2500 = 1u << 22;  // igc
constexpr uint32_t kCtrlExtDrvLoad = 1u << 28;  // tells the firmware a driver owns the port
constexpr uint32_t kQueueEnable = 1u << 25;

constexpr uint32_t kRctlEn    = 1u << 1;
constexpr uint32_t kRctlBam   = 1u << 15;
constexpr uint32_t kRctlSecrc = 1u << 26;
constexpr uint32_t kTctlEn    = 1u << 1;
constexpr uint32_t kTctlPsp   = 1u << 3;

constexpr uint32_t kSrrctlOneBuffer = 1u << 25;  // advanced descriptors, one buffer
constexpr uint32_t kSrrctlDropEn    = 1u << 31;

// Advanced transmit data descriptor command bits (in cmd_type_len).
constexpr uint32_t kTxDtypData = 3u << 20;
constexpr uint32_t kTxEop  = 1u << 24;
constexpr uint32_t kTxIfcs = 1u << 25;
constexpr uint32_t kTxRs   = 1u << 27;
constexpr uint32_t kTxDext = 1u << 29;
constexpr uint32_t kDescDone = 1u << 0;
constexpr uint32_t kRxEop    = 1u << 1;
constexpr uint32_t kRxErrors = 0xFFFu << 20;  // RXE, IPE, L4E and friends in status_error

// Copper PHY (MII) registers.
constexpr uint32_t kPhyBmcr = 0;
constexpr uint16_t kBmcrPowerDown = 1u << 11;
constexpr uint16_t kBmcrAnEnable  = 1u << 12;
constexpr uint16_t kBmcrAnRestart = 1u << 9;

constexpr int kRxCount = 64;
constexpr int kTxCount = 64;
constexpr uint32_t kBufSize = 2048;

// Advanced receive descriptor: the driver writes addresses, the NIC writes
// back status over the same 16 bytes.
union RxDesc {
    struct {
        uint64_t pkt_addr;
        uint64_t hdr_addr;
    } read;
    struct {
        uint32_t info;
        uint32_t rss;
        uint32_t status_error;
        uint16_t length;
        uint16_t vlan;
    } wb;
};
static_assert(sizeof(RxDesc) == 16);

struct TxDesc {
    uint64_t addr;
    uint32_t cmd_type_len;
    uint32_t olinfo_status;  // payload length in, DD bit out
};
static_assert(sizeof(TxDesc) == 16);

enum class Family : uint8_t { Igb, Igc };

class Nic {
public:
    int32_t init(const dhi_ops* ops, uint64_t mmio_phys, Family family, dhi_net_info* info) {
        ops_ = ops;
        family_ = family;
        regs_ = static_cast<volatile uint8_t*>(ops_->map_mmio(mmio_phys, 0x20000));
        if (regs_ == nullptr) return -1;

        // Reset with interrupts masked (we poll). The MAC address and PHY
        // settings reload from the NVM during reset.
        write(kImc, 0xFFFFFFFF);
        const uint32_t rst = family_ == Family::Igc ? kCtrlDevRst : kCtrlRst;
        write(kCtrl, read(kCtrl) | rst);
        ops_->delay_us(20000);
        for (int i = 0; i < 1000 && (read(kCtrl) & rst); ++i) ops_->delay_us(100);
        write(kImc, 0xFFFFFFFF);
        write(kCtrlExt, read(kCtrlExt) | kCtrlExtDrvLoad);

        if (family_ == Family::Igc) {
            // I225 errata: Energy Efficient Ethernet makes early steppings
            // (B1/B2, often the I225-V) drop the link under load or not
            // reach 2.5 Gb/s with some switches. Turn it off, as Intel's
            // own workaround does.
            write(kEeer, 0);
            write(kIpcnfg, 0);
        }
        power_up_phy();
        write(kCtrl, read(kCtrl) | kCtrlSlu);

        read_mac(info->mac);
        for (int i = 0; i < 6; ++i) mac_[i] = info->mac[i];
        write(kRal0, uint32_t(mac_[0]) | uint32_t(mac_[1]) << 8 | uint32_t(mac_[2]) << 16 | uint32_t(mac_[3]) << 24);
        write(kRah0, uint32_t(mac_[4]) | uint32_t(mac_[5]) << 8 | (1u << 31));
        for (uint32_t i = 0; i < 128; ++i) write(kMta + i * 4, 0);

        if (!setup_rx() || !setup_tx()) return -2;

        // Auto-negotiation at 2.5 Gb/s can take a few seconds on a real cable.
        for (int i = 0; i < 400 && !link_up(); ++i) ops_->delay_us(10000);
        info->link_up = link_up() ? 1 : 0;
        info->speed_mbps = speed();
        return 0;
    }

    int32_t send(const void* data, uint32_t len) {
        if (len == 0 || len > kBufSize) return -1;
        volatile TxDesc& d = tx_[tx_tail_];
        // A slot is free until first use, then once the NIC wrote DD back.
        if (tx_used_[tx_tail_] && !(d.olinfo_status & kDescDone)) return -2;
        auto* dst = static_cast<uint8_t*>(tx_buf_.virt) + tx_tail_ * kBufSize;
        const auto* src = static_cast<const uint8_t*>(data);
        for (uint32_t i = 0; i < len; ++i) dst[i] = src[i];
        d.addr = tx_buf_.phys + uint64_t(tx_tail_) * kBufSize;
        d.cmd_type_len = len | kTxDtypData | kTxDext | kTxEop | kTxIfcs | kTxRs;
        d.olinfo_status = len << 14;  // PAYLEN
        tx_used_[tx_tail_] = true;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        tx_tail_ = (tx_tail_ + 1) % kTxCount;
        write(kTdt, uint32_t(tx_tail_));
        return 0;
    }

    int32_t recv(void* out, uint32_t max) {
        volatile RxDesc& d = rx_[rx_next_];
        const uint32_t status = d.wb.status_error;
        if (!(status & kDescDone)) return 0;
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        int32_t n;
        if ((status & kRxEop) && !(status & kRxErrors)) {
            const uint32_t len = d.wb.length;
            n = int32_t(len < max ? len : max);
            const auto* src = static_cast<const uint8_t*>(rx_buf_.virt) + rx_next_ * kBufSize;
            auto* dst = static_cast<uint8_t*>(out);
            for (int32_t i = 0; i < n; ++i) dst[i] = src[i];
        } else {
            n = -1;  // bad or multi-buffer frame: dropped
        }
        // Write-back replaced the buffer address; put it back and return the slot.
        arm_rx(rx_next_);
        write(kRdt, uint32_t(rx_next_));
        rx_next_ = (rx_next_ + 1) % kRxCount;
        return n;
    }

    bool link_up() const { return read(kStatus) & kStatusLinkUp; }

    uint32_t speed() const {
        const uint32_t s = read(kStatus);
        switch ((s >> 6) & 3) {
            case 0: return 10;
            case 1: return 100;
            default: return (family_ == Family::Igc && (s & kStatusSpeed2500)) ? 2500 : 1000;
        }
    }

private:
    // Powers the PHY up (firmware or a previous OS may have left it down)
    // and restarts auto-negotiation with whatever speeds the NVM advertises.
    void power_up_phy() {
        uint16_t bmcr;
        if (!mdio_read(kPhyBmcr, &bmcr)) return;
        mdio_write(kPhyBmcr, uint16_t((bmcr & ~kBmcrPowerDown) | kBmcrAnEnable | kBmcrAnRestart));
        ops_->delay_us(1000);
    }

    bool mdio_wait(uint32_t* v) {
        for (int i = 0; i < 2000; ++i) {
            *v = read(kMdic);
            if (*v & (1u << 28)) return !(*v & (1u << 30));  // ready, no error
            ops_->delay_us(50);
        }
        return false;
    }

    bool mdio_read(uint32_t reg, uint16_t* out) {
        write(kMdic, (reg << 16) | (kPhyAddr << 21) | (2u << 26));
        uint32_t v;
        if (!mdio_wait(&v)) return false;
        *out = uint16_t(v);
        return true;
    }

    bool mdio_write(uint32_t reg, uint16_t value) {
        write(kMdic, value | (reg << 16) | (kPhyAddr << 21) | (1u << 26));
        uint32_t v;
        return mdio_wait(&v);
    }

    void read_mac(uint8_t* mac) {
        const uint32_t lo = read(kRal0), hi = read(kRah0);
        for (int i = 0; i < 4; ++i) mac[i] = uint8_t(lo >> (8 * i));
        mac[4] = uint8_t(hi);
        mac[5] = uint8_t(hi >> 8);
    }

    void arm_rx(int i) {
        rx_[i].read.pkt_addr = rx_buf_.phys + uint64_t(i) * kBufSize;
        rx_[i].read.hdr_addr = 0;  // also clears DD
    }

    bool setup_rx() {
        if (ops_->dma_alloc(kRxCount * sizeof(RxDesc), &rx_ring_) != 0) return false;
        if (ops_->dma_alloc(kRxCount * kBufSize, &rx_buf_) != 0) return false;
        rx_ = static_cast<volatile RxDesc*>(rx_ring_.virt);
        for (int i = 0; i < kRxCount; ++i) arm_rx(i);
        write(kRxdctl, 0);
        write(kRdbal, uint32_t(rx_ring_.phys));
        write(kRdbah, uint32_t(rx_ring_.phys >> 32));
        write(kRdlen, kRxCount * sizeof(RxDesc));
        write(kSrrctl, (kBufSize / 1024) | kSrrctlOneBuffer | kSrrctlDropEn);
        write(kRdh, 0);
        write(kRdt, 0);
        write(kRxdctl, kQueueEnable | (8u << 0) | (8u << 8) | (4u << 16));  // prefetch/host/write-back thresholds
        for (int i = 0; i < 100 && !(read(kRxdctl) & kQueueEnable); ++i) ops_->delay_us(100);
        write(kRdt, kRxCount - 1);
        write(kRctl, kRctlEn | kRctlBam | kRctlSecrc);
        return true;
    }

    bool setup_tx() {
        if (ops_->dma_alloc(kTxCount * sizeof(TxDesc), &tx_ring_) != 0) return false;
        if (ops_->dma_alloc(kTxCount * kBufSize, &tx_buf_) != 0) return false;
        tx_ = static_cast<volatile TxDesc*>(tx_ring_.virt);
        write(kTxdctl, 0);
        write(kTdbal, uint32_t(tx_ring_.phys));
        write(kTdbah, uint32_t(tx_ring_.phys >> 32));
        write(kTdlen, kTxCount * sizeof(TxDesc));
        write(kTdh, 0);
        write(kTdt, 0);
        write(kTxdctl, kQueueEnable | (8u << 0) | (1u << 8) | (16u << 16));
        for (int i = 0; i < 100 && !(read(kTxdctl) & kQueueEnable); ++i) ops_->delay_us(100);
        write(kTctl, kTctlEn | kTctlPsp | (0x0Fu << 4) | (0x3Fu << 12));
        return true;
    }

    uint32_t read(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(regs_ + off); }
    void write(uint32_t off, uint32_t v) const { *reinterpret_cast<volatile uint32_t*>(regs_ + off) = v; }

    static constexpr uint32_t kPhyAddr = 1;

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* regs_ = nullptr;
    Family family_ = Family::Igb;
    dhi_dma rx_ring_{}, rx_buf_{}, tx_ring_{}, tx_buf_{};
    volatile RxDesc* rx_ = nullptr;
    volatile TxDesc* tx_ = nullptr;
    bool tx_used_[kTxCount] = {};
    int rx_next_ = 0, tx_tail_ = 0;
    uint8_t mac_[6] = {};
};

constexpr int kMaxNics = 4;
constinit Nic g_nics[kMaxNics]{};
constinit int32_t g_count = 0;

constexpr uint16_t kIgbIds[] = {
    0x10C9, 0x10E6, 0x10E7, 0x10E8, 0x1526, 0x150A, 0x1518, 0x150D,  // 82576
    0x1521, 0x1522, 0x1523, 0x1524,                                  // I350
    0x1533, 0x1536, 0x1537, 0x1538, 0x157B, 0x157C, 0x15F6,          // I210
    0x1539,                                                          // I211
};

constexpr uint16_t kIgcIds[] = {
    0x15F2, 0x15F3, 0x15F7, 0x15F8, 0x15FD, 0x0D9F, 0x3100, 0x3101, 0x5502,  // I220/I221/I225
    0x125B, 0x125C, 0x125D, 0x125E, 0x125F, 0x3102, 0x5503,                  // I226
};

int family_of(uint16_t device_id) {
    for (uint16_t id : kIgbIds)
        if (id == device_id) return int(Family::Igb);
    for (uint16_t id : kIgcIds)
        if (id == device_id) return int(Family::Igc);
    return -1;
}

}  // namespace

extern "C" int32_t aero_igc_supports(uint16_t device_id) {
    return family_of(device_id) < 0 ? 0 : 1;
}

extern "C" int32_t aero_igc_init(const dhi_ops* ops, uint64_t mmio_phys, uint16_t device_id, dhi_net_info* info) {
    const int family = family_of(device_id);
    if (ops == nullptr || info == nullptr || ops->abi_version != DHI_ABI_VERSION || family < 0 || g_count >= kMaxNics)
        return -1;
    const int32_t rc = g_nics[g_count].init(ops, mmio_phys, Family(family), info);
    if (rc < 0) return rc;
    return g_count++;
}

extern "C" int32_t aero_igc_send(int32_t nic, const void* frame, uint32_t len) {
    if (nic < 0 || nic >= g_count || frame == nullptr) return -1;
    return g_nics[nic].send(frame, len);
}

extern "C" int32_t aero_igc_recv(int32_t nic, void* frame, uint32_t max) {
    if (nic < 0 || nic >= g_count || frame == nullptr) return -1;
    return g_nics[nic].recv(frame, max);
}

extern "C" int32_t aero_igc_link(int32_t nic, uint32_t* speed_mbps) {
    if (nic < 0 || nic >= g_count) return 0;
    if (speed_mbps) *speed_mbps = g_nics[nic].speed();
    return g_nics[nic].link_up() ? 1 : 0;
}
