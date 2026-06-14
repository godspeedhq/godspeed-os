// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Bounded per-endpoint message queue — §8.5.
//!
//! Fixed depth: 16 messages per endpoint (worst-case 64 KiB per queue).
//! Not configurable per endpoint in v1; per-endpoint depth is v2 work.
//!
//! The queue lives on the core that owns the endpoint. Cross-core enqueue
//! goes through the routing table + IPI path, not a shared pointer.

use crate::ipc::message::Message;

pub const QUEUE_DEPTH: usize = 16;

/// A fixed-depth FIFO queue of IPC messages.
///
/// Derives Copy so that `RoutingEntry` (which embeds one) can be used in a
/// const-initialised static array. Copying a queue is only done at static
/// init time (all fields are zeroed); never copy a live queue.
#[derive(Copy, Clone)]
pub struct MessageQueue {
    slots: [Option<Message>; QUEUE_DEPTH],
    head: usize,
    len: usize,
}

impl MessageQueue {
    pub const fn new() -> Self {
        Self {
            slots: [None; QUEUE_DEPTH],
            head: 0,
            len: 0,
        }
    }

    pub fn is_full(&self) -> bool {
        self.len == QUEUE_DEPTH
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Enqueue a message. Returns `Err(msg)` if the queue is full.
    pub fn enqueue(&mut self, msg: Message) -> Result<(), Message> {
        if self.is_full() {
            return Err(msg);
        }
        let tail = (self.head + self.len) % QUEUE_DEPTH;
        self.slots[tail] = Some(msg);
        self.len += 1;
        Ok(())
    }

    /// Dequeue the oldest message.
    pub fn dequeue(&mut self) -> Option<Message> {
        if self.is_empty() {
            return None;
        }
        let msg = self.slots[self.head].take();
        self.head = (self.head + 1) % QUEUE_DEPTH;
        self.len -= 1;
        msg
    }

    pub fn depth(&self) -> usize {
        self.len
    }

    /// Drain all messages without delivering them (called on endpoint death — §8.6).
    pub fn drain(&mut self) {
        while self.dequeue().is_some() {}
    }

    /// Reset to empty in-place, without creating a large stack temporary.
    ///
    /// Call this instead of `queue = MessageQueue::new()` when re-initialising
    /// an existing entry: the new() form constructs a ~67 KiB temporary on the
    /// caller's stack, which overflows the 64 KiB SYSCALL kernel stack.
    /// After drain() all slots are already None; we only need to reset indices.
    pub fn reset(&mut self) {
        self.head = 0;
        self.len  = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::message::Message;
    use proptest::prelude::*;

    fn msg(byte: u8) -> Message {
        Message::new(&[byte]).unwrap()
    }

    #[test]
    fn new_queue_is_empty() {
        let q = MessageQueue::new();
        assert!(q.is_empty());
        assert!(!q.is_full());
        assert_eq!(q.depth(), 0);
    }

    #[test]
    fn enqueue_dequeue_single() {
        let mut q = MessageQueue::new();
        q.enqueue(msg(42)).unwrap();
        assert_eq!(q.depth(), 1);
        let out = q.dequeue().unwrap();
        assert_eq!(out.payload_bytes(), &[42]);
    }

    #[test]
    fn fifo_order_preserved() {
        let mut q = MessageQueue::new();
        for i in 0..8u8 {
            q.enqueue(msg(i)).unwrap();
        }
        for i in 0..8u8 {
            let out = q.dequeue().unwrap();
            assert_eq!(out.payload_bytes(), &[i]);
        }
    }

    #[test]
    fn full_queue_rejects_enqueue() {
        let mut q = MessageQueue::new();
        for i in 0..QUEUE_DEPTH as u8 {
            q.enqueue(msg(i)).unwrap();
        }
        assert!(q.is_full());
        assert_eq!(q.depth(), QUEUE_DEPTH);
        let extra = msg(99);
        assert!(q.enqueue(extra).is_err());
    }

    #[test]
    fn empty_queue_dequeue_returns_none() {
        let mut q = MessageQueue::new();
        assert!(q.dequeue().is_none());
    }

    #[test]
    fn depth_tracks_enqueue_dequeue() {
        let mut q = MessageQueue::new();
        q.enqueue(msg(1)).unwrap();
        q.enqueue(msg(2)).unwrap();
        assert_eq!(q.depth(), 2);
        q.dequeue();
        assert_eq!(q.depth(), 1);
        q.dequeue();
        assert_eq!(q.depth(), 0);
    }

    #[test]
    fn drain_empties_queue() {
        let mut q = MessageQueue::new();
        for i in 0..10u8 { q.enqueue(msg(i)).unwrap(); }
        q.drain();
        assert!(q.is_empty());
        assert_eq!(q.depth(), 0);
    }

    #[test]
    fn wraparound_preserves_fifo() {
        // Fill then drain half, fill again — exercises the head/tail wrap.
        let mut q = MessageQueue::new();
        for i in 0..8u8 { q.enqueue(msg(i)).unwrap(); }
        for _ in 0..8 { q.dequeue(); }
        for i in 10..18u8 { q.enqueue(msg(i)).unwrap(); }
        for i in 10..18u8 {
            assert_eq!(q.dequeue().unwrap().payload_bytes(), &[i]);
        }
    }

    #[test]
    fn queue_head_tail_invariant_depth_le_capacity() {
        let mut q = MessageQueue::new();
        for i in 0..QUEUE_DEPTH as u8 {
            q.enqueue(msg(i)).unwrap();
            assert!(q.depth() <= QUEUE_DEPTH);
        }
        for _ in 0..QUEUE_DEPTH {
            q.dequeue();
            assert!(q.depth() <= QUEUE_DEPTH);
        }
    }

    #[test]
    fn reset_clears_without_drain() {
        let mut q = MessageQueue::new();
        for i in 0..5u8 { q.enqueue(msg(i)).unwrap(); }
        q.drain(); // drain first so slots are None
        q.reset();
        assert!(q.is_empty());
        // Re-use after reset works correctly.
        q.enqueue(msg(77)).unwrap();
        assert_eq!(q.dequeue().unwrap().payload_bytes(), &[77]);
    }

    // --- property tests (§22 P6) --------------------------------------------

    #[derive(Debug, Clone)]
    enum Op { Enq(u8), Deq }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(
            prop_oneof![
                any::<u8>().prop_map(Op::Enq),
                Just(Op::Deq),
            ],
            0..64,
        )
    }

    fn apply(q: &mut MessageQueue, op: &Op) {
        match op {
            Op::Enq(b) => { let _ = q.enqueue(msg(*b)); }
            Op::Deq    => { let _ = q.dequeue(); }
        }
    }

    proptest! {
        /// depth never exceeds QUEUE_DEPTH under any enqueue/dequeue sequence (§8.5, P6).
        #[test]
        fn depth_never_exceeds_capacity(sequence in ops()) {
            let mut q = MessageQueue::new();
            for op in &sequence {
                apply(&mut q, op);
                prop_assert!(q.depth() <= QUEUE_DEPTH);
            }
        }

        /// is_full and is_empty are always consistent with depth (§8.5, P6).
        #[test]
        fn full_and_empty_flags_consistent_with_depth(sequence in ops()) {
            let mut q = MessageQueue::new();
            for op in &sequence {
                apply(&mut q, op);
                prop_assert_eq!(q.is_full(),  q.depth() == QUEUE_DEPTH);
                prop_assert_eq!(q.is_empty(), q.depth() == 0);
            }
        }

        /// FIFO order is preserved for any partial fill up to capacity (§8.1).
        #[test]
        fn fifo_order_preserved_for_any_fill(
            items in proptest::collection::vec(any::<u8>(), 1..=QUEUE_DEPTH),
        ) {
            let mut q = MessageQueue::new();
            for &b in &items { q.enqueue(msg(b)).unwrap(); }
            for &b in &items {
                let msg = q.dequeue().unwrap();
                prop_assert_eq!(msg.payload_bytes(), &[b]);
            }
        }

        /// drain always yields an empty queue regardless of prior state (§8.6).
        #[test]
        fn drain_always_yields_empty(sequence in ops()) {
            let mut q = MessageQueue::new();
            for op in &sequence { apply(&mut q, op); }
            q.drain();
            prop_assert!(q.is_empty());
            prop_assert_eq!(q.depth(), 0);
        }
    }
}
