// AeroForge OS: the f32 functions std has and core lacks, for the decoder's
// setup (windows, floor 0) where they are not hot. sqrt is the SSE2
// instruction; sin, cos, exp, atan and ln are series in f64, accurate to
// well under f32 rounding over the ranges a decoder uses.

pub trait F32Ext {
	fn sqrt(self) -> f32;
	fn floor(self) -> f32;
	fn sin(self) -> f32;
	fn cos(self) -> f32;
	fn exp(self) -> f32;
	fn atan(self) -> f32;
	fn exp2(self) -> f32;
}

const PI: f64 = core::f64::consts::PI;

fn floor64(x: f64) -> f64 {
	let t = x as i64 as f64;
	if t > x { t - 1.0 } else { t }
}

fn sin64(x: f64) -> f64 {
	// Into [-pi, pi], then [-pi/2, pi/2].
	let mut x = x - 2.0 * PI * floor64(x / (2.0 * PI) + 0.5);
	if x > PI / 2.0 {
		x = PI - x;
	} else if x < -PI / 2.0 {
		x = -PI - x;
	}
	let x2 = x * x;
	let mut term = x;
	let mut sum = x;
	let mut k = 1.0;
	while k < 25.0 {
		term *= -x2 / ((k + 1.0) * (k + 2.0));
		sum += term;
		k += 2.0;
	}
	sum
}

fn exp64(x: f64) -> f64 {
	// e^x = 2^n * e^r, |r| <= ln2/2.
	let ln2 = core::f64::consts::LN_2;
	let n = floor64(x / ln2 + 0.5);
	let r = x - n * ln2;
	let mut term = 1.0;
	let mut sum = 1.0;
	let mut k = 1.0;
	while k < 20.0 {
		term *= r / k;
		sum += term;
		k += 1.0;
	}
	let n = n.max(-1000.0).min(1000.0) as i32;
	let mut p = 1.0;
	let b = if n >= 0 { 2.0 } else { 0.5 };
	for _ in 0..n.abs() {
		p *= b;
	}
	sum * p
}

fn atan64(x: f64) -> f64 {
	// atan(x) = pi/2 - atan(1/x) for |x| > 1; then halve the angle twice:
	// atan(x) = 2 atan(x / (1 + sqrt(1 + x^2))).
	if x < 0.0 {
		return -atan64(-x);
	}
	if x > 1.0 {
		return PI / 2.0 - atan64(1.0 / x);
	}
	let mut y = x;
	for _ in 0..2 {
		y = y / (1.0 + sqrt64(1.0 + y * y));
	}
	let y2 = y * y;
	let mut term = y;
	let mut sum = y;
	let mut k = 1.0;
	while k < 40.0 {
		term *= -y2;
		sum += term / (k + 2.0);
		k += 2.0;
	}
	4.0 * sum
}

fn exp2_64(x: f64) -> f64 {
	// Exact for whole numbers (float32_unpack's case), from the bits.
	let n = x as i64;
	if n as f64 == x && n >= -1022 && n <= 1023 {
		return f64::from_bits(((n + 1023) as u64) << 52);
	}
	if n as f64 == x && n < -1022 {
		return 0.0;
	}
	exp64(x * core::f64::consts::LN_2)
}

#[allow(unsafe_code)]
fn sqrt64(x: f64) -> f64 {
	use core::arch::x86_64::{_mm_cvtsd_f64, _mm_set_sd, _mm_sqrt_pd};
	unsafe { _mm_cvtsd_f64(_mm_sqrt_pd(_mm_set_sd(x))) }
}

impl F32Ext for f32 {
	fn sqrt(self) -> f32 { sqrt64(self as f64) as f32 }
	fn floor(self) -> f32 { floor64(self as f64) as f32 }
	fn sin(self) -> f32 { sin64(self as f64) as f32 }
	fn cos(self) -> f32 { sin64(self as f64 + PI / 2.0) as f32 }
	fn exp(self) -> f32 { exp64(self as f64) as f32 }
	fn atan(self) -> f32 { atan64(self as f64) as f32 }
	fn exp2(self) -> f32 { exp2_64(self as f64) as f32 }
}
