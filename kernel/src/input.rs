//! Keyboard and mouse input for the program that owns the screen.
//!
//! While a program has the display (display::acquire), typed keys go to
//! it instead of the shell, and it can read the mouse pointer. Keys are
//! queued; the pointer is a snapshot (position and buttons) plus a count
//! of left-button presses, so a click between two reads is not lost, and a
//! count of the presses that made a double-click, timed here as they happen
//! rather than when a busy program gets round to reading them.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::sync::IrqMutex;
use crate::usb;

const NOBODY: u64 = u64::MAX;
const KEY_QUEUE: usize = 64;

/// Process that owns the screen (and so the keyboard), or NOBODY.
static OWNER: AtomicU64 = AtomicU64::new(NOBODY);
/// Left-button presses since boot (wraps at 20 bits for the syscall).
static PRESSES: AtomicU32 = AtomicU32::new(0);
/// Presses that were the second of a double-click (wraps at 4 bits).
static DOUBLES: AtomicU32 = AtomicU32::new(0);
/// When (microseconds) and where (x | y << 16) the last press that could
/// start a double-click was; 0 after a double-click, so a third press
/// starts over.
static LAST_PRESS_US: AtomicU64 = AtomicU64::new(0);
static LAST_PRESS_AT: AtomicU32 = AtomicU32::new(0);
/// Two presses closer together than this, in time and in pixels either
/// way, are a double-click.
const DOUBLE_CLICK_US: u64 = 500_000;
const DOUBLE_CLICK_PX: i32 = 8;
/// The modifier keys held now (DHI_MOD_*: Shift 1, Ctrl 2, Alt 4, Caps Lock 8),
/// from the last key event of any keyboard.
static MODIFIERS: AtomicU8 = AtomicU8::new(0);

/// A keyboard reported its modifier keys (with every key event).
pub fn modifiers(m: u8) {
    MODIFIERS.store(m & 0xF, Ordering::Relaxed);
}

struct Keys {
    /// Each key: its character, and the modifier keys held as it was pressed (<< 8).
    buf: [u16; KEY_QUEUE],
    head: usize,
    len: usize,
}

static KEYS: IrqMutex<Keys> = IrqMutex::new(Keys { buf: [0; KEY_QUEUE], head: 0, len: 0 });

/// Called by the display when `pid` takes the screen (Some) or gives it back (None).
pub fn set_owner(pid: Option<u64>) {
    let mut k = KEYS.lock();
    k.head = 0;
    k.len = 0;
    OWNER.store(pid.unwrap_or(NOBODY), Ordering::Release);
}

/// Panic path: the shell gets the keys again, without taking the queue lock.
pub fn force_release() {
    OWNER.store(NOBODY, Ordering::Release);
}

/// Whether a program owns the keyboard now.
pub fn owned() -> bool {
    OWNER.load(Ordering::Acquire) != NOBODY
}

/// Queues a typed key for the owner. False if nobody owns the keyboard.
pub fn push_key(c: u8) -> bool {
    if !owned() {
        return false;
    }
    let mut k = KEYS.lock();
    if k.len < KEY_QUEUE {
        let at = (k.head + k.len) % KEY_QUEUE;
        k.buf[at] = c as u16 | (MODIFIERS.load(Ordering::Relaxed) as u16) << 8;
        k.len += 1;
    }
    true
}

/// The mouse buttons changed from `old` to `new`.
pub fn buttons(old: u32, new: u32) {
    if new & 1 != 0 && old & 1 == 0 {
        let now = crate::apic::micros();
        let (x, y) = (usb::MOUSE_X.load(Ordering::Relaxed), usb::MOUSE_Y.load(Ordering::Relaxed));
        let when = LAST_PRESS_US.load(Ordering::Relaxed);
        let at = LAST_PRESS_AT.load(Ordering::Relaxed);
        let (px, py) = ((at & 0xFFFF) as i32, (at >> 16) as i32);
        let double = when != 0
            && now.saturating_sub(when) < DOUBLE_CLICK_US
            && (x - px).abs() <= DOUBLE_CLICK_PX
            && (y - py).abs() <= DOUBLE_CLICK_PX;
        if double {
            DOUBLES.fetch_add(1, Ordering::Relaxed);
            LAST_PRESS_US.store(0, Ordering::Relaxed);
        } else {
            LAST_PRESS_US.store(now.max(1), Ordering::Relaxed);
            LAST_PRESS_AT.store((x.max(0) as u32 & 0xFFFF) | (y.max(0) as u32 & 0xFFFF) << 16, Ordering::Relaxed);
        }
        PRESSES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Keys typed for `pid` into `out` (character | modifiers << 8); returns how
/// many. None if `pid` does not own the screen.
pub fn read_keys(pid: u64, out: &mut [u16]) -> Option<usize> {
    if OWNER.load(Ordering::Acquire) != pid {
        return None;
    }
    let mut k = KEYS.lock();
    let n = out.len().min(k.len);
    for slot in out.iter_mut().take(n) {
        *slot = k.buf[k.head];
        k.head = (k.head + 1) % KEY_QUEUE;
        k.len -= 1;
    }
    Some(n)
}

/// The pointer for `pid`: x | y << 16 | buttons << 32 | modifier keys << 36
/// | presses << 40 | double-clicks << 60. The modifiers ride along so a click can be told apart
/// from a Ctrl+click or Shift+click.
/// None if `pid` does not own the screen.
pub fn pointer(pid: u64) -> Option<u64> {
    if OWNER.load(Ordering::Acquire) != pid {
        return None;
    }
    let x = usb::MOUSE_X.load(Ordering::Relaxed).max(0) as u64 & 0xFFFF;
    let y = usb::MOUSE_Y.load(Ordering::Relaxed).max(0) as u64 & 0xFFFF;
    let b = usb::MOUSE_BUTTONS.load(Ordering::Relaxed) as u64 & 0xF;
    let m = MODIFIERS.load(Ordering::Relaxed) as u64 & 0xF;
    let p = PRESSES.load(Ordering::Relaxed) as u64 & 0xF_FFFF;
    let d = DOUBLES.load(Ordering::Relaxed) as u64 & 0xF;
    Some(x | y << 16 | b << 32 | m << 36 | p << 40 | d << 60)
}
