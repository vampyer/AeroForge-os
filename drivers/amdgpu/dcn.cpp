// AeroForge display-engine driver for AMD Radeon graphics with DCN 2.1
// (Renoir, Lucienne and Cezanne APUs, such as the Vega 8 in a Ryzen 9
// 5900HX), behind the Driver Host Interface.
//
// First stage (design doc section 4.3): the firmware has already set a
// screen mode and is scanning out a linear framebuffer. This driver does not
// change the mode. It finds the display pipe the firmware uses, reads its
// state, and moves its scan-out address at the next vertical blank (a
// "page flip"), which is what tear-free double buffering needs.
//
// Register offsets are dword indices from the Linux amdgpu headers
// (include/asic_reg/dcn/dcn_2_1_0_offset.h, _sh_mask.h and
// include/renoir_ip_offset.h, MIT licensed by AMD). The DCN registers sit in
// segment 2 of the DMU block: byte offset = (0x34C0 + reg) * 4 into the
// register BAR (BAR 5).

#include "dhi.h"

namespace {

constexpr uint32_t kDcnSeg2 = 0x34C0;
constexpr uint32_t kNbifSeg2 = 0x0D20;
constexpr uint32_t kMmioSize = 512 * 1024;

constexpr uint32_t kPipes = 4;           // HUBPs and OTGs on DCN 2.1
constexpr uint32_t kHubpStride = 0xDC;   // HUBP1 - HUBP0
constexpr uint32_t kOtgStride = 0x80;    // OTG1 - OTG0

// HUBP / HUBPREQ instance 0 (add pipe * kHubpStride).
constexpr uint32_t kSurfaceConfig = 0x05E5;     // HUBP0_DCSURF_SURFACE_CONFIG
constexpr uint32_t kHubpCntl = 0x05F3;          // HUBP0_DCHUBP_CNTL
constexpr uint32_t kSurfacePitch = 0x0607;      // HUBPREQ0_DCSURF_SURFACE_PITCH
constexpr uint32_t kSurfaceAddr = 0x060A;       // HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS
constexpr uint32_t kSurfaceAddrHigh = 0x060B;   // ..._HIGH
constexpr uint32_t kFlipControl = 0x061B;       // HUBPREQ0_DCSURF_FLIP_CONTROL
constexpr uint32_t kInuse = 0x0625;             // HUBPREQ0_DCSURF_SURFACE_EARLIEST_INUSE
constexpr uint32_t kInuseHigh = 0x0626;         // ..._HIGH

// OTG instance 0 (add otg * kOtgStride).
constexpr uint32_t kOtgHTotal = 0x1B2A;
constexpr uint32_t kOtgVTotal = 0x1B2F;
constexpr uint32_t kOtgControl = 0x1B41;
constexpr uint32_t kOtgStatus = 0x1B49;
constexpr uint32_t kOtgFrameCount = 0x1B4C;     // OTG_STATUS_FRAME_COUNT

// DCHUBBUB system aperture, in 16 MiB units (Linux: ADDR_HI24).
constexpr uint32_t kVmFbLocationBase = 0x0493;  // DCN_VM_FB_LOCATION_BASE
constexpr uint32_t kVmFbLocationTop = 0x0494;   // DCN_VM_FB_LOCATION_TOP
constexpr uint32_t kVmFbOffset = 0x0495;        // DCN_VM_FB_OFFSET

constexpr uint32_t kRccConfigMemsize = 0x00C3;  // NBIO 7.0, segment 2

// Fields.
constexpr uint32_t kHubpDisable = 1u << 2;
constexpr uint32_t kHubpBlank = 1u << 0;
constexpr uint32_t kFlipUpdateLock = 1u << 0;
constexpr uint32_t kFlipTypeImmediate = 1u << 1;
constexpr uint32_t kFlipPending = 1u << 8;
constexpr uint32_t kOtgMasterEn = 1u << 0;
constexpr uint32_t kFrameCountMask = 0x00FF'FFFF;
constexpr uint32_t kPitchMask = 0x3FFF;

const dhi_ops* g_ops = nullptr;
volatile uint32_t* g_mmio = nullptr;
int32_t g_pipe = -1;
int32_t g_otg = -1;

uint32_t rd(uint32_t seg, uint32_t reg) { return g_mmio[seg + reg]; }
void wr(uint32_t seg, uint32_t reg, uint32_t v) { g_mmio[seg + reg] = v; }

uint32_t hubp(uint32_t reg, uint32_t pipe) { return rd(kDcnSeg2, reg + pipe * kHubpStride); }
void hubp_wr(uint32_t reg, uint32_t pipe, uint32_t v) { wr(kDcnSeg2, reg + pipe * kHubpStride, v); }
uint32_t otg(uint32_t reg, uint32_t n) { return rd(kDcnSeg2, reg + n * kOtgStride); }

}  // namespace

extern "C" {

int32_t aero_dcn_init(const dhi_ops* ops, uint64_t mmio_phys, dhi_dcn_info* out) {
    if (!ops || ops->abi_version < 2 || !out || !mmio_phys) return -100;
    g_ops = ops;
    g_mmio = static_cast<volatile uint32_t*>(ops->map_mmio(mmio_phys, kMmioSize));
    if (!g_mmio) return -1;

    out->memsize_mb = rd(kNbifSeg2, kRccConfigMemsize);
    out->fb_base = uint64_t(rd(kDcnSeg2, kVmFbLocationBase) & 0xFF'FFFF) << 24;
    out->fb_top = (uint64_t(rd(kDcnSeg2, kVmFbLocationTop) & 0xFF'FFFF) << 24) | 0xFF'FFFF;
    out->fb_offset = uint64_t(rd(kDcnSeg2, kVmFbOffset) & 0xFF'FFFF) << 24;
    out->pipe = -1;
    out->otg = -1;
    for (uint32_t i = 0; i < kPipes; i++) {
        dhi_dcn_pipe& p = out->pipes[i];
        p.hubp_cntl = hubp(kHubpCntl, i);
        p.surface_config = hubp(kSurfaceConfig, i);
        p.pitch = hubp(kSurfacePitch, i) & kPitchMask;
        p.flip_control = hubp(kFlipControl, i);
        p.address = (uint64_t(hubp(kSurfaceAddrHigh, i)) << 32) | hubp(kSurfaceAddr, i);
        p.inuse = (uint64_t(hubp(kInuseHigh, i)) << 32) | hubp(kInuse, i);
        p.otg_control = otg(kOtgControl, i);
        p.otg_status = otg(kOtgStatus, i);
        p.frame_count = otg(kOtgFrameCount, i) & kFrameCountMask;
        p.h_total = otg(kOtgHTotal, i);
        p.v_total = otg(kOtgVTotal, i);
        // The firmware's pipe: enabled, not blanked, with a surface.
        if (out->pipe < 0 && !(p.hubp_cntl & (kHubpDisable | kHubpBlank)) && p.address)
            out->pipe = int32_t(i);
        if (out->otg < 0 && (p.otg_control & kOtgMasterEn))
            out->otg = int32_t(i);
    }
    g_pipe = out->pipe;
    g_otg = out->otg;
    return (g_pipe >= 0 && g_otg >= 0) ? 0 : -2;
}

// Frames the timing generator has started (24 bits, wraps).
uint32_t aero_dcn_frame_count(void) {
    return g_otg >= 0 ? otg(kOtgFrameCount, uint32_t(g_otg)) & kFrameCountMask : 0;
}

// Queues a flip to `address` (the display engine's address of a linear
// surface laid out like the current one). It takes effect at the start of
// the next vertical blank; until then the old surface is still read.
int32_t aero_dcn_flip(uint64_t address) {
    if (g_pipe < 0) return -100;
    uint32_t p = uint32_t(g_pipe);
    uint32_t ctl = hubp(kFlipControl, p);
    hubp_wr(kFlipControl, p, ctl & ~(kFlipTypeImmediate | kFlipUpdateLock));
    // As in Linux's hubp2_program_surface_flip_and_addr: high half first; the
    // write of the low half arms the flip.
    hubp_wr(kSurfaceAddrHigh, p, uint32_t(address >> 32));
    hubp_wr(kSurfaceAddr, p, uint32_t(address));
    return 0;
}

// 1 while a queued flip has not happened yet.
int32_t aero_dcn_flip_pending(void) {
    if (g_pipe < 0) return 0;
    return (hubp(kFlipControl, uint32_t(g_pipe)) & kFlipPending) ? 1 : 0;
}

// The surface the display engine is reading now.
uint64_t aero_dcn_scanout(void) {
    if (g_pipe < 0) return 0;
    uint32_t p = uint32_t(g_pipe);
    return (uint64_t(hubp(kInuseHigh, p)) << 32) | hubp(kInuse, p);
}

}  // extern "C"
