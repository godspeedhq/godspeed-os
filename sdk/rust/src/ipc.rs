//! IPC syscall wrappers — §8.2.
//!
//! Typed wrappers so service code cannot accidentally call the wrong syscall
//! number or pass a malformed message size.

use crate::capability::CapHandle;
use crate::syscall::raw_syscall;

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
        let len = bytes.len().min(MAX_PAYLOAD);
        let mut payload = [0u8; MAX_PAYLOAD];
        payload[..len].copy_from_slice(&bytes[..len]);
        Self {
            payload,
            payload_len: len,
            caps: [None; 4],
            cap_count: 0,
        }
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

// ---------------------------------------------------------------------------
// IPC syscall wrappers.
// ---------------------------------------------------------------------------

/// Block until a message is available on `endpoint`. (§8.2 — `recv`)
///
/// Passes a stack-allocated receive buffer to the kernel; the kernel copies the
/// message payload there and returns the number of bytes written.
pub fn recv(endpoint: CapHandle) -> Result<Message, IpcError> {
    let mut payload = [0u8; MAX_PAYLOAD];
    // SAFETY: raw_syscall(2) = Recv; buf is a valid stack slice within user space.
    let ret = unsafe {
        raw_syscall(
            2,
            endpoint.0 as u64,
            payload.as_mut_ptr() as u64,
            MAX_PAYLOAD as u64,
        )
    };
    if ret < 0 {
        Err(i64_to_ipc_error(ret))
    } else {
        Ok(Message::from_bytes(&payload[..ret as usize]))
    }
}

/// Send a message to `endpoint`; block if the queue is full. (§8.2 — `send`)
pub fn send(endpoint: CapHandle, msg: &Message) -> Result<(), IpcError> {
    let payload = msg.payload_bytes();
    // SAFETY: raw_syscall(1) = Send; payload is a valid slice within user space.
    let ret = unsafe {
        raw_syscall(
            1,
            endpoint.0 as u64,
            payload.as_ptr() as u64,
            payload.len() as u64,
        )
    };
    if ret == 0 { Ok(()) } else { Err(i64_to_ipc_error(ret)) }
}

/// Send without blocking; return `QueueFull` immediately. (§8.2 — `try_send`)
pub fn try_send(endpoint: CapHandle, msg: &Message) -> Result<(), IpcError> {
    let payload = msg.payload_bytes();
    // SAFETY: raw_syscall(3) = TrySend; payload is a valid slice within user space.
    let ret = unsafe {
        raw_syscall(
            3,
            endpoint.0 as u64,
            payload.as_ptr() as u64,
            payload.len() as u64,
        )
    };
    if ret == 0 { Ok(()) } else { Err(i64_to_ipc_error(ret)) }
}

// ---------------------------------------------------------------------------
// Error conversion.
// ---------------------------------------------------------------------------

pub(crate) fn i64_to_ipc_error(code: i64) -> IpcError {
    match code {
        -2  => IpcError::CapError(crate::capability::CapError::CapNotHeld),
        -3  => IpcError::CapError(crate::capability::CapError::CapInsufficientRights),
        -4  => IpcError::CapError(crate::capability::CapError::CapNotGrantable),
        -7  => IpcError::EndpointDead,
        -8  => IpcError::QueueFull,
        -9  => IpcError::QueueFull, // QueueEmpty on caller side == retry
        -10 => IpcError::MessageTooLarge,
        _   => IpcError::EndpointDead,
    }
}
