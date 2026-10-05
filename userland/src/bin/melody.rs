//! melody: plays a short tune through the audio system calls, to show that
//! programs can make sound. Each note is a sine wave with a short fade in
//! and out, followed by a short gap.

#![no_std]
#![no_main]

use aero::audio;

aero::entry!(main);

/// C5, E5, G5, C6 (Hz), 0.3 s each.
const NOTES: [u32; 4] = [523, 659, 784, 1047];
const NOTE_MS: u32 = 300;
const GAP_MS: u32 = 60;

/// sin(x) for x in any range, without libm (Taylor series after folding).
fn sin(x: f32) -> f32 {
    const PI: f32 = core::f32::consts::PI;
    let mut x = x % (2.0 * PI);
    if x > PI {
        x -= 2.0 * PI;
    } else if x < -PI {
        x += 2.0 * PI;
    }
    if x > PI / 2.0 {
        x = PI - x;
    } else if x < -PI / 2.0 {
        x = -PI - x;
    }
    let x2 = x * x;
    x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))))
}

fn main() -> i64 {
    let mut buf = [0i16; 2 * 960]; // 20 ms
    for &hz in NOTES.iter() {
        let frames = audio::RATE * NOTE_MS / 1000;
        let fade = audio::RATE / 200;
        let step = 2.0 * core::f32::consts::PI * hz as f32 / audio::RATE as f32;
        let mut k = 0u32;
        while k < frames {
            let n = (frames - k).min(960);
            for i in 0..n {
                let t = k + i;
                let ramp = (t.min(frames - 1 - t) as f32 / fade as f32).min(1.0);
                // Phase from the frame number keeps the wave exact over the note.
                let v = (8000.0 * ramp * sin(step * t as f32)) as i16;
                buf[2 * i as usize] = v;
                buf[2 * i as usize + 1] = v;
            }
            if let Err(e) = audio::write_all(&buf[..2 * n as usize]) {
                aero::println!("[melody] no sound output ({})", e);
                return 1;
            }
            k += n;
        }
        let silence = [0i16; 2 * 960];
        let gap = audio::RATE * GAP_MS / 1000;
        let mut g = 0;
        while g < gap {
            let n = (gap - g).min(960);
            let _ = audio::write_all(&silence[..2 * n as usize]);
            g += n;
        }
    }
    audio::drain();
    aero::println!("[melody] played {} notes", NOTES.len());
    0
}
