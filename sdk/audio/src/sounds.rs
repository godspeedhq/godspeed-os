// SPDX-License-Identifier: GPL-2.0-only
//! The system sounds (`docs/audio.md`, "System sounds"), written once for every audio driver.
//!
//! **Generated, never read from disk.** Errors happen when things are going wrong, `fs` being down among
//! them, and a sound that had to be read first would be silent exactly then. They are our own short
//! sequences of tones, not copies of anyone else's, and each is short enough to sit whole in either
//! driver's ring before it starts - so a sound never needs refilling.
//!
//! Each tone is faded in and out over a few milliseconds: a sine cut off mid-swing is a click, and a click
//! is the one thing a system sound must not add.

use crate::sine::Sine;
use crate::wire;

/// A sound's shape: tones of `(hz, ms)` in order, `hz` 0 being a gap.
pub fn shape(kind: u8) -> Option<&'static [(u16, u16)]> {
    Some(match kind {
        // Two short falling tones.
        wire::SOUND_ERROR => &[(880, 70), (0, 30), (587, 110)],
        // One low tone.
        wire::SOUND_REFUSED => &[(220, 160)],
        // A short rising chirp.
        wire::SOUND_DONE => &[(659, 60), (784, 60), (1047, 90)],
        // A rising pair, and the same pair falling.
        wire::SOUND_PLUGGED => &[(523, 70), (0, 20), (784, 90)],
        wire::SOUND_UNPLUGGED => &[(784, 70), (0, 20), (523, 90)],
        _ => return None,
    })
}

/// Frames a sound takes at `rate`.
pub fn frames(kind: u8, rate: u32) -> usize {
    shape(kind).map_or(0, |s| s.iter().map(|&(_, ms)| rate as usize * ms as usize / 1000).sum())
}

/// The fade at each end of a tone, in milliseconds.
const FADE_MS: u32 = 3;

/// A sound being rendered, one sample at a time.
pub struct Chime {
    segs: &'static [(u16, u16)],
    seg: usize,
    sine: Sine,
    /// Frames into the current tone, and its length.
    at: u32,
    len: u32,
    fade: u32,
    rate: u32,
}

impl Chime {
    /// `None` for a kind this module does not know.
    pub fn new(kind: u8, rate: u32) -> Option<Self> {
        let segs = shape(kind)?;
        let mut c = Chime { segs, seg: 0, sine: Sine::new(1, rate), at: 0, len: 0, fade: (rate * FADE_MS / 1000).max(1), rate };
        c.enter(0);
        Some(c)
    }

    fn enter(&mut self, i: usize) {
        self.seg = i;
        self.at = 0;
        if let Some(&(hz, ms)) = self.segs.get(i) {
            self.sine = Sine::new(hz.max(1) as u32, self.rate);
            self.len = self.rate * ms as u32 / 1000;
        }
    }

    /// The next sample, or `None` once the sound has ended.
    pub fn next(&mut self) -> Option<i16> {
        while self.at >= self.len {
            if self.seg + 1 >= self.segs.len() {
                return None;
            }
            self.enter(self.seg + 1);
        }
        let (hz, _) = self.segs[self.seg];
        let s = self.sine.next();
        let edge = self.at.min(self.len - 1 - self.at);
        self.at += 1;
        if hz == 0 {
            return Some(0);
        }
        Some(if edge < self.fade { (s as i32 * edge as i32 / self.fade as i32) as i16 } else { s })
    }
}
