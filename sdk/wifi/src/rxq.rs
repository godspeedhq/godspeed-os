// SPDX-License-Identifier: GPL-2.0-only
//! The received-frame queue between a radio and `nic-driver`: frames a pull read off the chip, held until
//! the stack asks for them. Every radio has one; only how a pull fills it is the chip's.

/// The largest ethernet frame handed up - `nic-driver`'s own `FRAME_MAX`, so a reply always fits its
/// buffer and a 4 KiB message.
pub const FRAME_MAX: usize = 1600;
/// Frames the queue holds. Eight is one `nic-driver` batch drain; `net-stack` drains every hundred
/// milliseconds when it has a link, and what does not fit stays in the chip for the next pull.
pub const RX_SLOTS: usize = 8;
/// A bounded ring of received ethernet frames, oldest first. About 13 KiB on the serve loop's stack.
pub struct RxQueue {
    slots: [[u8; FRAME_MAX]; RX_SLOTS],
    lens: [u16; RX_SLOTS],
    head: usize,
    count: usize,
    /// Frames queued and handed up over the driver's life. Kept for diagnostics; nothing reports them yet
    /// (`wifi debug stats` does not read them).
    pub queued: u32,
    pub handed: u32,
    /// Frames dropped because the buffer they were asked into could not hold them.
    pub too_big: u32,
}

impl RxQueue {
    pub fn new() -> Self {
        RxQueue { slots: [[0; FRAME_MAX]; RX_SLOTS], lens: [0; RX_SLOTS], head: 0, count: 0, queued: 0, handed: 0, too_big: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn has_room(&self) -> bool {
        self.count < RX_SLOTS
    }

    /// Forget what is queued - on `leave`, on `radio off`, and on a fresh join, because a frame from the
    /// old link handed to the stack on the new one is a frame from nowhere.
    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }

    pub fn push(&mut self, frame: &[u8]) -> bool {
        if !self.has_room() || frame.len() > FRAME_MAX {
            return false;
        }
        let i = (self.head + self.count) % RX_SLOTS;
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
    /// (`backlog/80` S8).
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        if self.count == 0 {
            return 0;
        }
        let n = self.lens[self.head] as usize;
        if n > out.len() {
            self.head = (self.head + 1) % RX_SLOTS;
            self.count -= 1;
            self.too_big = self.too_big.wrapping_add(1);
            return 0;
        }
        out[..n].copy_from_slice(&self.slots[self.head][..n]);
        self.head = (self.head + 1) % RX_SLOTS;
        self.count -= 1;
        self.handed = self.handed.wrapping_add(1);
        n
    }
}
