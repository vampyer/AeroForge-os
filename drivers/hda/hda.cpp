// AeroForge Intel High Definition Audio driver (C++20, freestanding), behind
// the Driver Host Interface.
//
// Covers the HDA controller (the motherboard's audio chip, and the audio
// function of graphics cards that carries HDMI/DisplayPort sound) and the
// codecs on its link: it resets the link, talks to the codecs through the
// CORB/RIRB command rings, walks each codec's widget graph to find the
// outputs (pins) and a path from a DAC to each, and plays 48 kHz 16-bit
// stereo through one output stream with a cyclic buffer. Polled, no
// interrupts.

#include "dhi.h"

namespace {

// Controller registers (HDA specification 1.0a, section 3.3).
constexpr uint32_t kGcap = 0x00;
constexpr uint32_t kGctl = 0x08;
constexpr uint32_t kStatests = 0x0E;
constexpr uint32_t kIntctl = 0x20;
constexpr uint32_t kCorbLbase = 0x40;
constexpr uint32_t kCorbUbase = 0x44;
constexpr uint32_t kCorbWp = 0x48;
constexpr uint32_t kCorbRp = 0x4A;
constexpr uint32_t kCorbCtl = 0x4C;
constexpr uint32_t kCorbSize = 0x4E;
constexpr uint32_t kRirbLbase = 0x50;
constexpr uint32_t kRirbUbase = 0x54;
constexpr uint32_t kRirbWp = 0x58;
constexpr uint32_t kRintCnt = 0x5A;
constexpr uint32_t kRirbCtl = 0x5C;
constexpr uint32_t kRirbSts = 0x5D;
constexpr uint32_t kRirbSize = 0x5E;
constexpr uint32_t kStreams = 0x80;  // stream descriptors, 0x20 each

// Stream descriptor registers.
constexpr uint32_t kSdCtl = 0x00;
constexpr uint32_t kSdSts = 0x03;
constexpr uint32_t kSdLpib = 0x04;
constexpr uint32_t kSdCbl = 0x08;
constexpr uint32_t kSdLvi = 0x0C;
constexpr uint32_t kSdFmt = 0x12;
constexpr uint32_t kSdBdpl = 0x18;
constexpr uint32_t kSdBdpu = 0x1C;

// Codec verbs (section 7.3).
constexpr uint32_t kGetParameter = 0xF00;
constexpr uint32_t kGetConnectionList = 0xF02;
constexpr uint32_t kGetPinSense = 0xF09;
constexpr uint32_t kGetConfigDefault = 0xF1C;
constexpr uint32_t kSetConnectionSelect = 0x701;
constexpr uint32_t kSetPowerState = 0x705;
constexpr uint32_t kSetStream = 0x706;
constexpr uint32_t kSetPinControl = 0x707;
constexpr uint32_t kSetEapd = 0x70C;
constexpr uint32_t kSetFormat = 0x2;      // 4-bit verbs, 16-bit payload
constexpr uint32_t kSetAmp = 0x3;

// Parameters.
constexpr uint8_t kParVendor = 0x00;
constexpr uint8_t kParNodes = 0x04;
constexpr uint8_t kParFunctionType = 0x05;
constexpr uint8_t kParWidgetCaps = 0x09;
constexpr uint8_t kParPinCaps = 0x0C;
constexpr uint8_t kParInAmpCaps = 0x0D;
constexpr uint8_t kParConnListLen = 0x0E;
constexpr uint8_t kParOutAmpCaps = 0x12;

// Widget types (widget capabilities bits 20-23).
constexpr uint32_t kWidgetOutput = 0;
constexpr uint32_t kWidgetMixer = 2;
constexpr uint32_t kWidgetSelector = 3;
constexpr uint32_t kWidgetPin = 4;

constexpr uint32_t kNoResponse = 0xFFFFFFFFu;
constexpr int kMaxNodes = 128;
constexpr int kMaxPath = 6;

// Playback buffer: 4 x 16 KiB, about a third of a second at 48 kHz stereo.
constexpr uint32_t kBdlEntries = 4;
constexpr uint32_t kChunk = 16384;
constexpr uint32_t kBufBytes = kBdlEntries * kChunk;
constexpr uint16_t kFormat48kStereo16 = 0x0011;  // 48 kHz base, 16 bits, 2 channels
constexpr uint8_t kStreamTag = 1;

struct Output {
    uint8_t codec;
    uint8_t afg;  // its audio function group (default amplifier caps)
    uint8_t pin;
    uint8_t path[kMaxPath];  // pin first, DAC last
    uint8_t length;
    dhi_hda_output info;
};

class Controller {
public:
    int32_t init(const dhi_ops* ops, uint64_t bar0, dhi_hda_info* info) {
        ops_ = ops;
        regs_ = static_cast<volatile uint8_t*>(ops_->map_mmio(bar0, 0x4000));
        if (regs_ == nullptr) return -1;
        const uint16_t gcap = r16(kGcap);
        in_streams_ = (gcap >> 8) & 0xF;
        out_streams_ = (gcap >> 12) & 0xF;
        if (out_streams_ == 0) return -2;

        // Reset the link: CRST low, then high, then give the codecs time
        // to announce themselves (at least 521 us, section 4.3).
        w32(kGctl, r32(kGctl) & ~1u);
        for (int i = 0; i < 1000 && (r32(kGctl) & 1); ++i) ops_->delay_us(10);
        ops_->delay_us(100);
        w32(kGctl, r32(kGctl) | 1);
        for (int i = 0; i < 1000 && !(r32(kGctl) & 1); ++i) ops_->delay_us(10);
        if (!(r32(kGctl) & 1)) return -3;
        ops_->delay_us(1000);
        w32(kIntctl, 0);
        const uint16_t present = r16(kStatests);
        w16(kStatests, present);
        if (!setup_rings()) return -4;

        info->out_streams = out_streams_;
        info->in_streams = in_streams_;
        for (uint8_t c = 0; c < 15; ++c) {
            if (!(present & (1u << c))) continue;
            const uint32_t id = param(c, 0, kParVendor);
            if (id == kNoResponse) continue;
            if (info->codec_count < 4) info->codec_ids[info->codec_count++] = id;
            scan_codec(c);
        }
        info->output_count = uint8_t(output_count_);
        for (int i = 0; i < output_count_; ++i) info->outputs[i] = outputs_[i].info;
        return 0;
    }

    // Routes output `index` and starts the stream (silent until written).
    int32_t start(int index) {
        if (index < 0 || index >= output_count_) return -1;
        stop();
        if (!buf_.virt && (ops_->dma_alloc(kBufBytes, &buf_) != 0 || ops_->dma_alloc(4096, &bdl_) != 0)) return -2;
        const Output& o = outputs_[index];
        route(o);

        sd_ = kStreams + 0x20u * in_streams_;  // first output stream
        w8(sd_ + kSdCtl, 0);
        w8(sd_ + kSdCtl, 1);  // stream reset
        for (int i = 0; i < 1000 && !(r8(sd_ + kSdCtl) & 1); ++i) ops_->delay_us(10);
        w8(sd_ + kSdCtl, 0);
        for (int i = 0; i < 1000 && (r8(sd_ + kSdCtl) & 1); ++i) ops_->delay_us(10);

        auto* buf = static_cast<volatile uint8_t*>(buf_.virt);
        for (uint32_t i = 0; i < kBufBytes; ++i) buf[i] = 0;
        auto* bdl = static_cast<volatile uint32_t*>(bdl_.virt);
        for (uint32_t i = 0; i < kBdlEntries; ++i) {
            const uint64_t a = buf_.phys + uint64_t(i) * kChunk;
            bdl[i * 4 + 0] = uint32_t(a);
            bdl[i * 4 + 1] = uint32_t(a >> 32);
            bdl[i * 4 + 2] = kChunk;
            bdl[i * 4 + 3] = 0;  // no interrupt on completion: we poll
        }
        w32(sd_ + kSdBdpl, uint32_t(bdl_.phys));
        w32(sd_ + kSdBdpu, uint32_t(bdl_.phys >> 32));
        w32(sd_ + kSdCbl, kBufBytes);
        w16(sd_ + kSdLvi, kBdlEntries - 1);
        w16(sd_ + kSdFmt, kFormat48kStereo16);
        w8(sd_ + kSdSts, 0x1C);
        w8(sd_ + kSdCtl + 2, uint8_t(kStreamTag << 4));
        write_pos_ = 0;
        last_lpib_ = 0;
        written_ = played_ = 0;
        w8(sd_ + kSdCtl, 0x02);  // run
        running_ = true;
        return 0;
    }

    void stop() {
        if (!running_) return;
        w8(sd_ + kSdCtl, 0);
        running_ = false;
    }

    // Queues 16-bit stereo frames; returns how many fit.
    int32_t write(const int16_t* frames, uint32_t count) {
        if (!running_) return -1;
        service();
        const uint64_t queued = written_ - played_;
        const uint32_t room = queued + 64 < kBufBytes ? uint32_t(kBufBytes - 64 - queued) & ~3u : 0;
        const uint32_t n = count * 4 < room ? count * 4 : room;
        auto* dst = static_cast<volatile uint8_t*>(buf_.virt);
        const auto* src = reinterpret_cast<const uint8_t*>(frames);
        for (uint32_t i = 0; i < n; ++i) dst[(write_pos_ + i) % kBufBytes] = src[i];
        write_pos_ = (write_pos_ + n) % kBufBytes;
        written_ += n;
        return int32_t(n / 4);
    }

    // Frames written but not played yet.
    int32_t pending() {
        if (!running_) return 0;
        service();
        return int32_t((written_ - played_) / 4);
    }

private:
    // Follows the play position (LPIB) and clears what has been played, so
    // the cyclic buffer goes silent when nothing new is written instead of
    // repeating old sound. After an underrun, writing resumes at the play
    // position.
    void service() {
        const uint32_t lpib = r32(sd_ + kSdLpib) % kBufBytes;
        auto* buf = static_cast<volatile uint8_t*>(buf_.virt);
        for (uint32_t p = last_lpib_; p != lpib; p = (p + 1) % kBufBytes) buf[p] = 0;
        played_ += (lpib + kBufBytes - last_lpib_) % kBufBytes;
        last_lpib_ = lpib;
        if (played_ > written_) {
            written_ = played_;
            write_pos_ = lpib;
        }
    }

    bool setup_rings() {
        if (ops_->dma_alloc(4096, &rings_) != 0) return false;
        // Stop both rings, then 256 entries each.
        w8(kCorbCtl, 0);
        w8(kRirbCtl, 0);
        for (int i = 0; i < 1000 && ((r8(kCorbCtl) & 2) || (r8(kRirbCtl) & 2)); ++i) ops_->delay_us(10);
        w32(kCorbLbase, uint32_t(rings_.phys));
        w32(kCorbUbase, uint32_t(rings_.phys >> 32));
        // 256 entries where supported, else 16, else 2 (size capability bits 4-6).
        const uint8_t corb_caps = r8(kCorbSize) >> 4;
        corb_entries_ = (corb_caps & 4) ? 256 : (corb_caps & 2) ? 16 : 2;
        w8(kCorbSize, corb_entries_ == 256 ? 2 : corb_entries_ == 16 ? 1 : 0);
        w16(kCorbRp, 0x8000);  // reset the read pointer
        for (int i = 0; i < 1000 && !(r16(kCorbRp) & 0x8000); ++i) ops_->delay_us(10);
        w16(kCorbRp, 0);
        for (int i = 0; i < 1000 && (r16(kCorbRp) & 0x8000); ++i) ops_->delay_us(10);
        w16(kCorbWp, 0);
        w32(kRirbLbase, uint32_t(rings_.phys + 2048));
        w32(kRirbUbase, uint32_t((rings_.phys + 2048) >> 32));
        const uint8_t rirb_caps = r8(kRirbSize) >> 4;
        rirb_entries_ = (rirb_caps & 4) ? 256 : (rirb_caps & 2) ? 16 : 2;
        w8(kRirbSize, rirb_entries_ == 256 ? 2 : rirb_entries_ == 16 ? 1 : 0);
        w16(kRirbWp, 0x8000);  // reset the write pointer
        w16(kRintCnt, 0xFF);
        w8(kCorbCtl, 2);
        w8(kRirbCtl, 2);
        corb_wp_ = 0;
        rirb_rp_ = 0;
        return true;
    }

    // Sends one verb and waits for its answer.
    uint32_t command(uint8_t codec, uint8_t node, uint32_t verb_payload) {
        auto* corb = static_cast<volatile uint32_t*>(rings_.virt);
        auto* rirb = reinterpret_cast<volatile uint32_t*>(static_cast<volatile uint8_t*>(rings_.virt) + 2048);
        corb_wp_ = uint16_t((corb_wp_ + 1) % corb_entries_);
        corb[corb_wp_] = uint32_t(codec) << 28 | uint32_t(node) << 20 | (verb_payload & 0xFFFFF);
        w16(kCorbWp, corb_wp_);
        for (int i = 0; i < 2000; ++i) {
            while (rirb_rp_ != (r16(kRirbWp) & 0xFF) % rirb_entries_) {
                rirb_rp_ = uint16_t((rirb_rp_ + 1) % rirb_entries_);
                const uint32_t resp = rirb[rirb_rp_ * 2];
                const uint32_t ex = rirb[rirb_rp_ * 2 + 1];
                w8(kRirbSts, 0x05);  // lets the controller go on after RINTCNT responses
                if (!(ex & 0x10)) return resp;  // not an unsolicited response
            }
            ops_->delay_us(5);
        }
        return kNoResponse;
    }

    uint32_t verb(uint8_t c, uint8_t n, uint32_t v, uint8_t payload) { return command(c, n, v << 8 | payload); }
    uint32_t param(uint8_t c, uint8_t n, uint8_t p) { return verb(c, n, kGetParameter, p); }

    // The nodes `node` can take input from.
    int connections(uint8_t c, uint8_t node, uint8_t* out, int max) {
        const uint32_t len_par = param(c, node, kParConnListLen);
        if (len_par == kNoResponse) return 0;
        const bool long_form = len_par & 0x80;
        const int len = int(len_par & 0x7F);
        int n = 0;
        int prev = -1;
        for (int i = 0; i < len && n < max;) {
            const uint32_t entries = verb(c, node, kGetConnectionList, uint8_t(i));
            const int per = long_form ? 2 : 4;
            for (int k = 0; k < per && i < len && n < max; ++k, ++i) {
                const uint32_t e = long_form ? (entries >> (16 * k)) & 0xFFFF : (entries >> (8 * k)) & 0xFF;
                const uint32_t id = long_form ? e & 0x7FFF : e & 0x7F;
                const bool range = long_form ? e & 0x8000 : e & 0x80;
                if (range && prev >= 0) {
                    for (uint32_t r = uint32_t(prev) + 1; r <= id && n < max; ++r) out[n++] = uint8_t(r);
                } else {
                    out[n++] = uint8_t(id);
                }
                prev = int(id);
            }
        }
        return n;
    }

    // Depth-first search from `node` towards a DAC.
    bool find_dac(uint8_t c, uint8_t node, Output& o, int depth) {
        if (depth >= kMaxPath) return false;
        o.path[depth] = node;
        const uint32_t type = (caps_[node] >> 20) & 0xF;
        if (type == kWidgetOutput && depth > 0) {
            o.length = uint8_t(depth + 1);
            return true;
        }
        if (depth > 0 && type != kWidgetMixer && type != kWidgetSelector) return false;
        uint8_t list[32];
        const int n = connections(c, node, list, 32);
        for (int i = 0; i < n; ++i)
            if (list[i] >= first_ && list[i] < first_ + count_ && find_dac(c, list[i], o, depth + 1)) return true;
        return false;
    }

    void scan_codec(uint8_t c) {
        const uint32_t sub = param(c, 0, kParNodes);
        if (sub == kNoResponse) return;
        for (uint32_t fg = (sub >> 16) & 0xFF, end = fg + (sub & 0xFF); fg < end; ++fg) {
            if ((param(c, uint8_t(fg), kParFunctionType) & 0xFF) != 1) continue;  // audio function group
            verb(c, uint8_t(fg), kSetPowerState, 0);
            const uint32_t nodes = param(c, uint8_t(fg), kParNodes);
            first_ = uint8_t((nodes >> 16) & 0xFF);
            count_ = uint8_t(nodes & 0xFF);
            if (first_ + count_ > kMaxNodes) count_ = uint8_t(kMaxNodes - first_);
            for (int n = first_; n < first_ + count_; ++n) caps_[n] = param(c, uint8_t(n), kParWidgetCaps);
            for (int n = first_; n < first_ + count_ && output_count_ < DHI_HDA_MAX_OUTPUTS; ++n) {
                if (((caps_[n] >> 20) & 0xF) != kWidgetPin) continue;
                const uint32_t pincaps = param(c, uint8_t(n), kParPinCaps);
                if (!(pincaps & (1u << 4))) continue;  // not output capable
                const uint32_t config = verb(c, uint8_t(n), kGetConfigDefault, 0);
                if ((config >> 30) == 1) continue;  // nothing connected to this pin
                Output o{};
                o.codec = c;
                o.pin = uint8_t(n);
                o.afg = uint8_t(fg);
                if (!find_dac(c, uint8_t(n), o, 0)) continue;
                describe(o, pincaps, config);
                outputs_[output_count_++] = o;
            }
        }
    }

    void describe(Output& o, uint32_t pincaps, uint32_t config) {
        dhi_hda_output& d = o.info;
        d.codec = o.codec;
        d.pin = o.pin;
        d.dac = o.path[o.length - 1];
        const uint32_t device = (config >> 20) & 0xF;
        const bool digital = (pincaps & (1u << 7)) || (pincaps & (1u << 24));  // HDMI or DisplayPort
        d.kind = digital ? DHI_HDA_HDMI
               : device == 1 ? DHI_HDA_SPEAKER
               : device == 2 ? DHI_HDA_HEADPHONES
               : device == 4 || device == 5 ? DHI_HDA_SPDIF
               : DHI_HDA_LINE_OUT;
        d.location = uint8_t((config >> 24) & 0x3F);
        d.color = uint8_t((config >> 12) & 0xF);
        d.fixed = (config >> 30) == 2;  // built in (laptop speaker)
        if (pincaps & (1u << 2)) {  // presence detect
            const uint32_t sense = verb(o.codec, o.pin, kGetPinSense, 0);
            d.plugged = sense == kNoResponse ? 2 : (sense >> 31) ? 1 : 0;
        } else {
            d.plugged = 2;
        }
    }

    // Unmutes and connects every widget on the path, sets the pin to drive
    // out, and points the DAC at our stream.
    void route(const Output& o) {
        const uint8_t c = o.codec;
        for (int i = 0; i < o.length; ++i) {
            const uint8_t n = o.path[i];
            verb(c, n, kSetPowerState, 0);
            const uint32_t caps = param(c, n, kParWidgetCaps);
            if (i + 1 < o.length) {
                // Select the next node down the path (pins and selectors);
                // mixers mix all inputs, so unmute the one we use.
                uint8_t list[32];
                const int count = connections(c, n, list, 32);
                for (int k = 0; k < count; ++k) {
                    if (list[k] != o.path[i + 1]) continue;
                    const uint32_t type = (caps >> 20) & 0xF;
                    if (type == kWidgetMixer) {
                        if (caps & (1u << 1)) amp(o, n, caps, false, uint8_t(k));
                    } else if (count > 1) {
                        verb(c, n, kSetConnectionSelect, uint8_t(k));
                    }
                    if (type != kWidgetMixer && (caps & (1u << 1))) amp(o, n, caps, false, uint8_t(k));
                }
            }
            if (caps & (1u << 2)) amp(o, n, caps, true, 0);  // output amplifier
        }
        const uint32_t pincaps = param(c, o.pin, kParPinCaps);
        const bool headphones = o.info.kind == DHI_HDA_HEADPHONES && (pincaps & (1u << 3));
        verb(c, o.pin, kSetPinControl, uint8_t(0x40 | (headphones ? 0x80 : 0)));
        if (pincaps & (1u << 16)) verb(c, o.pin, kSetEapd, 0x02);  // external amplifier on
        const uint8_t dac = o.path[o.length - 1];
        command(c, dac, kSetFormat << 16 | kFormat48kStereo16);
        verb(c, dac, kSetStream, uint8_t(kStreamTag << 4));
    }

    // Unmutes an amplifier at its 0 dB step (or the loudest one below it).
    void amp(const Output& o, uint8_t n, uint32_t widget_caps, bool output, uint8_t index) {
        // Widgets without their own amplifier caps (override bit 3) use the
        // function group's.
        const uint8_t from = (widget_caps & (1u << 3)) ? n : o.afg;
        const uint32_t caps = param(o.codec, from, output ? kParOutAmpCaps : kParInAmpCaps);
        const uint32_t offset = caps == kNoResponse ? 0 : caps & 0x7F;
        // Set output/input, left and right, this index, unmuted, gain.
        const uint32_t payload = (output ? 0x8000u : 0x4000u) | 0x3000u | uint32_t(index) << 8 | offset;
        command(o.codec, n, kSetAmp << 16 | payload);
    }

    uint8_t r8(uint32_t o) { return regs_[o]; }
    uint16_t r16(uint32_t o) { return *reinterpret_cast<volatile uint16_t*>(regs_ + o); }
    uint32_t r32(uint32_t o) { return *reinterpret_cast<volatile uint32_t*>(regs_ + o); }
    void w8(uint32_t o, uint8_t v) { regs_[o] = v; }
    void w16(uint32_t o, uint16_t v) { *reinterpret_cast<volatile uint16_t*>(regs_ + o) = v; }
    void w32(uint32_t o, uint32_t v) { *reinterpret_cast<volatile uint32_t*>(regs_ + o) = v; }

    const dhi_ops* ops_ = nullptr;
    volatile uint8_t* regs_ = nullptr;
    uint8_t in_streams_ = 0, out_streams_ = 0;
    dhi_dma rings_{}, buf_{}, bdl_{};
    uint16_t corb_wp_ = 0, rirb_rp_ = 0;
    uint16_t corb_entries_ = 256, rirb_entries_ = 256;
    uint32_t caps_[kMaxNodes]{};
    uint8_t first_ = 0, count_ = 0;
    Output outputs_[DHI_HDA_MAX_OUTPUTS]{};
    int output_count_ = 0;
    uint32_t sd_ = 0;
    bool running_ = false;
    uint32_t write_pos_ = 0, last_lpib_ = 0;
    uint64_t written_ = 0, played_ = 0;  // bytes, since start
};

constexpr int kMaxControllers = 4;
Controller g_ctrl[kMaxControllers];
int g_count = 0;

bool valid(int32_t c) { return c >= 0 && c < g_count; }

}  // namespace

extern "C" int32_t aero_hda_init(const dhi_ops* ops, uint64_t bar0_phys, dhi_hda_info* info) {
    if (ops == nullptr || info == nullptr || ops->abi_version != DHI_ABI_VERSION || g_count >= kMaxControllers) return -1;
    const int32_t rc = g_ctrl[g_count].init(ops, bar0_phys, info);
    if (rc < 0) return rc;
    return g_count++;
}

extern "C" int32_t aero_hda_start(int32_t ctrl, int32_t output) {
    return valid(ctrl) ? g_ctrl[ctrl].start(output) : -1;
}

extern "C" int32_t aero_hda_write(int32_t ctrl, const int16_t* frames, uint32_t count) {
    return valid(ctrl) && frames ? g_ctrl[ctrl].write(frames, count) : -1;
}

extern "C" int32_t aero_hda_pending(int32_t ctrl) {
    return valid(ctrl) ? g_ctrl[ctrl].pending() : 0;
}

extern "C" void aero_hda_stop(int32_t ctrl) {
    if (valid(ctrl)) g_ctrl[ctrl].stop();
}
