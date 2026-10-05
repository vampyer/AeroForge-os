// AeroForge xHCI (USB 3) driver (C++20, freestanding), behind the Driver Host
// Interface.
//
// Polling mode: the kernel calls aero_xhci_poll() regularly and the driver
// drains its event ring. At init it resets the controller, enumerates every
// device on the root ports and behind hubs, reads its descriptors and
// configures HID boot-protocol keyboards and mice and bulk-only mass storage
// (USB sticks and drives, SCSI commands). Interrupts (MSI-X), UAS and hotplug
// come later.

#include "dhi.h"

namespace {

// ---- registers (xHCI 1.2, chapter 5) ----

// Capability registers.
constexpr uint32_t kCapLength   = 0x00;
constexpr uint32_t kHcsParams1  = 0x04;
constexpr uint32_t kHcsParams2  = 0x08;
constexpr uint32_t kHccParams1  = 0x10;
constexpr uint32_t kDbOff       = 0x14;
constexpr uint32_t kRtsOff      = 0x18;

// Operational registers, relative to CAPLENGTH.
constexpr uint32_t kUsbCmd  = 0x00;
constexpr uint32_t kUsbSts  = 0x04;
constexpr uint32_t kCrcr    = 0x18;
constexpr uint32_t kDcbaap  = 0x30;
constexpr uint32_t kConfig  = 0x38;
constexpr uint32_t kPortsc0 = 0x400;  // + 0x10 * (port - 1)

constexpr uint32_t kCmdRun   = 1u << 0;
constexpr uint32_t kCmdReset = 1u << 1;
constexpr uint32_t kStsHalted = 1u << 0;
constexpr uint32_t kStsNotReady = 1u << 11;

// PORTSC bits.
constexpr uint32_t kPortConnected = 1u << 0;
constexpr uint32_t kPortEnabled   = 1u << 1;
constexpr uint32_t kPortReset     = 1u << 4;
constexpr uint32_t kPortPower     = 1u << 9;
constexpr uint32_t kPortChangeBits = 0x7Fu << 17;  // CSC..CEC, write 1 to clear

// Interrupter 0, relative to the runtime base.
constexpr uint32_t kIman   = 0x20;
constexpr uint32_t kErstsz = 0x28;
constexpr uint32_t kErstba = 0x30;
constexpr uint32_t kErdp   = 0x38;

// ---- TRBs (chapter 6.4) ----

struct Trb {
    uint32_t d0, d1, d2, d3;
};
static_assert(sizeof(Trb) == 16);

constexpr uint32_t kTrbNormal = 1, kTrbSetup = 2, kTrbData = 3, kTrbStatus = 4, kTrbLink = 6;
constexpr uint32_t kTrbEnableSlot = 9, kTrbAddressDevice = 11, kTrbConfigureEndpoint = 12,
                   kTrbEvaluateContext = 13, kTrbResetEndpoint = 14, kTrbSetTrDequeue = 16;
constexpr uint32_t kEvtTransfer = 32, kEvtCommandDone = 33;

constexpr uint32_t kTrbCycle = 1u << 0;
constexpr uint32_t kTrbToggle = 1u << 1;   // link TRB: toggle cycle
constexpr uint32_t kTrbIsp = 1u << 2;      // interrupt on short packet
constexpr uint32_t kTrbChain = 1u << 4;    // next TRB belongs to the same transfer
constexpr uint32_t kTrbIoc = 1u << 5;      // interrupt on completion
constexpr uint32_t kTrbIdt = 1u << 6;      // immediate data (setup stage)
constexpr uint32_t kTrbDirIn = 1u << 16;

constexpr uint32_t kCcSuccess = 1, kCcStall = 6, kCcShortPacket = 13;

constexpr uint32_t trb_type(uint32_t t) { return t << 10; }

constexpr int kRingTrbs = 256;        // one 4 KiB page per ring
constexpr int kMaxSlots = 16;
constexpr int kMaxDevices = 16;
constexpr int kReportTrbs = 16;       // interrupt-IN transfers kept in flight
constexpr int kReportSize = 16;
constexpr int kEventQueue = 128;

// ---- small helpers ----

void copy_str(char* dst, int cap, const char* src) {
    int i = 0;
    for (; src[i] != 0 && i < cap - 1; ++i) dst[i] = src[i];
    dst[i] = 0;
}

// Minimal log line builder, since drivers have no printf.
class Line {
public:
    Line& s(const char* str) {
        while (*str && len_ < int(sizeof(buf_)) - 1) buf_[len_++] = *str++;
        buf_[len_] = 0;
        return *this;
    }
    Line& u(uint64_t v) {
        char tmp[21];
        int n = 0;
        do { tmp[n++] = char('0' + v % 10); v /= 10; } while (v);
        while (n && len_ < int(sizeof(buf_)) - 1) buf_[len_++] = tmp[--n];
        buf_[len_] = 0;
        return *this;
    }
    Line& x4(uint16_t v) {
        for (int shift = 12; shift >= 0; shift -= 4) {
            const char digits[] = "0123456789abcdef";
            if (len_ < int(sizeof(buf_)) - 1) buf_[len_++] = digits[(v >> shift) & 0xF];
        }
        buf_[len_] = 0;
        return *this;
    }
    const char* str() const { return buf_; }

private:
    char buf_[128] = {};
    int len_ = 0;
};

// A producer ring (command or transfer) of kRingTrbs TRBs, the last one a link
// back to the start.
class Ring {
public:
    bool init(const dhi_ops* ops) {
        if (ops->dma_alloc(kRingTrbs * sizeof(Trb), &mem_) != 0) return false;
        trbs_ = static_cast<volatile Trb*>(mem_.virt);
        volatile Trb& link = trbs_[kRingTrbs - 1];
        link.d0 = uint32_t(mem_.phys);
        link.d1 = uint32_t(mem_.phys >> 32);
        link.d2 = 0;
        link.d3 = trb_type(kTrbLink) | kTrbToggle;
        return true;
    }

    // Writes one TRB and returns its physical address.
    uint64_t push(uint32_t d0, uint32_t d1, uint32_t d2, uint32_t d3) {
        const uint64_t phys = mem_.phys + uint64_t(enqueue_) * sizeof(Trb);
        volatile Trb& t = trbs_[enqueue_];
        t.d0 = d0;
        t.d1 = d1;
        t.d2 = d2;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        t.d3 = (d3 & ~kTrbCycle) | cycle_;  // hand it to the controller last
        if (++enqueue_ == kRingTrbs - 1) {
            volatile Trb& link = trbs_[kRingTrbs - 1];
            link.d3 = (link.d3 & ~kTrbCycle) | cycle_;
            cycle_ ^= 1;
            enqueue_ = 0;
        }
        return phys;
    }

    uint64_t phys() const { return mem_.phys; }
    // Where the next TRB goes, with the cycle bit it will carry (for Set TR Dequeue Pointer).
    uint64_t dequeue_for_reset() const { return mem_.phys + uint64_t(enqueue_) * sizeof(Trb) | cycle_; }
    int index_of(uint64_t phys) const { return int((phys - mem_.phys) / sizeof(Trb)); }
    int enqueue_index() const { return enqueue_; }

private:
    dhi_dma mem_{};
    volatile Trb* trbs_ = nullptr;
    int enqueue_ = 0;
    uint32_t cycle_ = 1;
};

enum class HidKind : uint8_t { None, Keyboard, Mouse };

struct Device {
    dhi_usb_device info{};
    uint8_t slot = 0;
    Ring ep0{};
    dhi_dma in_ctx{}, out_ctx{};

    // Boot-protocol HID interface, if any.
    HidKind hid = HidKind::None;
    uint8_t dci = 0;
    Ring intr{};
    dhi_dma reports{};
    uint8_t last_keys[8] = {};

    // Result of the last control transfer, filled in by the event handler.
    volatile bool ctrl_done = false;
    volatile uint32_t ctrl_code = 0;
    volatile uint32_t ctrl_left = 0;

    // Bulk-only mass storage interface (SCSI transparent command set), if any.
    bool storage = false;
    uint8_t ms_iface = 0;
    uint8_t in_addr = 0, out_addr = 0;     // endpoint addresses
    uint8_t in_dci = 0, out_dci = 0;
    uint16_t in_mps = 0, out_mps = 0;
    Ring ring_in{}, ring_out{};
    dhi_dma ms_buf{};                      // CBW at 0, CSW at 64, small replies from 128
    uint32_t tag = 0;
    bool disk_ok = false;
    dhi_block_info disk{};
    char serial[21] = {};
    volatile bool bulk_done = false;
    volatile uint32_t bulk_code = 0;
};

// HID usage (keyboard page) to ASCII, unshifted and shifted, for 0x04..0x38.
constexpr char kHidNormal[] =
    "abcdefghijklmnopqrstuvwxyz1234567890\n\x1b\b\t -=[]\\#;'`,./";
constexpr char kHidShifted[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()\n\x1b\b\t _+{}|~:\"~<>?";
static_assert(sizeof(kHidNormal) == 0x38 - 0x04 + 2);
constexpr uint8_t kHidCapsLock = 0x39;

class Controller {
public:
    int32_t init(const dhi_ops* ops, uint64_t mmio_phys) {
        ops_ = ops;
        base_ = static_cast<volatile uint8_t*>(ops_->map_mmio(mmio_phys, 0x10000));
        if (base_ == nullptr) return -1;

        op_ = base_ + (cap32(kCapLength) & 0xFF);
        db_ = base_ + (cap32(kDbOff) & ~3u);
        rt_ = base_ + (cap32(kRtsOff) & ~0x1Fu);
        const uint32_t hcs1 = cap32(kHcsParams1);
        max_ports_ = int(hcs1 >> 24);
        slots_ = int(hcs1 & 0xFF) < kMaxSlots ? int(hcs1 & 0xFF) : kMaxSlots;
        const uint32_t hcc1 = cap32(kHccParams1);
        ctx_size_ = (hcc1 & (1u << 2)) ? 64 : 32;

        walk_capabilities(hcc1 >> 16);
        if (!reset()) return -2;
        if (!setup_memory()) return -3;

        op32(kUsbCmd) = op32(kUsbCmd) | kCmdRun;
        if (!wait([&] { return (op32(kUsbSts) & kStsHalted) == 0; }, 100000)) return -4;

        Line l;
        l.s("xhci: controller running, ").u(uint64_t(max_ports_)).s(" root ports, ").u(uint64_t(slots_)).s(" slots");
        ops_->log(l.str());

        for (int port = 1; port <= max_ports_ && count_ < kMaxDevices; ++port) probe_port(port);
        return count_;
    }

    int32_t poll(dhi_input_event* out, int32_t max) {
        if (base_ == nullptr) return 0;
        // A disk read on another CPU owns the event ring right now; its
        // events (including key presses) are queued and handed out next time.
        if (__atomic_exchange_n(&busy_, true, __ATOMIC_ACQUIRE)) return 0;
        process_events();
        int32_t n = 0;
        while (n < max && q_tail_ != q_head_) {
            out[n++] = queue_[q_tail_];
            q_tail_ = (q_tail_ + 1) % kEventQueue;
        }
        __atomic_store_n(&busy_, false, __ATOMIC_RELEASE);
        return n;
    }

    // USB disk `index` (counting only mass-storage devices that came up).
    int32_t disk_info(int32_t index, dhi_block_info* out) const {
        for (int i = 0, n = 0; i < count_; ++i) {
            if (!devices_[i].disk_ok) continue;
            if (n++ == index) {
                *out = devices_[i].disk;
                return i;
            }
        }
        return -1;
    }

    int32_t read(int32_t dev, uint64_t lba, uint32_t count, uint64_t buf_phys) {
        if (dev < 0 || dev >= count_ || !devices_[dev].disk_ok) return -1;
        Device& d = devices_[dev];
        if (count == 0 || lba + count > d.disk.block_count || uint64_t(count) * d.disk.block_size > d.disk.max_transfer)
            return -1;
        while (__atomic_exchange_n(&busy_, true, __ATOMIC_ACQUIRE)) __builtin_ia32_pause();
        int rc = -1;
        for (int attempt = 0; attempt < 2 && rc != 0; ++attempt) {
            rc = read_blocks(d, lba, count, buf_phys);
            if (rc == 1) request_sense(d);  // clears the error so the retry can work
        }
        __atomic_store_n(&busy_, false, __ATOMIC_RELEASE);
        return rc == 0 ? 0 : -1;
    }

    int32_t device_info(int32_t index, dhi_usb_device* out) const {
        if (index < 0 || index >= count_) return -1;
        *out = devices_[index].info;
        return 0;
    }

private:
    // ---------------------------------------------------------------- setup

    // Asks the firmware to give up the controller (USB legacy support
    // capability), so it stops emulating PS/2 behind our back.
    // Walks the extended capabilities: takes the controller from the firmware
    // (USB legacy support) and reads which root ports speak USB 2 or USB 3 and
    // what each port speed ID means (supported protocol).
    void walk_capabilities(uint32_t xecp) {
        uint32_t off = xecp * 4;
        for (int guard = 0; off != 0 && guard < 64; ++guard) {
            volatile uint32_t& cap = *reinterpret_cast<volatile uint32_t*>(base_ + off);
            const uint32_t v = cap;
            if ((v & 0xFF) == 1) {
                cap = v | (1u << 24);  // OS owned
                wait([&] { return (cap & (1u << 16)) == 0; }, 100000);  // BIOS released
                // Turn off SMIs the firmware may have left enabled.
                *reinterpret_cast<volatile uint32_t*>(base_ + off + 4) = 0;
            } else if ((v & 0xFF) == 2) {
                read_protocol(off);
            }
            const uint32_t next = (v >> 8) & 0xFF;
            off = next ? off + next * 4 : 0;
        }
    }

    // One supported protocol capability: a range of root ports and, if the
    // controller defines its own port speed IDs (USB 3.1/3.2 hosts often do),
    // the bit rate of each one.
    void read_protocol(uint32_t off) {
        if (protocol_count_ >= kMaxProtocols) return;
        const auto dw = [&](uint32_t i) { return *reinterpret_cast<volatile uint32_t*>(base_ + off + i * 4); };
        Protocol& p = protocols_[protocol_count_];
        p.major = uint8_t(dw(0) >> 24);
        const uint32_t ports = dw(2);
        const uint32_t first = ports & 0xFF, count = (ports >> 8) & 0xFF;
        const uint32_t psic = ports >> 28;
        for (uint32_t i = 0; i < psic; ++i) {
            const uint32_t psi = dw(4 + i);
            const uint32_t psiv = psi & 0xF, exp = (psi >> 4) & 3, mantissa = psi >> 16;
            // Bit rate in Mb/s: mantissa * 10^(3 * exponent) bit/s.
            const uint32_t mbps = exp == 3 ? mantissa * 1000 : exp == 2 ? mantissa : 0;
            if (psiv != 0 && mbps > p.rate_mbps[psiv]) p.rate_mbps[psiv] = mbps;
        }
        for (uint32_t port = first; port < first + count && port < kMaxRootPorts; ++port)
            port_protocol_[port] = uint8_t(protocol_count_ + 1);
        ++protocol_count_;
    }

    // Turns a root port's speed ID into the DHI speed code (1 FS, 2 LS, 3 HS,
    // 4 SuperSpeed 5 Gb/s, 5 SuperSpeed+ 10 Gb/s, 6 SuperSpeed+ 20 Gb/s).
    uint8_t speed_code(int port, uint8_t psiv) const {
        const int idx = port < kMaxRootPorts ? port_protocol_[port] : 0;
        if (idx == 0) return psiv;  // no protocol info: default speed IDs
        const Protocol& p = protocols_[idx - 1];
        const uint32_t mbps = p.rate_mbps[psiv & 0xF];
        if (p.major < 3) {
            if (mbps == 0) return psiv;
            return mbps >= 480 ? 3 : mbps >= 12 ? 1 : 2;
        }
        if (mbps == 0) return psiv >= 5 ? 5 : 4;  // default IDs: 4 = Gen 1, 5 = Gen 2
        return mbps >= 20000 ? 6 : mbps >= 10000 ? 5 : 4;
    }

    bool reset() {
        op32(kUsbCmd) = op32(kUsbCmd) & ~kCmdRun;
        if (!wait([&] { return op32(kUsbSts) & kStsHalted; }, 100000)) return false;
        op32(kUsbCmd) = kCmdReset;
        if (!wait([&] { return (op32(kUsbCmd) & kCmdReset) == 0; }, 100000)) return false;
        return wait([&] { return (op32(kUsbSts) & kStsNotReady) == 0; }, 100000);
    }

    bool setup_memory() {
        op32(kConfig) = uint32_t(slots_);

        if (ops_->dma_alloc(4096, &dcbaa_) != 0) return false;
        auto* dcbaa = static_cast<volatile uint64_t*>(dcbaa_.virt);

        // Scratchpad buffers the controller may ask for.
        const uint32_t hcs2 = cap32(kHcsParams2);
        const uint32_t scratch = ((hcs2 >> 21) & 0x1F) << 5 | (hcs2 >> 27);
        if (scratch > 0) {
            dhi_dma array{};
            if (ops_->dma_alloc(4096, &array) != 0) return false;
            auto* entries = static_cast<volatile uint64_t*>(array.virt);
            for (uint32_t i = 0; i < scratch && i < 512; ++i) {
                dhi_dma page{};
                if (ops_->dma_alloc(4096, &page) != 0) return false;
                entries[i] = page.phys;
            }
            dcbaa[0] = array.phys;
        }
        op64(kDcbaap, dcbaa_.phys);

        if (!commands_.init(ops_)) return false;
        op64(kCrcr, commands_.phys() | 1);  // RCS = 1

        // One event ring segment.
        if (ops_->dma_alloc(kRingTrbs * sizeof(Trb), &events_) != 0) return false;
        if (ops_->dma_alloc(64, &erst_) != 0) return false;
        auto* erst = static_cast<volatile uint32_t*>(erst_.virt);
        erst[0] = uint32_t(events_.phys);
        erst[1] = uint32_t(events_.phys >> 32);
        erst[2] = kRingTrbs;
        rt32(kIman) = rt32(kIman) & ~2u;  // interrupts off: we poll
        rt32(kErstsz) = 1;
        rt64(kErdp, events_.phys);
        rt64(kErstba, erst_.phys);
        return true;
    }

    // ----------------------------------------------------------- enumeration

    void probe_port(int port) {
        uint32_t sc = portsc(port);
        if ((sc & kPortConnected) == 0) return;
        if ((sc & kPortEnabled) == 0) {
            // USB 2 ports need a reset to become enabled; USB 3 ports train on their own.
            set_portsc(port, (sc & kPortPower) | kPortReset);
            wait([&] { return (portsc(port) & kPortReset) == 0; }, 50000);
            ops_->delay_us(20000);
            sc = portsc(port);
            if ((sc & kPortEnabled) == 0) return;
        }
        set_portsc(port, (sc & kPortPower) | (sc & kPortChangeBits));  // clear change bits
        Where at{};
        at.root_port = uint8_t(port);
        at.port = uint8_t(port);
        at.psiv = uint8_t((sc >> 10) & 0xF);
        attach(at, speed_code(port, at.psiv));
    }

    // Where a device sits in the USB tree.
    struct Where {
        uint8_t root_port = 0;   // root hub port the path starts at
        uint32_t route = 0;      // route string: hub port per tier, 4 bits each
        uint8_t depth = 0;       // hubs between the root port and the device
        uint8_t parent_slot = 0; // 0 = root hub
        uint8_t port = 0;        // port on the parent
        uint8_t tt_slot = 0, tt_port = 0;  // transaction translator for LS/FS behind a HS hub
        uint8_t psiv = 0;        // root port speed ID as the controller reported it (0 = use the speed code)
    };

    // Addresses, describes and starts the device at `at`; hubs recurse into their ports.
    void attach(const Where& at, uint8_t speed) {
        if (count_ >= kMaxDevices) return;
        Device& d = devices_[count_];
        d.info.port = at.port;
        d.info.parent_slot = at.parent_slot;
        d.info.speed = speed;
        Line where;
        if (at.parent_slot) where.s("hub ").u(at.parent_slot).s(" port ").u(at.port);
        else where.s("port ").u(at.port);
        if (!address_device(d, at, speed)) {
            ops_->log(Line().s("xhci: ").s(where.str()).s(": device did not take an address").str());
            return;
        }
        if (!read_descriptors(d)) {
            ops_->log(Line().s("xhci: ").s(where.str()).s(": could not read descriptors").str());
            return;
        }
        ++count_;
        if (d.hid != HidKind::None && !start_hid(d)) d.hid = HidKind::None;
        if (d.storage && !start_storage(d)) d.storage = false;

        static const char* const kSpeed[] = {"?", "full speed", "low speed", "high speed", "SuperSpeed", "SuperSpeed+ 10 Gb/s", "SuperSpeed+ 20 Gb/s"};
        Line l;
        l.s("xhci: ").s(where.str()).s(", slot ").u(d.slot).s(": ").x4(d.info.vendor).s(":").x4(d.info.product)
         .s(" \"").s(d.info.name).s("\", ").s(speed < 7 ? kSpeed[speed] : "?");
        if (d.hid == HidKind::Keyboard) l.s(", HID boot keyboard");
        if (d.hid == HidKind::Mouse) l.s(", HID boot mouse");
        if (d.disk_ok) {
            l.s(", disk ").s(d.disk.model).s(", ").u(d.disk.block_count * d.disk.block_size / (1024 * 1024)).s(" MiB");
        } else if (d.info.iface_class == 8) {
            l.s(", mass storage (not usable)");
        }
        const bool hub = d.info.dev_class == 9 || d.info.iface_class == 9;
        if (hub) l.s(", hub");
        ops_->log(l.str());

        if (hub) {
            if (at.depth >= 5) {
                ops_->log("xhci: hubs nested too deep (USB allows 5), ports ignored");
                return;
            }
            start_hub(d, at, speed);
        }
    }

    // ------------------------------------------------------------------ hubs

    // Port status bits (USB 2.0 11.24.2.7; USB 3 hubs share the low ones).
    static constexpr uint16_t kHubPortConnection = 1u << 0;
    static constexpr uint16_t kHubPortEnable = 1u << 1;
    static constexpr uint16_t kHubPortReset = 1u << 4;
    static constexpr uint16_t kHubPortLowSpeed = 1u << 9;
    static constexpr uint16_t kHubPortHighSpeed = 1u << 10;
    static constexpr uint16_t kFeaturePortReset = 4, kFeaturePortPower = 8;
    static constexpr uint16_t kFeatureCConnection = 16, kFeatureCReset = 20, kFeatureBhReset = 28;

    bool hub_port_status(Device& d, int port, const dhi_dma& buf, uint16_t* status, uint16_t* change) {
        if (control(d, 0xA3, 0, 0, uint16_t(port), 4, buf.phys) < 4) return false;
        const auto* b = static_cast<const uint8_t*>(buf.virt);
        *status = uint16_t(b[0] | b[1] << 8);
        *change = uint16_t(b[2] | b[3] << 8);
        return true;
    }

    void start_hub(Device& hub, const Where& at, uint8_t speed) {
        dhi_dma buf{};
        if (ops_->dma_alloc(64, &buf) != 0) return;
        const auto* b = static_cast<const uint8_t*>(buf.virt);
        const bool superspeed = speed >= 4;

        // Hub descriptor: number of ports and power-on delay.
        const int n = control(hub, 0xA0, 6, superspeed ? 0x2A00 : 0x2900, 0, 16, buf.phys);
        if (n < 7) {
            ops_->log("xhci: hub descriptor unreadable, ports ignored");
            ops_->dma_free(&buf);
            return;
        }
        const int ports = b[2] > 15 ? 15 : b[2];
        const uint32_t power_ms = uint32_t(b[5]) * 2;
        const bool mtt = !superspeed && speed == 3 && hub.info.iface_protocol == 2;

        // Tell the controller this slot is a hub (Configure Endpoint, slot context only).
        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(hub.in_ctx.virt)[i] = 0;
        ctx(hub.in_ctx, 0)[1] = 1;
        volatile uint32_t* sl = ctx(hub.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(hub.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        sl[0] |= (1u << 26) | (mtt ? 1u << 25 : 0);
        sl[1] = (sl[1] & 0x00FFFFFF) | (uint32_t(ports) << 24);
        sl[3] = 0;
        command(hub.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(hub.slot) << 24), nullptr);
        if (superspeed) control(hub, 0x20, 12, uint16_t(at.depth), 0, 0, 0);  // SET_HUB_DEPTH

        for (int p = 1; p <= ports; ++p) control(hub, 0x23, 3, kFeaturePortPower, uint16_t(p), 0, 0);
        ops_->delay_us((power_ms > 100 ? power_ms : 100) * 1000);

        for (int p = 1; p <= ports && count_ < kMaxDevices; ++p) {
            uint16_t status = 0, change = 0;
            if (!hub_port_status(hub, p, buf, &status, &change) || !(status & kHubPortConnection)) continue;
            control(hub, 0x23, 1, kFeatureCConnection, uint16_t(p), 0, 0);
            control(hub, 0x23, 3, superspeed ? kFeatureBhReset : kFeaturePortReset, uint16_t(p), 0, 0);
            bool enabled = false;
            for (int i = 0; i < 50 && !enabled; ++i) {
                ops_->delay_us(10000);
                if (hub_port_status(hub, p, buf, &status, &change))
                    enabled = !(status & kHubPortReset) && (status & kHubPortEnable || superspeed);
            }
            control(hub, 0x23, 1, kFeatureCReset, uint16_t(p), 0, 0);
            if (!enabled) continue;
            ops_->delay_us(10000);  // reset recovery

            uint8_t child_speed = superspeed ? 4 : (status & kHubPortLowSpeed) ? 2 : (status & kHubPortHighSpeed) ? 3 : 1;
            Where child{};
            child.root_port = at.root_port;
            child.route = at.route | (uint32_t(p) << (4 * at.depth));
            child.depth = uint8_t(at.depth + 1);
            child.parent_slot = hub.slot;
            child.port = uint8_t(p);
            if (speed == 3 && child_speed < 3) {
                // Low/full speed device behind a high-speed hub: that hub translates.
                child.tt_slot = hub.slot;
                child.tt_port = uint8_t(p);
            } else {
                child.tt_slot = at.tt_slot;
                child.tt_port = at.tt_port;
            }
            attach(child, child_speed);
        }
        ops_->dma_free(&buf);
    }

    volatile uint32_t* ctx(const dhi_dma& mem, int index) const {
        return reinterpret_cast<volatile uint32_t*>(static_cast<uint8_t*>(mem.virt) + index * ctx_size_);
    }

    bool address_device(Device& d, const Where& at, uint8_t speed) {
        uint32_t slot = 0;
        if (command(0, 0, trb_type(kTrbEnableSlot), &slot) != kCcSuccess || slot == 0 || int(slot) > slots_)
            return false;
        d.slot = uint8_t(slot);
        d.info.slot = d.slot;

        if (ops_->dma_alloc(4096, &d.out_ctx) != 0 || ops_->dma_alloc(4096, &d.in_ctx) != 0) return false;
        if (!d.ep0.init(ops_)) return false;
        static_cast<volatile uint64_t*>(dcbaa_.virt)[slot] = d.out_ctx.phys;

        // Input context: [0] control, [1] slot, [2] EP0.
        volatile uint32_t* control = ctx(d.in_ctx, 0);
        control[1] = 0b11;  // add slot + EP0
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        sl[0] = (1u << 27) | (uint32_t(at.psiv ? at.psiv : speed) << 20) | (at.route & 0xFFFFF);  // one context entry
        sl[1] = uint32_t(at.root_port) << 16;                                  // root hub port
        sl[2] = uint32_t(at.tt_slot) | uint32_t(at.tt_port) << 8;              // TT for LS/FS behind a HS hub
        volatile uint32_t* ep = ctx(d.in_ctx, 2);
        const uint32_t mps = speed >= 4 ? 512 : speed == 3 ? 64 : 8;
        ep[1] = (mps << 16) | (4u << 3) | (3u << 1);  // control endpoint, 3 retries
        ep[2] = uint32_t(d.ep0.phys()) | 1;            // dequeue pointer, DCS = 1
        ep[3] = uint32_t(d.ep0.phys() >> 32);
        ep[4] = 8;                                     // average TRB length

        return command(d.in_ctx.phys, 0, trb_type(kTrbAddressDevice) | (slot << 24), nullptr) == kCcSuccess;
    }

    // Control transfer on EP0. Returns bytes received, or -1.
    int control(Device& d, uint8_t req_type, uint8_t request, uint16_t value, uint16_t index,
                uint16_t length, uint64_t buf_phys) {
        const bool in = (req_type & 0x80) != 0;
        const uint32_t trt = length == 0 ? 0 : in ? 3 : 2;
        d.ep0.push(uint32_t(req_type) | uint32_t(request) << 8 | uint32_t(value) << 16,
                   uint32_t(index) | uint32_t(length) << 16, 8,
                   trb_type(kTrbSetup) | kTrbIdt | (trt << 16));
        if (length > 0)
            d.ep0.push(uint32_t(buf_phys), uint32_t(buf_phys >> 32), length,
                       trb_type(kTrbData) | (in ? kTrbDirIn : 0));
        d.ctrl_done = false;
        d.ep0.push(0, 0, 0, trb_type(kTrbStatus) | kTrbIoc | ((length > 0 && in) ? 0 : kTrbDirIn));
        ring_doorbell(d.slot, 1);
        if (!wait([&] { process_events(); return d.ctrl_done; }, 200000)) return -1;
        if (d.ctrl_code != kCcSuccess && d.ctrl_code != kCcShortPacket) return -1;
        return int(length) - int(d.ctrl_left);
    }

    int get_descriptor(Device& d, uint8_t type, uint8_t index, uint16_t lang, uint16_t length, const dhi_dma& buf) {
        return control(d, 0x80, 6, uint16_t(type) << 8 | index, lang, length, buf.phys);
    }

    void read_string(Device& d, uint8_t index, const dhi_dma& buf, char* out, int cap) {
        out[0] = 0;
        if (index == 0) return;
        const int n = get_descriptor(d, 3, index, 0x0409, 255, buf);
        const auto* b = static_cast<const uint8_t*>(buf.virt);
        if (n < 2 || b[1] != 3) return;
        int len = 0;
        for (int i = 2; i + 1 < n && i < b[0] && len < cap - 1; i += 2)
            out[len++] = (b[i + 1] == 0 && b[i] >= 0x20 && b[i] < 0x7F) ? char(b[i]) : '?';
        while (len > 0 && out[len - 1] == ' ') --len;
        out[len] = 0;
    }

    bool read_descriptors(Device& d) {
        dhi_dma buf{};
        if (ops_->dma_alloc(512, &buf) != 0) return false;
        const auto* b = static_cast<const uint8_t*>(buf.virt);
        bool ok = false;

        do {
            if (get_descriptor(d, 1, 0, 0, 8, buf) < 8) break;
            const uint8_t mps0 = b[7];
            if (d.info.speed < 3 && mps0 != 8 && mps0 != 0) fix_ep0_packet_size(d, mps0);
            if (get_descriptor(d, 1, 0, 0, 18, buf) < 18) break;
            d.info.vendor = uint16_t(b[8] | b[9] << 8);
            d.info.product = uint16_t(b[10] | b[11] << 8);
            d.info.dev_class = b[4];
            const uint8_t i_manufacturer = b[14], i_product = b[15], i_serial = b[16];

            if (get_descriptor(d, 2, 0, 0, 9, buf) < 9) break;
            uint16_t total = uint16_t(b[2] | b[3] << 8);
            if (total > 512) total = 512;
            const int got = get_descriptor(d, 2, 0, 0, total, buf);
            if (got < 9) break;
            config_value_ = b[5];
            parse_config(d, b, got);

            char product[32];
            read_string(d, i_product, buf, product, sizeof(product));
            if (product[0] == 0) read_string(d, i_manufacturer, buf, product, sizeof(product));
            copy_str(d.info.name, sizeof(d.info.name), product[0] ? product : "(no name)");
            read_string(d, i_serial, buf, d.serial, sizeof(d.serial));

            // Select the first configuration.
            ok = control(d, 0x00, 9, config_value_, 0, 0, 0) >= 0;
        } while (false);

        ops_->dma_free(&buf);
        return ok;
    }

    void fix_ep0_packet_size(Device& d, uint8_t mps) {
        volatile uint32_t* control = ctx(d.in_ctx, 0);
        control[0] = 0;
        control[1] = 0b10;  // evaluate EP0
        volatile uint32_t* ep = ctx(d.in_ctx, 2);
        ep[1] = (ep[1] & 0xFFFF) | (uint32_t(mps) << 16);
        command(d.in_ctx.phys, 0, trb_type(kTrbEvaluateContext) | (uint32_t(d.slot) << 24), nullptr);
    }

    // Finds the first HID boot keyboard or mouse interface and its interrupt-IN endpoint.
    void parse_config(Device& d, const uint8_t* b, int len) {
        bool in_boot_hid = false, in_storage = false;
        for (int off = 0; off + 2 <= len && b[off] >= 2; off += b[off]) {
            const uint8_t type = b[off + 1];
            if (type == 4 && off + 9 <= len) {  // interface
                if (d.info.iface_class == 0) {
                    d.info.iface_class = b[off + 5];
                    d.info.iface_subclass = b[off + 6];
                    d.info.iface_protocol = b[off + 7];
                }
                in_boot_hid = d.hid == HidKind::None && b[off + 5] == 3 && b[off + 6] == 1 &&
                              (b[off + 7] == 1 || b[off + 7] == 2);
                if (in_boot_hid) {
                    hid_iface_ = b[off + 2];
                    hid_kind_ = b[off + 7] == 1 ? HidKind::Keyboard : HidKind::Mouse;
                }
                // Mass storage, SCSI transparent command set, bulk-only transport.
                in_storage = !d.storage && b[off + 5] == 8 && b[off + 6] == 6 && b[off + 7] == 0x50;
                if (in_storage) {
                    d.ms_iface = b[off + 2];
                    d.in_addr = d.out_addr = 0;
                }
            } else if (type == 5 && off + 7 <= len && in_storage) {  // bulk endpoint
                const uint8_t addr = b[off + 2], attr = b[off + 3];
                const uint16_t mps = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                if ((attr & 3) == 2) {
                    if (addr & 0x80) {
                        d.in_addr = addr;
                        d.in_dci = uint8_t((addr & 0xF) * 2 + 1);
                        d.in_mps = mps;
                    } else {
                        d.out_addr = addr;
                        d.out_dci = uint8_t((addr & 0xF) * 2);
                        d.out_mps = mps;
                    }
                    if (d.in_addr && d.out_addr) {
                        d.storage = true;
                        in_storage = false;
                    }
                }
            } else if (type == 5 && off + 7 <= len && in_boot_hid) {  // endpoint
                const uint8_t addr = b[off + 2], attr = b[off + 3];
                if ((addr & 0x80) && (attr & 3) == 3) {
                    d.hid = hid_kind_;
                    d.dci = uint8_t((addr & 0xF) * 2 + 1);
                    hid_mps_ = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                    hid_interval_ = b[off + 6];
                    hid_iface_of_device_ = hid_iface_;
                    in_boot_hid = false;
                }
            }
        }
    }

    bool start_hid(Device& d) {
        const uint8_t iface = hid_iface_of_device_;
        control(d, 0x21, 0x0B, 0, iface, 0, 0);  // SET_PROTOCOL(boot)
        control(d, 0x21, 0x0A, 0, iface, 0, 0);  // SET_IDLE(0): report only on change

        if (!d.intr.init(ops_) || ops_->dma_alloc(kReportTrbs * kReportSize, &d.reports) != 0) return false;

        // Interval as 2^n * 125 us: full/low speed give frames (1 ms), high speed and up an exponent.
        uint32_t interval = 0;
        if (d.info.speed >= 3) {
            interval = hid_interval_ ? hid_interval_ - 1u : 0u;
        } else {
            uint32_t micro = uint32_t(hid_interval_ ? hid_interval_ : 1) * 8;
            while ((2u << interval) <= micro) ++interval;
        }

        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
        volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
        control_ctx[1] = 1u | (1u << d.dci);  // slot + the new endpoint
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        sl[0] = (sl[0] & ~(0x1Fu << 27)) | (uint32_t(d.dci) << 27);
        sl[3] = 0;
        volatile uint32_t* ep = ctx(d.in_ctx, 1 + d.dci);
        ep[0] = interval << 16;
        ep[1] = (uint32_t(hid_mps_) << 16) | (7u << 3) | (3u << 1);  // interrupt IN, 3 retries
        ep[2] = uint32_t(d.intr.phys()) | 1;
        ep[3] = uint32_t(d.intr.phys() >> 32);
        ep[4] = (uint32_t(hid_mps_) << 16) | hid_mps_;  // max ESIT payload, average TRB length
        if (command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr) != kCcSuccess)
            return false;

        report_len_[d.slot] = uint8_t(hid_mps_ < kReportSize ? hid_mps_ : kReportSize);
        for (int i = 0; i < kReportTrbs; ++i) queue_report(d);
        return true;
    }

    // ------------------------------------------------------- mass storage

    bool start_storage(Device& d) {
        if (!d.ring_in.init(ops_) || !d.ring_out.init(ops_) || ops_->dma_alloc(4096, &d.ms_buf) != 0) return false;

        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
        volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
        control_ctx[1] = 1u | (1u << d.in_dci) | (1u << d.out_dci);
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        const uint32_t last = d.in_dci > d.out_dci ? d.in_dci : d.out_dci;
        sl[0] = (sl[0] & ~(0x1Fu << 27)) | (last << 27);
        sl[3] = 0;
        const auto setup = [&](uint8_t dci, uint16_t mps, uint32_t ep_type, const Ring& ring) {
            volatile uint32_t* ep = ctx(d.in_ctx, 1 + dci);
            ep[0] = 0;
            ep[1] = (uint32_t(mps) << 16) | (ep_type << 3) | (3u << 1);  // 3 retries
            ep[2] = uint32_t(ring.phys()) | 1;
            ep[3] = uint32_t(ring.phys() >> 32);
            ep[4] = 3072;  // average TRB length, as the spec suggests for bulk
        };
        setup(d.out_dci, d.out_mps, 2, d.ring_out);  // bulk OUT
        setup(d.in_dci, d.in_mps, 6, d.ring_in);     // bulk IN
        if (command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr) != kCcSuccess)
            return false;

        auto* buf = static_cast<uint8_t*>(d.ms_buf.virt);
        const uint64_t reply = d.ms_buf.phys + 128;

        // INQUIRY: only direct-access block devices (not CD drives or card readers' empty slots).
        const uint8_t inquiry[6] = {0x12, 0, 0, 0, 36, 0};
        if (scsi(d, inquiry, 6, true, reply, 36) != 0 || (buf[128] & 0x1F) != 0) return false;
        char model[41];
        int m = 0;
        for (int i = 8; i < 32 && m < 40; ++i) {
            const char c = char(buf[128 + i]);
            if (i == 16 && m > 0 && model[m - 1] != ' ') model[m++] = ' ';
            if (c >= 0x20 && c < 0x7F && !(c == ' ' && (m == 0 || model[m - 1] == ' '))) model[m++] = c;
        }
        while (m > 0 && model[m - 1] == ' ') --m;
        model[m] = 0;
        char rev[9];
        for (int i = 0; i < 4; ++i) rev[i] = char(buf[160 + i]) >= 0x20 ? char(buf[160 + i]) : ' ';
        rev[4] = 0;

        // Sticks often report "not ready" (unit attention) for a while after reset.
        bool ready = false;
        for (int i = 0; i < 30 && !ready; ++i) {
            const uint8_t tur[6] = {0x00, 0, 0, 0, 0, 0};
            const int rc = scsi(d, tur, 6, false, 0, 0);
            ready = rc == 0;
            if (rc == 1) request_sense(d);
            if (!ready) ops_->delay_us(100000);
        }
        if (!ready) return false;

        const uint8_t cap10[10] = {0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0};
        if (scsi(d, cap10, 10, true, reply, 8) != 0) return false;
        uint64_t last_lba = be32(buf + 128);
        uint32_t block_size = be32(buf + 132);
        if (last_lba == 0xFFFFFFFF) {  // over 2 TiB: READ CAPACITY(16)
            const uint8_t cap16[16] = {0x9E, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0};
            if (scsi(d, cap16, 16, true, reply, 32) != 0) return false;
            last_lba = uint64_t(be32(buf + 128)) << 32 | be32(buf + 132);
            block_size = be32(buf + 136);
        }
        if (block_size < 512 || block_size > 4096 || (block_size & (block_size - 1))) return false;

        d.disk.block_count = last_lba + 1;
        d.disk.block_size = block_size;
        d.disk.max_transfer = 32768;  // one bounce buffer, never crosses a 64 KiB boundary
        copy_str(d.disk.model, sizeof(d.disk.model), model[0] ? model : d.info.name);
        copy_str(d.disk.serial, sizeof(d.disk.serial), d.serial);
        copy_str(d.disk.firmware, sizeof(d.disk.firmware), rev);
        d.disk_ok = true;
        return true;
    }

    static uint32_t be32(const uint8_t* p) {
        return uint32_t(p[0]) << 24 | uint32_t(p[1]) << 16 | uint32_t(p[2]) << 8 | p[3];
    }

    // READ(10), or READ(16) past 2^32 blocks. 0 = ok, 1 = check condition, -1 = transport error.
    int read_blocks(Device& d, uint64_t lba, uint32_t count, uint64_t buf_phys) {
        uint8_t cdb[16] = {};
        int len;
        if (lba + count > 0xFFFFFFFFull) {
            cdb[0] = 0x88;
            for (int i = 0; i < 8; ++i) cdb[2 + i] = uint8_t(lba >> (56 - 8 * i));
            for (int i = 0; i < 4; ++i) cdb[10 + i] = uint8_t(count >> (24 - 8 * i));
            len = 16;
        } else {
            cdb[0] = 0x28;
            for (int i = 0; i < 4; ++i) cdb[2 + i] = uint8_t(lba >> (24 - 8 * i));
            cdb[7] = uint8_t(count >> 8);
            cdb[8] = uint8_t(count);
            len = 10;
        }
        return scsi(d, cdb, len, true, buf_phys, count * d.disk.block_size);
    }

    void request_sense(Device& d) {
        const uint8_t sense[6] = {0x03, 0, 0, 0, 18, 0};
        scsi(d, sense, 6, true, d.ms_buf.phys + 256, 18);
    }

    // One bulk-only transport command: CBW out, optional data stage, CSW in.
    // Returns the CSW status (0 passed, 1 failed) or -1 on a transport error.
    int scsi(Device& d, const uint8_t* cdb, int cdb_len, bool in, uint64_t data_phys, uint32_t data_len) {
        auto* cbw = static_cast<uint8_t*>(d.ms_buf.virt);
        for (int i = 0; i < 31; ++i) cbw[i] = 0;
        const uint32_t tag = ++d.tag;
        put32(cbw, 0x43425355);  // "USBC"
        put32(cbw + 4, tag);
        put32(cbw + 8, data_len);
        cbw[12] = in ? 0x80 : 0x00;
        cbw[14] = uint8_t(cdb_len);
        for (int i = 0; i < cdb_len; ++i) cbw[15 + i] = cdb[i];

        if (bulk(d, false, d.ms_buf.phys, 31) != kCcSuccess) {
            reset_recovery(d);
            return -1;
        }
        if (data_len > 0) {
            const uint32_t code = bulk(d, in, data_phys, data_len);
            if (code == kCcStall) {
                clear_halt(d, in);  // the device refused the data; the CSW says why
            } else if (code != kCcSuccess && code != kCcShortPacket) {
                reset_recovery(d);
                return -1;
            }
        }
        const uint8_t* csw = cbw + 64;
        uint32_t code = bulk(d, true, d.ms_buf.phys + 64, 13);
        if (code == kCcStall) {
            clear_halt(d, true);
            code = bulk(d, true, d.ms_buf.phys + 64, 13);
        }
        if ((code != kCcSuccess && code != kCcShortPacket) || get32(csw) != 0x53425355 || get32(csw + 4) != tag ||
            csw[12] > 1) {
            reset_recovery(d);  // phase error or garbage: start over
            return -1;
        }
        return csw[12];
    }

    // Queues one bulk transfer (split at 64 KiB boundaries) and waits for it.
    // Returns the completion code.
    uint32_t bulk(Device& d, bool in, uint64_t phys, uint32_t len) {
        Ring& ring = in ? d.ring_in : d.ring_out;
        const uint16_t mps = in ? d.in_mps : d.out_mps;
        d.bulk_done = false;
        uint32_t left = len;
        while (left > 0) {
            uint32_t chunk = 0x10000 - uint32_t(phys & 0xFFFF);
            if (chunk > left) chunk = left;
            left -= chunk;
            // TD size: packets still to come after this TRB (capped at 31).
            uint32_t td_size = mps ? (left + mps - 1) / mps : 0;
            if (td_size > 31) td_size = 31;
            ring.push(uint32_t(phys), uint32_t(phys >> 32), chunk | (td_size << 17),
                      trb_type(kTrbNormal) | kTrbIsp | (left > 0 ? kTrbChain : kTrbIoc));
            phys += chunk;
        }
        ring_doorbell(d.slot, in ? d.in_dci : d.out_dci);
        if (!wait([&] { process_events(); return d.bulk_done; }, 1000000)) return 0;  // 10 s
        return d.bulk_code;
    }

    // Clears a halted bulk endpoint on both sides: the controller's ring and the device.
    void clear_halt(Device& d, bool in) {
        const uint32_t dci = in ? d.in_dci : d.out_dci;
        const Ring& ring = in ? d.ring_in : d.ring_out;
        command(0, 0, trb_type(kTrbResetEndpoint) | (dci << 16) | (uint32_t(d.slot) << 24), nullptr);
        command(ring.dequeue_for_reset(), 0, trb_type(kTrbSetTrDequeue) | (dci << 16) | (uint32_t(d.slot) << 24), nullptr);
        control(d, 0x02, 1, 0, in ? d.in_addr : d.out_addr, 0, 0);  // CLEAR_FEATURE(ENDPOINT_HALT)
    }

    // Bulk-only mass storage reset, then clear both endpoints (BOT 5.3.4).
    void reset_recovery(Device& d) {
        control(d, 0x21, 0xFF, 0, d.ms_iface, 0, 0);
        clear_halt(d, true);
        clear_halt(d, false);
    }

    static void put32(uint8_t* p, uint32_t v) {
        for (int i = 0; i < 4; ++i) p[i] = uint8_t(v >> (8 * i));
    }
    static uint32_t get32(const uint8_t* p) {
        return uint32_t(p[0]) | uint32_t(p[1]) << 8 | uint32_t(p[2]) << 16 | uint32_t(p[3]) << 24;
    }

    // Queues one interrupt-IN transfer; its buffer slot follows the TRB's ring index.
    void queue_report(Device& d) {
        const int slot = d.intr.enqueue_index() % kReportTrbs;
        const uint64_t phys = d.reports.phys + uint64_t(slot) * kReportSize;
        d.intr.push(uint32_t(phys), uint32_t(phys >> 32), report_len_[d.slot],
                    trb_type(kTrbNormal) | kTrbIoc | kTrbIsp);
        ring_doorbell(d.slot, d.dci);
    }

    // --------------------------------------------------------------- events

    uint32_t command(uint64_t ptr, uint32_t d2, uint32_t d3, uint32_t* slot_out) {
        cmd_done_ = false;
        cmd_trb_ = commands_.push(uint32_t(ptr), uint32_t(ptr >> 32), d2, d3);
        ring_doorbell(0, 0);
        if (!wait([&] { process_events(); return cmd_done_; }, 200000)) return 0;
        if (slot_out) *slot_out = cmd_slot_;
        return cmd_code_;
    }

    void process_events() {
        auto* ring = static_cast<volatile Trb*>(events_.virt);
        bool any = false;
        for (;;) {
            volatile Trb& t = ring[ev_index_];
            const uint32_t d3 = t.d3;
            if ((d3 & kTrbCycle) != ev_cycle_) break;
            __atomic_thread_fence(__ATOMIC_ACQUIRE);
            handle_event(t.d0 | uint64_t(t.d1) << 32, t.d2, d3);
            any = true;
            if (++ev_index_ == kRingTrbs) {
                ev_index_ = 0;
                ev_cycle_ ^= 1;
            }
        }
        if (any) rt64(kErdp, (events_.phys + uint64_t(ev_index_) * sizeof(Trb)) | (1u << 3));
    }

    void handle_event(uint64_t ptr, uint32_t d2, uint32_t d3) {
        const uint32_t type = (d3 >> 10) & 0x3F;
        const uint32_t code = d2 >> 24;
        const uint8_t slot = uint8_t(d3 >> 24);
        if (type == kEvtCommandDone) {
            if (ptr == cmd_trb_) {
                cmd_code_ = code;
                cmd_slot_ = slot;
                cmd_done_ = true;
            }
            return;
        }
        if (type != kEvtTransfer) return;  // port status changes: hotplug comes later
        Device* d = by_slot(slot);
        if (d == nullptr) return;
        const uint8_t ep = uint8_t((d3 >> 16) & 0x1F);
        if (ep == 1) {
            d->ctrl_code = code;
            d->ctrl_left = d2 & 0xFFFFFF;
            d->ctrl_done = true;
        } else if (d->storage && (ep == d->in_dci || ep == d->out_dci)) {
            d->bulk_code = code;
            d->bulk_done = true;
        } else if (ep == d->dci) {
            if (code == kCcSuccess || code == kCcShortPacket) {
                const int index = d->intr.index_of(ptr) % kReportTrbs;
                const auto* report = static_cast<const uint8_t*>(d->reports.virt) + index * kReportSize;
                if (d->hid == HidKind::Keyboard) keyboard_report(*d, report);
                else mouse_report(report);
            }
            queue_report(*d);
        }
    }

    Device* by_slot(uint8_t slot) {
        for (int i = 0; i < kMaxDevices; ++i)
            if (devices_[i].slot == slot && slot != 0) return &devices_[i];
        return nullptr;
    }

    // Boot keyboard report: modifiers, reserved, up to six pressed usages.
    void keyboard_report(Device& d, const uint8_t* r) {
        const uint8_t mods = r[0];
        const bool shift = (mods & 0x22) != 0;
        uint8_t modifiers = (shift ? DHI_MOD_SHIFT : 0) | ((mods & 0x11) ? DHI_MOD_CTRL : 0) |
                            ((mods & 0x44) ? DHI_MOD_ALT : 0) | (caps_ ? DHI_MOD_CAPS : 0);
        for (int i = 2; i < 8; ++i) {
            const uint8_t usage = r[i];
            if (usage < 4) continue;  // none, or rollover error
            bool held = false;
            for (int j = 2; j < 8; ++j) held |= d.last_keys[j] == usage;
            if (held) continue;
            if (usage == kHidCapsLock) {
                caps_ = !caps_;
                modifiers ^= DHI_MOD_CAPS;
            }
            char c = 0;
            if (usage >= 0x04 && usage <= 0x38) c = (shift ? kHidShifted : kHidNormal)[usage - 0x04];
            if (caps_ && c >= 'a' && c <= 'z') c = char(c - 'a' + 'A');
            else if (caps_ && c >= 'A' && c <= 'Z') c = char(c - 'A' + 'a');
            dhi_input_event ev{};
            ev.kind = DHI_INPUT_KEY;
            ev.key.scancode = usage;
            ev.key.pressed = 1;
            ev.key.ascii = uint8_t(c);
            ev.key.modifiers = modifiers;
            enqueue(ev);
        }
        for (int i = 0; i < 8; ++i) d.last_keys[i] = r[i];
    }

    // Boot mouse report: buttons, dx, dy.
    void mouse_report(const uint8_t* r) {
        dhi_input_event ev{};
        ev.kind = DHI_INPUT_MOUSE;
        ev.buttons = r[0] & 7;
        ev.dx = int8_t(r[1]);
        ev.dy = int8_t(r[2]);
        enqueue(ev);
    }

    void enqueue(const dhi_input_event& ev) {
        const int next = (q_head_ + 1) % kEventQueue;
        if (next == q_tail_) return;  // full: drop
        queue_[q_head_] = ev;
        q_head_ = next;
    }

    // ------------------------------------------------------------ registers

    uint32_t cap32(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(base_ + off); }
    volatile uint32_t& op32(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(op_ + off); }
    volatile uint32_t& rt32(uint32_t off) const { return *reinterpret_cast<volatile uint32_t*>(rt_ + off); }
    void op64(uint32_t off, uint64_t v) const {
        op32(off) = uint32_t(v);
        op32(off + 4) = uint32_t(v >> 32);
    }
    void rt64(uint32_t off, uint64_t v) const {
        rt32(off) = uint32_t(v);
        rt32(off + 4) = uint32_t(v >> 32);
    }
    uint32_t portsc(int port) const { return op32(kPortsc0 + 0x10 * uint32_t(port - 1)); }
    void set_portsc(int port, uint32_t v) const { op32(kPortsc0 + 0x10 * uint32_t(port - 1)) = v; }
    void ring_doorbell(uint32_t slot, uint32_t target) const {
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        *reinterpret_cast<volatile uint32_t*>(db_ + slot * 4) = target;
    }

    // Spins (10 us steps) until `done` is true; false on timeout.
    template <typename F>
    bool wait(F done, int steps) const {
        for (int i = 0; i < steps; ++i) {
            if (done()) return true;
            ops_->delay_us(10);
        }
        return done();
    }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t *base_ = nullptr, *op_ = nullptr, *db_ = nullptr, *rt_ = nullptr;
    int max_ports_ = 0, slots_ = 0, ctx_size_ = 32;
    struct Protocol {
        uint8_t major = 0;
        uint32_t rate_mbps[16] = {};  // by port speed ID, 0 = default meaning
    };
    static constexpr int kMaxProtocols = 8, kMaxRootPorts = 256;
    Protocol protocols_[kMaxProtocols]{};
    int protocol_count_ = 0;
    uint8_t port_protocol_[kMaxRootPorts] = {};  // 1-based index into protocols_, 0 = unknown
    dhi_dma dcbaa_{}, events_{}, erst_{};
    Ring commands_{};
    int ev_index_ = 0;
    uint32_t ev_cycle_ = 1;

    volatile bool cmd_done_ = false;
    uint64_t cmd_trb_ = 0;
    uint32_t cmd_code_ = 0, cmd_slot_ = 0;

    Device devices_[kMaxDevices]{};
    int count_ = 0;
    uint8_t report_len_[kMaxSlots + 1] = {};

    // Scratch state while parsing one configuration descriptor.
    uint8_t config_value_ = 0, hid_iface_ = 0, hid_iface_of_device_ = 0, hid_interval_ = 0;
    HidKind hid_kind_ = HidKind::None;
    uint16_t hid_mps_ = 8;

    bool caps_ = false;
    bool busy_ = false;  // event ring owner: a disk read or the poll thread
    dhi_input_event queue_[kEventQueue]{};
    int q_head_ = 0, q_tail_ = 0;
};

constexpr int kMaxControllers = 4;
constinit Controller g_controllers[kMaxControllers]{};
constinit int32_t g_count = 0;

}  // namespace

extern "C" int32_t aero_xhci_init(const dhi_ops* ops, uint64_t mmio_phys, int32_t* devices) {
    if (ops == nullptr || devices == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
    if (g_count >= kMaxControllers) return -1;
    const int32_t n = g_controllers[g_count].init(ops, mmio_phys);
    if (n < 0) return n;
    *devices = n;
    return g_count++;
}

extern "C" int32_t aero_xhci_poll(int32_t ctrl, dhi_input_event* out, int32_t max) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return 0;
    return g_controllers[ctrl].poll(out, max);
}

extern "C" int32_t aero_xhci_disk(int32_t ctrl, int32_t index, dhi_block_info* out) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return -1;
    const int32_t dev = g_controllers[ctrl].disk_info(index, out);
    return dev < 0 ? -1 : ctrl * kMaxDevices + dev;
}

extern "C" int32_t aero_xhci_read(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (disk < 0 || disk / kMaxDevices >= g_count) return -1;
    return g_controllers[disk / kMaxDevices].read(disk % kMaxDevices, lba, count, buf_phys);
}

extern "C" int32_t aero_xhci_device(int32_t ctrl, int32_t index, dhi_usb_device* out) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return -1;
    return g_controllers[ctrl].device_info(index, out);
}
