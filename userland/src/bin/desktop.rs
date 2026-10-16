//! desktop: a first desktop in the style of Windows 7 (all artwork drawn
//! here, nothing copied), drawn in software.
//!
//! It takes the whole screen and draws a blue wallpaper with icons down the
//! left (Computer, Notes, System: double-click to open), a dark glass
//! taskbar (Start orb, window buttons, a clock with the date and a "show
//! desktop" strip at the right) and glass-framed windows: Welcome, System,
//! Notes, Computer and Calculator. Computer is a file manager in the style
//! of Windows Explorer: it opens on the drives (every FAT32, exFAT and NTFS
//! volume, with how full each is), has Back, Forward and Up, an address
//! whose parts can be clicked, a pane listing the drives, folders in
//! columns sorted by a click on Name, Type or Size, and New folder, Cut,
//! Copy, Paste, Rename and Delete (also Ctrl+N, Ctrl+X, Ctrl+C, Ctrl+V, F2
//! and Delete; arrows, Enter, Backspace and F5 work too). A text file
//! opens in Notes. Calculator (click its keys or type 0-9 + - * / . = Enter Backspace C).
//! The mouse moves a pointer; a window comes to the front
//! when clicked, moves when dragged by its title bar and changes size when
//! dragged by an edge or corner. Dragged against the top of the screen it
//! fills the screen, against the left or right side it fills that half
//! (a glass outline shows where first); dragging it away again gives back
//! its old size. Its caption
//! buttons minimize, maximize and close it; taskbar buttons switch between
//! windows. The Start menu lists the programs and has "Exit to console";
//! Esc also gives the screen back to the shell. Keys typed while Notes is
//! in front go into it; its Save button (or Ctrl+S) writes them back to the
//! file it opened, or to /Notes.txt.
//!
//! Only the parts of the screen that changed are redrawn and presented.
//! Lines starting "[desktop]" go to the serial log for the boot test.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use aero::display::{self, Pointer, Screen};
use aero::println;
use core::sync::atomic::{AtomicI32, Ordering};
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

aero::entry!(main);

// ------------------------------------------------------------------ drawing

#[derive(Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }
    fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
    fn intersect(&self, o: &Rect) -> Rect {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        if x1 <= x0 || y1 <= y0 { Rect::EMPTY } else { Rect::new(x0, y0, x1 - x0, y1 - y0) }
    }
    fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    (r.min(255) << 16) | (g.min(255) << 8) | b.min(255)
}

/// Mixes `top` over `under` with `alpha` out of 255.
fn blend(under: u32, top: u32, alpha: u32) -> u32 {
    let mix = |s: u32| {
        let a = (under >> s) & 0xFF;
        let b = (top >> s) & 0xFF;
        (a * (255 - alpha) + b * alpha) / 255
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

fn lerp(a: u32, b: u32, t: i32, n: i32) -> u32 {
    let t = t.clamp(0, n.max(1)) as u32;
    let n = n.max(1) as u32;
    blend(a, b, t * 255 / n)
}

/// Text styles. The glyphs are Noto Sans Mono (SIL Open Font License),
/// pre-rendered with smooth edges by the noto-sans-mono-bitmap crate.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Font {
    /// Window text, labels, menus: 24 pixels tall (32 at UI scale 2).
    Normal,
    /// Title bars.
    Bold,
    /// The taskbar clock: 20 pixels tall (24 at UI scale 2).
    Small,
    /// The clock in the Start menu: 32 pixels tall.
    Large,
    /// The preview pane's text and bytes: 16 pixels tall (20 at UI scale 2).
    Tiny,
}

/// The text size chosen in Control Panel: -1 smaller, 0 medium, 1 larger,
/// 2 extra large, 3 largest. Every style but Large steps one size with it.
static TEXT_LEVEL: AtomicI32 = AtomicI32::new(0);

fn text_level() -> i32 {
    TEXT_LEVEL.load(Ordering::Relaxed)
}

impl Font {
    /// The glyphs to draw from, and how tall to draw them: the font's own
    /// sizes go up to 32 pixels, bigger text is those glyphs scaled up.
    fn style(self, ui: i32) -> (FontWeight, RasterHeight, i32) {
        const SIZES: [i32; 6] = [16, 20, 24, 32, 40, 48];
        let weight = if self == Font::Bold { FontWeight::Bold } else { FontWeight::Regular };
        let step = match self {
            Font::Large => 3 + text_level().max(0),
            Font::Normal | Font::Bold => 2 + text_level(),
            Font::Small => 1 + text_level(),
            Font::Tiny => text_level(),
        } + if ui >= 2 { 1 } else { 0 };
        let px = SIZES[step.clamp(0, 5) as usize];
        let raster = match px {
            16 => RasterHeight::Size16,
            20 => RasterHeight::Size20,
            24 => RasterHeight::Size24,
            _ => RasterHeight::Size32,
        };
        (weight, raster, px)
    }
    /// The width of one character (the font is monospaced).
    fn w(self, ui: i32) -> i32 {
        let (weight, raster, px) = self.style(ui);
        get_raster_width(weight, raster) as i32 * px / raster.val() as i32
    }
    fn h(self, ui: i32) -> i32 {
        self.style(ui).2
    }
    fn width(self, ui: i32, text: &str) -> i32 {
        text.chars().count() as i32 * self.w(ui)
    }
}

/// Sharpens a glyph's coverage: faint edge pixels drop out and most of the
/// stroke becomes solid, so text stays clear instead of looking soft.
fn crisp(ink: u8) -> u32 {
    (ink as u32).saturating_sub(48).saturating_mul(2).min(255)
}

/// The frame being built, clipped to the area being redrawn.
/// The icons drawn by tools/make-icons.py, each 96 x 96, and copies of
/// them shrunk to the sizes asked for so far.
static ICON_FILE: &[u8] = include_bytes!("../icons.bin");

struct Icons {
    full: Vec<([u8; 16], usize, usize, Vec<u32>)>,
    sized: Vec<(usize, i32, Vec<u32>)>,
}
struct IconCell(core::cell::UnsafeCell<Option<Icons>>);
// SAFETY: only the desktop's main thread draws.
unsafe impl Sync for IconCell {}
static ICONS_CACHE: IconCell = IconCell(core::cell::UnsafeCell::new(None));

fn icons_cache() -> &'static mut Icons {
    // SAFETY: as above; nothing holds the reference across another call.
    let cell = unsafe { &mut *ICONS_CACHE.0.get() };
    cell.get_or_insert_with(|| {
        let d = ICON_FILE;
        let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]) as usize;
        let mut full = Vec::new();
        let count = if d.len() >= 8 && &d[..4] == b"AFIC" { u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize } else { 0 };
        let mut at = 8;
        for _ in 0..count {
            let mut name = [0u8; 16];
            name.copy_from_slice(&d[at..at + 16]);
            let (w, h) = (u16_at(at + 16), u16_at(at + 18));
            at += 20;
            let mut px = Vec::with_capacity(w * h);
            while px.len() < w * h {
                let (clear, solid) = (u16_at(at), u16_at(at + 2));
                at += 4;
                px.resize(px.len() + clear, 0);
                for _ in 0..solid {
                    px.push(u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]]));
                    at += 4;
                }
            }
            full.push((name, w, h, px));
        }
        Icons { full, sized: Vec::new() }
    })
}

/// Icon `name` at `size` x `size` (ARGB), shrunk by averaging the pixels
/// each one covers, weighted by how solid they are.
fn icon_pixels(name: &str, size: i32) -> Option<&'static [u32]> {
    let icons = icons_cache();
    let which = icons.full.iter().position(|(n, ..)| n.split(|&b| b == 0).next() == Some(name.as_bytes()))?;
    if let Some(i) = icons.sized.iter().position(|(w, z, _)| *w == which && *z == size) {
        return Some(&icons.sized[i].2);
    }
    let (_, w, h, src) = &icons.full[which];
    let n = size.max(1) as usize;
    let mut out = alloc::vec![0u32; n * n];
    for dy in 0..n {
        let (y0, y1) = (dy * h / n, ((dy + 1) * h).div_ceil(n).max(dy * h / n + 1));
        for dx in 0..n {
            let (x0, x1) = (dx * w / n, ((dx + 1) * w).div_ceil(n).max(dx * w / n + 1));
            let (mut a, mut r, mut g, mut b, mut cnt) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0..y1.min(*h) {
                for &p in &src[y * w + x0..y * w + x1.min(*w)] {
                    let pa = p >> 24;
                    a += pa;
                    r += (p >> 16 & 255) * pa;
                    g += (p >> 8 & 255) * pa;
                    b += (p & 255) * pa;
                    cnt += 1;
                }
            }
            if a > 0 {
                out[dy * n + dx] = ((a / cnt.max(1)) << 24) | ((r / a) << 16) | ((g / a) << 8) | (b / a);
            }
        }
    }
    icons.sized.push((which, size, out));
    Some(&icons.sized.last().unwrap().2)
}

struct Canvas {
    px: Vec<u32>,
    w: i32,
    h: i32,
    clip: Rect,
    ui: i32,
}

impl Canvas {
    /// Draws icon `name` (see tools/make-icons.py) `size` pixels square with
    /// its top left at (x, y). Returns false if there is no such icon.
    fn icon(&mut self, name: &str, x: i32, y: i32, size: i32) -> bool {
        let Some(px) = icon_pixels(name, size) else { return false };
        let c = Rect::new(x, y, size, size).intersect(&self.clip);
        for yy in c.y..c.y + c.h {
            let row = (yy * self.w) as usize;
            let src = ((yy - y) * size) as usize;
            for xx in c.x..c.x + c.w {
                let p = px[src + (xx - x) as usize];
                let a = p >> 24;
                if a == 255 {
                    self.px[row + xx as usize] = p & 0xFFFFFF;
                } else if a > 0 {
                    let under = &mut self.px[row + xx as usize];
                    *under = blend(*under, p & 0xFFFFFF, a);
                }
            }
        }
        true
    }

    fn fill(&mut self, r: Rect, color: u32) {
        let c = r.intersect(&self.clip);
        for y in c.y..c.y + c.h {
            let row = (y * self.w) as usize;
            self.px[row + c.x as usize..row + (c.x + c.w) as usize].fill(color);
        }
    }

    fn shade(&mut self, r: Rect, color: u32, alpha: u32) {
        let c = r.intersect(&self.clip);
        for y in c.y..c.y + c.h {
            let row = (y * self.w) as usize;
            for p in &mut self.px[row + c.x as usize..row + (c.x + c.w) as usize] {
                *p = blend(*p, color, alpha);
            }
        }
    }

    /// Vertical gradient from `top` to `bottom`, mixed in with `alpha`.
    fn gradient(&mut self, r: Rect, top: u32, bottom: u32, alpha: u32) {
        let c = r.intersect(&self.clip);
        for y in c.y..c.y + c.h {
            let color = lerp(top, bottom, y - r.y, r.h - 1);
            let row = (y * self.w) as usize;
            for p in &mut self.px[row + c.x as usize..row + (c.x + c.w) as usize] {
                *p = if alpha >= 255 { color } else { blend(*p, color, alpha) };
            }
        }
    }

    fn frame(&mut self, r: Rect, color: u32) {
        self.fill(Rect::new(r.x, r.y, r.w, 1), color);
        self.fill(Rect::new(r.x, r.y + r.h - 1, r.w, 1), color);
        self.fill(Rect::new(r.x, r.y, 1, r.h), color);
        self.fill(Rect::new(r.x + r.w - 1, r.y, 1, r.h), color);
    }

    /// Text with its top left at (x, y), smoothed into what is underneath.
    /// Text cut off at the edges of `area`.
    fn text_in(&mut self, area: Rect, x: i32, y: i32, s: &str, color: u32, font: Font) {
        let outer = self.clip;
        self.clip = outer.intersect(&area);
        self.text(x, y, s, color, font);
        self.clip = outer;
    }

    fn text(&mut self, x: i32, y: i32, s: &str, color: u32, font: Font) {
        let (weight, size, px) = font.style(self.ui);
        let (cw, ch) = (font.w(self.ui), font.h(self.ui));
        for (i, c) in s.chars().enumerate() {
            let gx = x + i as i32 * cw;
            let box_ = Rect::new(gx, y, cw, ch).intersect(&self.clip);
            if box_.is_empty() || c == ' ' {
                continue;
            }
            let Some(glyph) = get_raster(c, weight, size).or_else(|| get_raster('?', weight, size)) else { continue };
            if px != size.val() as i32 {
                self.glyph_scaled(glyph.raster(), gx, y, ch, &box_, color);
                continue;
            }
            for (row, line) in glyph.raster().iter().enumerate() {
                let py = y + row as i32;
                if py < box_.y || py >= box_.y + box_.h {
                    continue;
                }
                for (col, &ink) in line.iter().enumerate() {
                    let px = gx + col as i32;
                    if ink > 0 && px >= box_.x && px < box_.x + box_.w {
                        let p = &mut self.px[(py * self.w + px) as usize];
                        *p = blend(*p, color, crisp(ink));
                    }
                }
            }
        }
    }

    /// A filled circle, shaded from `inner` at the top to `outer` at the bottom.
    fn orb(&mut self, cx: i32, cy: i32, radius: i32, inner: u32, outer: u32) {
        let c = Rect::new(cx - radius, cy - radius, 2 * radius + 1, 2 * radius + 1).intersect(&self.clip);
        for y in c.y..c.y + c.h {
            for x in c.x..c.x + c.w {
                let (dx, dy) = (x - cx, y - cy);
                let d2 = dx * dx + dy * dy;
                if d2 <= radius * radius {
                    let base = lerp(inner, outer, dy + radius, 2 * radius);
                    // A soft highlight on the upper half, like glass.
                    let shine = if dy < 0 { ((-dy) * 120 / radius.max(1)) as u32 } else { 0 };
                    let edge = if d2 > (radius - 1) * (radius - 1) { 160 } else { 0 };
                    let mut p = blend(base, 0xFFFFFF, shine / 2);
                    p = blend(p, 0x0A2A55, edge);
                    self.px[(y * self.w + x) as usize] = p;
                }
            }
        }
    }
}

impl Canvas {
    /// Whether (x, y) is inside `r` with its corners rounded by `radius`
    /// (only the top two when `top_only`).
    fn inside_round(r: &Rect, radius: i32, top_only: bool, x: i32, y: i32) -> bool {
        let cx = if x < r.x + radius { r.x + radius } else if x >= r.x + r.w - radius { r.x + r.w - radius - 1 } else { return true };
        let cy = if y < r.y + radius {
            r.y + radius
        } else if !top_only && y >= r.y + r.h - radius {
            r.y + r.h - radius - 1
        } else {
            return true;
        };
        let (dx, dy) = (x - cx, y - cy);
        dx * dx + dy * dy <= radius * radius
    }

    /// A vertical gradient in a rounded rectangle, mixed in with `alpha`.
    fn rounded(&mut self, r: Rect, radius: i32, top_only: bool, top: u32, bottom: u32, alpha: u32) {
        let c = r.intersect(&self.clip);
        for y in c.y..c.y + c.h {
            let color = lerp(top, bottom, y - r.y, r.h - 1);
            let row = (y * self.w) as usize;
            for x in c.x..c.x + c.w {
                if Self::inside_round(&r, radius, top_only, x, y) {
                    let p = &mut self.px[row + x as usize];
                    *p = blend(*p, color, alpha);
                }
            }
        }
    }

    /// A one-pixel outline of a rounded rectangle.
    fn rounded_outline(&mut self, r: Rect, radius: i32, top_only: bool, color: u32, alpha: u32) {
        let c = r.intersect(&self.clip);
        for y in c.y..c.y + c.h {
            let row = (y * self.w) as usize;
            for x in c.x..c.x + c.w {
                let inside = Self::inside_round(&r, radius, top_only, x, y);
                let inner = Self::inside_round(&Rect::new(r.x + 1, r.y + 1, r.w - 2, r.h - 2), (radius - 1).max(0), top_only, x, y)
                    && x > r.x && y > r.y && x < r.x + r.w - 1 && y < r.y + r.h - 1;
                if inside && !inner {
                    let p = &mut self.px[row + x as usize];
                    *p = blend(*p, color, alpha);
                }
            }
        }
    }

    /// A thick line from (x0, y0) to (x1, y1) of `size` pixel squares.
    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, size: i32, color: u32) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        for i in 0..=steps {
            let x = x0 + (x1 - x0) * i / steps;
            let y = y0 + (y1 - y0) * i / steps;
            self.fill(Rect::new(x, y, size, size), color);
        }
    }

    /// Text with a soft light glow behind it, as on glass title bars.
    /// A glyph drawn `h` pixels tall from its smaller raster, smoothly
    /// (each pixel blends the four source pixels around where it falls).
    fn glyph_scaled(&mut self, raster: &[&[u8]], gx: i32, y: i32, h: i32, box_: &Rect, color: u32) {
        let (sh, sw) = (raster.len() as i32, raster.first().map_or(0, |r| r.len()) as i32);
        if sh == 0 || sw == 0 {
            return;
        }
        let ink_at = |cx: i32, cy: i32| raster[cy.clamp(0, sh - 1) as usize][cx.clamp(0, sw - 1) as usize] as i32;
        for py in box_.y..box_.y + box_.h {
            // Where this pixel's centre falls in the glyph, in 256ths.
            let fy = ((py - y) * 2 + 1) * sh * 128 / h - 128;
            let (y0, ty) = (fy.div_euclid(256), fy.rem_euclid(256));
            for px in box_.x..box_.x + box_.w {
                let fx = ((px - gx) * 2 + 1) * sh * 128 / h - 128;
                let (x0, tx) = (fx.div_euclid(256), fx.rem_euclid(256));
                let top = ink_at(x0, y0) * (256 - tx) + ink_at(x0 + 1, y0) * tx;
                let bottom = ink_at(x0, y0 + 1) * (256 - tx) + ink_at(x0 + 1, y0 + 1) * tx;
                let ink = ((top * (256 - ty) + bottom * ty) >> 16) as u8;
                if ink > 0 {
                    let p = &mut self.px[(py * self.w + px) as usize];
                    *p = blend(*p, color, crisp(ink));
                }
            }
        }
    }

    fn glow_text(&mut self, x: i32, y: i32, s: &str, color: u32, font: Font) {
        let ui = self.ui;
        let (w, h) = (font.width(ui, s), font.h(ui));
        let pad = 3 * ui;
        self.rounded(Rect::new(x - 2 * pad, y - pad / 2, w + 4 * pad, h + pad), 6 * ui, false, 0xFFFFFF, 0xFFFFFF, 110);
        self.text(x, y, s, color, font);
    }
}

/// A five-pointed star's corners around (0, 0), outer points 1000 out and
/// inner ones 382, starting straight up: (x, y) in thousandths.
const STAR: [(i32, i32); 10] = [
    (0, -1000), (225, -309), (951, -309), (363, 118), (588, 809),
    (0, 382), (-588, 809), (-363, 118), (-951, -309), (-225, -309),
];

/// Whether (x, y) is inside the polygon (even-odd rule).
fn inside_polygon(points: &[(i32, i32)], x: i32, y: i32) -> bool {
    let mut inside = false;
    let mut j = points.len() - 1;
    for i in 0..points.len() {
        let (xi, yi) = points[i];
        let (xj, yj) = points[j];
        if (yi > y) != (yj > y) && (x as i64 - xi as i64) * (yj - yi) as i64 * if yj > yi { 1 } else { -1 }
            < (xj - xi) as i64 * (y - yi) as i64 * if yj > yi { 1 } else { -1 }
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

impl Canvas {
    /// The Start button: an outer red ring, a white ring and a blue middle
    /// with a black star, shaded like glass and smoothed at the edges
    /// (four samples a pixel).
    fn start_orb(&mut self, cx: i32, cy: i32, radius: i32, hot: bool) {
        let c = Rect::new(cx - radius - 1, cy - radius - 1, 2 * radius + 3, 2 * radius + 3).intersect(&self.clip);
        // Everything in quarter pixels from the centre.
        let r4 = radius * 4;
        let star: Vec<(i32, i32)> = STAR.iter().map(|&(x, y)| (x * r4 * 52 / 100 / 1000, y * r4 * 52 / 100 / 1000 + r4 / 30)).collect();
        for y in c.y..c.y + c.h {
            for x in c.x..c.x + c.w {
                let (mut sum, mut hits) = ([0u32; 3], 0u32);
                for (sx, sy) in [(1, 1), (3, 1), (1, 3), (3, 3)] {
                    let (dx, dy) = ((x - cx) * 4 + sx - 2, (y - cy) * 4 + sy - 2);
                    let d2 = dx * dx + dy * dy;
                    if d2 > r4 * r4 {
                        continue;
                    }
                    hits += 1;
                    let base = if d2 > (r4 * 76 / 100).pow(2) {
                        rgb(205, 30, 40)
                    } else if d2 > (r4 * 58 / 100).pow(2) {
                        rgb(250, 250, 250)
                    } else if inside_polygon(&star, dx, dy) {
                        rgb(10, 10, 14)
                    } else {
                        rgb(30, 70, 170)
                    };
                    // Glass: light from above, darker towards the bottom,
                    // a thin dark rim, brighter while hot.
                    let mut p = blend(base, 0xFFFFFF, if dy < 0 { ((-dy) * 90 / r4.max(1)) as u32 } else { 0 });
                    p = blend(p, 0x000000, if dy > 0 { (dy * 60 / r4.max(1)) as u32 } else { 0 });
                    if d2 > (r4 - 5).pow(2) {
                        p = blend(p, 0x300808, 170);
                    }
                    if hot {
                        p = blend(p, 0xFFFFFF, 45);
                    }
                    for (k, v) in sum.iter_mut().enumerate() {
                        *v += (p >> (16 - 8 * k)) & 0xFF;
                    }
                }
                if hits > 0 {
                    let i = (y * self.w + x) as usize;
                    let color = rgb(sum[0] / hits, sum[1] / hits, sum[2] / hits);
                    self.px[i] = blend(self.px[i], color, hits * 255 / 4);
                }
            }
        }
        // A soft shine over the top half.
        let shine = Rect::new(cx - radius * 6 / 10, cy - radius * 9 / 10, radius * 12 / 10, radius * 7 / 10);
        self.rounded(shine, radius * 35 / 100, false, 0xFFFFFF, 0xFFFFFF, 50);
    }

    /// A small yellow folder, 16 x 12 at scale 1.
    fn folder(&mut self, x: i32, y: i32, s: i32) {
        self.icon("folder", x, y - 2 * s, 16 * s);
    }

    /// A small sheet of paper with a folded corner, 12 x 14 at scale 1.
    fn page(&mut self, x: i32, y: i32, s: i32) {
        self.icon("text", x - 2 * s, y - s, 16 * s);
    }

    /// A file's icon by what it is (see `file_kind`), 12 x 14 at scale 1
    /// like `page`; pictures, programs and compressed folders take a
    /// shape of their own in the same box.
    fn file_icon(&mut self, x: i32, y: i32, s: i32, kind: FileKind) {
        let name = match kind {
            FileKind::Text => "text",
            FileKind::Sound => "sound",
            FileKind::Picture => "picture",
            FileKind::Program | FileKind::Archive | FileKind::Disc | FileKind::Settings | FileKind::Save => "",
            _ => "page",
        };
        if !name.is_empty() && self.icon(name, x - 2 * s, y - s, 16 * s) {
            return;
        }
        match kind {
            FileKind::Picture => {
                // A framed landscape: sky, sun and a green hill.
                let r = Rect::new(x - s, y + s, 14 * s, 12 * s);
                self.fill(r, 0xFFFFFF);
                self.frame(r, rgb(120, 130, 150));
                let sky = Rect::new(r.x + s, r.y + s, r.w - 2 * s, r.h - 2 * s);
                self.gradient(sky, rgb(110, 180, 245), rgb(200, 230, 255), 255);
                self.fill(Rect::new(sky.x + 8 * s, sky.y + 2 * s, 2 * s, 2 * s), rgb(255, 210, 60));
                for i in 0..4 * s {
                    let w = (sky.w * (i + 2 * s) / (6 * s)).min(sky.w);
                    self.fill(Rect::new(sky.x, sky.y + sky.h - 4 * s + i, w, 1), rgb(70, 160, 70));
                }
            }
            FileKind::Program => {
                // A little window: a blue title bar over a grey body.
                let r = Rect::new(x - s, y + s, 14 * s, 12 * s);
                self.fill(r, rgb(235, 238, 243));
                self.frame(r, rgb(70, 85, 110));
                self.gradient(Rect::new(r.x + s, r.y + s, r.w - 2 * s, 3 * s), rgb(90, 150, 230), rgb(30, 90, 180), 255);
                self.fill(Rect::new(r.x + 3 * s, r.y + 6 * s, 8 * s, s), rgb(150, 160, 175));
                self.fill(Rect::new(r.x + 3 * s, r.y + 8 * s, 5 * s, s), rgb(150, 160, 175));
            }
            FileKind::Archive => {
                // A folder with a zipper down the middle.
                self.folder(x - 2 * s, y + s, s);
                for i in 0..5 {
                    self.fill(Rect::new(x + 5 * s + (i % 2) * s, y + (4 + 2 * i) * s, s, s), rgb(70, 60, 40));
                }
            }
            FileKind::Disc => {
                let (cx, cy) = (x + 6 * s, y + 7 * s);
                self.orb(cx, cy, 6 * s, rgb(235, 238, 245), rgb(160, 170, 190));
                self.orb(cx, cy, 2 * s, rgb(255, 255, 255), rgb(200, 205, 215));
            }
            _ => {
                self.fill(Rect::new(x, y, 12 * s, 14 * s), 0xFFFFFF);
                self.frame(Rect::new(x, y, 12 * s, 14 * s), rgb(120, 130, 150));
                self.fill(Rect::new(x + 8 * s, y, 4 * s, 4 * s), rgb(210, 220, 235));
                if kind == FileKind::Text {
                    for i in 0..3 {
                        self.fill(Rect::new(x + 2 * s, y + (6 + 2 * i) * s, 8 * s, s), rgb(150, 170, 200));
                    }
                }
                match kind {
                    FileKind::Settings => {
                        // A grey cog: a ring with four teeth.
                        let (cx, cy) = (x + 6 * s, y + 9 * s);
                        self.fill(Rect::new(cx - s / 2 - s, cy - 4 * s, 2 * s, 8 * s), rgb(110, 115, 125));
                        self.fill(Rect::new(cx - 4 * s, cy - s / 2 - s, 8 * s, 2 * s), rgb(110, 115, 125));
                        self.orb(cx, cy, 3 * s, rgb(150, 155, 165), rgb(100, 105, 115));
                        self.orb(cx, cy, s, rgb(255, 255, 255), rgb(235, 235, 235));
                    }
                    FileKind::Sound => {
                        // A note: a head and a stem with a flag.
                        self.orb(x + 5 * s, y + 11 * s, 2 * s, rgb(40, 70, 140), rgb(20, 40, 100));
                        self.fill(Rect::new(x + 6 * s, y + 4 * s, s.max(1), 7 * s), rgb(20, 40, 100));
                        self.fill(Rect::new(x + 6 * s, y + 4 * s, 3 * s, 2 * s), rgb(20, 40, 100));
                    }
                    FileKind::Save => {
                        // A blue disk with a white label.
                        let d = Rect::new(x + 2 * s, y + 5 * s, 8 * s, 8 * s);
                        self.fill(d, rgb(50, 100, 180));
                        self.fill(Rect::new(d.x + 2 * s, d.y, 4 * s, 3 * s), rgb(200, 205, 215));
                        self.fill(Rect::new(d.x + s, d.y + 4 * s, 6 * s, 4 * s), 0xFFFFFF);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ------------------------------------------------------------------ desktop

const ARROW: [&str; 19] = [
    "X", "XX", "X.X", "X..X", "X...X", "X....X", "X.....X", "X......X", "X.......X", "X........X",
    "X.........X", "X......XXXX", "X...X..X", "X..XX..X", "X.X  X..X", "XX   X..X", "X     X..X", "      X..X",
    "       XX",
];

// Window frame sizes at scale 1, close to the 2009-era glass look.
const BORDER: i32 = 8;
const TITLE: i32 = 30;
const TASKBAR: i32 = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Welcome,
    System,
    Notes,
    Computer,
    Calculator,
    Photos,
    Music,
    ControlPanel,
    Paint,
}

const KINDS: [Kind; 9] = [Kind::Computer, Kind::Notes, Kind::Calculator, Kind::Paint, Kind::Photos, Kind::Music, Kind::ControlPanel, Kind::System, Kind::Welcome];

/// The programs on the left of the Start menu (Control Panel is on the right, as in Windows 7).
const MENU: [Kind; 8] = [Kind::Computer, Kind::Notes, Kind::Calculator, Kind::Paint, Kind::Photos, Kind::Music, Kind::System, Kind::Welcome];

/// A page of the Control Panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CpPage {
    Home,
    Display,
    Sound,
    Mouse,
    Clock,
}

/// Something to click in the Control Panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctl {
    Back,
    Page(CpPage),
    Background(usize),
    TextSize(i32),
    /// Text one size smaller (false) or bigger (true).
    FontStep(bool),
    Volume,
    Mute,
    TestSound,
    Speed,
    DoubleClick,
    Swap,
    TestFolder,
    /// A part of the date and time (year, month, day, hour, minute) one less or one more.
    Field(usize, bool),
    SetClock,
    Hours24(bool),
}

/// The desktop backgrounds: a name, the sky's top and bottom colours and the glow's.
const BACKGROUNDS: [(&str, u32, u32, u32); 4] = [
    ("Aero blue", 0x145CAA, 0x041E52, 0x96DCFF),
    ("Aurora", 0x0A5A5A, 0x021A26, 0x78FFC8),
    ("Dusk", 0x6E3270, 0x1E0A32, 0xFFBE82),
    ("Graphite", 0x464B55, 0x14161A, 0xC8D2E1),
];

/// Settings kept in /AeroForge.ini besides the mouse speed and network drives.
#[derive(Clone, Copy)]
struct Prefs {
    background: usize,
    /// Text and window size: 0 to fit the screen, else the scale (1 or 2).
    text_size: i32,
    /// Text size: -1 smaller, 0 medium, 1 larger.
    font: i32,
    volume: u32,
    muted: bool,
    double_ms: u64,
    swapped: bool,
    hours24: bool,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs { background: 0, text_size: 0, font: 0, volume: 100, muted: false, double_ms: 500, swapped: false, hours24: false }
    }
}

/// Desktop icons, top to bottom.
const ICONS: [Kind; 4] = [Kind::Computer, Kind::Notes, Kind::Calculator, Kind::System];

/// Paint's tools, as on its toolbar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Pencil,
    Brush,
    Eraser,
    Fill,
    Line,
    Box,
}

const TOOLS: [(Tool, &str); 6] =
    [(Tool::Pencil, "Pencil"), (Tool::Brush, "Brush"), (Tool::Eraser, "Eraser"), (Tool::Fill, "Fill"), (Tool::Line, "Line"), (Tool::Box, "Box")];

/// Paint's brush sizes (a radius in pixels), small to large.
const PAINT_SIZES: [i32; 3] = [1, 4, 9];

/// Paint's colours, along the bottom.
const PAINT_COLORS: [(u32, &str); 14] = [
    (0x000000, "black"),
    (0x7F7F7F, "grey"),
    (0xC3C3C3, "silver"),
    (0xFFFFFF, "white"),
    (0xE22B2B, "red"),
    (0xFF8A1F, "orange"),
    (0xF5CC1F, "yellow"),
    (0x3FB34F, "green"),
    (0x1FA8A0, "teal"),
    (0x2F7FE0, "blue"),
    (0x1C2F9C, "navy"),
    (0x8E4FD0, "purple"),
    (0xE45FB0, "pink"),
    (0x8B5A2B, "brown"),
];

/// Paint's picture and how it is being drawn on.
struct Paint {
    px: Vec<u32>,
    w: i32,
    h: i32,
    tool: Tool,
    color: u32,
    /// Index into PAINT_SIZES.
    size: usize,
    /// The stroke under way: where it started and where the pointer was last
    /// (in picture pixels).
    stroke: Option<((i32, i32), (i32, i32))>,
    /// Earlier pictures, for Ctrl+Z (the last is the one before the latest stroke).
    undo: Vec<Vec<u32>>,
    /// Where it was last saved.
    path: Option<String>,
    changed: bool,
    status: String,
}

impl Paint {
    fn new(w: i32, h: i32) -> Paint {
        Paint {
            px: alloc::vec![0xFFFFFF; (w * h) as usize],
            w,
            h,
            tool: Tool::Pencil,
            color: 0x000000,
            size: 1,
            stroke: None,
            undo: Vec::new(),
            path: None,
            changed: false,
            status: String::new(),
        }
    }

    fn put(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && x < self.w && y < self.h {
            self.px[(y * self.w + x) as usize] = color;
        }
    }

    /// One touch of the tool at (x, y).
    fn dab(&mut self, x: i32, y: i32) {
        let r = PAINT_SIZES[self.size];
        match self.tool {
            Tool::Pencil | Tool::Line | Tool::Box => {
                // A thin line: one pixel, or a small square for the larger sizes.
                let half = if self.tool == Tool::Pencil { 0 } else { r / 3 };
                for dy in -half..=half {
                    for dx in -half..=half {
                        self.put(x + dx, y + dy, self.color);
                    }
                }
            }
            Tool::Brush => {
                for dy in -r..=r {
                    for dx in -r..=r {
                        if dx * dx + dy * dy <= r * r + r {
                            self.put(x + dx, y + dy, self.color);
                        }
                    }
                }
            }
            Tool::Eraser => {
                let e = r + 2;
                for dy in -e..=e {
                    for dx in -e..=e {
                        self.put(x + dx, y + dy, 0xFFFFFF);
                    }
                }
            }
            Tool::Fill => {}
        }
    }

    /// Dabs all along the line from a to b (Bresenham).
    fn line(&mut self, a: (i32, i32), b: (i32, i32)) {
        let (mut x, mut y) = a;
        let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
        let (sx, sy) = (if x < b.0 { 1 } else { -1 }, if y < b.1 { 1 } else { -1 });
        let mut err = dx + dy;
        loop {
            self.dab(x, y);
            if (x, y) == b {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    fn rectangle(&mut self, a: (i32, i32), b: (i32, i32)) {
        self.line(a, (b.0, a.1));
        self.line((b.0, a.1), b);
        self.line(b, (a.0, b.1));
        self.line((a.0, b.1), a);
    }

    /// Fills the area of one colour around (x, y) with the colour chosen.
    /// Returns how many pixels changed.
    fn flood(&mut self, x: i32, y: i32) -> usize {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return 0;
        }
        let (w, h) = (self.w, self.h);
        let old = self.px[(y * w + x) as usize];
        if old == self.color {
            return 0;
        }
        let mut count = 0;
        let mut todo = alloc::vec![(x, y)];
        while let Some((x, y)) = todo.pop() {
            // Along the row both ways, then look above and below.
            let row = (y * w) as usize;
            if self.px[row + x as usize] != old {
                continue;
            }
            let (mut l, mut r) = (x, x);
            while l > 0 && self.px[row + l as usize - 1] == old {
                l -= 1;
            }
            while r < w - 1 && self.px[row + r as usize + 1] == old {
                r += 1;
            }
            self.px[row + l as usize..=row + r as usize].fill(self.color);
            count += (r - l + 1) as usize;
            for ny in [y - 1, y + 1] {
                if ny < 0 || ny >= h {
                    continue;
                }
                let nrow = (ny * w) as usize;
                let mut inside = false;
                for nx in l..=r {
                    let same = self.px[nrow + nx as usize] == old;
                    if same && !inside {
                        todo.push((nx, ny));
                    }
                    inside = same;
                }
            }
        }
        count
    }

    /// The picture as a 24-bit BMP file (bottom row first, rows padded to 4 bytes).
    fn bmp(&self) -> Vec<u8> {
        let row = ((self.w * 3 + 3) & !3) as usize;
        let size = 54 + row * self.h as usize;
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(size as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&self.h.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&((row * self.h as usize) as u32).to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..self.h).rev() {
            let start = out.len();
            for &p in &self.px[(y * self.w) as usize..((y + 1) * self.w) as usize] {
                out.extend_from_slice(&[p as u8, (p >> 8) as u8, (p >> 16) as u8]);
            }
            out.resize(start + row, 0);
        }
        out
    }
}

/// Calculator keys, row by row; "=" fills two rows at the bottom right.
const CALC_KEYS: [[&str; 4]; 5] = [
    ["C", "<-", "/", "*"],
    ["7", "8", "9", "-"],
    ["4", "5", "6", "+"],
    ["1", "2", "3", "="],
    ["+/-", "0", ".", "="],
];

/// Calculator: what the display shows and the sum so far.
#[derive(Default)]
struct Calc {
    /// The number being typed or the last result, as shown.
    display: String,
    /// The left side and operator waiting for the right side.
    pending: Option<(f64, u8)>,
    /// The next digit starts a new number.
    fresh: bool,
}

impl Calc {
    fn value(&self) -> f64 {
        parse_number(&self.display)
    }

    fn show(&mut self, v: f64) {
        self.display = if v.is_finite() { number_text(v) } else { String::from("Cannot divide by zero") };
        self.fresh = true;
    }

    /// One key: a digit, ".", an operator, "=", "C", Backspace (8) or
    /// '~' (change sign). Returns the result when "=" finished a sum.
    fn key(&mut self, k: u8) -> Option<String> {
        match k {
            b'0'..=b'9' | b'.' => {
                if self.fresh || self.display == "0" || !self.display.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'-') {
                    self.display = String::from(if k == b'.' { "0" } else { "" });
                    self.fresh = false;
                }
                if (k != b'.' || !self.display.contains('.')) && self.display.len() < 16 {
                    self.display.push(k as char);
                }
            }
            8 => {
                if !self.fresh {
                    self.display.pop();
                    if self.display.is_empty() || self.display == "-" {
                        self.display = String::from("0");
                    }
                }
            }
            b'~' => {
                let v = -self.value();
                self.display = number_text(v);
            }
            b'c' | b'C' => *self = Calc { display: String::from("0"), ..Default::default() },
            b'+' | b'-' | b'*' | b'/' => {
                let v = match self.pending {
                    Some((left, op)) if !self.fresh => apply(left, op, self.value()),
                    _ => self.value(),
                };
                self.show(v);
                self.pending = Some((v, k));
            }
            b'=' | b'\n' => {
                let (left, op) = self.pending.take()?;
                let v = apply(left, op, self.value());
                self.show(v);
                return Some(self.display.clone());
            }
            _ => {}
        }
        None
    }
}

fn apply(a: f64, op: u8, b: f64) -> f64 {
    match op {
        b'+' => a + b,
        b'-' => a - b,
        b'*' => a * b,
        _ if b == 0.0 => f64::INFINITY,
        _ => a / b,
    }
}

fn parse_number(s: &str) -> f64 {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let mut v = 0.0;
    for b in whole.bytes().filter(u8::is_ascii_digit) {
        v = v * 10.0 + (b - b'0') as f64;
    }
    let mut scale = 0.1;
    for b in frac.bytes().filter(u8::is_ascii_digit) {
        v += (b - b'0') as f64 * scale;
        scale /= 10.0;
    }
    if neg { -v } else { v }
}

/// A result the way a pocket calculator shows it: up to 10 decimals,
/// without trailing zeros.
fn number_text(v: f64) -> String {
    if v.abs() >= 1e15 {
        return format!("{:e}", v);
    }
    let mut t = format!("{:.10}", v);
    while t.ends_with('0') {
        t.pop();
    }
    if t.ends_with('.') {
        t.pop();
    }
    if t == "-0" {
        t = String::from("0");
    }
    t
}

/// Which edges of a window a resize drag moves.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn any(&self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// How the Computer window sorts a folder (folders always come first).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
    Date,
    Type,
    Size,
}

/// The Computer window's command bar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    NewFolder,
    Cut,
    Copy,
    Paste,
    Rename,
    Delete,
    /// The Recycle Bin's own commands.
    Restore,
    Empty,
    /// Computer's: network drives.
    Map,
    Disconnect,
    Properties,
    /// Two panes: to the folder on the other side.
    CopyOver,
    MoveOver,
    /// The answers to "Delete ...?".
    Yes,
    No,
}

const COMMANDS: [(Command, &str); 7] = [
    (Command::NewFolder, "New folder"),
    (Command::Cut, "Cut"),
    (Command::Copy, "Copy"),
    (Command::Paste, "Paste"),
    (Command::Rename, "Rename"),
    (Command::Delete, "Delete"),
    (Command::Properties, "Properties"),
];

const COMPUTER_COMMANDS: [(Command, &str); 3] =
    [(Command::Map, "Map network drive"), (Command::Disconnect, "Disconnect"), (Command::Properties, "Properties")];

const BIN_COMMANDS: [(Command, &str); 4] = [
    (Command::Restore, "Restore"),
    (Command::Delete, "Delete"),
    (Command::Empty, "Empty Recycle Bin"),
    (Command::Properties, "Properties"),
];

/// The Computer window's path for the Recycle Bin.
const BIN: &str = "::bin";
/// Its path for search results.
const SEARCH: &str = "::search";
/// The view of what was deleted for good from a folder (FAT32 drives):
/// what can still be brought back.
const DELETED: &str = "::deleted";
const DELETED_COMMANDS: [(Command, &str); 1] = [(Command::Restore, "Restore")];
/// How many names a search looks at, and finds, at most.
const SEARCH_LOOKS: usize = 50_000;
const SEARCH_FINDS: usize = 2_000;
/// Where each drive keeps what was deleted from it, and the list of it.
const BIN_DIR: &str = "Recycle Bin";
const BIN_INFO: &str = "info.txt";
/// The list of what was emptied from it, so that the deleted files view can
/// show those under their own names and put them back where they were.
const BIN_GONE: &str = "emptied.txt";
/// How many emptied items that list remembers.
const BIN_GONE_MAX: usize = 500;

/// Whether the Recycle Bin's name for something ("R0007") is `name`, a
/// deleted file whose first letter may have been lost ("_0007").
fn same_id(name: &str, id: &str) -> bool {
    name.len() == id.len() && name.is_char_boundary(1) && name[1..].eq_ignore_ascii_case(&id[1..])
}

/// Something in the Recycle Bin.
#[derive(Clone)]
struct Recycled {
    /// The drive it came from ("/", "/sata0p1", ...).
    root: String,
    /// Its name inside the drive's Recycle Bin folder, such as "R0007".
    id: String,
    /// Where it was.
    original: String,
    is_dir: bool,
    size: u64,
    /// When it was deleted, "10/8/2026 4:52 AM".
    when: String,
}

impl Recycled {
    fn stored(&self) -> String {
        join(&join(&self.root, BIN_DIR), &self.id)
    }
}

/// A file or folder being copied, moved or dragged.
#[derive(Clone)]
struct Item {
    path: String,
    is_dir: bool,
    size: u64,
}

/// What Copy or Cut picked up.
#[derive(Clone)]
struct Clip {
    items: Vec<Item>,
    /// Cut: Paste moves them instead of copying them.
    cut: bool,
}

/// A press on selected files in the Computer window: once the pointer
/// moves a few pixels with the button held, they are dragged.
struct FileDrag {
    x0: i32,
    y0: i32,
    items: Vec<Item>,
    active: bool,
    /// Pressed on one of several selected: if no drag happens, the click
    /// leaves just this one selected.
    collapse: Option<usize>,
}

/// What dropping dragged files does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DropAction {
    Move,
    Copy,
    Recycle,
}

/// "Map network drive": where, and who to sign in as.
struct MapForm {
    /// Folder ("\\\\server\\share"), user name, password.
    fields: [String; 3],
    focus: usize,
    /// Why the last try failed.
    error: String,
    /// Signing in again to this saved drive, rather than adding one.
    share: Option<usize>,
}

/// What a row of the navigation pane is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Computer,
    Drive,
    Network,
    Bin,
    Folder,
}

/// What a line of the right-click menu does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Act {
    Open,
    OpenInTab,
    Do(Command),
    Refresh,
    /// Large icons or Details.
    View,
    /// What was deleted for good from this folder.
    ShowDeleted,
    /// Allow changes to the selected (NTFS) drive, or make it read-only again.
    Writes(bool),
    /// A line between groups.
    Line,
}

/// The right-click menu: where it is, its lines, and what they act on (a
/// place in the pane, for Open and Open in new tab there).
struct Popup {
    x: i32,
    y: i32,
    items: Vec<(Act, &'static str, bool)>,
    place: Option<String>,
}

/// A row of the navigation pane's tree.
#[derive(Clone)]
struct PaneRow {
    name: String,
    path: String,
    kind: Place,
    depth: i32,
    /// Its arrow: open or closed, or none when there is nothing inside.
    open: Option<bool>,
}

impl Files {
    fn new() -> Self {
        Files {
            path: String::new(),
            entries: Vec::new(),
            drives: Vec::new(),
            scroll: 0,
            selected: None,
            marked: Vec::new(),
            anchor: None,
            status: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            sort: SortBy::Name,
            descending: false,
            rename: None,
            rename_all: false,
            confirm: None,
            bin: Vec::new(),
            last_recycled: Vec::new(),
            form: None,
            query: String::new(),
            query_focus: false,
            searched_in: String::new(),
            results: Vec::new(),
            found: Vec::new(),
            lost_in: String::new(),
            lost: Vec::new(),
            lost_bin: Vec::new(),
            props: None,
        }
    }
    /// What tells entry `i` apart from the rest: its name, or in the
    /// Recycle Bin (where two can have the same name) its number, or in
    /// search results its full path.
    fn key(&self, i: usize) -> String {
        if self.path == BIN {
            return self.bin.get(i).map(|b| b.id.clone()).unwrap_or_default();
        }
        if self.path == SEARCH || self.path == DELETED {
            return self.found.get(i).cloned().unwrap_or_default();
        }
        self.entries.get(i).map(|e| e.name.clone()).unwrap_or_default()
    }
    /// Where entry `i` is.
    fn entry_path(&self, i: usize) -> String {
        if self.path == SEARCH {
            return self.found[i].clone();
        }
        if self.path == DELETED {
            return join(&self.lost_in, &self.entries[i].name);
        }
        join(&self.path, &self.entries[i].name)
    }
    fn is_marked(&self, i: usize) -> bool {
        match self.selected {
            None => false,
            Some(at) if self.marked.is_empty() => at == i,
            Some(_) => self.marked.contains(&self.key(i)),
        }
    }
    /// Whether it shows a folder (not Computer, the Recycle Bin or search results).
    fn in_folder(&self) -> bool {
        !self.path.is_empty() && self.path != BIN && self.path != SEARCH && self.path != DELETED
    }
    /// In the deleted files view: what entry `i` is.
    fn lost_at(&self, i: usize) -> Option<&aero::Deleted> {
        let slot: u32 = self.found.get(i)?.parse().ok()?;
        self.lost.iter().find(|d| d.slot == slot)
    }
}

/// A tab of the Computer window: the folder shown, and in two-pane mode
/// the other side too. The tab in front lives in Desktop's own fields;
/// its slot here is empty until another tab is brought forward.
struct Tab {
    files: Files,
    other: Option<Box<Files>>,
    right: bool,
}

impl Default for Tab {
    fn default() -> Self {
        Tab { files: Files::new(), other: None, right: false }
    }
}

/// The preview pane's copy of what is selected.
struct Preview {
    path: String,
    entry: aero::DirEntry,
    /// The first PREVIEW_BYTES of a file.
    data: Vec<u8>,
    /// Its lines, when it reads as text.
    lines: Vec<String>,
    /// Shown as bytes (hex) rather than text.
    hex: bool,
    /// What a folder holds.
    items: Option<usize>,
    error: Option<String>,
    /// The first line (of text or of hex) shown.
    scroll: usize,
}

/// How much of a file the preview pane reads.
const PREVIEW_BYTES: usize = 64 * 1024;

/// The Properties box: what is selected, in more detail.
struct Props {
    title: String,
    folder: bool,
    rows: Vec<(&'static str, String)>,
    /// A drive's used and total bytes, drawn as a pie.
    pie: Option<(u64, u64)>,
}

/// What the Computer window (the file manager) shows and is doing.
struct Files {
    /// The folder shown, or "" for Computer itself: the drives.
    path: String,
    entries: Vec<aero::DirEntry>,
    drives: Vec<aero::Volume>,
    /// The first entry shown.
    scroll: usize,
    /// The selected entry (the one with the focus), or drive in Computer.
    selected: Option<usize>,
    /// Every entry selected (by key(): name, or Recycle Bin number), when
    /// Ctrl or Shift picked more than one. Empty: just `selected`.
    marked: Vec<String>,
    /// Where a Shift+click or Shift+arrow range starts.
    anchor: Option<usize>,
    status: String,
    /// Where Back and Forward go.
    back: Vec<String>,
    forward: Vec<String>,
    sort: SortBy,
    descending: bool,
    /// The new name being typed for the selected entry.
    rename: Option<String>,
    /// The whole name is still selected: the first key typed replaces it.
    rename_all: bool,
    /// Asking about Delete or Empty (the Recycle Bin), with Yes and No.
    confirm: Option<Command>,
    /// The Recycle Bin, when shown: one per entry.
    bin: Vec<Recycled>,
    /// The last things deleted together, for Ctrl+Z.
    last_recycled: Vec<Recycled>,
    /// "Map network drive" is open (in Computer, over the drives).
    form: Option<MapForm>,
    /// What is typed in the search box, and whether keys go there.
    query: String,
    query_focus: bool,
    /// The last search: where it looked and what it found (full paths).
    searched_in: String,
    results: Vec<(String, aero::DirEntry)>,
    /// In the search results view: each entry's full path. In the deleted
    /// files view: each entry's slot in its folder.
    found: Vec<String>,
    /// The deleted files view: the folder looked in, and what it found.
    lost_in: String,
    lost: Vec<aero::Deleted>,
    /// When looking in a Recycle Bin's folder: what each of `lost` was
    /// before it went to the bin.
    lost_bin: Vec<Recycled>,
    /// The Properties box, when open.
    props: Option<Props>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Caption {
    Minimize,
    Maximize,
    Close,
}

struct Window {
    kind: Kind,
    title: &'static str,
    rect: Rect,
    /// Has a taskbar button (closed windows come back from the Start menu).
    open: bool,
    minimized: bool,
    /// Where it was before it was maximized.
    restore: Option<Rect>,
}

impl Window {
    fn shown(&self) -> bool {
        self.open && !self.minimized
    }
}

struct Desktop {
    w: i32,
    h: i32,
    ui: i32,
    windows: Vec<Window>, // back to front
    notes: String,
    /// The file Notes opened, where Save writes; None until one is opened.
    notes_path: Option<String>,
    /// What Notes says next to its Save button ("Saved", "Not saved", ...).
    notes_status: String,
    /// The file was too long to open whole, so Save is refused.
    notes_cut: bool,
    pointer: Pointer,
    drag: Option<(usize, i32, i32)>, // window, grab offset
    /// The window being resized, which edges move, and where it and the
    /// pointer were when the drag began.
    resize: Option<(usize, Edges, Rect, i32, i32)>,
    /// Where the window being dragged will snap to if let go now.
    snap: Option<Rect>,
    menu: bool,
    /// The Start menu shows every program, A to Z, instead of the usual few.
    all_programs: bool,
    /// Windows hidden by "Show desktop", to bring back on the next click.
    peeked: Vec<Kind>,
    started_us: u64,
    clock: (String, String),
    quit: bool,
    files: Files,
    calc: Calc,
    /// The selected desktop icon.
    icon: Option<usize>,
    /// Pointer speed, 1 to 10 (see aero::mouse_speed).
    mouse_speed: u64,
    /// Files being dragged in the Computer window (or pressed on, about to be).
    file_drag: Option<FileDrag>,
    /// The modifier keys held as the key being handled was typed.
    key_mods: u8,
    /// The gamepad: when it was last read, the pointer movement still owed
    /// (thousandths of a pixel), its buttons held, and the mouse buttons it holds.
    pad_us: u64,
    pad_rest: (i64, i64),
    pad_held: u16,
    pad_mouse: u8,
    /// Pads seen at rest (no button held, sticks in the middle) since they
    /// connected, a bit each: only those count, so a pad that sends a stuck
    /// button or a bent stick from the start cannot take the pointer over.
    pad_armed: u8,
    /// Whether a stick is moving the pointer (logged when it stops).
    pad_moving: bool,
    /// The navigation pane's tree: the places opened (lower case), and the
    /// folders found in each.
    tree_open: Vec<String>,
    tree_kids: Vec<(String, Vec<String>)>,
    popup: Option<Popup>,
    /// The picture open in Photos, and the song in Music.
    photo: Option<Photo>,
    player: Option<Player>,
    paint: Option<Paint>,
    /// A copy or move under way, with its progress window.
    progress: Option<Progress>,
    /// The Control Panel: its page, the settings, the date and time being
    /// set, the test folder on the Mouse page, and a test sound still to play.
    cp_page: CpPage,
    prefs: Prefs,
    cp_time: Option<aero::DateTime>,
    cp_folder_open: bool,
    /// The thread playing Control Panel's test sound.
    test_sound: Option<aero::thread::JoinHandle<()>>,
    /// Two-pane mode: the side not in use (`files` is the one in use), and
    /// whether the one in use is the right-hand side.
    other: Option<Box<Files>>,
    right: bool,
    /// The Computer window's tabs, and which is in front (its slot in
    /// `tabs` is empty: it is `files`, `other` and `right`).
    tabs: Vec<Tab>,
    tab: usize,
    /// The preview pane at the right of the Computer window, and what it shows.
    preview_on: bool,
    /// Folders shown as Large icons instead of Details.
    icons: bool,
    preview: Option<Preview>,
    /// What Copy or Cut picked up, for any tab or side.
    clip: Option<Clip>,
}

impl Desktop {
    fn taskbar(&self) -> Rect {
        Rect::new(0, self.h - taskbar_h(self.ui), self.w, taskbar_h(self.ui))
    }
    fn title_h(&self) -> i32 {
        (TITLE * self.ui).max(Font::Bold.h(self.ui) + 6 * self.ui)
    }
    fn client(&self, r: &Rect) -> Rect {
        let b = BORDER * self.ui;
        Rect::new(r.x + b, r.y + self.title_h(), r.w - 2 * b, r.h - self.title_h() - b)
    }
    /// The caption button and where it is: minimize, maximize and close,
    /// joined, hanging from the top edge at the right.
    fn caption(&self, r: &Rect, which: Caption) -> Rect {
        let s = self.ui;
        let right = r.x + r.w - 6 * s;
        let (x, w) = match which {
            Caption::Close => (right - 43 * s, 43 * s),
            Caption::Maximize => (right - 43 * s - 25 * s, 25 * s),
            Caption::Minimize => (right - 43 * s - 25 * s - 26 * s, 26 * s),
        };
        Rect::new(x, r.y + s, w, 19 * s)
    }
    fn start_button(&self) -> (i32, i32, i32) {
        let t = self.taskbar();
        (t.x + 27 * self.ui, t.y + t.h / 2, 18 * self.ui)
    }
    fn show_desktop(&self) -> Rect {
        let t = self.taskbar();
        Rect::new(t.x + t.w - 14 * self.ui, t.y, 14 * self.ui, t.h)
    }
    fn tray(&self) -> Rect {
        let t = self.taskbar();
        let (font, s) = (self.clock_font(), self.ui);
        let w = (100 * s).max(font.width(s, &self.clock.0).max(font.width(s, &self.clock.1)) + 12 * s);
        Rect::new(self.show_desktop().x - w, t.y, w, t.h)
    }
    /// The taskbar clock's two lines: Small, or Tiny when Small is too tall to fit both.
    fn clock_font(&self) -> Font {
        if 2 * Font::Small.h(self.ui) > self.taskbar().h { Font::Tiny } else { Font::Small }
    }
    fn task_button(&self, i: usize) -> Rect {
        let t = self.taskbar();
        let s = self.ui;
        // 150 wide, or narrower when that many would run into the clock.
        let (left, n) = (t.x + 58 * s, self.task_order().len().max(1) as i32);
        let step = (154 * s).min((self.tray().x - 4 * s - left) / n);
        Rect::new(left + i as i32 * step, t.y + 3 * s, step - 4 * s, t.h - 6 * s)
    }
    fn menu_rect(&self) -> Rect {
        let s = self.ui;
        let (t, g) = (self.taskbar(), grow(s));
        // Taller and wider as the text grows.
        Rect::new(2 * s, t.y - 420 * s - 11 * g, 480 * s + 12 * g, 420 * s + 11 * g)
    }
    fn menu_left(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + 8 * s, m.y + 8 * s, 250 * s + 6 * grow(s), m.h - 56 * s - grow(s))
    }
    fn menu_item(&self, i: usize) -> Rect {
        let l = self.menu_left();
        let s = self.ui;
        let g = grow(s);
        Rect::new(l.x + 4 * s, l.y + 6 * s + i as i32 * (40 * s + g), l.w - 8 * s, 36 * s + g)
    }
    /// "All programs" at the foot of the Start menu's left side ("Back" when
    /// the list of every program is showing).
    fn all_button(&self) -> Rect {
        let l = self.menu_left();
        let s = self.ui;
        let g = grow(s);
        Rect::new(l.x + 4 * s, l.y + l.h - 36 * s - g, l.w - 8 * s, 32 * s + g)
    }
    /// Every program, A to Z, for "All programs".
    fn all_list(&self) -> Vec<Kind> {
        let mut all = KINDS.to_vec();
        all.sort_by_key(|&k| self.windows[self.index(k)].title);
        all
    }
    /// Row `i` of the "All programs" list.
    fn all_item(&self, i: usize) -> Rect {
        let l = self.menu_left();
        let s = self.ui;
        let g = grow(s);
        Rect::new(l.x + 4 * s, l.y + 6 * s + i as i32 * (34 * s + g), l.w - 8 * s, 32 * s + g)
    }
    /// The Start menu's mouse speed buttons: slower (-) and faster (+), with
    /// ten bars between them.
    fn speed_button(&self, faster: bool) -> Rect {
        let l = self.menu_left();
        let m = self.menu_rect();
        let s = self.ui;
        let rx = l.x + l.w + 14 * s;
        Rect::new(rx + if faster { 152 * s } else { 0 }, m.y + 160 * s + 4 * grow(s), 30 * s, 30 * s)
    }
    /// "Control Panel" on the right of the Start menu, under the mouse speed.
    fn cp_menu_item(&self) -> Rect {
        let l = self.menu_left();
        let m = self.menu_rect();
        let s = self.ui;
        let g = grow(s);
        Rect::new(l.x + l.w + 8 * s, m.y + 214 * s + 5 * g, m.x + m.w - (l.x + l.w) - 16 * s, 36 * s + g)
    }
    fn exit_button(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        let w = (190 * s).max(Font::Normal.width(s, "Exit to console") + 24 * s);
        let g = grow(s);
        Rect::new(m.x + m.w - 10 * s - w, m.y + m.h - 42 * s - g, w, 34 * s + g)
    }
    fn cursor_rect(&self) -> Rect {
        // Room for the arrow and for the resize arrows centred on the pointer,
        // and for the label under it while files are dragged.
        let s = self.ui;
        let (x, y) = (self.pointer.x as i32, self.pointer.y as i32);
        if self.file_drag.as_ref().is_some_and(|d| d.active) {
            return Rect::new(x - 10 * s, y - 10 * s, 340 * s, 60 * s);
        }
        Rect::new(x - 10 * s, y - 10 * s, 23 * s, 30 * s)
    }
    /// The edges of the front-most window under (x, y) that a drag from
    /// there would move: the outer few pixels of its frame and its corners.
    fn edges_at(&self, x: i32, y: i32) -> Option<(usize, Edges)> {
        let i = (0..self.windows.len()).rev().find(|&i| self.windows[i].shown() && self.windows[i].rect.contains(x, y))?;
        let win = &self.windows[i];
        if win.restore.is_some() {
            return None;
        }
        let r = win.rect;
        let band = 5 * self.ui;
        let corner = 14 * self.ui;
        let near_left = x < r.x + band;
        let near_right = x >= r.x + r.w - band;
        let near_top = y < r.y + band;
        let near_bottom = y >= r.y + r.h - band;
        // Close to a corner, along either edge, moves both.
        let e = Edges {
            left: near_left || ((near_top || near_bottom) && x < r.x + corner),
            right: near_right || ((near_top || near_bottom) && x >= r.x + r.w - corner),
            top: near_top || ((near_left || near_right) && y < r.y + corner),
            bottom: near_bottom || ((near_left || near_right) && y >= r.y + r.h - corner),
        };
        e.any().then_some((i, e))
    }
    fn icon_rect(&self, i: usize) -> Rect {
        let s = self.ui;
        // Wide and tall enough for every name, so the biggest text sizes do
        // not cut them off at the screen's edge.
        let w = (116 * s).max(self.icon_label_width(Font::Tiny) + 8 * s);
        let h = (86 * s).max(58 * s + Font::Normal.h(s));
        Rect::new(4 * s, 10 * s + i as i32 * (h + 8 * s), w, h)
    }
    /// The widest desktop icon name in `font`.
    fn icon_label_width(&self, font: Font) -> i32 {
        ICONS.iter().map(|&k| font.width(self.ui, self.windows[self.index(k)].title)).max().unwrap_or(0)
    }

    /// The calculator's display and key `(row, col)` inside its client area.
    fn calc_display(&self, client: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(client.x + 10 * s, client.y + 10 * s, client.w - 20 * s, 52 * s)
    }
    fn calc_key(&self, client: &Rect, row: usize, col: usize) -> Rect {
        let s = self.ui;
        let top = client.y + 72 * s;
        let (kw, kh) = ((client.w - 20 * s - 3 * 6 * s) / 4, (client.y + client.h - 10 * s - top - 4 * 6 * s) / 5);
        let x = client.x + 10 * s + col as i32 * (kw + 6 * s);
        let y = top + row as i32 * (kh + 6 * s);
        // "=" covers the last two rows of the last column.
        if CALC_KEYS[row][col] == "=" {
            return Rect::new(x, top + 3 * (kh + 6 * s), kw, 2 * kh + 6 * s);
        }
        Rect::new(x, y, kw, kh)
    }

    fn notes_toolbar(&self, client: &Rect) -> Rect {
        Rect::new(client.x, client.y, client.w, (32 * self.ui).max(Font::Normal.h(self.ui) + 8 * self.ui))
    }
    fn notes_save(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let t = self.notes_toolbar(client);
        Rect::new(t.x + 6 * s, t.y + 4 * s, (64 * s).max(Font::Normal.width(s, "Save") + 16 * s), t.h - 8 * s)
    }
    fn index(&self, kind: Kind) -> usize {
        self.windows.iter().position(|w| w.kind == kind).unwrap_or(0)
    }
    fn top(&self) -> Option<&Window> {
        self.windows.iter().rev().find(|w| w.shown())
    }
    fn top_kind(&self) -> Option<Kind> {
        self.top().map(|w| w.kind)
    }
    /// Open windows in taskbar order.
    fn task_order(&self) -> Vec<Kind> {
        KINDS.iter().copied().filter(|&k| self.windows[self.index(k)].open).collect()
    }

    // -------------------------------------------------------------- drawing

    fn background(&self, c: &mut Canvas) {
        // Deep blue with a bright glow low on the right and soft light
        // bands sweeping across it.
        let clip = c.clip;
        let paper = Wallpaper::new(self.prefs.background, &Rect::new(0, 0, self.w, self.h));
        for y in clip.y..clip.y + clip.h {
            let row = (y * c.w) as usize;
            for x in clip.x..clip.x + clip.w {
                c.px[row + x as usize] = paper.px(x, y);
            }
        }
    }

    fn window(&self, c: &mut Canvas, win: &Window, active: bool) {
        let r = win.rect;
        let s = self.ui;
        if Rect::new(r.x - 8 * s, r.y - 8 * s, r.w + 16 * s, r.h + 16 * s).intersect(&c.clip).is_empty() {
            return;
        }
        // Soft shadow all round, bigger on the active window.
        let spread = if active { 8 * s } else { 5 * s };
        for i in 1..=spread {
            c.rounded_outline(Rect::new(r.x - i, r.y - i + 2 * s, r.w + 2 * i, r.h + 2 * i), 8 * s + i, false, 0, 18);
        }
        // The glass frame: the wallpaper shows through, tinted.
        let (tint_top, tint_bottom, alpha) = if active {
            (rgb(170, 210, 245), rgb(90, 150, 210), 150)
        } else {
            (rgb(200, 215, 230), rgb(150, 170, 195), 120)
        };
        c.rounded(r, 7 * s, true, tint_top, tint_bottom, alpha);
        // A diagonal streak of light across the glass.
        let streak_x = r.x + r.w / 3;
        for y in r.y..r.y + self.title_h() {
            let x0 = streak_x - (y - r.y);
            c.shade(Rect::new(x0, y, 40 * s, 1), 0xFFFFFF, 28);
        }
        c.rounded(Rect::new(r.x + 1, r.y + 1, r.w - 2, self.title_h() / 2), 6 * s, true, 0xFFFFFF, 0xFFFFFF, 55);
        c.rounded_outline(r, 7 * s, true, rgb(10, 30, 60), 200);
        c.rounded_outline(Rect::new(r.x + 1, r.y + 1, r.w - 2, r.h - 2), 6 * s, true, 0xFFFFFF, 90);

        let outer = c.clip;
        c.clip = outer.intersect(&Rect::new(r.x, r.y, self.caption(&r, Caption::Minimize).x - 4 * s - r.x, self.title_h()));
        c.glow_text(r.x + 14 * s, r.y + (self.title_h() - Font::Bold.h(s)) / 2, win.title, 0x000000, Font::Bold);
        c.clip = outer;
        for which in [Caption::Minimize, Caption::Maximize, Caption::Close] {
            self.caption_button(c, &r, which);
        }

        let client = self.client(&r);
        c.fill(Rect::new(client.x - 1, client.y - 1, client.w + 2, client.h + 2), rgb(90, 110, 140));
        c.fill(client, 0xFFFFFF);
        // Whatever a program draws stays inside its window, however big the text.
        c.clip = outer.intersect(&client);
        let (tx, mut ty) = (client.x + 10 * s, client.y + 10 * s);
        let line = Font::Normal.h(s) + 4 * s;
        let mut say = |c: &mut Canvas, text: &str, color: u32| {
            c.text(tx, ty, text, color, Font::Normal);
            ty += line;
        };
        match win.kind {
            Kind::Welcome => {
                say(c, "Welcome to AeroForge OS.", rgb(0, 51, 153));
                say(c, "", 0);
                say(c, "Drag a window by its title bar.", 0x202020);
                say(c, "Click a window to bring it forward.", 0x202020);
                say(c, "_  minimizes, [] maximizes, x closes.", 0x202020);
                say(c, "Taskbar buttons switch windows.", 0x202020);
                say(c, "Type into Notes when it is in front.", 0x202020);
                say(c, "Start > Exit to console, or Esc.", 0x202020);
            }
            Kind::System => {
                say(c, &format!("Screen   {} x {}", self.w, self.h), 0x202020);
                say(c, &format!("Up for   {}", uptime(aero::clock_us() - self.started_us)), 0x202020);
                let p = self.pointer;
                say(c, &format!("Pointer  {}, {}", p.x, p.y), 0x202020);
                say(c, &format!("Clicks   {}", p.presses), 0x202020);
            }
            Kind::Notes => self.notes_view(c, &client, active),
            Kind::Computer => self.files_view(c, &client),
            Kind::Calculator => self.calc_view(c, &client),
            Kind::Photos => self.photos_view(c, &client),
            Kind::Music => self.music_view(c, &client),
            Kind::ControlPanel => self.cp_view(c, &client),
            Kind::Paint => self.paint_view(c, &client),
        }
        c.clip = outer;
    }

    fn notes_view(&self, c: &mut Canvas, client: &Rect, active: bool) {
        let s = self.ui;
        // Toolbar: Save, the file's name and whether it is saved.
        let t = self.notes_toolbar(client);
        c.gradient(t, rgb(245, 250, 255), rgb(215, 230, 245), 255);
        c.fill(Rect::new(t.x, t.y + t.h - 1, t.w, 1), rgb(160, 180, 205));
        let b = self.notes_save(client);
        let hover = b.contains(self.pointer.x as i32, self.pointer.y as i32);
        let (top, bottom) = if hover { (rgb(235, 248, 255), rgb(170, 215, 250)) } else { (rgb(252, 253, 255), rgb(215, 225, 240)) };
        c.rounded(b, 3 * s, false, top, bottom, 255);
        c.rounded_outline(b, 3 * s, false, rgb(120, 140, 170), 255);
        let label = "Save";
        c.text(b.x + (b.w - Font::Normal.width(s, label)) / 2, b.y + (b.h - Font::Normal.h(s)) / 2, label, 0x101010, Font::Normal);
        let text_y = t.y + (t.h - Font::Normal.h(s)) / 2;
        let status_w = Font::Normal.width(s, &self.notes_status);
        let status_x = t.x + t.w - 8 * s - status_w;
        c.text(status_x, text_y, &self.notes_status, rgb(70, 90, 120), Font::Normal);
        let name = self.notes_path.as_deref().map(|p| p.rsplit('/').next().unwrap_or(p)).unwrap_or("New note");
        let room = ((status_x - 12 * s - (b.x + b.w + 10 * s)) / Font::Normal.w(s)).max(0) as usize;
        let shown: String = if name.chars().count() > room {
            name.chars().take(room.saturating_sub(3)).chain("...".chars()).collect()
        } else {
            String::from(name)
        };
        c.text(b.x + b.w + 10 * s, text_y, &shown, 0x101010, Font::Normal);

        // The text, word-wrapped, showing the end when it does not fit.
        let line = Font::Normal.h(s) + 4 * s;
        let area = Rect::new(client.x, t.y + t.h, client.w, client.h - t.h);
        let cols = ((area.w - 20 * s) / Font::Normal.w(s)).max(1) as usize;
        let rows = ((area.h - 16 * s) / line).max(1) as usize;
        let mut lines: Vec<String> = Vec::new();
        // Word wrap: break at the last space that fits, or mid-word
        // when a word is longer than the line.
        for part in self.notes.split('\n') {
            let mut rest: Vec<char> = part.chars().collect();
            loop {
                if rest.len() <= cols {
                    lines.push(rest.iter().collect());
                    break;
                }
                let cut = rest[..=cols].iter().rposition(|&ch| ch == ' ').filter(|&i| i > 0).unwrap_or(cols);
                lines.push(rest[..cut].iter().collect());
                let skip = rest[cut..].iter().take_while(|&&ch| ch == ' ').count();
                rest.drain(..cut + skip);
            }
        }
        let caret = active && (aero::clock_us() / 500_000) % 2 == 0;
        if let Some(last) = lines.last_mut() {
            if caret {
                last.push('_');
            }
        }
        let skip = lines.len().saturating_sub(rows);
        let mut y = area.y + 8 * s;
        for l in &lines[skip..] {
            c.text(area.x + 10 * s, y, l, 0x101010, Font::Normal);
            y += line;
        }
    }


    fn calc_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        c.gradient(*client, rgb(225, 235, 248), rgb(200, 215, 235), 255);
        let d = self.calc_display(client);
        c.gradient(d, rgb(250, 252, 255), rgb(228, 238, 250), 255);
        c.frame(d, rgb(130, 150, 180));
        if let Some((_, op)) = self.calc.pending {
            c.text(d.x + 8 * s, d.y + 6 * s, &format!("{}", op as char), rgb(90, 100, 120), Font::Normal);
        }
        let shown = &self.calc.display;
        let font = if Font::Large.width(s, shown) <= d.w - 16 * s { Font::Large } else { Font::Normal };
        c.text(d.x + d.w - 8 * s - font.width(s, shown), d.y + d.h - font.h(s) - 6 * s, shown, 0x101010, font);
        for row in 0..5 {
            for col in 0..4 {
                let label = CALC_KEYS[row][col];
                if label == "=" && row == 4 {
                    continue;
                }
                let k = self.calc_key(client, row, col);
                let hover = k.contains(self.pointer.x as i32, self.pointer.y as i32);
                let (top, bottom) = match (label, hover) {
                    ("=", false) => (rgb(255, 225, 170), rgb(240, 170, 80)),
                    ("=", true) => (rgb(255, 240, 200), rgb(250, 190, 110)),
                    (_, false) => (rgb(252, 253, 255), rgb(215, 225, 240)),
                    (_, true) => (rgb(235, 248, 255), rgb(170, 215, 250)),
                };
                c.rounded(k, 3 * s, false, top, bottom, 255);
                c.rounded_outline(k, 3 * s, false, rgb(120, 140, 170), 255);
                let (tw, th) = (Font::Normal.width(s, label), Font::Normal.h(s));
                c.text(k.x + (k.w - tw) / 2, k.y + (k.h - th) / 2, label, 0x101010, Font::Normal);
            }
        }
    }

    /// The desktop icons: a picture over a white label with a dark shadow.
    fn icons(&self, c: &mut Canvas) {
        let s = self.ui;
        for (i, &kind) in ICONS.iter().enumerate() {
            let r = self.icon_rect(i);
            if r.intersect(&c.clip).is_empty() {
                continue;
            }
            let hover = r.contains(self.pointer.x as i32, self.pointer.y as i32);
            if self.icon == Some(i) {
                c.rounded(r, 3 * s, false, rgb(150, 200, 250), rgb(90, 150, 220), 110);
                c.rounded_outline(r, 3 * s, false, rgb(200, 230, 255), 170);
            } else if hover {
                c.rounded(r, 3 * s, false, rgb(150, 200, 250), rgb(90, 150, 220), 60);
                c.rounded_outline(r, 3 * s, false, rgb(200, 230, 255), 110);
            }
            c.icon(kind_icon(kind), r.x + r.w / 2 - 24 * s, r.y + 6 * s, 48 * s);
            let label = self.windows[self.index(kind)].title;
            // One size for every name: the biggest they all fit in.
            let font = [Font::Normal, Font::Small].into_iter().find(|&f| self.icon_label_width(f) <= r.w - 4 * s).unwrap_or(Font::Tiny);
            let lx = r.x + (r.w - font.width(s, label)) / 2;
            let ly = r.y + r.h - font.h(s) - 4 * s;
            c.text(lx + s, ly + s, label, 0x000000, font);
            c.text(lx, ly, label, 0xFFFFFF, font);
        }
    }

    fn caption_button(&self, c: &mut Canvas, r: &Rect, which: Caption) {
        let s = self.ui;
        let b = self.caption(r, which);
        let hover = b.contains(self.pointer.x as i32, self.pointer.y as i32);
        let (top, bottom) = match (which, hover) {
            (Caption::Close, false) => (rgb(230, 150, 130), rgb(190, 50, 30)),
            (Caption::Close, true) => (rgb(250, 170, 150), rgb(230, 60, 30)),
            (_, false) => (rgb(220, 235, 250), rgb(150, 185, 220)),
            (_, true) => (rgb(235, 250, 255), rgb(110, 190, 240)),
        };
        let top_only = false;
        c.rounded(b, 3 * s, top_only, top, bottom, 235);
        c.rounded_outline(b, 3 * s, top_only, rgb(40, 60, 90), 200);
        let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
        let ink = 0xFFFFFF;
        match which {
            Caption::Close => {
                let d = 4 * s;
                c.line(cx - d - s, cy - d, cx + d - s, cy + d, 2 * s, ink);
                c.line(cx + d - s, cy - d, cx - d - s, cy + d, 2 * s, ink);
            }
            Caption::Maximize => {
                let box_ = Rect::new(cx - 5 * s, cy - 4 * s, 10 * s, 8 * s);
                c.frame(box_, 0x1A2A40);
                c.fill(Rect::new(box_.x, box_.y, box_.w, 2 * s), 0x1A2A40);
            }
            Caption::Minimize => c.fill(Rect::new(cx - 5 * s, cy + 2 * s, 10 * s, 2 * s), 0x1A2A40),
        }
    }

    fn taskbar_and_menu(&self, c: &mut Canvas) {
        let t = self.taskbar();
        let s = self.ui;
        // Dark glass with a lighter sheen on the upper half.
        c.gradient(t, rgb(30, 55, 85), rgb(5, 15, 30), 175);
        c.shade(Rect::new(t.x, t.y, t.w, t.h / 2), 0xFFFFFF, 22);
        c.fill(Rect::new(t.x, t.y, t.w, 1), rgb(150, 190, 230));
        c.fill(Rect::new(t.x, t.y + 1, t.w, 1), rgb(20, 40, 70));

        // Start: a glossy red, white and blue roundel with a black star.
        let (ox, oy, or) = self.start_button();
        let hot = self.menu || (self.pointer.x as i32 - ox).pow(2) + (self.pointer.y as i32 - oy).pow(2) <= or * or;
        c.start_orb(ox, oy, or, hot);

        let top = self.top_kind();
        // One size for every button's title: the chosen text size while five
        // letters and ".." still fit, else smaller; long titles are cut short.
        let order = self.task_order();
        let room = order.first().map_or(0, |_| self.task_button(0).w - 40 * s);
        let font = [Font::Normal, Font::Small].into_iter().find(|&f| f.width(s, "Abcde..") <= room).unwrap_or(Font::Tiny);
        for (i, kind) in order.into_iter().enumerate() {
            let win = &self.windows[self.index(kind)];
            let b = self.task_button(i);
            let active = win.shown() && Some(kind) == top;
            let hover = b.contains(self.pointer.x as i32, self.pointer.y as i32);
            let (a, z, alpha) = if active {
                (rgb(170, 210, 245), rgb(60, 110, 170), 170)
            } else if hover {
                (rgb(140, 170, 200), rgb(50, 80, 120), 140)
            } else {
                (rgb(110, 130, 160), rgb(30, 45, 70), 110)
            };
            c.rounded(b, 3 * s, false, a, z, alpha);
            c.rounded_outline(b, 3 * s, false, if active { rgb(220, 240, 255) } else { rgb(120, 150, 190) }, 160);
            // The program's icon, then the title.
            c.icon(kind_icon(kind), b.x + 6 * s, b.y + b.h / 2 - 12 * s, 24 * s);
            let room = b.w - 40 * s;
            let mut title = String::from(win.title);
            if font.width(s, &title) > room {
                while !title.is_empty() && font.width(s, &format!("{}..", title)) > room {
                    title.pop();
                }
                title.truncate(title.trim_end().len());
                title.push_str("..");
            }
            c.text_in(Rect::new(b.x, b.y, b.w - 6 * s, b.h), b.x + 34 * s, b.y + (b.h - font.h(s)) / 2, &title, 0xFFFFFF, font);
        }

        // Clock: time over date, then the "show desktop" strip.
        let tray = self.tray();
        let (time, date) = (&self.clock.0, &self.clock.1);
        let font = self.clock_font();
        let cx = |text: &String| tray.x + (tray.w - font.width(s, text)) / 2;
        let line = font.h(s);
        c.text(cx(time), tray.y + tray.h / 2 - line, time, 0xFFFFFF, font);
        c.text(cx(date), tray.y + tray.h / 2, date, 0xFFFFFF, font);
        let sd = self.show_desktop();
        c.shade(sd, 0xFFFFFF, if sd.contains(self.pointer.x as i32, self.pointer.y as i32) { 70 } else { 25 });
        c.fill(Rect::new(sd.x, sd.y, 1, sd.h), rgb(120, 150, 190));

        if self.menu {
            self.start_menu(c);
        }
    }

    fn start_menu(&self, c: &mut Canvas) {
        let s = self.ui;
        let m = self.menu_rect();
        c.rounded(m, 6 * s, false, rgb(60, 110, 170), rgb(10, 35, 70), 245);
        c.rounded_outline(m, 6 * s, false, rgb(160, 200, 240), 220);
        // Left: the programs on white.
        let l = self.menu_left();
        c.rounded(l, 4 * s, false, 0xFFFFFF, rgb(235, 242, 250), 255);
        c.rounded_outline(l, 4 * s, false, rgb(90, 120, 160), 255);
        let lit = |c: &mut Canvas, item: Rect| {
            if item.contains(self.pointer.x as i32, self.pointer.y as i32) {
                c.rounded(item, 3 * s, false, rgb(225, 240, 255), rgb(190, 220, 250), 255);
                c.rounded_outline(item, 3 * s, false, rgb(120, 170, 230), 255);
            }
        };
        if self.all_programs {
            for (i, kind) in self.all_list().into_iter().enumerate() {
                let item = self.all_item(i);
                lit(c, item);
                c.icon(kind_icon(kind), item.x + 6 * s, item.y + 4 * s, 24 * s);
                let title = self.windows[self.index(kind)].title;
                c.text(item.x + 38 * s, item.y + (item.h - Font::Normal.h(s)) / 2, title, 0x101010, Font::Normal);
            }
        } else {
            for (i, kind) in MENU.iter().enumerate() {
                let item = self.menu_item(i);
                lit(c, item);
                c.icon(kind_icon(*kind), item.x + 6 * s, item.y + 2 * s, 32 * s);
                let title = self.windows[self.index(*kind)].title;
                c.text(item.x + 40 * s, item.y + (item.h - Font::Normal.h(s)) / 2, title, 0x101010, Font::Normal);
            }
        }
        // "All programs" (or "Back"), with an arrow pointing the way it goes.
        let ab = self.all_button();
        lit(c, ab);
        c.fill(Rect::new(ab.x + 6 * s, ab.y, ab.w - 12 * s, s), rgb(210, 220, 232));
        let (ax, ay) = (ab.x + 12 * s, ab.y + ab.h / 2);
        for k in 0..5 {
            let h = if self.all_programs { (2 * k + 1) * s } else { (9 - 2 * k) * s };
            c.fill(Rect::new(ax + k * s, ay - h / 2, s, h), rgb(40, 110, 50));
        }
        let label = if self.all_programs { "Back" } else { "All programs" };
        c.text(ab.x + 28 * s, ab.y + (ab.h - Font::Normal.h(s)) / 2, label, 0x101010, Font::Normal);
        // Right: the date in large type, and the exit button.
        let rx = l.x + l.w + 14 * s;
        c.text(rx, m.y + 16 * s, &self.clock.0, 0xFFFFFF, Font::Large);
        let g = grow(s);
        c.text(rx, m.y + 56 * s + g, &self.clock.1, rgb(210, 230, 250), Font::Normal);
        c.text(rx, m.y + 84 * s + 2 * g, "AeroForge", rgb(210, 230, 250), Font::Normal);
        // Mouse speed: - and + buttons with ten bars between them.
        c.text(rx, m.y + 128 * s + 3 * g, &format!("Mouse speed {}", self.mouse_speed), 0xFFFFFF, Font::Normal);
        for faster in [false, true] {
            let b = self.speed_button(faster);
            let live = if faster { self.mouse_speed < 10 } else { self.mouse_speed > 1 };
            let hover = live && b.contains(self.pointer.x as i32, self.pointer.y as i32);
            let (top, bottom) = match (live, hover) {
                (false, _) => (rgb(150, 160, 175), rgb(110, 120, 135)),
                (true, false) => (rgb(250, 252, 255), rgb(200, 220, 240)),
                (true, true) => (rgb(235, 248, 255), rgb(150, 205, 250)),
            };
            c.rounded(b, 3 * s, false, top, bottom, 250);
            c.rounded_outline(b, 3 * s, false, rgb(40, 60, 90), 255);
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            c.fill(Rect::new(cx - 7 * s, cy - s, 14 * s, 2 * s), rgb(20, 40, 80));
            if faster {
                c.fill(Rect::new(cx - s, cy - 7 * s, 2 * s, 14 * s), rgb(20, 40, 80));
            }
        }
        let b = self.speed_button(false);
        for i in 0..10 {
            let bar_h = (6 + 2 * i) * s;
            let bar = Rect::new(b.x + b.w + 6 * s + i * 11 * s, b.y + b.h - bar_h, 8 * s, bar_h);
            let on = (i as u64) < self.mouse_speed;
            c.fill(bar, if on { rgb(120, 210, 255) } else { rgb(70, 90, 120) });
        }
        // Control Panel, lit up under the pointer like the programs.
        let cp = self.cp_menu_item();
        if cp.contains(self.pointer.x as i32, self.pointer.y as i32) {
            c.rounded(cp, 3 * s, false, rgb(90, 150, 210), rgb(40, 90, 150), 230);
            c.rounded_outline(cp, 3 * s, false, rgb(170, 210, 245), 255);
        }
        c.text(cp.x + 10 * s, cp.y + (cp.h - Font::Normal.h(s)) / 2, "Control Panel", 0xFFFFFF, Font::Normal);
        let e = self.exit_button();
        let hover = e.contains(self.pointer.x as i32, self.pointer.y as i32);
        c.rounded(e, 3 * s, false, if hover { rgb(250, 180, 150) } else { rgb(230, 150, 120) }, rgb(170, 50, 25), 240);
        c.rounded_outline(e, 3 * s, false, rgb(80, 20, 10), 255);
        let label = "Exit to console";
        c.text(e.x + (e.w - Font::Normal.width(s, label)) / 2, e.y + (e.h - Font::Normal.h(s)) / 2, label, 0xFFFFFF, Font::Normal);
    }

    fn cursor(&self, c: &mut Canvas) {
        let (x0, y0, s) = (self.pointer.x as i32, self.pointer.y as i32, self.ui);
        if let Some(d) = self.file_drag.as_ref().filter(|d| d.active) {
            // What is dragged, and what dropping it here would do.
            let what = match d.items.as_slice() {
                [one] => split_path(&one.path).1,
                many => format!("{} items", many.len()),
            };
            let what: String = if what.chars().count() > 22 { what.chars().take(19).chain("...".chars()).collect() } else { what };
            let (text, ok) = match self.drop_target(x0, y0) {
                Some((_, name, action)) => {
                    let verb = match action {
                        DropAction::Move => "Move to",
                        DropAction::Copy => "Copy to",
                        DropAction::Recycle => "Move to",
                    };
                    let name: String = if name.chars().count() > 18 { name.chars().take(15).chain("...".chars()).collect() } else { name };
                    (format!("{}  {} {}", what, verb, name), true)
                }
                None => (what, false),
            };
            let w = Font::Normal.width(s, &text) + 30 * s;
            let r = Rect::new(x0 + 14 * s, y0 + 20 * s, w, 24 * s);
            c.rounded(r, 3 * s, false, rgb(250, 252, 255), rgb(222, 233, 246), 235);
            c.rounded_outline(r, 3 * s, false, rgb(120, 150, 190), 255);
            if d.items.iter().any(|it| it.is_dir) {
                c.folder(r.x + 6 * s, r.y + 6 * s, s);
            } else {
                c.page(r.x + 8 * s, r.y + 5 * s, s);
            }
            let ink = if ok { rgb(20, 40, 80) } else { rgb(120, 125, 135) };
            c.text(r.x + 24 * s, r.y + (r.h - Font::Normal.h(s)) / 2, &text, ink, Font::Normal);
        }
        let edges = match self.resize {
            Some((_, e, ..)) => Some(e),
            None if self.drag.is_none() => self.edges_at(x0, y0).map(|(_, e)| e),
            None => None,
        };
        if let Some(e) = edges {
            // A double-headed arrow along the direction the edge moves.
            let ux = if e.left || e.right { 1 } else { 0 };
            let uy = if e.top || e.bottom { 1 } else { 0 };
            let uy = if (e.left && e.bottom) || (e.right && e.top) { -uy } else { uy };
            let len = if ux != 0 && uy != 0 { 6 * s } else { 8 * s };
            for (size, color) in [(3 * s, 0xFFFFFF), (s, 0x000000)] {
                let o = (size - s) / 2;
                c.line(x0 - ux * len - o, y0 - uy * len - o, x0 + ux * len - o, y0 + uy * len - o, size, color);
                for sign in [1, -1] {
                    // At each tip, two short strokes back towards the middle,
                    // turned 45 degrees either way.
                    let (tx, ty) = (x0 + sign * ux * len, y0 + sign * uy * len);
                    let (bx, by) = (-sign * ux, -sign * uy);
                    for (hx, hy) in [(bx + by, by - bx), (bx - by, by + bx)] {
                        c.line(tx - o, ty - o, tx + hx * 3 * s - o, ty + hy * 3 * s - o, size, color);
                    }
                }
            }
            return;
        }
        for (row, line) in ARROW.iter().enumerate() {
            for (col, ch) in line.bytes().enumerate() {
                let color = match ch {
                    b'X' => 0x000000,
                    b'.' => 0xFFFFFF,
                    _ => continue,
                };
                c.fill(Rect::new(x0 + col as i32 * s, y0 + row as i32 * s, s, s), color);
            }
        }
    }

    fn draw(&self, c: &mut Canvas) {
        // A window's inside is opaque: when the area lies wholly inside one,
        // the wallpaper and the windows behind it are not drawn at all.
        let clip = c.clip;
        let first = (0..self.windows.len())
            .rev()
            .find(|&i| self.windows[i].shown() && self.client(&self.windows[i].rect).intersect(&clip) == clip);
        if first.is_none() {
            self.background(c);
            self.icons(c);
        }
        let top = self.top_kind();
        for win in self.windows[first.unwrap_or(0)..].iter().filter(|w| w.shown()) {
            self.window(c, win, Some(win.kind) == top);
        }
        self.snap_preview(c);
        self.taskbar_and_menu(c);
        self.popup_view(c);
        self.progress_view(c);
        self.cursor(c);
    }

    /// The area a window covers on screen, shadow included.
    fn window_area(&self, i: usize) -> Rect {
        let r = self.windows[i].rect;
        let m = 10 * self.ui;
        Rect::new(r.x - m, r.y - m, r.w + 2 * m, r.h + 2 * m)
    }

    /// What the pointer lights up at (x, y): the one caption button, taskbar
    /// button, icon, file row, key or menu item under it, so that moving the
    /// pointer redraws only that and not whole windows.
    fn hover_area(&self, x: i32, y: i32) -> Rect {
        let s = self.ui;
        let mut spots: Vec<Rect> = Vec::new();
        if let Some(p) = &self.popup {
            spots.extend((0..p.items.len()).map(|i| self.popup_item(i)));
            return spots.into_iter().find(|r| r.contains(x, y)).unwrap_or(Rect::EMPTY);
        }
        if self.menu {
            spots.extend((0..KINDS.len()).map(|i| if self.all_programs { self.all_item(i) } else { self.menu_item(i) }));
            spots.push(self.all_button());
            spots.push(self.cp_menu_item());
            spots.push(self.exit_button());
            spots.push(self.speed_button(false));
            spots.push(self.speed_button(true));
        }
        let (bx, by, br) = self.start_button();
        spots.push(Rect::new(bx - br - 2 * s, by - br - 2 * s, 2 * br + 4 * s, 2 * br + 4 * s));
        spots.extend((0..self.task_order().len()).map(|i| self.task_button(i)));
        spots.push(self.show_desktop());
        spots.extend((0..ICONS.len()).map(|i| self.icon_rect(i)));
        if let Some(top) = self.windows.iter().rposition(|w| w.shown() && w.rect.contains(x, y)) {
            let r = self.windows[top].rect;
            for which in [Caption::Minimize, Caption::Maximize, Caption::Close] {
                spots.push(self.caption(&r, which));
            }
            let client = self.client(&r);
            match self.windows[top].kind {
                Kind::Computer => spots.extend(self.files_spots(&client)),
                Kind::Notes => spots.push(self.notes_save(&client)),
                Kind::Photos => spots.extend([self.photo_button(&client, false), self.photo_button(&client, true)]),
                Kind::Music => {
                    spots.extend((0..4).map(|i| self.music_button(&client, i)));
                    let (room, first) = self.music_rows(&client);
                    let count = self.player.as_ref().map_or(0, |p| p.list.len());
                    spots.extend((0..room.min(count.saturating_sub(first))).map(|i| self.music_row(&client, i)));
                }
                Kind::ControlPanel => spots.extend(self.cp_controls(&client).into_iter().map(|(_, r)| r)),
                Kind::Paint => spots.extend(self.paint_spots(&client)),
                Kind::Calculator => {
                    for row in 0..5 {
                        for col in 0..4 {
                            spots.push(self.calc_key(&client, row, col));
                        }
                    }
                }
                _ => {}
            }
        }
        spots.into_iter().find(|r| r.contains(x, y)).unwrap_or(Rect::EMPTY)
    }

    // ---------------------------------------------------------------- input

    fn raise(&mut self, i: usize) -> usize {
        let w = self.windows.remove(i);
        self.windows.push(w);
        self.windows.len() - 1
    }

    /// Opens (or restores) a window and brings it to the front.
    fn bring(&mut self, kind: Kind) -> Rect {
        let at = self.index(kind);
        self.windows[at].open = true;
        self.windows[at].minimized = false;
        let top = self.raise(at);
        self.window_area(top).union(&self.taskbar())
    }

    /// Where a window dragged with the pointer at (x, y) snaps to: the whole
    /// work area at the top edge, the left or right half at the sides.
    fn snap_target(&self, x: i32, y: i32) -> Option<Rect> {
        let work_h = self.h - taskbar_h(self.ui);
        if y <= 0 {
            Some(Rect::new(0, 0, self.w, work_h))
        } else if x <= 0 {
            Some(Rect::new(0, 0, self.w / 2, work_h))
        } else if x >= self.w - 1 {
            Some(Rect::new(self.w - self.w / 2, 0, self.w / 2, work_h))
        } else {
            None
        }
    }

    fn snap_preview(&self, c: &mut Canvas) {
        if let Some(r) = self.snap {
            let s = self.ui;
            let r = Rect::new(r.x + 6 * s, r.y + 6 * s, r.w - 12 * s, r.h - 12 * s);
            c.rounded(r, 6 * s, false, rgb(200, 230, 255), rgb(120, 180, 240), 80);
            c.rounded_outline(r, 6 * s, false, 0xFFFFFF, 200);
        }
    }

    fn maximize(&mut self, i: usize) {
        let work = Rect::new(0, 0, self.w, self.h - taskbar_h(self.ui));
        let win = &mut self.windows[i];
        match win.restore.take() {
            Some(old) => win.rect = old,
            None => {
                win.restore = Some(win.rect);
                win.rect = work;
            }
        }
    }

    /// Left button pressed at (x, y). Returns the area to redraw.
    /// A left-button press at (x, y); `double` when the system timed it as
    /// the second press of a double-click.
    fn click(&mut self, x: i32, y: i32, double: bool) -> Rect {
        let mut dirty = Rect::EMPTY;
        if self.popup.is_some() {
            return self.popup_click(x, y);
        }
        let menu_was_open = self.menu;
        if self.menu {
            self.menu = false;
            dirty = dirty.union(&self.menu_rect());
            if self.all_button().contains(x, y) {
                self.menu = true;
                self.all_programs = !self.all_programs;
                if self.all_programs {
                    let rows: Vec<String> = self.all_list().iter().enumerate().map(|(i, &k)| {
                        let r = self.all_item(i);
                        format!("{} at {},{}", self.windows[self.index(k)].title, r.x + r.w / 2, r.y + r.h / 2)
                    }).collect();
                    let b = self.all_button();
                    println!("[desktop] All programs: {}; Back at {},{} | end", rows.join("; "), b.x + b.w / 2, b.y + b.h / 2);
                } else {
                    println!("[desktop] Start menu: back to the usual programs");
                }
                return dirty;
            }
            if self.all_programs {
                for (i, kind) in self.all_list().into_iter().enumerate() {
                    if self.all_item(i).contains(x, y) {
                        return dirty.union(&self.open(kind));
                    }
                }
            } else {
                for (i, &kind) in MENU.iter().enumerate() {
                    if self.menu_item(i).contains(x, y) {
                        return dirty.union(&self.open(kind));
                    }
                }
            }
            if self.cp_menu_item().contains(x, y) {
                return dirty.union(&self.open(Kind::ControlPanel));
            }
            if self.exit_button().contains(x, y) {
                self.quit = true;
                return dirty;
            }
            for faster in [false, true] {
                if self.speed_button(faster).contains(x, y) {
                    let want = if faster { self.mouse_speed + 1 } else { self.mouse_speed - 1 }.clamp(1, 10);
                    self.set_mouse_speed(want);
                    return dirty.union(&self.menu_rect());
                }
            }
            if self.menu_rect().contains(x, y) {
                // A click on the menu's empty parts keeps it open.
                self.menu = true;
                return dirty;
            }
        }
        let (ox, oy, or) = self.start_button();
        if (x - ox) * (x - ox) + (y - oy) * (y - oy) <= or * or {
            self.menu = !menu_was_open;
            self.all_programs = false;
            if self.menu {
                let ab = self.all_button();
                let cp = self.cp_menu_item();
                let music = self.menu_item(MENU.iter().position(|&k| k == Kind::Music).unwrap_or(0));
                let paint = self.menu_item(MENU.iter().position(|&k| k == Kind::Paint).unwrap_or(0));
                println!("[desktop] Start menu open; Control Panel at {},{}; Music at {},{}; Paint at {},{}; All programs at {},{}", cp.x + cp.w / 2, cp.y + cp.h / 2,
                    music.x + music.w / 2, music.y + music.h / 2, paint.x + paint.w / 2, paint.y + paint.h / 2, ab.x + ab.w / 2, ab.y + ab.h / 2);
            }
            return dirty.union(&self.menu_rect()).union(&self.taskbar());
        }
        if self.show_desktop().contains(x, y) {
            // Hide every window, or bring back the ones it hid.
            if self.peeked.is_empty() {
                for w in self.windows.iter_mut().filter(|w| w.shown()) {
                    w.minimized = true;
                    self.peeked.push(w.kind);
                }
            } else {
                for kind in core::mem::take(&mut self.peeked) {
                    let at = self.index(kind);
                    self.windows[at].minimized = false;
                }
            }
            return Rect::new(0, 0, self.w, self.h);
        }
        if self.taskbar().contains(x, y) {
            for (b, kind) in self.task_order().into_iter().enumerate() {
                if self.task_button(b).contains(x, y) {
                    let at = self.index(kind);
                    dirty = dirty.union(&self.window_area(at)).union(&self.taskbar());
                    if self.windows[at].shown() && self.top_kind() == Some(kind) {
                        self.windows[at].minimized = true;
                    } else {
                        dirty = dirty.union(&self.bring(kind));
                    }
                    return dirty;
                }
            }
            return dirty;
        }
        // The front-most window under the pointer.
        let hit = (0..self.windows.len()).rev().find(|&i| self.windows[i].shown() && self.windows[i].rect.contains(x, y));
        if let Some(i) = hit {
            // Bringing a window forward redraws it and the taskbar; a click
            // inside the window already in front redraws only what it changes.
            let in_front = self.top_kind() == Some(self.windows[i].kind);
            let i = self.raise(i);
            let r = self.windows[i].rect;
            if !in_front {
                dirty = dirty.union(&self.window_area(i)).union(&self.taskbar());
            }
            if self.caption(&r, Caption::Close).contains(x, y) {
                self.windows[i].open = false;
                println!("[desktop] closed {}", self.windows[i].title);
                match self.windows[i].kind {
                    Kind::Photos => self.photo = None,
                    Kind::ControlPanel => self.cp_page = CpPage::Home,
                    Kind::Music => {
                        if self.player.take().is_some_and(|p| p.deck.lock().playing) {
                            println!("[desktop] music stopped");
                        }
                    }
                    _ => {}
                }
                dirty = dirty.union(&self.window_area(i)).union(&self.taskbar());
            } else if self.caption(&r, Caption::Minimize).contains(x, y) {
                self.windows[i].minimized = true;
                dirty = dirty.union(&self.window_area(i)).union(&self.taskbar());
            } else if self.caption(&r, Caption::Maximize).contains(x, y) {
                self.maximize(i);
                dirty = dirty.union(&self.window_area(i));
            } else if let Some((_, edges)) = self.edges_at(x, y) {
                self.resize = Some((i, edges, r, x, y));
            } else if double && y < r.y + self.title_h() {
                self.drag = None;
                self.maximize(i);
                dirty = dirty.union(&self.window_area(i));
            } else if y < r.y + self.title_h() {
                self.drag = Some((i, x - r.x, y - r.y));
            } else if self.windows[i].kind == Kind::Notes && self.notes_save(&self.client(&r)).contains(x, y) {
                dirty = dirty.union(&self.save_notes());
            } else if self.windows[i].kind == Kind::Computer && self.client(&r).contains(x, y) {
                dirty = dirty.union(&self.files_click(x, y, double));
                dirty = dirty.union(&self.sync_preview());
            } else if self.windows[i].kind == Kind::Photos && self.client(&r).contains(x, y) {
                let client = self.client(&r);
                for next in [false, true] {
                    if self.photo_button(&client, next).contains(x, y) {
                        dirty = dirty.union(&self.step_photo(next));
                    }
                }
            } else if self.windows[i].kind == Kind::Paint && self.client(&r).contains(x, y) {
                let client = self.client(&r);
                dirty = dirty.union(&self.paint_click(&client, x, y));
            } else if self.windows[i].kind == Kind::ControlPanel && self.client(&r).contains(x, y) {
                let client = self.client(&r);
                dirty = dirty.union(&self.cp_click(&client, x, y, double));
            } else if self.windows[i].kind == Kind::Music && self.client(&r).contains(x, y) {
                let client = self.client(&r);
                let (room, first) = self.music_rows(&client);
                let count = self.player.as_ref().map_or(0, |p| p.list.len());
                let vol = self.music_volume(&client);
                if let Some(b) = (0..4).find(|&b| self.music_button(&client, b).contains(x, y)) {
                    dirty = dirty.union(&self.music_command(b));
                } else if let Some(row) = (0..room.min(count.saturating_sub(first))).find(|&i| self.music_row(&client, i).contains(x, y)) {
                    if let Some(p) = self.player.as_mut() {
                        p.at = first + row;
                    }
                    self.load_song(true);
                    dirty = dirty.union(&client);
                } else if Rect::new(vol.x - 8 * self.ui, vol.y - 10 * self.ui, vol.w + 16 * self.ui, vol.h + 20 * self.ui).contains(x, y) {
                    let along = ((x - vol.x).clamp(0, vol.w) * 100 / vol.w.max(1)) as u32;
                    self.set_volume(along, false);
                    dirty = dirty.union(&client);
                } else if self.music_bar(&client).contains(x, y) {
                    let bar = self.music_bar(&client);
                    if let Some(p) = self.player.as_mut() {
                        let mut d = p.deck.lock();
                        if let Some(song) = d.song.as_mut() {
                            let to = song.frames * (x - bar.x) as u64 / bar.w.max(1) as u64;
                            song.seek(to);
                            println!("[desktop] music: moved to {}", minutes(to / 48_000));
                            d.spare.clear();
                            drop(d);
                            p.heard = to;
                        }
                    }
                    dirty = dirty.union(&client);
                }
            } else if self.windows[i].kind == Kind::Calculator {
                let client = self.client(&r);
                for row in 0..5 {
                    for col in 0..4 {
                        if self.calc_key(&client, row, col).contains(x, y) {
                            let key = match CALC_KEYS[row][col] {
                                "<-" => 8,
                                "+/-" => b'~',
                                label => label.as_bytes()[0],
                            };
                            self.calc_key_pressed(key);
                            return dirty.union(&self.window_area(i));
                        }
                    }
                }
            }
            return dirty;
        }
        // The desktop itself: icons are picked with a click and opened
        // with a double-click.
        let before = self.icon;
        self.icon = (0..ICONS.len()).find(|&i| self.icon_rect(i).contains(x, y));
        for i in [before, self.icon].into_iter().flatten() {
            dirty = dirty.union(&self.icon_rect(i));
        }
        if let (true, Some(i)) = (double, self.icon) {
            dirty = dirty.union(&self.open(ICONS[i]));
        }
        dirty
    }

    /// Changes the pointer speed and keeps it in /AeroForge.ini for next time.
    fn set_mouse_speed(&mut self, speed: u64) {
        if let Ok(now) = aero::mouse_speed(Some(speed)) {
            self.mouse_speed = now;
        }
        let saved = self.save_settings();
        println!("[desktop] mouse speed {}{}", self.mouse_speed, if saved { ", saved" } else { "" });
    }

    fn calc_key_pressed(&mut self, key: u8) {
        if let Some(result) = self.calc.key(key) {
            println!("[desktop] calculator: {}", result);
        }
    }

    /// Opens a window from its icon or the Start menu.
    fn open(&mut self, kind: Kind) -> Rect {
        let at = self.index(kind);
        if kind == Kind::Computer && !self.windows[at].open {
            // A fresh start: one tab, one pane.
            self.tabs = alloc::vec![Tab::default()];
            self.tab = 0;
            self.other = None;
            self.right = false;
            self.files.back.clear();
            self.files.forward.clear();
            self.open_dir(String::new());
        }
        let r = self.windows[at].rect;
        println!("[desktop] opened {} at {},{} ({}x{})", self.windows[at].title, r.x, r.y, r.w, r.h);
        if kind == Kind::ControlPanel {
            self.cp_page = CpPage::Home;
            self.log_window(kind);
            self.bring(kind);
            self.cp_log();
            let at = self.index(kind);
            return self.window_area(at).union(&self.taskbar());
        }
        if kind == Kind::Music && self.player.is_none() {
            // Opened on its own: line up the songs in the Music folders, ready to play.
            let songs = find_songs();
            println!("[desktop] Music: found {} song{} in Music folders", songs.len(), if songs.len() == 1 { "" } else { "s" });
            if !songs.is_empty() {
                self.player = Some(Player::new(songs, 0));
                self.load_song(false);
            }
            self.log_window(kind);
        }
        if kind == Kind::Paint {
            self.open_paint();
        }
        if kind == Kind::Calculator {
            let k = self.calc_key(&self.client(&r), 3, 3);
            println!("[desktop] Calculator = key at {},{}", k.x + k.w / 2, k.y + k.h / 2);
        }
        self.bring(kind)
    }




    /// Writes Notes to its file (or /Notes.txt), then reads it back to be
    /// sure it reached the disk the way it was typed.
    fn save_notes(&mut self) -> Rect {
        if self.notes_cut {
            self.notes_status = String::from("Too long to save");
            return self.window_area(self.index(Kind::Notes));
        }
        let path = self.notes_path.clone().unwrap_or_else(|| String::from("/Notes.txt"));
        let data = self.notes.as_bytes();
        self.notes_status = match fs_write(&path, data) {
            Ok(_) => {
                let same = matches!(fs_read(&path, data.len() + 1), Ok(back) if back == data);
                println!("[desktop] saved {} ({} bytes, {})", path, data.len(), if same { "read back the same" } else { "read back different" });
                self.notes_path = Some(path);
                String::from(if same { "Saved" } else { "Saved, but it reads back different" })
            }
            Err(e) => {
                println!("[desktop] cannot save {} ({})", path, e);
                String::from(match e {
                    aero::E_RIGHTS => "Can't save here (read-only)",
                    aero::E_FULL => "Can't save: the disk is full",
                    aero::E_NOTFOUND => "Can't save: no disk there",
                    aero::E_CLOSED => "Can't save: the network drive is not connected",
                    _ => "Can't save that file",
                })
            }
        };
        self.window_area(self.index(Kind::Notes))
    }

    /// Opens a text file in Notes.
    fn open_file(&mut self, path: String) -> Rect {
        let buf = match fs_read(&path, 4096) {
            Ok(b) => b,
            Err(e) => {
                println!("[desktop] cannot read {} ({})", path, e);
                self.files.status = String::from("Can't read that file");
                return self.files_area();
            }
        };
        let n = buf.len();
        let text = &buf[..];
        if text.iter().any(|&b| b == 0 || (b < 32 && b != b'\n' && b != b'\r' && b != b'\t')) {
            self.files.status = String::from("Notes can only open text files");
            return self.files_area();
        }
        self.notes = String::from_utf8_lossy(text).replace('\r', "").replace('\t', "    ");
        // Only the start of a long file fits; saving that would cut the file.
        self.notes_cut = n >= 4096 || self.notes.len() > 3000;
        while self.notes.len() > 3000 {
            self.notes.pop();
        }
        println!("[desktop] opened {} in Notes ({} bytes)", path, n);
        self.notes_status = String::from(if self.notes_cut { "Too long to save" } else { "" });
        self.notes_path = Some(path);
        self.files_area().union(&self.bring(Kind::Notes))
    }


    /// The gamepad drives the desktop: the left stick moves the pointer (the
    /// right stick too, slowly, for fine aiming), A clicks, X right-clicks,
    /// B closes the Start menu or a right-click menu, Start (or Guide) opens
    /// the Start menu, and RB and LB switch between the open windows.
    fn gamepad(&mut self, now: u64) -> Rect {
        use aero::gamepad as pad;
        let mut dirty = Rect::EMPTY;
        // How far a stick may lean and still count as in the middle.
        const DEAD: i64 = 7000;
        let (mut held, mut left, mut right) = (0u16, (0i16, 0i16), (0i16, 0i16));
        let mut i = 0;
        while let Ok((st, n)) = pad::read(i) {
            let bit = 1u8 << i.min(7);
            let rest = st.xbuttons == 0 && st.thumbs.iter().all(|&v| (v as i64).abs() <= DEAD);
            if st.connected == 0 {
                self.pad_armed &= !bit;
            } else if rest && self.pad_armed & bit == 0 {
                self.pad_armed |= bit;
                println!("[desktop] gamepad: {} ready", st.name());
            }
            if st.connected != 0 && self.pad_armed & bit != 0 {
                held |= st.xbuttons;
                let [lx, ly, rx, ry] = st.thumbs;
                if (lx as i32).abs() + (ly as i32).abs() > (left.0 as i32).abs() + (left.1 as i32).abs() {
                    left = (lx, ly);
                }
                if (rx as i32).abs() + (ry as i32).abs() > (right.0 as i32).abs() + (right.1 as i32).abs() {
                    right = (rx, ry);
                }
            }
            i += 1;
            if i >= n.min(8) {
                break;
            }
        }
        let dt = if self.pad_us == 0 { 0 } else { now.saturating_sub(self.pad_us).min(50_000) as i64 };
        self.pad_us = now;
        if i == 0 {
            return dirty;
        }
        // Thousandths of a pixel this frame for one stick axis: nothing in
        // the middle (worn sticks drift), then faster the further it goes,
        // up to `top` pixels a second.
        let step = |v: i16, top: i64| -> i64 {
            let a = ((v as i64).abs() - DEAD).max(0);
            let span = 32767 - DEAD;
            (top * 1000 * a / span * a / span) * dt / 1_000_000 * (v as i64).signum()
        };
        let s = self.ui as i64;
        // Up on a stick is positive, up on the screen is not.
        self.pad_rest.0 += step(left.0, 1400 * s) + step(right.0, 200 * s);
        self.pad_rest.1 -= step(left.1, 1400 * s) + step(right.1, 200 * s);
        let dx = (self.pad_rest.0 / 1000) as i32;
        let dy = (self.pad_rest.1 / 1000) as i32;
        self.pad_rest.0 -= dx as i64 * 1000;
        self.pad_rest.1 -= dy as i64 * 1000;
        if [left.0, left.1, right.0, right.1].iter().all(|&v| (v as i64).abs() <= DEAD) {
            // Sticks back in the middle leave no movement owed.
            self.pad_rest = (0, 0);
            if self.pad_moving {
                self.pad_moving = false;
                if let Ok(p) = gfx().screen.pointer() {
                    println!("[desktop] gamepad: pointer at {},{}", p.x, p.y);
                }
            }
        } else {
            self.pad_moving = true;
        }
        let mouse = (held & pad::A != 0) as u8 | ((held & pad::X != 0) as u8) << 1;
        let buttons = (mouse != self.pad_mouse).then_some(mouse);
        if dx != 0 || dy != 0 || buttons.is_some() {
            if gfx().screen.move_pointer(dx, dy, buttons).is_ok() {
                self.pad_mouse = mouse;
            }
        }
        let pressed = held & !self.pad_held;
        self.pad_held = held;
        if pressed & (pad::START | pad::GUIDE) != 0 {
            println!("[desktop] gamepad: Start");
            let (ox, oy, _) = self.start_button();
            dirty = dirty.union(&self.click(ox, oy, false));
        }
        if pressed & pad::B != 0 {
            println!("[desktop] gamepad: B");
            if self.popup.is_some() {
                dirty = dirty.union(&self.popup_area());
                self.popup = None;
            } else if self.menu {
                self.menu = false;
                self.all_programs = false;
                dirty = dirty.union(&self.menu_rect()).union(&self.taskbar());
            }
        }
        if pressed & (pad::LEFT_SHOULDER | pad::RIGHT_SHOULDER) != 0 {
            dirty = dirty.union(&self.switch_window(pressed & pad::RIGHT_SHOULDER != 0));
        }
        dirty
    }

    /// The next open window to the front (`forward`, like Alt+Tab), or the
    /// one in front to the back.
    fn switch_window(&mut self, forward: bool) -> Rect {
        let full = Rect::new(0, 0, self.w, self.h);
        if forward {
            let top = self.windows.iter().rposition(|w| w.shown());
            let Some(i) = self.windows.iter().position(|w| w.open) else { return Rect::EMPTY };
            if Some(i) == top {
                return Rect::EMPTY;
            }
            self.windows[i].minimized = false;
            let at = self.raise(i);
            println!("[desktop] gamepad: switched to {}", self.windows[at].title);
        } else {
            let Some(t) = self.windows.iter().rposition(|w| w.shown()) else { return Rect::EMPTY };
            let w = self.windows.remove(t);
            self.windows.insert(0, w);
            println!("[desktop] gamepad: switched to {}", self.top().map_or("the desktop", |w| w.title));
        }
        full
    }

    fn typed(&mut self, keys: &[(u8, u8)]) -> Rect {
        let mut dirty = Rect::EMPTY;
        for &(k, mods) in keys {
            self.key_mods = mods;
            if self.popup.is_some() {
                // The menu takes the keys: Esc closes it, Enter or a
                // line's first letter... kept simple: anything closes it.
                dirty = dirty.union(&self.popup_area());
                self.popup = None;
                if k == 27 {
                    continue;
                }
            }
            // Esc gives the screen back, unless the Computer window is
            // in front and waiting for a new name or a Yes or No.
            let files_busy = self.files.rename.is_some()
                || self.files.confirm.is_some()
                || self.files.form.is_some()
                || self.files.query_focus
                || self.files.props.is_some();
            if self.top_kind() == Some(Kind::Computer) && (k != 27 || files_busy) {
                dirty = dirty.union(&self.files_key(k));
                dirty = dirty.union(&self.sync_preview());
                continue;
            }
            if k == 27 {
                self.quit = true;
                continue;
            }
            if self.top_kind() == Some(Kind::Photos) && (k == KEY_LEFT || k == KEY_RIGHT) {
                dirty = dirty.union(&self.step_photo(k == KEY_RIGHT));
                continue;
            }
            if self.top_kind() == Some(Kind::Music) && k == b' ' {
                dirty = dirty.union(&self.music_command(1));
                continue;
            }
            if self.top_kind() == Some(Kind::Paint) && matches!(k, 0x13 | 0x1a | 0x0e) {
                // Ctrl+S saves, Ctrl+Z takes back the last stroke, Ctrl+N starts again.
                let at = self.windows.len() - 1;
                let client = self.client(&self.windows[at].rect);
                dirty = dirty.union(&match k {
                    0x13 => self.save_paint(&client),
                    0x1a => self.paint_undo(&client),
                    _ => self.paint_new(&client),
                });
                continue;
            }
            if self.top_kind() == Some(Kind::Calculator) {
                self.calc_key_pressed(k);
                dirty = dirty.union(&self.window_area(self.windows.len() - 1));
                continue;
            }
            if self.top_kind() != Some(Kind::Notes) {
                continue;
            }
            match k {
                0x13 => {
                    // Ctrl+S
                    dirty = dirty.union(&self.save_notes());
                    continue;
                }
                8 => {
                    self.notes.pop();
                }
                b'\n' | 32..=126 => {
                    if self.notes.len() < 4000 {
                        self.notes.push(k as char);
                    }
                }
                _ => continue,
            }
            self.notes_status = String::from("Not saved");
            dirty = dirty.union(&self.window_area(self.windows.len() - 1));
        }
        dirty
    }
}

// --------------------------------------------------------- the file manager
//
// The Computer window, inside its client area: a navigation bar (Back,
// Forward, Up and the address as clickable parts), a command bar, a pane
// listing Computer and the drives at the left, then either the drives as
// tiles or a folder's files in columns (Name, Type, Size: click one to sort),
// and a status bar.

/// Keys with no character (see DHI_KEY_* in drivers/include/dhi.h).
const KEY_UP: u8 = 0x80;
const KEY_DOWN: u8 = 0x81;
const KEY_LEFT: u8 = 0x82;
const KEY_RIGHT: u8 = 0x83;
const KEY_HOME: u8 = 0x84;
const KEY_END: u8 = 0x85;
const KEY_PGUP: u8 = 0x86;
const KEY_PGDN: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;
const KEY_F2: u8 = 0x89;
const KEY_F5: u8 = 0x8A;

/// How much of a file is copied at a time.
const COPY_CHUNK: usize = 1024 * 1024;

/// What copy_tree returns when Cancel was pressed.
const CANCELLED: &str = "Cancelled";

/// Characters Windows does not allow in names (AeroForge keeps to the same).
const BAD_NAME_CHARS: &str = "\\/:*?\"<>|";

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{}{}", dir, name) } else { format!("{}/{}", dir, name) }
}

/// The folder a path is in, and its last part.
fn split_path(path: &str) -> (String, String) {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => (String::from("/"), String::from(&t[1..])),
        Some(i) => (String::from(&t[..i]), String::from(&t[i + 1..])),
        None => (String::from("/"), String::from(t)),
    }
}

/// Whether `path` is a drive's Recycle Bin folder.
fn bin_folder(path: &str) -> bool {
    split_path(path).1.eq_ignore_ascii_case(BIN_DIR)
}

/// What a failed file system call means, for the status bar.
fn fs_error_text(e: i64) -> &'static str {
    match e {
        aero::E_RIGHTS => "access is denied (read-only)",
        aero::E_FULL => "the disk is full",
        aero::E_NOTFOUND => "it is not there any more",
        aero::E_EXISTS => "something with that name is already there",
        aero::E_CLOSED => "the network drive is not connected",
        _ => "the disk refused it",
    }
}

// ------------------------------------------------------- network drives

/// A picture shown in Photos, and the others in its folder.
struct Photo {
    list: Vec<String>,
    at: usize,
    /// The picture, made to fit the screen; its size before that.
    img: Option<aero::media::Image>,
    size: (usize, usize),
    error: String,
}

/// The song playing in Music, and the others in its folder.
struct Player {
    list: Vec<String>,
    at: usize,
    /// Shared with the thread that feeds the sound card.
    deck: Arc<aero::sync::Mutex<Deck>>,
    feeder: Option<aero::thread::JoinHandle<()>>,
    /// 48 kHz frames heard so far (made, less those still queued).
    heard: u64,
    error: String,
}

/// What the feeding thread plays.
struct Deck {
    song: Option<aero::media::Song>,
    playing: bool,
    /// Samples made that the audio queue had no room for yet.
    spare: Vec<i16>,
    /// Set by the feeding thread when there is no sound card to write to.
    no_output: bool,
    quit: bool,
}

impl Player {
    fn new(list: Vec<String>, at: usize) -> Player {
        let deck = Arc::new(aero::sync::Mutex::new(Deck { song: None, playing: false, spare: Vec::new(), no_output: false, quit: false }));
        let shared = deck.clone();
        let feeder = aero::thread::spawn(move || feed_music(shared)).ok();
        Player { list, at, deck, feeder, heard: 0, error: String::new() }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.deck.lock().quit = true;
        if let Some(t) = self.feeder.take() {
            t.join();
        }
    }
}

/// Music's own thread: keeps the audio queue about a third of a second
/// ahead while a song plays. It runs at high priority so that a busy
/// desktop (drawing, a big folder) does not leave gaps in the sound.
fn feed_music(deck: Arc<aero::sync::Mutex<Deck>>) {
    let _ = aero::thread::set_priority(aero::thread::Priority::High);
    loop {
        {
            let mut d = deck.lock();
            if d.quit {
                return;
            }
            if d.playing {
                feed_deck(&mut d);
            }
        }
        aero::sleep_ms(10);
    }
}

/// Plays a stereo wave to the end, as room comes up on the sound card.
fn play_wave(mut wave: Vec<i16>) {
    let _ = aero::thread::set_priority(aero::thread::Priority::High);
    while !wave.is_empty() {
        match aero::audio::write(&wave) {
            Ok(n) => {
                wave.drain(..2 * n);
                if n == 0 {
                    aero::sleep_ms(10);
                }
            }
            Err(_) => {
                println!("[desktop] test sound: no sound output");
                return;
            }
        }
    }
}

fn feed_deck(d: &mut Deck) {
    let queued = match aero::audio::queued() {
        Ok(n) => n as u64,
        Err(_) => {
            d.playing = false;
            d.no_output = true;
            return;
        }
    };
    let mut room = (48_000 / 3u64).saturating_sub(queued);
    while room > 0 {
        if d.spare.is_empty() {
            let Some(song) = d.song.as_mut() else { break };
            let mut buf = alloc::vec![0i16; 2 * 2400];
            let n = song.fill(&mut buf);
            if n == 0 {
                break;
            }
            buf.truncate(2 * n);
            d.spare = buf;
        }
        match aero::audio::write(&d.spare) {
            Ok(0) => break,
            Ok(n) => {
                d.spare.drain(..2 * n);
                room = room.saturating_sub(n as u64);
            }
            Err(_) => break,
        }
    }
}

/// The biggest picture or song opened (read whole into memory).
const MAX_MEDIA: u64 = 96 * 1024 * 1024;

/// How a copy or move is going, for its progress window.
struct Progress {
    moving: bool,
    /// How many things were picked, and the folder they go to.
    count: usize,
    to: String,
    /// The file being copied now.
    file: String,
    done: u64,
    total: u64,
    started_us: u64,
    drawn_us: u64,
    shown: bool,
    cancel: bool,
}

/// The screen and the frame drawn for it. The desktop has one thread; a
/// copy draws its progress from inside a click, so they live here.
struct Gfx {
    screen: Screen,
    canvas: Canvas,
}
struct GfxCell(core::cell::UnsafeCell<Option<Gfx>>);
unsafe impl Sync for GfxCell {}
static GFX: GfxCell = GfxCell(core::cell::UnsafeCell::new(None));

fn gfx() -> &'static mut Gfx {
    // SAFETY: only the desktop's one thread touches it, and no reference
    // is kept across calls.
    unsafe { (*GFX.0.get()).as_mut().expect("screen not set up") }
}

/// A mapped network drive: a share on another computer (SMB 2).
struct NetShare {
    /// "192.168.1.20", "nas" or "10.0.2.2:4450".
    host: String,
    share: String,
    user: String,
    /// Kept for this session only, to sign in again if the link drops;
    /// never saved to the disk.
    password: Option<String>,
    client: Option<aero::smb::Client>,
    letter: char,
}

impl NetShare {
    /// Its path in the Computer window: "//host/share".
    fn root(&self) -> String {
        format!("//{}/{}", self.host, self.share)
    }
    /// The way Windows writes it: "\\host\share".
    fn unc(&self) -> String {
        format!("\\\\{}\\{}", self.host, self.share)
    }
    fn name(&self) -> String {
        format!("{} on {} ({}:)", self.share, self.host, self.letter)
    }
}

/// The mapped network drives. The desktop has one thread, so a plain cell
/// does; the free functions below (copy, delete, ...) reach it too.
struct NetCell(core::cell::UnsafeCell<Vec<NetShare>>);
unsafe impl Sync for NetCell {}
static NET: NetCell = NetCell(core::cell::UnsafeCell::new(Vec::new()));

fn shares() -> &'static mut Vec<NetShare> {
    // SAFETY: only the desktop's one thread touches it, and no reference
    // is kept across calls.
    unsafe { &mut *NET.0.get() }
}

/// The mapped share a "//host/share/..." path is on, and the path inside it.
fn net_path(path: &str) -> Option<(usize, String)> {
    let rest = path.strip_prefix("//")?;
    let mut parts = rest.splitn(3, '/');
    let (host, share) = (parts.next()?, parts.next()?);
    let inner = String::from(parts.next().unwrap_or(""));
    let i = shares().iter().position(|s| s.host.eq_ignore_ascii_case(host) && s.share.eq_ignore_ascii_case(share))?;
    Some((i, inner))
}

/// Runs `f` on share `i`'s connection, signing in again once if the link dropped.
fn with_share<T>(i: usize, f: impl Fn(&mut aero::smb::Client) -> Result<T, i64>) -> Result<T, i64> {
    let s = &mut shares()[i];
    if let Some(c) = s.client.as_mut() {
        match f(c) {
            Err(aero::E_CLOSED) | Err(aero::E_TIMEDOUT) => s.client = None,
            r => return r,
        }
    }
    let Some(password) = s.password.clone() else { return Err(aero::E_CLOSED) };
    match aero::smb::Client::connect(&s.host, &s.share, &s.user, &password) {
        Ok(c) => {
            println!("[desktop] signed in to {} again", s.unc());
            f(s.client.insert(c))
        }
        Err(msg) => {
            println!("[desktop] {}: {}", s.unc(), msg);
            Err(aero::E_CLOSED)
        }
    }
}

// Files on the local drives or on network drives, by path.

fn fs_list(path: &str) -> Result<Vec<aero::DirEntry>, i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.list(&inner)),
        None => aero::list_dir(path),
    }
}

fn fs_read(path: &str, limit: usize) -> Result<Vec<u8>, i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.read(&inner, limit)),
        None => {
            let mut buf = alloc::vec![0u8; limit];
            let n = aero::read_file(path, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        }
    }
}

/// Up to `len` bytes of a file from byte `offset`.
fn fs_read_at(path: &str, offset: u64, len: usize) -> Result<Vec<u8>, i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.read_at(&inner, offset, len)),
        None => {
            let mut buf = alloc::vec![0u8; len];
            let n = aero::read_file_at(path, offset, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        }
    }
}

/// Adds `data` to the end of a file.
fn fs_append(path: &str, data: &[u8]) -> Result<(), i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.append(&inner, data)),
        None => aero::append_file(path, data).map(|_| ()),
    }
}

fn fs_write(path: &str, data: &[u8]) -> Result<(), i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.write(&inner, data)),
        None => aero::write_file(path, data).map(|_| ()),
    }
}

fn fs_mkdir(path: &str) -> Result<(), i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.create_dir(&inner)),
        None => aero::create_dir(path),
    }
}

fn fs_delete(path: &str, is_dir: bool) -> Result<(), i64> {
    match net_path(path) {
        Some((i, inner)) => with_share(i, |c| c.delete(&inner, is_dir)),
        None => aero::delete_file(path),
    }
}

/// Renames in place when both paths are on the same drive or network
/// share, without copying. None when it can't be done that way.
fn fs_rename(from: &str, to: &str) -> Option<Result<(), i64>> {
    match (net_path(from), net_path(to)) {
        (Some((i, a)), Some((j, b))) => (i == j).then(|| with_share(i, |c| c.rename(&a, &b))),
        (None, None) => match aero::rename(from, to) {
            Err(aero::E_XDEV) => None,
            r => Some(r),
        },
        _ => None,
    }
}

/// Hears how a copy is going: the file being copied and the bytes just
/// copied of it; says whether to go on (false once Cancel is pressed).
type Tick<'a> = &'a mut dyn FnMut(&str, u64) -> bool;

/// Moves a file or folder: a rename on one drive or network share, else
/// copy and delete. A cancelled move leaves the original where it was.
fn move_tree(from: &str, to: &str, is_dir: bool, tick: Tick) -> Result<(), String> {
    if let Some(r) = fs_rename(from, to) {
        return r.map_err(|e| format!("Can't move {}: {}", from, fs_error_text(e)));
    }
    copy_tree(from, to, is_dir, tick)?;
    delete_tree(from, is_dir).map(|_| ())
}

/// Copies a file, or a folder and all it holds, to `to`. Returns how many
/// files it copied.
fn copy_tree(from: &str, to: &str, is_dir: bool, tick: Tick) -> Result<usize, String> {
    if is_dir {
        fs_mkdir(to).map_err(|e| format!("Can't make {}: {}", to, fs_error_text(e)))?;
        let mut n = 0;
        for e in fs_list(from).map_err(|e| format!("Can't read {}: {}", from, fs_error_text(e)))? {
            n += copy_tree(&join(from, &e.name), &join(to, &e.name), e.is_dir, &mut *tick)?;
        }
        return Ok(n);
    }
    copy_file(from, to, tick)?;
    Ok(1)
}

/// Copies one file of any size, a piece at a time: the first piece makes
/// the new file and the rest are added to its end. Cancelled part way,
/// the part copied is deleted again.
fn copy_file(from: &str, to: &str, tick: Tick) -> Result<(), String> {
    if !tick(from, 0) {
        return Err(String::from(CANCELLED));
    }
    let mut at = 0u64;
    loop {
        let piece = fs_read_at(from, at, COPY_CHUNK).map_err(|e| format!("Can't read {}: {}", from, fs_error_text(e)))?;
        let wrote = if at == 0 {
            fs_write(to, &piece)
        } else if piece.is_empty() {
            Ok(())
        } else {
            fs_append(to, &piece)
        };
        if let Err(e) = wrote {
            if at > 0 {
                let _ = fs_delete(to, false);
            }
            return Err(format!("Can't write {}: {}", to, fs_error_text(e)));
        }
        at += piece.len() as u64;
        let go_on = tick(from, piece.len() as u64);
        if piece.len() < COPY_CHUNK {
            return Ok(());
        }
        if !go_on {
            let _ = fs_delete(to, false);
            return Err(String::from(CANCELLED));
        }
    }
}

/// Reads a whole file, a piece at a time (up to MAX_MEDIA).
fn read_whole(path: &str) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    loop {
        let piece = fs_read_at(path, data.len() as u64, COPY_CHUNK).map_err(|e| format!("Can't read it: {}", fs_error_text(e)))?;
        data.extend_from_slice(&piece);
        if piece.len() < COPY_CHUNK {
            return Ok(data);
        }
        if data.len() as u64 >= MAX_MEDIA {
            return Err(String::from("It is too big to open here"));
        }
    }
}

/// The files of `kind` in the folder `path` is in, by name, and where
/// `path` is among them.
fn siblings(path: &str, kind: FileKind) -> (Vec<String>, usize) {
    let (dir, name) = split_path(path);
    let mut names: Vec<String> = fs_list(&dir).unwrap_or_default().into_iter().filter(|e| !e.is_dir && file_kind(&e.name) == kind).map(|e| e.name).collect();
    names.sort_by_key(|n| n.to_lowercase());
    if !names.iter().any(|n| *n == name) {
        names.push(name.clone());
    }
    let at = names.iter().position(|n| *n == name).unwrap_or(0);
    (names.iter().map(|n| join(&dir, n)).collect(), at)
}

/// The songs in the Music folder at the top of each drive and the folders in it,
/// folder by folder in name order.
fn find_songs() -> Vec<String> {
    fn walk(dir: &str, depth: u32, out: &mut Vec<String>) {
        let Ok(mut entries) = fs_list(dir) else { return };
        entries.sort_by_key(|e| e.name.to_lowercase());
        for e in entries.iter().filter(|e| !e.is_dir && file_kind(&e.name) == FileKind::Sound) {
            if out.len() < 500 {
                out.push(join(dir, &e.name));
            }
        }
        if depth < 3 {
            for e in entries.iter().filter(|e| e.is_dir) {
                walk(&join(dir, &e.name), depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for v in aero::volumes().unwrap_or_default() {
        if !v.path.starts_with("//") {
            walk(&join(&v.path, "Music"), 0, &mut out);
        }
    }
    out
}

/// 75 seconds as "1:15".
fn minutes(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Deletes a file, or a folder and all it holds. Returns how many things it deleted.
fn delete_tree(path: &str, is_dir: bool) -> Result<usize, String> {
    let mut n = 0;
    if is_dir {
        for e in fs_list(path).map_err(|e| format!("Can't read {}: {}", path, fs_error_text(e)))? {
            n += delete_tree(&join(path, &e.name), e.is_dir)?;
        }
    }
    fs_delete(path, is_dir).map_err(|e| format!("Can't delete {}: {}", path, fs_error_text(e)))?;
    Ok(n + 1)
}

/// What a file is, for its icon.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Text,
    Settings,
    Picture,
    Sound,
    Program,
    Archive,
    Save,
    Disc,
    Other,
}

fn file_kind(name: &str) -> FileKind {
    let ext = match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_ascii_lowercase(),
        _ => return FileKind::Other,
    };
    match ext.as_str() {
        "txt" | "log" | "md" | "csv" => FileKind::Text,
        "ini" | "cfg" | "conf" => FileKind::Settings,
        "png" | "jpg" | "jpeg" | "bmp" | "gif" => FileKind::Picture,
        "wav" | "mp3" | "ogg" | "flac" => FileKind::Sound,
        "exe" | "elf" => FileKind::Program,
        "zip" | "7z" | "gz" | "tar" => FileKind::Archive,
        "sav" => FileKind::Save,
        "iso" | "img" => FileKind::Disc,
        _ => FileKind::Other,
    }
}

/// The Type column: "File folder", "Text Document", "PNG File", ...
fn type_text(e: &aero::DirEntry) -> String {
    if e.is_dir {
        return String::from("File folder");
    }
    let ext = match e.name.rfind('.') {
        Some(i) if i > 0 => e.name[i + 1..].to_ascii_lowercase(),
        _ => return String::from("File"),
    };
    String::from(match ext.as_str() {
        "txt" | "log" => "Text Document",
        "md" => "Markdown File",
        "ini" | "cfg" | "conf" => "Settings File",
        "png" | "jpg" | "jpeg" | "bmp" | "gif" => "Picture",
        "wav" | "mp3" | "ogg" | "flac" => "Sound",
        "exe" | "elf" => "Program",
        "zip" | "7z" | "gz" | "tar" => "Compressed Folder",
        "sav" => "Saved Game",
        "iso" | "img" => "Disc Image",
        _ => return format!("{} File", ext.to_ascii_uppercase()),
    })
}

/// A listing's last-written time (YYYYMMDDhhmmss) as Windows shows it:
/// "10/9/2026 9:31 PM".
fn date_text(packed: u64) -> String {
    let part = |div: u64, m: u64| packed / div % m;
    let (year, month, day) = (packed / 10_000_000_000, part(100_000_000, 100), part(1_000_000, 100));
    let (hour, minute) = (part(10_000, 100), part(100, 100));
    let h12 = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{}/{}/{} {}:{:02} {}", month, day, year, h12, minute, if hour < 12 { "AM" } else { "PM" })
}

/// A free or total size for a drive: "1.9 GB", "512 MB".
fn space_text(bytes: u64) -> String {
    let (unit, name) = if bytes >= 1 << 40 {
        (1u64 << 40, "TB")
    } else if bytes >= 1 << 30 {
        (1 << 30, "GB")
    } else if bytes >= 1 << 20 {
        (1 << 20, "MB")
    } else {
        return format!("{} KB", bytes.div_ceil(1024));
    };
    let tenths = bytes * 10 / unit;
    if tenths >= 100 { format!("{} {}", tenths / 10, name) } else { format!("{}.{} {}", tenths / 10, tenths % 10, name) }
}

/// A name not yet in `taken`: `name`, else "name - Copy", "name - Copy (2)", ...
/// (or "New folder (2)" for new folders), keeping the extension at the end.
fn free_name(name: &str, taken: &[&str], is_dir: bool, copy: bool) -> String {
    let is_taken = |n: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if !is_taken(name) {
        return String::from(name);
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !is_dir => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1.. {
        let candidate = match (copy, n) {
            (true, 1) => format!("{} - Copy{}", stem, ext),
            (true, n) => format!("{} - Copy ({}){}", stem, n, ext),
            (false, n) => format!("{} ({}){}", stem, n + 1, ext),
        };
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

impl Canvas {
    /// A hard drive, 32 x 22 at scale 1: a grey box with a green light.
    /// A network drive: the drive on a cable and a green network bar.
    fn net_drive(&mut self, x: i32, y: i32, s: i32, connected: bool) {
        self.icon("netdrive", x - 2 * s, y - 8 * s, 36 * s);
        if !connected {
            // Not connected: the green bar at the bottom turns red.
            self.fill(Rect::new(x + 5 * s, y + 21 * s, 22 * s, 2 * s), rgb(190, 50, 40));
        }
    }

    fn drive(&mut self, x: i32, y: i32, s: i32, removable: bool) {
        self.icon(if removable { "usb" } else { "drive" }, x - 2 * s, y - 4 * s, 36 * s);
    }

    /// The small drive for the navigation pane, 16 x 12 at scale 1.
    fn small_drive(&mut self, x: i32, y: i32, s: i32, network: bool) {
        self.icon(if network { "netdrive" } else { "drive" }, x - s, y - 3 * s, 18 * s);
    }

    /// A small wastebasket for the Recycle Bin, 14 x 16 at scale 1.
    fn small_bin(&mut self, x: i32, y: i32, s: i32) {
        self.icon("bin", x - 2 * s, y - s, 18 * s);
    }

    /// A small screen for the navigation pane, 16 x 14 at scale 1.
    fn small_computer(&mut self, x: i32, y: i32, s: i32) {
        self.icon("computer", x - s, y - 2 * s, 18 * s);
    }
}

impl Desktop {
    fn files_client(&self) -> Rect {
        self.client(&self.windows[self.index(Kind::Computer)].rect)
    }
    /// The Computer window's area, to redraw it.
    /// The inside of the Computer window, which is all that its own
    /// changes redraw.
    fn files_area(&self) -> Rect {
        self.client(&self.windows[self.index(Kind::Computer)].rect)
    }
    fn files_nav(&self, c: &Rect) -> Rect {
        Rect::new(c.x, c.y, c.w, 40 * self.ui + grow(self.ui))
    }
    /// Back (0), Forward (1) and Up (2): centre and radius.
    fn files_arrow(&self, c: &Rect, i: i32) -> (i32, i32, i32) {
        let s = self.ui;
        (c.x + 20 * s + i * 30 * s, c.y + 20 * s + grow(s) / 2, if i == 2 { 10 * s } else { 12 * s })
    }
    fn files_address(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let search = self.files_search(c);
        Rect::new(c.x + 112 * s, c.y + 6 * s, search.x - 8 * s - (c.x + 112 * s), 28 * s + grow(s))
    }
    /// The search box, right of the address, as in Windows 7.
    fn files_search(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let w = (c.w / 4).clamp(120 * s, 240 * s);
        Rect::new(c.x + c.w - 8 * s - w, c.y + 6 * s, w, 28 * s + grow(s))
    }
    /// The tabs, under the address.
    fn files_tabs(&self, c: &Rect) -> Rect {
        let g = grow(self.ui);
        Rect::new(c.x, c.y + 40 * self.ui + g, c.w, 30 * self.ui + g)
    }
    /// Tab `i`'s button in the strip, and its close (x) box.
    fn tab_rect(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let strip = self.files_tabs(c);
        let room = strip.w - 8 * s - self.new_tab_button(c).w - self.panes_button(c).w - self.preview_button(c).w - self.view_button(c).w - 28 * s;
        let w = (room / self.tabs.len().max(1) as i32).min(190 * s);
        Rect::new(strip.x + 6 * s + i as i32 * w, strip.y + 4 * s, w - 4 * s, strip.h - 4 * s)
    }
    fn tab_close(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let t = self.tab_rect(c, i);
        Rect::new(t.x + t.w - 20 * s, t.y + (t.h - 16 * s) / 2, 16 * s, 16 * s)
    }
    /// "+": a new tab, right after the last one.
    fn new_tab_button(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let strip = self.files_tabs(c);
        let n = self.tabs.len() as i32;
        let w = 26 * s;
        // Placed after the tabs; their width depends on the strip, not on this.
        let room = strip.w - 8 * s - w - self.panes_button(c).w - self.preview_button(c).w - self.view_button(c).w - 28 * s;
        let tab_w = (room / n.max(1)).min(190 * s);
        Rect::new(strip.x + 6 * s + n * tab_w, strip.y + 5 * s, w, strip.h - 8 * s)
    }
    /// "Two panes", at the right of the tab strip.
    fn panes_button(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let strip = self.files_tabs(c);
        let w = Font::Normal.width(s, "Two panes") + 34 * s;
        Rect::new(strip.x + strip.w - w - 8 * s, strip.y + 4 * s, w, strip.h - 7 * s)
    }
    /// "Preview", left of "Two panes".
    fn preview_button(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let p = self.panes_button(c);
        let w = Font::Normal.width(s, "Preview") + 34 * s;
        Rect::new(p.x - 6 * s - w, p.y, w, p.h)
    }
    /// "Large icons" / "Details", left of "Preview".
    fn view_button(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let p = self.preview_button(c);
        let w = Font::Normal.width(s, "Large icons") + 34 * s;
        Rect::new(p.x - 6 * s - w, p.y, w, p.h)
    }
    fn files_commands(&self, c: &Rect) -> Rect {
        let g = grow(self.ui);
        Rect::new(c.x, c.y + 70 * self.ui + 2 * g, c.w, 34 * self.ui + g)
    }
    fn files_status_bar(&self, c: &Rect) -> Rect {
        let g = grow(self.ui);
        Rect::new(c.x, c.y + c.h - 28 * self.ui - g, c.w, 28 * self.ui + g)
    }
    fn files_body(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let g = grow(s);
        Rect::new(c.x, c.y + 104 * s + 3 * g, c.w, c.h - 104 * s - 28 * s - 4 * g)
    }
    fn files_pane(&self, c: &Rect) -> Rect {
        let b = self.files_body(c);
        Rect::new(b.x, b.y, (240 * self.ui + 4 * grow(self.ui)).min(b.w / 3), b.h)
    }
    /// Right of the navigation pane: the folder shown, or both sides,
    /// then the preview pane if it is on.
    fn files_right(&self, c: &Rect) -> Rect {
        let b = self.files_body(c);
        let p = self.files_pane(c);
        Rect::new(p.x + p.w + 1, b.y, b.w - p.w - 1, b.h)
    }
    fn files_main(&self, c: &Rect) -> Rect {
        let m = self.files_right(c);
        let p = self.preview_rect(c);
        if p.is_empty() { m } else { Rect::new(m.x, m.y, p.x - 1 - m.x, m.h) }
    }
    fn preview_rect(&self, c: &Rect) -> Rect {
        if !self.preview_on {
            return Rect::EMPTY;
        }
        let s = self.ui;
        let m = self.files_right(c);
        let w = (m.w * 9 / 20).clamp(270 * s, 460 * s).min(m.w / 2);
        Rect::new(m.x + m.w - w, m.y, w, m.h)
    }
    /// Its Text and Hex buttons, at the top right.
    fn preview_mode(&self, c: &Rect, hex: bool) -> Rect {
        let s = self.ui;
        let p = self.preview_rect(c);
        let w = Font::Normal.width(s, "Text") + 14 * s;
        let x = p.x + p.w - 8 * s - 2 * w;
        Rect::new(if hex { x + w } else { x }, p.y + 4 * s, w, 22 * s)
    }
    /// Where the text or bytes go, under the name, type and size.
    fn preview_body(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let p = self.preview_rect(c);
        let top = 30 * s + 3 * (Font::Normal.h(s) + 4 * s) + 8 * s;
        Rect::new(p.x + 8 * s, p.y + top, p.w - 8 * s - 16 * s, (p.h - top - 4 * s).max(0))
    }
    fn preview_bar(&self, c: &Rect) -> Rect {
        let b = self.preview_body(c);
        Rect::new(b.x + b.w, b.y, 16 * self.ui, b.h)
    }
    fn preview_line_h(&self) -> i32 {
        Font::Tiny.h(self.ui) + 2 * self.ui
    }
    fn preview_lines_shown(&self, c: &Rect) -> usize {
        (self.preview_body(c).h / self.preview_line_h()).max(1) as usize
    }
    /// Bytes per hex row (16, 8 or 4, as many as fit), and whether the
    /// characters fit after them too.
    fn hex_layout(&self, c: &Rect) -> (usize, bool) {
        let cols = (self.preview_body(c).w / Font::Tiny.w(self.ui).max(1)) as usize;
        [(16, true), (8, true), (8, false), (4, true)].into_iter().find(|&(n, chars)| 5 + 3 * n + if chars { 2 + n } else { 0 } <= cols).unwrap_or((4, false))
    }
    fn hex_width(&self, c: &Rect) -> usize {
        self.hex_layout(c).0
    }
    /// How many lines the preview has, as shown now.
    fn preview_total(&self, c: &Rect) -> usize {
        match &self.preview {
            Some(p) if p.hex => p.data.len().div_ceil(self.hex_width(c)),
            Some(p) => p.lines.len(),
            None => 0,
        }
    }
    /// In two-pane mode, the left (false) or right (true) side, with its
    /// path along the top.
    fn side(&self, c: &Rect, right: bool) -> Rect {
        let m = self.files_main(c);
        let half = (m.w - 2 * self.ui) / 2;
        if right { Rect::new(m.x + m.w - half, m.y, half, m.h) } else { Rect::new(m.x, m.y, half, m.h) }
    }
    fn side_header(&self, c: &Rect, right: bool) -> Rect {
        let r = self.side(c, right);
        Rect::new(r.x, r.y, r.w, 26 * self.ui)
    }
    /// Where the folder in use is shown.
    fn files_content(&self, c: &Rect) -> Rect {
        if self.other.is_none() {
            return self.files_main(c);
        }
        self.side_content(c, self.right)
    }
    /// Where the other side's folder is shown, in two-pane mode.
    fn other_content(&self, c: &Rect) -> Rect {
        self.side_content(c, !self.right)
    }
    fn side_content(&self, c: &Rect, right: bool) -> Rect {
        let r = self.side(c, right);
        let top = 26 * self.ui;
        Rect::new(r.x, r.y + top, r.w, r.h - top)
    }
    /// The pane's rows: the Recycle Bin, then Computer's tree. When they
    /// don't all fit, the list starts far enough down to show the place in use.
    fn pane_item(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let p = self.files_pane(c);
        let first = self.pane_first(c);
        if i < first {
            return Rect::EMPTY;
        }
        let r = Rect::new(p.x, p.y + 6 * s + (i - first) as i32 * 28 * s, p.w, 28 * s);
        if r.y + r.h > p.y + p.h { Rect::EMPTY } else { r }
    }
    fn pane_first(&self, c: &Rect) -> usize {
        let s = self.ui;
        let p = self.files_pane(c);
        let fits = ((p.h - 6 * s) / (28 * s)).max(1) as usize;
        let rows = self.places();
        if rows.len() <= fits {
            return 0;
        }
        let here = rows.iter().position(|r| r.kind != Place::Computer && r.path.eq_ignore_ascii_case(&self.files.path)).unwrap_or(0);
        (here + 3).saturating_sub(fits).min(rows.len() - fits)
    }
    /// A tree row's arrow.
    fn pane_arrow(&self, c: &Rect, i: usize, depth: i32) -> Rect {
        let s = self.ui;
        let r = self.pane_item(c, i);
        if r.is_empty() {
            return r;
        }
        Rect::new(r.x + 2 * s + depth * 14 * s, r.y, 16 * s, r.h)
    }
    // A folder list's parts, inside content area `k`.
    fn head_in(&self, k: &Rect) -> Rect {
        Rect::new(k.x, k.y, k.w, 28 * self.ui)
    }
    fn rows_in(&self, k: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(k.x, k.y + 28 * s, k.w - 16 * s, k.h - 28 * s)
    }
    fn scrollbar_in(&self, k: &Rect) -> Rect {
        let r = self.rows_in(k);
        Rect::new(r.x + r.w, r.y, 16 * self.ui, r.h)
    }
    /// The size of one entry's cell in a folder's `rows`, and how many go
    /// across: one full-width row each in Details, tiles in Large icons.
    fn cells(&self, rows: &Rect) -> (i32, i32, usize) {
        let s = self.ui;
        if self.icons {
            let w = 128 * s;
            (w, 104 * s, (rows.w / w).max(1) as usize)
        } else {
            (rows.w, self.files_row_h(), 1)
        }
    }
    fn visible_in(&self, k: &Rect) -> usize {
        let rows = self.rows_in(k);
        let (_, h, across) = self.cells(&rows);
        across * (rows.h / h).max(1) as usize
    }
    /// Entry `i` (counted from the first one shown).
    fn row_in(&self, k: &Rect, i: usize) -> Rect {
        let rows = self.rows_in(k);
        let (w, h, across) = self.cells(&rows);
        Rect::new(rows.x + (i % across) as i32 * w, rows.y + (i / across) as i32 * h, w, h)
    }
    /// Which entry (counted from the first one shown) is at (x, y) in
    /// `rows`; a number past any entry when it is beside the last column.
    fn cell_at(&self, rows: &Rect, x: i32, y: i32) -> usize {
        let (w, h, across) = self.cells(rows);
        let col = ((x - rows.x) / w).max(0) as usize;
        if col >= across {
            return 1 << 40;
        }
        ((y - rows.y) / h).max(0) as usize * across + col
    }
    /// How many entries one step down moves: a row of them.
    fn files_across(&self, c: &Rect) -> usize {
        self.cells(&self.files_rows(c)).2
    }
    /// Where the Type and Size columns start (Size is left out when narrow).
    /// Where the Date modified, Type and Size columns start (Date and
    /// Size are left out when narrow: Date where it starts Type then).
    fn columns_in(&self, k: &Rect) -> (i32, i32, i32) {
        let s = self.ui;
        let rows = self.rows_in(k);
        let size_w = if rows.w < 380 * s { 0 } else { 90 * s };
        let size_x = rows.x + rows.w - size_w;
        // Narrow (a side of two panes): names get most of the room.
        let type_w = if size_w == 0 { (rows.w / 3).min(120 * s) } else { ((rows.w - size_w) * 2 / 5).min(170 * s) };
        let type_x = size_x - type_w;
        let date_w = if rows.w < 620 * s { 0 } else { Font::Normal.width(s, "12/31/2026 12:00 PM") + 14 * s };
        (type_x - date_w, type_x, size_x)
    }
    fn files_head(&self, c: &Rect) -> Rect {
        self.head_in(&self.files_content(c))
    }
    fn files_rows(&self, c: &Rect) -> Rect {
        self.rows_in(&self.files_content(c))
    }
    fn files_scrollbar(&self, c: &Rect) -> Rect {
        self.scrollbar_in(&self.files_content(c))
    }
    fn files_row_h(&self) -> i32 {
        30 * self.ui + grow(self.ui)
    }
    fn files_visible(&self, c: &Rect) -> usize {
        self.visible_in(&self.files_content(c))
    }
    /// Where entry `i` (counted from the first one shown) is drawn.
    fn files_row(&self, c: &Rect, i: usize) -> Rect {
        self.row_in(&self.files_content(c), i)
    }
    fn files_columns(&self, c: &Rect) -> (i32, i32, i32) {
        self.columns_in(&self.files_content(c))
    }
    fn tile_columns(&self, c: &Rect) -> usize {
        self.tile_columns_in(&self.files_content(c))
    }
    fn tile_columns_in(&self, k: &Rect) -> usize {
        let s = self.ui;
        ((k.w - 12 * s) / (262 * s)).max(1) as usize
    }
    /// The tile for drive `i` in Computer: as many columns as fit, sharing the width.
    fn drive_tile(&self, c: &Rect, i: usize) -> Rect {
        let k = self.files_content(c);
        self.drive_tile_in(&k, i)
    }
    fn drive_tile_in(&self, k: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let cols = self.tile_columns_in(k);
        let w = ((k.w - 12 * s) / cols as i32 - 12 * s).min(340 * s);
        let (col, row) = ((i % cols) as i32, (i / cols) as i32);
        Rect::new(k.x + 12 * s + col * (w + 12 * s), k.y + 40 * s + row * 74 * s, w, 66 * s)
    }
    /// The command bar's buttons, or Yes and No while asking about a delete.
    fn command_buttons(&self, c: &Rect) -> Vec<(Command, &'static str, Rect)> {
        let s = self.ui;
        let bar = self.files_commands(c);
        let mut x = bar.x + 8 * s;
        let mut out = Vec::new();
        let list: &[(Command, &'static str)] = if self.files.confirm.is_some() {
            x += Font::Normal.width(s, &self.delete_question()) + 16 * s;
            &[(Command::Yes, "Yes"), (Command::No, "No")]
        } else if self.files.path == BIN {
            &BIN_COMMANDS
        } else if self.files.path == DELETED {
            &DELETED_COMMANDS
        } else if self.files.path.is_empty() {
            &COMPUTER_COMMANDS
        } else {
            &COMMANDS
        };
        let mut list: Vec<(Command, &'static str)> = list.to_vec();
        if self.other.is_some() && self.files.confirm.is_none() && self.files.in_folder() || self.other.is_some() && self.files.confirm.is_none() && self.files.path == SEARCH {
            // Where the other side is, by an arrow.
            let over: [(Command, &'static str); 2] = if self.right {
                [(Command::CopyOver, "< Copy"), (Command::MoveOver, "< Move")]
            } else {
                [(Command::CopyOver, "Copy >"), (Command::MoveOver, "Move >")]
            };
            list.splice(4..4, over);
        }
        for &(cmd, label) in &list {
            let w = Font::Normal.width(s, label) + 20 * s;
            if x + w > bar.x + bar.w {
                break;
            }
            out.push((cmd, label, Rect::new(x, bar.y + 4 * s, w, bar.h - 8 * s)));
            x += w + 4 * s;
        }
        out
    }

    fn delete_question(&self) -> String {
        if self.files.confirm == Some(Command::Empty) {
            let n = self.files.bin.len();
            return format!("Delete {} item{} for good?", n, if n == 1 { "" } else { "s" });
        }
        let picked = self.picked();
        if picked.len() > 1 {
            let n = picked.len();
            let network = picked.iter().any(|&j| self.files.path != BIN && self.recycle_root_of(&self.entry_path(j)).is_none());
            return if self.files.path == BIN || network {
                format!("Delete these {} items for good?", n)
            } else {
                format!("Move these {} items to the Recycle Bin?", n)
            };
        }
        let name = self.selected_entry().map(|e| e.name).unwrap_or_default();
        let shown: String = if name.chars().count() > 24 {
            let mut n: String = name.chars().take(21).collect();
            n.push_str("...");
            n
        } else {
            name
        };
        if self.files.path == BIN || self.recycle_root().is_none() {
            format!("Delete \"{}\" for good?", shown)
        } else {
            format!("Move \"{}\" to the Recycle Bin?", shown)
        }
    }

    /// The drive a path is on.
    fn drive_of(&self, path: &str) -> Option<usize> {
        if !path.starts_with('/') || path.starts_with("//") {
            return None;
        }
        let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
        let d = &self.files.drives;
        d.iter().position(|v| v.path != "/" && v.path[1..].eq_ignore_ascii_case(first)).or_else(|| d.iter().position(|v| v.path == "/"))
    }
    /// "Local Disk (C:)", "HOTSTICK (E:)", ...
    fn drive_name(&self, i: usize) -> String {
        let v = &self.files.drives[i];
        let letter = (b'C' + i.min(23) as u8) as char;
        let label = v.label.trim();
        let base = if !label.is_empty() {
            label
        } else if v.device.starts_with("usb") {
            "Removable Disk"
        } else {
            "Local Disk"
        };
        format!("{} ({}:)", base, letter)
    }
    /// The folder shown is on a read-only drive (or is Computer itself).
    fn files_read_only(&self) -> bool {
        if net_path(&self.files.path).is_some() {
            return false;
        }
        self.drive_of(&self.files.path).map(|d| self.files.drives[d].read_only).unwrap_or(true)
    }
    fn selected_entry(&self) -> Option<aero::DirEntry> {
        if self.files.path.is_empty() {
            return None;
        }
        self.files.selected.and_then(|i| self.files.entries.get(i).cloned())
    }
    /// What tells entry `i` apart from the rest: its name, or in the
    /// Recycle Bin (where two can have the same name) its number.
    fn entry_key(&self, i: usize) -> String {
        self.files.key(i)
    }
    fn entry_path(&self, i: usize) -> String {
        self.files.entry_path(i)
    }
    fn is_marked(&self, i: usize) -> bool {
        self.files.is_marked(i)
    }
    /// The entries selected, in the order shown.
    fn picked(&self) -> Vec<usize> {
        if self.files.path.is_empty() {
            return Vec::new();
        }
        (0..self.files.entries.len()).filter(|&i| self.is_marked(i)).collect()
    }
    /// The selected entries as things to copy, move or drag.
    fn picked_items(&self) -> Vec<Item> {
        self.picked()
            .into_iter()
            .map(|i| {
                let e = &self.files.entries[i];
                Item { path: self.entry_path(i), is_dir: e.is_dir, size: e.size }
            })
            .collect()
    }
    /// Whether the folder shown is on a drive whose deleted files can be
    /// looked for (FAT32, writable; not the Recycle Bin's own folder).
    fn can_undelete(&self) -> bool {
        self.files.in_folder()
            && !self.files_read_only()
            && self.drive_of(&self.files.path).is_some_and(|d| self.files.drives[d].kind == "FAT32")
            && net_path(&self.files.path).is_none()
    }

    /// The drive whose Recycle Bin's emptied files can be looked for: the
    /// first writable FAT32 drive that has emptied something.
    fn emptied_root(&self) -> Option<String> {
        self.files.drives.iter()
            .filter(|v| v.kind == "FAT32" && !v.read_only && net_path(&v.path).is_none())
            .map(|v| v.path.clone())
            .find(|r| !Self::read_list(r, BIN_GONE).is_empty())
    }

    fn command_enabled(&self, cmd: Command) -> bool {
        let folder = !self.files.path.is_empty();
        let writable = folder && !self.files_read_only();
        let picked = !self.picked().is_empty();
        let search = self.files.path == SEARCH;
        // In search results each can be on a different drive.
        let all_writable = || self.picked().iter().all(|&j| !self.files_drive_read_only(&self.entry_path(j)));
        if self.files.path == DELETED {
            return cmd == Command::Restore && picked && self.picked().iter().all(|&j| self.files.lost_at(j).is_some_and(|d| d.whole));
        }
        match cmd {
            Command::NewFolder | Command::Paste | Command::Rename if search => false,
            Command::Cut | Command::Delete if search => picked && all_writable(),
            Command::NewFolder => writable,
            Command::Copy => picked,
            Command::Cut | Command::Rename => picked && writable,
            Command::Paste => writable && self.clip.is_some(),
            Command::Restore => picked && self.files.path == BIN,
            Command::Delete if self.files.path == BIN => picked,
            Command::Delete => picked && writable,
            Command::Empty => !self.files.bin.is_empty(),
            Command::Map => !folder && self.files.form.is_none(),
            Command::Disconnect => !folder && self.files.selected.is_some_and(|i| i >= self.files.drives.len()),
            Command::Properties if !folder => self.files.selected.is_some() && self.files.form.is_none(),
            Command::Properties => picked,
            Command::CopyOver | Command::MoveOver => {
                let there = self.other.as_ref().is_some_and(|o| o.in_folder() && !self.files_drive_read_only(&o.path));
                let away = cmd == Command::CopyOver || (writable || (search && all_writable()));
                there && picked && away && self.files.path != BIN
            }
            Command::Yes | Command::No => true,
        }
    }

    /// The navigation pane's rows: Computer, the drives, the network
    /// drives and the Recycle Bin, as (shown as, path, kind).
    fn places(&self) -> Vec<PaneRow> {
        let row = |name: String, path: String, kind: Place, depth: i32, open: Option<bool>| PaneRow { name, path, kind, depth, open };
        let mut out = alloc::vec![row(String::from("Recycle Bin"), String::from(BIN), Place::Bin, 0, None)];
        let computer_open = self.tree_is_open("");
        out.push(row(String::from("Computer"), String::new(), Place::Computer, 0, Some(computer_open)));
        if !computer_open {
            return out;
        }
        for i in 0..self.files.drives.len() {
            let path = self.files.drives[i].path.clone();
            out.push(row(self.drive_name(i), path.clone(), Place::Drive, 1, self.tree_arrow(&path)));
            self.tree_rows(&path, 2, &mut out);
        }
        for sh in shares().iter() {
            let path = sh.root();
            let arrow = if sh.client.is_some() { self.tree_arrow(&path) } else { None };
            out.push(row(sh.name(), path.clone(), Place::Network, 1, arrow));
            self.tree_rows(&path, 2, &mut out);
        }
        out
    }
    fn tree_is_open(&self, path: &str) -> bool {
        self.tree_open.iter().any(|p| p.eq_ignore_ascii_case(path))
    }
    fn tree_kids_of(&self, path: &str) -> Option<&Vec<String>> {
        self.tree_kids.iter().find(|(p, _)| p.eq_ignore_ascii_case(path)).map(|(_, k)| k)
    }
    /// A place's arrow: open, closed, or none if it has no folders.
    fn tree_arrow(&self, path: &str) -> Option<bool> {
        match self.tree_kids_of(path) {
            Some(k) if k.is_empty() => None,
            _ => Some(self.tree_is_open(path)),
        }
    }
    /// The folders inside an open place, and theirs if open, as rows.
    fn tree_rows(&self, path: &str, depth: i32, out: &mut Vec<PaneRow>) {
        if !self.tree_is_open(path) || depth > 12 {
            return;
        }
        let Some(kids) = self.tree_kids_of(path) else { return };
        for name in kids.clone() {
            let child = join(path, &name);
            out.push(PaneRow { name, path: child.clone(), kind: Place::Folder, depth, open: self.tree_arrow(&child) });
            self.tree_rows(&child, depth + 1, out);
        }
    }
    /// Reads the folders in `path` for the tree (once).
    fn tree_load(&mut self, path: &str) {
        if self.tree_kids_of(path).is_some() {
            return;
        }
        let Ok(list) = fs_list(path) else { return };
        let roots: Vec<String> = self.files.drives.iter().map(|v| v.path.to_lowercase()).collect();
        let at_root = roots.iter().any(|r| r == &path.to_lowercase()) || net_path(path).is_some_and(|(_, inner)| inner.trim_matches('/').is_empty());
        let mut kids: Vec<String> = list
            .into_iter()
            .filter(|e| e.is_dir)
            .map(|e| e.name)
            .filter(|n| !(at_root && n.eq_ignore_ascii_case(BIN_DIR)))
            // The first drive's root lists the other drives as folders.
            .filter(|n| !(path == "/" && roots.iter().any(|r| r[1..].eq_ignore_ascii_case(n))))
            .collect();
        kids.sort_by_key(|n| n.to_lowercase());
        self.tree_kids.push((String::from(path), kids));
    }
    /// Opens or closes a place's arrow in the tree.
    fn tree_toggle(&mut self, path: &str) -> Rect {
        if self.tree_is_open(path) {
            let lower = path.to_lowercase();
            let inside = format!("{}/", lower.trim_end_matches('/'));
            self.tree_open.retain(|p| *p != lower && !(p.starts_with(&inside) && !lower.is_empty()));
            println!("[desktop] tree: closed {}", if path.is_empty() { "Computer" } else { path });
        } else {
            if !path.is_empty() {
                self.tree_load(path);
            }
            self.tree_open.push(path.to_lowercase());
            let kids = self.tree_kids_of(path).map(|k| k.join(" | ")).unwrap_or_default();
            println!("[desktop] tree: opened {} = {}", if path.is_empty() { "Computer" } else { path }, kids);
        }
        self.log_pane();
        self.files_area()
    }
    /// Opens the tree down to the folder shown, as Explorer does.
    fn tree_reveal(&mut self, path: &str) {
        let root = self
            .files
            .drives
            .iter()
            .map(|v| v.path.clone())
            .chain(shares().iter().map(|sh| sh.root()))
            .filter(|r| {
                let r = r.to_lowercase();
                let p = path.to_lowercase();
                r == "/" || p == r || p.starts_with(&format!("{}/", r))
            })
            .max_by_key(|r| r.len());
        let Some(root) = root else { return };
        let mut at = root.clone();
        let rest: Vec<String> = path[root.len().min(path.len())..].split('/').filter(|p| !p.is_empty()).map(String::from).collect();
        // Every place above the folder shown is opened (not the folder itself).
        for part in rest.iter() {
            self.tree_load(&at);
            if !self.tree_is_open(&at) {
                self.tree_open.push(at.to_lowercase());
            }
            at = join(&at, part);
        }
    }
    /// The tree's rows and where they are, for the log.
    fn log_pane(&self) {
        let c = self.files_client();
        let rows: Vec<String> = self
            .places()
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let b = self.pane_item(&c, i);
                let a = self.pane_arrow(&c, i, r.depth);
                let arrow = match r.open {
                    Some(open) => format!(" ({} {},{})", if open { "v" } else { ">" }, a.x + a.w / 2, a.y + a.h / 2),
                    None => String::new(),
                };
                (!b.is_empty()).then(|| format!("{}{} at {},{}{}", "  ".repeat(r.depth as usize), r.name, b.x + b.w / 2, b.y + b.h / 2, arrow))
            })
            .collect();
        println!("[desktop] pane: {}", rows.join(" | "));
    }

    /// How many tiles Computer shows: the drives, then the network drives.
    fn tile_count(&self) -> usize {
        self.files.drives.len() + shares().len()
    }

    /// The parts of the address: (shown as, path to go to).
    fn crumbs(&self) -> Vec<(String, String)> {
        if self.files.path == BIN {
            return alloc::vec![(String::from("Recycle Bin"), String::from(BIN))];
        }
        if self.files.path == DELETED {
            return alloc::vec![(self.place_title(&self.files), String::from(DELETED))];
        }
        if self.files.path == SEARCH {
            let at = &self.files.searched_in;
            let place = if at.is_empty() {
                String::from("Computer")
            } else if let Some(d) = self.files.drives.iter().position(|v| v.path.eq_ignore_ascii_case(at)) {
                self.drive_name(d)
            } else if let Some(sh) = shares().iter().find(|sh| sh.root().eq_ignore_ascii_case(at)) {
                sh.name()
            } else {
                split_path(at).1
            };
            return alloc::vec![(format!("Search Results in {}", place), String::from(SEARCH))];
        }
        let mut out = alloc::vec![(String::from("Computer"), String::new())];
        if let Some((i, inner)) = net_path(&self.files.path) {
            let mut at = shares()[i].root();
            out.push((shares()[i].name(), at.clone()));
            for part in inner.split('/').filter(|p| !p.is_empty()) {
                at = join(&at, part);
                out.push((String::from(part), at.clone()));
            }
            return out;
        }
        let Some(d) = self.drive_of(&self.files.path) else { return out };
        let root = self.files.drives[d].path.clone();
        out.push((self.drive_name(d), root.clone()));
        let inner = if root == "/" { self.files.path.as_str() } else { &self.files.path[root.len()..] };
        let mut at = root;
        for part in inner.split('/').filter(|p| !p.is_empty()) {
            at = join(&at, part);
            out.push((String::from(part), at.clone()));
        }
        out
    }
    /// Where each part of the address is drawn, last ones kept when they don't all fit.
    fn crumb_rects(&self, c: &Rect) -> Vec<(Rect, String, String)> {
        let s = self.ui;
        let a = self.files_address(c);
        let fw = Font::Normal.w(s);
        let crumbs = self.crumbs();
        let room = (a.w - 32 * s) / fw;
        let mut first = 0;
        let len = |from: usize| crumbs[from..].iter().map(|(t, _)| t.chars().count() as i32 + 3).sum::<i32>() - 3;
        while first + 1 < crumbs.len() && len(first) > room {
            first += 1;
        }
        let mut x = a.x + 26 * s;
        let mut out = Vec::new();
        for (text, path) in crumbs.into_iter().skip(first) {
            let w = text.chars().count() as i32 * fw;
            out.push((Rect::new(x, a.y + 2 * s, w, a.h - 4 * s), text, path));
            x += w + 3 * fw;
        }
        out
    }

    /// What a folder shown is called in a tab: "Computer", "Recycle Bin",
    /// a drive's name, or the folder's own.
    fn place_title(&self, f: &Files) -> String {
        match f.path.as_str() {
            "" => String::from("Computer"),
            BIN => String::from("Recycle Bin"),
            SEARCH => String::from("Search Results"),
            DELETED if !f.lost_bin.is_empty() || bin_folder(&f.lost_in) => String::from("Emptied from the Recycle Bin"),
            DELETED => format!("Deleted from {}", split_path(&f.lost_in).1),
            p => {
                if let Some(d) = f.drives.iter().position(|v| v.path.eq_ignore_ascii_case(p)) {
                    return self.drive_name(d);
                }
                if let Some(sh) = shares().iter().find(|sh| sh.root().eq_ignore_ascii_case(p)) {
                    return sh.name();
                }
                split_path(p).1
            }
        }
    }
    /// Where a side shows, in full ("C:/docs/Stuff"), for two-pane headers.
    fn place_path(&self, f: &Files) -> String {
        if f.path.is_empty() || f.path == BIN || f.path == SEARCH || f.path == DELETED {
            return self.place_title(f);
        }
        if let Some((i, inner)) = net_path(&f.path) {
            return format!("{}:/{}", shares()[i].letter, inner.trim_start_matches('/'));
        }
        match f.drives.iter().position(|v| v.path != "/" && f.path.to_lowercase().starts_with(&v.path.to_lowercase())) {
            Some(d) => format!("{}:{}", (b'C' + d as u8) as char, &f.path[f.drives[d].path.len()..]).replace(":", ":/").replace("//", "/"),
            None => format!("C:{}", f.path),
        }
    }
    fn tab_title(&self, i: usize) -> String {
        if i == self.tab {
            self.place_title(&self.files)
        } else {
            self.place_title(&self.tabs[i].files)
        }
    }
    fn files_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let f = &self.files;
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let in_folder = !f.path.is_empty();

        // Navigation bar: Back, Forward and Up, then the address.
        let nav = self.files_nav(client);
        c.gradient(nav, rgb(245, 250, 255), rgb(215, 230, 245), 255);
        for i in 0..3 {
            let (bx, by, br) = self.files_arrow(client, i);
            let live = match i {
                0 => !f.back.is_empty(),
                1 => !f.forward.is_empty(),
                _ => in_folder,
            };
            if live {
                c.orb(bx, by, br, rgb(120, 200, 255), rgb(20, 90, 170));
            } else {
                c.orb(bx, by, br, rgb(220, 225, 230), rgb(150, 160, 170));
            }
            let d = br * 5 / 12;
            match i {
                2 => {
                    c.line(bx, by - d, bx, by + d, 2 * s, 0xFFFFFF);
                    c.line(bx, by - d, bx - d + s, by - s, 2 * s, 0xFFFFFF);
                    c.line(bx, by - d, bx + d - s, by - s, 2 * s, 0xFFFFFF);
                }
                _ => {
                    let dir = if i == 0 { 1 } else { -1 };
                    c.line(bx - d, by, bx + d - s, by, 2 * s, 0xFFFFFF);
                    let tip = bx - dir * d;
                    c.line(tip, by, tip + dir * (d - s), by - d + s, 2 * s, 0xFFFFFF);
                    c.line(tip, by, tip + dir * (d - s), by + d - s, 2 * s, 0xFFFFFF);
                }
            }
        }
        let a = self.files_address(client);
        c.fill(a, 0xFFFFFF);
        c.frame(a, rgb(130, 150, 180));
        if f.path == BIN {
            c.small_bin(a.x + 6 * s, a.y + a.h / 2 - 8 * s, s);
        } else if in_folder {
            c.folder(a.x + 5 * s, a.y + a.h / 2 - 6 * s, s);
        } else {
            c.small_computer(a.x + 5 * s, a.y + a.h / 2 - 7 * s, s);
        }
        let crumbs = self.crumb_rects(client);
        let ty = a.y + (a.h - Font::Normal.h(s)) / 2;
        for (i, (r, text, _)) in crumbs.iter().enumerate() {
            if r.contains(px, py) {
                c.rounded(*r, 2 * s, false, rgb(235, 244, 253), rgb(205, 228, 250), 255);
            }
            c.text(r.x, ty, text, 0x101010, Font::Normal);
            if i + 1 < crumbs.len() {
                c.text(r.x + r.w + Font::Normal.w(s), ty, ">", rgb(110, 120, 140), Font::Normal);
            }
        }

        // Search box.
        let q = self.files_search(client);
        c.fill(q, 0xFFFFFF);
        c.frame(q, if f.query_focus { rgb(60, 110, 190) } else { rgb(130, 150, 180) });
        let qy = q.y + (q.h - Font::Normal.h(s)) / 2;
        let room = ((q.w - 34 * s) / Font::Normal.w(s)).max(1) as usize;
        if f.query.is_empty() && !f.query_focus {
            let place = match self.crumbs().last() {
                Some((name, _)) if in_folder && f.path != BIN && f.path != SEARCH => name.clone(),
                _ => String::from("Computer"),
            };
            let hint: String = format!("Search {}", place).chars().take(room).collect();
            c.text(q.x + 6 * s, qy, &hint, rgb(140, 145, 155), Font::Normal);
        } else {
            let skip = f.query.chars().count().saturating_sub(room.saturating_sub(1));
            let shown: String = f.query.chars().skip(skip).collect();
            c.text(q.x + 6 * s, qy, &shown, 0x101010, Font::Normal);
            if f.query_focus {
                let caret = q.x + 6 * s + shown.chars().count() as i32 * Font::Normal.w(s);
                c.fill(Rect::new(caret, qy + 2 * s, s.max(2), Font::Normal.h(s) - 4 * s), 0x101010);
            }
        }
        // A magnifying glass.
        let (gx, gy) = (q.x + q.w - 22 * s, q.y + 6 * s);
        c.rounded_outline(Rect::new(gx, gy, 11 * s, 11 * s), 5 * s, false, rgb(80, 100, 130), 255);
        c.line(gx + 9 * s, gy + 9 * s, gx + 14 * s, gy + 14 * s, 2 * s, rgb(80, 100, 130));

        // Command bar.
        let bar = self.files_commands(client);
        c.gradient(bar, rgb(250, 252, 255), rgb(222, 233, 246), 255);
        c.fill(Rect::new(bar.x, bar.y + bar.h - 1, bar.w, 1), rgb(160, 180, 205));
        let label_y = |r: &Rect| r.y + (r.h - Font::Normal.h(s)) / 2;
        if f.confirm.is_some() {
            c.text(bar.x + 8 * s, bar.y + (bar.h - Font::Normal.h(s)) / 2, &self.delete_question(), rgb(150, 30, 20), Font::Normal);
        }
        for (cmd, label, r) in self.command_buttons(client) {
            let on = self.command_enabled(cmd);
            if f.confirm.is_some() || (on && r.contains(px, py)) {
                c.rounded(r, 3 * s, false, rgb(250, 252, 255), rgb(200, 222, 245), 255);
                c.rounded_outline(r, 3 * s, false, rgb(120, 150, 190), 255);
            }
            let ink = if on { rgb(20, 40, 80) } else { rgb(160, 165, 175) };
            c.text(r.x + 10 * s, label_y(&r), label, ink, Font::Normal);
        }

        // Tabs, and the two-pane switch.
        let strip = self.files_tabs(client);
        c.gradient(strip, rgb(236, 242, 250), rgb(218, 229, 243), 255);
        c.fill(Rect::new(strip.x, strip.y + strip.h - 1, strip.w, 1), rgb(170, 188, 212));
        for i in 0..self.tabs.len() {
            let r = self.tab_rect(client, i);
            let front = i == self.tab;
            if front {
                c.rounded(Rect::new(r.x, r.y, r.w, r.h + 2 * s), 4 * s, true, rgb(255, 255, 255), rgb(248, 251, 255), 255);
                c.rounded_outline(Rect::new(r.x, r.y, r.w, r.h + 2 * s), 4 * s, true, rgb(140, 165, 200), 255);
            } else if r.contains(px, py) {
                c.rounded(r, 4 * s, true, rgb(246, 250, 255), rgb(226, 236, 250), 255);
                c.rounded_outline(r, 4 * s, true, rgb(170, 188, 212), 255);
            } else {
                c.rounded_outline(r, 4 * s, true, rgb(190, 204, 224), 255);
            }
            c.folder(r.x + 7 * s, r.y + (r.h - 12 * s) / 2, s);
            let closable = self.tabs.len() > 1;
            let room = ((r.w - 32 * s - if closable { 20 * s } else { 0 }) / Font::Normal.w(s)).max(1) as usize;
            let title: String = self.tab_title(i).chars().take(room).collect();
            c.text(r.x + 28 * s, r.y + (r.h - Font::Normal.h(s)) / 2, &title, if front { 0x101010 } else { rgb(60, 70, 90) }, Font::Normal);
            if closable {
                let x = self.tab_close(client, i);
                let ink = if x.contains(px, py) { rgb(190, 40, 30) } else { rgb(110, 120, 140) };
                c.line(x.x + 4 * s, x.y + 4 * s, x.x + x.w - 5 * s, x.y + x.h - 5 * s, s.max(2), ink);
                c.line(x.x + x.w - 5 * s, x.y + 4 * s, x.x + 4 * s, x.y + x.h - 5 * s, s.max(2), ink);
            }
        }
        let plus = self.new_tab_button(client);
        if plus.contains(px, py) {
            c.rounded(plus, 3 * s, false, rgb(250, 252, 255), rgb(205, 225, 248), 255);
        }
        let (mx, my) = (plus.x + plus.w / 2, plus.y + plus.h / 2);
        c.fill(Rect::new(mx - 6 * s, my - s, 12 * s, 2 * s), rgb(50, 70, 110));
        c.fill(Rect::new(mx - s, my - 6 * s, 2 * s, 12 * s), rgb(50, 70, 110));
        let pb = self.panes_button(client);
        if self.other.is_some() {
            c.rounded(pb, 3 * s, false, rgb(205, 225, 248), rgb(180, 208, 242), 255);
            c.rounded_outline(pb, 3 * s, false, rgb(110, 140, 185), 255);
        } else if pb.contains(px, py) {
            c.rounded(pb, 3 * s, false, rgb(250, 252, 255), rgb(205, 225, 248), 255);
            c.rounded_outline(pb, 3 * s, false, rgb(130, 155, 190), 255);
        }
        // Its icon: two side-by-side panels.
        let ix = pb.x + 8 * s;
        let iy = pb.y + (pb.h - 12 * s) / 2;
        for px0 in [ix, ix + 9 * s] {
            c.fill(Rect::new(px0, iy, 8 * s, 12 * s), 0xFFFFFF);
            c.fill(Rect::new(px0, iy, 8 * s, 3 * s), rgb(90, 140, 210));
            c.frame(Rect::new(px0, iy, 8 * s, 12 * s), rgb(50, 70, 110));
        }
        c.text(pb.x + 26 * s, pb.y + (pb.h - Font::Normal.h(s)) / 2, "Two panes", rgb(20, 40, 80), Font::Normal);
        let vb = self.preview_button(client);
        if self.preview_on {
            c.rounded(vb, 3 * s, false, rgb(205, 225, 248), rgb(180, 208, 242), 255);
            c.rounded_outline(vb, 3 * s, false, rgb(110, 140, 185), 255);
        } else if vb.contains(px, py) {
            c.rounded(vb, 3 * s, false, rgb(250, 252, 255), rgb(205, 225, 248), 255);
            c.rounded_outline(vb, 3 * s, false, rgb(130, 155, 190), 255);
        }
        // Its icon: a window with a panel at the right.
        let (ix, iy) = (vb.x + 8 * s, vb.y + (vb.h - 12 * s) / 2);
        c.fill(Rect::new(ix, iy, 17 * s, 12 * s), 0xFFFFFF);
        c.fill(Rect::new(ix + 11 * s, iy, 6 * s, 12 * s), rgb(150, 190, 235));
        c.frame(Rect::new(ix, iy, 17 * s, 12 * s), rgb(50, 70, 110));
        c.text(vb.x + 26 * s, vb.y + (vb.h - Font::Normal.h(s)) / 2, "Preview", rgb(20, 40, 80), Font::Normal);
        // The view: what a click switches to, with an icon of it.
        let wb = self.view_button(client);
        if wb.contains(px, py) {
            c.rounded(wb, 3 * s, false, rgb(250, 252, 255), rgb(205, 225, 248), 255);
            c.rounded_outline(wb, 3 * s, false, rgb(130, 155, 190), 255);
        }
        let (ix, iy) = (wb.x + 8 * s, wb.y + (wb.h - 12 * s) / 2);
        if self.icons {
            for i in 0..3 {
                c.fill(Rect::new(ix, iy + i * 4 * s, 3 * s, 3 * s), rgb(90, 140, 210));
                c.fill(Rect::new(ix + 5 * s, iy + i * 4 * s + s, 11 * s, s), rgb(50, 70, 110));
            }
        } else {
            for (dx, dy) in [(0, 0), (9 * s, 0), (0, 7 * s), (9 * s, 7 * s)] {
                c.fill(Rect::new(ix + dx, iy + dy, 7 * s, 5 * s), rgb(90, 140, 210));
                c.frame(Rect::new(ix + dx, iy + dy, 7 * s, 5 * s), rgb(50, 70, 110));
            }
        }
        let label = if self.icons { "Details" } else { "Large icons" };
        c.text(wb.x + 26 * s, wb.y + (wb.h - Font::Normal.h(s)) / 2, label, rgb(20, 40, 80), Font::Normal);

        // Navigation pane: Computer and the drives.
        let pane = self.files_pane(client);
        c.fill(pane, rgb(241, 245, 251));
        c.fill(Rect::new(pane.x + pane.w, pane.y, 1, pane.h), rgb(205, 215, 230));
        let here = self.drive_of(&f.path);
        for (i, row) in self.places().into_iter().enumerate() {
            let r = self.pane_item(client, i);
            if r.is_empty() {
                continue;
            }
            let (text, path, icon) = (row.name, row.path, row.kind);
            let current = match icon {
                Place::Computer => f.path.is_empty(),
                Place::Bin => f.path == BIN,
                _ => f.path.eq_ignore_ascii_case(&path) || (icon == Place::Drive && path == "/" && f.path == "/"),
            };
            let box_ = Rect::new(r.x + 3 * s, r.y + s, r.w - 6 * s, r.h - 2 * s);
            if current {
                c.rounded(box_, 2 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(box_, 2 * s, false, rgb(125, 162, 206), 255);
            } else if r.contains(px, py) {
                c.rounded(box_, 2 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
            }
            // The arrow: a hollow one pointing right when closed, a solid
            // one pointing down-right when open, as in Windows 7.
            let a = self.pane_arrow(client, i, row.depth);
            if let Some(open) = row.open {
                let (cx, cy) = (a.x + a.w / 2, a.y + a.h / 2);
                let hot = a.contains(px, py);
                if open {
                    let ink = if hot { rgb(30, 140, 220) } else { rgb(60, 60, 60) };
                    for k in 0..5 * s {
                        c.fill(Rect::new(cx + 2 * s - k, cy - 2 * s + k, k + 1, 1), ink);
                    }
                } else {
                    let ink = if hot { rgb(30, 140, 220) } else { rgb(140, 140, 140) };
                    c.line(cx - 2 * s, cy - 4 * s, cx - 2 * s, cy + 4 * s, 1, ink);
                    c.line(cx - 2 * s, cy - 4 * s, cx + 2 * s, cy, 1, ink);
                    c.line(cx - 2 * s, cy + 4 * s, cx + 2 * s, cy, 1, ink);
                }
            }
            let ix = a.x + a.w + 2 * s - r.x;
            match icon {
                Place::Computer => c.small_computer(r.x + ix, r.y + (r.h - 14 * s) / 2, s),
                Place::Bin => c.small_bin(r.x + ix + s, r.y + (r.h - 16 * s) / 2, s),
                Place::Drive => c.small_drive(r.x + ix, r.y + (r.h - 12 * s) / 2, s, false),
                Place::Network => c.small_drive(r.x + ix, r.y + (r.h - 12 * s) / 2, s, true),
                Place::Folder => c.folder(r.x + ix, r.y + (r.h - 12 * s) / 2, s),
            }
            let room = ((r.w - ix - 26 * s) / Font::Normal.w(s)).max(1) as usize;
            let shown: String = if text.chars().count() > room {
                text.chars().take(room.saturating_sub(1)).chain(core::iter::once('.')).collect()
            } else {
                text
            };
            c.text(r.x + ix + 22 * s, label_y(&r), &shown, rgb(20, 40, 80), Font::Normal);
        }

        c.fill(self.files_main(client), 0xFFFFFF);
        if let Some(other) = &self.other {
            // Two panes: each side's place along its top, the one in use in blue.
            for right in [false, true] {
                let mine = right == self.right;
                let side_files: &Files = if mine { &self.files } else { other };
                let h = self.side_header(client, right);
                if mine {
                    c.gradient(h, rgb(225, 238, 252), rgb(200, 222, 248), 255);
                } else {
                    c.gradient(h, rgb(246, 247, 249), rgb(230, 232, 236), 255);
                }
                c.fill(Rect::new(h.x, h.y + h.h - 1, h.w, 1), rgb(190, 200, 215));
                let room = ((h.w - 16 * s) / Font::Normal.w(s)).max(1) as usize;
                let place = self.place_path(side_files);
                let skip = place.chars().count().saturating_sub(room);
                let shown: String = place.chars().skip(skip).collect();
                let ink = if mine { rgb(20, 50, 110) } else { rgb(90, 95, 105) };
                c.text(h.x + 8 * s, h.y + (h.h - Font::Normal.h(s)) / 2, &shown, ink, Font::Normal);
            }
            let m = self.files_main(client);
            let gap = self.side(client, false).x + self.side(client, false).w;
            c.fill(Rect::new(gap, m.y, self.side(client, true).x - gap, m.h), rgb(190, 200, 215));
            let k = self.other_content(client);
            if other.path.is_empty() {
                self.drives_view(c, client, other, k, false);
            } else {
                self.list_view(c, other, k, false);
            }
        }
        if in_folder {
            self.list_view(c, &self.files, self.files_content(client), true);
        } else {
            self.drives_view(c, client, &self.files, self.files_content(client), true);
        }
        self.preview_view(c, client);
        self.props_view(c, client);

        // Status bar: what is selected, and the drive's free space at the right.
        let sb = self.files_status_bar(client);
        c.gradient(sb, rgb(240, 245, 252), rgb(215, 228, 242), 255);
        c.fill(Rect::new(sb.x, sb.y, sb.w, 1), rgb(180, 195, 215));
        let sy = sb.y + (sb.h - Font::Normal.h(s)) / 2;
        let mut right_x = sb.x + sb.w;
        if let Some(free) = here.and_then(|d| f.drives[d].free) {
            let t = format!("{} free", space_text(free));
            right_x = sb.x + sb.w - Font::Normal.width(s, &t) - 10 * s;
            c.text(right_x, sy, &t, rgb(60, 80, 110), Font::Normal);
        }
        let room = ((right_x - sb.x - 20 * s) / Font::Normal.w(s)).max(0) as usize;
        let status: String = f.status.chars().take(room).collect();
        c.text(sb.x + 8 * s, sy, &status, rgb(30, 50, 80), Font::Normal);
    }

    /// The preview pane: what is selected, its first lines or its bytes.
    fn preview_view(&self, c: &mut Canvas, client: &Rect) {
        let r = self.preview_rect(client);
        if r.is_empty() {
            return;
        }
        let s = self.ui;
        c.fill(Rect::new(r.x - 1, r.y, 1, r.h), rgb(190, 200, 215));
        c.fill(r, rgb(250, 251, 253));
        let head = Rect::new(r.x, r.y, r.w, 30 * s);
        c.gradient(head, rgb(246, 248, 251), rgb(232, 237, 244), 255);
        c.fill(Rect::new(r.x, head.y + head.h - 1, r.w, 1), rgb(205, 213, 225));
        c.text(r.x + 8 * s, head.y + (head.h - Font::Normal.h(s)) / 2, "Preview", rgb(60, 80, 110), Font::Normal);
        let Some(p) = &self.preview else {
            let msg = "Select a file to see it here.";
            let room = ((r.w - 16 * s) / Font::Normal.w(s)).max(1) as usize;
            let shown: String = msg.chars().take(room).collect();
            c.text(r.x + 8 * s, r.y + 44 * s, &shown, rgb(120, 125, 135), Font::Normal);
            return;
        };
        // Text and Hex, the one shown pressed.
        if !p.entry.is_dir {
            for hex in [false, true] {
                let b = self.preview_mode(client, hex);
                let usable = hex || !p.lines.is_empty() || p.data.is_empty();
                if p.hex == hex {
                    c.rounded(b, 3 * s, false, rgb(205, 225, 248), rgb(180, 208, 242), 255);
                    c.rounded_outline(b, 3 * s, false, rgb(110, 140, 185), 255);
                } else {
                    c.rounded_outline(b, 3 * s, false, rgb(180, 190, 205), 255);
                }
                let ink = if usable { rgb(20, 40, 80) } else { rgb(170, 175, 185) };
                let label = if hex { "Hex" } else { "Text" };
                c.text(b.x + (b.w - Font::Normal.width(s, label)) / 2, b.y + (b.h - Font::Normal.h(s)) / 2, label, ink, Font::Normal);
            }
        }
        // Name, type and size.
        let line = Font::Normal.h(s) + 4 * s;
        let room = ((r.w - 46 * s) / Font::Normal.w(s)).max(1) as usize;
        let mut y = head.y + head.h + 6 * s;
        if p.entry.is_dir {
            c.folder(r.x + 10 * s, y + 2 * s, s);
        } else {
            c.file_icon(r.x + 12 * s, y, s, file_kind(&p.entry.name));
        }
        let name: String = p.entry.name.chars().take(room).collect();
        c.text(r.x + 34 * s, y, &name, 0x101010, Font::Normal);
        y += line;
        let kind: String = type_text(&p.entry).chars().take(room).collect();
        c.text(r.x + 34 * s, y, &kind, rgb(90, 95, 105), Font::Normal);
        y += line;
        let detail = match (&p.error, p.items) {
            (Some(e), _) => format!("Can't read it: {}", e),
            (None, Some(n)) => format!("{} item{}", n, if n == 1 { "" } else { "s" }),
            (None, None) if p.entry.size as usize > p.data.len() => format!("{} (first {} shown)", size_text(p.entry.size), size_text(p.data.len() as u64)),
            (None, None) => format!("{} bytes", group(p.entry.size)),
        };
        let detail: String = detail.chars().take(room).collect();
        c.text(r.x + 34 * s, y, &detail, rgb(90, 95, 105), Font::Normal);
        if p.entry.is_dir || p.error.is_some() {
            return;
        }
        // The text, or the bytes.
        let body = self.preview_body(client);
        c.fill(Rect::new(body.x - 2 * s, body.y - 4 * s, body.w + 2 * s + self.preview_bar(client).w, 1), rgb(220, 225, 235));
        let shown = self.preview_lines_shown(client);
        let total = self.preview_total(client);
        let cols = (body.w / Font::Tiny.w(s).max(1)).max(1) as usize;
        let (width, chars) = self.hex_layout(client);
        for n in 0..shown.min(total.saturating_sub(p.scroll)) {
            let row = p.scroll + n;
            let text = if p.hex { hex_row(&p.data, row, width, chars) } else { p.lines[row].clone() };
            let text: String = text.chars().take(cols).collect();
            let ink = if p.hex { rgb(30, 40, 60) } else { 0x101010 };
            c.text(body.x, body.y + n as i32 * self.preview_line_h(), &text, ink, Font::Tiny);
        }
        if total > shown {
            // A plain scrollbar: click above or below the thumb to page.
            let bar = self.preview_bar(client);
            c.fill(bar, rgb(236, 238, 242));
            let th = (bar.h as usize * shown / total).max(12 * s as usize) as i32;
            let ty = bar.y + ((bar.h - th) as usize * p.scroll / (total - shown).max(1)) as i32;
            c.rounded(Rect::new(bar.x + 3 * s, ty, bar.w - 6 * s, th), 3 * s, false, rgb(205, 210, 220), rgb(180, 188, 200), 255);
        }
    }

    /// Computer: the drives as tiles, with how full each one is.
    fn drives_view(&self, c: &mut Canvas, client: &Rect, f: &Files, k: Rect, active: bool) {
        let s = self.ui;
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        if active && f.form.is_some() {
            self.form_view(c, client);
            return;
        }
        let heading = format!("Drives ({})", f.drives.len() + shares().len());
        c.text(k.x + 12 * s, k.y + 8 * s, &heading, rgb(30, 57, 145), Font::Normal);
        let line_x = k.x + 24 * s + Font::Normal.width(s, &heading);
        c.fill(Rect::new(line_x, k.y + 8 * s + Font::Normal.h(s) / 2, k.x + k.w - 12 * s - line_x, 1), rgb(200, 215, 235));
        for i in 0..f.drives.len() {
            let t = self.drive_tile_in(&k, i);
            if t.y + t.h > k.y + k.h {
                break;
            }
            let v = &f.drives[i];
            if f.selected == Some(i) {
                selected_tile(c, t, s, active);
            } else if t.contains(px, py) {
                c.rounded(t, 3 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
                c.rounded_outline(t, 3 * s, false, rgb(185, 210, 238), 255);
            }
            c.drive(t.x + 8 * s, t.y + 14 * s, s, v.device.starts_with("usb"));
            let tx = t.x + 50 * s;
            let cols = ((t.x + t.w - 8 * s - tx) / Font::Normal.w(s)).max(1) as usize;
            let name: String = self.drive_name(i).chars().take(cols).collect();
            c.text(tx, t.y + 4 * s, &name, 0x101010, Font::Normal);
            let bar = Rect::new(tx, t.y + 31 * s, t.x + t.w - 10 * s - tx, 10 * s);
            let detail = match v.free {
                Some(free) => {
                    c.gradient(bar, rgb(235, 235, 235), rgb(215, 215, 215), 255);
                    let used = v.size.saturating_sub(free);
                    let fill = if v.size == 0 { 0 } else { (bar.w as u64 * used / v.size) as i32 };
                    let (top, bottom) = if used * 10 > v.size * 9 { (rgb(240, 90, 80), rgb(190, 30, 30)) } else { (rgb(70, 170, 245), rgb(20, 100, 200)) };
                    c.gradient(Rect::new(bar.x, bar.y, fill, bar.h), top, bottom, 255);
                    c.frame(bar, rgb(150, 160, 175));
                    format!("{} free of {}", space_text(free), space_text(v.size))
                }
                None => {
                    c.frame(bar, rgb(200, 205, 215));
                    format!("{} {}{}", space_text(v.size), v.kind, if v.read_only { ", read-only" } else { "" })
                }
            };
            let small_cols = ((t.x + t.w - 8 * s - tx) / Font::Small.w(s)).max(1) as usize;
            let detail: String = detail.chars().take(small_cols).collect();
            c.text(tx, t.y + 43 * s, &detail, rgb(90, 95, 105), Font::Small);
        }
        for (n, sh) in shares().iter().enumerate() {
            let i = f.drives.len() + n;
            let t = self.drive_tile_in(&k, i);
            if t.y + t.h > k.y + k.h {
                break;
            }
            if f.selected == Some(i) {
                selected_tile(c, t, s, active);
            } else if t.contains(px, py) {
                c.rounded(t, 3 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
                c.rounded_outline(t, 3 * s, false, rgb(185, 210, 238), 255);
            }
            c.net_drive(t.x + 8 * s, t.y + 14 * s, s, sh.client.is_some());
            let tx = t.x + 50 * s;
            let cols = ((t.x + t.w - 8 * s - tx) / Font::Normal.w(s)).max(1) as usize;
            let name: String = sh.name().chars().take(cols).collect();
            c.text(tx, t.y + 4 * s, &name, 0x101010, Font::Normal);
            let detail = if sh.client.is_some() { format!("{}, signed in as {}", sh.unc(), sh.user) } else { format!("{}, not connected", sh.unc()) };
            let small_cols = ((t.x + t.w - 8 * s - tx) / Font::Small.w(s)).max(1) as usize;
            let detail: String = detail.chars().take(small_cols).collect();
            c.text(tx, t.y + 34 * s, &detail, rgb(90, 95, 105), Font::Small);
        }
    }

    // The "Map network drive" form, in Computer's content area.
    fn form_field(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let k = self.files_content(c);
        Rect::new(k.x + 20 * s, k.y + 96 * s + i as i32 * 62 * s, (k.w - 40 * s).min(440 * s), 30 * s)
    }
    fn form_button(&self, c: &Rect, connect: bool) -> Rect {
        let s = self.ui;
        let last = self.form_field(c, 2);
        Rect::new(last.x + if connect { 0 } else { 116 * s }, last.y + last.h + 16 * s, 106 * s, 32 * s)
    }

    fn form_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let Some(form) = &self.files.form else { return };
        let k = self.files_content(client);
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        c.text(k.x + 20 * s, k.y + 10 * s, "Map a network drive", rgb(30, 57, 145), Font::Normal);
        c.text(k.x + 20 * s, k.y + 40 * s, "A shared folder on another computer,", rgb(80, 85, 95), Font::Small);
        c.text(k.x + 20 * s, k.y + 60 * s, "like \\\\192.168.1.20\\Users", rgb(80, 85, 95), Font::Small);
        let fw = Font::Normal.w(s);
        for (i, label) in ["Folder", "User name", "Password"].iter().enumerate() {
            let r = self.form_field(client, i);
            c.text(r.x, r.y - 22 * s, label, rgb(40, 50, 70), Font::Small);
            c.fill(r, 0xFFFFFF);
            let focused = form.focus == i;
            c.frame(r, if focused { rgb(60, 110, 190) } else { rgb(150, 160, 175) });
            let text: String = if i == 2 { "*".repeat(form.fields[2].chars().count()) } else { form.fields[i].clone() };
            let room = ((r.w - 12 * s) / fw).max(1) as usize;
            let skip = text.chars().count().saturating_sub(room.saturating_sub(1));
            let shown: String = text.chars().skip(skip).collect();
            let ty = r.y + (r.h - Font::Normal.h(s)) / 2;
            c.text(r.x + 6 * s, ty, &shown, 0x101010, Font::Normal);
            if focused {
                let caret = r.x + 6 * s + shown.chars().count() as i32 * fw;
                c.fill(Rect::new(caret, ty + 2 * s, s.max(2), Font::Normal.h(s) - 4 * s), 0x101010);
            }
        }
        for (connect, label) in [(true, "Connect"), (false, "Cancel")] {
            let b = self.form_button(client, connect);
            let hot = b.contains(px, py);
            let (top, bottom) = if hot { (rgb(250, 252, 255), rgb(190, 215, 245)) } else { (rgb(250, 251, 253), rgb(220, 228, 238)) };
            c.rounded(b, 3 * s, false, top, bottom, 255);
            c.rounded_outline(b, 3 * s, false, rgb(110, 130, 160), 255);
            c.text(b.x + (b.w - Font::Normal.width(s, label)) / 2, b.y + (b.h - Font::Normal.h(s)) / 2, label, rgb(20, 40, 80), Font::Normal);
        }
        if !form.error.is_empty() {
            let b = self.form_button(client, true);
            let room = ((k.x + k.w - 20 * s - b.x) / Font::Small.w(s)).max(1) as usize;
            let e: String = form.error.chars().take(room).collect();
            c.text(b.x, b.y + b.h + 12 * s, &e, rgb(170, 30, 20), Font::Small);
        }
    }

    /// Opens "Map network drive", empty or to sign in to saved drive `share` again.
    fn open_form(&mut self, share: Option<usize>) -> Rect {
        if !self.files.path.is_empty() {
            self.navigate(String::new());
        }
        let mut fields = [String::new(), String::new(), String::new()];
        let mut focus = 0;
        if let Some(i) = share {
            fields[0] = shares()[i].unc();
            fields[1] = shares()[i].user.clone();
            focus = 2;
        }
        self.files.form = Some(MapForm { fields, focus, error: String::new(), share });
        self.files.status = String::from("Tab moves between the boxes, Enter connects, Esc cancels");
        let c = self.files_client();
        let f: Vec<String> = (0..3).map(|i| { let r = self.form_field(&c, i); format!("{},{}", r.x + 20 * self.ui, r.y + r.h / 2) }).collect();
        let b = self.form_button(&c, true);
        println!("[desktop] map form: fields at {}; Connect at {},{}", f.join(" | "), b.x + b.w / 2, b.y + b.h / 2);
        self.files_area()
    }

    /// Connects the drive the form describes.
    fn form_connect(&mut self) -> Rect {
        let area = self.files_area();
        let Some(form) = self.files.form.as_mut() else { return area };
        let folder = form.fields[0].trim().replace('\\', "/");
        let mut parts = folder.trim_start_matches('/').split('/').filter(|p| !p.is_empty());
        let (Some(host), Some(share)) = (parts.next(), parts.next()) else {
            form.error = String::from("Type the folder as \\\\computer\\share");
            form.focus = 0;
            return area;
        };
        let (host, share) = (String::from(host), String::from(share));
        let (user, password) = (String::from(form.fields[1].trim()), form.fields[2].clone());
        if user.is_empty() {
            form.error = String::from("Type the user name to sign in with");
            form.focus = 1;
            return area;
        }
        let again = form.share;
        let unc = format!("\\\\{}\\{}", host, share);
        match aero::smb::Client::connect(&host, &share, &user, &password) {
            Ok(client) => {
                let existing = again.or_else(|| shares().iter().position(|s| s.host.eq_ignore_ascii_case(&host) && s.share.eq_ignore_ascii_case(&share)));
                let i = match existing {
                    Some(i) => {
                        let sh = &mut shares()[i];
                        sh.host = host;
                        sh.share = share;
                        sh.user = user;
                        i
                    }
                    None => {
                        let used: Vec<char> = shares().iter().map(|s| s.letter).collect();
                        let letter = ('M'..='Z').rev().find(|l| !used.contains(l)).unwrap_or('Z');
                        shares().push(NetShare { host, share, user, password: None, client: None, letter });
                        shares().len() - 1
                    }
                };
                let sh = &mut shares()[i];
                sh.password = Some(password);
                sh.client = Some(client);
                println!("[desktop] mapped {} as {}: signed in as {}", sh.unc(), sh.letter, sh.user);
                let root = sh.root();
                self.files.form = None;
                self.save_settings();
                self.navigate(root)
            }
            Err(msg) => {
                println!("[desktop] could not map {}: {}", unc, msg);
                if let Some(form) = self.files.form.as_mut() {
                    form.error = format!("Can't connect: {}", msg);
                    form.focus = if msg.contains("password") { 2 } else { 0 };
                }
                area
            }
        }
    }

    /// A key typed while the form is open.
    fn form_key(&mut self, k: u8) -> Rect {
        let area = self.files_area();
        let Some(form) = self.files.form.as_mut() else { return Rect::EMPTY };
        match k {
            b'\n' => return self.form_connect(),
            27 => {
                self.files.form = None;
                self.files.status = String::new();
            }
            9 | KEY_DOWN => form.focus = (form.focus + 1) % 3,
            KEY_UP => form.focus = (form.focus + 2) % 3,
            8 => {
                form.fields[form.focus].pop();
            }
            32..=126 if form.fields[form.focus].len() < 200 => form.fields[form.focus].push(k as char),
            _ => return Rect::EMPTY,
        }
        area
    }

    /// A click in the content area while the form is open.
    fn form_click(&mut self, x: i32, y: i32) -> Rect {
        let c = self.files_client();
        if self.form_button(&c, true).contains(x, y) {
            return self.form_connect();
        }
        if self.form_button(&c, false).contains(x, y) {
            return self.form_key(27);
        }
        if let Some(i) = (0..3).find(|&i| self.form_field(&c, i).contains(x, y)) {
            if let Some(form) = self.files.form.as_mut() {
                form.focus = i;
            }
            return self.files_area();
        }
        Rect::EMPTY
    }

    /// Writes /AeroForge.ini: the mouse speed and the network drives
    /// (where and who; never the password).
    fn save_settings(&self) -> bool {
        let p = &self.prefs;
        let mut text = format!("mouse speed = {}\n", self.mouse_speed);
        text.push_str(&format!("double-click = {}\nswap buttons = {}\n", p.double_ms, if p.swapped { "yes" } else { "no" }));
        text.push_str(&format!("volume = {}\nmuted = {}\n", p.volume, if p.muted { "yes" } else { "no" }));
        text.push_str(&format!("background = {}\n", BACKGROUNDS[p.background].0));
        text.push_str(&format!("text size = {}\n", match p.text_size { 1 => "normal", 2 => "large", _ => "auto" }));
        text.push_str(&format!("font size = {}\n", font_name(p.font)));
        text.push_str(&format!("clock = {}\n", if p.hours24 { "24-hour" } else { "12-hour" }));
        for sh in shares().iter() {
            text.push_str(&format!("network drive = {}|{}|{}\n", sh.unc(), sh.user, sh.letter));
        }
        aero::write_file(SETTINGS, text.as_bytes()).is_ok()
    }

    /// A folder: column headings, the rows and a scroll bar.
    /// A folder's entries in columns, in area `k`: the folder in use
    /// (`active`), or in two-pane mode the other side (its selection grey).
    /// Entry `i` in Large icons: a big icon, its name under it on up to
    /// two lines (or the name being typed, in an edit box).
    fn icon_cell(&self, c: &mut Canvas, f: &Files, i: usize, r: Rect) {
        let s = self.ui;
        let e = &f.entries[i];
        let big = 3 * s;
        let cx = r.x + r.w / 2;
        if e.is_dir {
            c.folder(cx - 8 * big, r.y + 8 * s + 2 * big, big);
        } else {
            c.file_icon(cx - 6 * big, r.y + 8 * s, big, file_kind(&e.name));
        }
        let fw = Font::Normal.w(s);
        let cols = ((r.w - 4 * s) / fw).max(1) as usize;
        let line_h = Font::Normal.h(s);
        let ty = r.y + 8 * s + 14 * big + 4 * s;
        if let (Some(new), true) = (&f.rename, f.selected == Some(i)) {
            let skip = new.chars().count().saturating_sub(cols.saturating_sub(1));
            let shown: String = new.chars().skip(skip).collect();
            let edit = Rect::new(r.x + 2 * s, ty - 2 * s, r.w - 4 * s, line_h + 4 * s);
            c.fill(edit, 0xFFFFFF);
            c.frame(edit, rgb(60, 110, 190));
            let text_w = shown.chars().count() as i32 * fw;
            let ink = if f.rename_all {
                c.fill(Rect::new(r.x + 5 * s, ty, text_w + 2 * s, line_h), rgb(51, 153, 255));
                0xFFFFFF
            } else {
                0x101010
            };
            c.text(r.x + 6 * s, ty, &shown, ink, Font::Normal);
            c.fill(Rect::new(r.x + 6 * s + text_w, ty + 2 * s, s.max(2), line_h - 4 * s), 0x101010);
            return;
        }
        let cut = self.clip.as_ref().is_some_and(|k| k.cut && k.items.iter().any(|it| it.path == f.entry_path(i)));
        let ink = if cut { rgb(140, 140, 140) } else { 0x101010 };
        // Two lines: break at a space, a dot or a dash when one is near the end.
        let chars: Vec<char> = e.name.chars().collect();
        let (first, rest) = if chars.len() <= cols {
            (chars.clone(), Vec::new())
        } else {
            let cut_at = chars[..cols].iter().rposition(|&ch| matches!(ch, ' ' | '-' | '_' | '.')).filter(|&p| p >= cols / 2).map_or(cols, |p| p + 1);
            (chars[..cut_at].to_vec(), chars[cut_at..].to_vec())
        };
        let mut second: String = rest.iter().collect();
        if rest.len() > cols {
            second = rest[..cols.saturating_sub(3)].iter().collect::<String>() + "...";
        }
        for (n, line) in [first.iter().collect::<String>(), second].iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let w = line.chars().count() as i32 * fw;
            c.text(cx - w / 2, ty + n as i32 * line_h, line.trim_end(), ink, Font::Normal);
        }
    }

    fn list_view(&self, c: &mut Canvas, f: &Files, k: Rect, active: bool) {
        let s = self.ui;
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let fw = Font::Normal.w(s);
        let (date_x, type_x, size_x) = self.columns_in(&k);
        let head = self.head_in(&k);
        let rows = self.rows_in(&k);
        let head_y = head.y + (head.h - Font::Normal.h(s)) / 2;
        let type_label = if f.path == BIN {
            "Original location"
        } else if f.path == SEARCH {
            "In folder"
        } else {
            "Type"
        };
        // Narrow (one side of two): no Size column.
        let sizes = size_x < rows.x + rows.w - 4 * s;
        let dates = date_x < type_x;
        for (by, label, x, end) in [
            (SortBy::Name, "Name", rows.x + 30 * s, date_x),
            (SortBy::Date, "Date modified", date_x, type_x),
            (SortBy::Type, type_label, type_x, size_x),
            (SortBy::Size, "Size", size_x, rows.x + rows.w),
        ] {
            if (by == SortBy::Size && !sizes) || (by == SortBy::Date && !dates) {
                continue;
            }
            // Headings that don't fit their column are cut short.
            let fit = ((end - x - 16 * s) / fw).max(1) as usize;
            let label: String = label.chars().take(fit).collect();
            let label = label.as_str();
            c.text(x, head_y, label, rgb(60, 80, 110), Font::Normal);
            if f.sort == by {
                // A small triangle: up for A-Z (smallest first), down for Z-A.
                let (cx, cy) = (x + Font::Normal.width(s, label) + 8 * s, head.y + head.h / 2);
                for i in 0..4 * s {
                    let half = if f.descending { 4 * s - i } else { i };
                    c.fill(Rect::new(cx - half, cy - 2 * s + i, 2 * half + 1, 1), rgb(100, 130, 170));
                }
            }
            if x > rows.x + 30 * s {
                c.fill(Rect::new(x - 8 * s, head.y + 4 * s, 1, head.h - 8 * s), rgb(220, 225, 235));
            }
        }
        c.fill(Rect::new(head.x, head.y + head.h - 1, head.w, 1), rgb(225, 230, 240));
        if f.entries.is_empty() {
            let empty = if f.path == BIN {
                "The Recycle Bin is empty."
            } else if f.path == DELETED {
                "Nothing deleted is left in this folder."
            } else if f.path == SEARCH {
                "No items match your search."
            } else {
                "This folder is empty."
            };
            c.text(rows.x + 30 * s, rows.y + 10 * s, empty, rgb(120, 125, 135), Font::Normal);
        }
        let name_cols = ((date_x - 12 * s - rows.x - 30 * s) / fw).max(1) as usize;
        let type_cols = ((size_x - 12 * s - type_x) / fw).max(1) as usize;
        for (i, e) in f.entries.iter().enumerate().skip(f.scroll).take(self.visible_in(&k)) {
            let r = self.row_in(&k, i - f.scroll);
            let box_ = Rect::new(r.x + 2 * s, r.y, r.w - 4 * s, r.h);
            if f.is_marked(i) && active {
                c.rounded(box_, 2 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(box_, 2 * s, false, rgb(125, 162, 206), 255);
            } else if f.is_marked(i) {
                c.rounded(box_, 2 * s, false, rgb(236, 238, 242), rgb(222, 225, 231), 255);
            } else if r.contains(px, py) {
                c.rounded(box_, 2 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
            }
            if self.icons {
                self.icon_cell(c, f, i, r);
                continue;
            }
            if e.is_dir {
                c.folder(r.x + 8 * s, r.y + (r.h - 12 * s) / 2, s);
            } else {
                c.file_icon(r.x + 10 * s, r.y + (r.h - 14 * s) / 2, s, file_kind(&e.name));
            }
            let ty = r.y + (r.h - Font::Normal.h(s)) / 2;
            match (&f.rename, f.selected == Some(i)) {
                (Some(new), true) => {
                    // The name being typed, in an edit box with a caret.
                    let skip = new.chars().count().saturating_sub(name_cols.saturating_sub(1));
                    let shown: String = new.chars().skip(skip).collect();
                    let edit = Rect::new(rows.x + 26 * s, r.y + 2 * s, date_x - 8 * s - rows.x - 26 * s, r.h - 4 * s);
                    c.fill(edit, 0xFFFFFF);
                    c.frame(edit, rgb(60, 110, 190));
                    let text_w = shown.chars().count() as i32 * fw;
                    let ink = if f.rename_all {
                        c.fill(Rect::new(rows.x + 29 * s, ty, text_w + 2 * s, Font::Normal.h(s)), rgb(51, 153, 255));
                        0xFFFFFF
                    } else {
                        0x101010
                    };
                    c.text(rows.x + 30 * s, ty, &shown, ink, Font::Normal);
                    let caret = rows.x + 30 * s + text_w;
                    c.fill(Rect::new(caret, ty + 2 * s, s.max(2), Font::Normal.h(s) - 4 * s), 0x101010);
                }
                _ => {
                    let cut = self.clip.as_ref().is_some_and(|k| k.cut && k.items.iter().any(|it| it.path == f.entry_path(i)));
                    let ink = if cut { rgb(140, 140, 140) } else { 0x101010 };
                    let name: String = if e.name.chars().count() > name_cols {
                        let mut n: String = e.name.chars().take(name_cols.saturating_sub(3)).collect();
                        n.push_str("...");
                        n
                    } else {
                        e.name.clone()
                    };
                    c.text(rows.x + 30 * s, ty, &name, ink, Font::Normal);
                }
            }
            let kind = match f.bin.get(i) {
                Some(item) if f.path == BIN => split_path(&item.original).0,
                _ if f.path == SEARCH => split_path(&f.found[i]).0,
                _ if f.path == DELETED => String::from(match f.lost_at(i) {
                    Some(d) if d.whole => "Can be restored",
                    _ => "Written over",
                }),
                _ => type_text(e),
            };
            if dates && e.modified != 0 {
                c.text(date_x, ty, &date_text(e.modified), rgb(90, 90, 90), Font::Normal);
            }
            let kind: String = kind.chars().take(type_cols).collect();
            c.text(type_x, ty, &kind, rgb(90, 90, 90), Font::Normal);
            if !e.is_dir && sizes {
                c.text(size_x, ty, &size_text(e.size), rgb(80, 80, 80), Font::Normal);
            }
        }

        // Scroll bar: arrows at the ends, the thumb showing what part is in view.
        let sb = self.scrollbar_in(&k);
        c.fill(sb, rgb(240, 241, 244));
        let visible = self.visible_in(&k);
        let n = f.entries.len();
        let arrow = 16 * s;
        for (down, y) in [(false, sb.y), (true, sb.y + sb.h - arrow)] {
            let b = Rect::new(sb.x, y, sb.w, arrow);
            c.gradient(b, rgb(250, 251, 253), rgb(220, 225, 232), 255);
            c.frame(b, rgb(190, 196, 206));
            let ink = if n > visible { rgb(60, 70, 90) } else { rgb(180, 185, 192) };
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            for i in 0..3 * s {
                let half = if down { 3 * s - i } else { i };
                c.fill(Rect::new(cx - half, cy - s - s / 2 + i, 2 * half + 1, 1), ink);
            }
        }
        if n > visible {
            let track = Rect::new(sb.x, sb.y + arrow, sb.w, sb.h - 2 * arrow);
            let thumb_h = (track.h as usize * visible / n).max(12 * s as usize) as i32;
            let thumb_y = track.y + ((track.h - thumb_h) as usize * f.scroll / (n - visible).max(1)) as i32;
            let thumb = Rect::new(track.x + 2 * s, thumb_y, track.w - 4 * s, thumb_h);
            c.rounded(thumb, 3 * s, false, rgb(235, 238, 243), rgb(200, 207, 218), 255);
            c.rounded_outline(thumb, 3 * s, false, rgb(150, 160, 178), 255);
        }
    }

    /// What lights up under the pointer in the Computer window.
    fn files_spots(&self, c: &Rect) -> Vec<Rect> {
        let mut spots = Vec::new();
        for i in 0..3 {
            let (x, y, r) = self.files_arrow(c, i);
            spots.push(Rect::new(x - r, y - r, 2 * r + 1, 2 * r + 1));
        }
        spots.extend(self.crumb_rects(c).into_iter().map(|(r, _, _)| r));
        spots.extend(self.command_buttons(c).into_iter().map(|(_, _, r)| r));
        let rows = self.places();
        spots.extend((0..rows.len()).flat_map(|i| [self.pane_item(c, i), self.pane_arrow(c, i, rows[i].depth)]));
        spots.extend((0..self.tabs.len()).flat_map(|i| [self.tab_rect(c, i), self.tab_close(c, i)]));
        spots.push(self.new_tab_button(c));
        spots.push(self.panes_button(c));
        spots.push(self.preview_button(c));
        spots.push(self.view_button(c));
        if self.files.form.is_some() {
            spots.push(self.form_button(c, true));
            spots.push(self.form_button(c, false));
        } else if self.files.path.is_empty() {
            spots.extend((0..self.tile_count()).map(|i| self.drive_tile(c, i)));
        } else {
            spots.extend((0..self.files_visible(c)).map(|i| self.files_row(c, i)));
        }
        spots
    }

    // -------------------------------------------------------------- actions

    /// Shows `path` ("" for Computer), without touching Back and Forward.
    /// Returns false if it could not be read.
    fn open_dir(&mut self, path: String) -> bool {
        self.files.rename = None;
        self.files.confirm = None;
        self.files.props = None;
        if !path.is_empty() {
            self.files.form = None;
        }
        if let Ok(drives) = aero::volumes() {
            self.files.drives = drives;
        }
        if path == DELETED {
            let at = self.files.lost_in.clone();
            match aero::deleted_list(&at) {
                Ok(mut lost) => {
                    self.files.lost_bin.clear();
                    if bin_folder(&at) {
                        // Only the files emptied from the bin, under their own names.
                        let gone = Self::read_list(&split_path(&at).0, BIN_GONE);
                        lost.retain(|d| !d.is_dir && gone.iter().any(|g| same_id(&d.name, &g.id)));
                        self.files.lost_bin = lost.iter().map(|d| gone.iter().rev().find(|g| same_id(&d.name, &g.id)).unwrap().clone()).collect();
                    }
                    self.files.lost = lost;
                }
                Err(e) => {
                    self.files.status = format!("Can't look for deleted files there: {}", fs_error_text(e));
                    println!("[desktop] deleted files in {}: {}", at, fs_error_text(e));
                    return false;
                }
            }
            self.files.entries = self.files.lost.iter().enumerate()
                .map(|(i, d)| aero::DirEntry {
                    name: self.files.lost_bin.get(i).map_or_else(|| d.name.clone(), |g| split_path(&g.original).1),
                    is_dir: d.is_dir,
                    size: d.size,
                    modified: d.modified,
                })
                .collect();
            self.files.found = self.files.lost.iter().map(|d| format!("{}", d.slot)).collect();
            let whole = self.files.lost.iter().filter(|d| d.whole).count();
            let n = self.files.lost.len();
            self.files.status = format!("{} deleted item{}, {} can be restored", n, if n == 1 { "" } else { "s" }, whole);
            self.files.path = path;
            self.sort_entries();
            self.files.scroll = 0;
            self.files.selected = None;
            let list: Vec<String> = (0..self.files.entries.len())
                .map(|i| format!("{} ({})", self.files.entries[i].name, if self.files.lost_at(i).is_some_and(|d| d.whole) { "whole" } else { "written over" }))
                .collect();
            let c = self.files_client();
            let row = self.files_row(&c, 0);
            println!("[desktop] deleted files in {} = {}; first row at {},{}, rows {} apart",
                at, list.join(" | "), row.x + 40 * self.ui, row.y + row.h / 2, row.h);
            return true;
        }
        if path == SEARCH {
            self.files.entries = self.files.results.iter().map(|(_, e)| e.clone()).collect();
            self.files.found = self.files.results.iter().map(|(p, _)| p.clone()).collect();
            let n = self.files.found.len();
            self.files.status = format!("{} item{} found", n, if n == 1 { "" } else { "s" });
            self.files.path = path;
            self.sort_entries();
            self.files.scroll = 0;
            self.files.selected = None;
            return true;
        }
        if path == BIN {
            self.load_bin();
            let names: Vec<String> = self.files.bin.iter().map(|b| format!("{} (from {})", split_path(&b.original).1, b.original)).collect();
            println!("[desktop] Recycle Bin = {}", names.join(" | "));
            let n = self.files.bin.len();
            self.files.status = format!("{} item{}", n, if n == 1 { "" } else { "s" });
            self.files.path = path;
            self.files.scroll = 0;
            self.files.selected = None;
            return true;
        }
        if path.is_empty() {
            self.files.path = path;
            let c = self.files_client();
            let mut tiles: Vec<String> = (0..self.files.drives.len())
                .map(|i| {
                    let t = self.drive_tile(&c, i);
                    let v = &self.files.drives[i];
                    format!("{}: {} {} at {},{}", (b'C' + i as u8) as char, v.path, v.kind, t.x + t.w / 2, t.y + t.h / 2)
                })
                .collect();
            for (n, sh) in shares().iter().enumerate() {
                let t = self.drive_tile(&c, self.files.drives.len() + n);
                tiles.push(format!("{}: {} SMB at {},{}", sh.letter, sh.root(), t.x + t.w / 2, t.y + t.h / 2));
            }
            let bin = self.pane_item(&c, 0);
            let map = self.command_buttons(&c).into_iter().find(|(cmd, _, _)| *cmd == Command::Map).map(|(_, _, r)| r).unwrap_or(Rect::EMPTY);
            println!("[desktop] Computer: drives = {}; Recycle Bin at {},{}; Map network drive at {},{}", tiles.join(" | "),
                bin.x + 60 * self.ui, bin.y + bin.h / 2, map.x + map.w / 2, map.y + map.h / 2);
            self.log_pane();
            let n = self.tile_count();
            self.files.status = format!("{} drive{}", n, if n == 1 { "" } else { "s" });
            self.files.entries.clear();
            self.files.scroll = 0;
            self.files.selected = None;
            return true;
        }
        match fs_list(&path) {
            Ok(mut entries) => {
                // The first drive's root also lists the other drives as
                // folders; here they are in the pane instead.
                if path == "/" {
                    let others: Vec<String> = self.files.drives.iter().filter(|v| v.path != "/").map(|v| String::from(&v.path[1..])).collect();
                    entries.retain(|e| !(e.is_dir && e.size == 0 && others.iter().any(|o| o.eq_ignore_ascii_case(&e.name))));
                }
                // Each drive's Recycle Bin folder shows up as the Recycle Bin instead.
                if self.files.drives.iter().any(|v| v.path.eq_ignore_ascii_case(path.trim_end_matches('/')) || (v.path == "/" && path == "/")) {
                    entries.retain(|e| !(e.is_dir && e.name.eq_ignore_ascii_case(BIN_DIR)));
                }
                self.files.entries = entries;
                // Sorted as a folder (not as the Recycle Bin or search results it may come from).
                let from = core::mem::replace(&mut self.files.path, path.clone());
                self.sort_entries();
                self.files.path = from;
                self.files.found.clear();
                let n = self.files.entries.len();
                self.files.status = format!("{} item{}", n, if n == 1 { "" } else { "s" });
                self.tree_reveal(&path);
                let names: Vec<&str> = self.files.entries.iter().map(|e| e.name.as_str()).collect();
                let c = self.files_client();
                let row = self.files_row(&c, 0);
                let dated = self.files.entries.iter().filter(|e| e.modified != 0).count();
                println!("[desktop] Computer: {} = {}; first row at {},{}, rows {} apart; {} dated",
                    path, names.join(" | "), row.x + 40 * self.ui, row.y + row.h / 2, row.h, dated);
                self.files.path = path;
                self.files.scroll = 0;
                self.files.selected = None;
                self.log_pane();
                true
            }
            Err(e) => {
                println!("[desktop] Computer: cannot open {} ({})", path, e);
                self.files.status = format!("Can't open {}: {}", path, fs_error_text(e));
                false
            }
        }
    }

    /// Goes to `path`, remembering where it was for Back.
    fn navigate(&mut self, path: String) -> Rect {
        let from = self.files.path.clone();
        if path != from && self.open_dir(path) {
            self.files.back.push(from);
            self.files.forward.clear();
        }
        self.files_area()
    }

    fn go_back(&mut self) -> Rect {
        if let Some(to) = self.files.back.pop() {
            let from = self.files.path.clone();
            if self.open_dir(to.clone()) {
                self.files.forward.push(from);
            } else {
                self.files.back.push(to);
            }
        }
        self.files_area()
    }

    fn go_forward(&mut self) -> Rect {
        if let Some(to) = self.files.forward.pop() {
            let from = self.files.path.clone();
            if self.open_dir(to.clone()) {
                self.files.back.push(from);
            } else {
                self.files.forward.push(to);
            }
        }
        self.files_area()
    }

    /// The folder above the one shown; Computer above a drive.
    fn go_up(&mut self) -> Rect {
        let path = self.files.path.clone();
        if path.is_empty() {
            return Rect::EMPTY;
        }
        if path == BIN {
            return self.navigate(String::new());
        }
        if path == SEARCH {
            let back = self.files.searched_in.clone();
            return self.navigate(back);
        }
        if path == DELETED {
            let back = if bin_folder(&self.files.lost_in) { String::from(BIN) } else { self.files.lost_in.clone() };
            return self.navigate(back);
        }
        let at_root = self.drive_of(&path).is_some_and(|d| self.files.drives[d].path.eq_ignore_ascii_case(path.trim_end_matches('/')) || path == "/")
            || net_path(&path).is_some_and(|(_, inner)| inner.trim_matches('/').is_empty());
        let up = if at_root { String::new() } else { split_path(&path).0 };
        let name = split_path(&path).1;
        let r = self.navigate(up);
        // Select the folder just left, as Explorer does.
        if let Some(i) = self.files.entries.iter().position(|e| e.name == name) {
            self.select(i);
        }
        r
    }

    /// Runs `f` with the other side of two panes in use, as if it were
    /// the side in front (so its rows are where it is drawn).
    fn with_other<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let mut other = self.other.take()?;
        core::mem::swap(&mut self.files, &mut *other);
        self.right = !self.right;
        self.other = Some(other);
        let r = f(self);
        let mut other = self.other.take().unwrap();
        core::mem::swap(&mut self.files, &mut *other);
        self.right = !self.right;
        self.other = Some(other);
        Some(r)
    }

    /// Two panes: the other side comes into use.
    fn switch_side(&mut self) -> Rect {
        if self.files.rename.is_some() {
            self.finish_rename();
        }
        let Some(mut other) = self.other.take() else { return Rect::EMPTY };
        self.files.query_focus = false;
        self.files.confirm = None;
        self.files.props = None;
        core::mem::swap(&mut self.files, &mut *other);
        self.other = Some(other);
        self.right = !self.right;
        self.log_panes();
        self.files_area()
    }

    /// Two panes on or off. A new side starts in the same folder; the
    /// side in use stays when they close.
    fn toggle_panes(&mut self) -> Rect {
        if self.other.take().is_some() {
            self.right = false;
            println!("[desktop] two panes: off, {}", self.place_path(&self.files));
            return self.files_area();
        }
        let path = if self.files.in_folder() { self.files.path.clone() } else { String::new() };
        let mut other = Box::new(Files::new());
        other.drives = self.files.drives.clone();
        self.other = Some(other);
        self.right = false;
        self.with_other(|d| d.open_dir(path));
        self.log_panes();
        self.files_area()
    }

    fn log_panes(&self) {
        let Some(other) = &self.other else { return };
        let c = self.files_client();
        let (left, right) = if self.right { (&**other, &self.files) } else { (&self.files, &**other) };
        let (l, r) = (self.side_header(&c, false), self.side_header(&c, true));
        let over: Vec<String> = self
            .command_buttons(&c)
            .into_iter()
            .filter(|(cmd, _, _)| matches!(cmd, Command::CopyOver | Command::MoveOver))
            .map(|(_, label, b)| format!("; {} at {},{}", label, b.x + b.w / 2, b.y + b.h / 2))
            .collect();
        println!("[desktop] two panes: left = {}, right = {}, {} in use; headers at {},{} and {},{}{}",
            self.place_path(left), self.place_path(right), if self.right { "right" } else { "left" },
            l.x + l.w / 2, l.y + l.h / 2, r.x + r.w / 2, r.y + r.h / 2, over.concat());
    }

    fn log_tabs(&self) {
        let c = self.files_client();
        let tabs: Vec<String> = (0..self.tabs.len())
            .map(|i| {
                let t = self.tab_rect(&c, i);
                format!("{} at {},{}", self.tab_title(i), t.x + t.w / 2, t.y + t.h / 2)
            })
            .collect();
        let (plus, panes) = (self.new_tab_button(&c), self.panes_button(&c));
        println!("[desktop] tabs: {}; tab {} in front; + at {},{}; Two panes at {},{}", tabs.join(" | "), self.tab + 1,
            plus.x + plus.w / 2, plus.y + plus.h / 2, panes.x + panes.w / 2, panes.y + panes.h / 2);
        self.log_pane();
    }

    /// Brings tab `i` to the front.
    fn switch_tab(&mut self, i: usize) -> Rect {
        if i == self.tab || i >= self.tabs.len() {
            return Rect::EMPTY;
        }
        if self.files.rename.is_some() {
            self.finish_rename();
        }
        self.files.query_focus = false;
        self.file_drag = None;
        let back = core::mem::take(&mut self.tabs[i]);
        let front = Tab {
            files: core::mem::replace(&mut self.files, back.files),
            other: core::mem::replace(&mut self.other, back.other),
            right: core::mem::replace(&mut self.right, back.right),
        };
        self.tabs[self.tab] = front;
        self.tab = i;
        // What it shows may have changed while it was behind.
        let keep = self.selected_entry().map(|e| e.name);
        if self.files.in_folder() {
            self.refresh(keep);
        }
        self.with_other(|d| {
            if d.files.in_folder() {
                let keep = d.selected_entry().map(|e| e.name);
                d.refresh(keep);
            }
        });
        self.log_tabs();
        self.files_area()
    }

    /// A new tab, showing Computer, in front.
    fn new_tab(&mut self) -> Rect {
        if self.tabs.len() >= 12 {
            self.files.status = String::from("That's as many tabs as there is room for");
            return self.files_area();
        }
        let mut tab = Tab::default();
        tab.files.drives = self.files.drives.clone();
        self.tabs.push(tab);
        let n = self.tabs.len() - 1;
        self.switch_tab(n);
        self.open_dir(String::new());
        self.log_tabs();
        self.files_area()
    }

    fn close_tab(&mut self, i: usize) -> Rect {
        if self.tabs.len() < 2 || i >= self.tabs.len() {
            return Rect::EMPTY;
        }
        if i == self.tab {
            self.switch_tab(if i + 1 < self.tabs.len() { i + 1 } else { i - 1 });
        }
        self.tabs.remove(i);
        if self.tab > i {
            self.tab -= 1;
        }
        self.log_tabs();
        self.files_area()
    }

    /// The preview pane on or off.
    /// Large icons or Details, for every folder shown.
    fn toggle_icons(&mut self) -> Rect {
        self.icons = !self.icons;
        let c = self.files_client();
        let across = self.files_across(&c);
        self.files.scroll -= self.files.scroll % across;
        self.with_other(|d| d.files.scroll -= d.files.scroll % across);
        if let Some(i) = self.files.selected {
            self.scroll_to(i);
        }
        let first = self.files_row(&c, 0);
        println!("[desktop] view: {}; first item at {},{}, {} across", if self.icons { "Large icons" } else { "Details" },
            first.x + first.w / 2, first.y + first.h / 2, across);
        self.files_area()
    }

    fn toggle_preview(&mut self) -> Rect {
        self.preview_on = !self.preview_on;
        self.preview = None;
        if self.preview_on {
            let c = self.files_client();
            let (t, h) = (self.preview_mode(&c, false), self.preview_mode(&c, true));
            println!("[desktop] preview pane on; Text at {},{}, Hex at {},{}", t.x + t.w / 2, t.y + t.h / 2, h.x + h.w / 2, h.y + h.h / 2);
            self.sync_preview();
        } else {
            println!("[desktop] preview pane off");
        }
        self.files_area()
    }

    /// Reads what is selected into the preview pane, if that changed.
    fn sync_preview(&mut self) -> Rect {
        if !self.preview_on {
            return Rect::EMPTY;
        }
        let one = self.files.selected.filter(|&i| {
            (self.files.in_folder() || self.files.path == SEARCH) && i < self.files.entries.len() && self.picked().len() <= 1
        });
        let Some(i) = one else {
            return if self.preview.take().is_some() { self.files_area() } else { Rect::EMPTY };
        };
        let path = self.entry_path(i);
        if self.preview.as_ref().is_some_and(|p| p.path == path) {
            return Rect::EMPTY;
        }
        let entry = self.files.entries[i].clone();
        let mut p = Preview { path: path.clone(), entry, data: Vec::new(), lines: Vec::new(), hex: false, items: None, error: None, scroll: 0 };
        if p.entry.is_dir {
            match fs_list(&path) {
                Ok(list) => p.items = Some(list.len()),
                Err(e) => p.error = Some(String::from(fs_error_text(e))),
            }
            println!("[desktop] preview of {}: folder, {} items", path, p.items.unwrap_or(0));
        } else {
            match fs_read(&path, PREVIEW_BYTES) {
                Ok(data) => p.data = data,
                Err(e) => p.error = Some(String::from(fs_error_text(e))),
            }
            match text_lines(&p.data) {
                Some(lines) => p.lines = lines,
                None => p.hex = true,
            }
            let first = if p.hex { hex_row(&p.data, 0, 16, true) } else { p.lines.first().cloned().unwrap_or_default() };
            println!("[desktop] preview of {}: {} bytes read, as {}: {}", path, p.data.len(), if p.hex { "hex" } else { "text" }, first);
        }
        self.preview = Some(p);
        self.files_area()
    }

    /// Text or Hex, for a file that reads as text.
    fn preview_as(&mut self, hex: bool) -> Rect {
        let c = self.files_client();
        let (width, chars) = self.hex_layout(&c);
        let Some(p) = self.preview.as_mut() else { return Rect::EMPTY };
        if p.entry.is_dir || p.hex == hex || (!hex && p.lines.is_empty() && !p.data.is_empty()) {
            return Rect::EMPTY;
        }
        p.hex = hex;
        p.scroll = 0;
        let first = if hex { hex_row(&p.data, 0, width, chars) } else { p.lines.first().cloned().unwrap_or_default() };
        println!("[desktop] preview of {} as {}: {}", p.path, if hex { "hex" } else { "text" }, first);
        self.files_area()
    }

    /// Reads the folder again, keeping the selection on `keep` if it is there.
    fn refresh(&mut self, keep: Option<String>) {
        let scroll = self.files.scroll;
        let path = self.files.path.clone();
        self.open_dir(path);
        self.files.scroll = scroll.min(self.files.entries.len().saturating_sub(1));
        if let Some(i) = keep.and_then(|k| self.files.entries.iter().position(|e| e.name.eq_ignore_ascii_case(&k))) {
            self.select(i);
        }
    }

    fn sort_entries(&mut self) {
        if self.files.path == BIN {
            return;
        }
        let (by, desc) = (self.files.sort, self.files.descending);
        if self.files.path == SEARCH || self.files.path == DELETED {
            let mut both: Vec<(aero::DirEntry, String)> =
                core::mem::take(&mut self.files.entries).into_iter().zip(core::mem::take(&mut self.files.found)).collect();
            both.sort_by(|(a, pa), (b, pb)| {
                let order = match by {
                    SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortBy::Type => pa.to_lowercase().cmp(&pb.to_lowercase()),
                    SortBy::Size => a.size.cmp(&b.size).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                    SortBy::Date => a.modified.cmp(&b.modified).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                };
                b.is_dir.cmp(&a.is_dir).then(if desc { order.reverse() } else { order })
            });
            (self.files.entries, self.files.found) = both.into_iter().unzip();
            return;
        }
        self.files.entries.sort_by(|a, b| {
            let order = match by {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Type => type_text(a).cmp(&type_text(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                SortBy::Size => a.size.cmp(&b.size).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                SortBy::Date => a.modified.cmp(&b.modified).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            };
            b.is_dir.cmp(&a.is_dir).then(if desc { order.reverse() } else { order })
        });
    }

    /// Brings entry `i` into view.
    fn scroll_to(&mut self, i: usize) {
        let c = self.files_client();
        let visible = self.files_visible(&c);
        let across = self.files_across(&c);
        if i < self.files.scroll {
            self.files.scroll = i - i % across;
        } else if i >= self.files.scroll + visible {
            let first = i + 1 - visible;
            self.files.scroll = first.div_ceil(across) * across;
        }
    }

    /// Ctrl+click: adds entry `i` to the selection, or takes it out.
    fn toggle(&mut self, i: usize) {
        if self.files.path.is_empty() {
            return self.select(i);
        }
        let mut marked = self.picked().into_iter().map(|j| self.entry_key(j)).collect::<Vec<_>>();
        let key = self.entry_key(i);
        match marked.iter().position(|k| *k == key) {
            Some(at) => {
                marked.remove(at);
            }
            None => marked.push(key),
        }
        self.files.marked = marked;
        self.files.selected = Some(i);
        self.files.anchor = Some(i);
        self.scroll_to(i);
        self.pick_status();
    }

    /// Shift+click or Shift+arrow: selects everything from the anchor to `i`.
    fn extend(&mut self, i: usize) {
        if self.files.path.is_empty() {
            return self.select(i);
        }
        let from = self.files.anchor.unwrap_or(i).min(self.files.entries.len().saturating_sub(1));
        let (a, b) = (from.min(i), from.max(i));
        self.files.marked = (a..=b).map(|j| self.entry_key(j)).collect();
        self.files.selected = Some(i);
        self.files.anchor = Some(from);
        self.scroll_to(i);
        self.pick_status();
    }

    /// Ctrl+A.
    fn select_all(&mut self) {
        let n = self.files.entries.len();
        if self.files.path.is_empty() || n == 0 {
            return;
        }
        self.files.marked = (0..n).map(|j| self.entry_key(j)).collect();
        self.files.selected = Some(self.files.selected.unwrap_or(0).min(n - 1));
        self.pick_status();
    }

    /// The status line for what is selected: one thing says what it is,
    /// several say how many and how big.
    fn pick_status(&mut self) {
        let picked = self.picked();
        match picked.len() {
            0 => self.files.status = format!("{} items", self.files.entries.len()),
            1 if self.files.marked.len() <= 1 => {
                let at = picked[0];
                self.files.marked.clear();
                self.files.selected = Some(at);
                self.describe(at);
            }
            n => {
                let names: Vec<String> = picked.iter().map(|&j| self.files.entries[j].name.clone()).collect();
                println!("[desktop] selected {} items: {}", n, names.join(" | "));
                let bytes: u64 = picked.iter().map(|&j| self.files.entries[j].size).sum();
                let files = picked.iter().filter(|&&j| !self.files.entries[j].is_dir).count();
                self.files.status = if files > 0 {
                    format!("{} items selected  {} in {} file{}", n, size_text(bytes), files, if files == 1 { "" } else { "s" })
                } else {
                    format!("{} items selected", n)
                };
            }
        }
    }

    /// Selects entry (or drive) `i`, scrolls it into view and says what it is.
    fn select(&mut self, i: usize) {
        self.files.marked.clear();
        self.files.anchor = Some(i);
        self.files.selected = Some(i);
        if !self.files.path.is_empty() {
            self.scroll_to(i);
        }
        self.describe(i);
    }

    /// Says on the status line what entry (or drive) `i` is.
    fn describe(&mut self, i: usize) {
        if self.files.path.is_empty() && i >= self.files.drives.len() {
            let sh = &shares()[i - self.files.drives.len()];
            self.files.status = format!("{}  {}", sh.unc(), if sh.client.is_some() { "connected" } else { "not connected: open it to sign in" });
            return;
        }
        if self.files.path.is_empty() {
            let v = &self.files.drives[i];
            self.files.status = format!("{}  {} {}{}", self.drive_name(i), space_text(v.size), v.kind, if v.read_only { ", read-only" } else { "" });
            return;
        }
        if self.files.path == BIN {
            let b = &self.files.bin[i];
            self.files.status = format!("{}  deleted {}", b.original, b.when);
            return;
        }
        let e = &self.files.entries[i];
        self.files.status = if e.is_dir { format!("{}  File folder", e.name) } else { format!("{}  {}  {}", e.name, type_text(e), size_text(e.size)) };
    }

    /// Goes to network drive `n`, asking for the password first if this
    /// session has not signed in to it yet.
    fn open_share(&mut self, n: usize) -> Rect {
        let sh = &shares()[n];
        if sh.client.is_none() && sh.password.is_none() {
            return self.open_form(Some(n));
        }
        let root = sh.root();
        self.navigate(root)
    }

    /// Opens the selected entry: a drive or folder is shown, a text file goes to Notes.
    fn open_selected(&mut self) -> Rect {
        let Some(i) = self.files.selected else { return Rect::EMPTY };
        if self.files.path.is_empty() {
            if i >= self.files.drives.len() {
                return self.open_share(i - self.files.drives.len());
            }
            let to = self.files.drives[i].path.clone();
            return self.navigate(to);
        }
        if self.files.path == BIN || self.files.path == DELETED {
            return self.command(Command::Restore);
        }
        let Some(e) = self.files.entries.get(i).cloned() else { return Rect::EMPTY };
        let path = self.entry_path(i);
        if e.is_dir {
            self.navigate(path)
        } else {
            match file_kind(&e.name) {
                FileKind::Picture => self.files_area().union(&self.open_picture(path)),
                FileKind::Sound => self.files_area().union(&self.open_song(path)),
                _ => self.open_file(path),
            }
        }
    }

    fn command(&mut self, cmd: Command) -> Rect {
        let r = self.run_command(cmd);
        // In two panes, the other side may show what just changed.
        if self.other.is_some() && !matches!(cmd, Command::Copy | Command::Cut | Command::Properties | Command::No) {
            self.with_other(|d| {
                if d.files.in_folder() || d.files.path == BIN {
                    let keep = d.selected_entry().map(|e| e.name);
                    d.refresh(keep);
                }
            });
        }
        r
    }

    fn run_command(&mut self, cmd: Command) -> Rect {
        let area = self.files_area();
        if !self.command_enabled(cmd) {
            return Rect::EMPTY;
        }
        let dir = self.files.path.clone();
        match cmd {
            Command::CopyOver | Command::MoveOver => {
                let to = self.other.as_ref().map(|o| o.path.clone()).unwrap_or_default();
                let items = self.picked_items();
                let cut = cmd == Command::MoveOver;
                let n = items.len();
                println!("[desktop] {} {} item{} to the other side, {}", if cut { "moving" } else { "copying" }, n, if n == 1 { "" } else { "s" }, to);
                let (status, _) = self.transfer(&items, &to, cut);
                if cut {
                    if let Some(clip) = self.clip.as_mut() {
                        clip.items.retain(|it| !items.iter().any(|m| m.path == it.path));
                    }
                    if self.clip.as_ref().is_some_and(|c| c.items.is_empty()) {
                        self.clip = None;
                    }
                    self.refresh(None);
                }
                self.files.status = status;
            }
            Command::NewFolder => {
                let taken: Vec<&str> = self.files.entries.iter().map(|e| e.name.as_str()).collect();
                let name = free_name("New folder", &taken, true, false);
                let path = join(&dir, &name);
                match fs_mkdir(&path) {
                    Ok(()) => {
                        println!("[desktop] new folder {}", path);
                        self.refresh(Some(name.clone()));
                        // Type its name straight away.
                        self.files.rename = Some(name);
                        self.files.rename_all = true;
                        self.files.status = String::from("Type a name, then press Enter");
                    }
                    Err(e) => self.files.status = format!("Can't make a folder here: {}", fs_error_text(e)),
                }
            }
            Command::Copy | Command::Cut => {
                let items = self.picked_items();
                let cut = cmd == Command::Cut;
                for it in &items {
                    println!("[desktop] {} {}", if cut { "cut" } else { "copied" }, it.path);
                }
                let what = match items.as_slice() {
                    [one] => format!("\"{}\"", split_path(&one.path).1),
                    _ => format!("{} items", items.len()),
                };
                self.files.status = format!("{} {}: open a folder and Paste", if cut { "Cut" } else { "Copied" }, what);
                self.clip = Some(Clip { items, cut });
            }
            Command::Paste => {
                let Some(clip) = self.clip.clone() else { return area };
                let (status, moved) = self.transfer(&clip.items, &dir, clip.cut);
                if clip.cut && moved > 0 {
                    self.clip = None;
                }
                self.files.status = status;
            }
            Command::Rename => {
                let e = self.selected_entry().unwrap();
                self.files.rename = Some(e.name);
                self.files.rename_all = true;
                self.files.status = String::from("Type the new name, then press Enter (Esc to cancel)");
            }
            Command::Delete | Command::Empty => {
                self.files.confirm = Some(cmd);
                self.files.status = String::from("Press Enter or Yes to go ahead, Esc or No to keep it");
            }
            Command::Restore if self.files.path == DELETED => {
                let picked = self.picked();
                let i = picked[0];
                let slots: Vec<u32> = picked.iter().filter_map(|&j| self.files.lost_at(j).map(|d| d.slot)).collect();
                let at = self.files.lost_in.clone();
                let mut done = Vec::new();
                let mut failed = None;
                for slot in slots {
                    let was = self.files.lost.iter().position(|d| d.slot == slot).and_then(|k| self.files.lost_bin.get(k)).cloned();
                    match aero::undelete(&at, slot) {
                        Ok(name) => {
                            println!("[desktop] undeleted {}", join(&at, &name));
                            match was {
                                // Back from the bin's folder to where it was.
                                Some(g) => {
                                    let root = split_path(&at).0;
                                    let item = Recycled { id: name.clone(), ..g.clone() };
                                    let gone: Vec<Recycled> = Self::read_list(&root, BIN_GONE).into_iter().filter(|b| b.id != g.id).collect();
                                    let _ = Self::write_list(&root, BIN_GONE, &gone);
                                    match self.restore(&item) {
                                        Ok(to) => done.push(split_path(&to).1),
                                        Err(msg) => {
                                            println!("[desktop] {}", msg);
                                            failed = Some(msg);
                                        }
                                    }
                                }
                                None => done.push(name),
                            }
                        }
                        Err(e) => {
                            let msg = match e {
                                aero::E_EXISTS => String::from("Can't restore it: something with that name is there now"),
                                aero::E_FULL => String::from("Can't restore it: its data has been written over"),
                                e => format!("Can't restore it: {}", fs_error_text(e)),
                            };
                            println!("[desktop] undelete failed in {}: {}", at, msg);
                            failed = Some(msg);
                        }
                    }
                }
                self.refresh(None);
                if !self.files.entries.is_empty() {
                    self.select(i.min(self.files.entries.len() - 1));
                }
                self.files.status = match (failed, done.as_slice()) {
                    (Some(msg), _) => msg,
                    (None, [one]) => format!("Restored {}", one),
                    (None, _) => format!("Restored {} items", done.len()),
                };
            }
            Command::Restore => {
                let picked = self.picked();
                let i = picked[0];
                let items: Vec<Recycled> = picked.iter().map(|&j| self.files.bin[j].clone()).collect();
                let mut done = Vec::new();
                let mut failed = None;
                for item in &items {
                    match self.restore(item) {
                        Ok(to) => done.push(to),
                        Err(msg) => failed = Some(msg),
                    }
                }
                self.refresh(None);
                if !self.files.entries.is_empty() {
                    self.select(i.min(self.files.entries.len() - 1));
                }
                self.files.status = match (failed, done.as_slice()) {
                    (Some(msg), _) => msg,
                    (None, [one]) => format!("Restored to {}", one),
                    (None, _) => format!("Restored {} items", done.len()),
                };
            }
            Command::Yes => {
                let what = self.files.confirm.take();
                let at = self.files.selected.unwrap_or(0);
                let result = match what {
                    Some(Command::Empty) => {
                        let items = core::mem::take(&mut self.files.bin);
                        let n = items.len();
                        let mut failed = None;
                        for item in &items {
                            if let Err(msg) = self.purge(item, true) {
                                failed = Some(msg);
                            }
                        }
                        println!("[desktop] emptied the Recycle Bin ({} item{})", n, if n == 1 { "" } else { "s" });
                        failed.map_or(Ok(String::from("The Recycle Bin is empty")), Err)
                    }
                    _ if self.files.path == BIN => {
                        let items: Vec<Recycled> = self.picked().iter().map(|&j| self.files.bin[j].clone()).collect();
                        let mut result = Ok(match items.as_slice() {
                            [one] => format!("Deleted \"{}\" for good", split_path(&one.original).1),
                            _ => format!("Deleted {} items for good", items.len()),
                        });
                        for item in &items {
                            if let Err(msg) = self.purge(item, true) {
                                result = Err(msg);
                            }
                        }
                        result
                    }
                    _ => {
                        let items = self.picked_items();
                        self.delete_items(&items)
                    }
                };
                self.refresh(None);
                if !self.files.entries.is_empty() {
                    self.select(at.min(self.files.entries.len() - 1));
                }
                match result {
                    Ok(msg) => self.files.status = msg,
                    Err(msg) => {
                        println!("[desktop] {}", msg);
                        self.files.status = msg;
                    }
                }
            }
            Command::No => {
                self.files.confirm = None;
                self.files.status = String::from("Nothing was deleted");
            }
            Command::Map => return self.open_form(None),
            Command::Properties => self.open_props(),
            Command::Disconnect => {
                let n = self.files.selected.unwrap() - self.files.drives.len();
                let sh = shares().remove(n);
                println!("[desktop] disconnected {} ({}:)", sh.unc(), sh.letter);
                self.save_settings();
                self.refresh(None);
                self.files.status = format!("Disconnected {}", sh.unc());
            }
        }
        area
    }

    /// Copies (or moves, when `cut`) `items` into folder `dir`, under new
    /// names where theirs are taken there. Shows `dir` afterwards if it is
    /// the folder open, with what arrived selected. Returns what to say and
    /// how many went.
    fn transfer(&mut self, items: &[Item], dir: &str, cut: bool) -> (String, usize) {
        let mut taken: Vec<String> = fs_list(dir).unwrap_or_default().into_iter().map(|e| e.name).collect();
        let mut arrived = Vec::new();
        let mut files = 0;
        let mut problem = None;
        let total = items.iter().map(|it| if it.is_dir { tree_size(&it.path).0 } else { it.size }).sum();
        let now = aero::clock_us();
        let to_name = if dir.is_empty() || dir == "/" { String::from(dir) } else { split_path(dir).1 };
        self.progress = Some(Progress { moving: cut, count: items.len(), to: to_name, file: String::new(), done: 0, total, started_us: now, drawn_us: 0, shown: false, cancel: false });
        for it in items {
            if self.progress.as_ref().is_some_and(|p| p.cancel) {
                break;
            }
            let (from_dir, name) = split_path(&it.path);
            let lower = it.path.to_lowercase();
            if it.is_dir && (dir.eq_ignore_ascii_case(&it.path) || dir.to_lowercase().starts_with(&format!("{}/", lower))) {
                problem = Some(String::from("A folder can't go inside itself"));
                continue;
            }
            if cut && from_dir.eq_ignore_ascii_case(dir) {
                problem = Some(if items.len() == 1 { String::from("It is already here") } else { String::from("Some are already here") });
                continue;
            }
            let names: Vec<&str> = taken.iter().map(|t| t.as_str()).collect();
            let new_name = free_name(&name, &names, it.is_dir, true);
            let to = join(dir, &new_name);
            let tick: Tick = &mut |file, bytes| self.copy_tick(file, bytes);
            let result = if cut {
                move_tree(&it.path, &to, it.is_dir, tick).map(|()| {
                    println!("[desktop] moved {} to {}", it.path, to);
                    0
                })
            } else {
                copy_tree(&it.path, &to, it.is_dir, tick).map(|n| {
                    println!("[desktop] pasted {} as {} ({} file{})", it.path, to, n, if n == 1 { "" } else { "s" });
                    n
                })
            };
            match result {
                Ok(n) => {
                    files += n;
                    taken.push(new_name.clone());
                    arrived.push(new_name);
                }
                Err(msg) if msg == CANCELLED => {
                    println!("[desktop] {} cancelled at {}", if cut { "move" } else { "paste" }, it.path);
                    problem = Some(String::from(if cut { "Moving was cancelled" } else { "Copying was cancelled" }));
                    // What was copied of a folder stays where it got to.
                    if it.is_dir && fs_list(&to).is_ok() {
                        taken.push(new_name.clone());
                        arrived.push(new_name);
                    }
                }
                Err(msg) => {
                    println!("[desktop] {} failed: {}", if cut { "move" } else { "paste" }, msg);
                    problem = Some(msg);
                }
            }
        }
        self.end_progress();
        if self.files.path.eq_ignore_ascii_case(dir) {
            self.refresh(None);
            self.files.marked.clear();
            let at: Vec<usize> = arrived.iter().filter_map(|n| self.files.entries.iter().position(|e| e.name == *n)).collect();
            if let Some(&first) = at.first() {
                self.select(first);
                if at.len() > 1 {
                    self.files.marked = at.iter().map(|&j| self.entry_key(j)).collect();
                }
            }
        } else if cut {
            // Moved out of the folder shown.
            self.refresh(None);
        }
        let n = arrived.len();
        let status = match (problem, arrived.as_slice()) {
            (Some(msg), []) => msg,
            (Some(msg), _) => format!("{} of {} done: {}", n, items.len(), msg),
            (None, [one]) if cut => format!("Moved \"{}\" to {}", one, dir),
            (None, [one]) => format!("Pasted \"{}\" ({} file{})", one, files, if files == 1 { "" } else { "s" }),
            (None, _) if cut => format!("Moved {} items to {}", n, dir),
            (None, _) => format!("Pasted {} items ({} files)", n, files),
        };
        (status, n)
    }

    /// Deletes `items`: into their drive's Recycle Bin where there is one
    /// (Ctrl+Z puts them all back), for good otherwise (network drives).
    fn delete_items(&mut self, items: &[Item]) -> Result<String, String> {
        self.files.last_recycled.clear();
        let mut binned = 0;
        let mut gone = 0;
        for it in items {
            let e = aero::DirEntry { name: split_path(&it.path).1, is_dir: it.is_dir, size: it.size, modified: 0 };
            if self.recycle_root_of(&it.path).is_some() {
                self.recycle(&it.path, &e)?;
                binned += 1;
            } else {
                let n = delete_tree(&it.path, it.is_dir)?;
                println!("[desktop] deleted {} ({} item{})", it.path, n, if n == 1 { "" } else { "s" });
                gone += 1;
            }
        }
        Ok(match (items, binned) {
            ([one], 1) => format!("Moved \"{}\" to the Recycle Bin (Ctrl+Z puts it back)", split_path(&one.path).1),
            ([one], _) => format!("Deleted \"{}\"", split_path(&one.path).1),
            (_, 0) => format!("Deleted {} items", gone),
            _ => format!("Moved {} items to the Recycle Bin (Ctrl+Z puts them back)", binned),
        })
    }

    /// Gives the selected entry the name typed for it.
    fn finish_rename(&mut self) {
        let Some(new) = self.files.rename.take() else { return };
        let Some(e) = self.selected_entry() else { return };
        let new = String::from(new.trim().trim_end_matches('.'));
        if new.is_empty() || new == e.name {
            self.files.status = String::new();
            return;
        }
        // Its own name in other letter case is fine: only the case changes.
        if self.files.entries.iter().any(|o| o.name != e.name && o.name.eq_ignore_ascii_case(&new)) {
            self.files.status = format!("There is already something called \"{}\" here", new);
            return;
        }
        let dir = self.files.path.clone();
        let (from, to) = (join(&dir, &e.name), join(&dir, &new));
        let result = move_tree(&from, &to, e.is_dir, &mut |_, _| true);
        match result {
            Ok(_) => {
                println!("[desktop] renamed {} to {}", from, to);
                self.refresh(Some(new.clone()));
                self.files.status = format!("Renamed to \"{}\"", new);
            }
            Err(msg) => {
                println!("[desktop] rename failed: {}", msg);
                self.refresh(Some(e.name));
                self.files.status = msg;
            }
        }
    }

    // --------------------------------------------------------- Recycle Bin
    //
    // Deleting on a writable drive moves the thing into that drive's
    // "Recycle Bin" folder under a numbered name, and adds a line to the
    // folder's info.txt: number, D or F, size, when, and where it was.

    /// The drive root of the folder shown, if things deleted there can go to its Recycle Bin.
    fn recycle_root(&self) -> Option<String> {
        self.recycle_root_of(&self.files.path)
    }
    /// The root of the drive `path` is on, if it has a Recycle Bin.
    fn recycle_root_of(&self, path: &str) -> Option<String> {
        let d = self.drive_of(path)?;
        let v = &self.files.drives[d];
        // Not on network drives, and not on Windows drives until changes are allowed.
        if v.read_only { None } else { Some(v.path.clone()) }
    }

    fn read_bin(root: &str) -> Vec<Recycled> {
        Self::read_list(root, BIN_INFO)
    }

    fn read_list(root: &str, file: &str) -> Vec<Recycled> {
        let mut buf = alloc::vec![0u8; 64 * 1024];
        let Ok(n) = aero::read_file(&join(&join(root, BIN_DIR), file), &mut buf) else { return Vec::new() };
        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
        text.lines()
            .filter_map(|line| {
                let mut f = line.splitn(5, '\t');
                let (id, kind, size, when, original) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
                Some(Recycled {
                    root: String::from(root),
                    id: String::from(id),
                    original: String::from(original),
                    is_dir: kind == "D",
                    size: size.parse().unwrap_or(0),
                    when: String::from(when),
                })
            })
            .collect()
    }

    fn write_bin(root: &str, items: &[Recycled]) -> Result<(), String> {
        Self::write_list(root, BIN_INFO, items)
    }

    fn write_list(root: &str, file: &str, items: &[Recycled]) -> Result<(), String> {
        let mut text = String::new();
        for b in items {
            text.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", b.id, if b.is_dir { "D" } else { "F" }, b.size, b.when, b.original));
        }
        let path = join(&join(root, BIN_DIR), file);
        aero::write_file(&path, text.as_bytes()).map(|_| ()).map_err(|e| format!("Can't write {}: {}", path, fs_error_text(e)))
    }

    /// Everything in the Recycle Bins of the writable drives, newest first.
    fn load_bin(&mut self) {
        let mut all = Vec::new();
        for v in self.files.drives.iter().filter(|v| !v.read_only) {
            all.extend(Self::read_bin(&v.path));
        }
        all.reverse();
        self.files.entries = all
            .iter()
            .map(|b| aero::DirEntry { name: split_path(&b.original).1, is_dir: b.is_dir, size: b.size, modified: 0 })
            .collect();
        self.files.bin = all;
    }

    /// Moves `path` into its drive's Recycle Bin.
    fn recycle(&mut self, path: &str, e: &aero::DirEntry) -> Result<Recycled, String> {
        let root = self.recycle_root_of(path).ok_or_else(|| String::from("This drive has no Recycle Bin"))?;
        let bin = join(&root, BIN_DIR);
        match aero::create_dir(&bin) {
            Ok(()) | Err(aero::E_EXISTS) => {}
            Err(e) => return Err(format!("Can't make the Recycle Bin: {}", fs_error_text(e))),
        }
        let mut items = Self::read_bin(&root);
        // The next free number, past anything already in the folder.
        let mut next = 1;
        let listed = aero::list_dir(&bin).unwrap_or_default();
        let gone = Self::read_list(&root, BIN_GONE);
        for name in items.iter().chain(gone.iter()).map(|b| b.id.as_str()).chain(listed.iter().map(|l| l.name.as_str())) {
            if let Some(n) = name.strip_prefix('R').and_then(|n| n.parse::<u32>().ok()) {
                next = next.max(n + 1);
            }
        }
        let when = match aero::now() {
            Some(t) => format!("{}/{}/{} {}:{:02} {}", t.month, t.day, t.year, if t.hour % 12 == 0 { 12 } else { t.hour % 12 }, t.minute, if t.hour < 12 { "AM" } else { "PM" }),
            None => String::from("-"),
        };
        let item = Recycled { root: root.clone(), id: format!("R{:04}", next), original: String::from(path), is_dir: e.is_dir, size: e.size, when };
        // Listed first, then moved in (a rename on its own drive): stopped in
        // between, the list has a line for something not there, which is
        // dropped from it, rather than the bin holding something unlisted.
        items.push(item.clone());
        Self::write_bin(&root, &items).map_err(|m| format!("Not deleted: {}", m))?;
        if let Err(m) = move_tree(path, &item.stored(), e.is_dir, &mut |_, _| true) {
            items.pop();
            let _ = Self::write_bin(&root, &items);
            return Err(format!("Not deleted: {}", m));
        }
        println!("[desktop] moved {} to the Recycle Bin as {}", path, item.stored());
        self.files.last_recycled.push(item.clone());
        Ok(item)
    }

    /// Puts something from the Recycle Bin back where it was (under a new
    /// name if that one is taken now). Returns where it went.
    fn restore(&mut self, item: &Recycled) -> Result<String, String> {
        let (dir, name) = split_path(&item.original);
        // Its folder may have gone since: make it again.
        let mut at = String::from("/");
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            at = join(&at, part);
            match aero::create_dir(&at) {
                Ok(()) | Err(aero::E_EXISTS) => {}
                Err(_) if aero::list_dir(&at).is_ok() => {}
                Err(e) => return Err(format!("Can't make {}: {}", at, fs_error_text(e))),
            }
        }
        let taken: Vec<String> = aero::list_dir(&dir).unwrap_or_default().into_iter().map(|e| e.name).collect();
        let taken: Vec<&str> = taken.iter().map(|t| t.as_str()).collect();
        let to = join(&dir, &free_name(&name, &taken, item.is_dir, false));
        move_tree(&item.stored(), &to, item.is_dir, &mut |_, _| true).map_err(|m| format!("Not restored: {}", m))?;
        let items: Vec<Recycled> = Self::read_bin(&item.root).into_iter().filter(|b| b.id != item.id).collect();
        Self::write_bin(&item.root, &items)?;
        println!("[desktop] restored {} from the Recycle Bin to {}", item.original, to);
        self.files.last_recycled.retain(|l| !(l.root == item.root && l.id == item.id));
        Ok(to)
    }

    /// Deletes something in the Recycle Bin for good.
    /// `emptied`: remember it, so it can still be undeleted (FAT32).
    fn purge(&self, item: &Recycled, emptied: bool) -> Result<(), String> {
        // The list first: written after, a new list file could take the
        // directory slot of what was just deleted, and lose its name.
        // Only FAT32 drives can undelete.
        if emptied && self.drive_of(&item.root).is_some_and(|d| self.files.drives[d].kind == "FAT32") {
            let mut gone = Self::read_list(&item.root, BIN_GONE);
            gone.push(item.clone());
            let extra = gone.len().saturating_sub(BIN_GONE_MAX);
            if let Err(m) = Self::write_list(&item.root, BIN_GONE, &gone[extra..]) {
                println!("[desktop] {}", m);
            }
        }
        match delete_tree(&item.stored(), item.is_dir) {
            Ok(_) => {}
            // Gone already: just drop it from the list.
            Err(_) if aero::list_dir(&join(&item.root, BIN_DIR)).is_ok_and(|l| !l.iter().any(|e| e.name == item.id)) => {}
            Err(m) => return Err(m),
        }
        let items: Vec<Recycled> = Self::read_bin(&item.root).into_iter().filter(|b| b.id != item.id).collect();
        Self::write_bin(&item.root, &items)?;
        println!("[desktop] removed {} from the Recycle Bin", item.original);
        Ok(())
    }

    /// Ctrl+Z: puts back the last thing deleted.
    fn undo_delete(&mut self) -> Rect {
        let items = self.files.last_recycled.clone();
        if items.is_empty() {
            println!("[desktop] nothing to undo");
            self.files.status = String::from("Nothing to undo");
            return self.files_area();
        }
        let mut back = Vec::new();
        let mut failed = None;
        for item in items.iter().rev() {
            match self.restore(item) {
                Ok(to) => back.push(to),
                Err(msg) => {
                    println!("[desktop] {}", msg);
                    failed = Some(msg);
                }
            }
        }
        if let Some(to) = back.first() {
            let (dir, name) = split_path(to);
            if self.files.path.eq_ignore_ascii_case(&dir) || self.files.path == BIN {
                self.refresh(Some(name));
            }
        }
        self.files.status = match (failed, back.as_slice()) {
            (Some(msg), _) => msg,
            (None, [one]) => format!("Undid the delete: {} is back", one),
            (None, _) => format!("Undid the delete: {} items are back", back.len()),
        };
        self.files_area()
    }

    /// Looks for `query` in the names of everything under the folder shown
    /// (every drive, from Computer) and shows what it found. A query with
    /// * or ? matches whole names (*.txt); otherwise any name containing
    /// it matches. Case does not matter.
    fn search(&mut self) -> Rect {
        let query = String::from(self.files.query.trim());
        self.files.query_focus = false;
        if query.is_empty() {
            return self.files_area();
        }
        let base = match self.files.path.as_str() {
            "" | BIN => String::new(),
            SEARCH => self.files.searched_in.clone(),
            p => String::from(p),
        };
        let mut stack: Vec<String> = if base.is_empty() {
            let mut roots: Vec<String> = self.files.drives.iter().map(|v| v.path.clone()).collect();
            roots.extend(shares().iter().filter(|s| s.client.is_some()).map(|s| s.root()));
            roots
        } else {
            alloc::vec![base.clone()]
        };
        // Deeper drives (/sata0p1) are walked on their own, not again under /.
        let roots: Vec<String> = self.files.drives.iter().map(|v| v.path.to_lowercase()).collect();
        let pattern = query.to_lowercase();
        let mut found = Vec::new();
        let mut looked = 0;
        while let Some(dir) = stack.pop() {
            let Ok(list) = fs_list(&dir) else { continue };
            for e in list {
                looked += 1;
                let path = join(&dir, &e.name);
                if e.is_dir && (e.name.eq_ignore_ascii_case(BIN_DIR) && self.files.drives.iter().any(|v| v.path.eq_ignore_ascii_case(&dir))
                    || roots.contains(&path.to_lowercase()))
                {
                    continue;
                }
                if name_matches(&e.name.to_lowercase(), &pattern) && found.len() < SEARCH_FINDS {
                    found.push((path.clone(), e.clone()));
                }
                if e.is_dir {
                    stack.push(path);
                }
            }
            if looked >= SEARCH_LOOKS {
                break;
            }
        }
        let shown: Vec<&str> = found.iter().take(8).map(|(p, _)| p.as_str()).collect();
        println!("[desktop] search for \"{}\" in {}: {} found ({})", query, if base.is_empty() { "Computer" } else { base.as_str() },
            found.len(), shown.join(" | "));
        self.files.searched_in = base;
        self.files.results = found;
        let r = self.navigate(String::from(SEARCH));
        if looked >= SEARCH_LOOKS {
            self.files.status = format!("{}; stopped after looking at {} names", self.files.status, SEARCH_LOOKS);
        }
        r
    }

    /// A key typed while the search box has the focus.
    fn search_key(&mut self, k: u8) -> Rect {
        let area = self.files_area();
        match k {
            b'\n' => return self.search(),
            27 if !self.files.query.is_empty() => self.files.query.clear(),
            27 | 9 => self.files.query_focus = false,
            8 => {
                self.files.query.pop();
            }
            32..=126 if self.files.query.len() < 80 => self.files.query.push(k as char),
            _ => return Rect::EMPTY,
        }
        area
    }

    /// Fills in the Properties box for what is selected.
    fn open_props(&mut self) {
        let f = &self.files;
        let props = if f.path.is_empty() {
            let Some(i) = f.selected else { return };
            if i >= f.drives.len() {
                let sh = &shares()[i - f.drives.len()];
                Props {
                    title: sh.name(),
                    folder: false,
                    rows: alloc::vec![
                        ("Type", String::from("Network drive (SMB)")),
                        ("Folder", sh.unc()),
                        ("Signed in as", sh.user.clone()),
                        ("Status", String::from(if sh.client.is_some() { "Connected" } else { "Not connected" })),
                    ],
                    pie: None,
                }
            } else {
                let v = &f.drives[i];
                let kind = if v.device.starts_with("usb") { "USB Drive" } else { "Local Disk" };
                let mut rows = alloc::vec![("Type", String::from(kind)), ("File system", v.kind.clone()), ("Device", v.device.clone())];
                let pie = v.free.map(|free| {
                    let used = v.size.saturating_sub(free);
                    rows.push(("Used space", format!("{}  ({} bytes)", space_text(used), group(used))));
                    rows.push(("Free space", format!("{}  ({} bytes)", space_text(free), group(free))));
                    (used, v.size)
                });
                rows.push(("Capacity", format!("{}  ({} bytes)", space_text(v.size), group(v.size))));
                if v.read_only {
                    rows.push(("Attributes", String::from("Read-only")));
                }
                println!("[desktop] properties of {}: {} {}, {} bytes, {} free", self.drive_name(i), kind, v.kind, v.size,
                    v.free.map_or(String::from("not known"), |b| format!("{} bytes", b)));
                Props { title: self.drive_name(i), folder: false, rows, pie }
            }
        } else if f.path == BIN {
            let picked = self.picked();
            let items: Vec<&Recycled> = picked.iter().map(|&j| &f.bin[j]).collect();
            let bytes: u64 = items.iter().map(|b| b.size).sum();
            match items.as_slice() {
                [one] => Props {
                    title: split_path(&one.original).1,
                    folder: one.is_dir,
                    rows: alloc::vec![
                        ("Original location", split_path(&one.original).0),
                        ("Deleted", one.when.clone()),
                        ("Size", format!("{}  ({} bytes)", size_text(one.size), group(one.size))),
                    ],
                    pie: None,
                },
                many => Props {
                    title: format!("{} items", many.len()),
                    folder: false,
                    rows: alloc::vec![("Size", format!("{}  ({} bytes)", size_text(bytes), group(bytes)))],
                    pie: None,
                },
            }
        } else {
            let items = self.picked_items();
            if items.is_empty() {
                return;
            }
            let (mut bytes, mut files, mut dirs) = (0u64, 0usize, 0usize);
            for it in &items {
                if it.is_dir {
                    let (b, nf, nd) = tree_size(&it.path);
                    bytes += b;
                    files += nf;
                    dirs += nd;
                } else {
                    bytes += it.size;
                }
            }
            let size = format!("{}  ({} bytes)", size_text(bytes), group(bytes));
            let contains = format!("{} File{}, {} Folder{}", files, if files == 1 { "" } else { "s" }, dirs, if dirs == 1 { "" } else { "s" });
            let location = {
                let first = split_path(&items[0].path).0;
                if items.iter().all(|it| split_path(&it.path).0 == first) { first } else { String::from("Various folders") }
            };
            let props = match items.as_slice() {
                [one] if one.is_dir => {
                    println!("[desktop] properties of {}: {} bytes in {} file(s) and {} folder(s)", one.path, bytes, files, dirs);
                    Props {
                        title: split_path(&one.path).1,
                        folder: true,
                        rows: alloc::vec![("Type", String::from("File folder")), ("Location", location), ("Size", size), ("Contains", contains)],
                        pie: None,
                    }
                }
                [one] => {
                    let e = aero::DirEntry { name: split_path(&one.path).1, is_dir: false, size: one.size, modified: 0 };
                    println!("[desktop] properties of {}: {} bytes", one.path, bytes);
                    Props {
                        title: e.name.clone(),
                        folder: false,
                        rows: alloc::vec![("Type", type_text(&e)), ("Location", location), ("Size", size)],
                        pie: None,
                    }
                }
                many => {
                    let top_files = many.iter().filter(|it| !it.is_dir).count();
                    println!("[desktop] properties of {} items: {} bytes in {} file(s) and {} folder(s)", many.len(), bytes, files + top_files,
                        dirs + many.len() - top_files);
                    Props {
                        title: format!("{} items", many.len()),
                        folder: false,
                        rows: alloc::vec![
                            ("Location", location),
                            ("Size", size),
                            ("Contains", format!("{} Files, {} Folders", files + top_files, dirs + many.len() - top_files)),
                        ],
                        pie: None,
                    }
                }
            };
            props
        };
        self.files.props = Some(props);
    }

    /// Where the Properties box is drawn: in the middle of the content.
    fn props_rect(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let Some(p) = &self.files.props else { return Rect::EMPTY };
        let k = self.files_content(client);
        let h = 60 * s + p.rows.len() as i32 * 24 * s + if p.pie.is_some() { 130 * s } else { 0 } + 44 * s;
        let w = (500 * s).min(k.w - 16 * s);
        let h = h.min(k.h - 8 * s);
        Rect::new(k.x + (k.w - w) / 2, k.y + (k.h - h) / 2, w, h)
    }
    fn props_ok(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let r = self.props_rect(client);
        Rect::new(r.x + r.w - 90 * s, r.y + r.h - 36 * s, 76 * s, 26 * s)
    }

    fn props_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let Some(p) = &self.files.props else { return };
        let r = self.props_rect(client);
        c.rounded(r, 4 * s, false, rgb(252, 253, 255), rgb(236, 242, 250), 255);
        c.rounded_outline(r, 4 * s, false, rgb(110, 140, 180), 255);
        if p.folder {
            c.folder(r.x + 16 * s, r.y + 18 * s, s);
        } else if p.pie.is_some() || p.rows.first().is_some_and(|(_, v)| v.starts_with("Network")) {
            c.small_drive(r.x + 14 * s, r.y + 18 * s, s, p.pie.is_none());
        } else {
            c.file_icon(r.x + 18 * s, r.y + 16 * s, s, file_kind(&p.title));
        }
        let fw = Font::Normal.w(s);
        let title: String = p.title.chars().take(((r.w - 60 * s) / fw).max(1) as usize).collect();
        c.text(r.x + 44 * s, r.y + 16 * s, &title, rgb(30, 57, 145), Font::Normal);
        c.fill(Rect::new(r.x + 12 * s, r.y + 46 * s, r.w - 24 * s, 1), rgb(200, 210, 225));
        let label_w = p.rows.iter().map(|(l, _)| l.len() as i32 + 2).max().unwrap_or(8) * fw;
        let value_x = r.x + 16 * s + label_w;
        let room = ((r.x + r.w - 12 * s - value_x) / fw).max(1) as usize;
        let mut y = r.y + 58 * s;
        for (label, value) in &p.rows {
            c.text(r.x + 16 * s, y, &format!("{}:", label), rgb(80, 85, 95), Font::Normal);
            // Too long: the byte count in brackets goes first.
            let value = if value.chars().count() > room { value.split("  (").next().unwrap_or(value) } else { value.as_str() };
            let v: String = value.chars().take(room).collect();
            c.text(value_x, y, &v, 0x101010, Font::Normal);
            y += 24 * s;
        }
        if let Some((used, total)) = p.pie {
            // Used in blue and free in magenta, as Windows draws it.
            let rad = 50 * s;
            let (cx, cy) = (r.x + 90 * s, y + 8 * s + rad);
            let frac = if total == 0 { 0.0 } else { used as f32 / total as f32 };
            let (blue, pink) = (rgb(38, 110, 210), rgb(200, 60, 170));
            for dy in -rad..=rad {
                for dx in -rad..=rad {
                    if dx * dx + dy * dy > rad * rad {
                        continue;
                    }
                    let turn = clockwise_turn(dx as f32, dy as f32);
                    c.fill(Rect::new(cx + dx, cy + dy, 1, 1), if turn < frac { blue } else { pink });
                }
            }
            c.rounded_outline(Rect::new(cx - rad, cy - rad, 2 * rad + 1, 2 * rad + 1), rad, false, rgb(90, 100, 120), 255);
            let lx = cx + rad + 30 * s;
            for (i, (color, text)) in [(blue, format!("Used  {}", space_text(used))), (pink, format!("Free  {}", space_text(total.saturating_sub(used))))]
                .iter()
                .enumerate()
            {
                let ly = cy - 20 * s + i as i32 * 28 * s;
                c.fill(Rect::new(lx, ly + 2 * s, 12 * s, 12 * s), *color);
                c.text(lx + 20 * s, ly, text, 0x101010, Font::Normal);
            }
        }
        let ok = self.props_ok(client);
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let hover = ok.contains(px, py);
        c.rounded(ok, 3 * s, false, rgb(250, 252, 255), if hover { rgb(190, 220, 250) } else { rgb(220, 230, 242) }, 255);
        c.rounded_outline(ok, 3 * s, false, rgb(110, 135, 170), 255);
        c.text(ok.x + (ok.w - Font::Normal.width(s, "OK")) / 2, ok.y + (ok.h - Font::Normal.h(s)) / 2, "OK", 0x101010, Font::Normal);
    }

    /// The button went down on selected entries: they may be dragged next.
    fn press_drag(&mut self, x: i32, y: i32, collapse: Option<usize>) {
        if self.files.path.is_empty() || self.files.path == BIN || self.files.path == DELETED {
            return;
        }
        let items = self.picked_items();
        if !items.is_empty() {
            self.file_drag = Some(FileDrag { x0: x, y0: y, items, active: false, collapse });
        }
    }

    /// Where `path` lives: a drive's number, or a network drive's.
    fn volume_of(&self, path: &str) -> Option<(bool, usize)> {
        match net_path(path) {
            Some((i, _)) => Some((true, i)),
            None => self.drive_of(path).map(|d| (false, d)),
        }
    }

    /// Where dropping the dragged files at (x, y) would put them: a folder
    /// shown, a drive or the Recycle Bin in the pane, or a part of the
    /// address. With what it would be called and what would happen:
    /// Ctrl copies, Shift moves, and otherwise they move on the same drive
    /// and are copied to another one, as in Windows.
    fn drop_target(&self, x: i32, y: i32) -> Option<(String, String, DropAction)> {
        let d = self.file_drag.as_ref()?;
        let win = self.windows.last()?;
        if win.kind != Kind::Computer || !win.shown() {
            return None;
        }
        let c = self.files_client();
        if !c.contains(x, y) {
            return None;
        }
        let mut target = None;
        let rows = self.files_rows(&c);
        if rows.contains(x, y) && !self.files.path.is_empty() && self.files.path != BIN {
            let i = self.files.scroll + self.cell_at(&rows, x, y);
            if let Some(e) = self.files.entries.get(i) {
                if e.is_dir && !self.is_marked(i) {
                    target = Some((self.entry_path(i), e.name.clone()));
                }
            }
        }
        // The other side of two panes: a folder on it, or the folder it shows.
        if let Some(other) = self.other.as_ref().filter(|o| o.in_folder()) {
            let k = self.other_content(&c);
            if k.contains(x, y) {
                let mut hit = (other.path.clone(), self.place_title(other));
                let rows = self.rows_in(&k);
                if rows.contains(x, y) {
                    let i = other.scroll + self.cell_at(&rows, x, y);
                    if let Some(e) = other.entries.get(i).filter(|e| e.is_dir) {
                        hit = (other.entry_path(i), e.name.clone());
                    }
                }
                target = Some(hit);
            }
        }
        if self.files_pane(&c).contains(x, y) {
            let places = self.places();
            if let Some(i) = (0..places.len()).find(|&i| self.pane_item(&c, i).contains(x, y)) {
                let row = places[i].clone();
                if row.kind != Place::Computer {
                    target = Some((row.path, row.name));
                }
            }
        }
        if let Some((_, text, path)) = self.crumb_rects(&c).into_iter().find(|(r, _, _)| r.contains(x, y)) {
            if !path.is_empty() && path != BIN {
                target = Some((path, text));
            }
        }
        let (dir, name) = target?;
        if dir == BIN {
            // Only what is on a drive with a Recycle Bin can go in it.
            if d.items.iter().any(|it| self.recycle_root_of(&it.path).is_none()) {
                return None;
            }
            return Some((dir, name, DropAction::Recycle));
        }
        if self.volume_of(&dir).is_none() {
            return None;
        }
        // Not into itself, and not to where they already are.
        if d.items.iter().any(|it| {
            dir.eq_ignore_ascii_case(&it.path) || dir.to_lowercase().starts_with(&format!("{}/", it.path.to_lowercase()))
        }) {
            return None;
        }
        let keys = self.pointer.keys;
        let same = d.items.iter().all(|it| self.volume_of(&it.path) == self.volume_of(&dir));
        let action = if keys & aero::display::MOD_CTRL != 0 {
            DropAction::Copy
        } else if keys & aero::display::MOD_SHIFT != 0 || same {
            DropAction::Move
        } else {
            DropAction::Copy
        };
        if action == DropAction::Move && d.items.iter().all(|it| split_path(&it.path).0.eq_ignore_ascii_case(&dir)) {
            return None;
        }
        if action != DropAction::Recycle && self.files_drive_read_only(&dir) {
            return None;
        }
        Some((dir, name, action))
    }

    /// Whether `path` is on a drive that can't be written (NTFS).
    fn files_drive_read_only(&self, path: &str) -> bool {
        if net_path(path).is_some() {
            return false;
        }
        self.drive_of(path).map(|d| self.files.drives[d].read_only).unwrap_or(true)
    }

    /// The pointer moved or a button changed while files are pressed on or
    /// dragged. Returns what to redraw.
    fn file_drag_step(&mut self, p: aero::display::Pointer) -> Rect {
        let Some(d) = self.file_drag.as_mut() else { return Rect::EMPTY };
        let (x, y) = (p.x as i32, p.y as i32);
        if p.buttons & 1 != 0 {
            if !d.active && ((x - d.x0).abs() > 5 * self.ui || (y - d.y0).abs() > 5 * self.ui) {
                d.active = true;
                let n = d.items.len();
                println!("[desktop] dragging {} item{}", n, if n == 1 { "" } else { "s" });
            }
            return if d.active { self.files_area().union(&self.cursor_rect()) } else { Rect::EMPTY };
        }
        let d = self.file_drag.take().unwrap();
        if !d.active {
            if let Some(i) = d.collapse {
                if i < self.files.entries.len() {
                    self.select(i);
                }
                return self.files_area();
            }
            return Rect::EMPTY;
        }
        // Dropped.
        self.file_drag = Some(d);
        let target = self.drop_target(x, y);
        let d = self.file_drag.take().unwrap();
        let area = self.files_area();
        let Some((dir, name, action)) = target else {
            self.files.status = String::from("Drop them on a folder, a drive or the Recycle Bin");
            return area;
        };
        let n = d.items.len();
        println!("[desktop] dropped {} item{} on {}: {}", n, if n == 1 { "" } else { "s" }, dir, match action {
            DropAction::Move => "move",
            DropAction::Copy => "copy",
            DropAction::Recycle => "Recycle Bin",
        });
        let status = match action {
            DropAction::Recycle => {
                let r = self.delete_items(&d.items);
                self.refresh(None);
                r.unwrap_or_else(|m| m)
            }
            DropAction::Move | DropAction::Copy => {
                let cut = action == DropAction::Move;
                let (status, _) = self.transfer(&d.items, &dir, cut);
                if !self.files.path.eq_ignore_ascii_case(&dir) {
                    self.refresh(None);
                }
                if let Some(clip) = self.clip.as_mut() {
                    // What was cut and has now moved can't be pasted.
                    if cut {
                        clip.items.retain(|it| !d.items.iter().any(|m| m.path == it.path));
                    }
                }
                if self.clip.as_ref().is_some_and(|c| c.items.is_empty()) {
                    self.clip = None;
                }
                let _ = name;
                status
            }
        };
        self.files.status = status;
        self.with_other(|d| {
            if d.files.in_folder() || d.files.path == BIN {
                let keep = d.selected_entry().map(|e| e.name);
                d.refresh(keep);
            }
        });
        area
    }

    // ------------------------------------------------------------- Photos

    fn photo_bar(&self, client: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(client.x, client.y + client.h - 48 * s, client.w, 48 * s)
    }
    fn photo_button(&self, client: &Rect, next: bool) -> Rect {
        let s = self.ui;
        let b = self.photo_bar(client);
        let cx = b.x + b.w / 2;
        Rect::new(if next { cx + 6 * s } else { cx - 56 * s }, b.y + 8 * s, 50 * s, 32 * s)
    }
    /// Where the picture goes: as big as fits, never bigger than it is.
    fn photo_rect(&self, client: &Rect) -> Rect {
        let Some(img) = self.photo.as_ref().and_then(|p| p.img.as_ref()) else { return Rect::EMPTY };
        let s = self.ui;
        let area = Rect::new(client.x + 8 * s, client.y + 8 * s, client.w - 16 * s, client.h - 64 * s);
        let (iw, ih) = (img.w as i64, img.h as i64);
        let (mut w, mut h) = (iw.min(area.w as i64), ih.min(area.h as i64));
        if w * ih > h * iw {
            w = h * iw / ih.max(1);
        } else {
            h = w * ih / iw.max(1);
        }
        let (w, h) = (w.max(1) as i32, h.max(1) as i32);
        Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h)
    }

    /// Opens a picture in Photos, with the others in its folder a step away.
    fn open_picture(&mut self, path: String) -> Rect {
        let (list, at) = siblings(&path, FileKind::Picture);
        self.photo = Some(Photo { list, at, img: None, size: (0, 0), error: String::new() });
        self.load_photo();
        self.log_window(Kind::Photos);
        self.bring(Kind::Photos)
    }

    /// Where a window is and its close button, for the tests.
    fn log_window(&self, kind: Kind) {
        let w = &self.windows[self.index(kind)];
        let x = self.caption(&w.rect, Caption::Close);
        println!("[desktop] {} window at {},{} ({}x{}); close at {},{}", w.title, w.rect.x, w.rect.y, w.rect.w, w.rect.h, x.x + x.w / 2, x.y + x.h / 2);
    }

    fn load_photo(&mut self) {
        let (sw, sh) = (self.w as usize, self.h as usize);
        let Some(p) = self.photo.as_mut() else { return };
        let path = p.list[p.at].clone();
        p.img = None;
        p.error = String::new();
        match read_whole(&path).and_then(|data| aero::media::decode_image(&data).map_err(String::from)) {
            Ok(img) => {
                p.size = (img.w, img.h);
                let format = img.format;
                p.img = Some(aero::media::shrink_to_fit(img, sw, sh));
                println!("[desktop] Photos: {}, {} {}x{} ({} of {})", path, format, p.size.0, p.size.1, p.at + 1, p.list.len());
            }
            Err(e) => {
                println!("[desktop] Photos: {}: {}", path, e);
                p.error = e;
            }
        }
        let at = self.index(Kind::Photos);
        let client = self.client(&self.windows[at].rect);
        let (prev, next) = (self.photo_button(&client, false), self.photo_button(&client, true));
        println!("[desktop] Photos: Previous at {},{}; Next at {},{}", prev.x + prev.w / 2, prev.y + prev.h / 2, next.x + next.w / 2, next.y + next.h / 2);
    }

    /// The next picture in the folder (or the one before), round to the start.
    fn step_photo(&mut self, next: bool) -> Rect {
        let Some(p) = self.photo.as_mut() else { return Rect::EMPTY };
        let n = p.list.len();
        p.at = if next { (p.at + 1) % n } else { (p.at + n - 1) % n };
        self.load_photo();
        let at = self.index(Kind::Photos);
        self.client(&self.windows[at].rect)
    }

    fn photos_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        c.gradient(*client, rgb(246, 249, 253), rgb(226, 233, 243), 255);
        let Some(p) = &self.photo else { return };
        if let Some(img) = &p.img {
            let r = self.photo_rect(client);
            c.shade(Rect::new(r.x + 3 * s, r.y + 3 * s, r.w, r.h), 0x000000, 40);
            let d = r.intersect(&c.clip);
            for y in d.y..d.y + d.h {
                let sy = ((y - r.y) as usize * img.h / r.h as usize).min(img.h - 1);
                let src = &img.px[sy * img.w..(sy + 1) * img.w];
                let row = (y * c.w) as usize;
                for x in d.x..d.x + d.w {
                    let sx = ((x - r.x) as usize * img.w / r.w as usize).min(img.w - 1);
                    c.px[row + x as usize] = src[sx];
                }
            }
        } else if !p.error.is_empty() {
            let msg = format!("Can't show this picture: {}", p.error);
            let room = ((client.w - 40 * s) / Font::Normal.w(s)).max(1) as usize;
            let msg: String = msg.chars().take(room).collect();
            let mx = client.x + (client.w - Font::Normal.width(s, &msg)) / 2;
            c.text(mx, client.y + client.h / 2 - 30 * s, &msg, rgb(90, 60, 60), Font::Normal);
        }
        // The bar: name, size and place on the left; back and forward in the middle.
        let b = self.photo_bar(client);
        c.gradient(b, rgb(220, 232, 246), rgb(188, 207, 230), 255);
        c.fill(Rect::new(b.x, b.y, b.w, 1), rgb(150, 170, 200));
        let name = split_path(&p.list[p.at]).1;
        let info = if p.img.is_some() { format!("{}  {} x {}  ({} of {})", name, p.size.0, p.size.1, p.at + 1, p.list.len()) } else { format!("{}  ({} of {})", name, p.at + 1, p.list.len()) };
        let left_room = ((self.photo_button(client, false).x - b.x - 24 * s) / Font::Small.w(s)).max(0) as usize;
        let info: String = info.chars().take(left_room).collect();
        c.text(b.x + 12 * s, b.y + (b.h - Font::Small.h(s)) / 2, &info, rgb(30, 45, 70), Font::Small);
        for next in [false, true] {
            let r = self.photo_button(client, next);
            self.round_button(c, r, if next { ">" } else { "<" });
        }
    }

    /// A rounded button with a centred label, lit while the pointer is on it.
    fn round_button(&self, c: &mut Canvas, b: Rect, label: &str) {
        let s = self.ui;
        let hot = b.contains(self.pointer.x as i32, self.pointer.y as i32);
        let (top, bottom) = if hot { (rgb(250, 252, 255), rgb(190, 215, 245)) } else { (rgb(250, 251, 253), rgb(220, 228, 238)) };
        c.rounded(b, 4 * s, false, top, bottom, 255);
        c.rounded_outline(b, 4 * s, false, rgb(110, 130, 160), 255);
        c.text(b.x + (b.w - Font::Normal.width(s, label)) / 2, b.y + (b.h - Font::Normal.h(s)) / 2, label, rgb(20, 40, 80), Font::Normal);
    }

    // -------------------------------------------------------------- Paint

    /// Paint's toolbar along the top, its colours along the bottom and the
    /// picture between them, shown at its own size from the top left.
    fn paint_toolbar(&self, client: &Rect) -> Rect {
        Rect::new(client.x, client.y, client.w, 40 * self.ui)
    }
    fn paint_palette(&self, client: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(client.x, client.y + client.h - 38 * s, client.w, 38 * s)
    }
    fn paint_area(&self, client: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(client.x, client.y + 40 * s, client.w, client.h - 78 * s)
    }
    /// Where the picture is: its top left, and the part of it shown.
    fn paint_image(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let a = self.paint_area(client);
        let (w, h) = self.paint.as_ref().map_or((0, 0), |p| (p.w, p.h));
        Rect::new(a.x + 8 * s, a.y + 8 * s, w, h)
    }
    fn paint_tool(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let w = Font::Small.width(s, "Pencil") + 14 * s;
        Rect::new(client.x + 8 * s + i as i32 * (w + 4 * s), client.y + 6 * s, w, 28 * s)
    }
    fn paint_size(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let last = self.paint_tool(client, TOOLS.len() - 1);
        Rect::new(last.x + last.w + 14 * s + i as i32 * 32 * s, client.y + 6 * s, 28 * s, 28 * s)
    }
    /// New, Undo and Save, from the right.
    fn paint_command(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let w = Font::Small.width(s, "Undo") + 20 * s;
        Rect::new(client.x + client.w - 8 * s - (3 - i as i32) * (w + 4 * s) + 4 * s, client.y + 6 * s, w, 28 * s)
    }
    fn paint_color(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let b = self.paint_palette(client);
        Rect::new(b.x + 50 * s + i as i32 * 26 * s, b.y + 8 * s, 22 * s, 22 * s)
    }
    fn paint_spots(&self, client: &Rect) -> Vec<Rect> {
        let mut v: Vec<Rect> = (0..TOOLS.len()).map(|i| self.paint_tool(client, i)).collect();
        v.extend((0..PAINT_SIZES.len()).map(|i| self.paint_size(client, i)));
        v.extend((0..3).map(|i| self.paint_command(client, i)));
        v.extend((0..PAINT_COLORS.len()).map(|i| self.paint_color(client, i)));
        v
    }

    /// Paint opens with a white picture as big as its window has room for.
    fn open_paint(&mut self) {
        let at = self.index(Kind::Paint);
        let client = self.client(&self.windows[at].rect);
        if self.paint.is_none() {
            let s = self.ui;
            let a = self.paint_area(&client);
            self.paint = Some(Paint::new((a.w - 16 * s).max(64), (a.h - 16 * s).max(64)));
        }
        self.log_paint(&client);
    }

    /// Where Paint's picture and buttons are, for the tests.
    fn log_paint(&self, client: &Rect) {
        let mid = |r: Rect| format!("{},{}", r.x + r.w / 2, r.y + r.h / 2);
        let img = self.paint_image(client);
        let tools: Vec<String> = TOOLS.iter().enumerate().map(|(i, (_, name))| format!("{} at {}", name, mid(self.paint_tool(client, i)))).collect();
        let sizes: Vec<String> = (0..PAINT_SIZES.len()).map(|i| mid(self.paint_size(client, i))).collect();
        let colors: Vec<String> = PAINT_COLORS.iter().enumerate().map(|(i, (_, name))| format!("{} at {}", name, mid(self.paint_color(client, i)))).collect();
        println!("[desktop] Paint: picture at {},{} ({}x{}); {}; sizes at {}; {}; New at {}; Undo at {}; Save at {} | end", img.x, img.y, img.w, img.h,
            tools.join(" | "), sizes.join(" | "), colors.join(" | "), mid(self.paint_command(client, 0)), mid(self.paint_command(client, 1)), mid(self.paint_command(client, 2)));
    }

    fn paint_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let Some(p) = &self.paint else { return };
        // The toolbar.
        let t = self.paint_toolbar(client);
        c.gradient(t, rgb(245, 250, 255), rgb(215, 230, 245), 255);
        c.fill(Rect::new(t.x, t.y + t.h - 1, t.w, 1), rgb(160, 180, 205));
        let hot = |r: &Rect| r.contains(self.pointer.x as i32, self.pointer.y as i32);
        let button = |c: &mut Canvas, r: Rect, on: bool| {
            let (top, bottom) = match (on, hot(&r)) {
                (true, _) => (rgb(200, 225, 250), rgb(150, 195, 240)),
                (false, true) => (rgb(235, 248, 255), rgb(170, 215, 250)),
                (false, false) => (rgb(252, 253, 255), rgb(215, 225, 240)),
            };
            c.rounded(r, 3 * s, false, top, bottom, 255);
            c.rounded_outline(r, 3 * s, false, if on { rgb(60, 110, 180) } else { rgb(120, 140, 170) }, 255);
        };
        let label = |c: &mut Canvas, r: Rect, text: &str| {
            c.text(r.x + (r.w - Font::Small.width(s, text)) / 2, r.y + (r.h - Font::Small.h(s)) / 2, text, 0x102040, Font::Small);
        };
        for (i, &(tool, name)) in TOOLS.iter().enumerate() {
            let r = self.paint_tool(client, i);
            button(c, r, p.tool == tool);
            label(c, r, name);
        }
        for (i, &radius) in PAINT_SIZES.iter().enumerate() {
            let r = self.paint_size(client, i);
            button(c, r, p.size == i);
            let d = ((2 * radius + 1) * s).min(r.w - 8 * s).max(3 * s);
            c.rounded(Rect::new(r.x + (r.w - d) / 2, r.y + (r.h - d) / 2, d, d), d / 2, false, 0x202020, 0x202020, 255);
        }
        for (i, name) in ["New", "Undo", "Save"].iter().enumerate() {
            let r = self.paint_command(client, i);
            button(c, r, false);
            label(c, r, name);
        }
        // The picture on a grey ground, with a shadow.
        let a = self.paint_area(client);
        c.fill(a, rgb(171, 182, 199));
        let img = self.paint_image(client);
        c.shade(Rect::new(img.x + 3 * s, img.y + 3 * s, img.w, img.h).intersect(&a), 0x000000, 50);
        let d = img.intersect(&a).intersect(&c.clip);
        for y in d.y..d.y + d.h {
            let src = ((y - img.y) * p.w + (d.x - img.x)) as usize;
            let row = (y * c.w + d.x) as usize;
            c.px[row..row + d.w as usize].copy_from_slice(&p.px[src..src + d.w as usize]);
        }
        // The colours: the one in use, then the rest; and how it stands.
        let b = self.paint_palette(client);
        c.gradient(b, rgb(236, 242, 250), rgb(214, 224, 238), 255);
        c.fill(Rect::new(b.x, b.y, b.w, 1), rgb(160, 180, 205));
        let now = Rect::new(b.x + 10 * s, b.y + 5 * s, 28 * s, 28 * s);
        c.fill(now, p.color);
        c.frame(now, rgb(60, 70, 90));
        for (i, &(color, _)) in PAINT_COLORS.iter().enumerate() {
            let r = self.paint_color(client, i);
            c.fill(r, color);
            c.frame(r, if color == p.color { rgb(30, 90, 200) } else { rgb(120, 130, 150) });
        }
        let after = self.paint_color(client, PAINT_COLORS.len() - 1);
        let room = ((b.x + b.w - after.x - after.w - 20 * s) / Font::Small.w(s)).max(0) as usize;
        let status = if p.status.is_empty() { format!("{} x {}", p.w, p.h) } else { p.status.clone() };
        let status: String = status.chars().take(room).collect();
        c.text(after.x + after.w + 14 * s, b.y + (b.h - Font::Small.h(s)) / 2, &status, rgb(40, 55, 80), Font::Small);
    }

    /// A press in Paint: a button, a colour, or the start of a stroke.
    fn paint_click(&mut self, client: &Rect, x: i32, y: i32) -> Rect {
        let Some(p) = self.paint.as_ref() else { return Rect::EMPTY };
        let (pw, ph) = (p.w, p.h);
        if let Some(i) = (0..TOOLS.len()).find(|&i| self.paint_tool(client, i).contains(x, y)) {
            self.paint.as_mut().unwrap().tool = TOOLS[i].0;
            println!("[desktop] Paint: {}", TOOLS[i].1);
            return self.paint_toolbar(client);
        }
        if let Some(i) = (0..PAINT_SIZES.len()).find(|&i| self.paint_size(client, i).contains(x, y)) {
            self.paint.as_mut().unwrap().size = i;
            println!("[desktop] Paint: size {}", i + 1);
            return self.paint_toolbar(client);
        }
        if let Some(i) = (0..3).find(|&i| self.paint_command(client, i).contains(x, y)) {
            return match i {
                0 => self.paint_new(client),
                1 => self.paint_undo(client),
                _ => self.save_paint(client),
            };
        }
        if let Some(i) = (0..PAINT_COLORS.len()).find(|&i| self.paint_color(client, i).contains(x, y)) {
            self.paint.as_mut().unwrap().color = PAINT_COLORS[i].0;
            println!("[desktop] Paint: colour {}", PAINT_COLORS[i].1);
            return self.paint_palette(client);
        }
        let img = self.paint_image(client);
        if !img.intersect(&self.paint_area(client)).contains(x, y) {
            return Rect::EMPTY;
        }
        let at = ((x - img.x).clamp(0, pw - 1), (y - img.y).clamp(0, ph - 1));
        let p = self.paint.as_mut().unwrap();
        p.undo.push(p.px.clone());
        if p.undo.len() > 8 {
            p.undo.remove(0);
        }
        p.changed = true;
        if p.tool == Tool::Fill {
            let n = p.flood(at.0, at.1);
            println!("[desktop] Paint: filled {} pixels at {},{}", n, at.0, at.1);
            return self.paint_area(client);
        }
        p.stroke = Some((at, at));
        p.dab(at.0, at.1);
        self.paint_area(client)
    }

    /// The pointer moved or let go while a stroke is under way.
    fn paint_step(&mut self, pt: aero::display::Pointer) -> Rect {
        let at = self.index(Kind::Paint);
        let client = self.client(&self.windows[at].rect);
        let img = self.paint_image(&client);
        let area = self.paint_area(&client);
        let Some(p) = self.paint.as_mut() else { return Rect::EMPTY };
        let Some((start, last)) = p.stroke else { return Rect::EMPTY };
        let now = ((pt.x as i32 - img.x).clamp(0, p.w - 1), (pt.y as i32 - img.y).clamp(0, p.h - 1));
        if now != last {
            match p.tool {
                Tool::Line | Tool::Box => {
                    // Drawn again from the picture before the stroke as the pointer moves.
                    if let Some(before) = p.undo.last() {
                        p.px.copy_from_slice(before);
                    }
                    if p.tool == Tool::Line {
                        p.line(start, now);
                    } else {
                        p.rectangle(start, now);
                    }
                }
                _ => p.line(last, now),
            }
            p.stroke = Some((start, now));
        }
        if pt.buttons & 1 == 0 {
            p.stroke = None;
            let name = TOOLS.iter().find(|(t, _)| *t == p.tool).map_or("", |(_, n)| n);
            println!("[desktop] Paint: {} from {},{} to {},{}", name, start.0, start.1, now.0, now.1);
        }
        area
    }

    fn paint_new(&mut self, client: &Rect) -> Rect {
        let s = self.ui;
        let a = self.paint_area(client);
        let (w, h) = ((a.w - 16 * s).max(64), (a.h - 16 * s).max(64));
        let p = self.paint.as_mut().unwrap();
        let (tool, color, size) = (p.tool, p.color, p.size);
        *p = Paint::new(w, h);
        p.tool = tool;
        p.color = color;
        p.size = size;
        println!("[desktop] Paint: new picture ({}x{})", w, h);
        *client
    }

    fn paint_undo(&mut self, client: &Rect) -> Rect {
        let p = self.paint.as_mut().unwrap();
        match p.undo.pop() {
            Some(before) => {
                p.px = before;
                p.stroke = None;
                p.changed = true;
                println!("[desktop] Paint: undone");
            }
            None => p.status = String::from("Nothing to undo"),
        }
        *client
    }

    /// Saves the picture as a BMP file in Pictures (Painting.bmp, or the next
    /// free name the first time), then reads it back to be sure.
    fn save_paint(&mut self, client: &Rect) -> Rect {
        let path = match self.paint.as_ref().and_then(|p| p.path.clone()) {
            Some(path) => path,
            None => {
                let dir = if fs_list("/Pictures").is_ok() { "/Pictures" } else { "/" };
                let taken: Vec<String> = fs_list(dir).unwrap_or_default().into_iter().map(|e| e.name).collect();
                let names: Vec<&str> = taken.iter().map(|t| t.as_str()).collect();
                join(dir, &free_name("Painting.bmp", &names, false, false))
            }
        };
        let p = self.paint.as_mut().unwrap();
        let data = p.bmp();
        p.status = match fs_write(&path, &data) {
            Ok(()) if matches!(fs_read(&path, data.len() + 1), Ok(back) if back == data) => {
                println!("[desktop] Paint: saved {} ({}x{}, {} bytes)", path, p.w, p.h, data.len());
                p.changed = false;
                let name = split_path(&path).1;
                p.path = Some(path);
                format!("Saved as {}", name)
            }
            Ok(()) => {
                println!("[desktop] Paint: {} did not read back the same", path);
                String::from("Saved, but it did not read back the same")
            }
            Err(e) => {
                println!("[desktop] Paint: could not save {} ({})", path, e);
                String::from(match e {
                    aero::E_RIGHTS => "Can't save here (read-only)",
                    aero::E_FULL => "Can't save: the disk is full",
                    _ => "Can't save",
                })
            }
        };
        self.paint_palette(client)
    }

    // -------------------------------------------------------------- Music

    /// Music's song list down the left, the song playing on the right, and
    /// the controls along the bottom.
    fn music_list(&self, client: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(client.x, client.y, (client.w * 2 / 5).min(260 * s + 8 * grow(s)), client.h - 64 * s - grow(s))
    }
    fn music_controls(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let g = grow(s);
        Rect::new(client.x, client.y + client.h - 64 * s - g, client.w, 64 * s + g)
    }
    /// The songs the list has room for, and the first one shown (so the one
    /// playing is always in view).
    fn music_rows(&self, client: &Rect) -> (usize, usize) {
        let s = self.ui;
        let g = grow(s);
        let room = ((self.music_list(client).h - 40 * s - g) / (26 * s + g)).max(1) as usize;
        let at = self.player.as_ref().map_or(0, |p| p.at);
        (room, (at + 1).saturating_sub(room))
    }
    /// Row `i` of those shown in the list.
    fn music_row(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let l = self.music_list(client);
        let g = grow(s);
        Rect::new(l.x + 6 * s, l.y + 34 * s + g + i as i32 * (26 * s + g), l.w - 12 * s, 24 * s + g)
    }
    fn music_bar(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let l = self.music_list(client);
        let x0 = l.x + l.w + 24 * s;
        Rect::new(x0, client.y + 132 * s, client.x + client.w - 24 * s - x0, 12 * s)
    }
    /// Previous, Play/Pause, Stop and Next, left to right.
    fn music_button(&self, client: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let k = self.music_controls(client);
        let (w, gap) = ((76 * s).max(Font::Small.width(s, "Pause") + 16 * s), 8 * s);
        let x0 = k.x + 16 * s;
        Rect::new(x0 + i as i32 * (w + gap), k.y + 15 * s, w, 34 * s + grow(s))
    }
    fn music_volume(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let k = self.music_controls(client);
        let last = self.music_button(client, 3);
        // Room for "Volume" before the bar and "100%" after it.
        let label = (142 * s).max(Font::Small.width(s, "Volume") + Font::Small.width(s, "100%") + 42 * s);
        let w = (k.x + k.w - (last.x + last.w) - label).clamp(60 * s, 140 * s);
        let right = (56 * s).max(Font::Small.width(s, "100%") + 16 * s);
        Rect::new(k.x + k.w - right - w, k.y + 27 * s + grow(s) / 2, w, 10 * s)
    }

    /// Sets the volume (out of 100) and keeps it for next time.
    fn set_volume(&mut self, volume: u32, muted: bool) {
        self.prefs.volume = volume.min(100);
        self.prefs.muted = muted;
        let applied = aero::audio::volume(Some((self.prefs.volume, self.prefs.muted))).is_ok();
        let saved = self.save_settings();
        println!("[desktop] volume {}%{}{}{}", self.prefs.volume, if self.prefs.muted { ", muted" } else { "" },
            if applied { "" } else { " (no sound card)" }, if saved { ", saved" } else { "" });
    }

    /// Opens a song in Music and plays it, with the others in its folder after it.
    fn open_song(&mut self, path: String) -> Rect {
        let (list, at) = siblings(&path, FileKind::Sound);
        self.player = None;
        self.player = Some(Player::new(list, at));
        self.load_song(true);
        self.log_window(Kind::Music);
        self.bring(Kind::Music)
    }

    fn load_song(&mut self, play: bool) {
        let Some(p) = self.player.as_mut() else { return };
        let path = p.list[p.at].clone();
        let mut d = p.deck.lock();
        d.song = None;
        d.playing = false;
        d.spare.clear();
        drop(d);
        p.heard = 0;
        p.error = String::new();
        match read_whole(&path).and_then(|data| aero::media::Song::open(data).map_err(String::from)) {
            Ok(song) => {
                println!("[desktop] Music: {}, {} {} Hz, {} channel{}, {} ({} of {})", path, song.format, song.rate, song.channels,
                    if song.channels == 1 { "" } else { "s" }, minutes(song.frames / 48_000), p.at + 1, p.list.len());
                let mut d = p.deck.lock();
                d.song = Some(song);
                d.playing = play;
            }
            Err(e) => {
                println!("[desktop] Music: {}: {}", path, e);
                p.error = e;
            }
        }
        let at = self.index(Kind::Music);
        let client = self.client(&self.windows[at].rect);
        let names = ["Previous", "Play", "Stop", "Next"];
        let spots: Vec<String> = (0..4).map(|i| {
            let b = self.music_button(&client, i);
            format!("{} at {},{}", names[i], b.x + b.w / 2, b.y + b.h / 2)
        }).collect();
        println!("[desktop] Music: {}", spots.join("; "));
        let (room, first) = self.music_rows(&client);
        let row = self.music_row(&client, 0);
        let vol = self.music_volume(&client);
        println!("[desktop] Music list: song {} at {},{}, rows {} apart, {} shown; volume from {} to {} at {} | end", first + 1,
            row.x + row.w / 2, row.y + row.h / 2, 26 * self.ui, room, vol.x, vol.x + vol.w, vol.y + vol.h / 2);
    }

    /// A button: 0 previous, 1 play or pause, 2 stop, 3 next.
    fn music_command(&mut self, b: usize) -> Rect {
        let at = self.index(Kind::Music);
        let client = self.client(&self.windows[at].rect);
        let Some(p) = self.player.as_mut() else { return Rect::EMPTY };
        match b {
            0 | 3 => {
                let n = p.list.len();
                p.at = if b == 3 { (p.at + 1) % n } else { (p.at + n - 1) % n };
                let play = p.deck.lock().playing;
                self.load_song(play);
            }
            1 => {
                let mut d = p.deck.lock();
                if d.song.is_some() {
                    d.playing = !d.playing;
                    println!("[desktop] music {} at {}", if d.playing { "playing" } else { "paused" }, minutes(p.heard / 48_000));
                }
            }
            _ => {
                let mut d = p.deck.lock();
                d.playing = false;
                d.spare.clear();
                if let Some(song) = d.song.as_mut() {
                    song.seek(0);
                }
                drop(d);
                p.heard = 0;
                println!("[desktop] music stopped");
            }
        }
        client
    }

    /// Follows the song the feeding thread plays: the time heard, and on
    /// to the next song at the end of one. Returns what to redraw: Music's
    /// inside, when the time shown changes.
    fn pump_music(&mut self) -> Rect {
        let at = self.index(Kind::Music);
        let client = self.client(&self.windows[at].rect);
        let Some(p) = self.player.as_mut() else { return Rect::EMPTY };
        let mut d = p.deck.lock();
        if d.no_output {
            d.no_output = false;
            drop(d);
            p.error = String::from("There is no sound output");
            println!("[desktop] Music: no sound output");
            return client;
        }
        if !d.playing {
            return Rect::EMPTY;
        }
        let before = p.heard / 48_000;
        let made = d.song.as_ref().map_or(0, |s| s.made);
        let queued = aero::audio::queued().unwrap_or(0) as u64;
        p.heard = made.saturating_sub(queued + d.spare.len() as u64 / 2);
        let done = d.song.as_ref().is_none_or(|s| s.ended()) && d.spare.is_empty() && queued == 0;
        if done {
            println!("[desktop] Music: played {} to the end", p.list[p.at]);
            if p.at + 1 < p.list.len() {
                drop(d);
                p.at += 1;
                self.load_song(true);
            } else {
                d.playing = false;
                if let Some(song) = d.song.as_mut() {
                    song.seek(0);
                }
                drop(d);
                p.heard = 0;
            }
            return client;
        }
        if p.heard / 48_000 != before { client } else { Rect::EMPTY }
    }

    fn music_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let grey = rgb(80, 85, 95);
        c.gradient(*client, rgb(250, 252, 255), rgb(225, 234, 246), 255);
        // The song list.
        let l = self.music_list(client);
        c.gradient(l, rgb(255, 255, 255), rgb(240, 245, 251), 255);
        c.fill(Rect::new(l.x + l.w - s, l.y, s, l.h), rgb(190, 205, 225));
        let count = self.player.as_ref().map_or(0, |p| p.list.len());
        c.text(l.x + 12 * s, l.y + 8 * s, &format!("Songs ({})", count), rgb(30, 57, 145), Font::Normal);
        let (room, first) = self.music_rows(client);
        let cols = |w: i32, f: Font| (w / f.w(s)).max(1) as usize;
        match &self.player {
            None => {
                let mut y = l.y + 40 * s;
                for line in ["No songs found.", "Put songs in a Music", "folder, or open one", "from Computer."] {
                    c.text(l.x + 12 * s, y, &line.chars().take(cols(l.w - 24 * s, Font::Small)).collect::<String>(), grey, Font::Small);
                    y += Font::Small.h(s) + 4 * s;
                }
            }
            Some(p) => {
                let playing = p.deck.lock().playing;
                for i in 0..room.min(p.list.len().saturating_sub(first)) {
                    let r = self.music_row(client, i);
                    let n = first + i;
                    if n == p.at {
                        c.rounded(r, 3 * s, false, rgb(220, 236, 252), rgb(193, 220, 250), 255);
                        c.rounded_outline(r, 3 * s, false, rgb(125, 162, 206), 255);
                    } else if r.contains(px, py) {
                        c.rounded(r, 3 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
                    }
                    let mark = if n == p.at && playing { ">" } else { "" };
                    c.text(r.x + 6 * s, r.y + (r.h - Font::Normal.h(s)) / 2, mark, rgb(30, 100, 200), Font::Normal);
                    let name = split_path(&p.list[n]).1;
                    let name = name.rsplit_once('.').map_or(name.as_str(), |(a, _)| a);
                    let tx = r.x + 22 * s;
                    c.text(tx, r.y + (r.h - Font::Normal.h(s)) / 2, &name.chars().take(cols(r.x + r.w - 6 * s - tx, Font::Normal)).collect::<String>(),
                        if n == p.at { rgb(20, 40, 90) } else { 0x202020 }, Font::Normal);
                }
            }
        }
        // The song playing: a note in a blue disc, its name, kind and progress.
        let x0 = l.x + l.w + 24 * s;
        let (cx, cy) = (x0 + 40 * s, client.y + 58 * s);
        c.orb(cx, cy, 38 * s, rgb(120, 190, 250), rgb(20, 80, 170));
        c.fill(Rect::new(cx + 3 * s, cy - 20 * s, 4 * s, 31 * s), 0xFFFFFF);
        c.fill(Rect::new(cx + 3 * s, cy - 20 * s, 14 * s, 6 * s), 0xFFFFFF);
        c.orb(cx - 3 * s, cy + 13 * s, 7 * s, 0xFFFFFF, rgb(220, 230, 245));
        let tx = x0 + 96 * s;
        let right = client.x + client.w - 20 * s;
        let bar = self.music_bar(client);
        let (title, line, error, heard, total, playing) = match &self.player {
            None => (String::from("Nothing playing"), String::new(), false, 0, 0, false),
            Some(p) => {
                let d = p.deck.lock();
                let line = match (&d.song, p.error.is_empty()) {
                    (_, false) => p.error.clone(),
                    (Some(song), true) => format!("{}, {}.{} kHz, {}", song.format, song.rate / 1000, song.rate % 1000 / 100,
                        if song.channels == 1 { "mono" } else { "stereo" }),
                    (None, true) => String::new(),
                };
                let total = d.song.as_ref().map_or(0, |s| s.frames);
                (split_path(&p.list[p.at]).1, line, !p.error.is_empty(), p.heard.min(total), total, d.playing)
            }
        };
        c.text(tx, client.y + 32 * s, &title.chars().take(cols(right - tx, Font::Normal)).collect::<String>(), rgb(30, 57, 145), Font::Normal);
        c.text(tx, client.y + 64 * s, &line.chars().take(cols(right - tx, Font::Small)).collect::<String>(),
            if error { rgb(170, 30, 20) } else { grey }, Font::Small);
        c.fill(bar, rgb(225, 228, 233));
        c.frame(bar, rgb(150, 155, 165));
        if total > 0 {
            let fill = ((bar.w - 2 * s) as u64 * heard / total) as i32;
            c.gradient(Rect::new(bar.x + s, bar.y + s, fill, bar.h - 2 * s), rgb(110, 180, 250), rgb(30, 100, 200), 255);
        }
        c.text(bar.x, bar.y + bar.h + 8 * s, &format!("{} / {}", minutes(heard / 48_000), minutes(total / 48_000)), rgb(60, 65, 75), Font::Small);
        if let Some(p) = &self.player {
            let place = format!("{} of {}", p.at + 1, p.list.len());
            c.text(bar.x + bar.w - Font::Small.width(s, &place), bar.y + bar.h + 8 * s, &place, rgb(60, 65, 75), Font::Small);
        }
        // The controls: a dark glass strip, as Windows 7's player has.
        let k = self.music_controls(client);
        c.gradient(k, rgb(60, 78, 104), rgb(20, 30, 46), 255);
        c.fill(Rect::new(k.x, k.y, k.w, s), rgb(120, 150, 190));
        let labels = ["|<", if playing { "Pause" } else { "Play" }, "Stop", ">|"];
        for (i, label) in labels.iter().enumerate() {
            self.round_button(c, self.music_button(client, i), label);
        }
        let v = self.music_volume(client);
        c.text(v.x - Font::Small.width(s, "Volume") - 10 * s, v.y + (v.h - Font::Small.h(s)) / 2, "Volume", rgb(220, 230, 245), Font::Small);
        c.fill(v, rgb(40, 50, 66));
        c.frame(v, rgb(140, 160, 190));
        let filled = (v.w - 1) * self.prefs.volume as i32 / 100;
        c.gradient(Rect::new(v.x + s, v.y + s, (filled - 2 * s).max(0), v.h - 2 * s), rgb(110, 180, 250), rgb(30, 100, 200), 255);
        let knob = Rect::new(v.x + filled - 6 * s, v.y - 6 * s, 12 * s, v.h + 12 * s);
        c.rounded(knob, 3 * s, false, rgb(250, 252, 255), rgb(190, 210, 235), 255);
        c.rounded_outline(knob, 3 * s, false, rgb(70, 100, 150), 255);
        let pct = if self.prefs.muted { String::from("Muted") } else { format!("{}%", self.prefs.volume) };
        c.text(v.x + v.w + 10 * s, v.y + (v.h - Font::Small.h(s)) / 2, &pct, rgb(220, 230, 245), Font::Small);
    }

    // ------------------------------------------------------ Control Panel

    /// Everything to click on the page shown, with where it is.
    fn cp_controls(&self, client: &Rect) -> Vec<(Ctl, Rect)> {
        let s = self.ui;
        let (x0, y0, g) = (client.x + 24 * s, client.y + 64 * s + grow(s), grow(s));
        let mut out = Vec::new();
        if self.cp_page == CpPage::Home {
            // Four categories, two by two.
            let (tw, th) = ((client.w - 72 * s) / 2, 96 * s);
            let pages = [CpPage::Display, CpPage::Sound, CpPage::Mouse, CpPage::Clock];
            for (i, page) in pages.iter().enumerate() {
                let (col, row) = ((i % 2) as i32, (i / 2) as i32);
                out.push((Ctl::Page(*page), Rect::new(x0 + col * (tw + 24 * s), y0 + 8 * s + row * (th + 20 * s), tw, th)));
            }
            return out;
        }
        out.push((Ctl::Back, Rect::new(client.x + 12 * s, client.y + 10 * s, 36 * s, 36 * s)));
        let bar = |y: i32| Rect::new(x0 + 220 * s, y, 280 * s, 10 * s);
        match self.cp_page {
            CpPage::Display => {
                let (sw, sh) = (128 * s, 80 * s);
                for i in 0..BACKGROUNDS.len() {
                    out.push((Ctl::Background(i), Rect::new(x0 + i as i32 * (sw + 18 * s), y0 + 34 * s + g, sw, sh)));
                }
                // - and + either side of the size's name.
                out.push((Ctl::FontStep(false), Rect::new(x0, y0 + 210 * s + 3 * g, 40 * s, 36 * s + g)));
                out.push((Ctl::FontStep(true), Rect::new(x0 + 300 * s + 4 * g, y0 + 210 * s + 3 * g, 40 * s, 36 * s + g)));
                for (k, size) in [1, 2].iter().enumerate() {
                    out.push((Ctl::TextSize(*size), Rect::new(x0 + k as i32 * (220 * s + 4 * g), y0 + 298 * s + 5 * g, 200 * s + 4 * g, 32 * s + g)));
                }
            }
            CpPage::Sound => {
                out.push((Ctl::Volume, bar(y0 + 40 * s)));
                out.push((Ctl::Mute, Rect::new(x0, y0 + 84 * s, 240 * s, 28 * s)));
                out.push((Ctl::TestSound, Rect::new(x0, y0 + 136 * s, 200 * s, 34 * s)));
            }
            CpPage::Mouse => {
                out.push((Ctl::Speed, bar(y0 + 14 * s)));
                out.push((Ctl::DoubleClick, bar(y0 + 64 * s)));
                out.push((Ctl::TestFolder, Rect::new(x0 + 520 * s, y0 + 34 * s, 80 * s, 70 * s)));
                out.push((Ctl::Swap, Rect::new(x0, y0 + 122 * s, 420 * s, 28 * s)));
            }
            CpPage::Clock => {
                for i in 0..5 {
                    let fx = x0 + i as i32 * 124 * s;
                    out.push((Ctl::Field(i, false), Rect::new(fx, y0 + 116 * s, 30 * s, 30 * s)));
                    out.push((Ctl::Field(i, true), Rect::new(fx + 84 * s, y0 + 116 * s, 30 * s, 30 * s)));
                }
                out.push((Ctl::SetClock, Rect::new(x0, y0 + 166 * s, 180 * s, 34 * s)));
                for (k, h24) in [false, true].iter().enumerate() {
                    out.push((Ctl::Hours24(*h24), Rect::new(x0 + k as i32 * 200 * s, y0 + 254 * s, 180 * s, 28 * s)));
                }
            }
            CpPage::Home => {}
        }
        out
    }

    /// Logs where the page's controls are, for tests and for reading the log.
    fn cp_log(&self) {
        let at = self.index(Kind::ControlPanel);
        let client = self.client(&self.windows[at].rect);
        let spots: Vec<String> = self.cp_controls(&client).iter().map(|(ctl, r)| {
            let name = match ctl {
                Ctl::Back => String::from("Back"),
                Ctl::Page(p) => String::from(cp_title(*p)),
                Ctl::Background(i) => String::from(BACKGROUNDS[*i].0),
                Ctl::TextSize(n) => String::from(if *n == 1 { "Normal size" } else { "Large size" }),
                Ctl::FontStep(bigger) => String::from(if *bigger { "bigger text" } else { "smaller text" }),
                Ctl::Field(i, up) => format!("{} {}", ["Year", "Month", "Day", "Hour", "Minute"][*i], if *up { "+" } else { "-" }),
                Ctl::Hours24(h) => String::from(if *h { "24-hour" } else { "12-hour" }),
                other => format!("{:?}", other),
            };
            if matches!(ctl, Ctl::Volume | Ctl::Speed | Ctl::DoubleClick) {
                format!("{} bar at {},{} to {},{}", name, r.x, r.y + r.h / 2, r.x + r.w - 1, r.y + r.h / 2)
            } else {
                format!("{} at {},{}", name, r.x + r.w / 2, r.y + r.h / 2)
            }
        }).collect();
        // "end" marks a whole line for the boot test, which reads the log as it arrives.
        println!("[desktop] Control Panel {}: {} | end", cp_title(self.cp_page), spots.join(" | "));
    }

    fn cp_go(&mut self, page: CpPage) {
        self.cp_page = page;
        if page == CpPage::Clock {
            self.cp_time = aero::now();
        }
        self.cp_log();
    }

    fn cp_click(&mut self, client: &Rect, x: i32, y: i32, double: bool) -> Rect {
        let Some((ctl, r)) = self.cp_controls(client).into_iter().find(|(_, r)| r.contains(x, y)) else { return Rect::EMPTY };
        // Where on a bar, 0 to 1000.
        let along = ((x - r.x) * 1000 / (r.w - 1).max(1)).clamp(0, 1000) as u64;
        match ctl {
            Ctl::Back => self.cp_go(CpPage::Home),
            Ctl::Page(p) => self.cp_go(p),
            Ctl::Background(i) => {
                self.prefs.background = i;
                let saved = self.save_settings();
                println!("[desktop] background: {}{}", BACKGROUNDS[i].0, if saved { ", saved" } else { "" });
                return Rect::new(0, 0, self.w, self.h);
            }
            Ctl::FontStep(bigger) => {
                let n = (self.prefs.font + if bigger { 1 } else { -1 }).clamp(-1, 3);
                if n == self.prefs.font {
                    return Rect::EMPTY;
                }
                self.prefs.font = n;
                TEXT_LEVEL.store(n, Ordering::Relaxed);
                // The windows that are mostly text take their new size where they are.
                for i in 0..self.windows.len() {
                    let kind = self.windows[i].kind;
                    if matches!(kind, Kind::Welcome | Kind::System | Kind::Notes) && self.windows[i].restore.is_none() {
                        let (_, r, _) = window_place(kind, self.w, self.h, self.ui);
                        let old = self.windows[i].rect;
                        self.windows[i].rect = Rect::new(old.x.min(self.w - r.w - 8 * self.ui).max(0), old.y.min(self.h - taskbar_h(self.ui) - 8 * self.ui - r.h).max(0), r.w, r.h);
                    }
                }
                let saved = self.save_settings();
                println!("[desktop] font size: {}{}", font_name(n), if saved { ", saved" } else { "" });
                return Rect::new(0, 0, self.w, self.h);
            }
            Ctl::TextSize(n) => {
                if n == 2 && ui_for(self.w, self.h, 2) != 2 {
                    println!("[desktop] text size: Large needs a screen of at least 1600 x 1000");
                    return *client;
                }
                self.prefs.text_size = n;
                let saved = self.save_settings();
                println!("[desktop] text size: {}{}", if n == 1 { "normal" } else { "large" }, if saved { ", saved" } else { "" });
                self.set_ui(n);
                return Rect::new(0, 0, self.w, self.h);
            }
            Ctl::Volume => self.set_volume(((along + 5) / 10) as u32, self.prefs.muted),
            Ctl::Mute => self.set_volume(self.prefs.volume, !self.prefs.muted),
            Ctl::TestSound => {
                // A short bell: 880 Hz, fading out over half a second.
                let n = 24_000usize;
                let mut wave = Vec::with_capacity(2 * n);
                for i in 0..n {
                    let phase = (i * 880 % 48_000) as f32 / 48_000.0;
                    let tri = if phase < 0.5 { 4.0 * phase - 1.0 } else { 3.0 - 4.0 * phase };
                    let fade = if i < n * 4 / 5 { 1.0 } else { (n - i) as f32 / (n / 5) as f32 };
                    let v = (tri * 12_000.0 * fade) as i16;
                    wave.push(v);
                    wave.push(v);
                }
                println!("[desktop] test sound at volume {}%{}", self.prefs.volume, if self.prefs.muted { " (muted)" } else { "" });
                // Its own thread keeps the sound card fed, so a slow redraw can't leave a gap in it.
                if let Some(old) = self.test_sound.take() {
                    old.join();
                }
                self.test_sound = aero::thread::spawn(move || play_wave(wave)).ok();
            }
            Ctl::Speed => {
                let speed = (1 + (along * 9 + 500) / 1000).clamp(1, 10);
                self.set_mouse_speed(speed);
            }
            Ctl::DoubleClick | Ctl::Swap => {
                if ctl == Ctl::DoubleClick {
                    // Left is slow (900 ms), right is fast (200 ms).
                    self.prefs.double_ms = 900 - (along * 7 + 500) / 1000 * 100;
                } else {
                    self.prefs.swapped = !self.prefs.swapped;
                }
                let _ = aero::mouse_buttons(Some((self.prefs.double_ms, self.prefs.swapped)));
                let saved = self.save_settings();
                println!("[desktop] double-click {} ms{}{}", self.prefs.double_ms,
                    if self.prefs.swapped { ", buttons swapped" } else { "" }, if saved { ", saved" } else { "" });
            }
            Ctl::TestFolder => {
                if double {
                    self.cp_folder_open = !self.cp_folder_open;
                    println!("[desktop] test folder {}", if self.cp_folder_open { "opened" } else { "closed" });
                }
            }
            Ctl::Field(i, up) => {
                let Some(t) = self.cp_time.as_mut() else { return Rect::EMPTY };
                let step = |v: i32, lo: i32, hi: i32| if up { if v >= hi { lo } else { v + 1 } } else if v <= lo { hi } else { v - 1 };
                match i {
                    0 => t.year = step(t.year as i32, 2000, 2099) as u16,
                    1 => t.month = step(t.month as i32, 1, 12) as u8,
                    2 => t.day = step(t.day as i32, 1, 31) as u8,
                    3 => t.hour = step(t.hour as i32, 0, 23) as u8,
                    _ => t.minute = step(t.minute as i32, 0, 59) as u8,
                }
                t.day = t.day.min(days_in(t.year, t.month));
            }
            Ctl::SetClock => {
                if let Some(mut t) = self.cp_time {
                    t.second = 0;
                    match aero::set_time(&t) {
                        Ok(()) => println!("[desktop] clock set to {}/{}/{} {}", t.month, t.day, t.year, time_text(t.hour, t.minute, true)),
                        Err(e) => println!("[desktop] clock not set ({})", e),
                    }
                    self.clock = clock(self.started_us, self.prefs.hours24);
                    return client.union(&self.taskbar());
                }
            }
            Ctl::Hours24(h24) => {
                self.prefs.hours24 = h24;
                self.clock = clock(self.started_us, h24);
                let saved = self.save_settings();
                println!("[desktop] clock: {}{}", if h24 { "24-hour" } else { "12-hour" }, if saved { ", saved" } else { "" });
                return client.union(&self.taskbar());
            }
        }
        *client
    }

    /// Draws and lays out everything again at scale `ui` (text size).
    fn set_ui(&mut self, ui: i32) {
        let ui = ui_for(self.w, self.h, ui);
        if ui == self.ui {
            return;
        }
        self.ui = ui;
        gfx().canvas.ui = ui;
        for i in 0..self.windows.len() {
            let (_, rect, _) = window_place(self.windows[i].kind, self.w, self.h, ui);
            self.windows[i].rect = rect;
            self.windows[i].restore = None;
        }
        self.snap = None;
        self.cp_log();
    }

    fn cp_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let (x0, y0, g) = (client.x + 24 * s, client.y + 64 * s + grow(s), grow(s));
        let blue = rgb(30, 57, 145);
        let grey = rgb(80, 85, 95);
        c.gradient(*client, rgb(250, 252, 255), rgb(228, 236, 247), 255);
        let controls = self.cp_controls(client);
        let at = |ctl: Ctl| controls.iter().find(|(c2, _)| *c2 == ctl).map(|(_, r)| *r).unwrap_or(Rect::EMPTY);
        let hot = |r: &Rect| r.contains(self.pointer.x as i32, self.pointer.y as i32);
        // The heading, with a back arrow on the pages.
        let title_x = if self.cp_page == CpPage::Home { client.x + 24 * s } else { client.x + 60 * s };
        let heading = if self.cp_page == CpPage::Home { "Adjust your computer's settings" } else { cp_title(self.cp_page) };
        c.text(title_x, client.y + 16 * s, heading, blue, Font::Normal);
        c.fill(Rect::new(client.x + 16 * s, client.y + 52 * s, client.w - 32 * s, s), rgb(200, 212, 230));
        if self.cp_page != CpPage::Home {
            let b = at(Ctl::Back);
            c.orb(b.x + b.w / 2, b.y + b.h / 2, 15 * s, if hot(&b) { rgb(140, 200, 255) } else { rgb(100, 160, 230) }, rgb(30, 80, 160));
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            c.line(cx + 7 * s, cy, cx - 6 * s, cy, 3 * s, 0xFFFFFF);
            c.line(cx - 6 * s, cy, cx, cy - 6 * s, 3 * s, 0xFFFFFF);
            c.line(cx - 6 * s, cy, cx, cy + 6 * s, 3 * s, 0xFFFFFF);
        }
        let check = |c: &mut Canvas, r: Rect, on: bool, label: &str, round: bool| {
            let b = Rect::new(r.x, r.y + (r.h - 18 * s) / 2, 18 * s, 18 * s);
            if round {
                c.orb(b.x + 9 * s, b.y + 9 * s, 9 * s, 0xFFFFFF, rgb(120, 130, 150));
                c.orb(b.x + 9 * s, b.y + 9 * s, 7 * s, 0xFFFFFF, rgb(225, 230, 238));
                if on {
                    c.orb(b.x + 9 * s, b.y + 9 * s, 4 * s, rgb(90, 170, 250), rgb(20, 80, 170));
                }
            } else {
                c.fill(b, 0xFFFFFF);
                c.frame(b, rgb(120, 130, 150));
                if on {
                    c.line(b.x + 4 * s, b.y + 9 * s, b.x + 8 * s, b.y + 13 * s, 2 * s, rgb(20, 80, 170));
                    c.line(b.x + 8 * s, b.y + 13 * s, b.x + 14 * s, b.y + 4 * s, 2 * s, rgb(20, 80, 170));
                }
            }
            c.text(b.x + 28 * s, r.y + (r.h - Font::Normal.h(s)) / 2, label, 0x202020, Font::Normal);
        };
        let slider = |c: &mut Canvas, r: Rect, filled: i32, left: &str, right: &str| {
            c.fill(r, rgb(225, 228, 233));
            c.frame(r, rgb(150, 155, 165));
            c.gradient(Rect::new(r.x + s, r.y + s, (filled - 2 * s).max(0), r.h - 2 * s), rgb(110, 180, 250), rgb(30, 100, 200), 255);
            let knob = Rect::new(r.x + filled - 6 * s, r.y - 6 * s, 12 * s, r.h + 12 * s);
            c.rounded(knob, 3 * s, false, rgb(250, 252, 255), rgb(190, 210, 235), 255);
            c.rounded_outline(knob, 3 * s, false, rgb(70, 100, 150), 255);
            c.text(r.x, r.y + r.h + 10 * s, left, grey, Font::Small);
            c.text(r.x + r.w - Font::Small.width(s, right), r.y + r.h + 10 * s, right, grey, Font::Small);
        };
        match self.cp_page {
            CpPage::Home => {
                for (ctl, r) in &controls {
                    let Ctl::Page(page) = *ctl else { continue };
                    if hot(r) {
                        c.rounded(*r, 4 * s, false, rgb(235, 245, 255), rgb(205, 228, 250), 255);
                        c.rounded_outline(*r, 4 * s, false, rgb(120, 170, 230), 255);
                    }
                    cp_icon(c, page, r.x + 14 * s, r.y + 16 * s, s);
                    c.text(r.x + 92 * s, r.y + 18 * s, cp_title(page), rgb(20, 90, 40), Font::Normal);
                    let lines: &[&str] = match page {
                        CpPage::Display => &["Background, text size"],
                        CpPage::Sound => &["Volume, test sound"],
                        CpPage::Mouse => &["Speed, double-click,", "buttons"],
                        _ => &["Set the clock,", "12 or 24 hours"],
                    };
                    for (k, line) in lines.iter().enumerate() {
                        c.text(r.x + 92 * s, r.y + 50 * s + k as i32 * 22 * s, line, grey, Font::Small);
                    }
                }
            }
            CpPage::Display => {
                c.text(x0, y0, "Desktop background", 0x202020, Font::Normal);
                for i in 0..BACKGROUNDS.len() {
                    let r = at(Ctl::Background(i));
                    let paper = Wallpaper::new(i, &r);
                    let area = r.intersect(&c.clip);
                    for y in area.y..area.y + area.h {
                        for x in area.x..area.x + area.w {
                            let i2 = (y * c.w + x) as usize;
                            c.px[i2] = paper.px(x, y);
                        }
                    }
                    let chosen = self.prefs.background == i;
                    c.frame(r, if chosen { rgb(30, 110, 220) } else { rgb(150, 160, 175) });
                    if chosen {
                        c.frame(Rect::new(r.x - 2 * s, r.y - 2 * s, r.w + 4 * s, r.h + 4 * s), rgb(30, 110, 220));
                        c.frame(Rect::new(r.x - 3 * s, r.y - 3 * s, r.w + 6 * s, r.h + 6 * s), rgb(30, 110, 220));
                    }
                    let name = BACKGROUNDS[i].0;
                    c.text(r.x + (r.w - Font::Small.width(s, name)) / 2, r.y + r.h + 8 * s, name, if chosen { blue } else { grey }, Font::Small);
                }
                c.text(x0, y0 + 176 * s + 2 * g, "Text size", 0x202020, Font::Normal);
                let minus = at(Ctl::FontStep(false));
                let plus = at(Ctl::FontStep(true));
                self.round_button(c, minus, "-");
                self.round_button(c, plus, "+");
                let name = ["Small", "Medium", "Large", "Larger", "Largest"][(self.prefs.font + 1).clamp(0, 4) as usize];
                let mid = minus.x + minus.w + (plus.x - minus.x - minus.w - Font::Normal.width(s, name)) / 2;
                c.text(mid, minus.y + (minus.h - Font::Normal.h(s)) / 2, name, blue, Font::Normal);
                c.text(x0, y0 + 262 * s + 4 * g, "Size of everything", 0x202020, Font::Normal);
                let large_ok = ui_for(self.w, self.h, 2) == 2;
                check(c, at(Ctl::TextSize(1)), self.ui == 1, "Normal", true);
                check(c, at(Ctl::TextSize(2)), self.ui == 2, "Twice as big", true);
                let note = if large_ok { String::new() } else { String::from(" (twice as big needs 1600 x 1000)") };
                // Only when it fits: with the biggest text it would run out of the window.
                let line = format!("Screen: {} x {}{}", self.w, self.h, note);
                let ny = y0 + 344 * s + 6 * g;
                if ny + Font::Small.h(s) <= client.y + client.h - 4 * s && x0 + Font::Small.width(s, &line) <= client.x + client.w - 8 * s {
                    c.text(x0, ny, &line, grey, Font::Small);
                }
            }
            CpPage::Sound => {
                let (v, muted) = (self.prefs.volume, self.prefs.muted);
                let card = aero::audio::volume(None).is_ok() && aero::audio::queued().is_ok();
                c.text(x0, y0, if card { "Speakers (the sound card's main output)" } else { "No sound card was found" }, if card { 0x202020 } else { rgb(170, 30, 20) }, Font::Normal);
                c.text(x0, y0 + 32 * s, "Volume", 0x202020, Font::Normal);
                let r = at(Ctl::Volume);
                slider(c, r, (r.w - 1) * v as i32 / 100, "", "");
                c.text(r.x + r.w + 18 * s, y0 + 32 * s, &format!("{}%", v), blue, Font::Normal);
                check(c, at(Ctl::Mute), muted, "Mute", false);
                let t = at(Ctl::TestSound);
                self.round_button(c, t, "Play a test sound");
            }
            CpPage::Mouse => {
                c.text(x0, y0 + 6 * s, "Pointer speed", 0x202020, Font::Normal);
                let r = at(Ctl::Speed);
                slider(c, r, (r.w - 1) * (self.mouse_speed as i32 - 1) / 9, "Slow", "Fast");
                c.text(x0, y0 + 56 * s, "Double-click speed", 0x202020, Font::Normal);
                let r = at(Ctl::DoubleClick);
                slider(c, r, (r.w - 1) * (900 - self.prefs.double_ms as i32) / 700, "Slow", "Fast");
                let f = at(Ctl::TestFolder);
                if self.cp_folder_open {
                    c.folder(f.x + 16 * s, f.y + 8 * s, 3 * s);
                    c.page(f.x + 30 * s, f.y + 2 * s, 2 * s);
                } else {
                    c.folder(f.x + 16 * s, f.y + 8 * s, 3 * s);
                }
                c.text(f.x - 30 * s, f.y + f.h + 4 * s, "Double-click", grey, Font::Small);
                c.text(f.x - 30 * s, f.y + f.h + 24 * s, "to test", grey, Font::Small);
                check(c, at(Ctl::Swap), self.prefs.swapped, "Switch primary and secondary buttons", false);
            }
            CpPage::Clock => {
                let now = aero::now();
                let (time, date) = match now {
                    Some(t) => (time_text(t.hour, t.minute, self.prefs.hours24), format!("{}:{:02}  {}/{}/{}", t.minute, t.second, t.month, t.day, t.year)),
                    None => (String::from("--:--"), String::from("The PC's clock can't be read")),
                };
                let _ = date;
                c.text(x0, y0, &time, blue, Font::Large);
                if let Some(t) = now {
                    c.text(x0 + Font::Large.width(s, &time) + 16 * s, y0 + 8 * s, &format!("{}  {}/{}/{}", weekday(t.year, t.month, t.day), t.month, t.day, t.year), grey, Font::Normal);
                }
                c.text(x0, y0 + 58 * s, "Change the date and time:", 0x202020, Font::Normal);
                if let Some(t) = self.cp_time {
                    let values = [t.year as u32, t.month as u32, t.day as u32, t.hour as u32, t.minute as u32];
                    for i in 0..5 {
                        let label = ["Year", "Month", "Day", "Hour", "Minute"][i];
                        let minus = at(Ctl::Field(i, false));
                        c.text(minus.x, minus.y - 26 * s, label, grey, Font::Small);
                        self.round_button(c, minus, "-");
                        self.round_button(c, at(Ctl::Field(i, true)), "+");
                        let v = if i == 0 { format!("{}", values[i]) } else { format!("{:02}", values[i]) };
                        let mid = minus.x + 57 * s;
                        c.text(mid - Font::Normal.width(s, &v) / 2, minus.y + (minus.h - Font::Normal.h(s)) / 2, &v, 0x202020, Font::Normal);
                    }
                }
                self.round_button(c, at(Ctl::SetClock), "Set the clock");
                c.text(x0, y0 + 226 * s, "Clock in the taskbar", 0x202020, Font::Normal);
                check(c, at(Ctl::Hours24(false)), !self.prefs.hours24, "12-hour", true);
                check(c, at(Ctl::Hours24(true)), self.prefs.hours24, "24-hour", true);
            }
        }
    }

    // ------------------------------------------------------ copy progress
    //
    // Copying and moving run here, a piece at a time; between pieces the
    // progress window is drawn (once the copy has taken a moment) and the
    // pointer and keys are looked at for Cancel or Esc. Other clicks and
    // keys wait until it is done, as with a dialog box.

    fn progress_rect(&self) -> Rect {
        let s = self.ui;
        let (w, h) = (460 * s, 172 * s);
        Rect::new((self.w - w) / 2, (self.h - h) / 2 - 40 * s, w, h)
    }
    fn progress_area(&self) -> Rect {
        let r = self.progress_rect();
        let m = 8 * self.ui;
        Rect::new(r.x - m, r.y - m, r.w + 2 * m, r.h + 2 * m)
    }
    fn progress_cancel(&self) -> Rect {
        let s = self.ui;
        let r = self.progress_rect();
        Rect::new(r.x + r.w - 122 * s, r.y + r.h - 46 * s, 106 * s, 32 * s)
    }

    /// Called as each piece is copied: counts it, draws, and looks for Cancel.
    fn copy_tick(&mut self, file: &str, bytes: u64) -> bool {
        let now = aero::clock_us();
        let mut dirty = Rect::EMPTY;
        let Some(p) = self.progress.as_mut() else { return true };
        p.done += bytes;
        if p.file != file {
            p.file = String::from(file);
        }
        // Shown only once a copy takes a moment (a fifth of a second), so
        // small ones don't flash it.
        if !p.shown && now - p.started_us >= 200_000 {
            p.shown = true;
            p.drawn_us = 0;
            let c = self.progress_cancel();
            println!("[desktop] {} window up: {} bytes to copy; Cancel at {},{}", if self.progress.as_ref().unwrap().moving { "moving" } else { "copying" },
                self.progress.as_ref().unwrap().total, c.x + c.w / 2, c.y + c.h / 2);
        }
        let shown = self.progress.as_ref().unwrap().shown;
        // The pointer: drawn where it is, and the button pressed (or held
        // down) on Cancel cancels.
        let g = gfx();
        let pt = g.screen.pointer().unwrap_or(self.pointer);
        let mut pressed = pt.buttons & 1 != 0;
        if pt != self.pointer {
            let old = self.pointer;
            dirty = dirty.union(&self.cursor_rect());
            self.pointer = pt;
            dirty = dirty.union(&self.cursor_rect());
            pressed |= pt.presses.wrapping_sub(old.presses) & 0xF_FFFF != 0;
            if shown {
                dirty = dirty.union(&self.progress_cancel());
            }
        }
        if shown && pressed && self.progress_cancel().contains(pt.x as i32, pt.y as i32) {
            self.cancel_copy();
        }
        let mut keys = [(0u8, 0u8); 16];
        if let Ok(n) = g.screen.keys_with_modifiers(&mut keys) {
            if keys[..n].iter().any(|&(k, _)| k == 27) {
                self.cancel_copy();
            }
        }
        let p = self.progress.as_mut().unwrap();
        if p.shown && (now - p.drawn_us >= 60_000 || p.cancel) {
            p.drawn_us = now;
            dirty = dirty.union(&self.progress_area());
        }
        if !dirty.is_empty() {
            present(&g.screen, &mut g.canvas, self, dirty);
        }
        !self.progress.as_ref().unwrap().cancel
    }

    fn cancel_copy(&mut self) {
        if let Some(p) = self.progress.as_mut() {
            if !p.cancel {
                p.cancel = true;
                println!("[desktop] Cancel pressed with {} of {} bytes copied", p.done, p.total);
            }
        }
    }

    /// Takes the progress window away, if it was up.
    fn end_progress(&mut self) {
        let Some(p) = self.progress.take() else { return };
        if p.shown {
            let g = gfx();
            present(&g.screen, &mut g.canvas, self, self.progress_area());
        }
    }

    fn progress_view(&self, c: &mut Canvas) {
        let s = self.ui;
        let Some(p) = self.progress.as_ref().filter(|p| p.shown) else { return };
        let r = self.progress_rect();
        if r.intersect(&c.clip).is_empty() && self.progress_area().intersect(&c.clip).is_empty() {
            return;
        }
        c.shade(Rect::new(r.x + 5 * s, r.y + 5 * s, r.w, r.h), 0x000000, 60);
        c.rounded(r, 5 * s, false, rgb(252, 253, 255), rgb(234, 240, 249), 255);
        c.rounded_outline(r, 5 * s, false, rgb(100, 125, 165), 255);
        let fw = Font::Normal.w(s);
        let room = ((r.w - 40 * s) / fw).max(1) as usize;
        let verb = if p.moving { "Moving" } else { "Copying" };
        let what = if p.count == 1 { String::from("1 item") } else { format!("{} items", p.count) };
        let head: String = format!("{} {} to {}", verb, what, p.to).chars().take(room).collect();
        c.text(r.x + 20 * s, r.y + 16 * s, &head, rgb(30, 57, 145), Font::Normal);
        let name: String = split_path(&p.file).1.chars().take(room).collect();
        c.text(r.x + 20 * s, r.y + 44 * s, &name, rgb(60, 65, 75), Font::Small);
        // The bar: a green fill in a sunken trough, as in Windows 7.
        let bar = Rect::new(r.x + 20 * s, r.y + 70 * s, r.w - 40 * s, 18 * s);
        c.fill(bar, rgb(230, 230, 230));
        c.frame(bar, rgb(150, 155, 165));
        let frac = if p.total == 0 { 1.0 } else { (p.done.min(p.total)) as f32 / p.total as f32 };
        let fill = ((bar.w - 2 * s) as f32 * frac) as i32;
        if fill > 0 {
            c.gradient(Rect::new(bar.x + s, bar.y + s, fill, bar.h - 2 * s), rgb(130, 220, 120), rgb(40, 160, 50), 255);
        }
        let mb = |b: u64| (b + 512 * 1024) / (1024 * 1024);
        let amount = if p.total >= 1024 * 1024 {
            format!("{} MB of {} MB ({}%)", mb(p.done.min(p.total)), mb(p.total), (frac * 100.0) as u32)
        } else {
            format!("{} KB of {} KB ({}%)", p.done.min(p.total).div_ceil(1024), p.total.div_ceil(1024), (frac * 100.0) as u32)
        };
        c.text(r.x + 20 * s, r.y + 98 * s, &amount, rgb(60, 65, 75), Font::Small);
        if p.cancel {
            c.text(r.x + 20 * s, r.y + r.h - 38 * s, "Cancelling...", rgb(170, 30, 20), Font::Small);
        }
        let b = self.progress_cancel();
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let hot = b.contains(px, py);
        let (top, bottom) = if hot { (rgb(250, 252, 255), rgb(190, 215, 245)) } else { (rgb(250, 251, 253), rgb(220, 228, 238)) };
        c.rounded(b, 3 * s, false, top, bottom, 255);
        c.rounded_outline(b, 3 * s, false, rgb(110, 130, 160), 255);
        c.text(b.x + (b.w - Font::Normal.width(s, "Cancel")) / 2, b.y + (b.h - Font::Normal.h(s)) / 2, "Cancel", rgb(20, 40, 80), Font::Normal);
    }

    // ------------------------------------------------------ right-click menu

    fn popup_size(&self) -> (i32, i32) {
        let s = self.ui;
        let Some(p) = &self.popup else { return (0, 0) };
        let w = p.items.iter().map(|(_, l, _)| Font::Normal.width(s, l)).max().unwrap_or(0) + 64 * s;
        let h = p.items.iter().map(|(a, _, _)| if *a == Act::Line { 9 * s } else { 30 * s }).sum::<i32>() + 6 * s;
        (w, h)
    }
    fn popup_area(&self) -> Rect {
        let Some(p) = &self.popup else { return Rect::EMPTY };
        let (w, h) = self.popup_size();
        // With room for its shadow.
        Rect::new(p.x, p.y, w + 4 * self.ui, h + 4 * self.ui)
    }
    fn popup_item(&self, i: usize) -> Rect {
        let s = self.ui;
        let Some(p) = &self.popup else { return Rect::EMPTY };
        let (w, _) = self.popup_size();
        let mut y = p.y + 3 * s;
        for (j, (a, _, _)) in p.items.iter().enumerate() {
            let h = if *a == Act::Line { 9 * s } else { 30 * s };
            if j == i {
                return if *a == Act::Line { Rect::EMPTY } else { Rect::new(p.x + 3 * s, y, w - 6 * s, h) };
            }
            y += h;
        }
        Rect::EMPTY
    }

    /// Opens the menu at (x, y), kept on the screen, and logs its lines.
    fn show_popup(&mut self, x: i32, y: i32, items: Vec<(Act, &'static str, bool)>, place: Option<String>) -> Rect {
        self.popup = Some(Popup { x, y, items, place });
        let (w, h) = self.popup_size();
        let (sw, sh) = (self.w, self.h - 40 * self.ui);
        if let Some(p) = self.popup.as_mut() {
            p.x = x.min(sw - w - 4);
            p.y = if y + h > sh { (y - h).max(0) } else { y };
        }
        let p = self.popup.as_ref().unwrap();
        let lines: Vec<String> = (0..p.items.len())
            .filter(|&i| p.items[i].0 != Act::Line)
            .map(|i| {
                let r = self.popup_item(i);
                format!("{}{} at {},{}", p.items[i].1, if p.items[i].2 { "" } else { " (off)" }, r.x + r.w / 2, r.y + r.h / 2)
            })
            .collect();
        println!("[desktop] menu: {}", lines.join(" | "));
        self.popup_area()
    }

    fn popup_view(&self, c: &mut Canvas) {
        let s = self.ui;
        let Some(p) = &self.popup else { return };
        let (w, h) = self.popup_size();
        let r = Rect::new(p.x, p.y, w, h);
        // A soft shadow, then the menu: light grey with a white gutter edge, as in Windows 7.
        c.shade(Rect::new(r.x + 4 * s, r.y + 4 * s, r.w, r.h), 0x000000, 50);
        c.fill(r, rgb(240, 240, 240));
        c.frame(r, rgb(151, 151, 151));
        c.fill(Rect::new(r.x + 30 * s, r.y + 2 * s, 1, r.h - 4 * s), rgb(226, 227, 227));
        c.fill(Rect::new(r.x + 31 * s, r.y + 2 * s, 1, r.h - 4 * s), 0xFFFFFF);
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let mut y = r.y + 3 * s;
        for (i, (act, label, on)) in p.items.iter().enumerate() {
            if *act == Act::Line {
                c.fill(Rect::new(r.x + 34 * s, y + 4 * s, r.w - 38 * s, 1), rgb(226, 227, 227));
                c.fill(Rect::new(r.x + 34 * s, y + 5 * s, r.w - 38 * s, 1), 0xFFFFFF);
                y += 9 * s;
                continue;
            }
            let b = self.popup_item(i);
            if *on && b.contains(px, py) {
                c.rounded(b, 3 * s, false, rgb(241, 246, 252), rgb(220, 234, 250), 255);
                c.rounded_outline(b, 3 * s, false, rgb(168, 202, 238), 255);
            }
            let ink = if *on { rgb(0, 0, 0) } else { rgb(160, 160, 160) };
            let font = if *act == Act::Open { Font::Bold } else { Font::Normal };
            c.text(r.x + 40 * s, b.y + (b.h - Font::Normal.h(s)) / 2, label, ink, font);
            let _ = i;
            y += 30 * s;
        }
    }

    fn popup_click(&mut self, x: i32, y: i32) -> Rect {
        let area = self.popup_area();
        let n = self.popup.as_ref().map_or(0, |p| p.items.len());
        let hit = (0..n).find(|&i| self.popup_item(i).contains(x, y));
        let Some(p) = self.popup.take() else { return Rect::EMPTY };
        let Some(i) = hit else { return area };
        let (act, label, on) = p.items[i];
        if !on {
            return area;
        }
        println!("[desktop] menu: chose {}", label);
        let r = match act {
            Act::Open => match p.place {
                Some(path) if path == BIN || path.is_empty() || net_path(&path).is_none() => self.navigate(path),
                Some(path) => {
                    let n = shares().iter().position(|sh| sh.root() == path).unwrap_or(0);
                    self.open_share(n)
                }
                None => self.open_selected(),
            },
            Act::OpenInTab => {
                let path = match p.place {
                    Some(path) => path,
                    None => match self.files.selected {
                        Some(i) if self.files.path.is_empty() => {
                            if i < self.files.drives.len() { self.files.drives[i].path.clone() } else { shares()[i - self.files.drives.len()].root() }
                        }
                        Some(i) => self.entry_path(i),
                        None => return area,
                    },
                };
                self.new_tab();
                self.navigate(path)
            }
            Act::Do(cmd) => self.command(cmd),
            Act::Refresh => {
                let keep = self.selected_entry().map(|e| e.name);
                self.refresh(keep);
                self.files_area()
            }
            Act::View => self.toggle_icons(),
            Act::ShowDeleted => {
                self.files.lost_in = match self.files.path.as_str() {
                    BIN => self.emptied_root().map_or_else(String::new, |r| join(&r, BIN_DIR)),
                    p => String::from(p),
                };
                self.navigate(String::from(DELETED))
            }
            Act::Writes(on) => {
                let Some(d) = self.files.selected.filter(|&d| d < self.files.drives.len()) else { return area };
                let (path, name) = (self.files.drives[d].path.clone(), self.drive_name(d));
                match aero::set_writable(&path, on) {
                    Ok(()) => {
                        println!("[desktop] {} is {}", name, if on { "writable" } else { "read-only" });
                        self.files.status = String::from(if on {
                            "You can now change the files on this drive"
                        } else {
                            "This drive is read-only again"
                        });
                    }
                    Err(why) => {
                        println!("[desktop] can't allow changes to {}: {}", name, why);
                        self.files.status = format!("Can't allow changes: {}", why);
                    }
                }
                if let Ok(drives) = aero::volumes() {
                    self.files.drives = drives;
                }
                self.files_area()
            }
            Act::Line => Rect::EMPTY,
        };
        area.union(&r).union(&self.sync_preview())
    }

    /// A right-click: on the Computer window, a menu for what is under the
    /// pointer (selecting it first), as in Explorer.
    fn right_click(&mut self, x: i32, y: i32) -> Rect {
        if self.popup.is_some() {
            let a = self.popup_area();
            self.popup = None;
            return a;
        }
        let Some(top) = self.windows.iter().rposition(|w| w.shown() && w.rect.contains(x, y)) else { return Rect::EMPTY };
        if self.windows[top].kind != Kind::Computer {
            return Rect::EMPTY;
        }
        let mut dirty = Rect::EMPTY;
        if top != self.windows.len() - 1 {
            dirty = self.bring(Kind::Computer);
        }
        let c = self.files_client();
        if self.files.rename.is_some() {
            self.finish_rename();
        }
        self.files.confirm = None;
        self.files.props = None;
        self.files.query_focus = false;
        // The other side of two panes comes into use first.
        if self.other.is_some() && self.side(&c, !self.right).contains(x, y) {
            dirty = dirty.union(&self.switch_side());
        }
        let on = |d: &Self, cmd: Command| d.command_enabled(cmd);
        use Act::*;
        // The pane: Open, Open in new tab.
        if self.files_pane(&c).contains(x, y) {
            let rows = self.places();
            let Some(i) = (0..rows.len()).find(|&i| self.pane_item(&c, i).contains(x, y)) else { return dirty };
            let path = rows[i].path.clone();
            let items = alloc::vec![(Open, "Open", true), (OpenInTab, "Open in new tab", path != BIN)];
            return dirty.union(&self.files_area()).union(&self.show_popup(x, y, items, Some(path)));
        }
        let k = self.files_content(&c);
        if !k.contains(x, y) || self.files.form.is_some() {
            return dirty;
        }
        let items: Vec<(Act, &'static str, bool)> = if self.files.path.is_empty() {
            // Computer: a drive, or the empty space.
            match (0..self.tile_count()).find(|&i| self.drive_tile(&c, i).contains(x, y)) {
                Some(i) => {
                    self.select(i);
                    let mut v = alloc::vec![(Open, "Open", true), (OpenInTab, "Open in new tab", true), (Line, "", false)];
                    if i >= self.files.drives.len() {
                        v.push((Do(Command::Disconnect), "Disconnect", true));
                    } else if self.files.drives[i].kind == "NTFS" {
                        // Windows drives are read-only until asked.
                        v.push(if self.files.drives[i].read_only { (Writes(true), "Allow changes", true) } else { (Writes(false), "Make read-only", true) });
                        v.push((Line, "", false));
                    }
                    v.push((Do(Command::Properties), "Properties", true));
                    v
                }
                None => {
                    self.files.selected = None;
                    alloc::vec![(Do(Command::Map), "Map network drive...", true), (Refresh, "Refresh", true)]
                }
            }
        } else {
            let rows = self.files_rows(&c);
            let i = if rows.contains(x, y) { self.files.scroll + self.cell_at(&rows, x, y) } else { usize::MAX };
            if i < self.files.entries.len() {
                if !self.is_marked(i) {
                    self.select(i);
                }
                if self.files.path == BIN {
                    alloc::vec![(Do(Command::Restore), "Restore", true), (Line, "", false), (Do(Command::Delete), "Delete", true), (Line, "", false), (Do(Command::Properties), "Properties", true)]
                } else if self.files.path == DELETED {
                    alloc::vec![(Do(Command::Restore), "Restore", on(self, Command::Restore))]
                } else {
                    let one_folder = self.picked().len() == 1 && self.files.entries[i].is_dir;
                    let mut v = alloc::vec![(Open, "Open", self.picked().len() == 1)];
                    if one_folder {
                        v.push((OpenInTab, "Open in new tab", true));
                    }
                    v.extend([
                        (Line, "", false),
                        (Do(Command::Cut), "Cut", on(self, Command::Cut)),
                        (Do(Command::Copy), "Copy", on(self, Command::Copy)),
                    ]);
                    if one_folder {
                        v.push((Do(Command::Paste), "Paste", on(self, Command::Paste)));
                    }
                    v.extend([
                        (Line, "", false),
                        (Do(Command::Delete), "Delete", on(self, Command::Delete)),
                        (Do(Command::Rename), "Rename", on(self, Command::Rename)),
                        (Line, "", false),
                        (Do(Command::Properties), "Properties", true),
                    ]);
                    v
                }
            } else {
                // The empty space of a folder.
                self.files.selected = None;
                self.files.marked.clear();
                if self.files.path == BIN {
                    alloc::vec![
                        (Do(Command::Empty), "Empty Recycle Bin", on(self, Command::Empty)),
                        (Refresh, "Refresh", true),
                        (Line, "", false),
                        (ShowDeleted, "Show emptied files", self.emptied_root().is_some()),
                    ]
                } else if self.files.path == DELETED {
                    alloc::vec![(Refresh, "Refresh", true)]
                } else {
                    alloc::vec![
                        (View, if self.icons { "Details" } else { "Large icons" }, true),
                        (Refresh, "Refresh", true),
                        (Line, "", false),
                        (Do(Command::Paste), "Paste", on(self, Command::Paste)),
                        (Line, "", false),
                        (Do(Command::NewFolder), "New folder", on(self, Command::NewFolder)),
                        (Line, "", false),
                        (ShowDeleted, "Show deleted files", self.can_undelete()),
                    ]
                }
            }
        };
        dirty.union(&self.files_area()).union(&self.sync_preview()).union(&self.show_popup(x, y, items, None))
    }

    /// A click inside the Computer window.
    fn files_click(&mut self, x: i32, y: i32, double: bool) -> Rect {
        let c = self.files_client();
        let area = self.files_area();
        // Clicking anywhere but the name being typed finishes it.
        if self.files.rename.is_some() {
            let on_it = self.files.selected.is_some_and(|i| i >= self.files.scroll && self.files_row(&c, i - self.files.scroll).contains(x, y));
            if !on_it {
                self.finish_rename();
                return area;
            }
            return Rect::EMPTY;
        }
        if self.files.props.is_some() {
            // OK, or a click anywhere else, closes it.
            self.files.props = None;
            return area;
        }
        if self.files_tabs(&c).contains(x, y) {
            if self.panes_button(&c).contains(x, y) {
                return self.toggle_panes();
            }
            if self.preview_button(&c).contains(x, y) {
                return self.toggle_preview();
            }
            if self.view_button(&c).contains(x, y) {
                return self.toggle_icons();
            }
            if self.new_tab_button(&c).contains(x, y) {
                return self.new_tab();
            }
            for i in 0..self.tabs.len() {
                if self.tabs.len() > 1 && self.tab_close(&c, i).contains(x, y) {
                    return self.close_tab(i);
                }
                if self.tab_rect(&c, i).contains(x, y) {
                    return self.switch_tab(i);
                }
            }
            return Rect::EMPTY;
        }
        if self.preview_rect(&c).contains(x, y) {
            for hex in [false, true] {
                if self.preview_mode(&c, hex).contains(x, y) {
                    return self.preview_as(hex);
                }
            }
            let bar = self.preview_bar(&c);
            let (shown, total) = (self.preview_lines_shown(&c), self.preview_total(&c));
            if let Some(p) = self.preview.as_mut().filter(|_| bar.contains(x, y) && total > shown) {
                let thumb = bar.y + (bar.h as usize * p.scroll / total) as i32;
                p.scroll = if y < thumb { p.scroll.saturating_sub(shown) } else { (p.scroll + shown).min(total - shown) };
                return self.preview_rect(&c);
            }
            return Rect::EMPTY;
        }
        if self.other.is_some() && self.side(&c, !self.right).contains(x, y) {
            // The other side comes into use, and gets the click.
            let header = self.side_header(&c, !self.right).contains(x, y);
            let r = self.switch_side();
            return if header { r } else { r.union(&self.files_click(x, y, double)) };
        }
        if self.files_search(&c).contains(x, y) {
            self.files.query_focus = true;
            return area;
        }
        self.files.query_focus = false;
        for i in 0..3 {
            let (bx, by, br) = self.files_arrow(&c, i);
            if (x - bx) * (x - bx) + (y - by) * (y - by) <= br * br {
                return match i {
                    0 => self.go_back(),
                    1 => self.go_forward(),
                    _ => self.go_up(),
                };
            }
        }
        if let Some((_, _, path)) = self.crumb_rects(&c).into_iter().find(|(r, _, _)| r.contains(x, y)) {
            return self.navigate(path);
        }
        if let Some((cmd, _, _)) = self.command_buttons(&c).into_iter().find(|(_, _, r)| r.contains(x, y)) {
            return self.command(cmd);
        }
        if self.files.confirm.is_some() {
            // Anything else answers No.
            return self.command(Command::No);
        }
        if self.files_pane(&c).contains(x, y) {
            let places = self.places();
            if let Some(i) = (0..places.len()).find(|&i| self.pane_item(&c, i).contains(x, y)) {
                let row = places[i].clone();
                if row.open.is_some() && self.pane_arrow(&c, i, row.depth).contains(x, y) {
                    return self.tree_toggle(&row.path);
                }
                let (path, kind) = (row.path, row.kind);
                if kind == Place::Network {
                    let n = shares().iter().position(|s| s.root() == path).unwrap_or(0);
                    return self.open_share(n);
                }
                return self.navigate(path);
            }
            return Rect::EMPTY;
        }
        if self.files.form.is_some() {
            return if self.files_content(&c).contains(x, y) { self.form_click(x, y) } else { Rect::EMPTY };
        }
        if self.files.path.is_empty() {
            let hit = (0..self.tile_count()).find(|&i| self.drive_tile(&c, i).contains(x, y));
            match hit {
                Some(i) => {
                    self.select(i);
                    if double {
                        return self.open_selected();
                    }
                }
                None => self.files.selected = None,
            }
            return area;
        }
        let head = self.files_head(&c);
        if head.contains(x, y) && self.files.path != BIN {
            let (date_x, type_x, size_x) = self.files_columns(&c);
            let by = if x >= size_x - 8 * self.ui {
                SortBy::Size
            } else if x >= type_x - 8 * self.ui {
                SortBy::Type
            } else if date_x < type_x && x >= date_x - 8 * self.ui {
                SortBy::Date
            } else {
                SortBy::Name
            };
            self.files.descending = self.files.sort == by && !self.files.descending;
            self.files.sort = by;
            let keep = self.selected_entry().map(|e| e.name);
            self.sort_entries();
            self.files.selected = keep.and_then(|k| self.files.entries.iter().position(|e| e.name == k));
            return area;
        }
        let sb = self.files_scrollbar(&c);
        if sb.contains(x, y) {
            let visible = self.files_visible(&c);
            let across = self.files_across(&c);
            let last = self.files.entries.len().div_ceil(across).saturating_sub(visible / across) * across;
            let arrow = 16 * self.ui;
            let scroll = self.files.scroll;
            self.files.scroll = if y < sb.y + arrow {
                scroll.saturating_sub(across)
            } else if y >= sb.y + sb.h - arrow {
                (scroll + across).min(last)
            } else if y < sb.y + sb.h / 2 {
                scroll.saturating_sub(visible)
            } else {
                (scroll + visible).min(last)
            };
            return area;
        }
        let rows = self.files_rows(&c);
        if !rows.contains(x, y) {
            return Rect::EMPTY;
        }
        let i = self.files.scroll + self.cell_at(&rows, x, y);
        let keys = self.pointer.keys;
        if i >= self.files.entries.len() {
            if keys & (aero::display::MOD_CTRL | aero::display::MOD_SHIFT) == 0 {
                self.files.selected = None;
                self.files.marked.clear();
                self.files.status = format!("{} items", self.files.entries.len());
            }
            return area;
        }
        if double {
            self.select(i);
            return self.open_selected();
        }
        if keys & aero::display::MOD_CTRL != 0 {
            self.toggle(i);
        } else if keys & aero::display::MOD_SHIFT != 0 {
            self.extend(i);
        } else if self.is_marked(i) && self.picked().len() > 1 {
            // Pressed on one of several selected: they stay selected so
            // they can be dragged together; a plain click (no drag) leaves
            // just this one selected when the button comes up.
            self.files.selected = Some(i);
            self.press_drag(x, y, Some(i));
            return area;
        } else {
            self.select(i);
        }
        if keys & (aero::display::MOD_CTRL | aero::display::MOD_SHIFT) == 0 {
            self.press_drag(x, y, None);
        }
        area
    }

    /// A key typed while the Computer window is in front.
    fn files_key(&mut self, k: u8) -> Rect {
        let area = self.files_area();
        if let Some(name) = self.files.rename.as_mut() {
            // While the whole name is selected, typing or Backspace replaces it.
            if core::mem::take(&mut self.files.rename_all) && (k == 8 || (32..=126).contains(&k)) {
                name.clear();
            }
            match k {
                b'\n' => self.finish_rename(),
                27 => {
                    self.files.rename = None;
                    self.files.status = String::new();
                }
                8 => {
                    name.pop();
                }
                32..=126 if name.len() < 120 && !BAD_NAME_CHARS.contains(k as char) => name.push(k as char),
                32..=126 => self.files.status = String::from("A name can't have any of \\ / : * ? \" < > |"),
                _ => return Rect::EMPTY,
            }
            return area;
        }
        if self.files.form.is_some() {
            return self.form_key(k);
        }
        if self.files.query_focus {
            return self.search_key(k);
        }
        if self.files.props.is_some() {
            if matches!(k, b'\n' | 27 | b' ') {
                self.files.props = None;
                return area;
            }
            return Rect::EMPTY;
        }
        if k == b'\n' && self.key_mods & aero::display::MOD_ALT != 0 {
            // Alt+Enter.
            return self.command(Command::Properties);
        }
        if (k == b'p' || k == b'P') && self.key_mods & aero::display::MOD_ALT != 0 {
            // Alt+P, as in Windows.
            return self.toggle_preview();
        }
        if k == 0x06 || k == 0x05 {
            // Ctrl+F or Ctrl+E: to the search box.
            self.files.query_focus = true;
            return area;
        }
        if self.files.confirm.is_some() {
            return match k {
                b'\n' | b'y' | b'Y' => self.command(Command::Yes),
                27 | b'n' | b'N' => self.command(Command::No),
                _ => Rect::EMPTY,
            };
        }
        let ctrl = self.key_mods & aero::display::MOD_CTRL != 0;
        match k {
            // Ctrl+T, Ctrl+W: a new tab, close this one.
            0x14 => return self.new_tab(),
            0x17 => return self.close_tab(self.tab),
            // Ctrl+Tab and Ctrl+Shift+Tab: the next or previous tab.
            9 if ctrl => {
                let n = self.tabs.len();
                let back = self.key_mods & aero::display::MOD_SHIFT != 0;
                return self.switch_tab(if back { (self.tab + n - 1) % n } else { (self.tab + 1) % n });
            }
            // Tab: the other side of two panes.
            9 => return self.switch_side(),
            _ => {}
        }
        let count = if self.files.path.is_empty() { self.tile_count() } else { self.files.entries.len() };
        let c = self.files_client();
        let page = if self.files.path.is_empty() { self.tile_columns(&c) } else { self.files_visible(&c) };
        let step = if self.files.path.is_empty() { self.tile_columns(&c) } else { self.files_across(&c) };
        let at = self.files.selected;
        let target = match k {
            8 => return self.go_up(),
            b'\n' => return self.open_selected(),
            KEY_DELETE => return self.command(Command::Delete),
            KEY_F2 => return self.command(Command::Rename),
            KEY_F5 => {
                self.refresh(self.selected_entry().map(|e| e.name));
                self.with_other(|d| {
                    let keep = d.selected_entry().map(|e| e.name);
                    d.refresh(keep);
                });
                return area;
            }
            0x03 => return self.command(Command::Copy),
            0x18 => return self.command(Command::Cut),
            0x16 => return self.command(Command::Paste),
            0x0E => return self.command(Command::NewFolder),
            0x1A => return self.undo_delete(),
            0x01 => {
                self.select_all();
                return area;
            }
            KEY_DOWN => at.map_or(0, |i| i + step),
            KEY_UP => at.map_or(0, |i| i.saturating_sub(step)),
            KEY_RIGHT if self.files.path.is_empty() || step > 1 => at.map_or(0, |i| i + 1),
            KEY_LEFT if self.files.path.is_empty() || step > 1 => at.map_or(0, |i| i.saturating_sub(1)),
            KEY_PGDN => at.map_or(0, |i| i + page),
            KEY_PGUP => at.map_or(0, |i| i.saturating_sub(page)),
            KEY_HOME => 0,
            KEY_END => count.saturating_sub(1),
            33..=126 if !self.files.path.is_empty() => {
                // Jump to the next entry starting with that letter.
                let ch = (k as char).to_ascii_lowercase();
                let from = at.map_or(0, |i| i + 1);
                let names = &self.files.entries;
                let found = (0..count).map(|o| (from + o) % count.max(1)).find(|&i| names[i].name.to_lowercase().starts_with(ch));
                match found {
                    Some(i) => i,
                    None => return Rect::EMPTY,
                }
            }
            _ => return Rect::EMPTY,
        };
        if count == 0 {
            return Rect::EMPTY;
        }
        // Shift with an arrow (or Home, End, Page Up, Page Down) selects
        // everything passed over.
        let moving = matches!(k, KEY_DOWN | KEY_UP | KEY_PGDN | KEY_PGUP | KEY_HOME | KEY_END);
        if moving && self.key_mods & aero::display::MOD_SHIFT != 0 && !self.files.path.is_empty() {
            self.extend(target.min(count - 1));
        } else {
            self.select(target.min(count - 1));
        }
        area
    }
}

/// A file's lines if it reads as text (no NUL bytes, and nearly all of
/// it printable), with tabs as spaces and anything else unusual as '.'.
fn text_lines(data: &[u8]) -> Option<Vec<String>> {
    let head = &data[..data.len().min(4096)];
    let odd = head.iter().filter(|&&b| b < 32 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0C)).count();
    if head.contains(&0) || odd * 20 > head.len().max(1) {
        return None;
    }
    let text = String::from_utf8_lossy(data);
    let mut lines = Vec::new();
    for line in text.split('\n').take(5000) {
        let shown: String = line
            .trim_end_matches('\r')
            .replace('\t', "    ")
            .chars()
            .take(400)
            .map(|ch| if (' '..='~').contains(&ch) { ch } else { '.' })
            .collect();
        lines.push(shown);
    }
    Some(lines)
}

/// Row `row` of a hex dump, `width` bytes to a row:
/// "0010  48 65 6C 6C 6F ...  Hello..." (the preview reads at most 64 KB,
/// so four digits number every byte).
fn hex_row(data: &[u8], row: usize, width: usize, chars: bool) -> String {
    let start = row * width;
    let bytes = &data[start.min(data.len())..(start + width).min(data.len())];
    let mut out = format!("{:04X} ", start);
    for i in 0..width {
        match bytes.get(i) {
            Some(b) => out.push_str(&format!(" {:02X}", b)),
            None => out.push_str("   "),
        }
    }
    if !chars {
        return out;
    }
    out.push_str("  ");
    out.extend(bytes.iter().map(|&b| if (32..127).contains(&b) { b as char } else { '.' }));
    out
}

/// A selected drive tile; grey on the side of two panes not in use.
fn selected_tile(c: &mut Canvas, t: Rect, s: i32, active: bool) {
    if active {
        c.rounded(t, 3 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
        c.rounded_outline(t, 3 * s, false, rgb(125, 162, 206), 255);
    } else {
        c.rounded(t, 3 * s, false, rgb(236, 238, 241), rgb(222, 225, 230), 255);
        c.rounded_outline(t, 3 * s, false, rgb(175, 180, 190), 255);
    }
}

/// The parts of the screen to redraw this time round: rectangles that touch
/// are joined, ones far apart are drawn separately.
#[derive(Default)]
struct Damage {
    rects: Vec<Rect>,
}

impl Damage {
    fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        let mut r = r;
        // Joining can make a rectangle reach others, so repeat until none do.
        while let Some(i) = self.rects.iter().position(|o| !o.intersect(&Rect::new(r.x - 8, r.y - 8, r.w + 16, r.h + 16)).is_empty()) {
            r = r.union(&self.rects.swap_remove(i));
        }
        self.rects.push(r);
    }
}

fn present(screen: &Screen, canvas: &mut Canvas, desk: &Desktop, area: Rect) {
    let area = draw(canvas, desk, area);
    show(screen, canvas, area);
}

/// Redraws `area` of the frame; returns the part that is on screen.
fn draw(canvas: &mut Canvas, desk: &Desktop, area: Rect) -> Rect {
    let area = area.intersect(&Rect::new(0, 0, canvas.w, canvas.h));
    if !area.is_empty() {
        canvas.clip = area;
        desk.draw(canvas);
    }
    area
}

/// Puts `area` of the frame on the screen.
fn show(screen: &Screen, canvas: &Canvas, area: Rect) {
    if !area.is_empty() {
        let _ = screen.present(&canvas.px, canvas.w as usize, area.x as usize, area.y as usize, area.w as usize, area.h as usize);
    }
}

/// Where the desktop keeps its settings, on the first disk.
const SETTINGS: &str = "/AeroForge.ini";

/// The pointer speed saved in the settings file, applied; or the current one.
fn saved_mouse_speed() -> u64 {
    let mut buf = [0u8; 4096];
    if let Ok(n) = aero::read_file(SETTINGS, &mut buf) {
        let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "mouse speed" {
                    if let Ok(speed @ 1..=10) = value.trim().parse::<u64>() {
                        let _ = aero::mouse_speed(Some(speed));
                    }
                }
            }
        }
    }
    aero::mouse_speed(None).unwrap_or(5)
}

/// The network drives saved in the settings file, put back in the Computer
/// window. No password is saved, so each one asks for it when first opened.
fn saved_network_drives() {
    let mut buf = [0u8; 4096];
    let Ok(n) = aero::read_file(SETTINGS, &mut buf) else { return };
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        if key.trim() != "network drive" {
            continue;
        }
        let mut parts = value.trim().split('|');
        let (Some(unc), Some(user), Some(letter)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let mut names = unc.trim_start_matches('\\').split('\\');
        let (Some(host), Some(share)) = (names.next(), names.next()) else { continue };
        let Some(letter) = letter.trim().chars().next().filter(|c| c.is_ascii_uppercase()) else { continue };
        if host.is_empty() || share.is_empty() || shares().iter().any(|s| s.letter == letter) {
            continue;
        }
        shares().push(NetShare {
            host: String::from(host),
            share: String::from(share),
            user: String::from(user),
            password: None,
            client: None,
            letter,
        });
    }
}

/// 1234567 as "1,234,567".
fn group(n: u64) -> String {
    let digits = format!("{}", n);
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// How far round a circle (0 to 1, clockwise from the top) the point
/// (dx, dy) from its middle is, for drawing a pie.
fn clockwise_turn(dx: f32, dy: f32) -> f32 {
    if dx == 0.0 && dy == 0.0 {
        return 0.0;
    }
    // atan2 without the standard library: an octant, then a polynomial
    // good to about a tenth of a degree.
    let (ax, ay) = (if dx < 0.0 { -dx } else { dx }, if dy < 0.0 { -dy } else { dy });
    let (a, b) = if ax > ay { (ay, ax) } else { (ax, ay) };
    let t = a / b;
    let t2 = t * t;
    let mut angle = ((-0.0464964749 * t2 + 0.15931422) * t2 - 0.327622764) * t2 * t + t;
    if ay > ax {
        angle = core::f32::consts::FRAC_PI_2 - angle;
    }
    // angle is from the x axis towards y, in the first quadrant; turn it
    // into "clockwise from up" with y pointing down the screen.
    let full = match (dx >= 0.0, dy >= 0.0) {
        (true, false) => core::f32::consts::FRAC_PI_2 - angle,
        (true, true) => core::f32::consts::FRAC_PI_2 + angle,
        (false, true) => 3.0 * core::f32::consts::FRAC_PI_2 - angle,
        (false, false) => 3.0 * core::f32::consts::FRAC_PI_2 + angle,
    };
    full / (2.0 * core::f32::consts::PI)
}

/// What a folder holds: bytes, files and folders, all the way down (up to
/// SEARCH_LOOKS names).
fn tree_size(path: &str) -> (u64, usize, usize) {
    let (mut bytes, mut files, mut dirs) = (0u64, 0usize, 0usize);
    let mut stack = alloc::vec![String::from(path)];
    while let Some(dir) = stack.pop() {
        for e in fs_list(&dir).unwrap_or_default() {
            if e.is_dir {
                dirs += 1;
                stack.push(join(&dir, &e.name));
            } else {
                files += 1;
                bytes += e.size;
            }
        }
        if files + dirs >= SEARCH_LOOKS {
            break;
        }
    }
    (bytes, files, dirs)
}

/// Whether `name` matches a search: a pattern with * and ? matches whole
/// names; anything else matches names containing it. Both lowercase.
fn name_matches(name: &str, pattern: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return name.contains(pattern);
    }
    fn glob(n: &[char], p: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| glob(&n[i..], rest)),
            Some(('?', rest)) => !n.is_empty() && glob(&n[1..], rest),
            Some((c, rest)) => n.first() == Some(c) && glob(&n[1..], rest),
        }
    }
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    glob(&n, &p)
}

/// A file size the way Explorer shows it: whole kilobytes, rounded up.
fn size_text(bytes: u64) -> String {
    if bytes >= 10 * 1024 * 1024 * 1024 {
        format!("{} GB", bytes.div_ceil(1024 * 1024 * 1024))
    } else if bytes >= 10 * 1024 * 1024 {
        format!("{} MB", bytes.div_ceil(1024 * 1024))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn uptime(us: u64) -> String {
    let s = us / 1_000_000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// The taskbar clock: "3:07 PM" over "10/7/2026", or the time since the
/// desktop started if the PC's clock can't be read.
fn clock(started_us: u64, hours24: bool) -> (String, String) {
    match aero::now() {
        Some(t) => (time_text(t.hour, t.minute, hours24), format!("{}/{}/{}", t.month, t.day, t.year)),
        None => (uptime(aero::clock_us() - started_us), String::from("uptime")),
    }
}

/// Windows in the order they are stacked at start, back to front.
const KINDS_BY_LAYER: [Kind; 9] =
    [Kind::Welcome, Kind::System, Kind::Notes, Kind::Computer, Kind::Calculator, Kind::Paint, Kind::Photos, Kind::Music, Kind::ControlPanel];

/// A window's title, where it opens on a `w` x `h` screen at scale `s`, and
/// whether it is open at start.
fn window_place(kind: Kind, w: i32, h: i32, s: i32) -> (&'static str, Rect, bool) {
    // Windows that are mostly text grow and shrink with the text size...
    let f = |v: i32| v * Font::Normal.h(1) / 24;
    // ...but stay on the screen, above the taskbar.
    let fit = |r: Rect| {
        let (rw, rh) = (r.w.min(w - 16 * s), r.h.min(h - taskbar_h(s) - 16 * s));
        Rect::new(r.x.min(w - 8 * s - rw), r.y.min(h - taskbar_h(s) - 8 * s - rh), rw, rh)
    };
    match kind {
        Kind::Welcome => ("Welcome", fit(Rect::new(w / 12, h / 7, f(460) * s, f(290) * s)), true),
        Kind::System => ("System", fit(Rect::new(w / 12 + f(480) * s, h / 7 + 30 * s, f(300) * s, f(190) * s)), true),
        Kind::Notes => ("Notes", fit(Rect::new(w / 12 + 160 * s, h / 7 + f(200) * s, f(500) * s, f(290) * s)), true),
        // A file manager wants room: most of the screen, right of the icons.
        Kind::Computer => {
            let (cw, ch) = ((w - 150 * s).min(1040 * s).max(800 * s), (h - 180 * s).min(620 * s).max(480 * s));
            ("Computer", Rect::new((w - cw - 20 * s).max(130 * s), 30 * s, cw, ch), false)
        }
        Kind::Calculator => ("Calculator", Rect::new(w / 2 - 40 * s, h / 7 + 10 * s, 260 * s, 350 * s), false),
        Kind::Paint => {
            let (pw, ph) = ((w - 80 * s).min(f(820) * s), (h - 110 * s).min(580 * s));
            ("Paint", Rect::new((w - pw) / 2, 24 * s, pw, ph), false)
        }
        Kind::Photos => {
            let (pw, ph) = ((w - 100 * s).min(900 * s), (h - 120 * s).min(660 * s));
            ("Photos", Rect::new((w - pw) / 2, 20 * s, pw, ph), false)
        }
        Kind::Music => {
            let (mw, mh) = ((w - 100 * s).min(f(720) * s), (h - 120 * s).min(f(420) * s));
            ("Music", Rect::new((w - mw) / 2, (h - mh) / 2 - 20 * s, mw, mh), false)
        }
        Kind::ControlPanel => {
            let (cw, ch) = ((w - 16 * s).min(f(680) * s), (h - 120 * s).min(f(500) * s));
            ("Control Panel", Rect::new((w - cw) / 2, 40 * s, cw, ch), false)
        }
    }
}

/// The scale things are drawn at: 2 on big screens unless set otherwise
/// (Large needs a screen of at least 1600 x 1000).
/// Each program's icon (see tools/make-icons.py).
fn kind_icon(kind: Kind) -> &'static str {
    match kind {
        Kind::Computer => "computer",
        Kind::Notes => "notes",
        Kind::Calculator => "calculator",
        Kind::Photos => "photos",
        Kind::Music => "music",
        Kind::ControlPanel => "controlpanel",
        Kind::Paint => "paint",
        Kind::System => "system",
        Kind::Welcome => "welcome",
    }
}

/// How much taller than medium the text is at scale `s`: room that rows
/// of text set out at medium size need to add.
fn grow(s: i32) -> i32 {
    (Font::Normal.h(s) - 24 * s).max(0)
}

/// The taskbar's height: room for a line of text, or the clock's two.
fn taskbar_h(s: i32) -> i32 {
    (TASKBAR * s).max(Font::Normal.h(s) + 12 * s).max(2 * Font::Tiny.h(s) + 4 * s)
}

/// How the ini file and the log name text size `level`.
fn font_name(level: i32) -> &'static str {
    match level {
        -1 => "smaller",
        1 => "larger",
        2 => "extra large",
        3 => "largest",
        _ => "medium",
    }
}

fn ui_for(w: i32, h: i32, text_size: i32) -> i32 {
    match text_size {
        1 => 1,
        2 if w >= 1600 && h >= 1000 => 2,
        _ if w >= 2560 => 2,
        _ => 1,
    }
}

/// The wallpaper drawn into `r` (the screen, or a small preview of it): a
/// sky in background `which`'s colours with a glow low on the right and
/// soft light bands sweeping across it. Set up once, then asked pixel by pixel.
struct Wallpaper {
    r: Rect,
    top: u32,
    bottom: u32,
    light: u32,
    glow: (i32, i32, i32, i32),
    /// The bands, worked out as on a 1280 x 800 screen: x and y scaled by
    /// these (out of 65536), then (offset, width, strength) for each.
    scale: (i32, i32),
    bands: [(i32, i32, i32); 2],
}

impl Wallpaper {
    fn new(which: usize, r: &Rect) -> Wallpaper {
        let (_, top, bottom, light) = BACKGROUNDS[which.min(BACKGROUNDS.len() - 1)];
        let (w, h) = (r.w.max(1), r.h.max(1));
        let reach = (w * w + h * h) / 5;
        Wallpaper {
            r: *r,
            top,
            bottom,
            light,
            glow: (w * 2 / 3, h * 3 / 4, reach, (reach / 110).max(1)),
            scale: (1280 * 65536 / 3 / w, 800 * 65536 / h),
            bands: [(800 * 2 / 3, 800 / 6, 60), (800, 800 / 12, 50)],
        }
    }

    fn px(&self, x: i32, y: i32) -> u32 {
        let (x, y) = (x - self.r.x, y - self.r.y);
        let base = lerp(self.top, self.bottom, y, self.r.h.max(1));
        let (gx, gy, reach, step) = self.glow;
        let (dx, dy) = (x - gx, y - gy);
        let d = dx * dx + dy * dy;
        let mut light = if d < reach { (reach - d) / step } else { 0 };
        let at = ((x as i64 * self.scale.0 as i64 + y as i64 * self.scale.1 as i64) >> 16) as i32;
        for (offset, width, strength) in self.bands {
            let d = (at - offset).abs();
            if d < width {
                light += (width - d) * strength / width;
            }
        }
        blend(base, self.light, light.min(200) as u32)
    }
}

/// "3:07 PM", or "15:07" on a 24-hour clock.
fn time_text(hour: u8, minute: u8, hours24: bool) -> String {
    if hours24 {
        return format!("{:02}:{:02}", hour, minute);
    }
    let h12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    format!("{}:{:02} {}", h12, minute, if hour < 12 { "AM" } else { "PM" })
}

fn cp_title(page: CpPage) -> &'static str {
    match page {
        CpPage::Home => "Control Panel",
        CpPage::Display => "Display",
        CpPage::Sound => "Sound",
        CpPage::Mouse => "Mouse",
        CpPage::Clock => "Date and Time",
    }
}

/// A Control Panel category's picture, 64 x 64 at scale 1, from (x, y).
fn cp_icon(c: &mut Canvas, page: CpPage, x: i32, y: i32, s: i32) {
    match page {
        CpPage::Display => {
            // A screen showing the sky, on a stand.
            c.rounded(Rect::new(x + 2 * s, y + 4 * s, 60 * s, 42 * s), 3 * s, false, rgb(70, 80, 95), rgb(20, 25, 35), 255);
            c.gradient(Rect::new(x + 6 * s, y + 8 * s, 52 * s, 34 * s), rgb(60, 150, 230), rgb(10, 50, 120), 255);
            c.orb(x + 42 * s, y + 30 * s, 7 * s, rgb(200, 240, 255), rgb(90, 170, 240));
            c.fill(Rect::new(x + 28 * s, y + 46 * s, 8 * s, 8 * s), rgb(60, 65, 75));
            c.rounded(Rect::new(x + 16 * s, y + 54 * s, 32 * s, 6 * s), 2 * s, false, rgb(110, 115, 125), rgb(50, 55, 65), 255);
        }
        CpPage::Sound => {
            // A speaker and sound waves.
            c.fill(Rect::new(x + 6 * s, y + 22 * s, 12 * s, 20 * s), rgb(70, 80, 95));
            for k in 0..14 {
                c.fill(Rect::new(x + 18 * s + k * s, y + (22 - k) * s, s, (20 + 2 * k) * s), rgb(90 + 4 * k as u32, 100 + 4 * k as u32, 115 + 4 * k as u32));
            }
            for (k, r) in [10, 18, 26].iter().enumerate() {
                let r = *r * s;
                let (cx, cy) = (x + 32 * s, y + 32 * s);
                let col = rgb(40, 120 + 30 * k as u32, 210);
                let steps = 12;
                for i in 0..steps {
                    let a0 = -0.8 + 1.6 * i as f32 / steps as f32;
                    let a1 = -0.8 + 1.6 * (i + 1) as f32 / steps as f32;
                    let p = |a: f32| (cx + (r as f32 * cos_approx(a)) as i32, cy + (r as f32 * sin_approx(a)) as i32);
                    let (p0, p1) = (p(a0), p(a1));
                    c.line(p0.0, p0.1, p1.0, p1.1, 3 * s, col);
                }
            }
        }
        CpPage::Mouse => {
            // A mouse, its left button lit.
            c.rounded(Rect::new(x + 16 * s, y + 4 * s, 32 * s, 54 * s), 14 * s, false, rgb(245, 247, 250), rgb(170, 180, 195), 255);
            c.rounded_outline(Rect::new(x + 16 * s, y + 4 * s, 32 * s, 54 * s), 14 * s, false, rgb(70, 80, 95), 255);
            c.rounded(Rect::new(x + 18 * s, y + 6 * s, 13 * s, 20 * s), 6 * s, true, rgb(140, 210, 255), rgb(40, 130, 230), 255);
            c.fill(Rect::new(x + 32 * s, y + 6 * s, s, 22 * s), rgb(70, 80, 95));
            c.fill(Rect::new(x + 18 * s, y + 27 * s, 28 * s, s), rgb(150, 160, 175));
        }
        _ => {
            // A clock face.
            let (cx, cy) = (x + 32 * s, y + 32 * s);
            c.orb(cx, cy, 28 * s, rgb(90, 160, 240), rgb(20, 70, 150));
            c.orb(cx, cy, 23 * s, 0xFFFFFF, rgb(225, 232, 242));
            for k in 0..12 {
                let a = k as f32 * core::f32::consts::PI / 6.0;
                let (dx, dy) = (sin_approx(a), -cos_approx(a));
                c.line(cx + (dx * 18.0 * s as f32) as i32, cy + (dy * 18.0 * s as f32) as i32,
                       cx + (dx * 21.0 * s as f32) as i32, cy + (dy * 21.0 * s as f32) as i32, s, rgb(60, 70, 90));
            }
            c.line(cx, cy, cx + 10 * s, cy - 6 * s, 3 * s, rgb(30, 40, 60));
            c.line(cx, cy, cx - 2 * s, cy - 16 * s, 2 * s, rgb(30, 40, 60));
            c.orb(cx, cy, 3 * s, rgb(200, 60, 40), rgb(140, 30, 20));
        }
    }
}

/// Sine and cosine to a few thousandths, without the standard library.
fn sin_approx(a: f32) -> f32 {
    let pi = core::f32::consts::PI;
    let mut a = a % (2.0 * pi);
    if a > pi {
        a -= 2.0 * pi;
    } else if a < -pi {
        a += 2.0 * pi;
    }
    // Bhaskara-style parabola, then a correction.
    let y = 4.0 / pi * a - 4.0 / (pi * pi) * a * if a < 0.0 { -a } else { a };
    0.225 * (y * if y < 0.0 { -y } else { y } - y) + y
}
fn cos_approx(a: f32) -> f32 {
    sin_approx(a + core::f32::consts::FRAC_PI_2)
}

fn days_in(year: u16, month: u8) -> u8 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// The day of the week, by Sakamoto's method.
fn weekday(year: u16, month: u8, day: u8) -> &'static str {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = year as i32 - if month < 3 { 1 } else { 0 };
    let d = (y + y / 4 - y / 100 + y / 400 + T[(month.clamp(1, 12) - 1) as usize] + day as i32) % 7;
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"][d as usize]
}

/// The Control Panel's settings from /AeroForge.ini, put into effect
/// (volume, double-click time and buttons are the system's).
fn saved_prefs() -> Prefs {
    let mut p = Prefs::default();
    let mut buf = [0u8; 4096];
    if let Ok(n) = aero::read_file(SETTINGS, &mut buf) {
        let text = core::str::from_utf8(&buf[..n]).unwrap_or("");
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            let yes = value == "yes";
            match key.trim() {
                "double-click" => p.double_ms = value.parse().unwrap_or(500u64).clamp(200, 900),
                "swap buttons" => p.swapped = yes,
                "volume" => p.volume = value.parse().unwrap_or(100u32).min(100),
                "muted" => p.muted = yes,
                "background" => p.background = BACKGROUNDS.iter().position(|b| b.0.eq_ignore_ascii_case(value)).unwrap_or(0),
                "text size" => p.text_size = match value { "normal" => 1, "large" => 2, _ => 0 },
                "font size" => p.font = match value { "smaller" => -1, "larger" => 1, "extra large" => 2, "largest" => 3, _ => 0 },
                "clock" => p.hours24 = value == "24-hour",
                _ => {}
            }
        }
    }
    let _ = aero::mouse_buttons(Some((p.double_ms, p.swapped)));
    let _ = aero::audio::volume(Some((p.volume, p.muted)));
    p
}

fn main() -> i64 {
    let screen = match display::acquire() {
        Ok(s) => s,
        Err(e) => {
            println!("[desktop] could not take the screen ({})", e);
            return 1;
        }
    };
    let (w, h) = (screen.width as i32, screen.height as i32);
    let prefs = saved_prefs();
    TEXT_LEVEL.store(prefs.font.clamp(-1, 3), Ordering::Relaxed);
    let ui = ui_for(w, h, prefs.text_size);
    let windows = KINDS_BY_LAYER.iter().map(|&kind| {
        let (title, rect, open) = window_place(kind, w, h, ui);
        Window { kind, title, rect, open, minimized: false, restore: None }
    }).collect();
    let pointer = screen.pointer().unwrap_or_default();
    let started_us = aero::clock_us();
    let mut desk = Desktop {
        w,
        h,
        ui,
        windows,
        notes: String::new(),
        notes_path: None,
        notes_status: String::new(),
        notes_cut: false,
        pointer,
        drag: None,
        resize: None,
        snap: None,
        menu: false,
        all_programs: false,
        peeked: Vec::new(),
        started_us,
        clock: clock(started_us, prefs.hours24),
        quit: false,
        files: Files::new(),
        other: None,
        right: false,
        tabs: alloc::vec![Tab::default()],
        preview_on: false,
        icons: false,
        preview: None,
        tab: 0,
        clip: None,
        calc: Calc { display: String::from("0"), ..Default::default() },
        icon: None,
        mouse_speed: { saved_network_drives(); saved_mouse_speed() },
        file_drag: None,
        key_mods: 0,
        pad_us: 0,
        pad_rest: (0, 0),
        pad_held: 0,
        pad_mouse: 0,
        pad_armed: 0,
        pad_moving: false,
        tree_open: alloc::vec![String::new()],
        tree_kids: Vec::new(),
        popup: None,
        photo: None,
        player: None,
        paint: None,
        progress: None,
        cp_page: CpPage::Home,
        prefs,
        cp_time: None,
        cp_folder_open: false,
        test_sound: None,
    };
    let canvas = Canvas { px: alloc::vec![0u32; (w * h) as usize], w, h, clip: Rect::EMPTY, ui };
    // SAFETY: set once, before anything reads it.
    unsafe { *GFX.0.get() = Some(Gfx { screen, canvas }) };
    {
        let g = gfx();
        present(&g.screen, &mut g.canvas, &desk, Rect::new(0, 0, w, h));
    }
    let notes = desk.windows[desk.index(Kind::Notes)].rect;
    let s = ui;
    let icon = desk.icon_rect(0);
    let calc = desk.icon_rect(2);
    println!("[desktop] up at {}x{}; Notes title bar at {},{}; Computer icon at {},{}; Calculator icon at {},{}", w, h,
        notes.x + 60 * s, notes.y + 12 * s, icon.x + icon.w / 2, icon.y + icon.h / 2, calc.x + calc.w / 2, calc.y + calc.h / 2);

    let mut keys = [(0u8, 0u8); 64];
    let mut tick = 0u64;
    // How long redraws take after the pointer moves, for the log.
    let (mut moves, mut move_us, mut worst_us) = (0u64, 0u64, 0u64);
    while !desk.quit {
        let mut dirty = Rect::EMPTY;
        // The pointer and what it lights up are small; they are redrawn on
        // their own instead of joined into one big rectangle with the rest.
        let mut spots = Damage::default();
        let p = gfx().screen.pointer().unwrap_or(desk.pointer);
        if p != desk.pointer {
            let old = desk.pointer;
            spots.add(desk.cursor_rect());
            spots.add(desk.hover_area(old.x as i32, old.y as i32));
            desk.pointer = p;
            spots.add(desk.cursor_rect());
            spots.add(desk.hover_area(p.x as i32, p.y as i32));
            // Both presses of a quick double-click can land between two looks.
            // The double-clicks among them are the last ones.
            let presses = (p.presses.wrapping_sub(old.presses) & 0xF_FFFF).min(3);
            let doubles = (p.doubles.wrapping_sub(old.doubles) & 0x7) as u32;
            for k in 0..presses {
                dirty = dirty.union(&desk.click(p.x as i32, p.y as i32, k + doubles.min(presses) >= presses));
            }
            if p.buttons & 2 != 0 && old.buttons & 2 == 0 {
                dirty = dirty.union(&desk.right_click(p.x as i32, p.y as i32));
            }
            if let Some((i, mut gx, gy)) = desk.drag {
                let (px, py) = (p.x as i32, p.y as i32);
                if p.buttons & 1 != 0 && (px, py) != (old.x as i32, old.y as i32) {
                    let before = desk.window_area(i);
                    // A maximized or snapped window gets its old size back
                    // once it is dragged, keeping the same part of the title
                    // bar under the pointer.
                    let win = &mut desk.windows[i];
                    if let Some(old) = win.restore.take() {
                        gx = gx * old.w / win.rect.w.max(1);
                        win.rect.w = old.w;
                        win.rect.h = old.h;
                        desk.drag = Some((i, gx, gy));
                    }
                    let r = &mut desk.windows[i].rect;
                    r.x = (px - gx).clamp(-r.w + 60, w - 60);
                    r.y = (py - gy).clamp(0, h - 80);
                    let snap = desk.snap_target(px, py);
                    for area in [desk.snap, snap].into_iter().flatten() {
                        dirty = dirty.union(&area);
                    }
                    desk.snap = snap;
                    dirty = dirty.union(&before).union(&desk.window_area(i));
                } else if p.buttons & 1 == 0 {
                    let win = &mut desk.windows[i];
                    match desk.snap.take() {
                        Some(target) => {
                            dirty = dirty.union(&target);
                            win.restore = Some(win.rect);
                            win.rect = target;
                            println!("[desktop] snapped {} to {},{} {}x{}", win.title, target.x, target.y, target.w, target.h);
                        }
                        None => println!("[desktop] moved {} to {},{} ({}x{})", win.title, win.rect.x, win.rect.y, win.rect.w, win.rect.h),
                    }
                    desk.drag = None;
                    dirty = dirty.union(&desk.window_area(i));
                }
            }
            if let Some((i, edges, start, x0, y0)) = desk.resize {
                if p.buttons & 1 != 0 {
                    let before = desk.window_area(i);
                    let (dx, dy) = (p.x as i32 - x0, p.y as i32 - y0);
                    let (min_w, min_h) = (240 * s, 150 * s);
                    let r = &mut desk.windows[i].rect;
                    if edges.right {
                        r.w = (start.w + dx).max(min_w);
                    }
                    if edges.left {
                        r.w = (start.w - dx).max(min_w);
                        r.x = start.x + start.w - r.w;
                    }
                    if edges.bottom {
                        r.h = (start.h + dy).max(min_h);
                    }
                    if edges.top {
                        // The title bar may not go above the screen.
                        r.h = (start.h - dy).max(min_h).min(start.y + start.h);
                        r.y = start.y + start.h - r.h;
                    }
                    dirty = dirty.union(&before).union(&desk.window_area(i));
                } else {
                    let r = desk.windows[i].rect;
                    println!("[desktop] resized {} to {}x{}", desk.windows[i].title, r.w, r.h);
                    desk.resize = None;
                    dirty = dirty.union(&desk.window_area(i));
                }
            }
            if desk.paint.as_ref().is_some_and(|pt| pt.stroke.is_some()) {
                dirty = dirty.union(&desk.paint_step(p));
            }
            if desk.file_drag.is_some() {
                dirty = dirty.union(&desk.file_drag_step(p));
                dirty = dirty.union(&desk.sync_preview());
            }
            if desk.top_kind() == Some(Kind::System) {
                dirty = dirty.union(&desk.window_area(desk.windows.len() - 1));
            }
        }
        if let Ok(n) = gfx().screen.keys_with_modifiers(&mut keys) {
            dirty = dirty.union(&desk.typed(&keys[..n]));
        }
        dirty = dirty.union(&desk.gamepad(aero::clock_us()));
        dirty = dirty.union(&desk.pump_music());
        // Twice a second: the clock, the System window and the Notes caret.
        let now = aero::clock_us();
        if now / 500_000 != tick {
            tick = now / 500_000;
            let time = clock(desk.started_us, desk.prefs.hours24);
            if time != desk.clock {
                desk.clock = time;
                dirty = dirty.union(&desk.tray());
                if desk.menu {
                    dirty = dirty.union(&desk.menu_rect());
                }
            }
            // Only their insides, and the caret only blinks in the window in
            // front: redrawing whole windows here, glass and all, kept the
            // desktop busy for a large part of every second.
            if desk.cp_page == CpPage::Clock && desk.windows[desk.index(Kind::ControlPanel)].shown() {
                let at = desk.index(Kind::ControlPanel);
                dirty = dirty.union(&desk.client(&desk.windows[at].rect));
            }
            for kind in [Kind::System, Kind::Notes] {
                let at = desk.index(kind);
                if desk.windows[at].shown() && (kind == Kind::System || desk.top_kind() == Some(kind)) {
                    dirty = dirty.union(&desk.client(&desk.windows[at].rect));
                }
            }
        }
        let moved = !spots.rects.is_empty() && dirty.is_empty();
        spots.add(dirty);
        let t0 = aero::clock_us();
        // Draw each piece, then hand the screen one rectangle covering them
        // all: one present per frame, however many pieces changed.
        let mut shown = Rect::EMPTY;
        let g = gfx();
        for area in spots.rects {
            shown = shown.union(&draw(&mut g.canvas, &desk, area));
        }
        show(&g.screen, &g.canvas, shown);
        if moved && desk.drag.is_none() && desk.resize.is_none() {
            let took = aero::clock_us() - t0;
            moves += 1;
            move_us += took;
            worst_us = worst_us.max(took);
        }
        aero::sleep_ms(10);
    }
    if moves > 0 {
        println!("[desktop] pointer moves: {}, redraw took {} us on average, {} us at most", moves, move_us / moves, worst_us);
    }
    println!("[desktop] pointer ended at {},{}", desk.pointer.x, desk.pointer.y);
    println!("[desktop] notes: {}", desk.notes.replace('\n', " / "));
    println!("[desktop] closed, screen given back to the console");
    // SAFETY: nothing holds a reference to it any more.
    drop(unsafe { (*GFX.0.get()).take() });
    0
}
