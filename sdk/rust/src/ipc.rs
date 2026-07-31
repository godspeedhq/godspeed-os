// SPDX-License-Identifier: Apache-2.0
//! IPC syscall wrappers - §8.2.
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
    /// A synchronous `call` was woken because the peer (the would-be replier) died before replying
    /// (§8.6). The reply-side twin of `EndpointDead`: the caller waited on the peer's liveness, not a
    /// timer, and so never hangs (Commandment VIII). Reacquire the peer by name and retry, exactly as
    /// for a failed send.
    ReplyDead,
    CapError(crate::capability::CapError),
}

// ---------------------------------------------------------------------------
// IPC syscall wrappers.
// ---------------------------------------------------------------------------

/// Block until a message is available on `endpoint`. (§8.2 - `recv`)
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

/// Non-blocking `recv`: return `Ok(None)` immediately if the endpoint queue is empty,
/// `Ok(Some(msg))` if a message was waiting. (syscall 34 - `try_recv`) Lets a busy-polling
/// driver drain interrupt events (§12) without giving up its loop.
pub fn try_recv(endpoint: CapHandle) -> Result<Option<Message>, IpcError> {
    const TRY_RECV_EMPTY: i64 = -1000; // kernel sentinel for an empty queue
    let mut payload = [0u8; MAX_PAYLOAD];
    // SAFETY: raw_syscall(34) = TryRecv; buf is a valid stack slice within user space.
    let ret = unsafe {
        raw_syscall(
            34,
            endpoint.0 as u64,
            payload.as_mut_ptr() as u64,
            MAX_PAYLOAD as u64,
        )
    };
    if ret == TRY_RECV_EMPTY {
        Ok(None)
    } else if ret < 0 {
        Err(i64_to_ipc_error(ret))
    } else {
        Ok(Some(Message::from_bytes(&payload[..ret as usize])))
    }
}

/// Blocking `recv` with a timeout in TSC cycles: `Ok(Some(msg))` on a message, `Ok(None)`
/// on timeout, `Err` on a real error. `timeout_cycles == 0` blocks forever (like `recv`).
/// (syscall 35 - `recv_timeout`) Lets a driver idle on its interrupt yet still wake on a
/// timer (e.g. for keyboard auto-repeat while a key is held - §12).
pub fn recv_timeout(endpoint: CapHandle, timeout_cycles: u64) -> Result<Option<Message>, IpcError> {
    const RECV_TIMED_OUT: i64 = -1001; // kernel sentinel for "timed out, no message"
    let mut payload = [0u8; MAX_PAYLOAD];
    // arg0 packs the buffer length (high) and the cap slot (low) to fit the 3-arg ABI.
    let packed = ((MAX_PAYLOAD as u64) << 16) | (endpoint.0 as u64 & 0xFFFF);
    // ARM's 32-bit syscall ABI carries each argument in ONE register, and `raw_syscall` truncates a
    // u64 arg to u32. Every OTHER arg (pointer, handle, length) genuinely fits in 32 bits, but a timeout
    // in generic-timer ticks does NOT: at the Pi 2's ~62.5 MHz CNTFRQ, u32::MAX ticks is only ~68 s, so
    // a longer finite timeout would truncate to a tiny value (premature wake) or - if it landed on a
    // multiple of 2^32 - to 0, which the kernel reads as "block forever": a bounded VIII deadline turning
    // into an infinite hang. Saturate to u32::MAX so a long finite request becomes the longest
    // REPRESENTABLE timeout (~68 s), never a tiny one and never 0; keep a genuine 0 (block-forever) as 0.
    // x86-64 (64-bit registers) passes the full value. (userspace-audit Audit 4, A-U1.)
    #[cfg(target_arch = "arm")]
    let timeout_cycles = if timeout_cycles == 0 { 0 } else { timeout_cycles.min(u32::MAX as u64).max(1) };
    // SAFETY: raw_syscall(35) = RecvTimeout; buf is a valid stack slice within user space.
    let ret = unsafe { raw_syscall(35, packed, payload.as_mut_ptr() as u64, timeout_cycles) };
    if ret == RECV_TIMED_OUT {
        Ok(None)
    } else if ret < 0 {
        Err(i64_to_ipc_error(ret))
    } else {
        Ok(Some(Message::from_bytes(&payload[..ret as usize])))
    }
}

/// Send a message to `endpoint`; block if the queue is full. (§8.2 - `send`)
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

/// Send without blocking; return `QueueFull` immediately. (§8.2 - `try_send`)
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

/// Synchronous CALL (syscall 41): send `request` to `target` carrying `reply_grant` as a one-shot
/// reply cap, then block on `recv` (the caller's own endpoint, where the reply lands) until the peer
/// replies OR the peer's endpoint dies. Returns the reply, `Err(ReplyDead)` if the peer died before
/// replying, `Err(EndpointDead)` if the peer was already dead when the request was sent, or another
/// `IpcError` on a failed send.
///
/// This **waits on truth, not on time** (Commandment VIII): a peer that dies mid-request wakes the
/// caller with `ReplyDead` (the reply-side twin of `EndpointDead`, §8.6) instead of hanging it - no
/// timer, no fixed yield count. The buffer is in/out: the request is read from it, and the kernel
/// writes the reply back into the same buffer.
pub fn call(
    target:      CapHandle,
    reply_grant: CapHandle,
    recv:        CapHandle,
    request:     &Message,
) -> Result<Message, IpcError> {
    let mut buf = [0u8; MAX_PAYLOAD];
    let req = request.payload_bytes();
    buf[..req.len()].copy_from_slice(req);
    // Pack three 16-bit cap slots + the length into THREE 32-bit-safe args (not two): arg0 = target |
    // reply, arg2 = recv | len. The old layout put `recv` in bits 32-47 of arg0, which the ARM 32-bit
    // syscall ABI truncates away (each arg is one register) - recv_slot became 0 and the Call routed to
    // the wrong endpoint (userspace-audit A-U1 class). A request length never exceeds one message (4 KiB
    // < 0xFFFF), so `recv` rides the high half of the length arg. `handle_call` mirrors this.
    let packed = ((reply_grant.0 as u64) << 16) | (target.0 as u64);
    let recv_len = ((recv.0 as u64) << 16) | (req.len() as u64 & 0xFFFF);
    // SAFETY: raw_syscall(41) = Call; `buf` is a valid in/out stack buffer in user space (request in,
    // reply written back), `req.len() <= MAX_PAYLOAD`. The kernel validates every cap slot and the
    // buffer pointer before use.
    let ret = unsafe { raw_syscall(41, packed, buf.as_mut_ptr() as u64, recv_len) };
    if ret < 0 {
        Err(i64_to_ipc_error(ret))
    } else {
        Ok(Message::from_bytes(&buf[..ret as usize]))
    }
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
        -12 => IpcError::ReplyDead,
        _   => IpcError::EndpointDead,
    }
}
