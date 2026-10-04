/*
 * AeroForge Driver Host Interface (DHI), ABI version 1.
 *
 * The one boundary between the Rust kernel core and C++ drivers.
 * Rules (design doc section 1.3):
 *   - plain C types and extern "C" functions only, no classes/exceptions/RTTI
 *   - Rust owns lifetimes and security decisions, C++ owns the hardware
 *   - drivers never allocate on their own; they ask the kernel through dhi_ops
 *
 * TODO(dhi.idl): generate this header and kernel/src/dhi.rs from one IDL file
 * so the two sides cannot drift. Until then, keep them in sync by hand.
 */
#ifndef AEROFORGE_DHI_H
#define AEROFORGE_DHI_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define DHI_ABI_VERSION 1u

/* Services the Rust kernel hands to every driver at init. */
typedef struct dhi_ops {
    uint32_t abi_version;
    uint32_t _reserved;
    void    (*log)(const char *msg);            /* NUL-terminated, kernel log */
    uint8_t (*port_in8)(uint16_t port);
    void    (*port_out8)(uint16_t port, uint8_t value);
} dhi_ops;

/* Key event produced by input drivers. */
typedef struct dhi_key_event {
    uint8_t  scancode;   /* raw set-1 make code, release bit stripped */
    uint8_t  pressed;    /* 1 = make, 0 = break */
    uint8_t  ascii;      /* translated character, 0 if none */
    uint8_t  modifiers;  /* DHI_MOD_* bits */
} dhi_key_event;

#define DHI_MOD_SHIFT 0x01u
#define DHI_MOD_CTRL  0x02u
#define DHI_MOD_ALT   0x04u
#define DHI_MOD_CAPS  0x08u

/* ---- PS/2 keyboard driver (drivers/ps2kbd) ---- */

/* Returns 0 on success, negative on error. `ops` must outlive the driver. */
int32_t aero_ps2kbd_init(const dhi_ops *ops);

/* Call from the IRQ1 handler. Returns 1 and fills `out` if a key event
 * was decoded, 0 if the byte was consumed without an event. */
int32_t aero_ps2kbd_on_irq(dhi_key_event *out);

#ifdef __cplusplus
}
#endif

#endif /* AEROFORGE_DHI_H */
