//! desktop: a first desktop in the style of Windows 7 (all artwork drawn
//! here, nothing copied), drawn in software.
//!
//! It takes the whole screen and draws a blue wallpaper with icons down the
//! left (Computer, Notes, System: double-click to open), a dark glass
//! taskbar (Start orb, window buttons, a clock with the date and a "show
//! desktop" strip at the right) and glass-framed windows: Welcome, System,
//! Notes and Computer, which browses the disks (double-click a folder to go
//! in, Back or Backspace to go up, a text file to open it in Notes) and
//! Calculator (click its keys or type 0-9 + - * / . = Enter Backspace C).
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
    /// Window text, labels, menus: 20 pixels tall (32 at UI scale 2).
    Normal,
    /// Title bars.
    Bold,
    /// The taskbar clock: 16 pixels tall (24 at UI scale 2).
    Small,
    /// The clock in the Start menu: 32 pixels tall.
    Large,
}

impl Font {
    fn style(self, ui: i32) -> (FontWeight, RasterHeight) {
        let weight = if self == Font::Bold { FontWeight::Bold } else { FontWeight::Regular };
        let size = match (self, ui >= 2) {
            (Font::Small, false) => RasterHeight::Size16,
            (Font::Small, true) => RasterHeight::Size24,
            (Font::Large, _) | (_, true) => RasterHeight::Size32,
            _ => RasterHeight::Size20,
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
                        *p = blend(*p, color, ink as u32);
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

/// What the Computer window shows.
struct Files {
    path: String,
    entries: Vec<aero::DirEntry>,
    /// The first entry shown.
    scroll: usize,
    selected: Option<usize>,
    status: String,
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
        Rect::new(2 * s, t.y - 380 * s, 460 * s, 380 * s)
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
    fn exit_button(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + m.w - 170 * s, m.y + m.h - 40 * s, 160 * s, 30 * s)
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
        Rect::new(4 * s, 10 * s + i as i32 * 94 * s, 96 * s, 86 * s)
    }

    // The Computer window, inside its client area: a toolbar (Back, the
    // address, page up and down), column headings, the rows and a status bar.
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

    fn files_toolbar(&self, client: &Rect) -> Rect {
        Rect::new(client.x, client.y, client.w, 36 * self.ui)
    }
    fn notes_toolbar(&self, client: &Rect) -> Rect {
        Rect::new(client.x, client.y, client.w, 32 * self.ui)
    }
    fn notes_save(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let t = self.notes_toolbar(client);
        Rect::new(t.x + 6 * s, t.y + 4 * s, 64 * s, t.h - 8 * s)
    }
    fn files_back(&self, client: &Rect) -> (i32, i32, i32) {
        let t = self.files_toolbar(client);
        (t.x + 18 * self.ui, t.y + t.h / 2, 12 * self.ui)
    }
    fn files_page(&self, client: &Rect, down: bool) -> Rect {
        let s = self.ui;
        let t = self.files_toolbar(client);
        let x = t.x + t.w - if down { 32 * s } else { 62 * s };
        Rect::new(x, t.y + 6 * s, 28 * s, t.h - 12 * s)
    }
    fn files_address(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let t = self.files_toolbar(client);
        Rect::new(t.x + 36 * s, t.y + 5 * s, t.w - 36 * s - 68 * s, t.h - 10 * s)
    }
    fn files_rows(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let top = client.y + 36 * s + 26 * s;
        Rect::new(client.x, top, client.w, client.y + client.h - 28 * s - top)
    }
    fn files_row_h(&self) -> i32 {
        26 * self.ui
    }
    fn files_visible(&self, client: &Rect) -> usize {
        (self.files_rows(client).h / self.files_row_h()).max(1) as usize
    }
    /// Where entry `i` (counted from the first one shown) is drawn.
    fn files_row(&self, client: &Rect, i: usize) -> Rect {
        let rows = self.files_rows(client);
        Rect::new(rows.x, rows.y + i as i32 * self.files_row_h(), rows.w, self.files_row_h())
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

    fn files_view(&self, c: &mut Canvas, client: &Rect) {
        let s = self.ui;
        let f = &self.files;
        // Toolbar: a light glass strip with Back, the address and paging.
        let t = self.files_toolbar(client);
        c.gradient(t, rgb(245, 250, 255), rgb(215, 230, 245), 255);
        c.fill(Rect::new(t.x, t.y + t.h - 1, t.w, 1), rgb(160, 180, 205));
        let (bx, by, br) = self.files_back(client);
        let can_go_up = f.path != "/";
        if can_go_up {
            c.orb(bx, by, br, rgb(120, 200, 255), rgb(20, 90, 170));
        } else {
            c.orb(bx, by, br, rgb(220, 225, 230), rgb(150, 160, 170));
        }
        let d = 5 * s;
        c.line(bx - d, by, bx + d - s, by, 2 * s, 0xFFFFFF);
        c.line(bx - d, by, bx - s, by - d + s, 2 * s, 0xFFFFFF);
        c.line(bx - d, by, bx - s, by + d - s, 2 * s, 0xFFFFFF);
        let a = self.files_address(client);
        c.fill(a, 0xFFFFFF);
        c.frame(a, rgb(130, 150, 180));
        let max = ((a.w - 30 * s) / Font::Normal.w(s)).max(1) as usize;
        let skip = f.path.chars().count().saturating_sub(max);
        let shown: String = f.path.chars().skip(skip).collect();
        c.folder(a.x + 5 * s, a.y + a.h / 2 - 6 * s, s);
        c.text(a.x + 26 * s, a.y + (a.h - Font::Normal.h(s)) / 2, &shown, 0x101010, Font::Normal);
        let more_above = f.scroll > 0;
        let more_below = f.scroll + self.files_visible(client) < f.entries.len();
        for (down, live) in [(false, more_above), (true, more_below)] {
            let b = self.files_page(client, down);
            let (top, bottom) = if live { (rgb(250, 252, 255), rgb(200, 220, 240)) } else { (rgb(240, 240, 240), rgb(225, 225, 225)) };
            c.rounded(b, 3 * s, false, top, bottom, 255);
            c.rounded_outline(b, 3 * s, false, rgb(140, 160, 190), 255);
            let ink = if live { rgb(30, 60, 110) } else { rgb(170, 170, 170) };
            let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
            for i in 0..4 * s {
                let half = if down { 4 * s - i } else { i };
                c.fill(Rect::new(cx - half, cy - 2 * s + i, 2 * half + 1, 1), ink);
            }
        }

        // Column headings.
        let size_x = client.x + client.w - 90 * s;
        let head = Rect::new(client.x, t.y + t.h, client.w, 26 * s);
        c.fill(head, 0xFFFFFF);
        let head_y = head.y + (head.h - Font::Normal.h(s)) / 2;
        c.text(client.x + 30 * s, head_y, "Name", rgb(60, 80, 110), Font::Normal);
        c.text(size_x, head_y, "Size", rgb(60, 80, 110), Font::Normal);
        c.fill(Rect::new(size_x - 8 * s, head.y + 3 * s, 1, head.h - 6 * s), rgb(220, 225, 235));
        c.fill(Rect::new(head.x, head.y + head.h - 1, head.w, 1), rgb(225, 230, 240));

        let rows = self.files_rows(client);
        c.fill(rows, 0xFFFFFF);
        let name_cols = ((size_x - 16 * s - client.x - 30 * s) / Font::Normal.w(s)).max(1) as usize;
        for (i, e) in f.entries.iter().enumerate().skip(f.scroll).take(self.files_visible(client)) {
            let r = self.files_row(client, i - f.scroll);
            let hover = r.contains(self.pointer.x as i32, self.pointer.y as i32);
            if f.selected == Some(i) {
                c.rounded(Rect::new(r.x + 2 * s, r.y, r.w - 4 * s, r.h), 2 * s, false, rgb(220, 236, 252), rgb(196, 222, 250), 255);
                c.rounded_outline(Rect::new(r.x + 2 * s, r.y, r.w - 4 * s, r.h), 2 * s, false, rgb(125, 162, 206), 255);
            } else if hover {
                c.rounded(Rect::new(r.x + 2 * s, r.y, r.w - 4 * s, r.h), 2 * s, false, rgb(240, 247, 254), rgb(228, 240, 252), 255);
            }
            if e.is_dir {
                c.folder(r.x + 8 * s, r.y + (r.h - 12 * s) / 2, s);
            } else {
                c.page(r.x + 10 * s, r.y + (r.h - 14 * s) / 2, s);
            }
            let name: String = if e.name.chars().count() > name_cols {
                let mut n: String = e.name.chars().take(name_cols.saturating_sub(3)).collect();
                n.push_str("...");
                n
            } else {
                e.name.clone()
            };
            let ty = r.y + (r.h - Font::Normal.h(s)) / 2;
            c.text(client.x + 30 * s, ty, &name, 0x101010, Font::Normal);
            if !e.is_dir {
                c.text(size_x, ty, &size_text(e.size), rgb(80, 80, 80), Font::Normal);
            }
        }

        // Status bar.
        let bar = Rect::new(client.x, rows.y + rows.h, client.w, client.y + client.h - rows.y - rows.h);
        c.gradient(bar, rgb(240, 245, 252), rgb(215, 228, 242), 255);
        c.fill(Rect::new(bar.x, bar.y, bar.w, 1), rgb(180, 195, 215));
        c.text(bar.x + 8 * s, bar.y + (bar.h - Font::Normal.h(s)) / 2, &f.status, rgb(30, 50, 80), Font::Normal);
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
        c.text(l.x + 12 * s, l.y + l.h - Font::Normal.h(s) - 8 * s, "All programs are listed", rgb(90, 90, 90), Font::Normal);
        // Right: the date in large type, and the exit button.
        let rx = l.x + l.w + 14 * s;
        c.text(rx, m.y + 16 * s, &self.clock.0, 0xFFFFFF, Font::Large);
        c.text(rx, m.y + 56 * s, &self.clock.1, rgb(210, 230, 250), Font::Normal);
        c.text(rx, m.y + 84 * s, "AeroForge", rgb(210, 230, 250), Font::Normal);
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

    /// What the pointer lights up: caption buttons, taskbar buttons, menu items.
    fn hover_area(&self, x: i32, y: i32) -> Rect {
        let mut area = Rect::EMPTY;
        if let Some(top) = self.windows.iter().rposition(|w| w.shown() && w.rect.contains(x, y)) {
            let r = self.windows[top].rect;
            for which in [Caption::Minimize, Caption::Maximize, Caption::Close] {
                area = area.union(&self.caption(&r, which));
            }
        }
        if self.taskbar().contains(x, y) {
            area = area.union(&self.taskbar());
        }
        for i in 0..ICONS.len() {
            if self.icon_rect(i).contains(x, y) {
                area = area.union(&self.icon_rect(i));
            }
        }
        let at = self.index(Kind::Computer);
        if self.windows[at].shown() {
            let client = self.client(&self.windows[at].rect);
            if client.contains(x, y) {
                area = area.union(&client);
            }
        }
        let at = self.index(Kind::Notes);
        if self.windows[at].shown() {
            let save = self.notes_save(&self.client(&self.windows[at].rect));
            area = area.union(&save);
        }
        if self.menu {
            area = area.union(&self.menu_rect());
        }
        area
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

    fn calc_key_pressed(&mut self, key: u8) {
        if let Some(result) = self.calc.key(key) {
            println!("[desktop] calculator: {}", result);
        }
    }

    /// Opens a window from its icon or the Start menu.
    fn open(&mut self, kind: Kind) -> Rect {
        let at = self.index(kind);
        if kind == Kind::Computer && !self.windows[at].open {
            self.open_dir(String::from("/"));
        }
        let r = self.windows[at].rect;
        println!("[desktop] opened {} at {},{} ({}x{})", self.windows[at].title, r.x, r.y, r.w, r.h);
        if kind == Kind::Calculator {
            let k = self.calc_key(&self.client(&r), 3, 3);
            println!("[desktop] Calculator = key at {},{}", k.x + k.w / 2, k.y + k.h / 2);
        }
        self.bring(kind)
    }

    /// The Computer window's area, to redraw it.
    fn files_area(&self) -> Rect {
        self.window_area(self.index(Kind::Computer))
    }

    /// Shows the directory `path` in the Computer window.
    fn open_dir(&mut self, path: String) {
        match aero::list_dir(&path) {
            Ok(mut entries) => {
                // Folders first, then files, each by name, ignoring case.
                entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                let n = entries.len();
                let status = format!("{} item{}", n, if n == 1 { "" } else { "s" });
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                let at = self.index(Kind::Computer);
                let row = self.files_row(&self.client(&self.windows[at].rect), 0);
                println!("[desktop] Computer: {} = {}; first row at {},{}, rows {} apart",
                    path, names.join(" | "), row.x + 40 * self.ui, row.y + row.h / 2, row.h);
                self.files = Files { path, entries, scroll: 0, selected: None, status };
            }
            Err(e) => {
                println!("[desktop] Computer: cannot open {} ({})", path, e);
                self.files.status = format!("Can't open {}", path);
            }
        }
    }

    /// The folder above the one shown.
    fn go_up(&mut self) -> Rect {
        if self.files.path == "/" {
            return Rect::EMPTY;
        }
        let parent = match self.files.path.trim_end_matches('/').rfind('/') {
            Some(0) | None => String::from("/"),
            Some(i) => String::from(&self.files.path[..i]),
        };
        self.open_dir(parent);
        self.files_area()
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
        self.notes_status = match aero::write_file(&path, data) {
            Ok(_) => {
                let mut back = alloc::vec![0u8; data.len() + 1];
                let same = matches!(aero::read_file(&path, &mut back), Ok(n) if &back[..n] == data);
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
                    _ => "Can't save that file",
                })
            }
        };
        self.window_area(self.index(Kind::Notes))
    }

    /// Opens a text file in Notes.
    fn open_file(&mut self, path: String) -> Rect {
        let mut buf = alloc::vec![0u8; 4096];
        let n = match aero::read_file(&path, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                println!("[desktop] cannot read {} ({})", path, e);
                self.files.status = String::from("Can't read that file");
                return self.files_area();
            }
        };
        let text = &buf[..n];
        if text.iter().any(|&b| b == 0 || (b < 32 && b != b'\n' && b != b'\r' && b != b'\t')) {
            self.files.status = String::from("Notes can only open text files");
            return self.files_area();
        }
        self.notes = String::from_utf8_lossy(text).replace('\r', "").replace('\t', "    ");
        // Only the start of a long file fits; saving that would cut the file.
        self.notes_cut = n == buf.len() || self.notes.len() > 3000;
        while self.notes.len() > 3000 {
            self.notes.pop();
        }
        println!("[desktop] opened {} in Notes ({} bytes)", path, n);
        self.notes_status = String::from(if self.notes_cut { "Too long to save" } else { "" });
        self.notes_path = Some(path);
        self.files_area().union(&self.bring(Kind::Notes))
    }

    /// A click inside the Computer window.
    fn files_click(&mut self, x: i32, y: i32, double: bool) -> Rect {
        let at = self.index(Kind::Computer);
        let client = self.client(&self.windows[at].rect);
        let area = self.files_area();
        let (bx, by, br) = self.files_back(&client);
        if (x - bx) * (x - bx) + (y - by) * (y - by) <= br * br {
            return self.go_up();
        }
        let page = self.files_visible(&client);
        if self.files_page(&client, false).contains(x, y) {
            self.files.scroll = self.files.scroll.saturating_sub(page);
            return area;
        }
        if self.files_page(&client, true).contains(x, y) {
            if self.files.scroll + page < self.files.entries.len() {
                self.files.scroll += page;
            }
            return area;
        }
        if !self.files_rows(&client).contains(x, y) {
            return Rect::EMPTY;
        }
        let i = self.files.scroll + ((y - self.files_rows(&client).y) / self.files_row_h()) as usize;
        let Some(entry) = self.files.entries.get(i).cloned() else {
            self.files.selected = None;
            return area;
        };
        self.files.selected = Some(i);
        if !double {
            self.files.status = if entry.is_dir { entry.name.clone() } else { format!("{}  {}", entry.name, size_text(entry.size)) };
            return area;
        }
        let path = if self.files.path == "/" { format!("/{}", entry.name) } else { format!("{}/{}", self.files.path, entry.name) };
        if entry.is_dir {
            self.open_dir(path);
            area
        } else {
            self.open_file(path)
        }
    }

    fn typed(&mut self, keys: &[u8]) -> Rect {
        let mut dirty = Rect::EMPTY;
        for &k in keys {
            if k == 27 {
                self.quit = true;
                continue;
            }
            if self.top_kind() == Some(Kind::Computer) && k == 8 {
                dirty = dirty.union(&self.go_up());
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

fn present(screen: &Screen, canvas: &mut Canvas, desk: &Desktop, area: Rect) {
    let area = area.intersect(&Rect::new(0, 0, canvas.w, canvas.h));
    if area.is_empty() {
        return;
    }
    canvas.clip = area;
    desk.draw(canvas);
    let _ = screen.present(&canvas.px, canvas.w as usize, area.x as usize, area.y as usize, area.w as usize, area.h as usize);
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
        window(Kind::Welcome, "Welcome", Rect::new(w / 12, h / 7, 400 * s, 250 * s)),
        window(Kind::System, "System", Rect::new(w / 12 + 420 * s, h / 7 + 30 * s, 260 * s, 160 * s)),
        window(Kind::Notes, "Notes", Rect::new(w / 12 + 160 * s, h / 7 + 200 * s, 420 * s, 240 * s)),
        Window { open: false, ..window(Kind::Computer, "Computer", Rect::new(w - 540 * s, 40 * s, 520 * s, 360 * s)) },
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
        files: Files { path: String::from("/"), entries: Vec::new(), scroll: 0, selected: None, status: String::new() },
        calc: Calc { display: String::from("0"), ..Default::default() },
        icon: None,
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
    while !desk.quit {
        let mut dirty = Rect::EMPTY;
        let p = screen.pointer().unwrap_or(desk.pointer);
        if p != desk.pointer {
            let old = desk.pointer;
            dirty = dirty.union(&desk.cursor_rect()).union(&desk.hover_area(old.x as i32, old.y as i32));
            desk.pointer = p;
            dirty = dirty.union(&desk.cursor_rect()).union(&desk.hover_area(p.x as i32, p.y as i32));
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
        present(&screen, &mut canvas, &desk, dirty);
        aero::sleep_ms(10);
    }
    println!("[desktop] notes: {}", desk.notes.replace('\n', " / "));
    println!("[desktop] closed, screen given back to the console");
    drop(screen);
    0
}
