// AeroForge PS/2 keyboard driver (C++20, freestanding).
// First C++ driver behind the Driver Host Interface: talks to the i8042
// controller and turns scancode set 1 into key events for the Rust kernel.

#include "dhi.h"

namespace {

constexpr uint16_t kDataPort   = 0x60;
constexpr uint16_t kStatusPort = 0x64;
constexpr uint8_t  kStatusOutputFull = 0x01;

// Scancode set 1 -> ASCII, unshifted and shifted. 0 = no printable char.
constexpr char kNormal[0x3A] = {
    0, 27, '1','2','3','4','5','6','7','8','9','0','-','=', '\b',
    '\t','q','w','e','r','t','y','u','i','o','p','[',']','\n',
    0, 'a','s','d','f','g','h','j','k','l',';','\'','`',
    0, '\\','z','x','c','v','b','n','m',',','.','/', 0,
    '*', 0, ' ',
};
constexpr char kShifted[0x3A] = {
    0, 27, '!','@','#','$','%','^','&','*','(',')','_','+', '\b',
    '\t','Q','W','E','R','T','Y','U','I','O','P','{','}','\n',
    0, 'A','S','D','F','G','H','J','K','L',':','"','~',
    0, '|','Z','X','C','V','B','N','M','<','>','?', 0,
    '*', 0, ' ',
};

constexpr uint8_t kLShift = 0x2A, kRShift = 0x36, kCtrl = 0x1D, kAlt = 0x38, kCaps = 0x3A;

class Ps2Keyboard {
public:
    int32_t init(const dhi_ops* ops) {
        if (ops == nullptr || ops->abi_version != DHI_ABI_VERSION) return -1;
        ops_ = ops;
        // Drain anything the firmware left in the output buffer.
        for (int i = 0; i < 64 && (ops_->port_in8(kStatusPort) & kStatusOutputFull); ++i)
            (void)ops_->port_in8(kDataPort);
        ops_->log("ps2kbd: i8042 ready, scancode set 1");
        return 0;
    }

    int32_t on_irq(dhi_key_event* out) {
        if (ops_ == nullptr || out == nullptr) return 0;
        const uint8_t byte = ops_->port_in8(kDataPort);

        if (byte == 0xE0) { extended_ = true; return 0; }   // prefix; next byte is the key
        const bool was_extended = extended_;
        extended_ = false;

        const bool pressed = (byte & 0x80) == 0;
        const uint8_t code = byte & 0x7F;

        switch (code) {
            case kLShift: case kRShift: shift_ = pressed; break;
            case kCtrl: ctrl_ = pressed; break;
            case kAlt:  alt_ = pressed; break;
            case kCaps: if (pressed) caps_ = !caps_; break;
            default: break;
        }

        out->scancode  = code;
        out->pressed   = pressed ? 1 : 0;
        out->modifiers = modifiers();
        out->ascii     = was_extended ? 0 : translate(code);
        return 1;
    }

private:
    uint8_t modifiers() const {
        return (shift_ ? DHI_MOD_SHIFT : 0) | (ctrl_ ? DHI_MOD_CTRL : 0) |
               (alt_ ? DHI_MOD_ALT : 0) | (caps_ ? DHI_MOD_CAPS : 0);
    }

    uint8_t translate(uint8_t code) const {
        if (code >= sizeof(kNormal)) return 0;
        char c = shift_ ? kShifted[code] : kNormal[code];
        if (caps_ && c >= 'a' && c <= 'z') c = static_cast<char>(c - 'a' + 'A');
        else if (caps_ && c >= 'A' && c <= 'Z') c = static_cast<char>(c - 'A' + 'a');
        return static_cast<uint8_t>(c);
    }

    const dhi_ops* ops_ = nullptr;
    bool shift_ = false, ctrl_ = false, alt_ = false, caps_ = false, extended_ = false;
};

// constinit: no global constructors, since the kernel runs none.
constinit Ps2Keyboard g_keyboard{};

}  // namespace

extern "C" int32_t aero_ps2kbd_init(const dhi_ops* ops) { return g_keyboard.init(ops); }
extern "C" int32_t aero_ps2kbd_on_irq(dhi_key_event* out) { return g_keyboard.on_irq(out); }
