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
        n if n == SyscallNumber::TrySend        as u64 => handle_try_send(arg0, arg1, arg2),
        n if n == SyscallNumber::Yield          as u64 => {
            crate::task::scheduler::yield_current();
            0
        }
        n if n == SyscallNumber::Log            as u64 => handle_log(arg0, arg1, arg2),
        n if n == SyscallNumber::AllocMem       as u64 => handle_alloc_mem(arg0),
        n if n == SyscallNumber::Spawn          as u64 => handle_spawn(arg0, arg1, arg2),
        n if n == SyscallNumber::Kill           as u64 => handle_kill(arg0, arg1),
        n if n == SyscallNumber::Abort          as u64 => handle_abort(arg0, arg1),
        n if n == SyscallNumber::AcquireSendCap as u64 => handle_acquire_send_cap(arg0, arg1, arg2),
        n if n == SyscallNumber::SendWithCap    as u64 => handle_send_with_cap(arg0, arg1, arg2),
        n if n == SyscallNumber::TakePendingCap as u64 => handle_take_pending_cap(),
        n if n == SyscallNumber::InspectKernel  as u64 => handle_inspect_kernel(arg0, arg1, arg2),
        n if n == SyscallNumber::QueryCapRights as u64 => handle_query_cap_rights(arg0),
        n if n == SyscallNumber::RemoveCap      as u64 => handle_remove_cap(arg0),
        n if n == SyscallNumber::TaskStat       as u64 => handle_task_stat(arg0, arg1, arg2),
        n if n == SyscallNumber::ConsoleRead    as u64 => handle_console_read(arg0),
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
    if crate::task::kill_by_name(name) { 0 } else { -1 }
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
            core::mem::forget(frame);
            return -1;
        }
        // Transfer frame ownership to the page table (freed when task dies).
        core::mem::forget(frame);
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
    match query_id {
        0 => scheduler::current_task_alloc_bytes() as i64,
        1 => crate::ipc::routing::count_live_endpoints() as i64,
        3 => read_cycle_counter() as i64,
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
/// No capability required — read-only kernel state, consistent with InspectKernel.
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
