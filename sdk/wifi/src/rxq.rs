// SPDX-License-Identifier: GPL-2.0-only
//! The received-frame queue between a radio and `nic-driver`: frames a pull read off the chip, held until
//! the stack asks for them. Every radio has one; only how a pull fills it is the chip's.

/// The largest ethernet frame handed up - `nic-driver`'s own `FRAME_MAX`, so a reply always fits its
/// buffer and a 4 KiB message.
pub const FRAME_MAX: usize = 1600;
/// Frames the queue holds. What does not fit before a read stays in the chip for the next pull; what does not
/// fit from a burst already read (a superframe's sub-frames, the AIC8800's whole queue) is turned away and
/// counted in `full`.
///
/// **32, since 2026-10-22; it was 8, one `nic-driver` batch.** On the VisionFive a 10 MB download over the
/// AIC8800 turned away 136 frames against 459 handed up, in bursts, and the data waited up to 161 s behind
/// the gaps they left (37-45 KiB/s); a `nic-driver` drain carries at most two full frames (a 4 KiB message),
/// so a burst of more than eight was dropped while the stack collected the first.
pub const RX_SLOTS: usize = 32;
/// A bounded ring of received ethernet frames, oldest first, `N` of them (`RX_SLOTS` unless a holder sizes its
/// own): about 51 KiB on the serve loop's stack at 32.
pub struct RxQueue<const N: usize = RX_SLOTS> {
    slots: [[u8; FRAME_MAX]; N],
    lens: [u16; N],
    head: usize,
    count: usize,
    /// Frames queued and handed up over the driver's life. Kept for diagnostics: `handed` is said beside `full`
    /// by the serve loop (`note_full`); `queued` is reported nowhere yet, and `wifi debug stats` reads neither.
    pub queued: u32,
    pub handed: u32,
    /// Frames dropped for size: one larger than a slot (`FRAME_MAX`) offered to `push`, or a queued one larger
    /// than the buffer `pop` was given.
    pub too_big: u32,
    /// Frames turned away because every slot was taken. A radio that reads a burst at once - a Broadcom
    /// superframe, the AIC8800's whole queue - can hold more than this queue does, and those frames are
    /// gone; the serve loop says how many (`serve.rs`), so a slow link can be told from a lossy one.
    pub full: u32,
}

impl<const N: usize> RxQueue<N> {
    pub fn new() -> Self {
        RxQueue { slots: [[0; FRAME_MAX]; N], lens: [0; N], head: 0, count: 0, queued: 0, handed: 0, too_big: 0, full: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn has_room(&self) -> bool {
        self.count < N
    }

    /// Forget what is queued - on `leave`, on `radio off`, and on a fresh join, because a frame from the
    /// old link handed to the stack on the new one is a frame from nowhere.
    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    pub fn push(&mut self, frame: &[u8]) -> bool {
        if !self.has_room() {
            self.full = self.full.wrapping_add(1);
            return false;
        }
        if frame.len() > FRAME_MAX {
            self.too_big = self.too_big.wrapping_add(1);
            return false;
        }
        let i = (self.head + self.count) % N;
        self.slots[i][..frame.len()].copy_from_slice(frame);
        self.lens[i] = frame.len() as u16;
        self.count += 1;
        self.queued = self.queued.wrapping_add(1);
        true
    }

    /// The oldest frame, copied into `out`; 0 when nothing is queued.
    ///
    /// A head frame `out` cannot hold is DROPPED and counted in [`RxQueue::too_big`], and 0 returned:
    /// it used to stay at the head, so every later call returned 0 and the queue never moved again
    /// (`backlog/fixed/80` S8).
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        if self.count == 0 {
            return 0;
        }
        let n = self.lens[self.head] as usize;
        if n > out.len() {
            self.head = (self.head + 1) % N;
            self.count -= 1;
            self.too_big = self.too_big.wrapping_add(1);
            return 0;
        }
        out[..n].copy_from_slice(&self.slots[self.head][..n]);
        self.head = (self.head + 1) % N;
        self.count -= 1;
        self.handed = self.handed.wrapping_add(1);
        n
    }
}
