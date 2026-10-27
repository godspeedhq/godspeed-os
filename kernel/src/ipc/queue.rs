// SPDX-License-Identifier: GPL-2.0-only
//! Bounded per-endpoint message queue - §8.5.
//!
//! Fixed depth: 16 messages per endpoint (worst-case 64 KiB per queue).
//! Not configurable per endpoint in v1; per-endpoint depth is v2 work.
//!
//! The queue lives in the endpoint's routing-table entry (`routing::TABLE`), which
//! every core reaches under the one table lock; a cross-core wake is an IPI
//! (`scheduler::wake_by_slot`).

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

    /// How many messages are waiting. Used by the teardown path to say how many were DISCARDED when
    /// an endpoint died - a message queued onto a service that then dies is lost, and silently until
    /// this could be counted.
    pub fn len(&self) -> usize {
        self.len
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

    /// Dequeue the oldest message whose `sender_ep` matches, leaving every other message queued in
    /// FIFO order. `None` if no queued message matches.
    ///
    /// This is the primitive behind reply correlation in `Call` (§8.2): the caller's own endpoint
    /// receives BOTH unrelated client requests and the awaited reply, and taking the plain queue head
    /// handed a blocked `Call` whichever arrived first - observed on hardware as `fs` receiving a
    /// shell request as its block-driver read "reply", then every later reply off-by-one (the
    /// "MALFORMED reply / protocol desync" line), including the undetectable case where a stale
    /// 513-byte read-reply was accepted as a different block's data. Bounded: one pass over at most
    /// `QUEUE_DEPTH` (16) slots; the compaction shift preserves arrival order for the untouched rest.
    ///
    /// A message matches only if BOTH its sender's endpoint id and that endpoint's generation are the
    /// ones asked for: ids are reused, so the id alone matched a dead incarnation's reply (D5).
    pub fn dequeue_matching(&mut self, sender_ep: u64, sender_gen: u32) -> Option<Message> {
        for i in 0..self.len {
            let idx = (self.head + i) % QUEUE_DEPTH;
            let matches = self.slots[idx].as_ref()
                .is_some_and(|m| m.sender_ep == sender_ep && m.sender_gen == sender_gen);
            if !matches { continue; }
            let msg = self.slots[idx].take();
            // Close the gap: shift every later message back one slot, keeping FIFO order.
            for j in i..self.len - 1 {
                let from = (self.head + j + 1) % QUEUE_DEPTH;
                let to   = (self.head + j) % QUEUE_DEPTH;
                self.slots[to] = self.slots[from].take();
            }
            self.len -= 1;
            return msg;
        }
        None
    }

    /// Drain all messages without delivering them (called on endpoint death - §8.6).
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
        // Fill then drain half, fill again - exercises the head/tail wrap.
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

    #[test]
    fn reset_empties_a_queue_that_was_not_drained() {
        let mut q = MessageQueue::new();
        for i in 0..5u8 { q.enqueue(msg(i)).unwrap(); }
        q.reset();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        q.enqueue(msg(77)).unwrap();
        assert_eq!(q.dequeue().unwrap().payload_bytes(), &[77]);
    }

    #[test]
    fn len_is_the_number_waiting() {
        let mut q = MessageQueue::new();
        assert_eq!(q.len(), 0);
        for i in 0..3u8 { q.enqueue(msg(i)).unwrap(); }
        assert_eq!(q.len(), 3);
        q.dequeue();
        assert_eq!(q.len(), 2);
    }

    // --- reply correlation: dequeue_matching (§8.2 Call) ----------------------

    fn from(sender_ep: u64, byte: u8) -> Message {
        let mut m = msg(byte);
        m.sender_ep = sender_ep;
        m
    }

    /// The awaited reply is taken from the middle of the queue; the requests around it stay, in order.
    #[test]
    fn dequeue_matching_takes_the_reply_and_keeps_the_rest_in_order() {
        let mut q = MessageQueue::new();
        q.enqueue(from(1, 10)).unwrap();
        q.enqueue(from(2, 20)).unwrap();
        q.enqueue(from(1, 11)).unwrap();
        q.enqueue(from(3, 30)).unwrap();
        let got = q.dequeue_matching(2, 0).unwrap();
        assert_eq!((got.sender_ep, got.payload_bytes()), (2, &[20][..]));
        assert_eq!(q.len(), 3);
        for (ep, b) in [(1, 10), (1, 11), (3, 30)] {
            let m = q.dequeue().unwrap();
            assert_eq!((m.sender_ep, m.payload_bytes()), (ep, &[b][..]));
        }
        assert!(q.is_empty());
    }

    /// Two from the same sender: the OLDER one is taken.
    #[test]
    fn dequeue_matching_takes_the_oldest_match() {
        let mut q = MessageQueue::new();
        q.enqueue(from(5, 1)).unwrap();
        q.enqueue(from(5, 2)).unwrap();
        assert_eq!(q.dequeue_matching(5, 0).unwrap().payload_bytes(), &[1]);
        assert_eq!(q.dequeue_matching(5, 0).unwrap().payload_bytes(), &[2]);
        assert!(q.dequeue_matching(5, 0).is_none());
    }

    /// Same endpoint id, another generation: a different incarnation, never the reply (D5).
    #[test]
    fn dequeue_matching_needs_the_generation_too() {
        let mut q = MessageQueue::new();
        let mut late = from(7, 1);
        late.sender_gen = 5; // the dead incarnation of endpoint 7
        let mut now = from(7, 2);
        now.sender_gen = 9;  // its successor, same id
        q.enqueue(late).unwrap();
        q.enqueue(now).unwrap();
        assert_eq!(q.dequeue_matching(7, 9).unwrap().payload_bytes(), &[2]);
        assert!(q.dequeue_matching(7, 9).is_none(), "the dead incarnation's message is not the reply");
        assert_eq!(q.len(), 1);
    }

    /// No match leaves the queue exactly as it was.
    #[test]
    fn dequeue_matching_without_a_match_changes_nothing() {
        let mut q = MessageQueue::new();
        q.enqueue(from(1, 1)).unwrap();
        q.enqueue(from(2, 2)).unwrap();
        assert!(q.dequeue_matching(9, 0).is_none());
        assert_eq!(q.len(), 2);
        assert_eq!(q.dequeue().unwrap().payload_bytes(), &[1]);
        assert_eq!(q.dequeue().unwrap().payload_bytes(), &[2]);
        assert!(MessageQueue::new().dequeue_matching(0, 0).is_none());
    }

    /// A FULL queue: the slot past the tail is the head, so a shift that ran one step too far would move
    /// the oldest message into the tail. Every message but the reply must come out once, in order.
    #[test]
    fn dequeue_matching_in_a_full_queue() {
        for head in [0usize, 5] {
            let mut q = MessageQueue::new();
            for _ in 0..head { q.enqueue(msg(0)).unwrap(); q.dequeue(); }
            for b in 0..QUEUE_DEPTH as u8 { q.enqueue(from(if b == 6 { 9 } else { 1 }, b)).unwrap(); }
            assert!(q.is_full());
            assert_eq!(q.dequeue_matching(9, 0).unwrap().payload_bytes(), &[6]);
            for b in (0..QUEUE_DEPTH as u8).filter(|&b| b != 6) {
                assert_eq!(q.dequeue().unwrap().payload_bytes(), &[b]);
            }
            assert!(q.is_empty());
        }
    }

    /// Across the wrap: head near the end of the slot array, the match past it, the shift crossing it.
    #[test]
    fn dequeue_matching_across_the_wrap() {
        let mut q = MessageQueue::new();
        for i in 0..(QUEUE_DEPTH - 2) as u8 { q.enqueue(msg(i)).unwrap(); }
        for _ in 0..(QUEUE_DEPTH - 2) { q.dequeue(); }
        // head is now QUEUE_DEPTH - 2; these four occupy the last two slots and the first two.
        for (ep, b) in [(1, 1), (1, 2), (7, 3), (1, 4)] { q.enqueue(from(ep, b)).unwrap(); }
        assert_eq!(q.dequeue_matching(7, 0).unwrap().payload_bytes(), &[3]);
        for b in [1u8, 2, 4] { assert_eq!(q.dequeue().unwrap().payload_bytes(), &[b]); }
        assert!(q.is_empty());
    }

    // --- property tests (§22 P6) --------------------------------------------

    #[derive(Debug, Clone)]
    enum Op { Enq(u8), Deq }

    #[derive(Debug, Clone)]
    enum COp { Enq(u64, u8), Deq, Match(u64) }

    fn call_ops() -> impl Strategy<Value = Vec<COp>> {
        proptest::collection::vec(
            prop_oneof![
                (0u64..4, any::<u8>()).prop_map(|(e, b)| COp::Enq(e, b)),
                Just(COp::Deq),
                (0u64..5).prop_map(COp::Match),
            ],
            0..96,
        )
    }

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

        /// The queue under enqueue, dequeue and dequeue_matching behaves exactly as a plain ordered
        /// list, wrap included: same answer from every operation, same contents after it (§8.2, §8.5).
        #[test]
        fn queue_matches_a_list_model_under_reply_correlation(sequence in call_ops()) {
            let mut q = MessageQueue::new();
            let mut model: std::collections::VecDeque<(u64, u8)> = std::collections::VecDeque::new();
            for op in &sequence {
                match *op {
                    COp::Enq(e, b) => {
                        let full = model.len() == QUEUE_DEPTH;
                        prop_assert_eq!(q.enqueue(from(e, b)).is_err(), full);
                        if !full { model.push_back((e, b)); }
                    }
                    COp::Deq => {
                        let got = q.dequeue().map(|m| (m.sender_ep, m.payload_bytes()[0]));
                        prop_assert_eq!(got, model.pop_front());
                    }
                    COp::Match(e) => {
                        let got = q.dequeue_matching(e, 0).map(|m| (m.sender_ep, m.payload_bytes()[0]));
                        let want = model.iter().position(|&(s, _)| s == e).and_then(|i| model.remove(i));
                        prop_assert_eq!(got, want);
                    }
                }
                prop_assert_eq!(q.len(), model.len());
            }
            while let Some(want) = model.pop_front() {
                let m = q.dequeue().unwrap();
                prop_assert_eq!((m.sender_ep, m.payload_bytes()[0]), want);
            }
            prop_assert!(q.is_empty());
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
