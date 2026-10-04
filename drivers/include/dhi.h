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

#define DHI_ABI_VERSION 2u

/* A physically contiguous, kernel-owned buffer a device can DMA into. */
typedef struct dhi_dma {
    uint64_t phys;
    void    *virt;
    uint64_t size;
} dhi_dma;

/* Services the Rust kernel hands to every driver at init. */
typedef struct dhi_ops {
    uint32_t abi_version;
    uint32_t _reserved;
    void    (*log)(const char *msg);            /* NUL-terminated, kernel log */
    uint8_t (*port_in8)(uint16_t port);
    void    (*port_out8)(uint16_t port, uint8_t value);
    /* ABI 2 */
    int32_t (*dma_alloc)(uint64_t size, dhi_dma *out);  /* zeroed, page aligned; 0 = ok */
    void    (*dma_free)(const dhi_dma *buf);
    volatile void *(*map_mmio)(uint64_t phys, uint64_t size); /* uncached */
    void    (*delay_us)(uint32_t us);
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

/* ---- NVMe driver (drivers/nvme) ---- */

typedef struct dhi_block_info {
    uint64_t block_count;
    uint32_t block_size;
    uint32_t max_transfer;   /* bytes per read call */
    char     model[41];
    char     serial[21];
    char     firmware[9];
    uint8_t  _pad;
} dhi_block_info;

/* Brings up the controller at `bar0_phys` (bus mastering already enabled)
 * and its first namespace. Returns a controller id >= 0, or a negative error. */
int32_t aero_nvme_init(const dhi_ops *ops, uint64_t bar0_phys, dhi_block_info *out);

/* Reads `count` blocks starting at `lba` into the DMA buffer at `buf_phys`.
 * count * block_size must not exceed max_transfer. 0 = ok. */
int32_t aero_nvme_read(int32_t ctrl, uint64_t lba, uint32_t count, uint64_t buf_phys);

#ifdef __cplusplus
}
#endif

#endif /* AEROFORGE_DHI_H */
