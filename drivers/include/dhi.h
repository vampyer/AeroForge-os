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

/* USB mass storage (bulk-only transport, SCSI) found during init. Fills
 * `out` for the controller's disk `index` (0, 1, ...) and returns a disk id
 * for aero_xhci_read, or -1 when there is no such disk. */
int32_t aero_xhci_disk(int32_t ctrl, int32_t index, dhi_block_info *out);

/* Reads `count` blocks starting at `lba` into the DMA buffer at `buf_phys`.
 * count * block_size must not exceed max_transfer. 0 = ok. Safe to call
 * while another CPU runs aero_xhci_poll. */
int32_t aero_xhci_read(int32_t disk, uint64_t lba, uint32_t count, uint64_t buf_phys);

/* Bluetooth adapters (USB class E0/01/01) found during init. Fills `out` for
 * the controller's adapter `index` (0, 1, ...) and returns an adapter id, or
 * -1 when there is no such adapter. */
int32_t aero_xhci_bt(int32_t ctrl, int32_t index, dhi_usb_device *out);

#define DHI_BT_COMMAND 1u  /* HCI packet types, as in the UART transport */
#define DHI_BT_ACL     2u
#define DHI_BT_SCO     3u  /* voice: sent on, and received from, the isochronous endpoints */
#define DHI_BT_EVENT   4u

/* Sends one HCI command (on EP0) or ACL packet (on bulk OUT), header
 * included. 0 = ok. */
int32_t aero_xhci_bt_send(int32_t bt, uint8_t type, const void *data, uint32_t len);

/* Copies the next received chunk of the event or ACL byte stream into
 * `data` and its type into `type`. Chunks are USB transfers, not whole
 * packets: the caller reassembles. Returns the length, or 0 if nothing is
 * waiting. Never blocks. */
int32_t aero_xhci_bt_recv(int32_t bt, uint8_t *type, void *data, uint32_t max);

/* Selects the alternate setting of the adapter's voice (SCO) interface:
 * 0 stops voice, a higher one gives more isochronous bandwidth (2 for one
 * 16-bit CVSD link). Received voice then arrives as DHI_BT_SCO chunks
 * (one isochronous packet each). 0 = ok. */
int32_t aero_xhci_bt_sco(int32_t bt, uint8_t alt);

/* A raw control transfer on EP0 for vendor set-up (firmware download).
 * Returns the bytes transferred, or -1 (for example on a STALL). */
int32_t aero_xhci_bt_control(int32_t bt, uint8_t req_type, uint8_t request, uint16_t value,
                             uint16_t index, void *data, uint16_t len);

/* ---- Intel Ethernet driver, e1000/e1000e family (drivers/e1000) ---- */

typedef struct dhi_net_info {
    uint8_t  mac[6];
    uint8_t  link_up;     /* 1 = cable connected and negotiated */
    uint8_t  _pad;
    uint32_t speed_mbps;  /* 10, 100, 1000 or 2500 */
} dhi_net_info;

/* 1 if this driver handles Intel (vendor 0x8086) device `device_id`. */
int32_t aero_e1000_supports(uint16_t device_id);

/* Resets the NIC whose registers (BAR0) are at `mmio_phys`, sets up one
 * receive and one transmit ring and waits briefly for the link. Returns a
 * NIC id >= 0, or a negative error. */
int32_t aero_e1000_init(const dhi_ops *ops, uint64_t mmio_phys, dhi_net_info *info);

/* Queues one Ethernet frame (without CRC, at most 2048 bytes). 0 = queued,
 * negative if the transmit ring is full or the frame is invalid. */
int32_t aero_e1000_send(int32_t nic, const void *frame, uint32_t len);

/* Copies the next received frame into `frame`. Returns its length, 0 if
 * nothing is waiting, or -1 for a dropped (bad or oversized) frame. */
int32_t aero_e1000_recv(int32_t nic, void *frame, uint32_t max);

/* Returns 1 if the link is up and reports the current speed. */
int32_t aero_e1000_link(int32_t nic, uint32_t *speed_mbps);

/* ---- Intel Ethernet driver, igb/igc family (drivers/igc) ----
 * igb: 82576, I350, I210, I211. igc: I225, I226 (2.5 Gb/s).
 * Same calls as the e1000 driver; init also takes the PCI device id, which
 * picks the family. */
int32_t aero_igc_supports(uint16_t device_id);
int32_t aero_igc_init(const dhi_ops *ops, uint64_t mmio_phys, uint16_t device_id, dhi_net_info *info);
int32_t aero_igc_send(int32_t nic, const void *frame, uint32_t len);
int32_t aero_igc_recv(int32_t nic, void *frame, uint32_t max);
int32_t aero_igc_link(int32_t nic, uint32_t *speed_mbps);

/* ---- MediaTek Bluetooth set-up (drivers/btmtk) ----
 * MT7921 and MT7922 adapters boot with a ROM only: the firmware patch has to
 * be downloaded before ordinary HCI commands work. */

typedef struct dhi_btmtk_chip {
    uint32_t dev_id;       /* 0x7961 = MT7921, 0x7922 = MT7922 */
    uint32_t fw_version;
    uint32_t flavor;
    uint32_t supported;    /* 1 if aero_btmtk_setup knows this chip */
    char     firmware[64]; /* linux-firmware path, e.g. mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin */
} dhi_btmtk_chip;

typedef struct dhi_btmtk_result {
    uint32_t sections;     /* firmware sections downloaded */
    uint32_t bytes;        /* firmware bytes downloaded */
    char     error[48];    /* why it failed, empty on success */
} dhi_btmtk_result;

/* 1 if the USB adapter vendor:product is a MediaTek Bluetooth chip. */
int32_t aero_btmtk_is_mediatek(uint16_t vendor, uint16_t product);

/* Reads the chip id and firmware version over the Bluetooth adapter `bt`
 * (an aero_xhci_bt id) and names the firmware file it needs. 0 = ok. */
int32_t aero_btmtk_chip(const dhi_ops *ops, int32_t bt, dhi_btmtk_chip *out);

/* Downloads `firmware` and switches the Bluetooth function on. 0 = ok. */
int32_t aero_btmtk_setup(const dhi_ops *ops, int32_t bt, const void *firmware, uint32_t size,
                         dhi_btmtk_result *out);

/* ---- Intel High Definition Audio driver (drivers/hda) ---- */

#define DHI_HDA_MAX_OUTPUTS 8
#define DHI_HDA_LINE_OUT   0u
#define DHI_HDA_SPEAKER    1u
#define DHI_HDA_HEADPHONES 2u
#define DHI_HDA_SPDIF      3u
#define DHI_HDA_HDMI       4u  /* HDMI or DisplayPort (graphics card audio) */

/* One output jack (pin) with a path from a DAC. */
typedef struct dhi_hda_output {
    uint8_t codec;
    uint8_t pin;
    uint8_t dac;
    uint8_t kind;      /* DHI_HDA_* */
    uint8_t location;  /* configuration default bits 24-29 */
    uint8_t color;     /* configuration default bits 12-15 */
    uint8_t fixed;     /* built in (a laptop speaker) */
    uint8_t plugged;   /* 1 = something plugged in, 0 = nothing, 2 = cannot tell */
} dhi_hda_output;

typedef struct dhi_hda_info {
    uint32_t codec_ids[4];   /* vendor << 16 | device */
    uint8_t  codec_count;
    uint8_t  in_streams;
    uint8_t  out_streams;
    uint8_t  output_count;
    dhi_hda_output outputs[DHI_HDA_MAX_OUTPUTS];
} dhi_hda_info;

/* Resets the controller whose registers are at `bar0_phys` (bus mastering
 * already enabled), finds its codecs and their outputs. Returns a
 * controller id >= 0, or a negative error. */
int32_t aero_hda_init(const dhi_ops *ops, uint64_t bar0_phys, dhi_hda_info *out);

/* Routes output `output` (an index into dhi_hda_info.outputs) to a 48 kHz
 * 16-bit stereo stream and starts it, silent until written. 0 = ok. */
int32_t aero_hda_start(int32_t ctrl, int32_t output);

/* Queues interleaved left/right frames; returns how many fit (the buffer
 * holds about a third of a second). Played sound is cleared behind the
 * play position, so the stream goes silent when nothing more is written. */
int32_t aero_hda_write(int32_t ctrl, const int16_t *frames, uint32_t count);

/* Frames written but not played yet. */
int32_t aero_hda_pending(int32_t ctrl);

void aero_hda_stop(int32_t ctrl);

#ifdef __cplusplus
}
#endif

#endif /* AEROFORGE_DHI_H */
