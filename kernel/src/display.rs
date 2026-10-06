//! The display layer: one screen, shown either through the firmware's
//! framebuffer (GOP, set up by the bootloader) or through a GPU driver
//! (virtio-gpu in QEMU for now; the Radeon later).
//!
//! The console never draws on the screen itself. It draws into an
//! off-screen image in RAM and the display copies what changed to the
//! screen ("presents" it). That keeps reads (console scrolling) off slow
//! video memory, and it lets a GPU that has no CPU-visible framebuffer show
//! the console at all.
//!
//! A program can take the whole screen (`acquire`), present its own frames
//! and give it back (`release`, also done when the program exits). While a
//! program owns the screen the console keeps drawing off-screen and is
//! shown again, complete, when the program lets go.

use spin::Mutex;

use crate::arch;
use crate::console::CONSOLE;
use crate::dhi;
use crate::fb::Fb;
use crate::pagebuf::PageBuffer;
use crate::pci;
use crate::security;

/// virtio-gpu resource ids.
const CONSOLE_RESOURCE: u32 = 1;
const PROGRAM_RESOURCE: u32 = 2;

enum Front {
    /// The bootloader's linear framebuffer.
    Gop { base: *mut u32, stride: usize },
    /// virtio-gpu: the device copies from resources backed by our pages.
    Virtio { program: Option<PageBuffer> },
}

struct Display {
    front: Front,
    width: usize,
    height: usize,
    /// The off-screen console image. The console's Fb points into it.
    console: PageBuffer,
    /// Process that owns the screen, if any.
    owner: Option<u64>,
}

unsafe impl Send for Display {}

static DISPLAY: Mutex<Option<Display>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NoDisplay,
    Busy,
    NotOwner,
    Fault,
    NoMemory,
}

pub struct Info {
    pub width: usize,
    pub height: usize,
    pub kind: &'static str,
    /// Process that has the whole screen, if any.
    pub owner: Option<u64>,
}

impl Display {
    fn info(&self) -> Info {
        let kind = match self.front {
            Front::Gop { .. } => "firmware framebuffer",
            Front::Virtio { .. } => "virtio-gpu",
        };
        Info { width: self.width, height: self.height, kind, owner: self.owner }
    }

    /// Copies a rectangle of the console image to the screen.
    fn show_console(&mut self, (x, y, w, h): (usize, usize, usize, usize)) {
        match &self.front {
            Front::Gop { base, stride } => {
                let src = self.console.ptr() as *const u32;
                for row in y..y + h {
                    unsafe {
                        core::ptr::copy_nonoverlapping(src.add(row * self.width + x), base.add(row * stride + x), w);
                    }
                }
            }
            Front::Virtio { .. } => unsafe {
                dhi::aero_vgpu_flush(CONSOLE_RESOURCE, x as u32, y as u32, w as u32, h as u32, self.width as u32 * 4);
            },
        }
    }
}

/// Called by the console after drawing: shows what changed, unless a
/// program owns the screen (then the change waits for `release`).
pub fn present_console(fb: &mut Fb) {
    // try_lock: a panic can print while this CPU is inside the display.
    let Some(mut guard) = DISPLAY.try_lock() else { return };
    match guard.as_mut() {
        // No display layer yet: the console draws on the screen directly.
        None => {
            fb.take_damage();
        }
        Some(d) if d.owner.is_none() => {
            if let Some(rect) = fb.take_damage() {
                d.show_console(rect);
            }
        }
        Some(_) => {}
    }
}

/// After memory is up: moves the console off the bootloader's framebuffer
/// into an off-screen copy, and presents from there. Returns the size.
pub fn adopt_gop() -> Option<(usize, usize)> {
    arch::without_interrupts(|| {
        let mut con = CONSOLE.lock();
        let front = con.image()?;
        let (w, h, stride, base) = (front.width, front.height, front.stride(), front.base());
        let console = PageBuffer::new(w * h * 4)?;
        let dst = console.ptr() as *mut u32;
        for row in 0..h {
            unsafe { core::ptr::copy_nonoverlapping(base.add(row * stride), dst.add(row * w), w) };
        }
        let image = unsafe { Fb::new(console.ptr(), w, h, w * 4) };
        *DISPLAY.lock() = Some(Display { front: Front::Gop { base, stride }, width: w, height: h, console, owner: None });
        con.retarget(image);
        Some((w, h))
    })
}

/// After PCI: when the firmware gave us no screen, looks for a virtio-gpu,
/// sets it up and puts the console on it. Returns its size.
pub fn probe_virtio() -> Result<Option<(usize, usize)>, &'static str> {
    if DISPLAY.lock().is_some() {
        return Ok(None);
    }
    let Some(dev) = pci::devices().iter().find(|d| d.vendor == 0x1AF4 && d.device == 0x1050) else {
        return Ok(None);
    };
    let regions = dev.virtio_regions().ok_or("no virtio 1.x register regions")?;
    dev.enable_mmio_and_dma();
    let (mut w, mut h) = (0u32, 0u32);
    let rc = unsafe { dhi::aero_vgpu_init(&dhi::OPS, &regions, &mut w, &mut h) };
    if rc != 0 {
        return Err("device did not start");
    }
    let (width, height) = (w as usize, h as usize);
    let console = PageBuffer::new(width * height * 4).ok_or("out of memory for the screen image")?;
    let rc = unsafe { dhi::aero_vgpu_create(CONSOLE_RESOURCE, w, h, console.pages.as_ptr(), console.pages.len() as u32) };
    if rc != 0 {
        return Err("could not create the screen resource");
    }
    if unsafe { dhi::aero_vgpu_scanout(CONSOLE_RESOURCE, w, h) } != 0 {
        return Err("could not show the screen resource");
    }
    let image = unsafe { Fb::new(console.ptr(), width, height, width * 4) };
    arch::without_interrupts(|| {
        *DISPLAY.lock() = Some(Display { front: Front::Virtio { program: None }, width, height, console, owner: None });
        CONSOLE.lock().attach_framebuffer(image, true);
    });
    Ok(Some((width, height)))
}

pub fn info() -> Option<Info> {
    arch::without_interrupts(|| DISPLAY.lock().as_ref().map(|d| d.info()))
}

/// Gives process `pid` the whole screen. Returns its size.
pub fn acquire(pid: u64) -> Result<(usize, usize), Error> {
    arch::without_interrupts(|| {
        let mut guard = DISPLAY.lock();
        let d = guard.as_mut().ok_or(Error::NoDisplay)?;
        match d.owner {
            Some(p) if p == pid => return Ok((d.width, d.height)),
            Some(_) => return Err(Error::Busy),
            None => {}
        }
        if let Front::Virtio { program } = &mut d.front {
            // The program's frames go into their own resource, so the
            // console image stays intact underneath.
            let buf = PageBuffer::new(d.width * d.height * 4).ok_or(Error::NoMemory)?;
            let (w, h) = (d.width as u32, d.height as u32);
            unsafe {
                if dhi::aero_vgpu_create(PROGRAM_RESOURCE, w, h, buf.pages.as_ptr(), buf.pages.len() as u32) != 0 {
                    return Err(Error::NoMemory);
                }
                dhi::aero_vgpu_scanout(PROGRAM_RESOURCE, w, h);
            }
            *program = Some(buf);
        }
        d.owner = Some(pid);
        Ok((d.width, d.height))
    })
}

/// Copies rectangle (x, y, w, h) of the caller's image at `src` (rows of
/// `stride` pixels, the image's top-left at `src`) to the screen.
pub fn present(pid: u64, src: u64, stride: usize, (x, y, w, h): (usize, usize, usize, usize)) -> Result<(), Error> {
    arch::without_interrupts(|| {
        let mut guard = DISPLAY.lock();
        let d = guard.as_mut().ok_or(Error::NoDisplay)?;
        if d.owner != Some(pid) {
            return Err(Error::NotOwner);
        }
        let x1 = x.saturating_add(w).min(d.width);
        let y1 = y.saturating_add(h).min(d.height);
        if x >= x1 || y >= y1 {
            return Ok(());
        }
        let (w, h) = (x1 - x, y1 - y);
        if stride < x1 {
            return Err(Error::Fault);
        }
        // Check the whole source once; rows are then copied straight in.
        let first = src.checked_add(((y * stride + x) * 4) as u64).ok_or(Error::Fault)?;
        let span = (((h - 1) * stride + w) * 4) as u64;
        if !security::check_user(first, span, false) {
            return Err(Error::Fault);
        }
        let (dst, dst_stride) = match &d.front {
            Front::Gop { base, stride } => (*base, *stride),
            Front::Virtio { program } => (program.as_ref().ok_or(Error::NoDisplay)?.ptr() as *mut u32, d.width),
        };
        for row in 0..h {
            let from = first + (row * stride * 4) as u64;
            let to = unsafe { dst.add((y + row) * dst_stride + x) };
            unsafe { security::copy_from_user_unchecked(from, to as *mut u8, w * 4) };
        }
        if let Front::Virtio { .. } = d.front {
            unsafe { dhi::aero_vgpu_flush(PROGRAM_RESOURCE, x as u32, y as u32, w as u32, h as u32, d.width as u32 * 4) };
        }
        Ok(())
    })
}

/// Gives the screen back to the console, if `pid` has it.
pub fn release(pid: u64) -> bool {
    let released = arch::without_interrupts(|| {
        let mut guard = DISPLAY.lock();
        let Some(d) = guard.as_mut() else { return false };
        if d.owner != Some(pid) {
            return false;
        }
        d.owner = None;
        if let Front::Virtio { program } = &mut d.front {
            let (w, h) = (d.width as u32, d.height as u32);
            unsafe {
                dhi::aero_vgpu_scanout(CONSOLE_RESOURCE, w, h);
                dhi::aero_vgpu_destroy(PROGRAM_RESOURCE);
            }
            *program = None;
        }
        true
    });
    if released {
        arch::without_interrupts(|| CONSOLE.lock().redraw());
    }
    released
}

/// Panic path: takes the screen back for the console without waiting on
/// a lock the faulting code may hold.
///
/// # Safety
/// Only from the panic handler, after the console lock was forced.
pub unsafe fn force_console() {
    DISPLAY.force_unlock();
    if let Some(d) = DISPLAY.lock().as_mut() {
        if d.owner.take().is_some() {
            if let Front::Virtio { .. } = d.front {
                dhi::aero_vgpu_scanout(CONSOLE_RESOURCE, d.width as u32, d.height as u32);
            }
            d.show_console((0, 0, d.width, d.height));
        }
    }
}

/// A name for well-known graphics chips (PCI vendor:device).
fn gpu_name(vendor: u16, device: u16) -> &'static str {
    match (vendor, device) {
        (0x1002, 0x15DD) => "AMD Radeon Vega (Raven Ridge)",
        (0x1002, 0x15D8) => "AMD Radeon Vega (Picasso)",
        (0x1002, 0x1636) => "AMD Radeon Vega (Renoir)",
        (0x1002, 0x1638) => "AMD Radeon Vega (Cezanne)",
        (0x1002, 0x164C) => "AMD Radeon Vega (Lucienne)",
        (0x1002, 0x15E7) => "AMD Radeon Vega (Barcelo)",
        (0x1002, 0x1681) => "AMD Radeon 680M (Rembrandt)",
        (0x1002, 0x164E) => "AMD Radeon (Raphael)",
        (0x1002, 0x15BF) => "AMD Radeon 780M (Phoenix)",
        (0x1002, _) => "AMD Radeon",
        (0x8086, _) => "Intel graphics",
        (0x10DE, _) => "NVIDIA graphics",
        (0x1234, 0x1111) => "QEMU standard VGA",
        (0x1AF4, 0x1050) => "virtio-gpu",
        _ => "graphics device",
    }
}

/// Prints every graphics device on PCI with what identifies it, its
/// memory windows (BARs) and which one holds the screen the firmware set up.
/// This is what the Radeon driver work starts from on real hardware.
pub fn report_gpus() {
    let gop = arch::without_interrupts(|| {
        DISPLAY.lock().as_ref().and_then(|d| match d.front {
            Front::Gop { base, stride } => Some((crate::memory::virt_to_phys_direct(base as u64), d.width, d.height, stride)),
            Front::Virtio { .. } => None,
        })
    });
    for d in pci::devices().iter().filter(|d| d.class == 0x03) {
        let rev = d.read32(0x08) & 0xFF;
        let sub = d.read32(0x2C);
        crate::kok!("GPU {:02x}:{:02x}.{}: {} [{:04x}:{:04x}] rev {:02x}, subsystem {:04x}:{:04x}",
            d.bus, d.dev, d.func, gpu_name(d.vendor, d.device), d.vendor, d.device, rev, sub & 0xFFFF, sub >> 16);
        // Memory BARs (the address only: sizing a BAR means briefly
        // switching off the window the screen is shown from).
        let mut bars = alloc::vec::Vec::new();
        let mut i = 0;
        while i < 6 {
            let lo = d.read32(0x10 + i * 4);
            let wide = lo & 1 == 0 && (lo >> 1) & 3 == 2;
            if lo & 1 == 0 {
                if let Some(addr) = d.bar(i) {
                    bars.push((i, addr, wide, lo & 8 != 0));
                }
            }
            i += if wide { 2 } else { 1 };
        }
        // The screen is in the BAR with the highest start at or below it.
        let screen_bar = gop.and_then(|(fb, ..)| bars.iter().filter(|b| b.1 <= fb).max_by_key(|b| b.1).map(|b| b.0));
        for (i, addr, wide, pref) in &bars {
            crate::console::print_colored(crate::console::DIM, format_args!(
                "       BAR{} at {:#x}{}{}{}\n", i, addr,
                if *wide { ", 64-bit" } else { "" },
                if *pref { ", prefetchable" } else { "" },
                if screen_bar == Some(*i) { "  <- screen" } else { "" }));
        }
    }
    if let Some((fb, w, h, stride)) = gop {
        crate::console::print_colored(crate::console::DIM, format_args!(
            "       firmware screen: {}x{}, {} bytes per row, at {:#x}\n", w, h, stride * 4, fb));
    }
}
