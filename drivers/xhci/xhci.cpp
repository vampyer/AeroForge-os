// AeroForge xHCI (USB 3) driver (C++20, freestanding), behind the Driver Host
// Interface.
//
// Polling mode: the kernel calls aero_xhci_poll() regularly and the driver
// drains its event ring. At init it resets the controller, enumerates every
// device on the root ports (no hubs yet), reads its descriptors and configures
// HID boot-protocol keyboards and mice. Hubs, mass storage, interrupts (MSI-X)
// and hotplug come later.

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
                   kTrbEvaluateContext = 13;
constexpr uint32_t kEvtTransfer = 32, kEvtCommandDone = 33;

constexpr uint32_t kTrbCycle = 1u << 0;
constexpr uint32_t kTrbToggle = 1u << 1;   // link TRB: toggle cycle
constexpr uint32_t kTrbIsp = 1u << 2;      // interrupt on short packet
constexpr uint32_t kTrbIoc = 1u << 5;      // interrupt on completion
constexpr uint32_t kTrbIdt = 1u << 6;      // immediate data (setup stage)
constexpr uint32_t kTrbDirIn = 1u << 16;

constexpr uint32_t kCcSuccess = 1, kCcShortPacket = 13;

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

        take_ownership(hcc1 >> 16);
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
        process_events();
        int32_t n = 0;
        while (n < max && q_tail_ != q_head_) {
            out[n++] = queue_[q_tail_];
            q_tail_ = (q_tail_ + 1) % kEventQueue;
        }
        return n;
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
    void take_ownership(uint32_t xecp) {
        uint32_t off = xecp * 4;
        for (int guard = 0; off != 0 && guard < 64; ++guard) {
            volatile uint32_t& cap = *reinterpret_cast<volatile uint32_t*>(base_ + off);
            const uint32_t v = cap;
            if ((v & 0xFF) == 1) {
                cap = v | (1u << 24);  // OS owned
                wait([&] { return (cap & (1u << 16)) == 0; }, 100000);  // BIOS released
                // Turn off SMIs the firmware may have left enabled.
                *reinterpret_cast<volatile uint32_t*>(base_ + off + 4) = 0;
                return;
            }
            const uint32_t next = (v >> 8) & 0xFF;
            off = next ? off + next * 4 : 0;
        }
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
        const uint8_t speed = uint8_t((sc >> 10) & 0xF);

        Device& d = devices_[count_];
        d.info.port = uint8_t(port);
        d.info.speed = speed;
        if (!address_device(d, port, speed)) {
            ops_->log(Line().s("xhci: port ").u(uint64_t(port)).s(": device did not take an address").str());
            return;
        }
        if (!read_descriptors(d)) {
            ops_->log(Line().s("xhci: port ").u(uint64_t(port)).s(": could not read descriptors").str());
            return;
        }
        ++count_;
        if (d.hid != HidKind::None && !start_hid(d)) d.hid = HidKind::None;

        static const char* const kSpeed[] = {"?", "full speed", "low speed", "high speed", "SuperSpeed", "SuperSpeed+"};
        Line l;
        l.s("xhci: port ").u(uint64_t(port)).s(", slot ").u(d.slot).s(": ").x4(d.info.vendor).s(":").x4(d.info.product)
         .s(" \"").s(d.info.name).s("\", ").s(speed < 6 ? kSpeed[speed] : "?");
        if (d.hid == HidKind::Keyboard) l.s(", HID boot keyboard");
        if (d.hid == HidKind::Mouse) l.s(", HID boot mouse");
        ops_->log(l.str());
    }

    volatile uint32_t* ctx(const dhi_dma& mem, int index) const {
        return reinterpret_cast<volatile uint32_t*>(static_cast<uint8_t*>(mem.virt) + index * ctx_size_);
    }

    bool address_device(Device& d, int port, uint8_t speed) {
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
        sl[0] = (1u << 27) | (uint32_t(speed) << 20);  // one context entry
        sl[1] = uint32_t(port) << 16;                   // root hub port
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
            const uint8_t i_manufacturer = b[14], i_product = b[15];

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
        bool in_boot_hid = false;
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

extern "C" int32_t aero_xhci_device(int32_t ctrl, int32_t index, dhi_usb_device* out) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return -1;
    return g_controllers[ctrl].device_info(index, out);
}
