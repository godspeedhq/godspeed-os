//! IPC syscall wrappers — §8.2.
//!
//! Typed wrappers so service code cannot accidentally call the wrong syscall
//! number or pass a malformed message size.

use crate::capability::CapHandle;

pub const MAX_PAYLOAD: usize = 4096;

/// An IPC message as seen by service code.
pub struct Message {
    pub payload: [u8; MAX_PAYLOAD],
    pub payload_len: usize,
    /// Capability handles embedded in this message (received side).
    pub caps: [Option<CapHandle>; 4],
    pub cap_count: usize,
}

impl Message {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        todo!("copy bytes into payload field, zero caps")
    }

    pub fn payload_bytes(&self) -> &[u8] {
        &self.payload[..self.payload_len]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    EndpointDead,
    QueueFull,
    MessageTooLarge,
    CapError(crate::capability::CapError),
}

/// Block until a message is available on `endpoint`. (§8.2 — `recv`)
pub fn recv(endpoint: CapHandle) -> Result<Message, IpcError> {
    todo!("syscall SyscallNumber::Recv with endpoint.0")
}

/// Send a message to `endpoint`; block if the queue is full. (§8.2 — `send`)
pub fn send(endpoint: CapHandle, msg: &Message) -> Result<(), IpcError> {
    todo!("syscall SyscallNumber::Send with endpoint.0, msg ptr, msg len")
}

/// Send without blocking; return QueueFull immediately. (§8.2 — `try_send`)
pub fn try_send(endpoint: CapHandle, msg: &Message) -> Result<(), IpcError> {
    todo!("syscall SyscallNumber::TrySend")
}
