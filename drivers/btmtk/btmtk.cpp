// AeroForge MediaTek Bluetooth set-up (C++20, freestanding), behind the
// Driver Host Interface.
//
// MediaTek's MT7921 (MT7961) and MT7922 combo chips start their Bluetooth
// side with only a boot ROM: before the normal HCI commands work, the host
// has to download a firmware patch with MediaTek's vendor "WMT" commands and
// then switch the Bluetooth function on. This follows Linux's btmtk driver
// (drivers/bluetooth/btmtk.c): WMT commands go out as HCI command 0xFC6F,
// their answers are fetched by polling a vendor control request on EP0, and
// chip registers are read with vendor control requests. The transport is the
// xHCI driver's Bluetooth interface.

#include "dhi.h"

namespace {

constexpr uint8_t kWmtPatchDownload = 0x01;
constexpr uint8_t kWmtFuncCtrl = 0x06;
constexpr uint8_t kHciEventWmt = 0xE4;

enum class Status { Invalid, PatchUndone, PatchProgress, PatchDone, OnUndone, OnDone, OnProgress };

// Patch file layout (btmtk.c).
constexpr uint32_t kRomPatchHeaderSize = 32;
constexpr uint32_t kGlobalDescSize = 64;
constexpr uint32_t kSectionMapSize = 64;
constexpr uint32_t kSectionCommonSize = 12;
constexpr uint32_t kSectionSendSize = 52;

constexpr uint32_t kRegChipId = 0x70010200;
constexpr uint32_t kRegFwVersion = 0x80021004;
constexpr uint32_t kRegFlavor = 0x70010020;
constexpr uint32_t kRegEpResetOpt = 0x74011890;

struct UsbId {
    uint16_t vendor, product;
};

// Adapters Linux's btusb drives as MediaTek, besides vendor 0x0E8D itself.
constexpr UsbId kIds[] = {
    {0x13d3, 0x3560},
    {0x043e, 0x310c},
    {0x04ca, 0x3801},
    {0x043e, 0x3109},
    {0x0489, 0xe134},
    {0x0489, 0xe135},
    {0x13d3, 0x3620},
    {0x13d3, 0x3621},
    {0x13d3, 0x3622},
    {0x0489, 0xe158},
    {0x0489, 0xe0c8},
    {0x0489, 0xe0cd},
    {0x0489, 0xe0e0},
    {0x0489, 0xe0f2},
    {0x04ca, 0x3802},
    {0x0e8d, 0x0608},
    {0x13d3, 0x3563},
    {0x13d3, 0x3564},
    {0x13d3, 0x3567},
    {0x13d3, 0x3576},
    {0x13d3, 0x3578},
    {0x13d3, 0x3583},
    {0x13d3, 0x3606},
    {0x0489, 0xe156},
    {0x0e8d, 0x1ede},
    {0x13d3, 0x3579},
    {0x13d3, 0x3580},
    {0x13d3, 0x3594},
    {0x13d3, 0x3596},
    {0x13d3, 0x3585},
    {0x13d3, 0x3610},
    {0x0489, 0xe0d8},
    {0x0489, 0xe0d9},
    {0x0489, 0xe0e2},
    {0x0489, 0xe0e4},
    {0x0489, 0xe0f1},
    {0x0489, 0xe0f2},
    {0x0489, 0xe0f5},
    {0x0489, 0xe0f6},
    {0x0489, 0xe102},
    {0x0489, 0xe11d},
    {0x0489, 0xe152},
    {0x0489, 0xe153},
    {0x0489, 0xe170},
    {0x0489, 0xe174},
    {0x04ca, 0x3804},
    {0x04ca, 0x3807},
    {0x04ca, 0x38e4},
    {0x0e8d, 0x223c},
    {0x13d3, 0x3568},
    {0x13d3, 0x3584},
    {0x13d3, 0x3605},
    {0x13d3, 0x3607},
    {0x13d3, 0x3614},
    {0x13d3, 0x3615},
    {0x13d3, 0x3633},
    {0x35f5, 0x7922},
    {0x0489, 0xe111},
    {0x0489, 0xe113},
    {0x0489, 0xe118},
    {0x0489, 0xe11e},
    {0x0489, 0xe124},
    {0x0489, 0xe139},
    {0x0489, 0xe13a},
    {0x0489, 0xe0fa},
    {0x0489, 0xe10f},
    {0x0489, 0xe110},
    {0x0489, 0xe116},
    {0x13d3, 0x3588},
    {0x0489, 0xe14e},
    {0x0489, 0xe14f},
    {0x0489, 0xe150},
    {0x0489, 0xe151},
    {0x0e8d, 0x8c38},
    {0x13d3, 0x3602},
    {0x13d3, 0x3603},
    {0x13d3, 0x3604},
    {0x13d3, 0x3608},
    {0x13d3, 0x3609},
    {0x13d3, 0x3613},
    {0x13d3, 0x3625},
    {0x13d3, 0x3627},
    {0x13d3, 0x3628},
    {0x13d3, 0x3630},
    {0x2c7c, 0x7009}
};

uint32_t le32(const uint8_t* p) {
    return uint32_t(p[0]) | uint32_t(p[1]) << 8 | uint32_t(p[2]) << 16 | uint32_t(p[3]) << 24;
}

class Setup {
public:
    Setup(const dhi_ops* ops, int32_t bt) : ops_(ops), bt_(bt) {}

    // Vendor register read on EP0 (btmtk_usb_reg_read).
    bool reg_read(uint32_t reg, uint32_t* value) {
        uint8_t buf[4] = {};
        if (aero_xhci_bt_control(bt_, 0xC0, 0x63, uint16_t(reg >> 16), uint16_t(reg), buf, 4) != 4) return false;
        *value = le32(buf);
        return true;
    }

    // "UHW" register write (btmtk_usb_uhw_reg_write).
    bool uhw_write(uint32_t reg, uint32_t value) {
        uint8_t buf[4] = {uint8_t(value), uint8_t(value >> 8), uint8_t(value >> 16), uint8_t(value >> 24)};
        return aero_xhci_bt_control(bt_, 0x5E, 0x02, uint16_t(reg >> 16), uint16_t(reg), buf, 4) == 4;
    }

    // Sends one WMT command and waits for its WMT event.
    bool wmt(uint8_t op, uint8_t flag, const uint8_t* data, uint16_t dlen, Status* status) {
        uint8_t cmd[3 + 5 + 256];
        const uint32_t hlen = 5u + dlen;
        if (hlen > 255) return false;
        cmd[0] = 0x6F;  // opcode 0xFC6F, little-endian
        cmd[1] = 0xFC;
        cmd[2] = uint8_t(hlen);
        cmd[3] = 1;  // direction: host to chip
        cmd[4] = op;
        cmd[5] = uint8_t(dlen + 1);
        cmd[6] = uint8_t((dlen + 1) >> 8);
        cmd[7] = flag;
        for (uint16_t i = 0; i < dlen; ++i) cmd[8 + i] = data[i];
        if (aero_xhci_bt_send(bt_, DHI_BT_COMMAND, cmd, 3 + hlen) != 0) return false;

        // The answer does not come on the event endpoint: poll EP0 until the
        // chip has one ready (an empty reply means "not yet"), up to 3 s.
        uint8_t evt[64];
        int n = 0;
        for (int i = 0; i < 6000 && n <= 0; ++i) {
            n = aero_xhci_bt_control(bt_, 0xC0, 0x01, 48, 0, evt, sizeof(evt));
            if (n <= 0) ops_->delay_us(500);
        }
        if (n < 7 || evt[0] != kHciEventWmt || evt[3] != op) {
            last_error_ = n <= 0 ? "no WMT event" : "unexpected WMT event";
            return false;
        }
        const uint8_t evt_flag = evt[6];
        Status st = Status::Invalid;
        if (op == kWmtPatchDownload) {
            st = evt_flag == 2 ? Status::PatchDone : evt_flag == 1 ? Status::PatchProgress : Status::PatchUndone;
        } else if (op == kWmtFuncCtrl) {
            if (n < 9) {
                st = evt_flag ? Status::OnUndone : Status::OnDone;
            } else {
                const uint16_t code = uint16_t(evt[7] << 8 | evt[8]);  // big-endian
                st = code == 0x404 ? Status::OnDone : code == 0x420 ? Status::OnProgress : Status::OnUndone;
            }
        }
        if (status) *status = st;
        return true;
    }

    // btmtk_setup_firmware_79xx.
    int32_t download(const uint8_t* fw, uint32_t size) {
        if (size < kRomPatchHeaderSize + kGlobalDescSize) return fail("firmware file too short");
        const uint32_t sections = le32(fw + kRomPatchHeaderSize + 12);
        if (sections == 0 || sections > 64 ||
            kRomPatchHeaderSize + kGlobalDescSize + sections * kSectionMapSize > size)
            return fail("bad firmware section table");

        for (uint32_t i = 0; i < sections; ++i) {
            const uint8_t* map = fw + kRomPatchHeaderSize + kGlobalDescSize + kSectionMapSize * i;
            const uint32_t offset = le32(map + 4);
            uint32_t left = le32(map + kSectionCommonSize + 4);  // bin_info_spec.dlsize
            if (left == 0) continue;
            if (offset > size || left > size - offset) return fail("firmware section outside the file");

            // Announce the section; the chip says whether it still needs it.
            uint8_t announce[1 + kSectionSendSize];
            announce[0] = 0;  // legacy download mode
            for (uint32_t k = 0; k < kSectionSendSize; ++k) announce[1 + k] = map[kSectionCommonSize + k];
            Status st = Status::Invalid;
            bool skip = false;
            int retry = 20;
            for (; retry > 0; --retry) {
                if (!wmt(kWmtPatchDownload, 0, announce, sizeof(announce), &st)) return fail(last_error_);
                if (st == Status::PatchUndone) break;
                if (st == Status::PatchDone) {
                    skip = true;
                    break;
                }
                if (st != Status::PatchProgress) return fail("firmware download refused");
                ops_->delay_us(100000);
            }
            if (retry == 0) return fail("chip stayed busy");
            if (skip) continue;

            const uint8_t* p = fw + offset;
            bool first = true;
            while (left > 0) {
                const uint16_t chunk = left < 250 ? uint16_t(left) : 250;
                const uint8_t flag = first ? 1 : (left == chunk ? 3 : 2);
                first = false;
                if (!wmt(kWmtPatchDownload, flag, p, chunk, &st)) return fail(last_error_);
                if (st == Status::PatchProgress) return fail("unexpected progress status");
                left -= chunk;
                p += chunk;
                bytes_ += chunk;
            }
            ++sections_;
        }
        ops_->delay_us(110000);  // firmware activation
        return 0;
    }

    int32_t enable() {
        if (!uhw_write(kRegEpResetOpt, 0x00010001)) return fail("endpoint reset option write failed");
        const uint8_t on = 1;
        if (!wmt(kWmtFuncCtrl, 0, &on, 1, nullptr)) return fail(last_error_);
        return 0;
    }

    int32_t fail(const char* why) {
        last_error_ = why;
        return -1;
    }

    const char* last_error_ = "";
    uint32_t sections_ = 0, bytes_ = 0;

private:
    const dhi_ops* ops_;
    int32_t bt_;
};

char hex_digit(uint32_t v) { return "0123456789abcdef"[v & 0xF]; }

}  // namespace

extern "C" int32_t aero_btmtk_is_mediatek(uint16_t vendor, uint16_t product) {
    if (vendor == 0x0E8D) return 1;
    for (const UsbId& id : kIds)
        if (id.vendor == vendor && id.product == product) return 1;
    return 0;
}

extern "C" int32_t aero_btmtk_chip(const dhi_ops* ops, int32_t bt, dhi_btmtk_chip* out) {
    if (ops == nullptr || out == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
    Setup s(ops, bt);
    uint32_t flavor = 0;
    if (!s.reg_read(kRegChipId, &out->dev_id) || !s.reg_read(kRegFwVersion, &out->fw_version) ||
        !s.reg_read(kRegFlavor, &flavor))
        return -1;
    out->flavor = (flavor & 0x80) >> 7;
    // Firmware file name, as btmtk_fw_get_filename builds it.
    const uint32_t id = out->dev_id & 0xFFFF;
    const char* prefix = "mediatek/BT_RAM_CODE_MT";
    int n = 0;
    for (const char* c = prefix; *c; ++c) out->firmware[n++] = *c;
    for (int shift = 12; shift >= 0; shift -= 4) out->firmware[n++] = hex_digit(id >> shift);
    const char* mid = (id == 0x7961 && out->flavor) ? "_1a_" : "_1_";
    for (const char* c = mid; *c; ++c) out->firmware[n++] = *c;
    const uint32_t ver = (out->fw_version & 0xFF) + 1;
    if (ver >= 16) out->firmware[n++] = hex_digit(ver >> 4);
    out->firmware[n++] = hex_digit(ver);
    for (const char* c = "_hdr.bin"; *c; ++c) out->firmware[n++] = *c;
    out->firmware[n] = 0;
    out->supported = (out->dev_id == 0x7961 || out->dev_id == 0x7922) ? 1 : 0;
    return 0;
}

extern "C" int32_t aero_btmtk_setup(const dhi_ops* ops, int32_t bt, const void* firmware, uint32_t size,
                                    dhi_btmtk_result* out) {
    if (ops == nullptr || firmware == nullptr || out == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
    Setup s(ops, bt);
    int32_t rc = s.download(static_cast<const uint8_t*>(firmware), size);
    if (rc == 0) rc = s.enable();
    out->sections = s.sections_;
    out->bytes = s.bytes_;
    int i = 0;
    for (; s.last_error_[i] && i < int(sizeof(out->error)) - 1; ++i) out->error[i] = s.last_error_[i];
    out->error[i] = 0;
    return rc;
}
