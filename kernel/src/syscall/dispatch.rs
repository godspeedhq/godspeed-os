//! Syscall entry point and dispatch — §8.2, §7.5.
//!
//! Every syscall validates the supplied capability before performing any
//! privileged action. No capability → no action; no exceptions (§3.1).
//!
//! Syscall numbers are fixed; adding a syscall requires a new number and a
//! capability that authorises it.

use crate::capability::cap::{CapError, ResourceId};
use crate::capability::rights::Rights;
use crate::ipc::message::{IpcError, Message};
use crate::ipc::endpoint::EndpointId;
use crate::task::scheduler;

/// Syscall numbers. Stable ABI.
#[repr(u64)]
pub enum SyscallNumber {
    Send    = 1,
    Recv    = 2,
    TrySend = 3,
    Yield   = 4,
    Log     = 5,
    AllocMem = 6,
}

/// Raw syscall dispatcher — called from the SYSCALL/SYSENTER IDT stub.
///
/// Registers: rax = syscall number, rdi/rsi/rdx = arguments.
///
/// # Safety
/// Called from ring 3 → ring 0 transition; must validate all user-supplied
/// values before use. Never trusts register values as kernel pointers.
#[no_mangle]
pub unsafe extern "C" fn syscall_handler(
    number: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
) -> i64 {
    match number {
        n if n == SyscallNumber::Send    as u64 => handle_send(arg0, arg1, arg2),
        n if n == SyscallNumber::Recv    as u64 => handle_recv(arg0),
        n if n == SyscallNumber::TrySend as u64 => handle_try_send(arg0, arg1, arg2),
        n if n == SyscallNumber::Yield   as u64 => {
            crate::task::scheduler::yield_current();
            0
        }
        n if n == SyscallNumber::Log     as u64 => handle_log(arg0, arg1, arg2),
        _ => -1, // Unknown syscall.
    }
}

// ---------------------------------------------------------------------------
// Syscall: Log (5) — write a message to the kernel ring buffer.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot, arg1 = pointer to UTF-8 bytes, arg2 = byte length.
///
/// Requires `Rights::WRITE` on `LOG_WRITE_RESOURCE`.
/// v1 note: msg_ptr is trusted (kernel tasks only); userspace validation
/// happens in Milestone 7 when ring-3 tasks are introduced.
unsafe fn handle_log(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c) => c,
        Err(e) => return cap_err_to_i64(e),
    };

    if cap.resource_id != crate::capability::LOG_WRITE_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    let len = msg_len as usize;
    if len == 0 || len > 256 {
        return -1;
    }

    // SAFETY: v1 kernel tasks pass a valid kernel pointer; ring-3 validation
    // deferred to Milestone 7.
    let bytes = unsafe { core::slice::from_raw_parts(msg_ptr as *const u8, len) };
    match core::str::from_utf8(bytes) {
        Ok(s) => { crate::kprintln!("{}", s); 0 }
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// Syscall: Send / Recv / TrySend (1, 2, 3) — Milestone 5.
// ---------------------------------------------------------------------------

unsafe fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    todo!(
        "1. validate cap (SEND right + generation) from current task's cap table; \
         2. copy msg_len bytes from user msg_ptr into a kernel Message; \
         3. call ipc::routing::enqueue; \
         4. if cross-core, send IPI via smp::ipi; \
         5. if queue full, block task via scheduler::block_on_send"
    )
}

unsafe fn handle_recv(cap_slot: u64) -> i64 {
    todo!(
        "1. validate cap (RECV right + generation); \
         2. dequeue from endpoint queue; \
         3. if empty, block via scheduler::block_on_recv; \
         4. copy message to user buffer"
    )
}

unsafe fn handle_try_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    todo!("same as handle_send but returns QueueFull immediately instead of blocking")
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn cap_err_to_i64(e: CapError) -> i64 {
    match e {
        CapError::CapNotHeld           => -2,
        CapError::CapInsufficientRights => -3,
        CapError::CapNotGrantable       => -4,
        CapError::CapWrongScope         => -5,
        CapError::CapRevoked            => -6,
        CapError::EndpointDead          => -7,
        CapError::GenerationMismatch    => -6, // maps to CapRevoked
    }
}
