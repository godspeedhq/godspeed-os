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
}
