// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Syscall entry point and dispatch — §8.2, §7.5.
//!
//! Every syscall validates the supplied capability before performing any
//! privileged action. No capability → no action; no exceptions (§3.1).
//!
//! Syscall numbers are fixed; adding a syscall requires a new number and a
//! capability that authorises it.

use crate::arch::x86_64::{read_user_bytes, validate_user_ptr, write_user_bytes, read_cycle_counter};
use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
use crate::capability::cap::CapError;
use crate::capability::rights::Rights;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message, MAX_MESSAGE_SIZE};
use crate::memory::allocator::alloc_frame;
use crate::task::scheduler;
use crate::task::state::TaskState;

/// Syscall numbers. Stable ABI.
#[repr(u64)]
pub enum SyscallNumber {
    Send           = 1,
    Recv           = 2,
    TrySend        = 3,
    Yield          = 4,
    Log            = 5,
    AllocMem       = 6,
    Spawn          = 7,
    Kill           = 8,
    Abort          = 9,
    AcquireSendCap = 10,
    SendWithCap    = 11,
    TakePendingCap = 12,
    InspectKernel  = 13,
    QueryCapRights = 14,
    RemoveCap      = 15,
    TaskStat       = 16,
    ConsoleRead    = 17,
    Reboot         = 18,
    SpawnPipe      = 19,
    ConsolePush    = 20,
    Park           = 21,
    Print          = 22,
    ConsoleWrite   = 23,
    TryConsoleRead = 24,
    ConsoleEcho    = 25,
    ConsoleBootComplete = 26,
    SignalInputReady    = 27,
    TaskCaps            = 28,
    DeriveCap           = 29,
    ResourceMint        = 30,
    ResourceInvoke      = 31,
    ResourceRevoke      = 32,
    LastRecvBadge       = 33,
    TryRecv             = 34,
    RecvTimeout         = 35,
    IrqUnmask           = 36,
    Sleep               = 37,
    SpawnReturningEndpoint = 38,
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
        n if n == SyscallNumber::Send           as u64 => handle_send(arg0, arg1, arg2),
        n if n == SyscallNumber::Recv           as u64 => handle_recv(arg0, arg1, arg2),
        n if n == SyscallNumber::TryRecv        as u64 => handle_try_recv(arg0, arg1, arg2),
        n if n == SyscallNumber::RecvTimeout    as u64 => handle_recv_timeout(arg0, arg1, arg2),
        n if n == SyscallNumber::IrqUnmask      as u64 => handle_irq_unmask(arg0),
        n if n == SyscallNumber::Sleep          as u64 => handle_sleep(arg0),
        n if n == SyscallNumber::TrySend        as u64 => handle_try_send(arg0, arg1, arg2),
        n if n == SyscallNumber::Yield          as u64 => {
            crate::task::scheduler::yield_current();
            0
        }
        n if n == SyscallNumber::Log            as u64 => handle_log(arg0, arg1, arg2),
        n if n == SyscallNumber::AllocMem       as u64 => handle_alloc_mem(arg0),
        n if n == SyscallNumber::Spawn          as u64 => handle_spawn(arg0, arg1, arg2),
        n if n == SyscallNumber::SpawnReturningEndpoint as u64 => handle_spawn_returning_endpoint(arg0, arg1, arg2),
        n if n == SyscallNumber::Kill           as u64 => handle_kill(arg0, arg1),
        n if n == SyscallNumber::Abort          as u64 => handle_abort(arg0, arg1),
        n if n == SyscallNumber::AcquireSendCap as u64 => handle_acquire_send_cap(arg0, arg1, arg2),
        n if n == SyscallNumber::DeriveCap      as u64 => handle_derive_cap(arg0, arg1, arg2),
        n if n == SyscallNumber::SendWithCap    as u64 => handle_send_with_cap(arg0, arg1, arg2),
        n if n == SyscallNumber::TakePendingCap as u64 => handle_take_pending_cap(),
        n if n == SyscallNumber::InspectKernel  as u64 => handle_inspect_kernel(arg0, arg1, arg2),
        n if n == SyscallNumber::QueryCapRights as u64 => handle_query_cap_rights(arg0),
        n if n == SyscallNumber::RemoveCap      as u64 => handle_remove_cap(arg0),
        n if n == SyscallNumber::TaskStat       as u64 => handle_task_stat(arg0, arg1, arg2),
        n if n == SyscallNumber::ConsoleRead    as u64 => handle_console_read(arg0),
        n if n == SyscallNumber::Reboot        as u64 => handle_reboot(),
        n if n == SyscallNumber::SpawnPipe     as u64 => handle_spawn_pipe(arg0, arg1, arg2),
        n if n == SyscallNumber::ConsolePush   as u64 => handle_console_push(arg0, arg1),
        n if n == SyscallNumber::Park          as u64 => scheduler::park_current(),
        n if n == SyscallNumber::Print         as u64 => handle_print(arg0, arg1, arg2),
        n if n == SyscallNumber::ConsoleWrite  as u64 => handle_console_write(arg0, arg1, arg2),
        n if n == SyscallNumber::TryConsoleRead as u64 => handle_try_console_read(arg0),
        n if n == SyscallNumber::ConsoleEcho   as u64 => handle_console_echo(arg0, arg1),
        n if n == SyscallNumber::ConsoleBootComplete as u64 => handle_console_boot_complete(arg0),
        n if n == SyscallNumber::SignalInputReady as u64 => handle_signal_input_ready(arg0),
        n if n == SyscallNumber::TaskCaps as u64 => handle_task_caps(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceMint   as u64 => handle_resource_mint(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceInvoke as u64 => handle_resource_invoke(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceRevoke as u64 => handle_resource_revoke(arg0),
        n if n == SyscallNumber::LastRecvBadge  as u64 => scheduler::take_last_recv_badge() as i64,
        _ => -1, // Unknown syscall.
    }
}

// ---------------------------------------------------------------------------
// Syscall: Log (5) — write a message to the kernel ring buffer.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot, arg1 = pointer to UTF-8 bytes, arg2 = byte length.
///
/// Requires `Rights::WRITE` on `LOG_WRITE_RESOURCE`.
fn handle_log(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c) => c,
        Err(e) => return cap_err_to_i64(e),
    };

    if cap.resource_id != crate::capability::LOG_WRITE_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    // §3.1 (no ambient authority): control reaches the privileged log write only
    // with a cap the lookup + scope check validated. Executable §3.1 checkpoint.
    crate::invariants::assertions::assert_cap_validated(&Ok(()));

    let len = msg_len as usize;
    if len == 0 || len > 256 { return -1; }

    let bytes = match read_user_bytes(msg_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    match core::str::from_utf8(bytes) {
        Ok(s) => { crate::kprintln!("{}", s); 0 }
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// Syscall: Print (22) — like Log but WITHOUT a trailing newline.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot, arg1 = pointer to UTF-8 bytes, arg2 = byte length.
///
/// Requires `Rights::WRITE` on `LOG_WRITE_RESOURCE`. For inline console output
/// such as the shell prompt (`gsh> `), where a newline would push typed input to
/// the next line.
fn handle_print(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c) => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::LOG_WRITE_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    let len = msg_len as usize;
    if len == 0 || len > 256 { return -1; }
    let bytes = match read_user_bytes(msg_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    match core::str::from_utf8(bytes) {
        Ok(s) => { crate::kprint!("{}", s); 0 }
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleWrite (23) — write to the interactive console (serial + TV).
// ---------------------------------------------------------------------------

/// arg0 = cap_slot, arg1 = pointer to UTF-8 bytes, arg2 = byte length.
///
/// Requires `Rights::WRITE` on `LOG_WRITE_RESOURCE` (Stage 1; Stage 2 gives the
/// console service a dedicated cap). Unlike `Log`/`Print` (which now go to the
/// log stream = serial only), this writes the CONSOLE path — serial AND the
/// framebuffer — for interactive output (the shell prompt, `observe`). No newline
/// is added; the caller includes one if wanted. See `docs/console-service.md`.
fn handle_console_write(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c) => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::LOG_WRITE_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    let len = msg_len as usize;
    if len == 0 || len > 256 { return -1; }
    let bytes = match read_user_bytes(msg_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    crate::arch::x86_64::console_write_bytes(bytes);
    0
}

// ---------------------------------------------------------------------------
// Syscall: Send / Recv / TrySend (1, 2, 3) — Milestone 5/6.
// ---------------------------------------------------------------------------

fn handle_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);

    let msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };

    // §3.1 (no ambient authority): the send below requires a validated SEND cap,
    // which the lookup above enforced. Executable §3.1 checkpoint.
    crate::invariants::assertions::assert_cap_validated(&Ok(()));

    let my_slot = scheduler::current_task_slot();

    // enqueue atomically records us as a blocked sender if QueueFull —
    // no separate record_blocked_sender call needed.
    match crate::ipc::routing::enqueue(endpoint_id, msg, cap.generation, Some(my_slot)) {
        Ok(Some(receiver_slot)) => {
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => 0,
        Err(IpcError::QueueFull) => {
            // We are now recorded in the routing table as a blocked sender.
            // block_and_reschedule checks for "already woken" and returns
            // TASK_WAKEUP_ERR[slot] (0 on success, negative on EndpointDead).
            scheduler::block_and_reschedule(TaskState::BlockedOnSend)
        }
        Err(e) => ipc_err_to_i64(e),
    }
}

/// arg0 = cap_slot, arg1 = out_buf_ptr (user VA), arg2 = out_buf_len.
///
/// Blocks until a message is dequeued from the endpoint, then copies the
/// payload into the caller-supplied buffer.  Returns the number of bytes
/// written on success, or a negative error code.
fn handle_recv(cap_slot: u64, out_buf: u64, out_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::RECV) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    // §3.1 (no ambient authority): the recv below requires a validated RECV cap,
    // which the lookup above enforced. Executable §3.1 checkpoint.
    crate::invariants::assertions::assert_cap_validated(&Ok(()));
    let endpoint_id = EndpointId(cap.resource_id.0);

    let buf_len = out_len as usize;
    if buf_len == 0 || buf_len > MAX_MESSAGE_SIZE { return -1; }
    if !validate_user_ptr(out_buf, buf_len) { return -1; }

    let my_slot = scheduler::current_task_slot();

    loop {
        match crate::ipc::routing::dequeue(endpoint_id, cap.generation, Some(my_slot)) {
            Ok((msg, sender_to_wake)) => {
                if let Some(slot) = sender_to_wake {
                    scheduler::wake_by_slot(slot, 0);
                }
                // Record the delegated-resource badge (§7.10), if any, for retrieval via
                // LastRecvBadge. Unbadged messages (every ordinary send) clear it to 0, so a
                // stale badge from a prior recv can never be read as this message's.
                scheduler::set_last_recv_badge(msg.badge_id, msg.badge_right);
                // Install any embedded capabilities into the receiver's cap table
                // and push their slot indices into the pending-recv-cap buffer so
                // the receiver can retrieve them via syscall 12 (TakePendingCap).
                let n_caps = msg.cap_count.min(msg.caps.len());
                for i in 0..n_caps {
                    if let Some(embedded_cap) = msg.caps[i] {
                        if let Ok(new_slot) = scheduler::current_task_insert_cap(embedded_cap) {
                            scheduler::push_pending_recv_cap(new_slot as u32);
                        }
                    }
                }
                // Copy payload to the caller's user-space buffer.
                let payload  = msg.payload_bytes();
                let copy_len = payload.len().min(buf_len);
                if !write_user_bytes(out_buf, &payload[..copy_len]) {
                    return -1;
                }
                return copy_len as i64;
            }
            Err(IpcError::QueueEmpty) => {
                let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
                if err != 0 { return err; }
                // Sender woke us; loop to dequeue the message.
            }
            Err(e) => return ipc_err_to_i64(e),
        }
    }
}

/// Sentinel returned by `TryRecv` when the endpoint queue is empty (distinct from a
/// 0-byte message, which is a valid non-negative length, and from the small-negative
/// cap/IPC error codes).
pub const TRY_RECV_EMPTY: i64 = -1000;

/// Non-blocking `recv` (syscall 34). Identical to `handle_recv` except it returns
/// `TRY_RECV_EMPTY` instead of blocking when the queue is empty — so a busy-polling driver
/// can drain interrupt events (§12) without giving up its loop. Same args as `recv`.
fn handle_try_recv(cap_slot: u64, out_buf: u64, out_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::RECV) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    crate::invariants::assertions::assert_cap_validated(&Ok(()));
    let endpoint_id = EndpointId(cap.resource_id.0);

    let buf_len = out_len as usize;
    if buf_len == 0 || buf_len > MAX_MESSAGE_SIZE { return -1; }
    if !validate_user_ptr(out_buf, buf_len) { return -1; }

    let my_slot = scheduler::current_task_slot();
    match crate::ipc::routing::dequeue(endpoint_id, cap.generation, Some(my_slot)) {
        Ok((msg, sender_to_wake)) => {
            if let Some(slot) = sender_to_wake {
                scheduler::wake_by_slot(slot, 0);
            }
            scheduler::set_last_recv_badge(msg.badge_id, msg.badge_right);
            let n_caps = msg.cap_count.min(msg.caps.len());
            for i in 0..n_caps {
                if let Some(embedded_cap) = msg.caps[i] {
                    if let Ok(new_slot) = scheduler::current_task_insert_cap(embedded_cap) {
                        scheduler::push_pending_recv_cap(new_slot as u32);
                    }
                }
            }
            let payload  = msg.payload_bytes();
            let copy_len = payload.len().min(buf_len);
            if !write_user_bytes(out_buf, &payload[..copy_len]) {
                return -1;
            }
            copy_len as i64
        }
        Err(IpcError::QueueEmpty) => TRY_RECV_EMPTY,
        Err(e) => ipc_err_to_i64(e),
    }
}

/// Sentinel returned by `RecvTimeout` when the timeout elapsed with no message (distinct
/// from a non-negative length, `TRY_RECV_EMPTY`, and the cap/IPC error codes).
pub const RECV_TIMED_OUT: i64 = -1001;

/// Blocking `recv` with a timeout (syscall 35, §12 timed-wait). Blocks until a message
/// arrives OR `timeout` TSC cycles elapse, whichever first; `timeout == 0` means no timeout
/// (block forever, like `recv`). Returns the payload length, `RECV_TIMED_OUT` on timeout, or
/// a negative error. Lets a driver wait on its interrupt yet still wake on a timer for
/// auto-repeat. Args are packed to fit the 3-register ABI:
///   arg0 = (out_len << 16) | (cap_slot & 0xFFFF), arg1 = out_buf, arg2 = timeout_cycles.
fn handle_recv_timeout(packed: u64, out_buf: u64, timeout: u64) -> i64 {
    let cap_slot = (packed & 0xFFFF) as usize;
    let buf_len  = (packed >> 16) as usize;
    let cap = match scheduler::current_task_lookup_cap(cap_slot, Rights::RECV) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    crate::invariants::assertions::assert_cap_validated(&Ok(()));
    let endpoint_id = EndpointId(cap.resource_id.0);

    if buf_len == 0 || buf_len > MAX_MESSAGE_SIZE { return -1; }
    if !validate_user_ptr(out_buf, buf_len) { return -1; }

    let my_slot = scheduler::current_task_slot();
    // 0 = no deadline (block forever); else an absolute deadline in BSP timer TICKS, not TSC
    // cycles — the timed-wake scan runs on the BSP and compares one shared tick clock, which is
    // valid cross-core where a per-core TSC is not (see scheduler::scan_timed_wakes).
    let deadline = if timeout == 0 {
        0
    } else {
        scheduler::monotonic_ticks().wrapping_add(scheduler::cycles_to_ticks(timeout))
    };

    let result = loop {
        match crate::ipc::routing::dequeue(endpoint_id, cap.generation, Some(my_slot)) {
            Ok((msg, sender_to_wake)) => {
                if let Some(slot) = sender_to_wake {
                    scheduler::wake_by_slot(slot, 0);
                }
                scheduler::set_last_recv_badge(msg.badge_id, msg.badge_right);
                let n_caps = msg.cap_count.min(msg.caps.len());
                for i in 0..n_caps {
                    if let Some(embedded_cap) = msg.caps[i] {
                        if let Ok(new_slot) = scheduler::current_task_insert_cap(embedded_cap) {
                            scheduler::push_pending_recv_cap(new_slot as u32);
                        }
                    }
                }
                let payload  = msg.payload_bytes();
                let copy_len = payload.len().min(buf_len);
                if !write_user_bytes(out_buf, &payload[..copy_len]) { break -1; }
                break copy_len as i64;
            }
            Err(IpcError::QueueEmpty) => {
                if deadline != 0 && scheduler::monotonic_ticks() >= deadline {
                    break RECV_TIMED_OUT;
                }
                if deadline != 0 {
                    scheduler::set_wake_deadline(my_slot, deadline);
                }
                let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
                if err != 0 { break err; }
                // Woken by a sender (message ready) or the timer (deadline) — re-check.
            }
            Err(e) => break ipc_err_to_i64(e),
        }
    };
    scheduler::clear_wake_deadline(my_slot);
    result
}

/// Re-open the IOAPIC gate for a level-triggered IRQ after the driver has cleared its device's
/// interrupt source (syscall 36, §12). The kernel masks a level INTx in `route::deliver` so it
/// can't storm while the driver handles it; the driver calls this to unmask once acked. Gated:
/// the caller must own the endpoint registered for `irq` (its `hw_interrupt` route). A no-op
/// for edge/MSI vectors (their GSI table entry is empty). arg0 = irq/vector.
fn handle_irq_unmask(irq: u64) -> i64 {
    let irq = (irq & 0xFF) as u8;
    let my_ep = scheduler::current_task_endpoint();
    if my_ep.is_none() || crate::interrupt::route::registered_endpoint(irq) != my_ep {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    crate::arch::x86_64::ioapic::unmask_vector(irq);
    0
}

/// Block the calling task for roughly `cycles` TSC cycles, then return (syscall 37). A real
/// sleep — the core can `hlt` while the task is parked — so a service that needs to wait (e.g.
/// a foreground UI polling for `q` between repaints, or the shell waiting for that UI to exit)
/// does NOT busy-`yield`, which would peg its core at ~100% and make every task on that core
/// read as fully busy in `observe`. Like `yield`, sleeping your own task needs no capability.
/// Uses the same BSP-tick timed-wake as `recv_timeout` (§12); a `cycles` of 0 returns at once.
fn handle_sleep(cycles: u64) -> i64 {
    if cycles == 0 { return 0; }
    let my_slot = scheduler::current_task_slot();
    let deadline = scheduler::monotonic_ticks().wrapping_add(scheduler::cycles_to_ticks(cycles));
    loop {
        if scheduler::monotonic_ticks() >= deadline { break; }
        scheduler::set_wake_deadline(my_slot, deadline);
        let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
        if err != 0 { break; }
    }
    scheduler::clear_wake_deadline(my_slot);
    0
}

fn handle_try_send(cap_slot: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(cap.resource_id.0);

    let msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };

    // §3.1 (no ambient authority): the send below requires a validated SEND cap,
    // which the lookup above enforced. Executable §3.1 checkpoint.
    crate::invariants::assertions::assert_cap_validated(&Ok(()));

    // Pass None for blocked_sender_slot — QueueFull is returned directly.
    match crate::ipc::routing::enqueue(endpoint_id, msg, cap.generation, None) {
        Ok(Some(receiver_slot)) => {
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => {
            0
        }
        Err(e)   => ipc_err_to_i64(e),
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Build a kernel `Message` from a user-space pointer + length.
fn build_message(msg_ptr: u64, msg_len: u64) -> Result<Message, i64> {
    let len = msg_len as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ipc_err_to_i64(IpcError::MessageTooLarge));
    }
    let bytes = match read_user_bytes(msg_ptr, len) {
        Some(b) => b,
        None    => return Err(-1),
    };
    Message::new(bytes).map_err(|e| ipc_err_to_i64(e))
}

// ---------------------------------------------------------------------------
// Syscall: Spawn (7) / Kill (8) / AcquireSendCap (10).
// ---------------------------------------------------------------------------

/// arg0 = (core_id << 16) | spawn_cap_slot, arg1 = name_ptr, arg2 = name_len.
///
/// Validates the spawn capability, reads the service name from user space,
/// then calls `task::spawn_service_by_name`.
///
/// core_id encoding:
///   - 0x0000 = core 0, 0x0001 = core 1, …
///   - 0xFFFF = let the kernel choose (preferred_core from service_config).
fn handle_spawn(packed_arg0: u64, name_ptr: u64, name_len: u64) -> i64 {
    let spawn_cap_slot = (packed_arg0 & 0xFFFF) as usize;
    let core_raw       = ((packed_arg0 >> 16) & 0xFFFF) as u32;
    let core_override  = if core_raw == 0xFFFF { None } else { Some(core_raw) };

    // Validate spawn capability.
    let cap = match scheduler::current_task_lookup_cap(spawn_cap_slot, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::SPAWN_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    let len = name_len as usize;
    if len == 0 || len > 64 { return -1; }
    let name_bytes = match read_user_bytes(name_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s)  => s,
        Err(_) => return -1,
    };

    match crate::task::spawn_service_by_name(name, core_override) {
        Ok(_)  => 0,
        Err(_) => -1,
    }
}

/// Syscall: SpawnReturningEndpoint (38). Like Spawn (7), but on success mints a `SEND|GRANT`
/// cap to the new service's recv endpoint and inserts it into the **caller's** cap table,
/// returning the slot. This is the Phase-0 seam for moving naming out of the kernel
/// (`docs/naming-design.md`): a spawner (the supervisor) can collect a cap to every service it
/// starts — a userspace `name → cap` map — without the kernel resolving names for third parties.
/// The old name-wiring path is unchanged; this is purely additive.
///
/// arg0 = packed (spawn_cap_slot in low 16, core in next 16; core 0xFFFF = round-robin).
/// arg1 = name ptr, arg2 = name len. Returns the endpoint cap slot (≥0), or a negative error
/// (cap error, or -1 if the spawn failed / the service has no recv endpoint to hand back).
fn handle_spawn_returning_endpoint(packed_arg0: u64, name_ptr: u64, name_len: u64) -> i64 {
    let spawn_cap_slot = (packed_arg0 & 0xFFFF) as usize;
    let core_raw       = ((packed_arg0 >> 16) & 0xFFFF) as u32;
    let core_override  = if core_raw == 0xFFFF { None } else { Some(core_raw) };

    // Validate the SPAWN capability (same gate as Spawn — §3.1).
    let cap = match scheduler::current_task_lookup_cap(spawn_cap_slot, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::SPAWN_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    let len = name_len as usize;
    if len == 0 || len > 64 { return -1; }
    let name_bytes = match read_user_bytes(name_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s)  => s,
        Err(_) => return -1,
    };

    match crate::task::spawn_service_by_name(name, core_override) {
        Ok(Some(ep_id)) => {
            // Mint a SEND|GRANT cap to the new endpoint at its current generation and hand it
            // to the caller. SEND so the caller can route to it; GRANT so it can delegate copies
            // into dependents (the supervisor wiring its name→cap map, future phases).
            let rid    = crate::capability::cap::ResourceId::from(ep_id);
            let ep_cap = crate::capability::mint_cap(rid, Rights::SEND | Rights::GRANT);
            match scheduler::current_task_insert_cap(ep_cap) {
                Ok(slot) => slot as i64,
                Err(e)   => cap_err_to_i64(e),
            }
        }
        Ok(None) => -1, // spawned, but no recv endpoint — nothing to hand back
        Err(_)   => -1,
    }
}

/// arg0 = packed (cap_slot in low 16 bits, core in next 16; core 0xFFFF = round-robin).
/// arg1 = ptr to a "producer sink" string, arg2 = its length.
///
/// Capability-broker pipe spawn (`producer | sink`): spawns `producer` and
/// delegates it a SEND cap to `sink`'s endpoint as its send_peers[0]
/// (task::spawn_service_pipe). The shell spawns `sink` first, then calls this.
fn handle_spawn_pipe(packed_arg0: u64, buf_ptr: u64, buf_len: u64) -> i64 {
    let spawn_cap_slot = (packed_arg0 & 0xFFFF) as usize;
    let core_raw       = ((packed_arg0 >> 16) & 0xFFFF) as u32;
    let core_override  = if core_raw == 0xFFFF { None } else { Some(core_raw) };

    // Same authorization as handle_spawn: the caller must hold the spawn cap.
    let cap = match scheduler::current_task_lookup_cap(spawn_cap_slot, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::SPAWN_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    let len = buf_len as usize;
    if len == 0 || len > 130 { return -1; }
    let bytes = match read_user_bytes(buf_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let s = match core::str::from_utf8(bytes) {
        Ok(s)  => s,
        Err(_) => return -1,
    };

    // Buffer is "producer sink" (single space). Split into the two names.
    let mut parts = s.split(' ').filter(|p| !p.is_empty());
    let producer = match parts.next() { Some(p) => p, None => return -1 };
    let sink     = match parts.next() { Some(p) => p, None => return -1 };

    match crate::task::spawn_service_pipe(producer, sink, core_override) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// arg0 = name_ptr, arg1 = name_len.
///
/// Kills the named running task: marks Dead, kills endpoint, wakes blocked tasks.
/// Phase 5: no capability check (cap check added in Phase 6 when service_control
/// is fully wired).
fn handle_kill(name_ptr: u64, name_len: u64) -> i64 {
    // §3.1 / §14.4: killing a service is a privileged action — it requires the
    // service_control capability. Without this gate `kill` was ambient authority
    // (any service could kill any non-trusted-root service). Like the other
    // name-taking syscalls it consumes both arg registers, so it validates by
    // holdings on the stable SERVICE_CONTROL resource. See
    // docs/service-control-cap.md.
    if !scheduler::current_task_holds_resource(
        crate::capability::SERVICE_CONTROL_RESOURCE, Rights::WRITE)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let len = name_len as usize;
    if len == 0 || len > 64 { return -1; }
    let name_bytes = match read_user_bytes(name_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s)  => s,
        Err(_) => return -1,
    };
    // §6.2 (fail-closed): the trusted root (init/supervisor) is non-restartable —
    // its death requires a reboot, so a caller must not be able to kill it via this
    // syscall. Reject the request *before* any kill happens. This is the primary §6.2
    // gate; the assert_tcb_alive sweep below is a defensive secondary check. Rejection
    // (not panic) is deliberate: a mere kill *attempt* is not a TCB death, and
    // panicking would hand any caller of this syscall a reboot denial-of-service.
    //
    // `registry` is NOT listed (H11 ph6): it is now a restartable userspace name
    // service — killing it degrades name resolution until the supervisor restarts it,
    // it does not reboot the system. It can therefore be killed by a SERVICE_CONTROL
    // holder like any other restartable service (and the identity test does so).
    if matches!(name, "init" | "supervisor") {
        return -1;
    }
    if crate::task::kill_by_name(name) {
        // A kill bumps the dead endpoint's generation and could (if a bug let it
        // target a trusted service) take down the TCB. Now that the kill has
        // completed and no kernel locks are held, verify the two invariants a
        // kill is most likely to break:
        //   §6.2 — every TCB service (init/supervisor/registry) is still alive;
        //          TCB death is a loud, unrecoverable failure, not a silent one.
        //   §7.8 — the cap table is still consistent (no cap carries a generation
        //          beyond its resource's current generation). The generation bump
        //          only ever moves resources forward, so all surviving caps stay
        //          stale-or-current. This is an O(active-caps) walk; the kill path
        //          is not a per-syscall hot path, so it is an acceptable home for
        //          the §7.8 check (see invariants/CLAUDE.md).
        crate::invariants::assertions::assert_tcb_alive();
        crate::invariants::assertions::assert_cap_table_consistent();
        0
    } else { -1 }
}

/// arg0 = name_ptr, arg1 = name_len, arg2 = include_grant (0 = SEND only, 1 = SEND|GRANT).
///
/// Looks up `name` in the kernel name registry, mints a SEND (or SEND|GRANT)
/// cap to that endpoint in the calling task's cap table, and returns the slot.
///
/// Used by services to reacquire a fresh SEND cap after `EndpointDead` (§14.2)
/// and by property-test probes that need to transfer caps (P3 — arg2=1).
fn handle_acquire_send_cap(name_ptr: u64, name_len: u64, include_grant: u64) -> i64 {
    let len = name_len as usize;
    if len == 0 || len > 64 { return -1; }
    let name_bytes = match read_user_bytes(name_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s)  => s,
        Err(_) => return -1,
    };

    let ep_id = match crate::ipc::names::lookup(name) {
        Some(id) => id,
        None     => return -1, // service not registered
    };

    let resource_id = crate::capability::cap::ResourceId::from(ep_id);
    let rights = if include_grant != 0 {
        crate::capability::Rights::SEND | crate::capability::Rights::GRANT
    } else {
        crate::capability::Rights::SEND
    };
    let cap = crate::capability::mint_cap(resource_id, rights);

    match scheduler::current_task_insert_cap(cap) {
        Ok(slot) => slot as i64,
        Err(_)   => -1, // cap table full
    }
}

/// Syscall: DeriveCap (29) — duplicate a capability the caller holds **with GRANT**
/// into a fresh slot. arg0 = held cap slot. Returns the new slot, or a negative
/// cap-error code.
///
/// This is the primitive that lets a userspace name service (the `registry`) serve
/// many `lookup`s from one held endpoint cap: it derives a copy per client and grants
/// that copy away (via `SendWithCap`) while retaining the original. Sound and
/// non-escalating (§7.3): the copy carries the *same* resource, generation, and
/// rights — never wider — and the GRANT gate means the caller could already transfer
/// the whole cap wholesale, so duplicating it grants no authority it lacked. Endpoint
/// caps already permit many concurrent senders, so duplication matches the IPC model.
/// The generation check inside `lookup_cap` also forbids deriving from a stale cap.
fn handle_derive_cap(held_slot: u64, _a1: u64, _a2: u64) -> i64 {
    let held = match scheduler::current_task_lookup_cap(held_slot as usize, Rights::GRANT) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    match scheduler::current_task_insert_cap(held) {
        Ok(slot) => slot as i64,
        Err(_)   => -1, // cap table full
    }
}

// ---------------------------------------------------------------------------
// Syscall: SendWithCap (11) — send a message with an embedded capability.
// ---------------------------------------------------------------------------

/// arg0 = (grant_slot << 16) | endpoint_slot
/// arg1 = msg_ptr (user VA)
/// arg2 = msg_len
///
/// Validates SEND on the endpoint cap and GRANT on the cap to transfer.
/// Embeds the cap in the message, enqueues, then removes the cap from the
/// sender's table (§7.6 — cap moved exactly once).
///
/// Returns `CapNotGrantable` (-4) if the grant cap lacks the GRANT right, so
/// the sender knows the cap was NOT transferred (it remains in their table).
fn handle_send_with_cap(packed: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    let endpoint_slot = (packed & 0xFFFF) as usize;
    let grant_slot    = ((packed >> 16) & 0xFFFF) as usize;

    // 1. Validate endpoint cap (SEND right required).
    let endpoint_cap = match scheduler::current_task_lookup_cap(endpoint_slot, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let endpoint_id = EndpointId(endpoint_cap.resource_id.0);

    // 2. Validate grant cap (GRANT right required).
    //    CapInsufficientRights → CapNotGrantable so the caller gets the exact
    //    error code from §7.7 rather than the generic rights-failure code.
    let cap_to_grant = match scheduler::current_task_lookup_cap(grant_slot, Rights::GRANT) {
        Ok(c)  => c,
        Err(crate::capability::cap::CapError::CapInsufficientRights) =>
            return cap_err_to_i64(crate::capability::cap::CapError::CapNotGrantable),
        Err(e) => return cap_err_to_i64(e),
    };

    // 3. Build message with embedded cap.
    let mut msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };
    msg.caps[0]   = Some(cap_to_grant);
    msg.cap_count = 1;

    let my_slot = scheduler::current_task_slot();

    // 4. Enqueue; remove cap from sender on success (cap is now in the message).
    //    On QueueFull the message (with cap) is stored in the routing table as
    //    a blocked-sender record; remove the cap from the sender's table so it
    //    is not duplicated.
    match crate::ipc::routing::enqueue(endpoint_id, msg, endpoint_cap.generation, Some(my_slot)) {
        Ok(Some(receiver_slot)) => {
            scheduler::current_task_remove_cap(grant_slot);
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => {
            scheduler::current_task_remove_cap(grant_slot);
            0
        }
        Err(IpcError::QueueFull) => {
            // Cap is now embedded in the message held by the routing table.
            scheduler::current_task_remove_cap(grant_slot);
            scheduler::block_and_reschedule(TaskState::BlockedOnSend)
        }
        Err(e) => ipc_err_to_i64(e), // failure before delivery — cap stays
    }
}

// ---------------------------------------------------------------------------
// Syscall: ResourceMint (30) — allocate a delegated resource + mint a cap (§7.10, P2).
// ---------------------------------------------------------------------------

/// arg0 = rights bitfield for the minted cap, arg1 = user ptr to receive the u64 ResourceId,
/// arg2 = unused.
///
/// Gated by `RESOURCE_MINT_RESOURCE` (WRITE). Allocates a fresh delegated resource owned by
/// the caller's endpoint, mints a cap with the requested rights into the caller's table,
/// writes the new `ResourceId` to `*arg1`, and returns the cap slot. The caller (`fs`) records
/// `ResourceId → file` and GRANT-transfers a narrowed copy to a client (file-as-capability).
fn handle_resource_mint(rights_bits: u64, out_id_ptr: u64, _a2: u64) -> i64 {
    use crate::capability::{delegated, mint_cap, RESOURCE_MINT_RESOURCE};
    // §3.1: minting a delegated resource requires the RESOURCE_MINT authority (held by `fs`).
    if !scheduler::current_task_holds_resource(RESOURCE_MINT_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    crate::invariants::assertions::assert_cap_validated(&Ok(()));
    let owner = match scheduler::current_task_endpoint() {
        Some(e) => e.0, // delegated band tracks the owner endpoint as a raw u64
        None    => return -1, // a service with no endpoint cannot own resources
    };
    // Only file-meaningful rights may ride a delegated cap (READ/WRITE), plus GRANT to transfer.
    let allowed = Rights::READ | Rights::WRITE | Rights::GRANT;
    let rights = Rights((rights_bits as u8) & allowed.0);
    let id = match delegated::allocate(owner) {
        Some(i) => i,
        None    => return -1, // band full (loud, §26.6)
    };
    let cap = mint_cap(id, rights);
    let slot = match scheduler::current_task_insert_cap(cap) {
        Ok(s)  => s,
        Err(_) => { delegated::release(id); return -1; } // cap table full — don't leak the id
    };
    if !write_user_bytes(out_id_ptr, &id.0.to_le_bytes()) {
        return -1;
    }
    slot as i64
}

// ---------------------------------------------------------------------------
// Syscall: ResourceInvoke (31) — use a delegated (file) cap (§7.10, P2).
// ---------------------------------------------------------------------------

/// arg0 = (right_bits << 32) | (reply_grant_slot << 16) | file_cap_slot
/// arg1 = msg_ptr (user VA), arg2 = msg_len.
///
/// The "use = send" of a delegated resource cap. Validates the file cap carries `right_bits`
/// (a READ-only cap invoking with WRITE fails `CapInsufficientRights` — non-escalation, §7.3),
/// then routes the message to the owning service's endpoint with the badge carried in the
/// **kernel-set `Message` fields** `badge_id`/`badge_right` (unforgeable — an ordinary `send`
/// leaves them 0), and an embedded reply cap exactly as `SendWithCap`. The owner reads the badge
/// (via `LastRecvBadge`) to know which resource + which right the kernel validated; it never
/// trusts the client, and the kernel never learns the operation.
fn handle_resource_invoke(packed: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    use crate::capability::delegated;
    let file_slot  = (packed & 0xFFFF) as usize;
    let reply_slot = ((packed >> 16) & 0xFFFF) as usize;
    let right_bits = ((packed >> 32) & 0xFF) as u8;
    let required   = Rights(right_bits);

    // 1. Validate the file cap holds the requested right (generation + rights, global table).
    let file_cap = match scheduler::current_task_lookup_cap(file_slot, required) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if !delegated::is_delegated(file_cap.resource_id) {
        return cap_err_to_i64(CapError::CapWrongScope); // not a delegated/file cap
    }
    let owner = match delegated::owner_of(file_cap.resource_id) {
        Some(o) => EndpointId(o), // u64 → the owner endpoint to route to
        None    => return ipc_err_to_i64(IpcError::EndpointDead), // resource freed
    };
    crate::invariants::assertions::assert_cap_validated(&Ok(()));

    // 2. Validate the embedded reply cap (GRANT) so the owner can reply (reply-cap pattern).
    let reply_cap = match scheduler::current_task_lookup_cap(reply_slot, Rights::GRANT) {
        Ok(c)  => c,
        Err(CapError::CapInsufficientRights) => return cap_err_to_i64(CapError::CapNotGrantable),
        Err(e) => return cap_err_to_i64(e),
    };

    // 3. Build the message: the client's payload UNCHANGED, with the badge carried in
    //    kernel-set Message fields (NOT prepended to the payload). The badge is unforgeable:
    //    only this handler — after validating the cap above — sets it; an ordinary `send`
    //    leaves it 0, so the owner can trust a badged message is a real cap invocation and not
    //    a payload a client crafted over a plain send (§7.10).
    let mut msg = match build_message(msg_ptr, msg_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };
    msg.badge_id    = file_cap.resource_id.0;
    msg.badge_right = right_bits;
    msg.caps[0]   = Some(reply_cap);
    msg.cap_count = 1;

    // 4. Route to the owner endpoint. The file cap's generation was validated against the
    //    global table above; the routing table tracks the OWNER endpoint's generation, so pass
    //    that (a live owner matches; a dead owner returns EndpointDead via check_live).
    let owner_gen = crate::ipc::routing::get_generation(owner);
    let my_slot   = scheduler::current_task_slot();
    match crate::ipc::routing::enqueue(owner, msg, owner_gen, Some(my_slot)) {
        Ok(Some(receiver_slot)) => {
            scheduler::current_task_remove_cap(reply_slot);
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => {
            scheduler::current_task_remove_cap(reply_slot);
            0
        }
        Err(IpcError::QueueFull) => {
            scheduler::current_task_remove_cap(reply_slot);
            scheduler::block_and_reschedule(TaskState::BlockedOnSend)
        }
        Err(e) => ipc_err_to_i64(e),
    }
}

// ---------------------------------------------------------------------------
// Syscall: ResourceRevoke (32) — revoke a delegated resource you own (§7.10, P2).
// ---------------------------------------------------------------------------

/// arg0 = `ResourceId` (u64). Owner-gated: succeeds only if the calling task's endpoint owns
/// the resource (ownership IS the capability check, §3.1). Bumps the generation so every
/// outstanding cap to it goes stale → next `ResourceInvoke` returns `CapRevoked` (§7.5).
fn handle_resource_revoke(id_lo: u64) -> i64 {
    use crate::capability::{delegated, ResourceId};
    let owner = match scheduler::current_task_endpoint() {
        Some(e) => e.0,
        None    => return -1,
    };
    if delegated::revoke_owned(ResourceId(id_lo), owner) { 0 } else { -1 }
}

// ---------------------------------------------------------------------------
// Syscall: TakePendingCap (12) — retrieve the next received cap slot.
// ---------------------------------------------------------------------------

/// No arguments.
///
/// Returns the next pending received cap slot as a non-negative i64, or -1 if
/// no pending caps remain.  The slot is into the calling task's own cap table;
/// it was inserted by handle_recv when it processed an embedded cap.
fn handle_take_pending_cap() -> i64 {
    match scheduler::pop_pending_recv_cap() {
        Some(slot) => slot as i64,
        None       => -1,
    }
}

// ---------------------------------------------------------------------------
// Syscall: AllocMem (6) — dynamic page allocation within the task's budget.
// ---------------------------------------------------------------------------

/// arg0 = size in bytes to allocate (must be > 0).
///
/// No capability required — the task's budget is implicitly granted at spawn
/// from the memory limit in its contract (§10.2, implicit authority).
///
/// Returns the virtual address of the newly-mapped region on success, or a
/// negative error code:
///   -11  AllocDenied — request would exceed the task's memory limit.
///   -1   other failure (physical memory exhausted; partial allocation left mapped).
fn handle_alloc_mem(size: u64) -> i64 {
    if size == 0 { return -1; }

    // Reserve budget and obtain the base virtual address to map from.
    let base_va = match scheduler::current_task_claim_alloc(size) {
        Some(va) => va,
        None     => return -11, // AllocDenied
    };

    let pages = (size + 4095) / 4096;
    // User-space read/write pages, not executable.
    let flags = (PageFlags::PRESENT | PageFlags::WRITABLE
                 | PageFlags::USER   | PageFlags::NO_EXEC).bits();

    for i in 0..pages {
        let va = base_va + i * 4096;
        let frame = match alloc_frame() {
            Some(f) => f,
            None    => return -1, // physical memory exhausted; budget already updated
        };
        let phys = frame.phys_addr().0;
        // SAFETY: va is in the task heap range (0x1_0000_0000+); phys is from the
        // allocator; the task's page table is the active CR3 during this syscall.
        if unsafe { map_in_active_tables(va, phys, flags) }.is_err() {
            return -1;
        }
        // Frame ownership passes to the page table (freed when task dies);
        // `Frame` is Copy/no-Drop, so there is nothing to release here.
    }

    base_va as i64
}

// ---------------------------------------------------------------------------
// Syscall: Abort (9) — TCB service reports a fatal failure; causes kernel panic.
// ---------------------------------------------------------------------------

/// arg0 = msg_ptr (user VA), arg1 = msg_len.
///
/// Prints "KERNEL PANIC" immediately (so the harness sees it even on minimal
/// serial buffering), then panics with "reason: {msg}" (§6.2, §22 Test 1B).
/// Does not return.
fn handle_abort(msg_ptr: u64, msg_len: u64) -> i64 {
    let len = msg_len as usize;
    if len > 0 && len <= 128 {
        if let Some(bytes) = read_user_bytes(msg_ptr, len) {
            if let Ok(s) = core::str::from_utf8(bytes) {
                crate::kprintln!("KERNEL PANIC");
                panic!("reason: {}", s);
            }
        }
    }
    crate::kprintln!("KERNEL PANIC");
    panic!("reason: (init abort — no message)");
}

// ---------------------------------------------------------------------------
// Syscall: InspectKernel (13) — structured kernel state queries.
// ---------------------------------------------------------------------------

/// arg0 = query_id, arg1/arg2 = query-specific args.
///
/// query_id = 2: endpoint generation by name.
///   arg1 = name_ptr (user VA), arg2 = name_len.
///   Returns the current generation of the named endpoint as a non-negative
///   i64, or -1 if the name is not registered.
fn handle_inspect_kernel(query_id: u64, arg1: u64, arg2: u64) -> i64 {
    // Self-state (0 = own alloc bytes), the clock (3 = TSC), and console geometry
    // (9 = fbcon rows/cols — task-neutral hardware info) are ungated, as are the
    // boot/RTC reads (10, 11). Every other query discloses another task's or
    // system-wide state and requires the INTROSPECT capability with READ (§3.1;
    // docs/introspection-capability.md).
    if !matches!(query_id, 0 | 3 | 9 | 10 | 11 | 12)
        && !scheduler::current_task_holds_resource(
            crate::capability::INTROSPECT_RESOURCE, Rights::READ)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    match query_id {
        0 => scheduler::current_task_alloc_bytes() as i64,
        1 => crate::ipc::routing::count_live_endpoints() as i64,
        3 => read_cycle_counter() as i64,
        // Console (fbcon) geometry packed as (rows << 16) | cols. The console
        // service needs this to lay out its terminal (pin the input line to the
        // bottom row). 0 if the framebuffer never initialised.
        9 => crate::arch::x86_64::fb::dims_packed() as i64,
        // Input-ready flag — set by the xHCI driver when it finishes setup (the
        // last boot step). The shell watches it to auto-clear the boot screen.
        10 => crate::arch::x86_64::input_ready() as i64,
        // Wall-clock date/time from the hardware RTC, packed (see rtc.rs). Ungated
        // — the time of day is task-neutral hardware info, like the TSC (query 3).
        11 => crate::arch::x86_64::rtc::read_datetime() as i64,
        // Wall-clock datetime captured at boot (same packed layout as query 11). Pairs with
        // query 11 for `uptime` = now − boot, a portable wall-clock delta (a tick counter's rate
        // varies with the APIC timer mode). Task-neutral hardware info like the RTC, so ungated.
        12 => crate::arch::x86_64::rtc::boot_datetime() as i64,
        4 => crate::memory::allocator::free_frame_count() as i64,
        5 => crate::memory::allocator::total_frame_count() as i64,
        6 => scheduler::core_active_ticks(arg1 as usize) as i64,
        7 => scheduler::core_total_ticks(arg1 as usize) as i64,
        8 => crate::smp::core::ready_count() as i64,
        2 => {
            // Endpoint generation by name.
            let len = arg2 as usize;
            if len == 0 || len > 64 { return -1; }
            let name_bytes = match read_user_bytes(arg1, len) {
                Some(b) => b,
                None    => return -1,
            };
            let name = match core::str::from_utf8(name_bytes) {
                Ok(s)  => s,
                Err(_) => return -1,
            };
            let ep_id = match crate::ipc::names::lookup(name) {
                Some(id) => id,
                None     => return -1,
            };
            // Use the persistent capability table (append-only GLOBAL_RESOURCES)
            // rather than the routing table, which recycles dead slots under
            // concurrent respawns — reading routing::get_generation after a kill
            // can race with another service's register() overwriting that slot.
            let rid = crate::capability::cap::ResourceId::from(ep_id);
            let gen = crate::capability::get_resource_generation(rid)
                .unwrap_or(crate::capability::generation::Generation::INITIAL);
            gen.0 as i64
        }
        _ => -1,
    }
}

// ---------------------------------------------------------------------------
// Syscall: QueryCapRights (14) — read the rights bitfield of a cap slot.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot.
///
/// Returns the `Rights` byte of the cap at `slot` as a non-negative i64, or
/// -2 (`CapNotHeld`) if the slot is empty or out of range.
fn handle_query_cap_rights(slot: u64) -> i64 {
    match scheduler::current_task_read_cap_rights(slot as usize) {
        Some(rights) => rights.0 as i64,
        None         => cap_err_to_i64(CapError::CapNotHeld),
    }
}

// ---------------------------------------------------------------------------
// Syscall: RemoveCap (15) — remove a cap slot from the calling task's table.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot.
///
/// Clears the cap at `slot`. Always returns 0; out-of-range slots are silently
/// ignored (idempotent — the slot is already empty).
fn handle_remove_cap(slot: u64) -> i64 {
    scheduler::current_task_remove_cap(slot as usize);
    0
}

// ---------------------------------------------------------------------------
// Syscall: TaskStat (16) — read task state for a given slot index.
// ---------------------------------------------------------------------------

/// arg0 = slot (u32), arg1 = buf_ptr (user VA), arg2 = buf_len (must be ≥ 72).
///
/// Requires the INTROSPECT capability (READ) — discloses any task's state (§3.1).
///
/// Buffer layout (72 bytes):
///   [0]       valid:       u8  (1 = live, 0 = dead/unused)
///   [1]       state:       u8  (0=Ready, 1=Running, 2=BlockedOnRecv, 3=BlockedOnSend, 4=Dead)
///   [2]       core:        u8
///   [3]       pad:         u8
///   [4..8]    name_len:    u32 LE
///   [8..16]   mem_used:    u64 LE
///   [16..24]  mem_limit:   u64 LE
///   [24..56]  name:        [u8; 32] (truncated; zero-padded)
///   [56..60]  generation:  u32 LE
///   [60]      queue_depth: u8
///   [61..64]  pad:         [u8; 3]
///   [64..72]  run_ticks:   u64 LE
///
/// Returns 0 on success, -1 on invalid args.
fn handle_task_stat(slot: u64, buf_ptr: u64, buf_len: u64) -> i64 {
    const STAT_SIZE: usize = 72;
    // TaskStat discloses any task's full snapshot — requires INTROSPECT (READ)
    // (§3.1; docs/introspection-capability.md).
    if !scheduler::current_task_holds_resource(
        crate::capability::INTROSPECT_RESOURCE, Rights::READ)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    if buf_len < STAT_SIZE as u64 { return -1; }
    if !validate_user_ptr(buf_ptr, STAT_SIZE) { return -1; }

    let stat = scheduler::task_stat(slot as usize);

    let name_bytes = stat.name.as_bytes();
    let copy_len   = name_bytes.len().min(32);
    let name_len   = copy_len as u32;

    let mut buf = [0u8; STAT_SIZE];
    buf[0] = stat.valid as u8;
    buf[1] = stat.state;
    buf[2] = stat.core as u8;
    // buf[3] = 0 (pad, already zeroed)
    buf[4..8].copy_from_slice(&name_len.to_le_bytes());
    buf[8..16].copy_from_slice(&stat.mem_used.to_le_bytes());
    buf[16..24].copy_from_slice(&stat.mem_limit.to_le_bytes());
    buf[24..24 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    buf[56..60].copy_from_slice(&stat.generation.to_le_bytes());
    buf[60] = stat.queue_depth;
    // buf[61..64] = 0 (pad, already zeroed)
    buf[64..72].copy_from_slice(&stat.run_ticks.to_le_bytes());

    if write_user_bytes(buf_ptr, &buf) { 0 } else { -1 }
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleRead (17) — block until one byte is available on COM1 RX.
// ---------------------------------------------------------------------------

fn handle_console_read(cap_slot: u64) -> i64 {
    use crate::capability::CONSOLE_READ_RESOURCE;
    use core::sync::atomic::Ordering;

    // Validate cap: must hold CONSOLE_READ_RESOURCE with READ right.
    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_READ_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    // Store our slot as waiter before entering the block loop to avoid a
    // lost-wakeup race with the IRQ handler.
    let my_slot = scheduler::current_task_slot();
    crate::arch::x86_64::CONSOLE_READ_WAITER.store(my_slot as u32, Ordering::Release);

    loop {
        // Try to consume a byte from the ring buffer.
        if let Some(b) = crate::arch::x86_64::uart_rx_pop() {
            crate::arch::x86_64::CONSOLE_READ_WAITER.store(u32::MAX, Ordering::Release);
            return b as i64;
        }

        // Block until the IRQ handler wakes us.
        let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
        if err != 0 {
            crate::arch::x86_64::CONSOLE_READ_WAITER.store(u32::MAX, Ordering::Release);
            return err;
        }
        // Woken by uart_rx_irq_handler; loop to pop the byte.
    }
}

// ---------------------------------------------------------------------------
// Syscall: TryConsoleRead (24) — non-blocking console read.
// ---------------------------------------------------------------------------

/// Pop one byte from the console ring without blocking. A foreground full-screen
/// app (live `observe`) uses this to poll for `q` between repaints, since it
/// cannot afford to block in `ConsoleRead`. Does NOT register as the console
/// waiter (it never sleeps).
///
/// Returns the byte (0..=255) if one is available, `NO_CONSOLE_BYTE` (256) if the
/// ring is empty, or a negative cap error.
fn handle_try_console_read(cap_slot: u64) -> i64 {
    use crate::capability::CONSOLE_READ_RESOURCE;
    const NO_CONSOLE_BYTE: i64 = 256;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_READ_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    match crate::arch::x86_64::uart_rx_pop() {
        Some(b) => b as i64,
        None    => NO_CONSOLE_BYTE,
    }
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleEcho (25) — enable/disable keystroke echo.
// ---------------------------------------------------------------------------

/// Turn console keystroke echo on (`arg1 != 0`) or off (`arg1 == 0`). A
/// foreground app disables echo while it owns the screen and re-enables it on
/// exit. Gated by CONSOLE_READ (only services that consume the keyboard may
/// control its echo).
fn handle_console_echo(cap_slot: u64, on: u64) -> i64 {
    use crate::capability::CONSOLE_READ_RESOURCE;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_READ_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    crate::arch::x86_64::set_console_echo(on != 0);
    0
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleBootComplete (26) — end boot-log mirroring + clear the screen.
// ---------------------------------------------------------------------------

/// End boot-log mirroring to the framebuffer and clear the TV, handing over a
/// clean interactive console. The shell calls this once, on the first keystroke,
/// after the boot sequence has been displayed. Gated by CONSOLE_READ (only the
/// keyboard-owning service decides when boot output is dismissed).
fn handle_console_boot_complete(cap_slot: u64) -> i64 {
    use crate::capability::CONSOLE_READ_RESOURCE;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_READ_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    crate::arch::x86_64::console_boot_complete();
    0
}

// ---------------------------------------------------------------------------
// Syscall: SignalInputReady (27) — input driver reports setup complete.
// ---------------------------------------------------------------------------

/// The USB keyboard driver (xHCI) calls this once it finishes setup, in every
/// terminal path. As the last subsystem to come up, its report is the
/// deterministic end-of-boot signal the shell uses to auto-clear the boot screen.
/// Gated by CONSOLE_PUSH (held only by the input driver, §12) so no other service
/// can fake "boot done".
fn handle_signal_input_ready(cap_slot: u64) -> i64 {
    use crate::capability::CONSOLE_PUSH_RESOURCE;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_PUSH_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    crate::arch::x86_64::set_input_ready();
    0
}

// ---------------------------------------------------------------------------
// Syscall: TaskCaps (28) — list the capabilities held by a task.
// ---------------------------------------------------------------------------

/// arg0 = slot, arg1 = buf_ptr (user VA), arg2 = buf_len (bytes).
///
/// Writes up to `buf_len / 16` entries describing the target task's held caps,
/// returns the count. Each 16-byte entry: [0..8] resource_id u64 LE, [8] rights
/// u8, [9..16] pad. Requires INTROSPECT (READ) — discloses a task's authority
/// (the in-OS form of `osdev caps`, §17; makes authority visible per §26.9).
///
/// Best-effort snapshot (see `scheduler::for_each_cap_of`). Returns -1 on bad args.
fn handle_task_caps(slot: u64, buf_ptr: u64, buf_len: u64) -> i64 {
    const ENTRY: usize = 16;
    const MAX_ENTRIES: usize = 64; // CapTable holds at most 64 slots

    if !scheduler::current_task_holds_resource(
        crate::capability::INTROSPECT_RESOURCE, Rights::READ)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let cap = (buf_len as usize / ENTRY).min(MAX_ENTRIES);
    if cap == 0 { return 0; }

    // Collect into a kernel buffer first; do not touch user memory inside the
    // iteration closure.
    let mut tmp = [0u8; ENTRY * MAX_ENTRIES];
    let mut n = 0usize;
    scheduler::for_each_cap_of(slot as usize, |c| {
        if n < cap {
            let o = n * ENTRY;
            tmp[o..o + 8].copy_from_slice(&c.resource_id.0.to_le_bytes());
            tmp[o + 8] = c.rights.0;
            n += 1;
        }
    });

    let bytes = n * ENTRY;
    if !validate_user_ptr(buf_ptr, bytes) { return -1; }
    if write_user_bytes(buf_ptr, &tmp[..bytes]) { n as i64 } else { -1 }
}

// ---------------------------------------------------------------------------
// Syscall: ConsolePush (20) — inject a byte into the console input ring.
// Gated by CONSOLE_PUSH_RESOURCE (held only by the USB keyboard driver, §12)
// so an arbitrary service cannot forge keystrokes into the shell.
// ---------------------------------------------------------------------------

fn handle_console_push(cap_slot: u64, byte: u64) -> i64 {
    use crate::capability::CONSOLE_PUSH_RESOURCE;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::WRITE) {
        Ok(c) => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_PUSH_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    crate::arch::x86_64::console_push_byte(byte as u8);
    0
}

// ---------------------------------------------------------------------------
// Syscall: Reboot (18) — hardware reset via keyboard controller CPU reset line.
// ---------------------------------------------------------------------------

/// No arguments. Does not return.
///
/// Phase 5: no capability check — intended for dev-mode use by the shell
/// service (same rationale as Kill/8). Logs to serial before resetting so
/// the operator sees confirmation in PuTTY before the line goes silent.
fn handle_reboot() -> i64 {
    crate::kprintln!("reboot: hardware reset");
    crate::arch::x86_64::hardware_reset();
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
