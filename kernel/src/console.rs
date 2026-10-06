//! Kernel console: every line goes to COM1 and, once a framebuffer is up,
//! into the console window on screen.

use core::fmt::{self, Write};
use spin::Mutex;

use crate::arch;
use crate::fb::{self, Fb, Layout};
use crate::serial;

const CELL_W: usize = 8;
const CELL_H: usize = 16;

pub const WHITE: u32 = fb::rgb(0xe6, 0xee, 0xf8);
pub const DIM: u32 = fb::rgb(0x8a, 0xa0, 0xb8);
pub const GREEN: u32 = fb::rgb(0x6e, 0xe0, 0x8a);
pub const CYAN: u32 = fb::rgb(0x6c, 0xd0, 0xff);
pub const YELLOW: u32 = fb::rgb(0xff, 0xd8, 0x6a);
pub const RED: u32 = fb::rgb(0xff, 0x6b, 0x6b);

struct Screen {
    fb: Fb,
    layout: Layout,
    col: usize,
    row: usize,
    cols: usize,
    rows: usize,
}

/// The last HISTORY characters written, with their colours, so a screen
/// that appears later in boot (virtio-gpu, after PCI) can show them.
const HISTORY: usize = 8192;

pub struct Console {
    screen: Option<Screen>,
    fg: u32,
    hist: [u8; HISTORY],
    hist_fg: [u32; HISTORY],
    hist_len: usize, // total written; the ring holds the last HISTORY
}

pub static CONSOLE: Mutex<Console> = Mutex::new(Console {
    screen: None,
    fg: WHITE,
    hist: [0; HISTORY],
    hist_fg: [0; HISTORY],
    hist_len: 0,
});

impl Screen {
    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            let l = &self.layout;
            let h = self.rows * CELL_H;
            self.fb.scroll_up(l.text_x, l.text_y, l.text_w, h, CELL_H);
            self.fb.fill(l.text_x, l.text_y + h - CELL_H, l.text_w, CELL_H, fb::CONSOLE_BG);
        }
    }

    fn put(&mut self, b: u8, fg: u32) {
        match b {
            b'\n' => self.newline(),
            b'\r' => self.col = 0,
            8 => {
                // Backspace: step back and erase the cell.
                if self.col > 0 {
                    self.col -= 1;
                    let (x, y) = self.cell_xy();
                    self.fb.fill(x, y, CELL_W, CELL_H, fb::CONSOLE_BG);
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                let (x, y) = self.cell_xy();
                self.fb.fill(x, y, CELL_W, CELL_H, fb::CONSOLE_BG);
                self.fb.glyph(x, y, b, fg, 1, 2);
                self.col += 1;
            }
        }
    }

    fn cell_xy(&self) -> (usize, usize) {
        (self.layout.text_x + self.col * CELL_W, self.layout.text_y + self.row * CELL_H)
    }
}

impl Console {
    /// Draws the desktop on `fb` and puts the console there. With `replay`,
    /// the recent history is drawn again (for a screen found late in boot).
    pub fn attach_framebuffer(&mut self, mut fb: Fb, replay: bool) {
        let layout = fb::draw_scene(&mut fb);
        let cols = layout.text_w / CELL_W;
        let rows = layout.text_h / CELL_H;
        let mut screen = Screen { fb, layout, col: 0, row: 0, cols, rows };
        if replay {
            let start = self.hist_len.saturating_sub(HISTORY);
            for i in start..self.hist_len {
                screen.put(self.hist[i % HISTORY], self.hist_fg[i % HISTORY]);
            }
        }
        self.screen = Some(screen);
        self.present();
    }

    /// Moves the console to another image of the same size (an off-screen
    /// copy of the screen), returning the old one.
    pub fn retarget(&mut self, fb: Fb) -> Option<Fb> {
        let s = self.screen.as_mut()?;
        Some(core::mem::replace(&mut s.fb, fb))
    }

    /// The image the console draws into, for the display layer.
    pub fn image(&self) -> Option<&Fb> {
        self.screen.as_ref().map(|s| &s.fb)
    }

    /// Hands what changed to the display layer.
    fn present(&mut self) {
        if let Some(s) = self.screen.as_mut() {
            crate::display::present_console(&mut s.fb);
        }
    }

    /// Marks the whole screen changed and shows it (after a program gave
    /// the display back).
    pub fn redraw(&mut self) {
        if let Some(s) = self.screen.as_mut() {
            let (w, h) = (s.fb.width, s.fb.height);
            s.fb.mark(0, 0, w, h);
        }
        self.present();
    }

    pub fn set_color(&mut self, fg: u32) {
        self.fg = fg;
    }

    pub fn tray(&mut self, text: &str) {
        if let Some(s) = self.screen.as_mut() {
            fb::draw_tray(&mut s.fb, &s.layout, text);
        }
        self.present();
    }

    pub fn screen_size(&self) -> Option<(usize, usize)> {
        self.screen.as_ref().map(|s| (s.fb.width, s.fb.height))
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        serial::write_str(s);
        for b in s.bytes() {
            let i = self.hist_len % HISTORY;
            self.hist[i] = b;
            self.hist_fg[i] = self.fg;
            self.hist_len += 1;
        }
        if let Some(screen) = self.screen.as_mut() {
            for b in s.bytes() {
                screen.put(b, self.fg);
            }
        }
        self.present();
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    arch::without_interrupts(|| {
        let _ = CONSOLE.lock().write_fmt(args);
    });
}

/// Prints in a colour, then restores the default.
pub fn print_colored(fg: u32, args: fmt::Arguments) {
    arch::without_interrupts(|| {
        let mut c = CONSOLE.lock();
        c.set_color(fg);
        let _ = c.write_fmt(args);
        c.set_color(WHITE);
    });
}

/// "[  OK  ] message" boot-progress line.
pub fn ok(args: fmt::Arguments) {
    _print(format_args!("["));
    print_colored(GREEN, format_args!("  OK  "));
    _print(format_args!("] {}\n", args));
}

/// Used by the panic handler: the lock may be held by the code that faulted.
pub unsafe fn force_unlock() {
    CONSOLE.force_unlock();
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

#[macro_export]
macro_rules! kok {
    ($($arg:tt)*) => { $crate::console::ok(format_args!($($arg)*)) };
}
