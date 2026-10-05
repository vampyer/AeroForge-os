// AeroForge virtio-gpu driver (C++20, freestanding), behind the Driver Host
// Interface.
//
// QEMU's paravirtual GPU, in 2D mode: the guest keeps each image
// (a "resource") in its own memory, and tells the device which part changed
// so it can copy it over and show it. This is the first GPU driver behind
// the kernel's display layer (design doc section 4.3, stage G1); the same
// layer later sits on the Radeon.
//
// Virtio 1.x over PCI (the "modern" interface): the kernel finds the
// common, notify, ISR and device-config regions from the PCI capabilities.
// One control queue; commands are synchronous (submit, then poll the used
// ring), which is fine for 2D. No interrupts.

#include "dhi.h"

namespace {

// Common configuration (virtio 1.2, section 4.1.4.3).
constexpr uint32_t kDeviceFeatureSelect = 0x00;
constexpr uint32_t kDeviceFeature = 0x04;
constexpr uint32_t kDriverFeatureSelect = 0x08;
constexpr uint32_t kDriverFeature = 0x0C;
constexpr uint32_t kDeviceStatus = 0x14;
constexpr uint32_t kQueueSelect = 0x16;
constexpr uint32_t kQueueSize = 0x18;
constexpr uint32_t kQueueMsixVector = 0x1A;
constexpr uint32_t kQueueEnable = 0x1C;
constexpr uint32_t kQueueNotifyOff = 0x1E;
constexpr uint32_t kQueueDesc = 0x20;
constexpr uint32_t kQueueDriver = 0x28;
constexpr uint32_t kQueueDevice = 0x30;

constexpr uint8_t kStatusAck = 1;
constexpr uint8_t kStatusDriver = 2;
constexpr uint8_t kStatusDriverOk = 4;
constexpr uint8_t kStatusFeaturesOk = 8;
constexpr uint32_t kFeatureVersion1 = 1;  // bit 32, as bit 0 of the high word

// Split virtqueue descriptor flags.
constexpr uint16_t kDescNext = 1;
constexpr uint16_t kDescWrite = 2;

// GPU commands and responses (virtio 1.2, section 5.7.6).
constexpr uint32_t kCmdGetDisplayInfo = 0x0100;
constexpr uint32_t kCmdResourceCreate2d = 0x0101;
constexpr uint32_t kCmdResourceUnref = 0x0102;
constexpr uint32_t kCmdSetScanout = 0x0103;
constexpr uint32_t kCmdResourceFlush = 0x0104;
constexpr uint32_t kCmdTransferToHost2d = 0x0105;
constexpr uint32_t kCmdAttachBacking = 0x0106;
constexpr uint32_t kRespOkNodata = 0x1100;
constexpr uint32_t kRespOkDisplayInfo = 0x1101;
constexpr uint32_t kFormatB8G8R8X8 = 2;  // bytes B, G, R, X: our 0x00RRGGBB pixels

constexpr uint16_t kQueueLen = 16;
constexpr uint32_t kReqSize = 64 * 1024;  // fits a backing list of 4000 pages
constexpr uint32_t kMaxBackingPages = (kReqSize - 32) / 16;

struct CtrlHdr {
    uint32_t type, flags;
    uint64_t fence;
    uint32_t ctx;
    uint8_t ring, pad[3];
};

struct Rect {
    uint32_t x, y, w, h;
};

struct Desc {
    uint64_t addr;
    uint32_t len;
    uint16_t flags, next;
};

class Gpu {
public:
    int32_t init(const dhi_ops* ops, const dhi_virtio_pci* pci, uint32_t* width, uint32_t* height) {
        ops_ = ops;
        common_ = static_cast<volatile uint8_t*>(ops->map_mmio(pci->common, 0x40));
        notify_ = static_cast<volatile uint8_t*>(ops->map_mmio(pci->notify, 0x1000));
        notify_mult_ = pci->notify_mult;

        w8(kDeviceStatus, 0);
        for (int i = 0; i < 1000 && r8(kDeviceStatus) != 0; i++) ops->delay_us(10);
        w8(kDeviceStatus, kStatusAck | kStatusDriver);
        w32(kDeviceFeatureSelect, 1);
        if (!(r32(kDeviceFeature) & kFeatureVersion1)) {
            ops->log("virtio-gpu: device lacks VIRTIO_F_VERSION_1");
            return -1;
        }
        // No optional features (no 3D, no EDID): plain 2D.
        w32(kDriverFeatureSelect, 0);
        w32(kDriverFeature, 0);
        w32(kDriverFeatureSelect, 1);
        w32(kDriverFeature, kFeatureVersion1);
        w8(kDeviceStatus, kStatusAck | kStatusDriver | kStatusFeaturesOk);
        if (!(r8(kDeviceStatus) & kStatusFeaturesOk)) {
            ops->log("virtio-gpu: features not accepted");
            return -2;
        }

        // Control queue 0: descriptors, driver ring and device ring in one page.
        w16(kQueueSelect, 0);
        uint16_t size = r16(kQueueSize);
        if (size == 0) return -3;
        qlen_ = size < kQueueLen ? size : kQueueLen;
        w16(kQueueSize, qlen_);
        if (ops->dma_alloc(4096, &ring_) != 0 || ops->dma_alloc(kReqSize, &req_) != 0 ||
            ops->dma_alloc(4096, &resp_) != 0) {
            return -4;
        }
        desc_ = static_cast<volatile Desc*>(ring_.virt);
        avail_ = reinterpret_cast<volatile uint16_t*>(static_cast<volatile uint8_t*>(ring_.virt) + 1024);
        used_ = reinterpret_cast<volatile uint16_t*>(static_cast<volatile uint8_t*>(ring_.virt) + 2048);
        w64(kQueueDesc, ring_.phys);
        w64(kQueueDriver, ring_.phys + 1024);
        w64(kQueueDevice, ring_.phys + 2048);
        w16(kQueueMsixVector, 0xFFFF);
        queue_notify_ = notify_ + uint32_t(r16(kQueueNotifyOff)) * notify_mult_;
        w16(kQueueEnable, 1);
        w8(kDeviceStatus, kStatusAck | kStatusDriver | kStatusFeaturesOk | kStatusDriverOk);

        // The first enabled scanout's preferred size.
        auto* h = request<CtrlHdr>();
        *h = CtrlHdr{kCmdGetDisplayInfo, 0, 0, 0, 0, {}};
        if (submit(sizeof(CtrlHdr), 24 + 16 * 24) != kRespOkDisplayInfo) return -5;
        auto* modes = reinterpret_cast<volatile uint32_t*>(static_cast<volatile uint8_t*>(resp_.virt) + 24);
        *width = 1024;
        *height = 768;
        for (int i = 0; i < 16; i++) {
            volatile uint32_t* m = modes + i * 6;  // rect (4), enabled, flags
            if (m[4] && m[2] && m[3]) {
                *width = m[2];
                *height = m[3];
                break;
            }
        }
        return 0;
    }

    int32_t create(uint32_t id, uint32_t w, uint32_t h, const uint64_t* pages, uint32_t count) {
        if (count > kMaxBackingPages) return -1;
        struct Create {
            CtrlHdr hdr;
            uint32_t id, format, w, h;
        };
        *request<Create>() = Create{{kCmdResourceCreate2d, 0, 0, 0, 0, {}}, id, kFormatB8G8R8X8, w, h};
        if (submit(sizeof(Create), sizeof(CtrlHdr)) != kRespOkNodata) return -2;

        struct Attach {
            CtrlHdr hdr;
            uint32_t id, entries;
        };
        auto* a = request<Attach>();
        *a = Attach{{kCmdAttachBacking, 0, 0, 0, 0, {}}, id, count};
        auto* e = reinterpret_cast<volatile uint32_t*>(static_cast<uint8_t*>(req_.virt) + sizeof(Attach));
        for (uint32_t i = 0; i < count; i++) {
            e[i * 4 + 0] = uint32_t(pages[i]);
            e[i * 4 + 1] = uint32_t(pages[i] >> 32);
            e[i * 4 + 2] = 4096;
            e[i * 4 + 3] = 0;
        }
        return submit(sizeof(Attach) + 16 * count, sizeof(CtrlHdr)) == kRespOkNodata ? 0 : -3;
    }

    int32_t destroy(uint32_t id) {
        struct Unref {
            CtrlHdr hdr;
            uint32_t id, pad;
        };
        *request<Unref>() = Unref{{kCmdResourceUnref, 0, 0, 0, 0, {}}, id, 0};
        return submit(sizeof(Unref), sizeof(CtrlHdr)) == kRespOkNodata ? 0 : -1;
    }

    int32_t scanout(uint32_t id, uint32_t w, uint32_t h) {
        struct SetScanout {
            CtrlHdr hdr;
            Rect r;
            uint32_t scanout, id;
        };
        *request<SetScanout>() = SetScanout{{kCmdSetScanout, 0, 0, 0, 0, {}}, {0, 0, w, h}, 0, id};
        return submit(sizeof(SetScanout), sizeof(CtrlHdr)) == kRespOkNodata ? 0 : -1;
    }

    int32_t flush(uint32_t id, uint32_t x, uint32_t y, uint32_t w, uint32_t h, uint32_t stride) {
        struct Transfer {
            CtrlHdr hdr;
            Rect r;
            uint64_t offset;
            uint32_t id, pad;
        };
        *request<Transfer>() = Transfer{{kCmdTransferToHost2d, 0, 0, 0, 0, {}}, {x, y, w, h},
                                        uint64_t(y) * stride + uint64_t(x) * 4, id, 0};
        if (submit(sizeof(Transfer), sizeof(CtrlHdr)) != kRespOkNodata) return -1;
        struct Flush {
            CtrlHdr hdr;
            Rect r;
            uint32_t id, pad;
        };
        *request<Flush>() = Flush{{kCmdResourceFlush, 0, 0, 0, 0, {}}, {x, y, w, h}, id, 0};
        return submit(sizeof(Flush), sizeof(CtrlHdr)) == kRespOkNodata ? 0 : -2;
    }

private:
    template <typename T>
    T* request() {
        return static_cast<T*>(req_.virt);
    }

    // Sends the request in req_ and waits for the response in resp_;
    // returns the response type (0 if the device never answered).
    uint32_t submit(uint32_t req_len, uint32_t resp_len) {
        auto* resp = static_cast<volatile CtrlHdr*>(resp_.virt);
        resp->type = 0;
        desc_[0].addr = req_.phys;
        desc_[0].len = req_len;
        desc_[0].flags = kDescNext;
        desc_[0].next = 1;
        desc_[1].addr = resp_.phys;
        desc_[1].len = resp_len;
        desc_[1].flags = kDescWrite;
        desc_[1].next = 0;
        uint16_t idx = avail_[1];
        avail_[2 + idx % qlen_] = 0;
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        avail_[1] = uint16_t(idx + 1);
        __atomic_thread_fence(__ATOMIC_SEQ_CST);
        *reinterpret_cast<volatile uint16_t*>(queue_notify_) = 0;
        for (int i = 0; i < 200000; i++) {
            if (used_[1] == last_used_ + 1) {
                last_used_++;
                __atomic_thread_fence(__ATOMIC_SEQ_CST);
                return resp->type;
            }
            if (i > 1000) ops_->delay_us(1);
        }
        ops_->log("virtio-gpu: command timed out");
        last_used_ = used_[1];
        return 0;
    }

    uint8_t r8(uint32_t o) { return common_[o]; }
    uint16_t r16(uint32_t o) { return *reinterpret_cast<volatile uint16_t*>(common_ + o); }
    uint32_t r32(uint32_t o) { return *reinterpret_cast<volatile uint32_t*>(common_ + o); }
    void w8(uint32_t o, uint8_t v) { common_[o] = v; }
    void w16(uint32_t o, uint16_t v) { *reinterpret_cast<volatile uint16_t*>(common_ + o) = v; }
    void w32(uint32_t o, uint32_t v) { *reinterpret_cast<volatile uint32_t*>(common_ + o) = v; }
    void w64(uint32_t o, uint64_t v) {
        w32(o, uint32_t(v));
        w32(o + 4, uint32_t(v >> 32));
    }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* common_ = nullptr;
    volatile uint8_t* notify_ = nullptr;
    volatile uint8_t* queue_notify_ = nullptr;
    uint32_t notify_mult_ = 0;
    dhi_dma ring_{}, req_{}, resp_{};
    volatile Desc* desc_ = nullptr;
    volatile uint16_t* avail_ = nullptr;  // flags, idx, ring[]
    volatile uint16_t* used_ = nullptr;   // flags, idx, ring[] of {id, len}
    uint16_t qlen_ = 0;
    uint16_t last_used_ = 0;
};

Gpu g_gpu;
bool g_ready = false;

}  // namespace

extern "C" {

int32_t aero_vgpu_init(const dhi_ops* ops, const dhi_virtio_pci* pci, uint32_t* width, uint32_t* height) {
    if (!ops || ops->abi_version < 2 || g_ready) return -100;
    int32_t r = g_gpu.init(ops, pci, width, height);
    g_ready = r == 0;
    return r;
}

int32_t aero_vgpu_create(uint32_t id, uint32_t width, uint32_t height, const uint64_t* pages, uint32_t count) {
    return g_ready ? g_gpu.create(id, width, height, pages, count) : -100;
}

int32_t aero_vgpu_destroy(uint32_t id) {
    return g_ready ? g_gpu.destroy(id) : -100;
}

int32_t aero_vgpu_scanout(uint32_t id, uint32_t width, uint32_t height) {
    return g_ready ? g_gpu.scanout(id, width, height) : -100;
}

int32_t aero_vgpu_flush(uint32_t id, uint32_t x, uint32_t y, uint32_t w, uint32_t h, uint32_t stride) {
    return g_ready ? g_gpu.flush(id, x, y, w, h, stride) : -100;
}

}  // extern "C"
