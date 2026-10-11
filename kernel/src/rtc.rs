//! The PC's battery-backed real-time clock (CMOS RTC), read for the wall
//! clock time that files get stamped with. No time zones yet: the clock is
//! taken as local time, as Windows keeps it.

use crate::arch;

static CMOS: spin::Mutex<()> = spin::Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    /// As one number, YYYYMMDDhhmmss in decimal digits (what directory
    /// listings carry; 0 means unknown).
    pub fn packed(&self) -> u64 {
        ((((self.year as u64 * 100 + self.month as u64) * 100 + self.day as u64) * 100 + self.hour as u64) * 100
            + self.minute as u64)
            * 100
            + self.second as u64
    }

    /// From FAT's (and exFAT's) date and time words.
    pub fn from_dos(date: u16, time: u16) -> Option<Self> {
        let t = DateTime {
            year: 1980 + (date >> 9),
            month: (date >> 5 & 0xF) as u8,
            day: (date & 0x1F) as u8,
            hour: (time >> 11) as u8,
            minute: (time >> 5 & 0x3F) as u8,
            second: ((time & 0x1F) * 2) as u8,
        };
        ((1..=12).contains(&t.month) && (1..=31).contains(&t.day) && t.hour < 24 && t.minute < 60).then_some(t)
    }

    /// From an NTFS time: 100 ns steps since 1601-01-01 (UTC).
    pub fn from_filetime(ft: u64) -> Option<Self> {
        if ft == 0 {
            return None;
        }
        let secs = ft / 10_000_000;
        let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
        // Days since 1601-01-01 to a civil date (Howard Hinnant's method,
        // counted from 0000-03-01): 1601-01-01 is day 584_694 there.
        let z = days + 584_694;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = (yoe + era * 400 + if month <= 2 { 1 } else { 0 }) as u16;
        Some(DateTime { year, month, day, hour: (rest / 3600) as u8, minute: (rest / 60 % 60) as u8, second: (rest % 60) as u8 })
    }
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
        // One CPU at a time: the index and data ports are a pair.
        let _cmos = CMOS.lock();
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
