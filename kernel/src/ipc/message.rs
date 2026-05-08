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
}

/// Errors returned by IPC syscalls — §8.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// The endpoint no longer exists (service died or was restarted).
    EndpointDead,
    /// The queue is full and the caller used `try_send`.
    QueueFull,
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
