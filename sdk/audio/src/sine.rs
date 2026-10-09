// SPDX-License-Identifier: GPL-2.0-only
//! The test tone every audio driver plays for `audio tone`, written once.
//!
//! A sine wave from a phase accumulator, in fixed point - no floating point and no table. One full turn
//! of the phase is 2^32. The polynomial is sin's Taylor series to x^7 over a quarter turn, folded to the
//! other three: worst error about 1.6e-4, some 76 dB down - about 2.6 steps of the half-scale 16-bit
//! output, so visible in the samples but far below anything a test tone is listened for.

pub struct Sine {
    phase: u32,
    step: u32,
}

impl Sine {
    /// A sine at `hz`, sampled at `rate` frames a second.
    pub fn new(hz: u32, rate: u32) -> Self {
        Sine { phase: 0, step: (((hz as u64) << 32) / rate.max(1) as u64) as u32 }
    }

    /// The next sample, at half of full scale.
    pub fn next(&mut self) -> i16 {
        const ONE: i64 = 1 << 30;
        const HALF_PI: i64 = 1_686_629_713; // pi/2 in Q30
        let quadrant = self.phase >> 30;
        let frac = (self.phase & 0x3FFF_FFFF) as i64; // a quarter turn, in Q30
        let x = (frac * HALF_PI) >> 30;
        let x = if quadrant & 1 == 1 { HALF_PI - x } else { x };
        let x2 = (x * x) >> 30;
        let mut t = ONE - x2 / 42;
        t = ONE - ((x2 * t) >> 30) / 20;
        t = ONE - ((x2 * t) >> 30) / 6;
        let s = (x * t) >> 30; // sin(x), Q30
        let s = if quadrant >= 2 { -s } else { s };
        self.phase = self.phase.wrapping_add(self.step);
        ((s * 16_383) >> 30) as i16
    }
}
