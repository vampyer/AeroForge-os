//! Linear framebuffer drawing (32 bpp) and the Aero-style boot scene.
//! All artwork is drawn procedurally; no third-party images or fonts
//! beyond the public-domain 8x8 bitmap font.

use font8x8::legacy::BASIC_LEGACY;

pub struct Fb {
    base: *mut u32,
    pub width: usize,
    pub height: usize,
    stride: usize, // in pixels
    /// Bounding box of what was drawn since the last `take_damage`:
    /// (x0, y0, x1, y1), exclusive ends.
    damage: Option<(usize, usize, usize, usize)>,
}

unsafe impl Send for Fb {}

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

fn lerp(a: u32, b: u32, t: u32, max: u32) -> u32 {
    let ch = |shift: u32| {
        let ca = (a >> shift) & 0xFF;
        let cb = (b >> shift) & 0xFF;
        ((ca * (max - t) + cb * t) / max) << shift
    };
    ch(16) | ch(8) | ch(0)
}

impl Fb {
    /// # Safety
    /// `base` must point at a mapped 32 bpp framebuffer of the given geometry.
    pub unsafe fn new(base: *mut u8, width: usize, height: usize, pitch: usize) -> Self {
        Self { base: base as *mut u32, width, height, stride: pitch / 4, damage: None }
    }

    pub fn base(&self) -> *mut u32 {
        self.base
    }

    /// Row length in pixels.
    pub fn stride(&self) -> usize {
        self.stride
    }

    /// Records that a rectangle changed, for whoever shows this image.
    pub fn mark(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let (x1, y1) = ((x + w).min(self.width), (y + h).min(self.height));
        if x >= x1 || y >= y1 {
            return;
        }
        self.damage = Some(match self.damage {
            None => (x, y, x1, y1),
            Some((a, b, c, d)) => (a.min(x), b.min(y), c.max(x1), d.max(y1)),
        });
    }

    /// The changed rectangle (x, y, w, h) since the last call, if any.
    pub fn take_damage(&mut self) -> Option<(usize, usize, usize, usize)> {
        self.damage.take().map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
    }

    #[inline]
    pub fn put(&mut self, x: usize, y: usize, c: u32) {
        if x < self.width && y < self.height {
            unsafe { self.base.add(y * self.stride + x).write_volatile(c) }
            self.mark(x, y, 1, 1);
        }
    }

    #[inline]
    fn get(&self, x: usize, y: usize) -> u32 {
        unsafe { self.base.add(y * self.stride + x).read_volatile() }
    }

    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                self.put(xx, yy, c);
            }
        }
    }

    /// Alpha-blends `c` over what is already there (alpha out of 255).
    pub fn blend(&mut self, x: usize, y: usize, w: usize, h: usize, c: u32, alpha: u32) {
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let under = self.get(xx, yy);
                self.put(xx, yy, lerp(under, c, alpha, 255));
            }
        }
    }

    pub fn vgradient(&mut self, x: usize, y: usize, w: usize, h: usize, top: u32, bottom: u32) {
        for row in 0..h {
            let c = lerp(top, bottom, row as u32, h.max(1) as u32);
            self.fill(x, y + row, w, 1, c);
        }
    }

    /// Draws one 8x8 glyph scaled by (sx, sy). Transparent background.
    pub fn glyph(&mut self, x: usize, y: usize, ch: u8, fg: u32, sx: usize, sy: usize) {
        let idx = if (ch as usize) < 128 { ch as usize } else { b'?' as usize };
        let rows = BASIC_LEGACY[idx];
        for (r, bits) in rows.iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) != 0 {
                    for dy in 0..sy {
                        for dx in 0..sx {
                            self.put(x + col * sx + dx, y + r * sy + dy, fg);
                        }
                    }
                }
            }
        }
    }

    pub fn text(&mut self, x: usize, y: usize, s: &str, fg: u32, sx: usize, sy: usize) {
        for (i, b) in s.bytes().enumerate() {
            self.glyph(x + i * 8 * sx, y, b, fg, sx, sy);
        }
    }

    fn disc(&mut self, cx: isize, cy: isize, r: isize, inner: u32, outer: u32) {
        for dy in -r..=r {
            for dx in -r..=r {
                let d2 = dx * dx + dy * dy;
                if d2 <= r * r {
                    let t = (d2 * 255 / (r * r)) as u32;
                    self.put((cx + dx) as usize, (cy + dy) as usize, lerp(inner, outer, t, 255));
                }
            }
        }
    }

    /// Moves a rectangle's pixel rows up by `dy`, for console scrolling.
    pub fn scroll_up(&mut self, x: usize, y: usize, w: usize, h: usize, dy: usize) {
        for row in y..y + h - dy {
            unsafe {
                let dst = self.base.add(row * self.stride + x);
                let src = self.base.add((row + dy) * self.stride + x);
                core::ptr::copy(src, dst, w);
            }
        }
        self.mark(x, y, w, h - dy);
    }
}

/// Geometry of the console window on the boot scene.
pub struct Layout {
    pub text_x: usize,
    pub text_y: usize,
    pub text_w: usize,
    pub text_h: usize,
    pub taskbar_y: usize,
}

pub const CONSOLE_BG: u32 = rgb(0x0c, 0x16, 0x26);
const TASKBAR_H: usize = 40;

/// Paints the desktop: sky gradient, light streaks, a glass console window
/// and a taskbar with an orb. Returns where the console text goes.
pub fn draw_scene(fb: &mut Fb) -> Layout {
    let (w, h) = (fb.width, fb.height);

    fb.vgradient(0, 0, w, h, rgb(0x0a, 0x3a, 0x7a), rgb(0x1b, 0x8c, 0xc4));
    // Soft diagonal light streaks, the signature Aero backdrop feel.
    for (offset, width, alpha) in [(0isize, 90isize, 28u32), (260, 50, 22), (520, 140, 18)] {
        for y in 0..h as isize {
            let x0 = offset + y / 2 - 200;
            for x in x0.max(0)..(x0 + width).min(w as isize) {
                let under = fb.get(x as usize, y as usize);
                fb.put(x as usize, y as usize, lerp(under, 0xFFFFFF, alpha, 255));
            }
        }
    }

    // Console window with a glass title bar.
    let margin_x = w / 14;
    let margin_y = h / 14;
    let win_x = margin_x;
    let win_y = margin_y;
    let win_w = w - 2 * margin_x;
    let win_h = h - TASKBAR_H - 2 * margin_y;
    let title_h = 30;

    fb.blend(win_x - 6, win_y - 6, win_w + 12, win_h + 12, 0xFFFFFF, 60); // glass frame
    fb.blend(win_x, win_y, win_w, title_h, 0xFFFFFF, 70);
    fb.blend(win_x, win_y, win_w, title_h / 2, 0xFFFFFF, 40); // top highlight
    fb.text(win_x + 12, win_y + 7, "AeroKernel Console", rgb(0x0b, 0x1f, 0x3a), 2, 2);

    // Caption buttons: minimise, maximise, close.
    let bx = win_x + win_w - 3 * 32 - 8;
    for i in 0..3 {
        let c = if i == 2 { rgb(0xc8, 0x3c, 0x2c) } else { rgb(0x8a, 0xb4, 0xdc) };
        fb.vgradient(bx + i * 32, win_y + 5, 28, 18, lerp(c, 0xFFFFFF, 90, 255), c);
    }

    let body_y = win_y + title_h;
    let body_h = win_h - title_h;
    fb.fill(win_x, body_y, win_w, body_h, CONSOLE_BG);

    // Taskbar with a start orb.
    let taskbar_y = h - TASKBAR_H;
    fb.blend(0, taskbar_y, w, TASKBAR_H, rgb(0x08, 0x12, 0x20), 200);
    fb.blend(0, taskbar_y, w, 1, 0xFFFFFF, 90);
    fb.disc(28, (taskbar_y + TASKBAR_H / 2) as isize, 16, rgb(0x7f, 0xd8, 0xff), rgb(0x0b, 0x5c, 0x9e));
    fb.blend(18, taskbar_y + 6, 20, 7, 0xFFFFFF, 90);
    fb.text(56, taskbar_y + 12, "AeroForge OS", 0xFFFFFF, 2, 2);

    let pad = 10;
    Layout {
        text_x: win_x + pad,
        text_y: body_y + pad,
        text_w: win_w - 2 * pad,
        text_h: body_h - 2 * pad,
        taskbar_y,
    }
}

/// Redraws the right-hand taskbar status area (uptime, CPUs, ...).
pub fn draw_tray(fb: &mut Fb, layout: &Layout, text: &str) {
    let w = text.len() * 8 + 24;
    let x = fb.width.saturating_sub(w);
    let y = layout.taskbar_y + 1;
    fb.fill(x, y, w, TASKBAR_H - 1, rgb(0x15, 0x27, 0x3c));
    fb.text(x + 12, y + 12, text, rgb(0xd8, 0xec, 0xff), 1, 2);
}
