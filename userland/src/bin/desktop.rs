//! desktop: a first desktop in the style of Windows 7 (all artwork drawn
//! here, nothing copied), drawn in software.
//!
//! It takes the whole screen and draws a blue wallpaper with icons down the
//! left (Computer, Notes, System: double-click to open), a dark glass
//! taskbar (Start orb, window buttons, a clock with the date and a "show
//! desktop" strip at the right) and glass-framed windows: Welcome, System,
//! Notes and Computer, which browses the disks (double-click a folder to go
//! in, Back or Backspace to go up, a text file to open it in Notes).
//! The mouse moves a pointer; a window comes to the front
//! when clicked, moves when dragged by its title bar and changes size when
//! dragged by an edge or corner; its caption
//! buttons minimize, maximize and close it; taskbar buttons switch between
//! windows. The Start menu lists the programs and has "Exit to console";
//! Esc also gives the screen back to the shell. Keys typed while Notes is
//! in front go into it.
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
use font8x8::legacy::BASIC_LEGACY;

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

/// The frame being built, clipped to the area being redrawn.
struct Canvas {
    px: Vec<u32>,
    w: i32,
    h: i32,
    clip: Rect,
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

    fn text(&mut self, x: i32, y: i32, s: &str, color: u32, scale: i32) {
        for (i, ch) in s.bytes().enumerate() {
            let glyph = BASIC_LEGACY[if ch < 128 { ch as usize } else { b'?' as usize }];
            let gx = x + i as i32 * 8 * scale;
            if !Rect::new(gx, y, 8 * scale, 8 * scale).intersect(&self.clip).is_empty() {
                for (row, bits) in glyph.iter().enumerate() {
                    for col in 0..8 {
                        if bits & (1 << col) != 0 {
                            self.fill(Rect::new(gx + col * scale, y + row as i32 * scale, scale, scale), color);
                        }
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
    fn glow_text(&mut self, x: i32, y: i32, s: &str, color: u32, scale: i32) {
        let w = s.len() as i32 * 8 * scale;
        let pad = 4 * scale;
        self.rounded(Rect::new(x - pad, y - pad, w + 2 * pad, 8 * scale + 2 * pad), 4 * scale, false, 0xFFFFFF, 0xFFFFFF, 90);
        self.text(x, y, s, color, scale);
    }
}

impl Canvas {
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
}

const KINDS: [Kind; 4] = [Kind::Computer, Kind::Notes, Kind::System, Kind::Welcome];

/// Desktop icons, top to bottom.
const ICONS: [Kind; 3] = [Kind::Computer, Kind::Notes, Kind::System];

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
    pointer: Pointer,
    drag: Option<(usize, i32, i32)>, // window, grab offset
    /// The window being resized, which edges move, and where it and the
    /// pointer were when the drag began.
    resize: Option<(usize, Edges, Rect, i32, i32)>,
    menu: bool,
    /// Windows hidden by "Show desktop", to bring back on the next click.
    peeked: Vec<Kind>,
    started_us: u64,
    clock: (String, String),
    quit: bool,
    files: Files,
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
        (t.x + 27 * self.ui, t.y + t.h / 2, 17 * self.ui)
    }
    fn show_desktop(&self) -> Rect {
        let t = self.taskbar();
        Rect::new(t.x + t.w - 14 * self.ui, t.y, 14 * self.ui, t.h)
    }
    fn tray(&self) -> Rect {
        let t = self.taskbar();
        let w = 90 * self.ui;
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
        Rect::new(2 * s, t.y - 360 * s, 400 * s, 360 * s)
    }
    fn menu_left(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + 8 * s, m.y + 8 * s, 240 * s, m.h - 50 * s)
    }
    fn menu_item(&self, i: usize) -> Rect {
        let l = self.menu_left();
        let s = self.ui;
        Rect::new(l.x + 4 * s, l.y + 6 * s + i as i32 * 36 * s, l.w - 8 * s, 32 * s)
    }
    fn exit_button(&self) -> Rect {
        let m = self.menu_rect();
        let s = self.ui;
        Rect::new(m.x + m.w - 140 * s, m.y + m.h - 36 * s, 130 * s, 26 * s)
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
        Rect::new(8 * s, 10 * s + i as i32 * 88 * s, 76 * s, 80 * s)
    }

    // The Computer window, inside its client area: a toolbar (Back, the
    // address, page up and down), column headings, the rows and a status bar.
    fn files_toolbar(&self, client: &Rect) -> Rect {
        Rect::new(client.x, client.y, client.w, 30 * self.ui)
    }
    fn files_back(&self, client: &Rect) -> (i32, i32, i32) {
        let t = self.files_toolbar(client);
        (t.x + 17 * self.ui, t.y + t.h / 2, 11 * self.ui)
    }
    fn files_page(&self, client: &Rect, down: bool) -> Rect {
        let s = self.ui;
        let t = self.files_toolbar(client);
        let x = t.x + t.w - if down { 30 * s } else { 58 * s };
        Rect::new(x, t.y + 5 * s, 26 * s, t.h - 10 * s)
    }
    fn files_address(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let t = self.files_toolbar(client);
        Rect::new(t.x + 34 * s, t.y + 5 * s, t.w - 34 * s - 64 * s, t.h - 10 * s)
    }
    fn files_rows(&self, client: &Rect) -> Rect {
        let s = self.ui;
        let top = client.y + 30 * s + 20 * s;
        Rect::new(client.x, top, client.w, client.y + client.h - 22 * s - top)
    }
    fn files_row_h(&self) -> i32 {
        18 * self.ui
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

        c.glow_text(r.x + 12 * s, r.y + 11 * s, win.title, 0x000000, s);
        for which in [Caption::Minimize, Caption::Maximize, Caption::Close] {
            self.caption_button(c, &r, which);
        }

        let client = self.client(&r);
        c.fill(Rect::new(client.x - 1, client.y - 1, client.w + 2, client.h + 2), rgb(90, 110, 140));
        c.fill(client, 0xFFFFFF);
        let (tx, mut ty) = (client.x + 10 * s, client.y + 10 * s);
        let line = 14 * s;
        let mut say = |c: &mut Canvas, text: &str, color: u32| {
            c.text(tx, ty, text, color, s);
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
            Kind::Notes => {
                let cols = ((client.w - 20 * s) / (8 * s)).max(1) as usize;
                let rows = ((client.h - 20 * s) / line).max(1) as usize;
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
                for l in &lines[skip..] {
                    say(c, l, 0x101010);
                }
            }
            Kind::Computer => self.files_view(c, &client),
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
        let max = ((a.w - 30 * s) / (8 * s)).max(1) as usize;
        let skip = f.path.chars().count().saturating_sub(max);
        let shown: String = f.path.chars().skip(skip).collect();
        c.folder(a.x + 5 * s, a.y + a.h / 2 - 6 * s, s);
        c.text(a.x + 24 * s, a.y + a.h / 2 - 4 * s, &shown, 0x101010, s);
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
        let head = Rect::new(client.x, t.y + t.h, client.w, 20 * s);
        c.fill(head, 0xFFFFFF);
        c.text(client.x + 30 * s, head.y + 6 * s, "Name", rgb(60, 80, 110), s);
        c.text(size_x, head.y + 6 * s, "Size", rgb(60, 80, 110), s);
        c.fill(Rect::new(size_x - 8 * s, head.y + 3 * s, 1, head.h - 6 * s), rgb(220, 225, 235));
        c.fill(Rect::new(head.x, head.y + head.h - 1, head.w, 1), rgb(225, 230, 240));

        let rows = self.files_rows(client);
        c.fill(rows, 0xFFFFFF);
        let name_cols = ((size_x - 16 * s - client.x - 30 * s) / (8 * s)).max(1) as usize;
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
                c.folder(r.x + 8 * s, r.y + 3 * s, s);
            } else {
                c.page(r.x + 10 * s, r.y + 2 * s, s);
            }
            let name: String = if e.name.chars().count() > name_cols {
                let mut n: String = e.name.chars().take(name_cols.saturating_sub(3)).collect();
                n.push_str("...");
                n
            } else {
                e.name.clone()
            };
            c.text(client.x + 30 * s, r.y + 5 * s, &name, 0x101010, s);
            if !e.is_dir {
                c.text(size_x, r.y + 5 * s, &size_text(e.size), rgb(80, 80, 80), s);
            }
        }

        // Status bar.
        let bar = Rect::new(client.x, rows.y + rows.h, client.w, client.y + client.h - rows.y - rows.h);
        c.gradient(bar, rgb(240, 245, 252), rgb(215, 228, 242), 255);
        c.fill(Rect::new(bar.x, bar.y, bar.w, 1), rgb(180, 195, 215));
        c.text(bar.x + 8 * s, bar.y + bar.h / 2 - 4 * s, &f.status, rgb(30, 50, 80), s);
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
                _ => {
                    c.orb(px + 20 * s, py + 19 * s, 17 * s, rgb(140, 210, 255), rgb(20, 90, 170));
                    c.fill(Rect::new(px + 18 * s, py + 9 * s, 4 * s, 4 * s), 0xFFFFFF);
                    c.fill(Rect::new(px + 18 * s, py + 16 * s, 4 * s, 13 * s), 0xFFFFFF);
                }
            }
            let label = self.windows[self.index(kind)].title;
            let lx = r.x + (r.w - label.len() as i32 * 8 * s) / 2;
            let ly = r.y + r.h - 18 * s;
            c.text(lx + s, ly + s, label, 0x000000, s);
            c.text(lx, ly, label, 0xFFFFFF, s);
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

        // Start: a glossy blue orb with a white star of our own.
        let (ox, oy, or) = self.start_button();
        let hot = self.menu || (self.pointer.x as i32 - ox).pow(2) + (self.pointer.y as i32 - oy).pow(2) <= or * or;
        c.orb(ox, oy, or, if hot { rgb(130, 210, 255) } else { rgb(90, 170, 235) }, rgb(10, 60, 140));
        let d = 7 * s;
        c.line(ox - d, oy, ox + d, oy, 2 * s, 0xFFFFFF);
        c.line(ox, oy - d, ox, oy + d, 2 * s, 0xFFFFFF);
        c.line(ox - d / 2, oy - d / 2, ox + d / 2, oy + d / 2, s, 0xFFFFFF);
        c.line(ox + d / 2, oy - d / 2, ox - d / 2, oy + d / 2, s, 0xFFFFFF);

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
            c.text(b.x + 34 * s, b.y + b.h / 2 - 4 * s, win.title, 0xFFFFFF, s);
        }

        // Clock: time over date, then the "show desktop" strip.
        let tray = self.tray();
        let (time, date) = (&self.clock.0, &self.clock.1);
        let cx = |text: &String| tray.x + (tray.w - text.len() as i32 * 8 * s) / 2;
        c.text(cx(time), tray.y + tray.h / 2 - 10 * s, time, 0xFFFFFF, s);
        c.text(cx(date), tray.y + tray.h / 2 + 3 * s, date, 0xFFFFFF, s);
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
        c.rounded(m, 6 * s, false, rgb(60, 110, 170), rgb(10, 35, 70), 215);
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
            c.text(item.x + 40 * s, item.y + item.h / 2 - 4 * s, title, 0x101010, s);
        }
        c.text(l.x + 12 * s, l.y + l.h - 22 * s, "All programs are listed", rgb(90, 90, 90), s);
        // Right: the date in large type, and the exit button.
        let rx = l.x + l.w + 14 * s;
        c.text(rx, m.y + 20 * s, &self.clock.0, 0xFFFFFF, 2 * s);
        c.text(rx, m.y + 44 * s, &self.clock.1, rgb(210, 230, 250), s);
        c.text(rx, m.y + 70 * s, "AeroForge", rgb(210, 230, 250), s);
        let e = self.exit_button();
        let hover = e.contains(self.pointer.x as i32, self.pointer.y as i32);
        c.rounded(e, 3 * s, false, if hover { rgb(250, 180, 150) } else { rgb(230, 150, 120) }, rgb(170, 50, 25), 240);
        c.rounded_outline(e, 3 * s, false, rgb(80, 20, 10), 255);
        c.text(e.x + (e.w - 15 * 8 * s) / 2, e.y + e.h / 2 - 4 * s, "Exit to console", 0xFFFFFF, s);
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
            } else if y < r.y + self.title_h() && self.windows[i].restore.is_none() {
                self.drag = Some((i, x - r.x, y - r.y));
            } else if self.windows[i].kind == Kind::Computer && self.client(&r).contains(x, y) {
                dirty = dirty.union(&self.files_click(x, y, double));
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

    /// Opens a window from its icon or the Start menu.
    fn open(&mut self, kind: Kind) -> Rect {
        let at = self.index(kind);
        if kind == Kind::Computer && !self.windows[at].open {
            self.open_dir(String::from("/"));
        }
        let r = self.windows[at].rect;
        println!("[desktop] opened {} at {},{} ({}x{})", self.windows[at].title, r.x, r.y, r.w, r.h);
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
        while self.notes.len() > 3000 {
            self.notes.pop();
        }
        println!("[desktop] opened {} in Notes ({} bytes)", path, n);
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
            if self.top_kind() != Some(Kind::Notes) {
                continue;
            }
            match k {
                8 => {
                    self.notes.pop();
                }
                b'\n' | 32..=126 => {
                    if self.notes.len() < 4000 {
                        self.notes.push(k as char);
                    }
                }
                _ => {}
            }
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
        window(Kind::Welcome, "Welcome", Rect::new(w / 12, h / 7, 340 * s, 180 * s)),
        window(Kind::System, "System", Rect::new(w / 12 + 370 * s, h / 7 + 30 * s, 270 * s, 130 * s)),
        window(Kind::Notes, "Notes", Rect::new(w / 12 + 160 * s, h / 7 + 200 * s, 360 * s, 180 * s)),
        Window { open: false, ..window(Kind::Computer, "Computer", Rect::new(w - 490 * s, 40 * s, 460 * s, 320 * s)) },
    ];
    let pointer = screen.pointer().unwrap_or_default();
    let started_us = aero::clock_us();
    let mut desk = Desktop {
        w,
        h,
        ui,
        windows,
        notes: String::new(),
        pointer,
        drag: None,
        resize: None,
        menu: false,
        peeked: Vec::new(),
        started_us,
        clock: clock(started_us),
        quit: false,
        files: Files { path: String::from("/"), entries: Vec::new(), scroll: 0, selected: None, status: String::new() },
        icon: None,
        last_press: (0, 0, 0),
    };
    let mut canvas = Canvas { px: alloc::vec![0u32; (w * h) as usize], w, h, clip: Rect::EMPTY };
    present(&screen, &mut canvas, &desk, Rect::new(0, 0, w, h));
    let notes = desk.windows[desk.index(Kind::Notes)].rect;
    let icon = desk.icon_rect(0);
    println!("[desktop] up at {}x{}; Notes title bar at {},{}; Computer icon at {},{}", w, h, notes.x + 60 * s,
        notes.y + 12 * s, icon.x + icon.w / 2, icon.y + icon.h / 2);

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
            if let Some((i, gx, gy)) = desk.drag {
                if p.buttons & 1 != 0 {
                    let before = desk.window_area(i);
                    let r = &mut desk.windows[i].rect;
                    r.x = (p.x as i32 - gx).clamp(-r.w + 60, w - 60);
                    r.y = (p.y as i32 - gy).clamp(0, h - 80);
                    dirty = dirty.union(&before).union(&desk.window_area(i));
                } else {
                    let r = desk.windows[i].rect;
                    println!("[desktop] moved {} to {},{}", desk.windows[i].title, r.x, r.y);
                    desk.drag = None;
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
