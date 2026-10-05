/*
 * AeroForge Driver Host Interface (DHI), ABI version 2.
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

/* ---- AHCI (SATA) driver (drivers/ahci) ---- */

/* Brings up the AHCI controller whose ABAR (BAR5) is at `abar_phys` and
 * identifies every SATA disk on it. Fills up to `max` entries of `out` and
 * returns how many disks were found (negative on controller error). Disk ids
 * for aero_ahci_read are 0..n-1 across all controllers, in discovery order,
 * starting at the value returned through `first_id`. */
int32_t aero_ahci_init(const dhi_ops *ops, uint64_t abar_phys, dhi_block_info *out,
                       int32_t max, int32_t *first_id);

/* Reads `count` sectors starting at `lba` into the DMA buffer at `buf_phys`.
 * count * block_size must not exceed max_transfer. 0 = ok. */
int32_t aero_ahci_read(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys);

/* ---- xHCI (USB 3) driver (drivers/xhci) ---- */

#define DHI_INPUT_KEY   1u
#define DHI_INPUT_MOUSE 2u

/* One input event from a USB HID device. For keys, key.scancode holds the
 * HID usage id (keyboard page), not a PS/2 scancode. */
typedef struct dhi_input_event {
    uint8_t  kind;      /* DHI_INPUT_* */
    uint8_t  buttons;   /* mouse: bit 0 left, 1 right, 2 middle */
    int16_t  dx, dy;    /* mouse: relative motion */
    uint16_t _pad;
    dhi_key_event key;  /* keyboard: press events only */
} dhi_input_event;

/* What the driver learned about one USB device. */
typedef struct dhi_usb_device {
    uint16_t vendor, product;
    uint8_t  port, speed, slot, dev_class;   /* speed: 1 FS, 2 LS, 3 HS, 4 SS 5 Gb/s, 5 SS+ 10 Gb/s, 6 SS+ 20 Gb/s */
    uint8_t  iface_class, iface_subclass, iface_protocol;
    uint8_t  parent_slot;                    /* 0 = root hub, else the hub's slot; port is on that hub */
    char     name[32];                       /* product string, ASCII */
} dhi_usb_device;

/* Resets the controller whose registers (BAR0) are at `mmio_phys`, enumerates
 * the devices on its root ports and starts HID boot keyboards and mice.
 * Returns a controller id >= 0 and the device count through `devices`. */
int32_t aero_xhci_init(const dhi_ops *ops, uint64_t mmio_phys, int32_t *devices);

/* Drains the controller's event ring. Fills up to `max` input events and
 * returns how many. Call regularly (the driver polls; no interrupts yet). */
int32_t aero_xhci_poll(int32_t ctrl, dhi_input_event *out, int32_t max);

/* Describes device `index` (0..devices-1) of a controller. 0 = ok. */
int32_t aero_xhci_device(int32_t ctrl, int32_t index, dhi_usb_device *out);

#ifdef __cplusplus
}
#endif

#endif /* AEROFORGE_DHI_H */
