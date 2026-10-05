//! The PC's battery-backed real-time clock (CMOS RTC), read for the wall
//! clock time that files get stamped with. No time zones yet: the clock is
//! taken as local time, as Windows keeps it.

use crate::arch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn cmos(reg: u8) -> u8 {
    unsafe {
        arch::outb(0x70, 0x80 | reg); // bit 7 keeps NMIs masked during the access
        arch::inb(0x71)
    }
}

fn read_raw() -> [u8; 7] {
    // Wait out an update in progress so the fields belong to the same second.
    for _ in 0..100_000 {
        if cmos(0x0A) & 0x80 == 0 {
            break;
        }
        core::hint::spin_loop();
    }
    [cmos(0x00), cmos(0x02), cmos(0x04), cmos(0x07), cmos(0x08), cmos(0x09), cmos(0x32)]
}

/// The current date and time, or None if the clock reads as nonsense.
pub fn now() -> Option<DateTime> {
    let (raw, status_b) = arch::without_interrupts(|| {
        // Read until two reads agree, in case an update slipped in between.
        let mut a = read_raw();
        for _ in 0..5 {
            let b = read_raw();
            if a == b {
                break;
            }
            a = b;
        }
        (a, cmos(0x0B))
    });
    let bcd = status_b & 0x04 == 0;
    let dec = |v: u8| if bcd { (v & 0x0F) + (v >> 4) * 10 } else { v };
    let mut hour = dec(raw[2] & 0x7F);
    if status_b & 0x02 == 0 && raw[2] & 0x80 != 0 {
        hour = (hour % 12) + 12; // 12-hour clock, PM
    } else if status_b & 0x02 == 0 && hour == 12 {
        hour = 0; // 12 AM
    }
    let century = match dec(raw[6]) {
        c @ 19..=21 => c as u16,
        _ => 20,
    };
    let t = DateTime {
        year: century * 100 + dec(raw[5]) as u16,
        month: dec(raw[4]),
        day: dec(raw[3]),
        hour,
        minute: dec(raw[1]),
        second: dec(raw[0]),
    };
    let valid = (1..=12).contains(&t.month) && (1..=31).contains(&t.day) && t.hour < 24 && t.minute < 60 && t.second < 60;
    valid.then_some(t)
}
