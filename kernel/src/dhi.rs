//! Rust side of the Driver Host Interface. Must match drivers/include/dhi.h.

use core::ffi::{c_char, CStr};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{apic, arch, memory, sched};

pub const ABI_VERSION: u32 = 3;

#[repr(C)]
pub struct DmaBuf {
    pub phys: u64,
    pub virt: *mut u8,
    pub size: u64,
}

#[repr(C)]
pub struct DhiOps {
    pub abi_version: u32,
    pub _reserved: u32,
    pub log: extern "C" fn(*const c_char),
    pub port_in8: extern "C" fn(u16) -> u8,
    pub port_out8: extern "C" fn(u16, u8),
    pub dma_alloc: extern "C" fn(u64, *mut DmaBuf) -> i32,
    pub dma_free: extern "C" fn(*const DmaBuf),
    pub map_mmio: extern "C" fn(u64, u64) -> *mut u8,
    pub delay_us: extern "C" fn(u32),
    pub irq_wait: extern "C" fn(u32),
}

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct KeyEvent {
    pub scancode: u8,
    pub pressed: u8,
    pub ascii: u8,
    pub modifiers: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BlockInfo {
    pub block_count: u64,
    pub block_size: u32,
    pub max_transfer: u32,
    pub model: [u8; 41],
    pub serial: [u8; 21],
    pub firmware: [u8; 9],
    pub _pad: u8,
}

impl BlockInfo {
    pub const fn zeroed() -> Self {
        Self { block_count: 0, block_size: 0, max_transfer: 0, model: [0; 41], serial: [0; 21], firmware: [0; 9], _pad: 0 }
    }
}

pub fn c_field(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("?").trim()
}

pub const INPUT_KEY: u8 = 1;
pub const INPUT_MOUSE: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputEvent {
    pub kind: u8,
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    pub _pad: u16,
    pub key: KeyEvent,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct UsbDevice {
    pub vendor: u16,
    pub product: u16,
    pub port: u8,
    pub speed: u8,
    pub slot: u8,
    pub dev_class: u8,
    pub iface_class: u8,
    pub iface_subclass: u8,
    pub iface_protocol: u8,
    pub parent_slot: u8,
    pub name: [u8; 32],
}

impl UsbDevice {
    pub const fn zeroed() -> Self {
        Self { vendor: 0, product: 0, port: 0, speed: 0, slot: 0, dev_class: 0, iface_class: 0,
            iface_subclass: 0, iface_protocol: 0, parent_slot: 0, name: [0; 32] }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct NetInfo {
    pub mac: [u8; 6],
    pub link_up: u8,
    pub _pad: u8,
    pub speed_mbps: u32,
}

pub const BT_COMMAND: u8 = 1;
pub const BT_ACL: u8 = 2;
pub const BT_SCO: u8 = 3;
pub const BT_EVENT: u8 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BtmtkChip {
    pub dev_id: u32,
    pub fw_version: u32,
    pub flavor: u32,
    pub supported: u32,
    pub firmware: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BtmtkResult {
    pub sections: u32,
    pub bytes: u32,
    pub error: [u8; 48],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HdaOutput {
    pub codec: u8,
    pub pin: u8,
    pub dac: u8,
    pub kind: u8,
    pub location: u8,
    pub color: u8,
    pub fixed: u8,
    pub plugged: u8,
}

pub const HDA_SPEAKER: u8 = 1;
pub const HDA_HEADPHONES: u8 = 2;
pub const HDA_SPDIF: u8 = 3;
pub const HDA_HDMI: u8 = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HdaInfo {
    pub codec_ids: [u32; 4],
    pub codec_count: u8,
    pub in_streams: u8,
    pub out_streams: u8,
    pub output_count: u8,
    pub outputs: [HdaOutput; 8],
}

extern "C" {
    pub fn aero_ps2kbd_init(ops: *const DhiOps) -> i32;
    pub fn aero_ps2kbd_on_irq(out: *mut KeyEvent) -> i32;
    pub fn aero_nvme_init(ops: *const DhiOps, bar0_phys: u64, irq_source: u32, out: *mut BlockInfo) -> i32;
    pub fn aero_nvme_read(ctrl: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_nvme_write(ctrl: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_nvme_flush(ctrl: i32) -> i32;
    pub fn aero_ahci_init(ops: *const DhiOps, abar_phys: u64, out: *mut BlockInfo, max: i32, first_id: *mut i32) -> i32;
    pub fn aero_ahci_read(disk: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_ahci_write(disk: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_ahci_flush(disk: i32) -> i32;
    pub fn aero_xhci_init(ops: *const DhiOps, mmio_phys: u64, devices: *mut i32) -> i32;
    pub fn aero_xhci_poll(ctrl: i32, out: *mut InputEvent, max: i32) -> i32;
    pub fn aero_xhci_enable_irq(ctrl: i32);
    pub fn aero_xhci_device(ctrl: i32, index: i32, out: *mut UsbDevice) -> i32;
    pub fn aero_xhci_disk(ctrl: i32, index: i32, out: *mut BlockInfo) -> i32;
    pub fn aero_xhci_read(disk: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_xhci_write(disk: i32, lba: u64, count: u32, buf_phys: u64) -> i32;
    pub fn aero_xhci_flush(disk: i32) -> i32;
    pub fn aero_xhci_bt(ctrl: i32, index: i32, out: *mut UsbDevice) -> i32;
    pub fn aero_xhci_pad(ctrl: i32, index: i32, out: *mut UsbDevice, desc: *mut u8, max: u32) -> i32;
    pub fn aero_xhci_generation(ctrl: i32) -> u32;
    pub fn aero_xhci_pad_report(ctrl: i32, pad: *mut i32, data: *mut u8, max: u32) -> i32;
    pub fn aero_xhci_bt_send(bt: i32, kind: u8, data: *const u8, len: u32) -> i32;
    pub fn aero_xhci_bt_recv(bt: i32, kind: *mut u8, data: *mut u8, max: u32) -> i32;
    pub fn aero_xhci_bt_sco(bt: i32, alt: u8) -> i32;
    pub fn aero_btmtk_is_mediatek(vendor: u16, product: u16) -> i32;
    pub fn aero_btmtk_chip(ops: *const DhiOps, bt: i32, out: *mut BtmtkChip) -> i32;
    pub fn aero_btmtk_setup(ops: *const DhiOps, bt: i32, firmware: *const u8, size: u32, out: *mut BtmtkResult) -> i32;
    pub fn aero_hda_init(ops: *const DhiOps, bar0: u64, out: *mut HdaInfo) -> i32;
    pub fn aero_hda_start(ctrl: i32, output: i32) -> i32;
    pub fn aero_hda_write(ctrl: i32, frames: *const i16, count: u32) -> i32;
    pub fn aero_hda_pending(ctrl: i32) -> i32;
    pub fn aero_hda_stop(ctrl: i32);
    pub fn aero_e1000_supports(device_id: u16) -> i32;
    pub fn aero_e1000_init(ops: *const DhiOps, mmio_phys: u64, info: *mut NetInfo) -> i32;
    pub fn aero_e1000_send(nic: i32, frame: *const u8, len: u32) -> i32;
    pub fn aero_e1000_recv(nic: i32, frame: *mut u8, max: u32) -> i32;
    pub fn aero_e1000_link(nic: i32, speed_mbps: *mut u32) -> i32;
    pub fn aero_igc_supports(device_id: u16) -> i32;
    pub fn aero_igc_init(ops: *const DhiOps, mmio_phys: u64, device_id: u16, info: *mut NetInfo) -> i32;
    pub fn aero_igc_send(nic: i32, frame: *const u8, len: u32) -> i32;
    pub fn aero_igc_recv(nic: i32, frame: *mut u8, max: u32) -> i32;
    pub fn aero_igc_link(nic: i32, speed_mbps: *mut u32) -> i32;
}

extern "C" fn dhi_log(msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { CStr::from_ptr(msg) }.to_str().unwrap_or("<invalid utf-8>");
    crate::console::print_colored(crate::console::DIM, format_args!("       [driver] {}\n", s));
}

extern "C" fn dhi_in8(port: u16) -> u8 {
    unsafe { arch::inb(port) }
}

extern "C" fn dhi_out8(port: u16, v: u8) {
    unsafe { arch::outb(port, v) }
}

/// Physically contiguous, zeroed, page-aligned memory from the buddy allocator.
extern "C" fn dhi_dma_alloc(size: u64, out: *mut DmaBuf) -> i32 {
    let order = memory::order_for(size);
    let Some(phys) = memory::BUDDY.lock().alloc(order) else { return -1 };
    let bytes = memory::PAGE_SIZE << order;
    let virt = memory::phys_to_virt(phys) as *mut u8;
    unsafe {
        core::ptr::write_bytes(virt, 0, bytes as usize);
        out.write(DmaBuf { phys, virt, size: bytes });
    }
    0
}

extern "C" fn dhi_dma_free(buf: *const DmaBuf) {
    let b = unsafe { &*buf };
    memory::BUDDY.lock().free(b.phys, memory::order_for(b.size));
}

extern "C" fn dhi_map_mmio(phys: u64, size: u64) -> *mut u8 {
    memory::map_mmio(phys, size) as *mut u8
}

extern "C" fn dhi_delay_us(us: u32) {
    apic::delay_us(us as u64);
}

/// Interrupt sources drivers wait on through `irq_wait`; each one belongs to
/// a device whose MSI-X/MSI handler signals it.
pub const IRQ_SOURCES: usize = 8;
static IRQ_EVENTS: [sched::Event; IRQ_SOURCES] = [const { sched::Event::new() }; IRQ_SOURCES];
static IRQ_NEXT: AtomicUsize = AtomicUsize::new(0);
const IRQ_HANDLERS: [fn(); IRQ_SOURCES] = [
    || IRQ_EVENTS[0].signal(), || IRQ_EVENTS[1].signal(), || IRQ_EVENTS[2].signal(), || IRQ_EVENTS[3].signal(),
    || IRQ_EVENTS[4].signal(), || IRQ_EVENTS[5].signal(), || IRQ_EVENTS[6].signal(), || IRQ_EVENTS[7].signal(),
];

/// `irq_source` value for a driver that polls.
pub const NO_IRQ: u32 = u32::MAX;

/// Reserves an interrupt source: its number for the driver and the handler
/// to give `msi::attach`.
pub fn irq_source() -> Option<(u32, fn())> {
    let n = IRQ_NEXT.fetch_add(1, Ordering::SeqCst);
    (n < IRQ_SOURCES).then(|| (n as u32, IRQ_HANDLERS[n]))
}

extern "C" fn dhi_irq_wait(source: u32) {
    match IRQ_EVENTS.get(source as usize) {
        Some(e) if sched::can_block() => {
            e.wait(1);
        }
        _ => apic::delay_us(1),
    }
}

/// Lives for the whole kernel lifetime, as the DHI contract requires.
pub static OPS: DhiOps = DhiOps {
    abi_version: ABI_VERSION,
    _reserved: 0,
    log: dhi_log,
    port_in8: dhi_in8,
    port_out8: dhi_out8,
    dma_alloc: dhi_dma_alloc,
    dma_free: dhi_dma_free,
    map_mmio: dhi_map_mmio,
    delay_us: dhi_delay_us,
    irq_wait: dhi_irq_wait,
};
