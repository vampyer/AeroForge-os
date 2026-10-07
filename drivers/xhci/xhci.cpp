// AeroForge xHCI (USB 3) driver (C++20, freestanding), behind the Driver Host
// Interface.
//
// The kernel calls aero_xhci_poll() regularly and the driver drains its
// event ring; once aero_xhci_enable_irq() has run, the controller also raises
// an interrupt (MSI-X or MSI, set up by the kernel) for new events, so the
// kernel can poll right away instead of at its next tick. At init it resets the controller, enumerates every
// device on the root ports and behind hubs, reads its descriptors and
// configures HID boot-protocol keyboards and mice and bulk-only mass storage
// (USB sticks and drives, SCSI commands). UAS comes
// later.

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
constexpr uint32_t kPortConnectChange = 1u << 17;

// Interrupter 0, relative to the runtime base.
constexpr uint32_t kIman   = 0x20;
constexpr uint32_t kImod   = 0x24;
constexpr uint32_t kImanPending = 1u << 0;  // RW1C
constexpr uint32_t kImanEnable  = 1u << 1;
constexpr uint32_t kCmdIrqEnable = 1u << 2;
constexpr uint32_t kStsEventIrq  = 1u << 3;  // RW1C
constexpr uint32_t kErstsz = 0x28;
constexpr uint32_t kErstba = 0x30;
constexpr uint32_t kErdp   = 0x38;

// ---- TRBs (chapter 6.4) ----

struct Trb {
    uint32_t d0, d1, d2, d3;
};
static_assert(sizeof(Trb) == 16);

constexpr uint32_t kTrbNormal = 1, kTrbSetup = 2, kTrbData = 3, kTrbStatus = 4, kTrbIsoch = 5, kTrbLink = 6;
constexpr uint32_t kTrbEnableSlot = 9, kTrbDisableSlot = 10, kTrbAddressDevice = 11, kTrbConfigureEndpoint = 12,
                   kTrbEvaluateContext = 13, kTrbResetEndpoint = 14, kTrbSetTrDequeue = 16;
constexpr uint32_t kEvtTransfer = 32, kEvtCommandDone = 33, kEvtPortStatus = 34;

constexpr uint32_t kTrbCycle = 1u << 0;
constexpr uint32_t kTrbToggle = 1u << 1;   // link TRB: toggle cycle
constexpr uint32_t kTrbIsp = 1u << 2;      // interrupt on short packet
constexpr uint32_t kTrbChain = 1u << 4;    // next TRB belongs to the same transfer
constexpr uint32_t kTrbIoc = 1u << 5;      // interrupt on completion
constexpr uint32_t kTrbIdt = 1u << 6;      // immediate data (setup stage)
constexpr uint32_t kTrbDirIn = 1u << 16;
constexpr uint32_t kTrbSia = 1u << 31;     // isochronous: start as soon as possible

constexpr uint32_t kCcSuccess = 1, kCcStall = 6, kCcShortPacket = 13, kCcRingUnderrun = 14, kCcRingOverrun = 15;

constexpr uint32_t trb_type(uint32_t t) { return t << 10; }

constexpr int kRingTrbs = 256;        // one 4 KiB page per ring
constexpr int kMaxSlots = 16;
constexpr int kMaxDevices = 16;
constexpr int kReportTrbs = 16;       // interrupt-IN transfers kept in flight
constexpr int kReportSize = 16;
constexpr uint32_t kHidMaxErrors = 100;  // halts of one keyboard or mouse endpoint before it is dropped
constexpr int kEventQueue = 128;
constexpr int kBtTrbs = 8;            // Bluetooth event and ACL-in transfers kept in flight
constexpr int kBtEventBuf = 512;
constexpr int kBtAclBuf = 1024;
constexpr int kBtQueue = 32;          // received Bluetooth chunks waiting for the kernel
constexpr int kMaxBt = 2;             // Bluetooth adapters per controller
constexpr int kScoTrbs = 16;          // isochronous voice transfers in flight, each way
constexpr int kScoBuf = 64;           // one isochronous packet (largest SCO alternate setting: 63)
constexpr int kScoQueue = 256;        // received voice packets waiting for the kernel
constexpr int kScoAlts = 8;
constexpr int kPadTrbs = 8;           // gamepad interrupt-IN transfers in flight
constexpr int kPadBuf = 64;           // one gamepad report (HID full-speed interrupt packets are at most 64 bytes)
constexpr int kPadDesc = 1024;        // largest HID report descriptor kept
constexpr int kPadQueue = 64;         // gamepad reports waiting for the kernel
constexpr int kPadCandidates = 4;     // non-boot HID interfaces checked for a gamepad

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

    void free(const dhi_ops* ops) {
        if (mem_.virt != nullptr) ops->dma_free(&mem_);
        mem_ = dhi_dma{};
        trbs_ = nullptr;
        enqueue_ = 0;
        cycle_ = 1;
    }

    uint64_t phys() const { return mem_.phys; }
    // Where the next TRB goes, with the cycle bit it will carry (for Set TR Dequeue Pointer).
    uint64_t dequeue_for_reset() const { return mem_.phys + uint64_t(enqueue_) * sizeof(Trb) | cycle_; }
    int index_of(uint64_t phys) const { return int((phys - mem_.phys) / sizeof(Trb)); }
    // The data buffer of a TRB on this ring (from a transfer event's TRB
    // pointer), or 0 when the pointer is not one of ours.
    uint64_t buffer_of(uint64_t phys) const {
        if (phys < mem_.phys || phys >= mem_.phys + (kRingTrbs - 1) * sizeof(Trb) || phys % sizeof(Trb)) return 0;
        const volatile Trb& t = trbs_[index_of(phys)];
        return t.d0 | uint64_t(t.d1) << 32;
    }

private:
    dhi_dma mem_{};
    volatile Trb* trbs_ = nullptr;
    int enqueue_ = 0;
    uint32_t cycle_ = 1;
};

enum class HidKind : uint8_t { None, Keyboard, Mouse, Tablet };
enum class PadKind : uint8_t { None, Hid, XInput };

// The report the driver turns each Xbox 360 (XInput) input report into, so
// the kernel decodes every USB gamepad with its HID parser: hat (4 bits, 8 =
// centred), 11 buttons (A B X Y LB RB Back Start LS RS Guide), the sticks as
// X Y Rx Ry (down positive, as in HID) and the triggers as Z and Rz.
constexpr uint8_t kXInputReportDesc[] = {
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01,                    // Generic Desktop, Gamepad, Application
    0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42,  // hat, null state
    0x75, 0x04, 0x95, 0x01, 0x81, 0x01,                    // padding
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0B, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0B, 0x81, 0x02,
    0x75, 0x05, 0x95, 0x01, 0x81, 0x01,                    // padding
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x33, 0x09, 0x34,
    0x16, 0x00, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02,
    0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0xC0,
};
constexpr int kXInputReport = 13;

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

struct Device {
    bool alive = false;                    // in use (entries of unplugged devices are reused)
    dhi_usb_device info{};
    Where at{};
    uint8_t slot = 0;
    uint8_t hub_ports = 0;                 // a hub: its port count (0 = not a hub)
    Ring ep0{};
    dhi_dma in_ctx{}, out_ctx{};

    // Boot-protocol HID interface, or an absolute pointer (tablet), if any.
    HidKind hid = HidKind::None;
    uint8_t dci = 0;
    // Where a tablet's report keeps its fields (from its report descriptor).
    struct {
        uint8_t report_id;                 // 0 = reports have no ID byte
        uint16_t x_bit, y_bit, button_bit; // bit offsets after the ID byte
        uint8_t x_size, y_size, buttons;   // field sizes in bits; button count
        uint32_t x_max, y_max;             // logical maximum of X and Y
    } tab{};
    Ring intr{};
    dhi_dma reports{};
    uint8_t last_keys[8] = {};
    uint32_t report_next = 0;  // report buffer the next queued transfer uses
    uint8_t intr_halt = 0;     // completion code that halted the endpoint, until poll() resets it
    uint32_t intr_errors = 0;  // failed interrupt transfers

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

    // Bluetooth HCI transport (USB class E0/01/01): commands on EP0, events
    // on an interrupt-IN endpoint, ACL data on the bulk pair (in_*/out_*, as
    // for storage; a device is one or the other).
    int8_t bt = -1;                        // index into the controller's Bluetooth queues
    bool bt_iface_seen = false;
    uint8_t evt_dci = 0, evt_interval = 0;
    uint16_t evt_mps = 0;
    Ring evt_ring{};
    dhi_dma bt_bufs{};                     // kBtTrbs event buffers, then kBtTrbs ACL buffers
    uint32_t evt_queued = 0, evt_done = 0, acl_queued = 0, acl_done = 0;

    // Voice (SCO) on the second interface: isochronous endpoints whose
    // packet size depends on the alternate setting (0 = no bandwidth).
    int8_t sco_iface = -1;
    uint8_t sco_alt = 0, sco_in_dci = 0, sco_out_dci = 0, sco_interval = 0;
    uint16_t sco_in_mps[kScoAlts] = {}, sco_out_mps[kScoAlts] = {};
    Ring sco_in_ring{}, sco_out_ring{};
    dhi_dma sco_bufs{};                    // kScoTrbs IN packets, then kScoTrbs OUT packets
    uint32_t sco_in_queued = 0, sco_in_done = 0, sco_out_queued = 0, sco_out_done = 0;
    uint32_t sco_in_busy = 0;              // bit per IN buffer with a transfer queued

    // Gamepad: an Xbox 360 style interface (class FF/5D/01, which most 2.4 GHz
    // dongles and wired pads use) or a HID interface whose report descriptor
    // describes a joystick or gamepad.
    PadKind pad = PadKind::None;
    int8_t pad_index = -1;                 // index in the controller's gamepad list
    uint8_t pad_iface = 0, pad_in_dci = 0, pad_out_dci = 0, pad_interval = 0, pad_out_interval = 0;
    uint16_t pad_mps = 0, pad_out_mps = 0, pad_desc_len = 0;
    Ring pad_ring{}, pad_out_ring{};
    dhi_dma pad_bufs{};                    // kPadTrbs reports, an OUT packet, then the report descriptor
};

// HID usage (keyboard page) to ASCII, unshifted and shifted, for 0x04..0x38.
constexpr char kHidNormal[] =
    "abcdefghijklmnopqrstuvwxyz1234567890\n\x1b\b\t -=[]\\#;'`,./";
constexpr char kHidShifted[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()\n\x1b\b\t _+{}|~:\"~<>?";
static_assert(sizeof(kHidNormal) == 0x38 - 0x04 + 2);
constexpr uint8_t kHidCapsLock = 0x39;
// The number pad, 0x54..0x63, read as if Num Lock were on.
constexpr char kHidKeypad[] = "/*-+\n1234567890.";
static_assert(sizeof(kHidKeypad) == 0x63 - 0x54 + 2);

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
        if (!try_lock()) return 0;
        process_events();
        service_ports();
        recover_hid();
        if (++polls_ % 32 == 0) poll_hubs();
        int32_t n = 0;
        while (n < max && q_tail_ != q_head_) {
            out[n++] = queue_[q_tail_];
            q_tail_ = (q_tail_ + 1) % kEventQueue;
        }
        unlock();
        return n;
    }

    // Bluetooth adapter `index` on this controller: its device number, or -1.
    int32_t bt_device(int32_t index, dhi_usb_device* out) const {
        for (int i = 0; i < count_; ++i) {
            if (devices_[i].bt == index) {
                if (out) *out = devices_[i].info;
                return i;
            }
        }
        return -1;
    }

    // HCI command (type 1) on EP0 or ACL data (type 2) on bulk OUT. 0 = ok.
    int32_t bt_send(int32_t dev, uint8_t type, const uint8_t* data, uint32_t len) {
        if (dev < 0 || dev >= count_ || devices_[dev].bt < 0 || len == 0 || len > 2048) return -1;
        Device& d = devices_[dev];
        lock();
        auto* buf = static_cast<uint8_t*>(d.ms_buf.virt);
        for (uint32_t i = 0; i < len; ++i) buf[i] = data[i];
        int32_t rc = -1;
        if (type == 1) {
            rc = control(d, 0x20, 0, 0, 0, uint16_t(len), d.ms_buf.phys) == int(len) ? 0 : -1;
        } else if (type == 2) {
            const uint32_t code = bulk(d, false, d.ms_buf.phys, len);
            if (code == kCcStall) clear_halt(d, false);
            rc = code == kCcSuccess ? 0 : -1;
        } else if (type == DHI_BT_SCO) {
            rc = send_sco(d, data, len);
        }
        unlock();
        return rc;
    }

    // Next received chunk (type 4 event bytes, 2 ACL bytes): length, or 0.
    int32_t bt_recv(int32_t dev, uint8_t* type, uint8_t* out, uint32_t max) {
        if (dev < 0 || dev >= count_ || devices_[dev].bt < 0) return -1;
        if (!try_lock()) return 0;
        process_events();
        BtQueue& q = bt_queues_[devices_[dev].bt];
        ScoQueue& sq = sco_queues_[devices_[dev].bt];
        int32_t n = 0;
        if (q.tail != q.head) {
            const BtChunk& c = q.chunks[q.tail];
            n = int32_t(c.len < max ? c.len : max);
            *type = c.type;
            for (int32_t i = 0; i < n; ++i) out[i] = c.data[i];
            q.tail = (q.tail + 1) % kBtQueue;
        } else if (sq.tail != sq.head) {
            const ScoChunk& c = sq.chunks[sq.tail];
            n = int32_t(c.len < max ? c.len : max);
            *type = DHI_BT_SCO;
            for (int32_t i = 0; i < n; ++i) out[i] = c.data[i];
            sq.tail = (sq.tail + 1) % kScoQueue;
        }
        unlock();
        return n;
    }

    // A raw control transfer on EP0, for vendor set-up (firmware download).
    // Returns the bytes transferred or -1.
    int32_t bt_control(int32_t dev, uint8_t req_type, uint8_t request, uint16_t value, uint16_t index,
                       uint8_t* data, uint16_t len) {
        if (dev < 0 || dev >= count_ || devices_[dev].bt < 0 || len > 2048) return -1;
        Device& d = devices_[dev];
        lock();
        auto* buf = static_cast<uint8_t*>(d.ms_buf.virt);
        const bool in = req_type & 0x80;
        if (!in)
            for (uint16_t i = 0; i < len; ++i) buf[i] = data[i];
        const int n = control(d, req_type, request, value, index, len, d.ms_buf.phys);
        if (n < 0) clear_ep0_halt(d);
        if (in && n > 0)
            for (int i = 0; i < n; ++i) data[i] = buf[i];
        unlock();
        return n;
    }

    // Selects the voice interface's alternate setting (0 stops voice) and,
    // for a nonzero one, starts receiving. 0 = ok.
    int32_t bt_sco(int32_t dev, uint8_t alt) {
        if (dev < 0 || dev >= count_ || devices_[dev].bt < 0) return -1;
        Device& d = devices_[dev];
        if (d.sco_iface < 0 || alt >= kScoAlts || (alt && !d.sco_in_mps[alt])) return -1;
        lock();
        const int32_t rc = select_sco(d, alt) ? 0 : -1;
        unlock();
        return rc;
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
        return transfer(dev, false, lba, count, buf_phys);
    }

    int32_t write(int32_t dev, uint64_t lba, uint32_t count, uint64_t buf_phys) {
        return transfer(dev, true, lba, count, buf_phys);
    }

    // SYNCHRONIZE CACHE(10) over the whole disk.
    int32_t flush(int32_t dev) {
        if (dev < 0 || dev >= count_ || !devices_[dev].disk_ok) return -1;
        Device& d = devices_[dev];
        const uint8_t cdb[10] = {0x35};
        lock();
        int rc = scsi(d, cdb, 10, false, 0, 0);
        if (rc == 1) request_sense(d);  // some sticks don't support it; treat that as done
        unlock();
        return rc < 0 ? -1 : 0;
    }

    // Gamepad `index`: fills `out` and copies its HID report descriptor.
    int32_t pad_info(int32_t index, dhi_usb_device* out, uint8_t* desc, uint32_t max) const {
        for (int i = 0; i < count_; ++i) {
            const Device& d = devices_[i];
            if (d.pad == PadKind::None || d.pad_index != index) continue;
            if (out) *out = d.info;
            const uint32_t n = d.pad_desc_len < max ? d.pad_desc_len : max;
            const auto* src = static_cast<const uint8_t*>(d.pad_bufs.virt) + (kPadTrbs + 1) * kPadBuf;
            for (uint32_t k = 0; k < n; ++k) desc[k] = src[k];
            return int32_t(n);
        }
        return index >= 0 && index < pad_count_ ? -2 : -1;
    }

    int32_t pad_report(int32_t* pad, uint8_t* out, uint32_t max) {
        if (base_ == nullptr || !try_lock()) return 0;
        process_events();
        int32_t n = 0;
        if (pq_tail_ != pq_head_) {
            const PadReport& r = pad_queue_[pq_tail_];
            n = int32_t(r.len < max ? r.len : max);
            *pad = r.pad;
            for (int32_t i = 0; i < n; ++i) out[i] = r.data[i];
            pq_tail_ = (pq_tail_ + 1) % kPadQueue;
        }
        unlock();
        return n;
    }

    // Device entry `index`: 0 = filled in, 1 = empty (unplugged), -1 = past the last entry.
    int32_t device_info(int32_t index, dhi_usb_device* out) const {
        if (index < 0 || index >= count_) return -1;
        if (!devices_[index].alive) return 1;
        *out = devices_[index].info;
        return 0;
    }

    // Bumped whenever a device is plugged in or unplugged.
    uint32_t generation() const { return generation_; }

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
        rt32(kIman) = rt32(kIman) & ~kImanEnable;  // off until aero_xhci_enable_irq
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

    // Addresses, describes and starts the device at `at`; hubs recurse into their ports.
    void attach(const Where& at, uint8_t speed) {
        int index = 0;
        while (index < count_ && devices_[index].alive) ++index;
        if (index >= kMaxDevices) {
            ops_->log("xhci: too many USB devices, one ignored");
            return;
        }
        Device& d = devices_[index];
        d = Device{};
        d.at = at;
        d.info.port = at.port;
        d.info.parent_slot = at.parent_slot;
        d.info.speed = speed;
        Line where;
        if (at.parent_slot) where.s("hub ").u(at.parent_slot).s(" port ").u(at.port);
        else where.s("port ").u(at.port);
        if (!address_device(d, at, speed)) {
            ops_->log(Line().s("xhci: ").s(where.str()).s(": device did not take an address").str());
            release(d);
            return;
        }
        if (!read_descriptors(d)) {
            ops_->log(Line().s("xhci: ").s(where.str()).s(": could not read descriptors").str());
            release(d);
            return;
        }
        d.alive = true;
        if (index == count_) ++count_;
        ++generation_;
        if (d.hid == HidKind::None && candidates_ > 0) find_tablet(d);
        if (d.hid != HidKind::None && !start_hid(d)) d.hid = HidKind::None;
        if (d.storage && !start_storage(d)) d.storage = false;
        if ((d.pad != PadKind::None || candidates_ > 0) && !start_pad(d)) d.pad = PadKind::None;
        if (d.bt_iface_seen && d.evt_dci && d.in_dci && d.out_dci && bt_count_ < kMaxBt && start_bt(d))
            d.bt = int8_t(bt_count_++);

        static const char* const kSpeed[] = {"?", "full speed", "low speed", "high speed", "SuperSpeed", "SuperSpeed+ 10 Gb/s", "SuperSpeed+ 20 Gb/s"};
        Line l;
        l.s("xhci: ").s(where.str()).s(", slot ").u(d.slot).s(": ").x4(d.info.vendor).s(":").x4(d.info.product)
         .s(" \"").s(d.info.name).s("\", ").s(speed < 7 ? kSpeed[speed] : "?");
        if (d.hid == HidKind::Keyboard) l.s(", HID boot keyboard");
        if (d.hid == HidKind::Mouse) l.s(", HID boot mouse");
        if (d.hid == HidKind::Tablet) l.s(", HID tablet (absolute pointer)");
        if (d.disk_ok) {
            l.s(", disk ").s(d.disk.model).s(", ").u(d.disk.block_count * d.disk.block_size / (1024 * 1024)).s(" MiB");
        } else if (d.info.iface_class == 8) {
            l.s(", mass storage (not usable)");
        }
        if (d.bt >= 0) l.s(", Bluetooth adapter");
        if (d.pad == PadKind::XInput) l.s(", gamepad (Xbox 360 style)");
        if (d.pad == PadKind::Hid) l.s(", HID gamepad");
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
        hub.hub_ports = uint8_t(ports);

        for (int p = 1; p <= ports; ++p) {
            uint16_t status = 0, change = 0;
            if (!hub_port_status(hub, p, buf, &status, &change) || !(status & kHubPortConnection)) continue;
            control(hub, 0x23, 1, kFeatureCConnection, uint16_t(p), 0, 0);
            attach_hub_port(hub, p, buf);
        }
        ops_->dma_free(&buf);
    }

    // Resets the device on hub port `p` and attaches it.
    void attach_hub_port(Device& hub, int p, const dhi_dma& buf) {
        const Where& at = hub.at;
        const uint8_t speed = hub.info.speed;
        const bool superspeed = speed >= 4;
        uint16_t status = 0, change = 0;
        control(hub, 0x23, 3, superspeed ? kFeatureBhReset : kFeaturePortReset, uint16_t(p), 0, 0);
        bool enabled = false;
        for (int i = 0; i < 50 && !enabled; ++i) {
            ops_->delay_us(10000);
            if (hub_port_status(hub, p, buf, &status, &change))
                enabled = !(status & kHubPortReset) && (status & kHubPortEnable || superspeed);
        }
        control(hub, 0x23, 1, kFeatureCReset, uint16_t(p), 0, 0);
        if (!enabled) return;
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

    // ------------------------------------------------------------- hot-plug

    // Root ports whose connection changed, from Port Status Change events;
    // handled outside the event handler because attaching waits for commands.
    void service_ports() {
        while (pending_ports_ != 0) {
            const int port = __builtin_ctzll(pending_ports_);
            pending_ports_ &= pending_ports_ - 1;
            if (port < 1 || port > max_ports_) continue;
            const uint32_t sc = portsc(port);
            set_portsc(port, (sc & kPortPower) | (sc & kPortChangeBits));  // clear change bits
            if (!(sc & kPortConnectChange)) continue;  // a reset finishing, not a plug
            // Unplugged, or replaced by another device: whatever was there is gone.
            for (int i = 0; i < count_; ++i)
                if (devices_[i].alive && devices_[i].at.root_port == port && devices_[i].at.depth == 0) detach(i);
            if (sc & kPortConnected) {
                ops_->delay_us(100000);  // debounce (USB 2.0 7.1.7.3)
                probe_port(port);
            }
        }
    }

    // Hubs report plugs on an interrupt endpoint; asking each port's status
    // a few times a second is simpler and cheap.
    void poll_hubs() {
        if (hub_buf_.virt == nullptr && ops_->dma_alloc(64, &hub_buf_) != 0) return;
        for (int i = 0; i < count_; ++i) {
            Device& hub = devices_[i];
            if (!hub.alive || hub.hub_ports == 0) continue;
            for (int p = 1; p <= hub.hub_ports && hub.alive; ++p) {
                uint16_t status = 0, change = 0;
                if (!hub_port_status(hub, p, hub_buf_, &status, &change) || !(change & kHubPortConnection)) continue;
                control(hub, 0x23, 1, kFeatureCConnection, uint16_t(p), 0, 0);
                for (int j = 0; j < count_; ++j) {
                    const Device& c = devices_[j];
                    if (c.alive && c.at.parent_slot == hub.slot && c.at.port == p) detach(j);
                }
                if (status & kHubPortConnection) {
                    ops_->delay_us(100000);  // debounce
                    attach_hub_port(hub, p, hub_buf_);
                }
            }
        }
    }

    // Forgets an unplugged device (and, for a hub, everything behind it).
    void detach(int index) {
        Device& d = devices_[index];
        if (!d.alive) return;
        if (d.hub_ports != 0) {
            for (int j = 0; j < count_; ++j)
                if (devices_[j].alive && devices_[j].at.parent_slot == d.slot && j != index) detach(j);
        }
        Line l;
        l.s("xhci: ");
        if (d.at.parent_slot) l.s("hub ").u(d.at.parent_slot).s(" port ").u(d.at.port);
        else l.s("port ").u(d.at.port);
        l.s(", slot ").u(d.slot).s(": ").x4(d.info.vendor).s(":").x4(d.info.product).s(" \"").s(d.info.name)
         .s("\" unplugged");
        ops_->log(l.str());
        release(d);
        ++generation_;
    }

    // Gives back a device's slot and memory and clears its entry.
    void release(Device& d) {
        if (d.slot != 0) {
            command(0, 0, trb_type(kTrbDisableSlot) | (uint32_t(d.slot) << 24), nullptr);
            static_cast<volatile uint64_t*>(dcbaa_.virt)[d.slot] = 0;
        }
        Ring* rings[] = {&d.ep0, &d.intr, &d.ring_in, &d.ring_out, &d.evt_ring, &d.sco_in_ring, &d.sco_out_ring,
                         &d.pad_ring, &d.pad_out_ring};
        for (Ring* r : rings) r->free(ops_);
        dhi_dma* mems[] = {&d.in_ctx, &d.out_ctx, &d.reports, &d.ms_buf, &d.bt_bufs, &d.sco_bufs, &d.pad_bufs};
        for (dhi_dma* m : mems)
            if (m->virt != nullptr) ops_->dma_free(m);
        d = Device{};
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

    // Finds the first HID boot keyboard or mouse interface and its interrupt-IN
    // endpoint, mass storage, Bluetooth, and gamepad interfaces (XInput, or
    // HID interfaces that start_pad checks for a gamepad report descriptor).
    void parse_config(Device& d, const uint8_t* b, int len) {
        bool in_boot_hid = false, in_storage = false, in_bt = false, in_xinput = false;
        int sco_alt = -1, in_candidate = -1;
        candidates_ = 0;
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
                // Gamepads: Xbox 360 style (control interface 0 only), or any other HID interface.
                in_xinput = d.pad == PadKind::None && b[off + 3] == 0 && b[off + 5] == 0xFF && b[off + 6] == 0x5D &&
                            b[off + 7] == 0x01;
                if (in_xinput) {
                    d.pad_iface = b[off + 2];
                    d.pad_in_dci = d.pad_out_dci = 0;
                }
                in_candidate = -1;
                if (b[off + 3] == 0 && b[off + 5] == 3 && !(b[off + 6] == 1 && (b[off + 7] == 1 || b[off + 7] == 2)) &&
                    candidates_ < kPadCandidates) {
                    in_candidate = candidates_;
                    candidate_[in_candidate] = PadCandidate{};
                    candidate_[in_candidate].iface = b[off + 2];
                }
                // Mass storage, SCSI transparent command set, bulk-only transport.
                in_storage = !d.storage && b[off + 5] == 8 && b[off + 6] == 6 && b[off + 7] == 0x50;
                if (in_storage) {
                    d.ms_iface = b[off + 2];
                    d.in_addr = d.out_addr = 0;
                }
                // Bluetooth primary controller: only interface 0, alternate 0 (SCO audio lives on 1).
                in_bt = !d.bt_iface_seen && b[off + 5] == 0xE0 && b[off + 6] == 1 && b[off + 7] == 1 && b[off + 3] == 0;
                if (in_bt) d.bt_iface_seen = true;
                // Its voice interface: the next E0/01/01 interface, each alternate setting.
                sco_alt = -1;
                if (d.bt_iface_seen && !in_bt && b[off + 5] == 0xE0 && b[off + 6] == 1 && b[off + 7] == 1 &&
                    b[off + 3] < kScoAlts && (d.sco_iface < 0 || d.sco_iface == b[off + 2])) {
                    d.sco_iface = int8_t(b[off + 2]);
                    sco_alt = b[off + 3];
                }
            } else if (type == 0x21 && off + 9 <= len && in_candidate >= 0 && b[off + 6] == 0x22) {  // HID descriptor
                candidate_[in_candidate].desc_len = uint16_t(b[off + 7] | b[off + 8] << 8);
            } else if (type == 5 && off + 7 <= len && in_candidate >= 0) {  // HID endpoint
                const uint8_t addr = b[off + 2], attr = b[off + 3];
                PadCandidate& c = candidate_[in_candidate];
                if ((addr & 0x80) && (attr & 3) == 3 && c.dci == 0) {
                    c.dci = uint8_t((addr & 0xF) * 2 + 1);
                    c.mps = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                    c.interval = b[off + 6];
                    if (c.desc_len) ++candidates_;
                    in_candidate = -1;
                }
            } else if (type == 5 && off + 7 <= len && in_xinput && (b[off + 3] & 3) == 3) {  // XInput endpoints
                const uint8_t addr = b[off + 2];
                const uint16_t mps = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                if (addr & 0x80) {
                    d.pad_in_dci = uint8_t((addr & 0xF) * 2 + 1);
                    d.pad_mps = mps;
                    d.pad_interval = b[off + 6];
                } else {
                    d.pad_out_dci = uint8_t((addr & 0xF) * 2);
                    d.pad_out_mps = mps;
                    d.pad_out_interval = b[off + 6];
                }
                if (d.pad_in_dci) d.pad = PadKind::XInput;  // the OUT endpoint (player LEDs) is optional
            } else if (type == 5 && off + 7 <= len && sco_alt >= 0 && (b[off + 3] & 3) == 1) {  // isochronous
                const uint8_t addr = b[off + 2];
                const uint16_t mps = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                if (addr & 0x80) {
                    d.sco_in_dci = uint8_t((addr & 0xF) * 2 + 1);
                    d.sco_in_mps[sco_alt] = mps;
                } else {
                    d.sco_out_dci = uint8_t((addr & 0xF) * 2);
                    d.sco_out_mps[sco_alt] = mps;
                }
                d.sco_interval = b[off + 6];
            } else if (type == 5 && off + 7 <= len && in_bt) {  // Bluetooth endpoints
                const uint8_t addr = b[off + 2], attr = b[off + 3];
                const uint16_t mps = uint16_t((b[off + 4] | b[off + 5] << 8) & 0x7FF);
                if ((attr & 3) == 3 && (addr & 0x80)) {
                    d.evt_dci = uint8_t((addr & 0xF) * 2 + 1);
                    d.evt_mps = mps;
                    d.evt_interval = b[off + 6];
                } else if ((attr & 3) == 2 && (addr & 0x80)) {
                    d.in_addr = addr;
                    d.in_dci = uint8_t((addr & 0xF) * 2 + 1);
                    d.in_mps = mps;
                } else if ((attr & 3) == 2) {
                    d.out_addr = addr;
                    d.out_dci = uint8_t((addr & 0xF) * 2);
                    d.out_mps = mps;
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
        if (d.hid != HidKind::Tablet) control(d, 0x21, 0x0B, 0, iface, 0, 0);  // SET_PROTOCOL(boot)
        control(d, 0x21, 0x0A, 0, iface, 0, 0);  // SET_IDLE(0): report only on change

        if (!d.intr.init(ops_) || ops_->dma_alloc(kReportTrbs * kReportSize, &d.reports) != 0) return false;
        const IntrEp ep{d.dci, hid_mps_, hid_interval_, &d.intr};
        const uint32_t code = add_interrupt_endpoints(d, &ep, 1);
        if (code != kCcSuccess) {
            ops_->log(Line().s("xhci: slot ").u(d.slot).s(d.hid == HidKind::Keyboard ? " keyboard" : " pointer")
                          .s(": controller refused its endpoint (code ").u(code).s(")").str());
            return false;
        }

        report_len_[d.slot] = uint8_t(hid_mps_ < kReportSize ? hid_mps_ : kReportSize);
        for (int i = 0; i < kReportTrbs; ++i) queue_report(d);
        return true;
    }

    // Interval as 2^n * 125 us: full/low speed give frames (1 ms), high speed and up an exponent.
    static uint32_t interval_exponent(const Device& d, uint8_t b_interval) {
        if (d.info.speed >= 3) return b_interval ? b_interval - 1u : 0u;
        const uint32_t micro = uint32_t(b_interval ? b_interval : 1) * 8;
        uint32_t interval = 0;
        while ((2u << interval) <= micro) ++interval;
        return interval;
    }

    struct IntrEp {
        uint8_t dci;           // odd = IN, even = OUT
        uint16_t mps;
        uint8_t interval;      // bInterval from the endpoint descriptor
        const Ring* ring;
    };

    // Adds interrupt endpoints to the device's running configuration. Returns
    // the Configure Endpoint completion code (kCcSuccess = done).
    uint32_t add_interrupt_endpoints(Device& d, const IntrEp* eps, int n) {
        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
        volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        uint32_t entries = (out_slot[0] >> 27) & 0x1F;
        control_ctx[1] = 1u;  // slot
        for (int i = 0; i < n; ++i) {
            const IntrEp& e = eps[i];
            control_ctx[1] |= 1u << e.dci;
            if (e.dci > entries) entries = e.dci;
            volatile uint32_t* ep = ctx(d.in_ctx, 1 + e.dci);
            ep[0] = interval_exponent(d, e.interval) << 16;
            ep[1] = (uint32_t(e.mps) << 16) | ((e.dci & 1 ? 7u : 3u) << 3) | (3u << 1);  // interrupt IN/OUT, 3 retries
            ep[2] = uint32_t(e.ring->phys()) | 1;
            ep[3] = uint32_t(e.ring->phys() >> 32);
            ep[4] = (uint32_t(e.mps) << 16) | e.mps;  // max ESIT payload, average TRB length
        }
        sl[0] = (sl[0] & ~(0x1Fu << 27)) | (entries << 27);
        sl[3] = 0;
        return command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr);
    }

    // ------------------------------------------------------------- gamepads

    // True if a HID report descriptor has a Joystick or Gamepad application collection.
    static bool describes_gamepad(const uint8_t* desc, int len) {
        uint32_t page = 0, usage = 0;
        for (int i = 0; i < len;) {
            const uint8_t b = desc[i];
            if (b == 0xFE) {  // long item
                i += 3 + (i + 1 < len ? desc[i + 1] : 0);
                continue;
            }
            const int size = (b & 3) == 3 ? 4 : (b & 3);
            if (i + 1 + size > len) break;
            uint32_t v = 0;
            for (int k = size - 1; k >= 0; --k) v = v << 8 | desc[i + 1 + k];
            if ((b & 0xFC) == 0x04) page = v;                                   // usage page
            else if ((b & 0xFC) == 0x08) usage = size == 4 ? v : page << 16 | v;  // usage
            else if ((b & 0xFC) == 0xA0) {                                       // collection
                if (v == 1 && (usage == 0x10004 || usage == 0x10005)) return true;
                usage = 0;
            } else if ((b & 0x0C) == 0) usage = 0;                               // other main items
            i += 1 + size;
        }
        return false;
    }

    // Reads a HID report descriptor for an absolute pointer: absolute X and Y
    // (Generic Desktop) and buttons (Button page) in one input report, the
    // way tablets and touchscreens (and QEMU's usb-tablet) describe
    // themselves. Fills `d.tab` and returns true if it found one.
    static bool parse_tablet(Device& d, const uint8_t* desc, int len) {
        uint32_t page = 0, size = 0, count = 0, max = 0, id = 0;
        uint32_t usages[16], n_usages = 0, umin = 0, umax = 0;
        uint32_t offset = 0;  // bits into the current report
        bool have_x = false, have_y = false;
        auto& t = d.tab;
        t = {};
        for (int i = 0; i < len;) {
            const uint8_t b = desc[i];
            if (b == 0xFE) {  // long item
                i += 3 + (i + 1 < len ? desc[i + 1] : 0);
                continue;
            }
            const int bytes = (b & 3) == 3 ? 4 : (b & 3);
            if (i + 1 + bytes > len) break;
            uint32_t v = 0;
            for (int k = bytes - 1; k >= 0; --k) v = v << 8 | desc[i + 1 + k];
            switch (b & 0xFC) {
                case 0x04: page = v; break;                 // usage page
                case 0x24: max = v; break;                  // logical maximum
                case 0x74: size = v; break;                 // report size
                case 0x94: count = v; break;                // report count
                case 0x84:                                   // report ID
                    if (have_x && have_y) return true;      // the pointer's report is complete
                    id = v;
                    offset = 0;
                    have_x = have_y = false;
                    t = {};
                    break;
                case 0x08:                                   // usage
                    if (n_usages < 16) usages[n_usages++] = bytes == 4 ? v : page << 16 | v;
                    break;
                case 0x18: umin = page << 16 | v; break;     // usage minimum
                case 0x28: umax = page << 16 | v; break;     // usage maximum
                case 0x80: {                                 // input
                    const bool absolute = (v & 4) == 0, constant = (v & 1) != 0;
                    for (uint32_t k = 0; k < count && !constant; ++k) {
                        uint32_t u = 0;
                        if (k < n_usages) u = usages[k];
                        else if (n_usages) u = usages[n_usages - 1];
                        else if (umin && umin + k <= umax) u = umin + k;
                        const uint32_t at = offset + k * size;
                        if (u == 0x10030 && absolute && max && size <= 32) {
                            t.x_bit = uint16_t(at), t.x_size = uint8_t(size), t.x_max = max, have_x = true;
                        } else if (u == 0x10031 && absolute && max && size <= 32) {
                            t.y_bit = uint16_t(at), t.y_size = uint8_t(size), t.y_max = max, have_y = true;
                        } else if ((u >> 16) == 9 && size == 1 && t.buttons == 0) {
                            t.button_bit = uint16_t(at);
                            t.buttons = uint8_t(count - k < 8 ? count - k : 8);
                        }
                    }
                    offset += size * count;
                    n_usages = umin = umax = 0;
                    break;
                }
                case 0x90: case 0xB0:                        // output, feature
                    n_usages = umin = umax = 0;
                    break;
                default:
                    break;
            }
            i += 1 + bytes;
        }
        if (!(have_x && have_y)) return false;
        t.report_id = uint8_t(id);
        return true;
    }

    // Looks among the device's other HID interfaces for an absolute pointer
    // (not a gamepad) and makes it the device's HID interface if there is one.
    void find_tablet(Device& d) {
        dhi_dma buf{};
        if (ops_->dma_alloc(kPadDesc, &buf) != 0) return;
        auto* desc = static_cast<const uint8_t*>(buf.virt);
        for (int i = 0; i < candidates_ && d.hid == HidKind::None; ++i) {
            const PadCandidate& c = candidate_[i];
            const uint16_t want = c.desc_len < kPadDesc ? c.desc_len : uint16_t(kPadDesc);
            const int n = control(d, 0x81, 6, 0x2200, c.iface, want, buf.phys);  // GET_DESCRIPTOR(report)
            if (n <= 0 || describes_gamepad(desc, n) || !parse_tablet(d, desc, n)) continue;
            // The report must fit the transfer buffer.
            const uint32_t bits_needed = (d.tab.report_id ? 8u : 0u) + d.tab.y_bit + d.tab.y_size;
            if (bits_needed > uint32_t(kReportSize) * 8 || d.tab.x_bit + d.tab.x_size > uint32_t(kReportSize - 1) * 8) continue;
            d.hid = HidKind::Tablet;
            d.dci = c.dci;
            hid_mps_ = c.mps;
            hid_interval_ = c.interval;
            hid_iface_of_device_ = c.iface;
        }
        ops_->dma_free(&buf);
    }

    // Starts the device's gamepad interface: the XInput one parse_config
    // found, or the first HID interface whose report descriptor is a gamepad's.
    bool start_pad(Device& d) {
        if (ops_->dma_alloc((kPadTrbs + 1) * kPadBuf + kPadDesc, &d.pad_bufs) != 0) return false;
        auto* desc = static_cast<uint8_t*>(d.pad_bufs.virt) + (kPadTrbs + 1) * kPadBuf;
        const uint64_t desc_phys = d.pad_bufs.phys + (kPadTrbs + 1) * kPadBuf;
        if (d.pad == PadKind::XInput) {
            for (uint32_t i = 0; i < sizeof(kXInputReportDesc); ++i) desc[i] = kXInputReportDesc[i];
            d.pad_desc_len = sizeof(kXInputReportDesc);
        } else {
            for (int i = 0; i < candidates_ && d.pad == PadKind::None; ++i) {
                const PadCandidate& c = candidate_[i];
                const uint16_t want = c.desc_len < kPadDesc ? c.desc_len : uint16_t(kPadDesc);
                const int n = control(d, 0x81, 6, 0x2200, c.iface, want, desc_phys);  // GET_DESCRIPTOR(report)
                if (n <= 0 || !describes_gamepad(desc, n)) continue;
                d.pad = PadKind::Hid;
                d.pad_iface = c.iface;
                d.pad_in_dci = c.dci;
                d.pad_mps = c.mps;
                d.pad_interval = c.interval;
                d.pad_desc_len = uint16_t(n);
            }
            if (d.pad == PadKind::None) {
                ops_->dma_free(&d.pad_bufs);
                return false;
            }
            control(d, 0x21, 0x0A, 0, d.pad_iface, 0, 0);  // SET_IDLE(0): report only on change
        }
        if (!d.pad_ring.init(ops_) || (d.pad_out_dci && !d.pad_out_ring.init(ops_))) return false;
        const IntrEp eps[2] = {{d.pad_in_dci, d.pad_mps, d.pad_interval, &d.pad_ring},
                               {d.pad_out_dci, d.pad_out_mps, d.pad_out_interval, &d.pad_out_ring}};
        if (add_interrupt_endpoints(d, eps, d.pad_out_dci ? 2 : 1) != kCcSuccess) return false;
        for (uint32_t i = 0; i < kPadTrbs; ++i) queue_pad(d, i);
        if (d.pad == PadKind::XInput && d.pad_out_dci) {
            // Light the player 1 quarter of the ring, as Windows does; some
            // pads keep blinking until they get an LED command.
            auto* out = static_cast<uint8_t*>(d.pad_bufs.virt) + kPadTrbs * kPadBuf;
            out[0] = 0x01;
            out[1] = 0x03;
            out[2] = 0x06;
            const uint64_t phys = d.pad_bufs.phys + kPadTrbs * kPadBuf;
            d.pad_out_ring.push(uint32_t(phys), uint32_t(phys >> 32), 3, trb_type(kTrbNormal) | kTrbIoc);
            ring_doorbell(d.slot, d.pad_out_dci);
        }
        d.pad_index = int8_t(pad_count_++);
        return true;
    }

    void queue_pad(Device& d, uint32_t slot) {
        const uint64_t phys = d.pad_bufs.phys + uint64_t(slot) * kPadBuf;
        d.pad_ring.push(uint32_t(phys), uint32_t(phys >> 32), d.pad_mps < kPadBuf ? d.pad_mps : kPadBuf,
                        trb_type(kTrbNormal) | kTrbIoc | kTrbIsp);
        ring_doorbell(d.slot, d.pad_in_dci);
    }

    void pad_received(Device& d, uint32_t slot, uint32_t code, uint32_t left) {
        if (code != kCcSuccess && code != kCcShortPacket) return;
        const uint32_t size = d.pad_mps < kPadBuf ? d.pad_mps : kPadBuf;
        uint32_t len = left < size ? size - left : 0;
        const auto* src = static_cast<const uint8_t*>(d.pad_bufs.virt) + slot * kPadBuf;
        uint8_t r[kPadBuf];
        if (d.pad == PadKind::XInput) {
            // Input report: 00 14, buttons (2 bytes), LT, RT, LX LY RX RY (signed 16-bit, up positive).
            // Other message types (LED and rumble status) are skipped.
            if (len < 14 || src[0] != 0x00 || src[1] < 14) return;
            static constexpr uint8_t kHat[16] = {8, 0, 4, 8, 6, 7, 5, 6, 2, 1, 3, 2, 8, 0, 4, 8};
            const uint8_t b0 = src[2], b1 = src[3];
            const uint16_t buttons = uint16_t(
                (b1 >> 4 & 0xF) |                  // A B X Y
                (b1 & 3) << 4 |                    // LB RB
                (b0 >> 5 & 1) << 6 |               // Back
                (b0 >> 4 & 1) << 7 |               // Start
                (b0 >> 6 & 3) << 8 |               // left and right stick clicks
                (b1 >> 2 & 1) << 10);              // Guide
            r[0] = kHat[b0 & 0xF];
            r[1] = uint8_t(buttons);
            r[2] = uint8_t(buttons >> 8);
            for (int a = 0; a < 4; ++a) {
                int32_t v = int16_t(src[6 + 2 * a] | src[7 + 2 * a] << 8);
                if (a & 1) v = v == -32768 ? 32767 : -v;  // Y axes: HID has down positive
                r[3 + 2 * a] = uint8_t(v);
                r[4 + 2 * a] = uint8_t(v >> 8);
            }
            r[11] = src[4];
            r[12] = src[5];
            src = r;
            len = kXInputReport;
        }
        if (len == 0) return;
        const int next = (pq_head_ + 1) % kPadQueue;
        if (next == pq_tail_) return;  // full: drop
        PadReport& q = pad_queue_[pq_head_];
        q.pad = uint8_t(d.pad_index);
        q.len = uint8_t(len);
        for (uint32_t i = 0; i < len; ++i) q.data[i] = src[i];
        pq_head_ = next;
    }

    // ------------------------------------------------------------ Bluetooth

    bool start_bt(Device& d) {
        if (!d.evt_ring.init(ops_) || !d.ring_in.init(ops_) || !d.ring_out.init(ops_)) return false;
        if (ops_->dma_alloc(kBtTrbs * (kBtEventBuf + kBtAclBuf), &d.bt_bufs) != 0) return false;
        if (ops_->dma_alloc(4096, &d.ms_buf) != 0) return false;  // outgoing commands and ACL

        uint32_t interval = 0;  // as for HID: 2^n * 125 us
        if (d.info.speed >= 3) {
            interval = d.evt_interval ? d.evt_interval - 1u : 0u;
        } else {
            const uint32_t micro = uint32_t(d.evt_interval ? d.evt_interval : 1) * 8;
            while ((2u << interval) <= micro) ++interval;
        }

        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
        volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
        control_ctx[1] = 1u | (1u << d.evt_dci) | (1u << d.in_dci) | (1u << d.out_dci);
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        uint32_t last = d.evt_dci;
        if (d.in_dci > last) last = d.in_dci;
        if (d.out_dci > last) last = d.out_dci;
        sl[0] = (sl[0] & ~(0x1Fu << 27)) | (last << 27);
        sl[3] = 0;
        const auto setup = [&](uint8_t dci, uint16_t mps, uint32_t ep_type, const Ring& ring, uint32_t ep0,
                               uint32_t avg) {
            volatile uint32_t* ep = ctx(d.in_ctx, 1 + dci);
            ep[0] = ep0;
            ep[1] = (uint32_t(mps) << 16) | (ep_type << 3) | (3u << 1);
            ep[2] = uint32_t(ring.phys()) | 1;
            ep[3] = uint32_t(ring.phys() >> 32);
            ep[4] = avg;
        };
        setup(d.evt_dci, d.evt_mps, 7, d.evt_ring, interval << 16, (uint32_t(d.evt_mps) << 16) | d.evt_mps);
        setup(d.in_dci, d.in_mps, 6, d.ring_in, 0, 3072);
        setup(d.out_dci, d.out_mps, 2, d.ring_out, 0, 3072);
        if (command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr) != kCcSuccess)
            return false;

        for (int i = 0; i < kBtTrbs; ++i) {
            queue_bt_event(d);
            queue_bt_acl(d);
        }
        return true;
    }

    // Transfers complete in the order they were queued, so the n-th
    // completion on a ring always belongs to buffer n % kBtTrbs.
    void queue_bt_event(Device& d) {
        const uint64_t phys = d.bt_bufs.phys + uint64_t(d.evt_queued++ % kBtTrbs) * kBtEventBuf;
        d.evt_ring.push(uint32_t(phys), uint32_t(phys >> 32), kBtEventBuf, trb_type(kTrbNormal) | kTrbIoc | kTrbIsp);
        ring_doorbell(d.slot, d.evt_dci);
    }

    void queue_bt_acl(Device& d) {
        const uint64_t phys = d.bt_bufs.phys + uint64_t(kBtTrbs) * kBtEventBuf +
                              uint64_t(d.acl_queued++ % kBtTrbs) * kBtAclBuf;
        d.ring_in.push(uint32_t(phys), uint32_t(phys >> 32), kBtAclBuf, trb_type(kTrbNormal) | kTrbIoc | kTrbIsp);
        ring_doorbell(d.slot, d.in_dci);
    }

    // ------------------------------------------------------- Bluetooth voice

    // Drops the old isochronous endpoints, switches the alternate setting and
    // adds the new ones (xHCI 4.6.6: Configure Endpoint with drop and add flags).
    bool select_sco(Device& d, uint8_t alt) {
        if (alt == d.sco_alt) return true;
        if (!d.sco_bufs.virt) {
            if (!d.sco_in_ring.init(ops_) || (d.sco_out_dci && !d.sco_out_ring.init(ops_))) return false;
            if (ops_->dma_alloc(2 * kScoTrbs * kScoBuf, &d.sco_bufs) != 0) return false;
        }
        const uint32_t bits = (1u << d.sco_in_dci) | (d.sco_out_dci ? 1u << d.sco_out_dci : 0);
        if (d.sco_alt) {
            for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
            volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
            control_ctx[0] = bits;
            control_ctx[1] = 1;
            volatile uint32_t* sl = ctx(d.in_ctx, 1);
            const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
            for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
            sl[3] = 0;
            command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr);
            d.sco_alt = 0;
        }
        if (control(d, 0x01, 11, alt, uint16_t(d.sco_iface), 0, 0) < 0) {  // SET_INTERFACE
            clear_ep0_halt(d);
            return false;
        }
        if (alt == 0) return true;

        // Interval as 2^n * 125 us: high speed counts microframes, full speed frames.
        const uint32_t b_interval = d.sco_interval ? d.sco_interval : 1;
        const uint32_t interval = d.info.speed >= 3 ? b_interval - 1 : b_interval - 1 + 3;
        for (int i = 0; i < 64 * 33 / 4; ++i) static_cast<volatile uint32_t*>(d.in_ctx.virt)[i] = 0;
        volatile uint32_t* control_ctx = ctx(d.in_ctx, 0);
        control_ctx[1] = 1u | bits;
        volatile uint32_t* sl = ctx(d.in_ctx, 1);
        const volatile uint32_t* out_slot = ctx(d.out_ctx, 0);
        for (int i = 0; i < 4; ++i) sl[i] = out_slot[i];
        const uint32_t last_now = (sl[0] >> 27) & 0x1F;
        uint32_t last = d.sco_in_dci > d.sco_out_dci ? d.sco_in_dci : d.sco_out_dci;
        if (last_now > last) last = last_now;
        sl[0] = (sl[0] & ~(0x1Fu << 27)) | (last << 27);
        sl[3] = 0;
        const auto setup = [&](uint8_t dci, uint16_t mps, uint32_t ep_type, const Ring& ring) {
            volatile uint32_t* ep = ctx(d.in_ctx, 1 + dci);
            ep[0] = interval << 16;
            ep[1] = (uint32_t(mps) << 16) | (ep_type << 3);  // isochronous: no error retries
            const uint64_t deq = ring.dequeue_for_reset();
            ep[2] = uint32_t(deq);
            ep[3] = uint32_t(deq >> 32);
            ep[4] = (uint32_t(mps) << 16) | mps;  // max ESIT payload, average TRB length
        };
        setup(d.sco_in_dci, d.sco_in_mps[alt], 5, d.sco_in_ring);
        if (d.sco_out_dci) setup(d.sco_out_dci, d.sco_out_mps[alt], 1, d.sco_out_ring);
        if (command(d.in_ctx.phys, 0, trb_type(kTrbConfigureEndpoint) | (uint32_t(d.slot) << 24), nullptr) != kCcSuccess)
            return false;
        d.sco_alt = alt;
        d.sco_in_queued = d.sco_in_done = 0;
        d.sco_in_busy = 0;
        d.sco_out_queued = d.sco_out_done = 0;
        for (int i = 0; i < kScoTrbs; ++i) queue_sco_in(d, uint32_t(i));
        return true;
    }

    // Queues buffer `slot` for one isochronous IN packet.
    void queue_sco_in(Device& d, uint32_t slot) {
        if (!d.sco_alt) return;
        ++d.sco_in_queued;
        d.sco_in_busy |= 1u << slot;
        const uint64_t phys = d.sco_bufs.phys + uint64_t(slot) * kScoBuf;
        d.sco_in_ring.push(uint32_t(phys), uint32_t(phys >> 32), d.sco_in_mps[d.sco_alt],
                           trb_type(kTrbIsoch) | kTrbIoc | kTrbIsp | kTrbSia);
        ring_doorbell(d.slot, d.sco_in_dci);
    }

    void sco_received(Device& d, uint32_t slot, uint32_t code, uint32_t left) {
        if (code != kCcSuccess && code != kCcShortPacket) return;
        const uint32_t size = d.sco_in_mps[d.sco_alt];
        const uint32_t len = left < size ? size - left : 0;
        if (len == 0) return;
        ScoQueue& q = sco_queues_[d.bt];
        const int next = (q.head + 1) % kScoQueue;
        if (next == q.tail) {
            ++q.dropped;
            return;
        }
        const auto* src = static_cast<const uint8_t*>(d.sco_bufs.virt) + slot * kScoBuf;
        ScoChunk& c = q.chunks[q.head];
        c.len = uint8_t(len);
        for (uint32_t i = 0; i < len; ++i) c.data[i] = src[i];
        q.head = next;
    }

    // One SCO packet (header included), split into isochronous packets.
    int32_t send_sco(Device& d, const uint8_t* data, uint32_t len) {
        if (!d.sco_alt || !d.sco_out_dci) return -1;
        const uint32_t mps = d.sco_out_mps[d.sco_alt];
        if (mps == 0) return -1;
        process_events();
        const uint32_t packets = (len + mps - 1) / mps;
        if (d.sco_out_queued - d.sco_out_done + packets > kScoTrbs) return -1;  // full: drop it
        for (uint32_t off = 0; off < len; off += mps) {
            const uint32_t n = len - off < mps ? len - off : mps;
            const uint32_t slot = kScoTrbs + d.sco_out_queued++ % kScoTrbs;
            auto* buf = static_cast<uint8_t*>(d.sco_bufs.virt) + slot * kScoBuf;
            for (uint32_t i = 0; i < n; ++i) buf[i] = data[off + i];
            const uint64_t phys = d.sco_bufs.phys + uint64_t(slot) * kScoBuf;
            d.sco_out_ring.push(uint32_t(phys), uint32_t(phys >> 32), n, trb_type(kTrbIsoch) | kTrbIoc | kTrbSia);
        }
        ring_doorbell(d.slot, d.sco_out_dci);
        return 0;
    }

    // Copies a finished event or ACL transfer into the adapter's queue.
    // `slot` counts event buffers first, then ACL buffers.
    void bt_received(Device& d, uint8_t type, uint32_t slot, uint32_t size, uint32_t code, uint32_t left) {
        if (code != kCcSuccess && code != kCcShortPacket) return;
        const uint32_t len = left < size ? size - left : 0;
        if (len == 0) return;
        BtQueue& q = bt_queues_[d.bt];
        const int next = (q.head + 1) % kBtQueue;
        if (next == q.tail) {
            ++q.dropped;
            return;
        }
        const uint32_t offset = slot < kBtTrbs ? slot * kBtEventBuf : kBtTrbs * kBtEventBuf + (slot - kBtTrbs) * kBtAclBuf;
        const auto* src = static_cast<const uint8_t*>(d.bt_bufs.virt) + offset;
        BtChunk& c = q.chunks[q.head];
        c.type = type;
        c.len = uint16_t(len);
        for (uint32_t i = 0; i < len; ++i) c.data[i] = src[i];
        q.head = next;
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

    int32_t transfer(int32_t dev, bool write, uint64_t lba, uint32_t count, uint64_t buf_phys) {
        if (dev < 0 || dev >= count_ || !devices_[dev].disk_ok) return -1;
        Device& d = devices_[dev];
        if (count == 0 || lba + count > d.disk.block_count || uint64_t(count) * d.disk.block_size > d.disk.max_transfer)
            return -1;
        lock();
        int rc = -1;
        for (int attempt = 0; attempt < 2 && rc != 0; ++attempt) {
            rc = rw_blocks(d, write, lba, count, buf_phys);
            if (rc == 1) request_sense(d);  // clears the error so the retry can work
        }
        unlock();
        return rc == 0 ? 0 : -1;
    }

    // READ/WRITE(10), or (16) past 2^32 blocks. 0 = ok, 1 = check condition, -1 = transport error.
    int rw_blocks(Device& d, bool write, uint64_t lba, uint32_t count, uint64_t buf_phys) {
        uint8_t cdb[16] = {};
        int len;
        if (lba + count > 0xFFFFFFFFull) {
            cdb[0] = write ? 0x8A : 0x88;
            for (int i = 0; i < 8; ++i) cdb[2 + i] = uint8_t(lba >> (56 - 8 * i));
            for (int i = 0; i < 4; ++i) cdb[10 + i] = uint8_t(count >> (24 - 8 * i));
            len = 16;
        } else {
            cdb[0] = write ? 0x2A : 0x28;
            for (int i = 0; i < 4; ++i) cdb[2 + i] = uint8_t(lba >> (24 - 8 * i));
            cdb[7] = uint8_t(count >> 8);
            cdb[8] = uint8_t(count);
            len = 10;
        }
        return scsi(d, cdb, len, !write, buf_phys, count * d.disk.block_size);
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

    // A STALL on EP0 (a vendor request the device refuses) halts it in the
    // controller; reset it and skip past the abandoned TRBs. The device
    // side clears itself at the next SETUP.
    void clear_ep0_halt(Device& d) {
        command(0, 0, trb_type(kTrbResetEndpoint) | (1u << 16) | (uint32_t(d.slot) << 24), nullptr);
        command(d.ep0.dequeue_for_reset(), 0, trb_type(kTrbSetTrDequeue) | (1u << 16) | (uint32_t(d.slot) << 24), nullptr);
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

    // Restarts keyboard and mouse endpoints that an error halted (xHCI 4.6.8):
    // reset the endpoint, skip the abandoned transfers, clear a STALL on the
    // device too, and queue the reports again. Gives up after many errors.
    void recover_hid() {
        for (int i = 0; i < count_; ++i) {
            Device& d = devices_[i];
            if (!d.alive || d.hid == HidKind::None || !d.intr_halt) continue;
            const uint32_t code = d.intr_halt;
            ++d.intr_errors;
            if (d.intr_errors <= 3 || d.intr_errors == kHidMaxErrors)
                ops_->log(Line().s("xhci: slot ").u(d.slot).s(d.hid == HidKind::Keyboard ? " keyboard" : " mouse")
                              .s(": transfer error (code ").u(code).s(")")
                              .s(d.intr_errors >= kHidMaxErrors ? ", giving up" : ", endpoint reset").str());
            if (d.intr_errors >= kHidMaxErrors) {
                d.hid = HidKind::None;
                continue;
            }
            command(0, 0, trb_type(kTrbResetEndpoint) | (uint32_t(d.dci) << 16) | (uint32_t(d.slot) << 24), nullptr);
            command(d.intr.dequeue_for_reset(), 0, trb_type(kTrbSetTrDequeue) | (uint32_t(d.dci) << 16) | (uint32_t(d.slot) << 24), nullptr);
            if (code == kCcStall) control(d, 0x02, 1, 0, uint16_t(0x80 | (d.dci - 1) / 2), 0, 0);  // CLEAR_FEATURE(ENDPOINT_HALT)
            d.intr_halt = 0;
            for (int k = 0; k < kReportTrbs; ++k) queue_report(d);
        }
    }

    // Queues one interrupt-IN transfer into the next report buffer. Transfers
    // finish in order and each is queued again only after its report is read,
    // so a buffer is never in use twice. (The ring's 255 TRBs are not a
    // multiple of kReportTrbs, so the ring index can't pick the buffer.)
    void queue_report(Device& d) {
        const int slot = int(d.report_next++ % kReportTrbs);
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
        if (irq_) {
            // Acknowledge before draining, so an event that lands meanwhile
            // raises a new interrupt.
            rt32(kIman) = kImanPending | kImanEnable;
            op32(kUsbSts) = kStsEventIrq;
        }
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
        // Writing ERDP with EHB (bit 3) also re-arms the interrupter.
        if (any || irq_) rt64(kErdp, (events_.phys + uint64_t(ev_index_) * sizeof(Trb)) | (1u << 3));
    }

public:
    // Interrupts from interrupter 0, at most one every 40 us (IMOD counts 250 ns).
    void enable_irq() {
        if (base_ == nullptr) return;
        rt32(kImod) = 160;
        rt32(kIman) = kImanPending | kImanEnable;
        op32(kUsbCmd) = op32(kUsbCmd) | kCmdIrqEnable;
        irq_ = true;
    }

private:

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
        if (type == kEvtPortStatus) {
            const uint32_t port = uint32_t(ptr >> 24) & 0xFF;
            if (port < 64) pending_ports_ |= 1ull << port;
            return;
        }
        if (type != kEvtTransfer) return;
        Device* d = by_slot(slot);
        if (d == nullptr) return;
        const uint8_t ep = uint8_t((d3 >> 16) & 0x1F);
        if (ep == 1) {
            d->ctrl_code = code;
            d->ctrl_left = d2 & 0xFFFFFF;
            d->ctrl_done = true;
        } else if (d->bt >= 0 && ep == d->evt_dci) {
            bt_received(*d, 4, d->evt_done++ % kBtTrbs, kBtEventBuf, code, d2 & 0xFFFFFF);
            queue_bt_event(*d);
        } else if (d->bt >= 0 && ep == d->in_dci) {
            bt_received(*d, 2, kBtTrbs + d->acl_done++ % kBtTrbs, kBtAclBuf, code, d2 & 0xFFFFFF);
            queue_bt_acl(*d);
        } else if (d->bt >= 0 && d->sco_alt && ep == d->sco_in_dci) {
            // Ring overrun (no TRB queued when a packet came) finishes no
            // transfer; counting it as one made the driver read the wrong
            // buffers. So the buffer comes from the TRB the event names, and
            // that same buffer is queued again once its data is copied out.
            if (code == kCcRingUnderrun || code == kCcRingOverrun) return;
            const uint64_t buf = d->sco_in_ring.buffer_of(ptr);
            if (buf < d->sco_bufs.phys || buf >= d->sco_bufs.phys + kScoTrbs * kScoBuf) return;
            const uint32_t slot = uint32_t((buf - d->sco_bufs.phys) / kScoBuf);
            if (!(d->sco_in_busy & (1u << slot))) return;
            d->sco_in_busy &= ~(1u << slot);
            ++d->sco_in_done;
            sco_received(*d, slot, code, d2 & 0xFFFFFF);
            queue_sco_in(*d, slot);
        } else if (d->bt >= 0 && ep == d->sco_out_dci) {
            if (code != kCcRingUnderrun && code != kCcRingOverrun) ++d->sco_out_done;
        } else if ((d->storage || d->bt >= 0) && (ep == d->in_dci || ep == d->out_dci)) {
            d->bulk_code = code;
            d->bulk_done = true;
        } else if (d->pad != PadKind::None && ep == d->pad_in_dci) {
            const uint64_t buf = d->pad_ring.buffer_of(ptr);
            if (buf < d->pad_bufs.phys || buf >= d->pad_bufs.phys + kPadTrbs * kPadBuf) return;
            const uint32_t slot = uint32_t((buf - d->pad_bufs.phys) / kPadBuf);
            pad_received(*d, slot, code, d2 & 0xFFFFFF);
            queue_pad(*d, slot);
        } else if (d->pad != PadKind::None && ep == d->pad_out_dci) {
            // The player LED command went out.
        } else if (ep == d->dci) {
            if (code == kCcSuccess || code == kCcShortPacket) {
                const uint64_t buf = d->intr.buffer_of(ptr);
                if (buf < d->reports.phys || buf >= d->reports.phys + kReportTrbs * kReportSize) return;
                const auto* report = static_cast<const uint8_t*>(d->reports.virt) + (buf - d->reports.phys);
                if (d->hid == HidKind::Keyboard) keyboard_report(*d, report);
                else if (d->hid == HidKind::Tablet) tablet_report(*d, report);
                else mouse_report(report);
                queue_report(*d);
            } else if (code != kCcRingUnderrun && code != kCcRingOverrun && !d->intr_halt) {
                // Any other error (a STALL, or a transaction error through a
                // hub's transaction translator) halts the endpoint: nothing
                // more arrives until it is reset, which poll() does.
                d->intr_halt = uint8_t(code);
            }
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
            else if (usage >= 0x54 && usage <= 0x63) c = kHidKeypad[usage - 0x54];
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

    // A tablet's report: absolute X and Y, scaled to 0..32767, and buttons.
    void tablet_report(const Device& d, const uint8_t* r) {
        if (d.tab.report_id && r[0] != d.tab.report_id) return;
        const uint8_t* f = d.tab.report_id ? r + 1 : r;
        dhi_input_event ev{};
        ev.kind = DHI_INPUT_TABLET;
        for (int i = 0; i < d.tab.buttons && i < 3; ++i) ev.buttons |= uint8_t(bits(f, d.tab.button_bit + i, 1) << i);
        ev.dx = int16_t(uint64_t(bits(f, d.tab.x_bit, d.tab.x_size)) * 32767 / d.tab.x_max);
        ev.dy = int16_t(uint64_t(bits(f, d.tab.y_bit, d.tab.y_size)) * 32767 / d.tab.y_max);
        enqueue(ev);
    }

    // `size` bits (up to 32) starting at bit `at` of a little-endian report.
    static uint32_t bits(const uint8_t* r, uint32_t at, uint32_t size) {
        uint32_t v = 0;
        for (uint32_t i = 0; i < size && i < 32; ++i) v |= uint32_t((r[(at + i) / 8] >> ((at + i) % 8)) & 1) << i;
        return v;
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
    int count_ = 0;                        // entries used so far (some may be empty again)
    uint32_t generation_ = 0;
    uint64_t pending_ports_ = 0;           // bit per root port with a status change to look at
    uint32_t polls_ = 0;
    bool irq_ = false;
    dhi_dma hub_buf_{};
    uint8_t report_len_[kMaxSlots + 1] = {};

    // Scratch state while parsing one configuration descriptor.
    uint8_t config_value_ = 0, hid_iface_ = 0, hid_iface_of_device_ = 0, hid_interval_ = 0;
    HidKind hid_kind_ = HidKind::None;
    uint16_t hid_mps_ = 8;
    struct PadCandidate {
        uint8_t iface = 0, dci = 0, interval = 0;
        uint16_t mps = 0, desc_len = 0;
    };
    PadCandidate candidate_[kPadCandidates]{};
    int candidates_ = 0;

    struct PadReport {
        uint8_t pad = 0, len = 0;
        uint8_t data[kPadBuf] = {};
    };
    PadReport pad_queue_[kPadQueue]{};
    int pq_head_ = 0, pq_tail_ = 0;
    int pad_count_ = 0;

    bool caps_ = false;
    bool busy_ = false;  // event ring owner: a disk read, a Bluetooth call or the poll thread
    bool try_lock() { return !__atomic_exchange_n(&busy_, true, __ATOMIC_ACQUIRE); }
    void lock() {
        while (__atomic_exchange_n(&busy_, true, __ATOMIC_ACQUIRE)) __builtin_ia32_pause();
    }
    void unlock() { __atomic_store_n(&busy_, false, __ATOMIC_RELEASE); }

    struct BtChunk {
        uint8_t type = 0;
        uint16_t len = 0;
        uint8_t data[kBtAclBuf] = {};
    };
    struct BtQueue {
        BtChunk chunks[kBtQueue]{};
        int head = 0, tail = 0;
        uint32_t dropped = 0;
    };
    BtQueue bt_queues_[kMaxBt]{};
    struct ScoChunk {
        uint8_t len = 0;
        uint8_t data[kScoBuf] = {};
    };
    struct ScoQueue {
        ScoChunk chunks[kScoQueue]{};
        int head = 0, tail = 0;
        uint32_t dropped = 0;
    };
    ScoQueue sco_queues_[kMaxBt]{};
    int bt_count_ = 0;
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

extern "C" void aero_xhci_enable_irq(int32_t ctrl) {
    if (ctrl >= 0 && ctrl < g_count) g_controllers[ctrl].enable_irq();
}

extern "C" int32_t aero_xhci_poll(int32_t ctrl, dhi_input_event* out, int32_t max) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return 0;
    return g_controllers[ctrl].poll(out, max);
}

// Bluetooth ids: controller * kMaxDevices + device number.
extern "C" int32_t aero_xhci_bt(int32_t ctrl, int32_t index, dhi_usb_device* out) {
    if (ctrl < 0 || ctrl >= g_count) return -1;
    const int32_t dev = g_controllers[ctrl].bt_device(index, out);
    return dev < 0 ? -1 : ctrl * kMaxDevices + dev;
}

extern "C" int32_t aero_xhci_bt_send(int32_t bt, uint8_t type, const void* data, uint32_t len) {
    if (bt < 0 || bt / kMaxDevices >= g_count || data == nullptr) return -1;
    return g_controllers[bt / kMaxDevices].bt_send(bt % kMaxDevices, type, static_cast<const uint8_t*>(data), len);
}

extern "C" int32_t aero_xhci_bt_recv(int32_t bt, uint8_t* type, void* data, uint32_t max) {
    if (bt < 0 || bt / kMaxDevices >= g_count || data == nullptr || type == nullptr) return -1;
    return g_controllers[bt / kMaxDevices].bt_recv(bt % kMaxDevices, type, static_cast<uint8_t*>(data), max);
}

extern "C" int32_t aero_xhci_bt_sco(int32_t bt, uint8_t alt) {
    if (bt < 0 || bt / kMaxDevices >= g_count) return -1;
    return g_controllers[bt / kMaxDevices].bt_sco(bt % kMaxDevices, alt);
}

extern "C" int32_t aero_xhci_bt_control(int32_t bt, uint8_t req_type, uint8_t request, uint16_t value,
                                        uint16_t index, void* data, uint16_t len) {
    if (bt < 0 || bt / kMaxDevices >= g_count || (len > 0 && data == nullptr)) return -1;
    return g_controllers[bt / kMaxDevices].bt_control(bt % kMaxDevices, req_type, request, value, index,
                                                      static_cast<uint8_t*>(data), len);
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

extern "C" int32_t aero_xhci_write(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys) {
    if (disk < 0 || disk / kMaxDevices >= g_count) return -1;
    return g_controllers[disk / kMaxDevices].write(disk % kMaxDevices, lba, count, buf_phys);
}

extern "C" int32_t aero_xhci_flush(int32_t disk) {
    if (disk < 0 || disk / kMaxDevices >= g_count) return -1;
    return g_controllers[disk / kMaxDevices].flush(disk % kMaxDevices);
}

extern "C" int32_t aero_xhci_device(int32_t ctrl, int32_t index, dhi_usb_device* out) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr) return -1;
    return g_controllers[ctrl].device_info(index, out);
}

extern "C" int32_t aero_xhci_pad(int32_t ctrl, int32_t index, dhi_usb_device* out, void* desc, uint32_t max) {
    if (ctrl < 0 || ctrl >= g_count || out == nullptr || (max > 0 && desc == nullptr)) return -1;
    return g_controllers[ctrl].pad_info(index, out, static_cast<uint8_t*>(desc), max);
}

extern "C" uint32_t aero_xhci_generation(int32_t ctrl) {
    if (ctrl < 0 || ctrl >= g_count) return 0;
    return g_controllers[ctrl].generation();
}

extern "C" int32_t aero_xhci_pad_report(int32_t ctrl, int32_t* pad, void* data, uint32_t max) {
    if (ctrl < 0 || ctrl >= g_count || pad == nullptr || data == nullptr) return 0;
    return g_controllers[ctrl].pad_report(pad, static_cast<uint8_t*>(data), max);
}
