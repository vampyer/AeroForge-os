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
use alloc::string::String;
use alloc::vec::Vec;
use aero::display::{self, Pointer, Screen};
use aero::println;
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
}

impl Font {
    fn style(self, ui: i32) -> (FontWeight, RasterHeight) {
        let weight = if self == Font::Bold { FontWeight::Bold } else { FontWeight::Regular };
        let size = match (self, ui >= 2) {
            (Font::Small, false) => RasterHeight::Size20,
            (Font::Small, true) => RasterHeight::Size24,
            (Font::Large, _) | (_, true) => RasterHeight::Size32,
            _ => RasterHeight::Size24,
        };
        (weight, size)
    }
    /// The width of one character (the font is monospaced).
    fn w(self, ui: i32) -> i32 {
        let (weight, size) = self.style(ui);
        get_raster_width(weight, size) as i32
    }
    fn h(self, ui: i32) -> i32 {
        self.style(ui).1.val() as i32
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
struct Canvas {
    px: Vec<u32>,
    w: i32,
    h: i32,
    clip: Rect,
    ui: i32,
}

impl Canvas {
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
    fn text(&mut self, x: i32, y: i32, s: &str, color: u32, font: Font) {
        let (weight, size) = font.style(self.ui);
        let (cw, ch) = (font.w(self.ui), font.h(self.ui));
        for (i, c) in s.chars().enumerate() {
            let gx = x + i as i32 * cw;
            let box_ = Rect::new(gx, y, cw, ch).intersect(&self.clip);
            if box_.is_empty() || c == ' ' {
                continue;
            }
            let Some(glyph) = get_raster(c, weight, size).or_else(|| get_raster('?', weight, size)) else { continue };
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
        self.fill(Rect::new(x, y, 7 * s, 3 * s), rgb(220, 170, 50));
        self.gradient(Rect::new(x, y + 2 * s, 16 * s, 10 * s), rgb(255, 225, 120), rgb(230, 175, 50), 255);
        self.frame(Rect::new(x, y + 2 * s, 16 * s, 10 * s), rgb(180, 130, 30));
    }

    /// A small sheet of paper with a folded corner, 12 x 14 at scale 1.
    fn page(&mut self, x: i32, y: i32, s: i32) {
        self.fill(Rect::new(x, y, 12 * s, 14 * s), 0xFFFFFF);
        self.frame(Rect::new(x, y, 12 * s, 14 * s), rgb(120, 130, 150));
        self.fill(Rect::new(x + 8 * s, y, 4 * s, 4 * s), rgb(210, 220, 235));
        for i in 0..3 {
            self.fill(Rect::new(x + 2 * s, y + (6 + 2 * i) * s, 8 * s, s), rgb(150, 170, 200));
        }
    }

    /// The Computer icon: a flat screen on a stand, 40 x 36 at scale 1.
    fn computer(&mut self, x: i32, y: i32, s: i32) {
        let body = Rect::new(x, y, 40 * s, 28 * s);
        self.rounded(body, 2 * s, false, rgb(70, 75, 85), rgb(20, 22, 28), 255);
        let glass = Rect::new(x + 3 * s, y + 3 * s, 34 * s, 21 * s);
        self.gradient(glass, rgb(120, 200, 250), rgb(20, 80, 170), 255);
        self.shade(Rect::new(glass.x, glass.y, glass.w, glass.h / 2), 0xFFFFFF, 50);
        self.fill(Rect::new(x + 17 * s, y + 28 * s, 6 * s, 4 * s), rgb(60, 60, 70));
        self.rounded(Rect::new(x + 10 * s, y + 32 * s, 20 * s, 4 * s), 2 * s, false, rgb(110, 110, 120), rgb(40, 40, 50), 255);
    }

    /// The Calculator icon: a grey body, a green display and keys, 24 x 36.
    fn calculator(&mut self, x: i32, y: i32, s: i32) {
        let body = Rect::new(x, y, 24 * s, 36 * s);
        self.rounded(body, 3 * s, false, rgb(120, 130, 145), rgb(50, 55, 65), 255);
        self.fill(Rect::new(x + 3 * s, y + 3 * s, 18 * s, 8 * s), rgb(190, 225, 190));
        for row in 0..4 {
            for col in 0..3 {
                let color = if row == 3 && col == 2 { rgb(240, 160, 60) } else { rgb(235, 238, 242) };
                self.fill(Rect::new(x + (3 + col * 6) * s, y + (14 + row * 5) * s, 4 * s, 3 * s), color);
            }
        }
    }

    /// The Notes icon: a spiral notepad, 28 x 36 at scale 1.
    fn notepad(&mut self, x: i32, y: i32, s: i32) {
        let pad = Rect::new(x, y + 3 * s, 28 * s, 33 * s);
        self.fill(pad, 0xFFFFFF);
        self.frame(pad, rgb(110, 120, 140));
        self.fill(Rect::new(x, y + 3 * s, 28 * s, 6 * s), rgb(80, 140, 210));
        for i in 0..5 {
            self.fill(Rect::new(x + 3 * s + i * 5 * s, y, 2 * s, 7 * s), rgb(90, 90, 100));
        }
        for i in 0..6 {
            self.fill(Rect::new(x + 4 * s, y + (13 + 4 * i) * s, 20 * s, s), rgb(150, 180, 220));
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
}

const KINDS: [Kind; 5] = [Kind::Computer, Kind::Notes, Kind::Calculator, Kind::System, Kind::Welcome];

/// Desktop icons, top to bottom.
const ICONS: [Kind; 4] = [Kind::Computer, Kind::Notes, Kind::Calculator, Kind::System];

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

/// Two presses closer together than this are a double-click.
const DOUBLE_CLICK_US: u64 = 500_000;

/// How the Computer window sorts a folder (folders always come first).
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Name,
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
    /// The answers to "Delete ...?".
    Yes,
    No,
}

const COMMANDS: [(Command, &str); 6] = [
    (Command::NewFolder, "New folder"),
    (Command::Cut, "Cut"),
    (Command::Copy, "Copy"),
    (Command::Paste, "Paste"),
    (Command::Rename, "Rename"),
    (Command::Delete, "Delete"),
];

const COMPUTER_COMMANDS: [(Command, &str); 2] = [(Command::Map, "Map network drive"), (Command::Disconnect, "Disconnect")];

const BIN_COMMANDS: [(Command, &str); 3] =
    [(Command::Restore, "Restore"), (Command::Delete, "Delete"), (Command::Empty, "Empty Recycle Bin")];

/// The Computer window's path for the Recycle Bin.
const BIN: &str = "::bin";
/// Where each drive keeps what was deleted from it, and the list of it.
const BIN_DIR: &str = "Recycle Bin";
const BIN_INFO: &str = "info.txt";

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

/// What Copy or Cut picked up.
#[derive(Clone)]
struct Clip {
    path: String,
    is_dir: bool,
    size: u64,
    /// Cut: Paste moves it instead of copying it.
    cut: bool,
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
}

/// What the Computer window (the file manager) shows and is doing.
struct Files {
    /// The folder shown, or "" for Computer itself: the drives.
    path: String,
    entries: Vec<aero::DirEntry>,
    drives: Vec<aero::Volume>,
    /// The first entry shown.
    scroll: usize,
    /// The selected entry, or drive in Computer.
    selected: Option<usize>,
    status: String,
    /// Where Back and Forward go.
    back: Vec<String>,
    forward: Vec<String>,
    sort: SortBy,
    descending: bool,
    clip: Option<Clip>,
    /// The new name being typed for the selected entry.
    rename: Option<String>,
    /// The whole name is still selected: the first key typed replaces it.
    rename_all: bool,
    /// Asking about Delete or Empty (the Recycle Bin), with Yes and No.
    confirm: Option<Command>,
    /// The Recycle Bin, when shown: one per entry.
    bin: Vec<Recycled>,
    /// The last thing deleted, for Ctrl+Z.
    last_recycled: Option<Recycled>,
    /// "Map network drive" is open (in Computer, over the drives).
    form: Option<MapForm>,
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
    /// When and where the last press was, for double-clicks.
    last_press: (u64, i32, i32),
}

impl Desktop {
    fn taskbar(&self) -> Rect {
        Rect::new(0, self.h - TASKBAR * self.ui, self.w, TASKBAR * self.ui)
    }
    fn title_h(&self) -> i32 {
        TITLE * self.ui
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
        let w = 100 * self.ui;
        Rect::new(self.show_desktop().x - w, t.y, w, t.h)
    }
    fn task_button(&self, i: usize) -> Rect {
        let t = self.taskbar();
        let s = self.ui;
        Rect::new(t.x + 58 * s + i as i32 * 154 * s, t.y + 3 * s, 150 * s, t.h - 6 * s)
    }
    fn menu_rect(&self) -> Rect {
        let s = self.ui;
        let t = self.taskbar();
        Rect::new(2 * s, t.y - 380 * s, 480 * s, 380 * s)
    }
    fn menu_left(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + 8 * s, m.y + 8 * s, 250 * s, m.h - 56 * s)
    }
    fn menu_item(&self, i: usize) -> Rect {
        let l = self.menu_left();
        let s = self.ui;
        Rect::new(l.x + 4 * s, l.y + 6 * s + i as i32 * 40 * s, l.w - 8 * s, 36 * s)
    }
    /// The Start menu's mouse speed buttons: slower (-) and faster (+), with
    /// ten bars between them.
    fn speed_button(&self, faster: bool) -> Rect {
        let l = self.menu_left();
        let m = self.menu_rect();
        let s = self.ui;
        let rx = l.x + l.w + 14 * s;
        Rect::new(rx + if faster { 152 * s } else { 0 }, m.y + 160 * s, 30 * s, 30 * s)
    }
    fn exit_button(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + m.w - 200 * s, m.y + m.h - 42 * s, 190 * s, 34 * s)
    }
    fn cursor_rect(&self) -> Rect {
        // Room for the arrow and for the resize arrows centred on the pointer.
        let s = self.ui;
        Rect::new(self.pointer.x as i32 - 10 * s, self.pointer.y as i32 - 10 * s, 23 * s, 30 * s)
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
        Rect::new(4 * s, 10 * s + i as i32 * 94 * s, 116 * s, 86 * s)
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
        Rect::new(client.x, client.y, client.w, 32 * self.ui)
    }
    fn notes_save(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let t = self.notes_toolbar(client);
        Rect::new(t.x + 6 * s, t.y + 4 * s, 64 * s, t.h - 8 * s)
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
        let (gx, gy) = (self.w * 2 / 3, self.h * 3 / 4);
        let reach = (self.w * self.w + self.h * self.h) / 5;
        for y in clip.y..clip.y + clip.h {
            let base = lerp(rgb(20, 92, 170), rgb(4, 30, 82), y, self.h);
            let row = (y * c.w) as usize;
            for x in clip.x..clip.x + clip.w {
                let (dx, dy) = (x - gx, y - gy);
                let d = dx * dx + dy * dy;
                let glow = if d < reach { (reach - d) / (reach / 110).max(1) } else { 0 };
                let band = |offset: i32, width: i32, strength: i32| {
                    let d = (x / 3 + y - offset).abs();
                    if d < width { (width - d) * strength / width } else { 0 }
                };
                let light = (glow + band(self.h * 2 / 3, self.h / 6, 60) + band(self.h, self.h / 12, 50)).min(200);
                c.px[row + x as usize] = blend(base, rgb(150, 220, 255), light as u32);
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

        c.glow_text(r.x + 14 * s, r.y + (self.title_h() - Font::Bold.h(s)) / 2, win.title, 0x000000, Font::Bold);
        for which in [Caption::Minimize, Caption::Maximize, Caption::Close] {
            self.caption_button(c, &r, which);
        }

        let client = self.client(&r);
        c.fill(Rect::new(client.x - 1, client.y - 1, client.w + 2, client.h + 2), rgb(90, 110, 140));
        c.fill(client, 0xFFFFFF);
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
        }
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
            let (px, py) = (r.x + r.w / 2 - 20 * s, r.y + 6 * s);
            match kind {
                Kind::Computer => c.computer(px, py, s),
                Kind::Notes => c.notepad(px + 6 * s, py, s),
                Kind::Calculator => c.calculator(px + 8 * s, py, s),
                _ => {
                    c.orb(px + 20 * s, py + 19 * s, 17 * s, rgb(140, 210, 255), rgb(20, 90, 170));
                    c.fill(Rect::new(px + 18 * s, py + 9 * s, 4 * s, 4 * s), 0xFFFFFF);
                    c.fill(Rect::new(px + 18 * s, py + 16 * s, 4 * s, 13 * s), 0xFFFFFF);
                }
            }
            let label = self.windows[self.index(kind)].title;
            let lx = r.x + (r.w - Font::Normal.width(s, label)) / 2;
            let ly = r.y + r.h - Font::Normal.h(s) - 4 * s;
            c.text(lx + s, ly + s, label, 0x000000, Font::Normal);
            c.text(lx, ly, label, 0xFFFFFF, Font::Normal);
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
        for (i, kind) in self.task_order().into_iter().enumerate() {
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
            // A small window icon, then the title.
            let icon = Rect::new(b.x + 8 * s, b.y + b.h / 2 - 8 * s, 18 * s, 16 * s);
            c.fill(icon, rgb(235, 245, 255));
            c.fill(Rect::new(icon.x, icon.y, icon.w, 4 * s), rgb(60, 130, 210));
            c.frame(icon, rgb(20, 50, 90));
            c.text(b.x + 34 * s, b.y + (b.h - Font::Normal.h(s)) / 2, win.title, 0xFFFFFF, Font::Normal);
        }

        // Clock: time over date, then the "show desktop" strip.
        let tray = self.tray();
        let (time, date) = (&self.clock.0, &self.clock.1);
        let cx = |text: &String| tray.x + (tray.w - Font::Small.width(s, text)) / 2;
        let line = Font::Small.h(s);
        c.text(cx(time), tray.y + tray.h / 2 - line, time, 0xFFFFFF, Font::Small);
        c.text(cx(date), tray.y + tray.h / 2, date, 0xFFFFFF, Font::Small);
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
        for (i, kind) in KINDS.iter().enumerate() {
            let item = self.menu_item(i);
            if item.contains(self.pointer.x as i32, self.pointer.y as i32) {
                c.rounded(item, 3 * s, false, rgb(225, 240, 255), rgb(190, 220, 250), 255);
                c.rounded_outline(item, 3 * s, false, rgb(120, 170, 230), 255);
            }
            let icon = Rect::new(item.x + 8 * s, item.y + 6 * s, 22 * s, 20 * s);
            c.fill(icon, rgb(235, 245, 255));
            c.fill(Rect::new(icon.x, icon.y, icon.w, 5 * s), rgb(60, 130, 210));
            c.frame(icon, rgb(20, 50, 90));
            let title = self.windows[self.index(*kind)].title;
            c.text(item.x + 40 * s, item.y + (item.h - Font::Normal.h(s)) / 2, title, 0x101010, Font::Normal);
        }
        c.text(l.x + 12 * s, l.y + l.h - Font::Normal.h(s) - 8 * s, "All programs", rgb(90, 90, 90), Font::Normal);
        // Right: the date in large type, and the exit button.
        let rx = l.x + l.w + 14 * s;
        c.text(rx, m.y + 16 * s, &self.clock.0, 0xFFFFFF, Font::Large);
        c.text(rx, m.y + 56 * s, &self.clock.1, rgb(210, 230, 250), Font::Normal);
        c.text(rx, m.y + 84 * s, "AeroForge", rgb(210, 230, 250), Font::Normal);
        // Mouse speed: - and + buttons with ten bars between them.
        c.text(rx, m.y + 128 * s, &format!("Mouse speed {}", self.mouse_speed), 0xFFFFFF, Font::Normal);
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
        let e = self.exit_button();
        let hover = e.contains(self.pointer.x as i32, self.pointer.y as i32);
        c.rounded(e, 3 * s, false, if hover { rgb(250, 180, 150) } else { rgb(230, 150, 120) }, rgb(170, 50, 25), 240);
        c.rounded_outline(e, 3 * s, false, rgb(80, 20, 10), 255);
        let label = "Exit to console";
        c.text(e.x + (e.w - Font::Normal.width(s, label)) / 2, e.y + (e.h - Font::Normal.h(s)) / 2, label, 0xFFFFFF, Font::Normal);
    }

    fn cursor(&self, c: &mut Canvas) {
        let (x0, y0, s) = (self.pointer.x as i32, self.pointer.y as i32, self.ui);
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
        self.background(c);
        self.icons(c);
        let top = self.top_kind();
        for win in self.windows.iter().filter(|w| w.shown()) {
            self.window(c, win, Some(win.kind) == top);
        }
        self.snap_preview(c);
        self.taskbar_and_menu(c);
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
        if self.menu {
            spots.extend((0..KINDS.len()).map(|i| self.menu_item(i)));
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
        let work_h = self.h - TASKBAR * self.ui;
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
        let work = Rect::new(0, 0, self.w, self.h - TASKBAR * self.ui);
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
    fn click(&mut self, x: i32, y: i32) -> Rect {
        let now = aero::clock_us();
        let (when, px, py) = self.last_press;
        let near = 4 * self.ui;
        let double = when != 0 && now - when < DOUBLE_CLICK_US && (x - px).abs() <= near && (y - py).abs() <= near;
        // A third press starts over rather than making a second double-click.
        self.last_press = if double { (0, 0, 0) } else { (now, x, y) };
        let mut dirty = Rect::EMPTY;
        let menu_was_open = self.menu;
        if self.menu {
            self.menu = false;
            dirty = dirty.union(&self.menu_rect());
            for (i, &kind) in KINDS.iter().enumerate() {
                if self.menu_item(i).contains(x, y) {
                    return dirty.union(&self.open(kind));
                }
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
            let i = self.raise(i);
            let r = self.windows[i].rect;
            dirty = dirty.union(&self.window_area(i)).union(&self.taskbar());
            if self.caption(&r, Caption::Close).contains(x, y) {
                self.windows[i].open = false;
                println!("[desktop] closed {}", self.windows[i].title);
            } else if self.caption(&r, Caption::Minimize).contains(x, y) {
                self.windows[i].minimized = true;
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
            self.files.back.clear();
            self.files.forward.clear();
            self.open_dir(String::new());
        }
        let r = self.windows[at].rect;
        println!("[desktop] opened {} at {},{} ({}x{})", self.windows[at].title, r.x, r.y, r.w, r.h);
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


    fn typed(&mut self, keys: &[u8]) -> Rect {
        let mut dirty = Rect::EMPTY;
        for &k in keys {
            // Esc gives the screen back, unless the Computer window is
            // in front and waiting for a new name or a Yes or No.
            let files_busy = self.files.rename.is_some() || self.files.confirm.is_some() || self.files.form.is_some();
            if self.top_kind() == Some(Kind::Computer) && (k != 27 || files_busy) {
                dirty = dirty.union(&self.files_key(k));
                continue;
            }
            if k == 27 {
                self.quit = true;
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

/// The biggest file Copy and Paste handle for now (one write system call).
const MAX_COPY: u64 = 8 * 1024 * 1024;

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

/// Renames in place when both paths are on the same network share (local
/// drives have no rename call yet). None when it can't be done that way.
fn fs_rename(from: &str, to: &str) -> Option<Result<(), i64>> {
    let (i, a) = net_path(from)?;
    let (j, b) = net_path(to)?;
    (i == j).then(|| with_share(i, |c| c.rename(&a, &b)))
}

/// Moves a file or folder: a rename on one network share, else copy and delete.
fn move_tree(from: &str, to: &str, is_dir: bool, size: u64) -> Result<(), String> {
    if let Some(r) = fs_rename(from, to) {
        return r.map_err(|e| format!("Can't move {}: {}", from, fs_error_text(e)));
    }
    copy_tree(from, to, is_dir, size)?;
    delete_tree(from, is_dir).map(|_| ())
}

/// Copies a file, or a folder and all it holds, to `to`. Returns how many
/// files it copied.
fn copy_tree(from: &str, to: &str, is_dir: bool, size: u64) -> Result<usize, String> {
    if is_dir {
        fs_mkdir(to).map_err(|e| format!("Can't make {}: {}", to, fs_error_text(e)))?;
        let mut n = 0;
        for e in fs_list(from).map_err(|e| format!("Can't read {}: {}", from, fs_error_text(e)))? {
            n += copy_tree(&join(from, &e.name), &join(to, &e.name), e.is_dir, e.size)?;
        }
        return Ok(n);
    }
    if size > MAX_COPY {
        return Err(format!("{} is too big to copy here yet (8 MB at most)", split_path(from).1));
    }
    let data = fs_read(from, size as usize + 1).map_err(|e| format!("Can't read {}: {}", from, fs_error_text(e)))?;
    fs_write(to, &data).map_err(|e| format!("Can't write {}: {}", to, fs_error_text(e)))?;
    Ok(1)
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
        "wav" | "mp3" => "Sound",
        "exe" | "elf" => "Program",
        "zip" => "Compressed Folder",
        _ => return format!("{} File", ext.to_ascii_uppercase()),
    })
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
        self.drive(x, y - 3 * s, s, false);
        self.fill(Rect::new(x + 15 * s, y + 19 * s, 3 * s, 4 * s), rgb(60, 70, 85));
        let bar = if connected { rgb(40, 160, 70) } else { rgb(170, 60, 50) };
        self.fill(Rect::new(x + 4 * s, y + 23 * s, 24 * s, 3 * s), bar);
    }

    fn drive(&mut self, x: i32, y: i32, s: i32, removable: bool) {
        let body = Rect::new(x, y + 6 * s, 32 * s, 16 * s);
        let (top, bottom) = if removable { (rgb(120, 170, 230), rgb(40, 90, 170)) } else { (rgb(210, 215, 222), rgb(130, 136, 146)) };
        self.rounded(body, 3 * s, false, top, bottom, 255);
        self.rounded_outline(body, 3 * s, false, rgb(90, 96, 106), 255);
        self.shade(Rect::new(body.x + 2 * s, body.y + 2 * s, body.w - 4 * s, 5 * s), 0xFFFFFF, 90);
        self.fill(Rect::new(x + 24 * s, y + 15 * s, 4 * s, 3 * s), rgb(80, 220, 90));
    }

    /// The small drive for the navigation pane, 16 x 12 at scale 1.
    fn small_drive(&mut self, x: i32, y: i32, s: i32, network: bool) {
        let body = Rect::new(x, y + 1 * s, 16 * s, 9 * s);
        self.gradient(body, rgb(215, 220, 228), rgb(140, 146, 156), 255);
        self.frame(body, rgb(90, 96, 106));
        self.fill(Rect::new(x + 11 * s, y + 6 * s, 3 * s, 2 * s), rgb(80, 220, 90));
        if network {
            // A cable down to a green bar: a network drive.
            self.fill(Rect::new(x + 7 * s, y + 10 * s, 2 * s, 2 * s), rgb(60, 70, 85));
            self.fill(Rect::new(x + 2 * s, y + 12 * s, 12 * s, 2 * s), rgb(40, 150, 70));
        }
    }

    /// A small wastebasket for the Recycle Bin, 14 x 16 at scale 1.
    fn small_bin(&mut self, x: i32, y: i32, s: i32) {
        self.fill(Rect::new(x, y + 2 * s, 14 * s, 2 * s), rgb(120, 140, 160));
        self.fill(Rect::new(x + 5 * s, y, 4 * s, 2 * s), rgb(120, 140, 160));
        let body = Rect::new(x + s, y + 4 * s, 12 * s, 12 * s);
        self.gradient(body, rgb(225, 238, 250), rgb(160, 190, 220), 255);
        self.frame(body, rgb(90, 110, 140));
        for i in 0..3 {
            self.fill(Rect::new(x + (4 + 3 * i) * s, y + 6 * s, s, 8 * s), rgb(110, 135, 165));
        }
    }

    /// A small screen for the navigation pane, 16 x 14 at scale 1.
    fn small_computer(&mut self, x: i32, y: i32, s: i32) {
        self.fill(Rect::new(x, y, 16 * s, 11 * s), rgb(40, 44, 52));
        self.gradient(Rect::new(x + s, y + s, 14 * s, 9 * s), rgb(120, 200, 250), rgb(20, 80, 170), 255);
        self.fill(Rect::new(x + 5 * s, y + 11 * s, 6 * s, 3 * s), rgb(80, 80, 90));
    }
}

impl Desktop {
    fn files_client(&self) -> Rect {
        self.client(&self.windows[self.index(Kind::Computer)].rect)
    }
    /// The Computer window's area, to redraw it.
    fn files_area(&self) -> Rect {
        self.window_area(self.index(Kind::Computer))
    }
    fn files_nav(&self, c: &Rect) -> Rect {
        Rect::new(c.x, c.y, c.w, 40 * self.ui)
    }
    /// Back (0), Forward (1) and Up (2): centre and radius.
    fn files_arrow(&self, c: &Rect, i: i32) -> (i32, i32, i32) {
        let s = self.ui;
        (c.x + 20 * s + i * 30 * s, c.y + 20 * s, if i == 2 { 10 * s } else { 12 * s })
    }
    fn files_address(&self, c: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(c.x + 112 * s, c.y + 6 * s, c.w - 120 * s, 28 * s)
    }
    fn files_commands(&self, c: &Rect) -> Rect {
        Rect::new(c.x, c.y + 40 * self.ui, c.w, 34 * self.ui)
    }
    fn files_status_bar(&self, c: &Rect) -> Rect {
        Rect::new(c.x, c.y + c.h - 28 * self.ui, c.w, 28 * self.ui)
    }
    fn files_body(&self, c: &Rect) -> Rect {
        let s = self.ui;
        Rect::new(c.x, c.y + 74 * s, c.w, c.h - 74 * s - 28 * s)
    }
    fn files_pane(&self, c: &Rect) -> Rect {
        let b = self.files_body(c);
        Rect::new(b.x, b.y, (200 * self.ui).min(b.w / 3), b.h)
    }
    fn files_content(&self, c: &Rect) -> Rect {
        let b = self.files_body(c);
        let p = self.files_pane(c);
        Rect::new(p.x + p.w + 1, b.y, b.w - p.w - 1, b.h)
    }
    /// The pane's rows: Computer (0), then the drives.
    fn pane_item(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let p = self.files_pane(c);
        Rect::new(p.x, p.y + 6 * s + i as i32 * 28 * s, p.w, 28 * s)
    }
    fn files_head(&self, c: &Rect) -> Rect {
        let k = self.files_content(c);
        Rect::new(k.x, k.y, k.w, 28 * self.ui)
    }
    fn files_rows(&self, c: &Rect) -> Rect {
        let s = self.ui;
        let k = self.files_content(c);
        Rect::new(k.x, k.y + 28 * s, k.w - 16 * s, k.h - 28 * s)
    }
    fn files_scrollbar(&self, c: &Rect) -> Rect {
        let r = self.files_rows(c);
        Rect::new(r.x + r.w, r.y, 16 * self.ui, r.h)
    }
    fn files_row_h(&self) -> i32 {
        30 * self.ui
    }
    fn files_visible(&self, c: &Rect) -> usize {
        (self.files_rows(c).h / self.files_row_h()).max(1) as usize
    }
    /// Where entry `i` (counted from the first one shown) is drawn.
    fn files_row(&self, c: &Rect, i: usize) -> Rect {
        let rows = self.files_rows(c);
        Rect::new(rows.x, rows.y + i as i32 * self.files_row_h(), rows.w, self.files_row_h())
    }
    /// Where the Type and Size columns start.
    fn files_columns(&self, c: &Rect) -> (i32, i32) {
        let s = self.ui;
        let rows = self.files_rows(c);
        let size_x = rows.x + rows.w - 90 * s;
        let type_w = ((rows.w - 90 * s) * 2 / 5).min(170 * s);
        (size_x - type_w, size_x)
    }
    fn tile_columns(&self, c: &Rect) -> usize {
        let s = self.ui;
        ((self.files_content(c).w - 12 * s) / (262 * s)).max(1) as usize
    }
    /// The tile for drive `i` in Computer: as many columns as fit, sharing the width.
    fn drive_tile(&self, c: &Rect, i: usize) -> Rect {
        let s = self.ui;
        let k = self.files_content(c);
        let cols = self.tile_columns(c);
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
        } else if self.files.path.is_empty() {
            &COMPUTER_COMMANDS
        } else {
            &COMMANDS
        };
        for &(cmd, label) in list {
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
    fn command_enabled(&self, cmd: Command) -> bool {
        let folder = !self.files.path.is_empty();
        let writable = folder && !self.files_read_only();
        let picked = self.selected_entry().is_some();
        match cmd {
            Command::NewFolder => writable,
            Command::Copy => picked,
            Command::Cut | Command::Rename => picked && writable,
            Command::Paste => writable && self.files.clip.is_some(),
            Command::Restore => picked && self.files.path == BIN,
            Command::Delete if self.files.path == BIN => picked,
            Command::Delete => picked && writable,
            Command::Empty => !self.files.bin.is_empty(),
            Command::Map => !folder && self.files.form.is_none(),
            Command::Disconnect => !folder && self.files.selected.is_some_and(|i| i >= self.files.drives.len()),
            Command::Yes | Command::No => true,
        }
    }

    /// The navigation pane's rows: Computer, the drives, the network
    /// drives and the Recycle Bin, as (shown as, path, kind).
    fn places(&self) -> Vec<(String, String, Place)> {
        let mut out = alloc::vec![(String::from("Computer"), String::new(), Place::Computer)];
        for i in 0..self.files.drives.len() {
            out.push((self.drive_name(i), self.files.drives[i].path.clone(), Place::Drive));
        }
        for sh in shares().iter() {
            out.push((sh.name(), sh.root(), Place::Network));
        }
        out.push((String::from("Recycle Bin"), String::from(BIN), Place::Bin));
        out
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

        // Navigation pane: Computer and the drives.
        let pane = self.files_pane(client);
        c.fill(pane, rgb(241, 245, 251));
        c.fill(Rect::new(pane.x + pane.w, pane.y, 1, pane.h), rgb(205, 215, 230));
        let here = self.drive_of(&f.path);
        let pane_cols = ((pane.w - 34 * s) / Font::Normal.w(s)).max(1) as usize;
        let net_here = net_path(&f.path).map(|(i, _)| i);
        for (i, (text, path, icon)) in self.places().into_iter().enumerate() {
            let r = self.pane_item(client, i);
            if r.y + r.h > pane.y + pane.h {
                break;
            }
            let current = match icon {
                Place::Computer => !in_folder,
                Place::Bin => f.path == BIN,
                Place::Drive => here.is_some_and(|d| f.drives[d].path == path),
                Place::Network => net_here.is_some_and(|n| shares()[n].root() == path),
            };
            let box_ = Rect::new(r.x + 3 * s, r.y + s, r.w - 6 * s, r.h - 2 * s);
            if current {
                c.rounded(box_, 2 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(box_, 2 * s, false, rgb(125, 162, 206), 255);
            } else if r.contains(px, py) {
                c.rounded(box_, 2 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
            }
            let ix = if matches!(icon, Place::Drive | Place::Network) { 18 * s } else { 8 * s };
            match icon {
                Place::Computer => c.small_computer(r.x + ix, r.y + (r.h - 14 * s) / 2, s),
                Place::Bin => c.small_bin(r.x + ix + s, r.y + (r.h - 16 * s) / 2, s),
                Place::Drive => c.small_drive(r.x + ix, r.y + (r.h - 12 * s) / 2, s, false),
                Place::Network => c.small_drive(r.x + ix, r.y + (r.h - 12 * s) / 2, s, true),
            }
            let room = pane_cols.saturating_sub(if ix > 8 * s { 1 } else { 0 });
            let shown: String = text.chars().take(room).collect();
            c.text(r.x + ix + 22 * s, label_y(&r), &shown, rgb(20, 40, 80), Font::Normal);
        }

        let content = self.files_content(client);
        c.fill(content, 0xFFFFFF);
        if in_folder {
            self.folder_view(c, client);
        } else {
            self.drives_view(c, client);
        }

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

    /// Computer: the drives as tiles, with how full each one is.
    fn drives_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let f = &self.files;
        let k = self.files_content(client);
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        if f.form.is_some() {
            self.form_view(c, client);
            return;
        }
        let heading = format!("Drives ({})", self.tile_count());
        c.text(k.x + 12 * s, k.y + 8 * s, &heading, rgb(30, 57, 145), Font::Normal);
        let line_x = k.x + 24 * s + Font::Normal.width(s, &heading);
        c.fill(Rect::new(line_x, k.y + 8 * s + Font::Normal.h(s) / 2, k.x + k.w - 12 * s - line_x, 1), rgb(200, 215, 235));
        for i in 0..f.drives.len() {
            let t = self.drive_tile(client, i);
            if t.y + t.h > k.y + k.h {
                break;
            }
            let v = &f.drives[i];
            if f.selected == Some(i) {
                c.rounded(t, 3 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(t, 3 * s, false, rgb(125, 162, 206), 255);
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
            let t = self.drive_tile(client, i);
            if t.y + t.h > k.y + k.h {
                break;
            }
            if f.selected == Some(i) {
                c.rounded(t, 3 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(t, 3 * s, false, rgb(125, 162, 206), 255);
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
        let mut text = format!("mouse speed = {}\n", self.mouse_speed);
        for sh in shares().iter() {
            text.push_str(&format!("network drive = {}|{}|{}\n", sh.unc(), sh.user, sh.letter));
        }
        aero::write_file(SETTINGS, text.as_bytes()).is_ok()
    }

    /// A folder: column headings, the rows and a scroll bar.
    fn folder_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let f = &self.files;
        let (px, py) = (self.pointer.x as i32, self.pointer.y as i32);
        let fw = Font::Normal.w(s);
        let (type_x, size_x) = self.files_columns(client);
        let head = self.files_head(client);
        let rows = self.files_rows(client);
        let head_y = head.y + (head.h - Font::Normal.h(s)) / 2;
        let type_label = if f.path == BIN { "Original location" } else { "Type" };
        for (by, label, x, end) in [
            (SortBy::Name, "Name", rows.x + 30 * s, type_x),
            (SortBy::Type, type_label, type_x, size_x),
            (SortBy::Size, "Size", size_x, rows.x + rows.w),
        ] {
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
            let empty = if f.path == BIN { "The Recycle Bin is empty." } else { "This folder is empty." };
            c.text(rows.x + 30 * s, rows.y + 10 * s, empty, rgb(120, 125, 135), Font::Normal);
        }
        let name_cols = ((type_x - 12 * s - rows.x - 30 * s) / fw).max(1) as usize;
        let type_cols = ((size_x - 12 * s - type_x) / fw).max(1) as usize;
        for (i, e) in f.entries.iter().enumerate().skip(f.scroll).take(self.files_visible(client)) {
            let r = self.files_row(client, i - f.scroll);
            let box_ = Rect::new(r.x + 2 * s, r.y, r.w - 4 * s, r.h);
            if f.selected == Some(i) {
                c.rounded(box_, 2 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(box_, 2 * s, false, rgb(125, 162, 206), 255);
            } else if r.contains(px, py) {
                c.rounded(box_, 2 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
            }
            if e.is_dir {
                c.folder(r.x + 8 * s, r.y + (r.h - 12 * s) / 2, s);
            } else {
                c.page(r.x + 10 * s, r.y + (r.h - 14 * s) / 2, s);
            }
            let ty = r.y + (r.h - Font::Normal.h(s)) / 2;
            match (&f.rename, f.selected == Some(i)) {
                (Some(new), true) => {
                    // The name being typed, in an edit box with a caret.
                    let skip = new.chars().count().saturating_sub(name_cols.saturating_sub(1));
                    let shown: String = new.chars().skip(skip).collect();
                    let edit = Rect::new(rows.x + 26 * s, r.y + 2 * s, type_x - 8 * s - rows.x - 26 * s, r.h - 4 * s);
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
                    let cut = f.clip.as_ref().is_some_and(|k| k.cut && k.path == join(&f.path, &e.name));
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
                _ => type_text(e),
            };
            let kind: String = kind.chars().take(type_cols).collect();
            c.text(type_x, ty, &kind, rgb(90, 90, 90), Font::Normal);
            if !e.is_dir {
                c.text(size_x, ty, &size_text(e.size), rgb(80, 80, 80), Font::Normal);
            }
        }

        // Scroll bar: arrows at the ends, the thumb showing what part is in view.
        let sb = self.files_scrollbar(client);
        c.fill(sb, rgb(240, 241, 244));
        let visible = self.files_visible(client);
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
        spots.extend((0..self.places().len()).map(|i| self.pane_item(c, i)));
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
        if !path.is_empty() {
            self.files.form = None;
        }
        if let Ok(drives) = aero::volumes() {
            self.files.drives = drives;
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
            let bin = self.pane_item(&c, self.places().len() - 1);
            let map = self.command_buttons(&c).into_iter().find(|(cmd, _, _)| *cmd == Command::Map).map(|(_, _, r)| r).unwrap_or(Rect::EMPTY);
            println!("[desktop] Computer: drives = {}; Recycle Bin at {},{}; Map network drive at {},{}", tiles.join(" | "),
                bin.x + 60 * self.ui, bin.y + bin.h / 2, map.x + map.w / 2, map.y + map.h / 2);
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
                self.sort_entries();
                let n = self.files.entries.len();
                self.files.status = format!("{} item{}", n, if n == 1 { "" } else { "s" });
                let names: Vec<&str> = self.files.entries.iter().map(|e| e.name.as_str()).collect();
                let c = self.files_client();
                let row = self.files_row(&c, 0);
                println!("[desktop] Computer: {} = {}; first row at {},{}, rows {} apart",
                    path, names.join(" | "), row.x + 40 * self.ui, row.y + row.h / 2, row.h);
                self.files.path = path;
                self.files.scroll = 0;
                self.files.selected = None;
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
        self.files.entries.sort_by(|a, b| {
            let order = match by {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Type => type_text(a).cmp(&type_text(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                SortBy::Size => a.size.cmp(&b.size).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            };
            b.is_dir.cmp(&a.is_dir).then(if desc { order.reverse() } else { order })
        });
    }

    /// Selects entry (or drive) `i`, scrolls it into view and says what it is.
    fn select(&mut self, i: usize) {
        let c = self.files_client();
        self.files.selected = Some(i);
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
        let visible = self.files_visible(&c);
        if i < self.files.scroll {
            self.files.scroll = i;
        } else if i >= self.files.scroll + visible {
            self.files.scroll = i + 1 - visible;
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
        if self.files.path == BIN {
            return self.command(Command::Restore);
        }
        let Some(e) = self.files.entries.get(i).cloned() else { return Rect::EMPTY };
        let path = join(&self.files.path, &e.name);
        if e.is_dir {
            self.navigate(path)
        } else {
            self.open_file(path)
        }
    }

    fn command(&mut self, cmd: Command) -> Rect {
        let area = self.files_area();
        if !self.command_enabled(cmd) {
            return Rect::EMPTY;
        }
        let dir = self.files.path.clone();
        match cmd {
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
                let e = self.selected_entry().unwrap();
                let path = join(&dir, &e.name);
                let cut = cmd == Command::Cut;
                println!("[desktop] {} {}", if cut { "cut" } else { "copied" }, path);
                self.files.status = format!("{} \"{}\": open a folder and Paste", if cut { "Cut" } else { "Copied" }, e.name);
                self.files.clip = Some(Clip { path, is_dir: e.is_dir, size: e.size, cut });
            }
            Command::Paste => self.paste(),
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
            Command::Restore => {
                let i = self.files.selected.unwrap();
                let item = self.files.bin[i].clone();
                match self.restore(&item) {
                    Ok(to) => {
                        self.refresh(None);
                        if !self.files.entries.is_empty() {
                            self.select(i.min(self.files.entries.len() - 1));
                        }
                        self.files.status = format!("Restored to {}", to);
                    }
                    Err(msg) => self.files.status = msg,
                }
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
                            if let Err(msg) = self.purge(item) {
                                failed = Some(msg);
                            }
                        }
                        println!("[desktop] emptied the Recycle Bin ({} item{})", n, if n == 1 { "" } else { "s" });
                        failed.map_or(Ok(String::from("The Recycle Bin is empty")), Err)
                    }
                    _ if self.files.path == BIN => {
                        let item = self.files.bin[at].clone();
                        self.purge(&item).map(|_| format!("Deleted \"{}\" for good", split_path(&item.original).1))
                    }
                    _ => {
                        let e = self.selected_entry().unwrap();
                        let path = join(&dir, &e.name);
                        if self.recycle_root().is_some() {
                            self.recycle(&path, &e).map(|_| format!("Moved \"{}\" to the Recycle Bin (Ctrl+Z puts it back)", e.name))
                        } else {
                            delete_tree(&path, e.is_dir).map(|n| {
                                println!("[desktop] deleted {} ({} item{})", path, n, if n == 1 { "" } else { "s" });
                                format!("Deleted \"{}\"", e.name)
                            })
                        }
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

    /// Copies (or moves, after Cut) what the clipboard holds into the folder shown.
    fn paste(&mut self) {
        let Some(clip) = self.files.clip.clone() else { return };
        let dir = self.files.path.clone();
        let (from_dir, name) = split_path(&clip.path);
        if clip.is_dir && (dir.eq_ignore_ascii_case(&clip.path) || dir.to_lowercase().starts_with(&format!("{}/", clip.path.to_lowercase()))) {
            self.files.status = String::from("A folder can't go inside itself");
            return;
        }
        if clip.cut && from_dir.eq_ignore_ascii_case(&dir) {
            self.files.clip = None;
            self.files.status = String::from("It is already here");
            return;
        }
        let taken: Vec<&str> = self.files.entries.iter().map(|e| e.name.as_str()).collect();
        let new_name = free_name(&name, &taken, clip.is_dir, true);
        let to = join(&dir, &new_name);
        if clip.cut {
            let r = move_tree(&clip.path, &to, clip.is_dir, clip.size);
            match r {
                Ok(()) => {
                    println!("[desktop] moved {} to {}", clip.path, to);
                    self.files.status = format!("Moved \"{}\" here", name);
                    self.files.clip = None;
                    self.refresh(Some(new_name));
                    self.files.status = format!("Moved \"{}\" here", name);
                }
                Err(msg) => {
                    println!("[desktop] move failed: {}", msg);
                    self.refresh(None);
                    self.files.status = msg;
                }
            }
            return;
        }
        match copy_tree(&clip.path, &to, clip.is_dir, clip.size) {
            Ok(n) => {
                let files = format!("{} file{}", n, if n == 1 { "" } else { "s" });
                {
                    println!("[desktop] pasted {} as {} ({})", clip.path, to, files);
                    self.files.status = format!("Pasted \"{}\" ({})", new_name, files);
                }
                let status = self.files.status.clone();
                self.refresh(Some(new_name));
                self.files.status = status;
            }
            Err(msg) => {
                println!("[desktop] paste failed: {}", msg);
                self.refresh(None);
                self.files.status = msg;
            }
        }
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
        if self.files.entries.iter().any(|o| o.name.eq_ignore_ascii_case(&new)) {
            self.files.status = format!("There is already something called \"{}\" here", new);
            return;
        }
        let dir = self.files.path.clone();
        let (from, to) = (join(&dir, &e.name), join(&dir, &new));
        // Locally there is no rename call yet: copy under the new name, then delete the old one.
        let result = move_tree(&from, &to, e.is_dir, e.size);
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
        let d = self.drive_of(&self.files.path)?;
        let v = &self.files.drives[d];
        if v.read_only { None } else { Some(v.path.clone()) }
    }

    fn read_bin(root: &str) -> Vec<Recycled> {
        let mut buf = alloc::vec![0u8; 64 * 1024];
        let Ok(n) = aero::read_file(&join(&join(root, BIN_DIR), BIN_INFO), &mut buf) else { return Vec::new() };
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
        let mut text = String::new();
        for b in items {
            text.push_str(&format!("{}\t{}\t{}\t{}\t{}\n", b.id, if b.is_dir { "D" } else { "F" }, b.size, b.when, b.original));
        }
        let path = join(&join(root, BIN_DIR), BIN_INFO);
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
            .map(|b| aero::DirEntry { name: split_path(&b.original).1, is_dir: b.is_dir, size: b.size })
            .collect();
        self.files.bin = all;
    }

    /// Moves `path` into its drive's Recycle Bin.
    fn recycle(&mut self, path: &str, e: &aero::DirEntry) -> Result<Recycled, String> {
        let root = self.recycle_root().ok_or_else(|| String::from("This drive has no Recycle Bin"))?;
        let bin = join(&root, BIN_DIR);
        match aero::create_dir(&bin) {
            Ok(()) | Err(aero::E_EXISTS) => {}
            Err(e) => return Err(format!("Can't make the Recycle Bin: {}", fs_error_text(e))),
        }
        let mut items = Self::read_bin(&root);
        // The next free number, past anything already in the folder.
        let mut next = 1;
        let listed = aero::list_dir(&bin).unwrap_or_default();
        for name in items.iter().map(|b| b.id.as_str()).chain(listed.iter().map(|l| l.name.as_str())) {
            if let Some(n) = name.strip_prefix('R').and_then(|n| n.parse::<u32>().ok()) {
                next = next.max(n + 1);
            }
        }
        let when = match aero::now() {
            Some(t) => format!("{}/{}/{} {}:{:02} {}", t.month, t.day, t.year, if t.hour % 12 == 0 { 12 } else { t.hour % 12 }, t.minute, if t.hour < 12 { "AM" } else { "PM" }),
            None => String::from("-"),
        };
        let item = Recycled { root: root.clone(), id: format!("R{:04}", next), original: String::from(path), is_dir: e.is_dir, size: e.size, when };
        copy_tree(path, &item.stored(), e.is_dir, e.size).map_err(|m| format!("Not deleted: {}", m))?;
        items.push(item.clone());
        if let Err(m) = Self::write_bin(&root, &items) {
            let _ = delete_tree(&item.stored(), e.is_dir);
            return Err(format!("Not deleted: {}", m));
        }
        delete_tree(path, e.is_dir)?;
        println!("[desktop] moved {} to the Recycle Bin as {}", path, item.stored());
        self.files.last_recycled = Some(item.clone());
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
        copy_tree(&item.stored(), &to, item.is_dir, item.size).map_err(|m| format!("Not restored: {}", m))?;
        self.purge(item)?;
        println!("[desktop] restored {} from the Recycle Bin to {}", item.original, to);
        if self.files.last_recycled.as_ref().is_some_and(|l| l.root == item.root && l.id == item.id) {
            self.files.last_recycled = None;
        }
        Ok(to)
    }

    /// Deletes something in the Recycle Bin for good.
    fn purge(&self, item: &Recycled) -> Result<(), String> {
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
        let Some(item) = self.files.last_recycled.clone() else {
            println!("[desktop] nothing to undo");
            self.files.status = String::from("Nothing to undo");
            return self.files_area();
        };
        match self.restore(&item) {
            Ok(to) => {
                let (dir, name) = split_path(&to);
                if self.files.path.eq_ignore_ascii_case(&dir) || self.files.path == BIN {
                    self.refresh(Some(name));
                }
                self.files.status = format!("Undid the delete: {} is back", to);
            }
            Err(msg) => {
                println!("[desktop] {}", msg);
                self.files.status = msg;
            }
        }
        self.files_area()
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
                let (_, path, kind) = places[i].clone();
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
            let (type_x, size_x) = self.files_columns(&c);
            let by = if x >= size_x - 8 * self.ui {
                SortBy::Size
            } else if x >= type_x - 8 * self.ui {
                SortBy::Type
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
            let last = self.files.entries.len().saturating_sub(visible);
            let arrow = 16 * self.ui;
            let scroll = self.files.scroll;
            self.files.scroll = if y < sb.y + arrow {
                scroll.saturating_sub(1)
            } else if y >= sb.y + sb.h - arrow {
                (scroll + 1).min(last)
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
        let i = self.files.scroll + ((y - rows.y) / self.files_row_h()) as usize;
        if i >= self.files.entries.len() {
            self.files.selected = None;
            self.files.status = format!("{} items", self.files.entries.len());
            return area;
        }
        self.select(i);
        if double {
            return self.open_selected();
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
        if self.files.confirm.is_some() {
            return match k {
                b'\n' | b'y' | b'Y' => self.command(Command::Yes),
                27 | b'n' | b'N' => self.command(Command::No),
                _ => Rect::EMPTY,
            };
        }
        let count = if self.files.path.is_empty() { self.tile_count() } else { self.files.entries.len() };
        let c = self.files_client();
        let page = if self.files.path.is_empty() { self.tile_columns(&c) } else { self.files_visible(&c) };
        let step = if self.files.path.is_empty() { self.tile_columns(&c) } else { 1 };
        let at = self.files.selected;
        let target = match k {
            8 => return self.go_up(),
            b'\n' => return self.open_selected(),
            KEY_DELETE => return self.command(Command::Delete),
            KEY_F2 => return self.command(Command::Rename),
            KEY_F5 => {
                self.refresh(self.selected_entry().map(|e| e.name));
                return area;
            }
            0x03 => return self.command(Command::Copy),
            0x18 => return self.command(Command::Cut),
            0x16 => return self.command(Command::Paste),
            0x0E => return self.command(Command::NewFolder),
            0x1A => return self.undo_delete(),
            KEY_DOWN => at.map_or(0, |i| i + step),
            KEY_UP => at.map_or(0, |i| i.saturating_sub(step)),
            KEY_RIGHT if self.files.path.is_empty() => at.map_or(0, |i| i + 1),
            KEY_LEFT if self.files.path.is_empty() => at.map_or(0, |i| i.saturating_sub(1)),
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
        self.select(target.min(count - 1));
        area
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
fn clock(started_us: u64) -> (String, String) {
    match aero::now() {
        Some(t) => {
            let hour = match t.hour % 12 {
                0 => 12,
                h => h,
            };
            let half = if t.hour < 12 { "AM" } else { "PM" };
            (format!("{}:{:02} {}", hour, t.minute, half), format!("{}/{}/{}", t.month, t.day, t.year))
        }
        None => (uptime(aero::clock_us() - started_us), String::from("uptime")),
    }
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
    let ui = if w >= 2560 { 2 } else { 1 };
    let s = ui;
    let window = |kind, title, rect| Window { kind, title, rect, open: true, minimized: false, restore: None };
    let windows = alloc::vec![
        window(Kind::Welcome, "Welcome", Rect::new(w / 12, h / 7, 460 * s, 290 * s)),
        window(Kind::System, "System", Rect::new(w / 12 + 480 * s, h / 7 + 30 * s, 300 * s, 190 * s)),
        window(Kind::Notes, "Notes", Rect::new(w / 12 + 160 * s, h / 7 + 200 * s, 500 * s, 290 * s)),
        Window { open: false, ..window(Kind::Computer, "Computer", Rect::new((w - 820 * s).max(130 * s), 40 * s, 800 * s, 480 * s)) },
        Window { open: false, ..window(Kind::Calculator, "Calculator", Rect::new(w / 2 - 40 * s, h / 7 + 10 * s, 260 * s, 350 * s)) },
    ];
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
        peeked: Vec::new(),
        started_us,
        clock: clock(started_us),
        quit: false,
        files: Files {
            path: String::new(),
            entries: Vec::new(),
            drives: Vec::new(),
            scroll: 0,
            selected: None,
            status: String::new(),
            back: Vec::new(),
            forward: Vec::new(),
            sort: SortBy::Name,
            descending: false,
            clip: None,
            rename: None,
            rename_all: false,
            confirm: None,
            bin: Vec::new(),
            last_recycled: None,
            form: None,
        },
        calc: Calc { display: String::from("0"), ..Default::default() },
        icon: None,
        mouse_speed: { saved_network_drives(); saved_mouse_speed() },
        last_press: (0, 0, 0),
    };
    let mut canvas = Canvas { px: alloc::vec![0u32; (w * h) as usize], w, h, clip: Rect::EMPTY, ui };
    present(&screen, &mut canvas, &desk, Rect::new(0, 0, w, h));
    let notes = desk.windows[desk.index(Kind::Notes)].rect;
    let icon = desk.icon_rect(0);
    let calc = desk.icon_rect(2);
    println!("[desktop] up at {}x{}; Notes title bar at {},{}; Computer icon at {},{}; Calculator icon at {},{}", w, h,
        notes.x + 60 * s, notes.y + 12 * s, icon.x + icon.w / 2, icon.y + icon.h / 2, calc.x + calc.w / 2, calc.y + calc.h / 2);

    let mut keys = [0u8; 64];
    let mut tick = 0u64;
    // How long redraws take after the pointer moves, for the log.
    let (mut moves, mut move_us, mut worst_us) = (0u64, 0u64, 0u64);
    while !desk.quit {
        let mut dirty = Rect::EMPTY;
        // The pointer and what it lights up are small; they are redrawn on
        // their own instead of joined into one big rectangle with the rest.
        let mut spots = Damage::default();
        let p = screen.pointer().unwrap_or(desk.pointer);
        if p != desk.pointer {
            let old = desk.pointer;
            spots.add(desk.cursor_rect());
            spots.add(desk.hover_area(old.x as i32, old.y as i32));
            desk.pointer = p;
            spots.add(desk.cursor_rect());
            spots.add(desk.hover_area(p.x as i32, p.y as i32));
            // Both presses of a quick double-click can land between two looks.
            let presses = p.presses.wrapping_sub(old.presses) & 0xFF_FFFF;
            for _ in 0..presses.min(3) {
                dirty = dirty.union(&desk.click(p.x as i32, p.y as i32));
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
            if desk.top_kind() == Some(Kind::System) {
                dirty = dirty.union(&desk.window_area(desk.windows.len() - 1));
            }
        }
        if let Ok(n) = screen.keys(&mut keys) {
            dirty = dirty.union(&desk.typed(&keys[..n]));
        }
        // Twice a second: the clock, the System window and the Notes caret.
        let now = aero::clock_us();
        if now / 500_000 != tick {
            tick = now / 500_000;
            let time = clock(desk.started_us);
            if time != desk.clock {
                desk.clock = time;
                dirty = dirty.union(&desk.tray());
                if desk.menu {
                    dirty = dirty.union(&desk.menu_rect());
                }
            }
            for kind in [Kind::System, Kind::Notes] {
                let at = desk.index(kind);
                if desk.windows[at].shown() {
                    dirty = dirty.union(&desk.window_area(at));
                }
            }
        }
        let moved = !spots.rects.is_empty() && dirty.is_empty();
        spots.add(dirty);
        let t0 = aero::clock_us();
        // Draw each piece, then hand the screen one rectangle covering them
        // all: one present per frame, however many pieces changed.
        let mut shown = Rect::EMPTY;
        for area in spots.rects {
            shown = shown.union(&draw(&mut canvas, &desk, area));
        }
        show(&screen, &canvas, shown);
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
    drop(screen);
    0
}
