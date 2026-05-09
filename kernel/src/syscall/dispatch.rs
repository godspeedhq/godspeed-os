//! Syscall entry point and dispatch — §8.2, §7.5.
//!
//! Every syscall validates the supplied capability before performing any
//! privileged action. No capability → no action; no exceptions (§3.1).
//!
//! Syscall numbers are fixed; adding a syscall requires a new number and a
//! capability that authorises it.

use crate::capability::cap::CapError;
use crate::capability::rights::Rights;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message, MAX_MESSAGE_SIZE};
use crate::task::scheduler;
use crate::task::state::TaskState;

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
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);

    let msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };

    match crate::ipc::routing::enqueue(endpoint_id, msg, cap.generation) {
        Ok(Some(receiver_slot)) => {
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => 0,
        Err(IpcError::QueueFull) => {
            // Atomically record the blocked-send state and block.  CLI ensures
            // no dequeue (and its corresponding wake) races between record and
            // block_and_reschedule (§8.9).
            unsafe { core::arch::asm!("cli", options(nostack, nomem)); }
            let my_slot = scheduler::current_task_slot();
            match crate::ipc::routing::record_blocked_sender(endpoint_id, my_slot, msg) {
                Ok(()) => {
                    // block_and_reschedule re-enables interrupts on resume.
                    // Returns 0 when the receiver moves our pending_send into
                    // the queue, or EndpointDead if the endpoint is killed.
                    scheduler::block_and_reschedule(TaskState::BlockedOnSend)
                }
                Err(e) => {
                    unsafe { core::arch::asm!("sti", options(nostack, nomem)); }
                    ipc_err_to_i64(e)
                }
            }
        }
        Err(e) => ipc_err_to_i64(e),
    }
}

unsafe fn handle_recv(cap_slot: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::RECV) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);

    loop {
        match crate::ipc::routing::dequeue(endpoint_id, cap.generation) {
            Ok((msg, sender_to_wake)) => {
                if let Some(slot) = sender_to_wake {
                    scheduler::wake_by_slot(slot, 0);
                }
                scheduler::store_recv_message(msg);
                return 0;
            }
            Err(IpcError::QueueEmpty) => {
                // Atomically record blocked state and block.  CLI prevents
                // a concurrent enqueue from seeing blocked_receiver = None
                // and missing the wakeup.
                unsafe { core::arch::asm!("cli", options(nostack, nomem)); }
                let my_slot = scheduler::current_task_slot();
                match crate::ipc::routing::record_blocked_receiver(endpoint_id, my_slot) {
                    Ok(()) => {}
                    Err(e) => {
                        unsafe { core::arch::asm!("sti", options(nostack, nomem)); }
                        return ipc_err_to_i64(e);
                    }
                }
                // block_and_reschedule re-enables interrupts on resume.
                let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
                if err != 0 {
                    return err;
                }
                // The sender enqueued a message and woke us; loop to dequeue it.
            }
            Err(e) => return ipc_err_to_i64(e),
        }
    }
}

unsafe fn handle_try_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);

    let msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };

    match crate::ipc::routing::enqueue(endpoint_id, msg, cap.generation) {
        Ok(Some(receiver_slot)) => { scheduler::wake_by_slot(receiver_slot, 0); 0 }
        Ok(None) => 0,
        Err(e)   => ipc_err_to_i64(e), // QueueFull returned directly (no blocking)
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Build a kernel `Message` from a (kernel-task) pointer + length.
///
/// # Safety
/// `msg_ptr` must point to at least `msg_len` readable bytes. In Milestone 5
/// this is always satisfied because the callers are ring-0 kernel tasks.
unsafe fn build_message(msg_ptr: u64, msg_len: u64) -> Result<Message, i64> {
    let len = msg_len as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ipc_err_to_i64(IpcError::MessageTooLarge));
    }
    // SAFETY: kernel-task pointer; ring-3 validation deferred to Milestone 7.
    let bytes = unsafe { core::slice::from_raw_parts(msg_ptr as *const u8, len) };
    Message::new(bytes).map_err(|e| ipc_err_to_i64(e))
}

fn ipc_err_to_i64(e: IpcError) -> i64 {
    match e {
        IpcError::EndpointDead    => -7,
        IpcError::QueueFull       => -8,
        IpcError::QueueEmpty      => -9,
        IpcError::MessageTooLarge => -10,
        IpcError::Cap(ce)         => cap_err_to_i64(ce),
    }
}

fn cap_err_to_i64(e: CapError) -> i64 {
    match e {
        CapError::CapNotHeld            => -2,
        CapError::CapInsufficientRights => -3,
        CapError::CapNotGrantable       => -4,
        CapError::CapWrongScope         => -5,
        CapError::CapRevoked            => -6,
        CapError::EndpointDead          => -7,
        CapError::GenerationMismatch    => -6, // maps to CapRevoked
    }
}
