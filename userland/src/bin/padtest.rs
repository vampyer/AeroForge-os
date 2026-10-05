//! padtest: lists the gamepads through the gamepad system call and shows
//! each one in the Xbox 360 layout, as a game would read it.

#![no_std]
#![no_main]

use core::fmt::Write;

use aero::gamepad;

aero::entry!(main);

fn main() -> i64 {
    let mut index = 0;
    while let Ok((s, count)) = gamepad::read(index) {
        let link = if s.link == 2 { "USB" } else { "Bluetooth" };
        let layout = if s.exact == 1 { "Xbox layout" } else { "Xbox layout guessed" };
        let mut w = aero::LineWriter::new();
        let _ = write!(w, "[padtest] {}/{} \"{}\" ({}, {}{}):", index + 1, count, s.name(), link, layout,
            if s.connected == 1 { "" } else { ", not connected" });
        let mut any = false;
        for (bit, name) in gamepad::BUTTON_NAMES {
            if s.xbuttons & bit != 0 {
                let _ = write!(w, " {}", name);
                any = true;
            }
        }
        if !any {
            let _ = write!(w, " no buttons");
        }
        let mut dpad = [""; 2];
        let mut k = 0;
        for (bit, name) in [(gamepad::DPAD_UP, "up"), (gamepad::DPAD_DOWN, "down"), (gamepad::DPAD_LEFT, "left"),
            (gamepad::DPAD_RIGHT, "right")] {
            if s.xbuttons & bit != 0 && k < 2 {
                dpad[k] = name;
                k += 1;
            }
        }
        match k {
            0 => {
                let _ = write!(w, ", d-pad centred");
            }
            1 => {
                let _ = write!(w, ", d-pad {}", dpad[0]);
            }
            _ => {
                let _ = write!(w, ", d-pad {}-{}", dpad[0], dpad[1]);
            }
        }
        let _ = writeln!(w, ", left stick {:+} {:+}, right stick {:+} {:+}, triggers {} {}", s.thumbs[0], s.thumbs[1],
            s.thumbs[2], s.thumbs[3], s.left_trigger, s.right_trigger);
        w.flush();
        index += 1;
    }
    aero::println!("[padtest] {} gamepad(s)", index);
    0
}
