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

use crate::{apic, arch};
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
    /// AMD display engine taking page flips (on the firmware framebuffer).
    radeon: Option<Radeon>,
}

/// Two screen buffers in graphics memory for tear-free program frames:
/// buffer 0 is the firmware's framebuffer (where the console is shown),
/// buffer 1 sits after it. A frame is drawn into the buffer not on screen
/// and the display engine switches to it at the next vertical blank.
struct Radeon {
    /// CPU pointer and display-engine address of each buffer.
    bufs: [(*mut u32, u64); 2],
    /// The buffer on screen. Always 0 while the console has the screen.
    shown: usize,
    hz: u32,
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
    /// Refresh rate when programs' frames are flipped at vertical blank.
    pub flip_hz: Option<u32>,
}

impl Display {
    fn info(&self) -> Info {
        let kind = match self.front {
            Front::Gop { .. } if self.radeon.is_some() => "firmware framebuffer, with Radeon page flips for programs",
            Front::Gop { .. } => "firmware framebuffer",
            Front::Virtio { .. } => "virtio-gpu",
        };
        Info { width: self.width, height: self.height, kind, owner: self.owner, flip_hz: self.radeon.as_ref().map(|r| r.hz) }
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
        *DISPLAY.lock() = Some(Display { front: Front::Gop { base, stride }, width: w, height: h, console, owner: None, radeon: None });
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
        *DISPLAY.lock() = Some(Display { front: Front::Virtio { program: None }, width, height, console, owner: None, radeon: None });
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
        crate::input::set_owner(Some(pid));
        Ok((d.width, d.height))
    })
}

/// Copies rectangle (x, y, w, h) of the caller's image at `src` (rows of
/// `stride` pixels, the image's top-left at `src`) to the screen. With
/// Radeon page flips, the frame goes into the buffer not on screen and this
/// returns once the display engine has switched to it (at a vertical blank).
pub fn present(pid: u64, src: u64, stride: usize, rect: (usize, usize, usize, usize)) -> Result<(), Error> {
    let flipped = arch::without_interrupts(|| {
        let mut guard = DISPLAY.lock();
        let d = guard.as_mut().ok_or(Error::NoDisplay)?;
        if d.owner != Some(pid) {
            return Err(Error::NotOwner);
        }
        let Some(rect) = clip(d, rect) else { return Ok(false) };
        let (dst, dst_stride) = match (&d.front, &d.radeon) {
            (Front::Gop { stride, .. }, Some(r)) => (r.bufs[1 - r.shown].0, *stride),
            (Front::Gop { base, stride }, None) => (*base, *stride),
            (Front::Virtio { program }, _) => (program.as_ref().ok_or(Error::NoDisplay)?.ptr() as *mut u32, d.width),
        };
        copy_rect(src, stride, dst, dst_stride, rect)?;
        let (x, y, w, h) = rect;
        if let Front::Virtio { .. } = d.front {
            unsafe { dhi::aero_vgpu_flush(PROGRAM_RESOURCE, x as u32, y as u32, w as u32, h as u32, d.width as u32 * 4) };
        }
        if let Some(r) = &d.radeon {
            unsafe { dhi::aero_dcn_flip(r.bufs[1 - r.shown].1) };
            return Ok(true);
        }
        Ok(false)
    })?;
    if !flipped {
        return Ok(());
    }
    // Sleep until the switch, then bring the other buffer up to date so
    // the next frame can be drawn into it (partial frames need it whole).
    wait_flip(true);
    arch::without_interrupts(|| {
        let mut guard = DISPLAY.lock();
        let d = guard.as_mut().ok_or(Error::NoDisplay)?;
        let Some(rect) = clip(d, rect) else { return Ok(()) };
        let Front::Gop { stride: dst_stride, .. } = d.front else { return Ok(()) };
        let Some(r) = d.radeon.as_mut() else { return Ok(()) };
        r.shown = 1 - r.shown;
        copy_rect(src, stride, r.bufs[1 - r.shown].0, dst_stride, rect)
    })
}

/// `rect` clipped to the screen, or None if nothing is left.
fn clip(d: &Display, (x, y, w, h): (usize, usize, usize, usize)) -> Option<(usize, usize, usize, usize)> {
    let x1 = x.saturating_add(w).min(d.width);
    let y1 = y.saturating_add(h).min(d.height);
    (x < x1 && y < y1).then(|| (x, y, x1 - x, y1 - y))
}

/// Copies a rectangle of a program's image into a screen buffer.
fn copy_rect(src: u64, stride: usize, dst: *mut u32, dst_stride: usize, (x, y, w, h): (usize, usize, usize, usize)) -> Result<(), Error> {
    if stride < x + w {
        return Err(Error::Fault);
    }
    // Check the whole source once; rows are then copied straight in.
    let first = src.checked_add(((y * stride + x) * 4) as u64).ok_or(Error::Fault)?;
    let span = (((h - 1) * stride + w) * 4) as u64;
    if !security::check_user(first, span, false) {
        return Err(Error::Fault);
    }
    for row in 0..h {
        let from = first + (row * stride * 4) as u64;
        let to = unsafe { dst.add((y + row) * dst_stride + x) };
        unsafe { security::copy_from_user_unchecked(from, to as *mut u8, w * 4) };
    }
    Ok(())
}

/// Waits for a queued Radeon flip to happen; returns how long it took in
/// microseconds, or None after 100 ms. `sleep` lets other threads run.
fn wait_flip(sleep: bool) -> Option<u64> {
    let t0 = apic::micros();
    while unsafe { dhi::aero_dcn_flip_pending() } != 0 {
        if apic::micros() - t0 > 100_000 {
            return None;
        }
        if sleep {
            crate::sched::sleep_us(250);
        } else {
            apic::delay_us(20);
        }
    }
    Some(apic::micros() - t0)
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
        crate::input::set_owner(None);
        if let Some(r) = d.radeon.as_mut() {
            // The console lives in buffer 0.
            if r.shown != 0 {
                unsafe { dhi::aero_dcn_flip(r.bufs[0].1) };
                wait_flip(false);
                r.shown = 0;
            }
        }
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
            crate::input::force_release();
            if let Some(r) = d.radeon.as_mut() {
                if r.shown != 0 {
                    dhi::aero_dcn_flip(r.bufs[0].1);
                    r.shown = 0;
                }
            }
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

/// After PCI, on the firmware framebuffer: looks for an AMD display engine
/// (DCN 2.1: Renoir, Lucienne, Cezanne), finds the pipe the firmware shows
/// its screen on, and tests page flips between the firmware's buffer and a
/// second one in graphics memory. Only if every flip lands at a vertical
/// blank are programs' frames flipped from then on. Prints what it found
/// either way, since on real hardware a photo is how it gets read.
pub fn probe_radeon() {
    // DCN 2.1 parts only: other generations have other register maps.
    let Some(dev) = pci::devices().iter().find(|d| d.vendor == 0x1002 && d.class == 0x03 && matches!(d.device, 0x1636 | 0x1638 | 0x164C)) else {
        return;
    };
    let gop = arch::without_interrupts(|| {
        DISPLAY.lock().as_ref().and_then(|d| match d.front {
            Front::Gop { base, stride } => Some((base, stride, d.width, d.height)),
            Front::Virtio { .. } => None,
        })
    });
    let Some((base, stride, width, height)) = gop else { return };
    let Some(mmio) = dev.bar(5) else {
        crate::kprintln!("[WARN] Radeon: no register BAR");
        return;
    };
    let mut info = dhi::DcnInfo::default();
    let rc = unsafe { dhi::aero_dcn_init(&dhi::OPS, mmio, &mut info) };
    let dim = |args: core::fmt::Arguments| crate::console::print_colored(crate::console::DIM, args);
    // Only pipes the firmware has turned on (or every pipe if none is).
    let busy = |p: &dhi::DcnPipe| p.address != 0 || p.otg_control & 1 != 0;
    let any = info.pipes.iter().any(busy);
    for (i, p) in info.pipes.iter().enumerate().filter(|(_, p)| !any || busy(p)) {
        dim(format_args!("       dcn pipe {}: hubp {:#x} surface {:#x} inuse {:#x} pitch {} fmt {} | otg {:#x} frame {} total {}x{}\n",
            i, p.hubp_cntl, p.address, p.inuse, p.pitch, p.surface_config & 0x7F, p.otg_control, p.frame_count,
            (p.h_total & 0x7FFF) + 1, (p.v_total & 0x7FFF) + 1));
    }
    if rc != 0 {
        crate::kprintln!("[WARN] Radeon: no active display pipe found (error {}), {} MiB graphics memory", rc, info.memsize_mb);
        return;
    }
    let pipe = info.pipes[info.pipe as usize];
    // Refresh rate from the frame counter over 200 ms.
    let f0 = unsafe { dhi::aero_dcn_frame_count() };
    apic::delay_us(200_000);
    let frames = unsafe { dhi::aero_dcn_frame_count() }.wrapping_sub(f0) & 0xFF_FFFF;
    let hz = frames * 5;
    crate::kok!("Radeon display engine (DCN 2.1): pipe {} shows the firmware screen at {:#x}, timing generator {} at about {} Hz, {} MiB graphics memory",
        info.pipe, pipe.address, info.otg, hz, info.memsize_mb);
    if pipe.pitch as usize != stride || hz == 0 {
        crate::kprintln!("[WARN] Radeon: pipe pitch {} does not match the screen ({} pixels) or no frames counted; page flips off", pipe.pitch, stride);
        return;
    }

    // Buffer 1 starts 2 MiB-aligned after the firmware's buffer, in the same
    // graphics memory. Before using it, check that the display engine's
    // address of the firmware's buffer really is where the CPU writes it:
    // either through the system address of graphics memory (the engine's
    // aperture registers) or through the graphics memory BAR (BAR 0).
    let bytes = (stride * height * 4) as u64;
    let offset = (bytes + (2 << 20) - 1) & !((2 << 20) - 1);
    let p0 = crate::memory::virt_to_phys_direct(base as u64);
    let a0 = pipe.address;
    dim(format_args!("       aperture: {:#x}..{:#x} at system address {:#x}; screen CPU address {:#x}, BAR0 {:#x}\n",
        info.fb_base, info.fb_top, info.fb_offset, p0, dev.bar(0).unwrap_or(0)));
    let in_fb = |a: u64| info.fb_top > info.fb_base.max(0xFF_FFFF) && a >= info.fb_base && a <= info.fb_top;
    let via_system = in_fb(a0) && info.fb_offset + (a0 - info.fb_base) == p0;
    let via_bar = in_fb(a0) && dev.bar(0).is_some_and(|b| p0 >= b && p0 - b == a0 - info.fb_base);
    if !(via_system || via_bar) {
        crate::kprintln!("[WARN] Radeon: cannot match the screen's display address {:#x} to its CPU address {:#x}; page flips off", a0, p0);
        return;
    }
    if !in_fb(a0 + offset + bytes - 1) {
        crate::kprintln!("[WARN] Radeon: no room for a second screen buffer in graphics memory; page flips off");
        return;
    }
    let back = crate::memory::map_wc(p0 + offset, bytes) as *mut u32;
    let bufs = [(base, pipe.address), (back, pipe.address + offset)];

    // Buffer 1 gets the console image, so the test flips are invisible.
    let copied = arch::without_interrupts(|| {
        let guard = DISPLAY.lock();
        let d = guard.as_ref()?;
        let src = d.console.ptr() as *const u32;
        for row in 0..height {
            unsafe { core::ptr::copy_nonoverlapping(src.add(row * width), back.add(row * stride), width) };
        }
        Some(())
    });
    if copied.is_none() {
        return;
    }

    // Flip back and forth: each flip must wait for a vertical blank (not
    // land at once) and then show the buffer asked for.
    const FLIPS: u32 = 8;
    let frame_us = 1_000_000 / hz as u64;
    let (mut total_us, mut waited, mut ok) = (0u64, 0u32, 0u32);
    // Printed afterwards: the console is not on screen during the test.
    let mut log = [(0u64, None::<u64>, 0u64); FLIPS as usize];
    for i in 0..FLIPS {
        let target = bufs[((i + 1) % 2) as usize].1;
        // Start in the middle of a frame, so a vblank flip has to wait about
        // half a frame. (The frame counter ticks at the start of the vertical
        // blank, and a flip queued right then still lands in that blank.)
        let f = unsafe { dhi::aero_dcn_frame_count() };
        let t0 = apic::micros();
        while unsafe { dhi::aero_dcn_frame_count() } == f && apic::micros() - t0 < 100_000 {}
        let t1 = apic::micros();
        while apic::micros() - t1 < frame_us / 2 {}
        unsafe { dhi::aero_dcn_flip(target) };
        let waited_us = wait_flip(false);
        let now = unsafe { dhi::aero_dcn_scanout() };
        log[i as usize] = (target, waited_us, now);
        let Some(us) = waited_us else { continue };
        total_us += us;
        if us > frame_us / 4 {
            waited += 1;
        }
        if now == target && us <= 2 * frame_us + 2_000 {
            ok += 1;
        }
    }
    // FLIPS is even, so buffer 0 (the console) is on screen again; make sure.
    unsafe { dhi::aero_dcn_flip(bufs[0].1) };
    wait_flip(false);
    let passed = ok == FLIPS && waited >= FLIPS / 2;
    // The details only when something went wrong, to keep the screen short.
    for (i, (target, us, now)) in log.iter().enumerate().filter(|_| !passed) {
        match us {
            Some(us) => dim(format_args!("       flip {} to {:#x}: {} us, now reading {:#x}\n", i, target, us, now)),
            None => dim(format_args!("       flip {} to {:#x}: not done within 100 ms, reading {:#x}\n", i, target, now)),
        }
    }
    if passed {
        crate::kok!("Radeon page flips: {} of {} at vertical blank, {} us average wait ({} Hz); tear-free frames for programs on",
            ok, FLIPS, total_us / FLIPS as u64, hz);
        arch::without_interrupts(|| {
            if let Some(d) = DISPLAY.lock().as_mut() {
                d.radeon = Some(Radeon { bufs, shown: 0, hz });
            }
        });
    } else {
        crate::kprintln!("[WARN] Radeon page flips: {} of {} correct, {} waited for a vertical blank ({} Hz); page flips off", ok, FLIPS, waited, hz);
    }
}
