//! drawtest: takes the whole screen, draws on it and gives it back.
//!
//! It fills the four quarters of the screen red, green, blue and white,
//! then changes one 64x64 square to yellow by presenting only that square.
//! It checks that a frame at a bad address is refused, holds the screen
//! for three seconds (the boot test takes a screenshot then), releases it
//! and checks that presenting afterwards is refused. The boot test takes a
//! second screenshot to see the console back.

#![no_std]
#![no_main]

extern crate alloc;

use aero::{display, println, sys, syscall, E_FAULT};

aero::entry!(main);

const RED: u32 = 0xFF0000;
const GREEN: u32 = 0x00FF00;
const BLUE: u32 = 0x0000FF;
const WHITE: u32 = 0xFFFFFF;
const YELLOW: u32 = 0xFFFF00;

fn fail(what: &str, e: i64) -> i64 {
    println!("[drawtest] FAILED: {} ({})", what, e);
    1
}

fn main() -> i64 {
    let screen = match display::acquire() {
        Ok(s) => s,
        Err(e) => return fail("could not take the screen", e),
    };
    let (w, h) = (screen.width, screen.height);
    let mut image = alloc::vec![0u32; w * h];
    for y in 0..h {
        for x in 0..w {
            image[y * w + x] = match (x < w / 2, y < h / 2) {
                (true, true) => RED,
                (false, true) => GREEN,
                (true, false) => BLUE,
                (false, false) => WHITE,
            };
        }
    }
    let t0 = aero::clock_us();
    if let Err(e) = screen.present_all(&image) {
        return fail("full-screen present", e);
    }
    let full_us = aero::clock_us() - t0;

    // Only the square is presented: the rest of the screen must not change.
    for y in 100..164 {
        for x in 100..164 {
            image[y * w + x] = YELLOW;
        }
    }
    if let Err(e) = screen.present(&image, w, 100, 100, 64, 64) {
        return fail("partial present", e);
    }

    let rect = (w as u64) << 32 | (h as u64) << 48;
    let r = unsafe { syscall(sys::DISPLAY_PRESENT, 0xDEAD_0000, w as u64, rect, 0) };
    if r != E_FAULT {
        return fail("a frame at a bad address was not refused", r);
    }

    println!("[drawtest] holding the screen ({}x{}, full frame in {} us)", w, h, full_us);
    aero::sleep_ms(3000);
    drop(screen);

    let r = unsafe { syscall(sys::DISPLAY_PRESENT, image.as_ptr() as u64, w as u64, rect, 0) };
    if r >= 0 {
        return fail("presenting after release was not refused", r);
    }
    println!("[drawtest] full-screen frame, partial update, bad frame refused, screen given back: OK");
    0
}
