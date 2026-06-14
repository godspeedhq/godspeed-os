// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! IPC message format — §8.5.
//!
//! Maximum size: 4 KiB (one page). Copy semantics: kernel copies sender →
//! receiver. Zero-copy is permanently rejected (§2.5).
//! Embedded capabilities are transferred with GRANT and removed from the
//! sender's cap table on enqueue.

use crate::capability::cap::{Capability, CapError};

pub const MAX_MESSAGE_SIZE: usize = 4096;
pub const MAX_EMBEDDED_CAPS: usize = 4;

/// A kernel IPC message.
#[derive(Copy, Clone)]
pub struct Message {
    /// Raw payload bytes; length is tracked separately.
    pub payload: [u8; MAX_MESSAGE_SIZE],
    pub payload_len: usize,
    /// Capabilities embedded in this message (transferred via GRANT).
    pub caps: [Option<Capability>; MAX_EMBEDDED_CAPS],
    pub cap_count: usize,
}

impl core::fmt::Debug for Message {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Message {{ payload_len: {}, cap_count: {} }}", self.payload_len, self.cap_count)
    }
}

impl Message {
    pub fn new(payload: &[u8]) -> Result<Self, IpcError> {
        if payload.len() > MAX_MESSAGE_SIZE {
            return Err(IpcError::MessageTooLarge);
        }
        let mut msg = Self {
            payload: [0u8; MAX_MESSAGE_SIZE],
            payload_len: payload.len(),
            caps: [None; MAX_EMBEDDED_CAPS],
            cap_count: 0,
        };
        msg.payload[..payload.len()].copy_from_slice(payload);
        Ok(msg)
    }

    pub fn payload_bytes(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }

    /// Build an interrupt-event message carrying the IRQ number (§12.2).
    ///
    /// Used exclusively by the kernel IDT routing path. No capability is
    /// involved — the kernel is the sender. Payload is one byte: the IRQ number.
    pub fn interrupt_event(irq: u8) -> Self {
        let mut msg = Self {
            payload:     [0u8; MAX_MESSAGE_SIZE],
            payload_len: 1,
            caps:        [None; MAX_EMBEDDED_CAPS],
            cap_count:   0,
        };
        msg.payload[0] = irq;
        msg
    }
}

/// Errors returned by IPC syscalls — §8.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The endpoint no longer exists (service died or was restarted).
    EndpointDead,
    /// The queue is full and the caller used `try_send` (or blocking send should block).
    QueueFull,
    /// The queue is empty and the caller should block on `recv`.
    QueueEmpty,
    /// Payload exceeds 4 KiB.
    MessageTooLarge,
    /// Capability error during send.
    Cap(CapError),
}

impl From<CapError> for IpcError {
    fn from(e: CapError) -> Self {
        IpcError::Cap(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- property tests (§8.5 size enforcement) -----------------------------

    proptest! {
        /// Any payload at or below MAX_MESSAGE_SIZE is accepted (§8.5).
        #[test]
        fn new_accepts_any_payload_within_limit(len in 0usize..=MAX_MESSAGE_SIZE) {
            let payload = vec![0u8; len];
            prop_assert!(Message::new(&payload).is_ok());
        }

        /// Any payload exceeding MAX_MESSAGE_SIZE is rejected (§8.5).
        #[test]
        fn new_rejects_oversized_payload(extra in 1usize..=MAX_MESSAGE_SIZE) {
            let payload = vec![0u8; MAX_MESSAGE_SIZE + extra];
            prop_assert!(Message::new(&payload).is_err());
        }

        /// payload_bytes round-trips arbitrary data without corruption.
        #[test]
        fn payload_bytes_round_trips(
            data in proptest::collection::vec(any::<u8>(), 0..=64usize),
        ) {
            let msg = Message::new(&data).unwrap();
            prop_assert_eq!(msg.payload_bytes(), data.as_slice());
        }
    }
}
