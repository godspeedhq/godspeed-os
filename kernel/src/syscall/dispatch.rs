// SPDX-License-Identifier: GPL-2.0-only
//! Syscall entry point and dispatch - §8.2, §7.5.
//!
//! Every syscall validates the supplied capability before performing any
//! privileged action. No capability → no action; no exceptions (§3.1).
//!
//! Syscall numbers are fixed; adding a syscall requires a new number and a
//! capability that authorises it.

use crate::arch::imp::{read_user_bytes, validate_user_ptr, write_user_bytes, read_cycle_counter};
use crate::arch::imp::page_tables::{map_in_active_tables, PageFlags};
use crate::capability::cap::CapError;
use crate::capability::rights::Rights;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message, MAX_MESSAGE_SIZE};
use crate::memory::allocator::{alloc_frame, zero_frame};
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
    // 9 = removed. Was `Abort`: an UNGATED syscall any task could fire to panic the kernel - the §3.1
    //     hole found by the syscall audit. A service that hits a fatal error dies and is restarted by
    //     the supervisor; it does not get to abort the kernel. Number 9 now falls through to
    //     UnknownSyscall. (`init`, its only caller, was removed in Phase 5.)
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
    SpawnWithCaps          = 39,
    ConsoleForeground      = 40,
    Call                   = 41,
    /// `Call` with a DEADLINE. Same reply-cap semantics, bounded wait.
    ///
    /// The SDK had a deadline variant already, hand-rolled as send + `RecvTimeout` on the endpoint -
    /// and `RecvTimeout` takes whatever is next, so a service awaiting its reply would consume an
    /// unrelated CLIENT REQUEST, discard it for having the wrong tag, and lose it. That is what left
    /// `fs` clients waiting forever and desynced the block protocol on both boards. `Call` never had
    /// that flaw because it dequeues the reply specifically (`call_dequeue`); it simply could not be
    /// bounded. This is `Call` with the bound, so the correct primitive is also the usable one.
    CallDeadline           = 50,
    NetFrameTx             = 42,
    NetFrameRx             = 43,
    NetInfo                = 44,
    FireIrq                = 51,
    Gpio                   = 45,
    UsbDiskInfo            = 46,
    UsbDiskRead            = 47,
    UsbDiskWrite           = 48,
    UsbDiskFlush           = 49,
    /// Spawn a task from an image the CALLER supplies, described by a `SpawnRequest` in its own
    /// memory (step C, `docs/service-ownership.md`). The kernel stops holding a catalogue of what
    /// each service IS; it loads what it is handed, and enforces what the request may ask for.
    SpawnImage             = 52,
    /// One complete PCI configuration read: select and fetch, indivisibly. Gated by `PCI_CFG`.
    ///
    /// ONE syscall rather than a select/read pair, because the pair is stateful: split, two callers
    /// (or a caller and the KERNEL, which uses the same registers on the spawn and kill paths) read
    /// whichever register the other selected. Atomic here, that window does not exist - and a caller
    /// can no longer WRITE anything at all, which is strictly less authority than the pair carried.
    PciCfgRead             = 53,
}

/// Raw syscall dispatcher - called from the SYSCALL/SYSENTER IDT stub.
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
        n if n == SyscallNumber::Call           as u64 => handle_call(arg0, arg1, arg2),
        n if n == SyscallNumber::CallDeadline   as u64 => handle_call_deadline(arg0, arg1, arg2),
        n if n == SyscallNumber::NetFrameTx     as u64 => handle_net_frame_tx(arg0, arg1),
        n if n == SyscallNumber::NetFrameRx     as u64 => handle_net_frame_rx(arg0, arg1),
        n if n == SyscallNumber::NetInfo        as u64 => handle_net_info(arg0),
        n if n == SyscallNumber::FireIrq        as u64 => handle_fire_irq(arg0),
        n if n == SyscallNumber::Gpio           as u64 => handle_gpio(arg0, arg1),
        n if n == SyscallNumber::UsbDiskInfo    as u64 => handle_usb_disk_info(),
        n if n == SyscallNumber::UsbDiskRead    as u64 => handle_usb_disk_read(arg0, arg1),
        n if n == SyscallNumber::UsbDiskWrite   as u64 => handle_usb_disk_write(arg0, arg1),
        n if n == SyscallNumber::UsbDiskFlush   as u64 => handle_usb_disk_flush(),
        n if n == SyscallNumber::Yield          as u64 => {
            crate::task::scheduler::yield_current();
            0
        }
        n if n == SyscallNumber::Log            as u64 => handle_log(arg0, arg1, arg2),
        n if n == SyscallNumber::AllocMem       as u64 => handle_alloc_mem(arg0),
        n if n == SyscallNumber::Spawn          as u64 => handle_spawn(arg0, arg1, arg2),
        n if n == SyscallNumber::SpawnImage     as u64 => handle_spawn_image(arg0, arg1, arg2),
        n if n == SyscallNumber::SpawnReturningEndpoint as u64 => handle_spawn_returning_endpoint(arg0, arg1, arg2),
        n if n == SyscallNumber::SpawnWithCaps as u64 => handle_spawn_with_caps(arg0, arg1, arg2),
        n if n == SyscallNumber::Kill           as u64 => handle_kill(arg0, arg1),
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
        n if n == SyscallNumber::ConsoleForeground as u64 => handle_console_foreground(arg0, arg1),
        n if n == SyscallNumber::ConsoleBootComplete as u64 => handle_console_boot_complete(arg0),
        n if n == SyscallNumber::SignalInputReady as u64 => handle_signal_input_ready(arg0),
        n if n == SyscallNumber::TaskCaps as u64 => handle_task_caps(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceMint   as u64 => handle_resource_mint(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceInvoke as u64 => handle_resource_invoke(arg0, arg1, arg2),
        n if n == SyscallNumber::ResourceRevoke as u64 => handle_resource_revoke(arg0),
        n if n == SyscallNumber::LastRecvBadge  as u64 => scheduler::take_last_recv_badge() as i64,
        n if n == SyscallNumber::PciCfgRead as u64 => handle_pci_cfg_read(arg0, arg1),
        _ => -1, // Unknown syscall.
    }
}

// ---------------------------------------------------------------------------
// Syscall: Log (5) - write a message to the kernel ring buffer.
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
// Syscall: Print (22) - like Log but WITHOUT a trailing newline.
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
// Syscall: ConsoleWrite (23) - write to the interactive console (serial + TV).
// ---------------------------------------------------------------------------

/// arg0 = cap_slot, arg1 = pointer to UTF-8 bytes, arg2 = byte length.
///
/// Requires `Rights::WRITE` on `LOG_WRITE_RESOURCE` (Stage 1; Stage 2 gives the
/// console service a dedicated cap). Unlike `Log`/`Print` (which now go to the
/// log stream = serial only), this writes the CONSOLE path - serial AND the
/// framebuffer - for interactive output (the shell prompt, `observe`). No newline
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
    // Console foreground gate: while a TUI app (e.g. `chaos`, syscall 40) owns the screen, a
    // backgrounded task's output goes to serial only - it must not smear the app's framebuffer. The
    // owner (or unclaimed = the normal case) writes to both.
    let to_fb = crate::arch::imp::console_foreground_allows(scheduler::current_task_slot() as u32);
    // A console write costs on BOTH sides of this boundary: the serial port is synchronous at
    // 115200 baud (~87 us a byte, so ~9 ms for a 100-byte line), and the delivery below can PARK
    // this task when the service's 16-deep queue is full.
    //
    // Both were measured, and the answer was the queue: writers parked ~25 ms each because the
    // console's drain loop was not reaching its paint. That is fixed in the service, and the
    // counters used to find it are gone - a measurement kept after it has answered its question is
    // just a tax on the hot path (three rdtsc reads per write, here).
    crate::arch::imp::console_write_bytes_gated(bytes, to_fb);
    // Serial is written FIRST and unconditionally above, so the log is complete and never duplicated
    // even though the call below can park this task. What the return value carries is the outcome of
    // the wait, not of the write: 0, or a negative code if the terminal died while we were blocked.
    if to_fb {
        return deliver_to_console_service(bytes);
    }
    0
}

/// Console writes that could not be shown because the terminal was gone, not merely behind.
static CONSOLE_LOST: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// Report every Nth loss. The report goes to serial, and serial is the thing under load.
const CONSOLE_LOSS_REPORT: u64 = 100;

fn deliver_to_console_service(bytes: &[u8]) -> i64 {
    // Never feed the console's own output back to it. Nothing does this today (the service logs through
    // the serial log path, not the console path), but the loop it would make is unbounded and silent.
    if scheduler::task_stat(scheduler::current_task_slot()).name == "console" {
        return 0;
    }
    // No terminal on this machine (or not up yet): serial already has the bytes, which is the whole
    // guarantee. Return without blocking - there is nothing to wait for.
    let Some(ep) = crate::ipc::names::lookup("console") else { return 0 };
    let Ok(msg) = crate::ipc::message::Message::new(bytes) else { return 0 };

    let my_slot = scheduler::current_task_slot();
    match crate::ipc::routing::enqueue_from_kernel_blocking(ep, msg, my_slot) {
        Ok(Some(receiver_slot)) => {
            scheduler::wake_by_slot(receiver_slot, 0);
            0
        }
        Ok(None) => 0,
        // The terminal is behind. We are now recorded as a blocked sender, so park until it drains -
        // ordinary bounded-queue back-pressure (§8.5/§8.6), the same thing any `send` to a full endpoint
        // does. The KERNEL is not waiting; the task that produced the output is, which is correct: it
        // should not run ahead of the display it is writing to. It wakes with a negative code if the
        // terminal dies instead, so this can never hang.
        Err(crate::ipc::IpcError::QueueFull) => {
            scheduler::block_and_reschedule(TaskState::BlockedOnSend)
        }
        // The terminal died between the name lookup and the enqueue. Serial has the bytes; say so
        // periodically rather than silently painting nothing (invariant 12).
        Err(_) => {
            let n = CONSOLE_LOST.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            if n % CONSOLE_LOSS_REPORT == 0 {
                crate::kprintln!("console: {} write(s) not displayed - no terminal (serial has them)", n);
            }
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Syscall: Send / Recv / TrySend (1, 2, 3) - Milestone 5/6.
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

    // enqueue atomically records us as a blocked sender if QueueFull -
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
/// SEC-7: narrow an embedded cap for its RECEIVER before installing it. A DELEGATED-resource cap
/// (a file/socket, §7.10) is installed WITHOUT GRANT - the owning service (e.g. fs) mints it with
/// GRANT only so it can transfer it (§8.5 rule 1: an embedded cap must be grantable), but the
/// recipient only USES it (invokes); it must not re-delegate it - the owner controls delegation by
/// minting fresh caps. Endpoint caps are returned unchanged: re-delegating an endpoint (e.g. the
/// shell wiring a pipe stage, or a reply cap) is a legitimate part of the IPC model, so GRANT is
/// preserved for those. This is the default "narrow to need" §8.5 wants, enforced at the boundary.
fn narrow_embedded_for_receiver(cap: crate::capability::cap::Capability) -> crate::capability::cap::Capability {
    if crate::capability::delegated::is_delegated(cap.resource_id) {
        cap.without_grant()
    } else {
        cap
    }
}

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
                        if let Ok(new_slot) = scheduler::current_task_insert_cap(narrow_embedded_for_receiver(embedded_cap)) {
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
/// `TRY_RECV_EMPTY` instead of blocking when the queue is empty - so a busy-polling driver
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
                    if let Ok(new_slot) = scheduler::current_task_insert_cap(narrow_embedded_for_receiver(embedded_cap)) {
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
    // cycles - the timed-wake scan runs on the BSP and compares one shared tick clock, which is
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
                        if let Ok(new_slot) = scheduler::current_task_insert_cap(narrow_embedded_for_receiver(embedded_cap)) {
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
                // Woken by a sender (message ready) or the timer (deadline) - re-check.
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
    crate::arch::imp::ioapic::unmask_vector(irq);
    0
}

/// Block the calling task for roughly `cycles` TSC cycles, then return (syscall 37). A real
/// sleep - the core can `hlt` while the task is parked - so a service that needs to wait (e.g.
/// a foreground UI polling for `q` between repaints, or the shell waiting for that UI to exit)
/// does NOT busy-`yield`, which would peg its core at ~100% and make every task on that core
/// read as fully busy in `observe`. Like `yield`, sleeping your own task needs no capability.
/// Uses the same BSP-tick timed-wake as `recv_timeout` (§12); a `cycles` of 0 returns at once.
fn handle_sleep(cycles: u64) -> i64 {
    if cycles == 0 { return 0; }
    let my_slot = scheduler::current_task_slot();

    // SUB-TICK SLEEPS GO TO THE MICROSECOND ONE-SHOT - RE-ENABLED, with the reason it was pulled now
    // understood and fixed elsewhere.
    //
    // This was withdrawn after a kernel panic: core 0 wedged under typing, and a queue lock shared
    // between this syscall path and an interrupt handler was the obvious suspect. It was the wrong
    // suspect twice over. The instrumented panic named the real one - `last source 0x10`, the mailbox
    // doorbell, frozen mid-handler - which was re-entering the scheduler from an asynchronous
    // interrupt; that is fixed, and three minutes of real typing plus 2m39s of synthetic storm now
    // pass with no panic. And `SpinLock::lock` calls `irq_save`, so interrupts are masked while the
    // queue lock is held: the self-deadlock I withdrew this for could not have happened.
    //
    // Withdrawing it was still right at the time. The evidence then pointed here, and shipping an
    // unproven mechanism into a machine that was panicking would have made the next measurement
    // unreadable. What changed is not confidence, it is evidence.
    #[cfg(target_arch = "arm")]
    {
        let us = scheduler::cycles_to_us(cycles);
        if us > 0 && us < 10_000 {
            match crate::arch::imp::irq::hires_arm(my_slot as u32, us as u32) {
                // The delay passed while we were arming it: the wait is already served, so return
                // rather than block. Blocking here made a 125 us sleep take 8 ms, because the compare
                // fires on EQUALITY and had nothing left to match.
                crate::arch::imp::irq::Armed::Elapsed => return 0,
                crate::arch::imp::irq::Armed::Full => {}   // fall through to the tick path
                crate::arch::imp::irq::Armed::Pending => {
                    // Tick backstop, always: if the compare interrupt never arrives the task must
                    // still wake. A timing optimisation that can hang `sleep` would hang every service
                    // that paces itself.
                    let deadline = scheduler::monotonic_ticks()
                        .wrapping_add(scheduler::cycles_to_ticks(cycles).max(1));
                    scheduler::set_wake_deadline(my_slot, deadline);
                    let _ = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
                    scheduler::clear_wake_deadline(my_slot);
                    crate::arch::imp::irq::hires_release(my_slot as u32);
                    return 0;
                }
            }
        }
    }

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

    // Pass None for blocked_sender_slot - QueueFull is returned directly.
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
    let mut msg = Message::new(bytes).map_err(|e| ipc_err_to_i64(e))?;
    // Stamp the sender's primary endpoint (kernel-set, unforgeable by userspace - the payload cannot
    // influence it). Every user send path funnels through here, so a blocked `Call` can correlate its
    // reply by WHO sent it (`call_dequeue`), instead of trusting queue arrival order - which handed a
    // caller whatever message reached its queue first and desynced fs's reply stream on hardware.
    // A task with no endpoint stamps 0, which matches no target.
    msg.sender_ep = scheduler::current_task_endpoint().map(|e| e.0).unwrap_or(0);
    Ok(msg)
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
/// Probe parameters ride in the UPPER 32 bits of `arg0`, which `Spawn` never used:
/// `[55..48] flags  [47..32] probe mode  [31..16] core  [15..0] spawn cap slot`.
/// No new syscall and no change in arity - see `docs/probe-params-design.md`.
const SPAWN_FLAG_HAS_RECV:  u64 = 1 << 48;
const SPAWN_FLAG_SMALL_MEM: u64 = 1 << 49;
const SPAWN_FLAG_IS_PROBE:  u64 = 1 << 50;

/// Upper bound on the name payload (`name` + NUL-separated peer names). It was 64, which held a
/// name alone; a peer list needs more. Bounded and small (26.6) - the longest real payload is
/// well under half of this.
const SPAWN_PAYLOAD_MAX: usize = 128;


// ---------------------------------------------------------------------------------------------
// SpawnImage (52) - the caller supplies the image
// ---------------------------------------------------------------------------------------------

/// What a spawner tells the kernel about the task it wants started.
///
/// Fixed layout, fetched ONCE (see `handle_spawn_image`), and VERSIONED so a mismatch is a loud
/// refusal rather than a misparse. Every field is here because step C or step D needs it, even where
/// the kernel does not honour it yet: the ABI is the expensive thing to change, so it is shaped for
/// the end state now and the interim is a REFUSAL, never a silent ignore
/// (`docs/service-ownership.md`, "design C's spawn ABI for the end state").
#[repr(C)]
#[derive(Clone, Copy)]
struct SpawnRequest {
    /// Must equal `SPAWN_REQUEST_VERSION`. A spawner built against a different kernel is refused.
    version:      u32,
    /// bit0 has_recv_endpoint, bit1 has_console_read.
    flags:        u32,
    image_ptr:    u64,
    image_len:    u64,
    name_ptr:     u64,
    name_len:     u32,
    /// `u32::MAX` = let the kernel round-robin (9.2).
    core:         u32,
    memory_limit: u64,
    /// Privilege bitmask (SPAWN, CONSOLE_PUSH, INTROSPECT, SERVICE_CONTROL, RESOURCE_MINT, REBOOT,
    /// ACQUIRE_ANY, ...). NOT honoured yet - must be 0.
    privileges:   u32,
    /// The DEVICE CLASS this service drives (`task::hw_class_of`), or 0 for none.
    ///
    /// A CLASS rather than raw MMIO/DMA addresses, and that is not a shortcut: the kernel keeps a
    /// PERMANENT PHYSICAL DMA RESERVATION PER DEVICE, reused across restarts so a respawned driver
    /// gets the same arena its controller may still be pointing at. That is per-device kernel state,
    /// so the request has to identify the DEVICE - an address cannot express it. The kernel resolves
    /// the class to what its own bus scan found; moving the SCAN is step D.
    hw_flags:     u32,
    /// Device MMIO window to grant. NOT honoured yet - must be 0.
    mmio_base:    u64,
    mmio_len:     u64,
    /// DMA arena size in pages, PCI BDF, and IRQ lines. NOT honoured yet - must be 0.
    dma_pages:    u32,
    bdf:          u32,
    irq_count:    u32,
    irqs:         [u8; 4],
    /// NUL-separated peer names, resolved through the kernel name directory exactly as a contract's
    /// `send_peers` are: the caller supplies the LIST, never the authority.
    peers_ptr:    u64,
    peers_len:    u32,
    /// Caller-provided caps to install into the child, as `[label_len][label][slot_lo][slot_hi]`
    /// repeated - the SAME encoding `SpawnWithCaps` already uses, deliberately, so there is one wire
    /// format for this and not two.
    installs_ptr:   u64,
    installs_count: u32,
    /// The service's MODE. Named `probe_mode` in `ServiceConfig` because probes were its first user,
    /// but it is a general mode selector: `observe` reads it to choose one-shot / live / foreground,
    /// and picks the LIVE LOOP when it is 0. Omitting it made `observe-now` spin at 100% CPU flooding
    /// the console until the shell blocked on a send - a service that moved, spawned, ran, and did
    /// the WRONG THING.
    probe_mode:     u32,
}

/// Wire size of a `SpawnRequest`, in bytes. Every field at a fixed little-endian offset:
///
/// ```text
///   0 version u32     36 core         u32     72 dma_pages u32
///   4 flags   u32     40 memory_limit u64     76 bdf       u32
///   8 image_ptr u64   48 privileges   u32     80 irq_count u32
///  16 image_len u64   52 hw_flags     u32     84 irqs      [u8;4]
///  24 name_ptr  u64   56 mmio_base    u64     88 peers_ptr u64
///  32 name_len  u32   64 mmio_len     u64     96 peers_len u32
///                                            100 reserved  u32
///                                            104 installs_ptr   u64
///                                            112 installs_count u32
///                                            116 reserved       u32
///                                            120 probe_mode     u32
///                                            124 reserved       u32
/// ```
///
/// These are exactly the offsets a `repr(C)` layout of the SDK's matching struct produces on both
/// 32- and 64-bit targets (every `u64` lands 8-aligned), so the two agree by construction - but the
/// KERNEL's copy is the definition, and it reads each field at a stated offset rather than trusting
/// that agreement.
const SPAWN_REQUEST_BYTES: usize = 128;

impl SpawnRequest {
    /// Decode the request from its wire bytes.
    ///
    /// EXPLICIT rather than a `repr(C)` reinterpret, deliberately. Casting the buffer to a struct
    /// would make the ABI depend on two separate crates agreeing about padding and alignment, and it
    /// would need `unsafe` in `syscall/`, which is one of 18.5's grandfathered floors - growable only
    /// by amendment. Reading each field at a stated offset costs a few lines, needs no `unsafe`, and
    /// makes the wire format something you can read off the source instead of infer from a layout.
    fn decode(b: &[u8]) -> Self {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| u64::from_le_bytes([
            b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7]]);
        Self {
            version:      u32_at(0),
            flags:        u32_at(4),
            image_ptr:    u64_at(8),
            image_len:    u64_at(16),
            name_ptr:     u64_at(24),
            name_len:     u32_at(32),
            core:         u32_at(36),
            memory_limit: u64_at(40),
            privileges:   u32_at(48),
            hw_flags:     u32_at(52),
            mmio_base:    u64_at(56),
            mmio_len:     u64_at(64),
            dma_pages:    u32_at(72),
            bdf:          u32_at(76),
            irq_count:    u32_at(80),
            irqs:         [b[84], b[85], b[86], b[87]],
            peers_ptr:    u64_at(88),
            peers_len:    u32_at(96),
            installs_ptr:   u64_at(104),
            installs_count: u32_at(112),
            probe_mode:     u32_at(120),
        }
    }
}

/// Bumped to 2 when `installs` was added. The version field exists for exactly this: a spawner
/// built against a different kernel is REFUSED loudly rather than misparsing a shorter struct.
const SPAWN_REQUEST_VERSION:  u32 = 3;
const SPAWN_FLAG_REQ_RECV:    u32 = 1 << 0;
const SPAWN_FLAG_REQ_CONSOLE: u32 = 1 << 1;
/// `core` is a STRICT placement (a restart's `--core N`), not a table's PREFERRED core. See §9.2 and
/// the SDK constant of the same name for why conflating the two stops a machine booting.
const SPAWN_FLAG_CORE_STRICT: u32 = 1 << 2;
/// Mint the child's peer caps with GRANT (§22 Test 5A). See the SDK constant.
const SPAWN_FLAG_PEERS_GRANT: u32 = 1 << 3;
/// Ceiling on a caller-requested DMA arena, in 4 KiB pages. 2048 = 8 MiB, comfortably above the
/// largest real request (xHCI's scratchpad, 1168 KiB) and far below anything that could starve the
/// frame allocator.
const MAX_DMA_PAGES: u32 = 2048;

fn handle_spawn_image(req_ptr: u64, req_len: u64, spawn_cap_slot: u64) -> i64 {
    // The SPAWN capability gates this exactly as it gates `Spawn` (3.1): supplying an image does not
    // supply the authority to start it.
    let cap = match scheduler::current_task_lookup_cap(spawn_cap_slot as usize, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != crate::capability::SPAWN_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    // AND `IMAGE_SPAWN` ON TOP OF IT - a SECOND, distinct authority, because these are two different
    // powers that were sharing one capability.
    //
    // `SPAWN` means "start a service the system already knows". This syscall means "start ARBITRARY
    // BYTES, under a name you choose", which is a power that did not exist before step C moved the
    // images out of the kernel (`docs/service-ownership.md`): while the kernel held every image, a
    // spawner could only ask for one the kernel already had. Afterwards this had to accept bytes from
    // userspace, and it was gated by the capability the SHELL happens to hold for unrelated reasons -
    // as do `chaos`, `control` and every probe. Any of them could have introduced new code under a
    // real service's name in the window while that service was dead, and clients reacquiring by name
    // (§14.3) would have wired themselves to it.
    //
    // Checked against the caller's own table rather than a slot it passes, exactly as
    // `privileges_caller_lacks` checks a delegated privilege: the authority is a property of the
    // caller, not of an argument it chooses.
    if !scheduler::current_task_holds_resource(
        crate::capability::IMAGE_SPAWN_RESOURCE, Rights::WRITE)
    {
        crate::kprintln!("task: SpawnImage refused - caller holds SPAWN but not IMAGE_SPAWN (starting arbitrary bytes is the supervisor's authority alone; ask it via supcmd::SPAWN)");
        return cap_err_to_i64(CapError::CapNotHeld);
    }

    if req_len as usize != SPAWN_REQUEST_BYTES {
        crate::kprintln!("task: SpawnImage refused - request is {} bytes (kernel expects {})",
            req_len, SPAWN_REQUEST_BYTES);
        return -1;
    }
    // ONE fetch. Every decision below is taken from this copy, so the caller cannot rewrite a field
    // between its validation and its use - the same double-fetch discipline the loader applies to
    // the ELF header window.
    let raw = match read_user_bytes(req_ptr, req_len as usize) {
        Some(b) => b,
        None => {
            crate::kprintln!("task: SpawnImage refused - request buffer {:#x} not readable", req_ptr);
            return -1;
        }
    };
    let req = SpawnRequest::decode(raw);

    if req.version != SPAWN_REQUEST_VERSION {
        crate::kprintln!("task: SpawnImage refused - request version {} (kernel speaks {})",
            req.version, SPAWN_REQUEST_VERSION);
        return -1;
    }

    // NOT-YET-HONOURED FIELDS ARE REFUSED, NOT IGNORED (invariant 12). A spawner asking for an MMIO
    // window or a privilege this kernel does not yet grant must hear so, rather than receive a task
    // that silently lacks what it needs and fails later somewhere unrelated.
    // `hw_flags` (the device class) is honoured below. The RAW address fields are still refused, and
    // refusing rather than ignoring is the rule (invariant 12): a spawner asking for an MMIO window
    // this kernel will not grant must hear so, not receive a task that silently lacks it and fails
    // somewhere unrelated. They stay refused because the kernel resolves addresses from the class -
    // see the field comment - and will keep doing so until the bus scan itself moves (step D).
    // An INTERRUPT VECTOR is authority exactly as a physical address is (`task::hw_irqs_for`), and
    // it was the one hardware field still passed through from the caller - so a spawner could name a
    // vector it had no claim to while being refused the addresses beside it. Refused now: the kernel
    // derives a driver's vectors from the device class it names.
    if req.irq_count != 0 {
        crate::kprintln!(
            "task: SpawnImage refused - interrupt vectors are not honoured; name the device CLASS instead (irq_count={})",
            req.irq_count);
        return -1;
    }
    // `dma_pages` is now HONOURED for a PCI descriptor: a SIZE is not an address, so a caller
    // naming one cannot reach memory it was not granted - the arena is still allocated by the
    // kernel, wherever the kernel decides. Bounded, because an unbounded size is a denial of
    // service (§26.6). The ADDRESS fields stay refused.
    if req.dma_pages > MAX_DMA_PAGES {
        crate::kprintln!("task: SpawnImage refused - dma_pages {} exceeds the cap of {}",
                         req.dma_pages, MAX_DMA_PAGES);
        return -1;
    }
    if req.mmio_base != 0 || req.mmio_len != 0 || req.bdf != 0 {
        crate::kprintln!(
            "task: SpawnImage refused - raw hardware addresses are not honoured; name the device CLASS instead (mmio={:#x}+{:#x} dma_pages={} bdf={:#x})",
            req.mmio_base, req.mmio_len, req.dma_pages, req.bdf);
        return -1;
    }
    if !crate::task::hw_class_known(req.hw_flags) {
        crate::kprintln!("task: SpawnImage refused - unknown device class {}", req.hw_flags);
        return -1;
    }

    // A CALLER MAY NOT GRANT ITSELF AUTHORITY IT DOES NOT HOLD.
    //
    // This is the whole security content of the privileges field, and it is the same rule
    // `SpawnWithCaps` applies to an installed cap (8.5, 7.3): a spawner may hand a child only what it
    // could already hand it. So each requested privilege is checked against the CALLER's own holdings
    // - the supervisor holds SPAWN and SERVICE_CONTROL and may therefore pass them on; it does not
    // hold REBOOT, so it cannot mint one for a child no matter what it asks for.
    //
    // Without this the field would be exactly the ambient authority 3.1 forbids: "ask and receive".
    if req.privileges != 0 {
        if let Some(missing) = crate::task::privileges_caller_lacks(req.privileges) {
            crate::kprintln!(
                "task: SpawnImage refused - caller asked to grant '{}' which it does not hold itself",
                missing);
            return -1;
        }
    }

    let name_len = req.name_len as usize;
    if name_len == 0 || name_len > 64 {
        crate::kprintln!("task: SpawnImage refused - name length {} (must be 1..=64)", name_len);
        return -1;
    }
    let name_bytes = match read_user_bytes(req.name_ptr, name_len) { Some(b) => b, None => return -1 };
    let mut name_buf = [0u8; 64];
    name_buf[..name_len].copy_from_slice(name_bytes);
    let name = match core::str::from_utf8(&name_buf[..name_len]) { Ok(s) => s, Err(_) => return -1 };

    // Peers: the same NUL-separated form and the same bound as the probe path.
    let mut peer_buf = [0u8; SPAWN_PAYLOAD_MAX];
    let mut peers: [&str; crate::task::MAX_SEND_PEERS] = [""; crate::task::MAX_SEND_PEERS];
    let mut np = 0usize;
    let plen = req.peers_len as usize;
    if plen > 0 {
        if plen > SPAWN_PAYLOAD_MAX {
            crate::kprintln!("task: SpawnImage refused - peer payload {} bytes (max {})",
                plen, SPAWN_PAYLOAD_MAX);
            return -1;
        }
        let pb = match read_user_bytes(req.peers_ptr, plen) { Some(b) => b, None => return -1 };
        peer_buf[..plen].copy_from_slice(pb);
        let ps = match core::str::from_utf8(&peer_buf[..plen]) { Ok(s) => s, Err(_) => return -1 };
        for part in ps.split('\0') {
            if part.is_empty() { continue; }
            if np >= crate::task::MAX_SEND_PEERS {
                crate::kprintln!("task: SpawnImage refused - more than {} peers",
                    crate::task::MAX_SEND_PEERS);
                return -1;
            }
            peers[np] = part;
            np += 1;
        }
    }

    if req.image_len == 0 || req.image_len > u32::MAX as u64 {
        crate::kprintln!("task: SpawnImage refused - image length {}", req.image_len);
        return -1;
    }
    // STRICT placement vs a PREFERENCE (9.2) - see `SPAWN_FLAG_CORE_STRICT`. A caller's explicit
    // `--core N` must be honoured or rejected; a table's preferred core must fall back to
    // round-robin so a machine with fewer ready cores still comes up (11.3).
    let strict = req.flags & SPAWN_FLAG_CORE_STRICT != 0;
    let core_override  = if req.core == u32::MAX || !strict { None } else { Some(req.core) };
    let core_preferred = if req.core == u32::MAX || strict  { u32::MAX } else { req.core };

    // Caller-provided caps for the child's peers. Same encoding and the SAME AUTHORITY RULE as
    // `SpawnWithCaps`: the caller must already hold each cap WITH GRANT, so copying it into the child
    // is non-escalating (7.3) - it could have transferred the whole cap anyway. Every step is bounds
    // checked because the descriptor is untrusted.
    use crate::task::{InstallCap, PEER_NAME_BYTES};
    let mut installs = [InstallCap { name: [0u8; PEER_NAME_BYTES], name_len: 0, cap };
                        crate::task::MAX_SEND_PEERS];
    let ni = req.installs_count as usize;
    if ni > crate::task::MAX_SEND_PEERS {
        crate::kprintln!("task: SpawnImage refused - {} installs (max {})",
            ni, crate::task::MAX_SEND_PEERS);
        return -1;
    }
    if ni > 0 {
        // Each entry is at least 1 + 1 + 2 bytes; the buffer is read once, like every other input.
        let ilen = ni * (2 + PEER_NAME_BYTES + 2);
        let ibuf = match read_user_bytes(req.installs_ptr, ilen.min(512)) {
            Some(b) => b,
            None    => return -1,
        };
        let mut p = 0usize;
        for entry in installs.iter_mut().take(ni) {
            if p >= ibuf.len() { return -1; }
            let label_len = ibuf[p] as usize;
            p += 1;
            if label_len == 0 || label_len > PEER_NAME_BYTES || p + label_len + 2 > ibuf.len() {
                return -1;
            }
            let mut nm = [0u8; PEER_NAME_BYTES];
            nm[..label_len].copy_from_slice(&ibuf[p..p + label_len]);
            p += label_len;
            let slot = (ibuf[p] as usize) | ((ibuf[p + 1] as usize) << 8);
            p += 2;
            let held = match scheduler::current_task_lookup_cap(slot, Rights::GRANT) {
                Ok(c)  => c,
                Err(e) => return cap_err_to_i64(e),
            };
            entry.name     = nm;
            entry.name_len = label_len as u8;
            entry.cap      = held;
        }
    }

    match crate::task::spawn_from_image(
        name,
        crate::loader::ImageSource::User { base: req.image_ptr, len: req.image_len as usize },
        core_override,
        core_preferred,
        req.memory_limit,
        req.flags & SPAWN_FLAG_REQ_RECV    != 0,
        req.flags & SPAWN_FLAG_REQ_CONSOLE != 0,
        &peers[..np],
        if ni > 0 { Some(&installs[..ni]) } else { None },
        req.privileges,
        req.probe_mode,
        req.hw_flags,
        req.dma_pages,
        req.flags & SPAWN_FLAG_PEERS_GRANT != 0,
    ) {
        // Hand back a SEND|GRANT cap to the new endpoint, as `SpawnReturningEndpoint` does: the
        // spawner has to be able to record `name -> cap` for the service it just started, or it
        // cannot wire dependents to it and cannot re-wire them after a restart.
        //
        // Returned as SLOT + 1, so 0 unambiguously means "spawned, but this service has no recv
        // endpoint". `SpawnReturningEndpoint` returns the bare slot and therefore cannot tell slot 0
        // from no-endpoint; not repeating that here.
        Ok(Some(ep_id)) => {
            let rid    = crate::capability::cap::ResourceId::from(ep_id);
            let ep_cap = crate::capability::mint_cap(rid, Rights::SEND | Rights::GRANT);
            match scheduler::current_task_insert_cap(ep_cap) {
                Ok(slot) => slot as i64 + 1,
                Err(e)   => cap_err_to_i64(e),
            }
        }
        Ok(None) => 0,
        Err(_)   => -1,
    }
}

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
    if len == 0 || len > SPAWN_PAYLOAD_MAX { return -1; }
    let payload = match read_user_bytes(name_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };
    let payload = match core::str::from_utf8(payload) {
        Ok(s)  => s,
        Err(_) => return -1,
    };

    // THE PROBE PATH IS GONE. `Spawn` used to carry a test probe's parameters packed into the
    // unused upper bits of `arg0` - the kernel supplying the image and looking its two authorities up
    // by name. The image moved to the supervisor, so there is nothing here to spawn: a probe is now
    // an ordinary `SpawnImage`, and a probe that needs a victim asks the supervisor for one.
    match crate::task::spawn_service_by_name(payload, core_override) {
        Ok(_)  => 0,
        Err(_) => -1,
    }
}

/// Syscall: SpawnReturningEndpoint (38). Like Spawn (7), but on success mints a `SEND|GRANT`
/// cap to the new service's recv endpoint and inserts it into the **caller's** cap table,
/// returning the slot. This is the Phase-0 seam for moving naming out of the kernel
/// (`docs/naming-design.md`): a spawner (the supervisor) can collect a cap to every service it
/// starts - a userspace `name → cap` map - without the kernel resolving names for third parties.
/// The old name-wiring path is unchanged; this is purely additive.
///
/// arg0 = packed (spawn_cap_slot in low 16, core in next 16; core 0xFFFF = round-robin).
/// arg1 = name ptr, arg2 = name len. Returns the endpoint cap slot (≥0), or a negative error
/// (cap error, or -1 if the spawn failed / the service has no recv endpoint to hand back).
fn handle_spawn_returning_endpoint(packed_arg0: u64, name_ptr: u64, name_len: u64) -> i64 {
    let spawn_cap_slot = (packed_arg0 & 0xFFFF) as usize;
    let core_raw       = ((packed_arg0 >> 16) & 0xFFFF) as u32;
    let core_override  = if core_raw == 0xFFFF { None } else { Some(core_raw) };

    // Validate the SPAWN capability (same gate as Spawn - §3.1).
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
                Ok(slot) => slot as i64 + 1,
                Err(e)   => cap_err_to_i64(e),
            }
        }
        // SLOT + 1 above, so 0 can mean "spawned, but this service has no recv endpoint" without
        // colliding with slot 0. It used to return -1 here, which is what a FAILED spawn returns -
        // so a caller could not tell a service that started and has no endpoint (`mem-pressure`,
        // `greet`, `roster`) from one that did not start at all.
        //
        // That was latent while only the shell's `spawncap` used this: it only ever spawned services
        // that HAVE endpoints. Routing the supervisor's spawns through it surfaced the ambiguity as
        // "chaos spawn-storm hit the ceiling at spawn #1" - a successful spawn read as a refusal.
        Ok(None) => 0,
        Err(_)   => -1,
    }
}

/// Syscall: SpawnWithCaps (39) - the full Phase-0 spawn protocol (`docs/naming-design.md`). Spawns
/// a service whose send-peers are wired from **caller-supplied caps** (not the kernel name table),
/// then returns a `SEND|GRANT` cap to the new endpoint (like SpawnReturningEndpoint). This is how
/// the supervisor wires a dependent from its `name → cap` map without the kernel resolving names.
/// The old name-wiring path is untouched (this is a distinct syscall).
///
/// arg0 = packed (spawn_cap_slot low 16, core next 16; 0xFFFF = round-robin).
/// arg1 = ptr, arg2 = len of a descriptor: `[name_len:u8, name…, count:u8,
///        {label_len:u8, label…, slot_lo:u8, slot_hi:u8} × count]` (count ≤ MAX_SEND_PEERS).
/// Each `slot` names a cap the CALLER holds; the kernel copies it (GRANT-validated, non-escalating
/// §7.3) into the child under `label`. Returns the endpoint cap slot (≥0), or a negative error.
fn handle_spawn_with_caps(packed_arg0: u64, buf_ptr: u64, buf_len: u64) -> i64 {
    let spawn_cap_slot = (packed_arg0 & 0xFFFF) as usize;
    let core_raw       = ((packed_arg0 >> 16) & 0xFFFF) as u32;
    let core_override  = if core_raw == 0xFFFF { None } else { Some(core_raw) };

    // Validate the SPAWN capability (same gate as Spawn - §3.1). Reuse it as the array filler.
    let spawn_cap = match scheduler::current_task_lookup_cap(spawn_cap_slot, Rights::WRITE) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if spawn_cap.resource_id != crate::capability::SPAWN_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }

    let len = buf_len as usize;
    if len < 2 || len > 512 { return -1; }
    let buf = match read_user_bytes(buf_ptr, len) {
        Some(b) => b,
        None    => return -1,
    };

    // Parse the descriptor with bounds checks at every step (untrusted input).
    let name_len = buf[0] as usize;
    let mut p = 1usize;
    if name_len == 0 || name_len > 64 || p + name_len > len { return -1; }
    let name = match core::str::from_utf8(&buf[p..p + name_len]) { Ok(s) => s, Err(_) => return -1 };
    p += name_len;
    if p >= len { return -1; }
    let count = buf[p] as usize;
    p += 1;
    if count > crate::task::MAX_SEND_PEERS { return -1; }

    // Build the install list - for each entry, copy the caller's cap (GRANT-validated).
    use crate::task::{InstallCap, PEER_NAME_BYTES, MAX_SEND_PEERS};
    let mut installs = [InstallCap { name: [0u8; PEER_NAME_BYTES], name_len: 0, cap: spawn_cap }; MAX_SEND_PEERS];
    for entry in installs.iter_mut().take(count) {
        if p >= len { return -1; }
        let label_len = buf[p] as usize;
        p += 1;
        if label_len == 0 || label_len > PEER_NAME_BYTES || p + label_len + 2 > len { return -1; }
        let mut nm = [0u8; PEER_NAME_BYTES];
        nm[..label_len].copy_from_slice(&buf[p..p + label_len]);
        p += label_len;
        let slot = (buf[p] as usize) | ((buf[p + 1] as usize) << 8);
        p += 2;
        // The caller must hold this cap WITH GRANT - copying it into the child is then
        // non-escalating (§7.3): the caller could already transfer the whole cap.
        let held = match scheduler::current_task_lookup_cap(slot, Rights::GRANT) {
            Ok(c)  => c,
            Err(e) => return cap_err_to_i64(e),
        };
        entry.name     = nm;
        entry.name_len = label_len as u8;
        entry.cap      = held;
    }

    match crate::task::spawn_service_by_name_with_installs(name, core_override, &installs[..count]) {
        Ok(Some(ep_id)) => {
            let rid    = crate::capability::cap::ResourceId::from(ep_id);
            let ep_cap = crate::capability::mint_cap(rid, Rights::SEND | Rights::GRANT);
            match scheduler::current_task_insert_cap(ep_cap) {
                Ok(slot) => slot as i64,
                Err(e)   => cap_err_to_i64(e),
            }
        }
        Ok(None) => -2, // spawned OK, but the service has no recv endpoint (a producer like `greet`)
        Err(_)   => -1, // spawn failed
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
/// Requires the `SERVICE_CONTROL` capability - validated by holdings below (§3.1 / §14.4;
/// `docs/service-control-cap.md`).
fn handle_kill(name_ptr: u64, name_len: u64) -> i64 {
    // §3.1 / §14.4: killing a service is a privileged action - it requires the
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
    // Path C / Phase 6: NO service is unkillable via this syscall - the only truly unkillable thing is
    // the kernel itself. The `supervisor` used to be rejected here (it was the non-restartable trusted
    // root); it is now **restartable** - the kernel respawns it on death, unconditionally and forever
    // (a bound would just re-introduce the reboot and hand an attacker a DoS - see
    // `task::poll_supervisor_respawn`). So a SERVICE_CONTROL holder (the `chaos` utility, the operator
    // control channel) may kill it, and the kernel recovers it. (`fs` and `block-driver` are
    // restartable too.) The shell still refuses a
    // *casual* `kill supervisor`/`restart supervisor` at the command layer (CORE_SERVICES); deliberate
    // chaos goes through `chaos kill-storm supervisor`.
    if crate::task::kill_by_name(name) {
        // A kill bumps the dead endpoint's generation and could (if a bug let it
        // target a trusted service) take down the TCB. Now that the kill has
        // completed and no kernel locks are held, verify the two invariants a
        // kill is most likely to break:
        //   §6.2 - every TCB service (the supervisor) is still alive;
        //          TCB death is a loud, unrecoverable failure, not a silent one.
        //   §7.8 - the cap table is still consistent (no cap carries a generation
        //          beyond its resource's current generation). The generation bump
        //          only ever moves resources forward, so all surviving caps stay
        //          stale-or-current. This is an O(active-caps) walk; the kill path
        //          is not a per-syscall hot path, so it is an acceptable home for
        //          the §7.8 check (see invariants/CLAUDE.md).
        crate::invariants::assertions::assert_tcb_alive();
        crate::invariants::assertions::assert_cap_table_consistent();
        // If the caller killed ITSELF (a SERVICE_CONTROL holder self-terminating - e.g. `chaos` at the
        // end of a run, so it does not linger in `observe`), it is now Dead. Do NOT return into the dead
        // task: its next instruction would hit "no running task" in block_and_reschedule. Switch away,
        // exactly like kill_current's tail; this never returns. A non-self kill falls through to Ok.
        if scheduler::current_task_is_dead() {
            scheduler::yield_current();
        }
        0
    } else { -1 }
}

/// arg0 = name_ptr, arg1 = name_len, arg2 = include_grant (0 = SEND only, 1 = SEND|GRANT).
///
/// Looks up `name` in the kernel name directory, mints a SEND (or SEND|GRANT)
/// cap to that endpoint in the calling task's cap table, and returns the slot.
///
/// Reacquire a fresh SEND cap to a named service (§14.2). **Gated (§3.1, see the in-body comment):**
/// the caller must hold `ACQUIRE_ANY` (operator/test) or declare `name` as a contract send-peer
/// (recovery). `arg2=1` also requests the GRANT right (cap-transfer tests, P3).
/// Has this name already been reported as absent from the directory? Records it if not.
///
/// Bounded and heap-free (§26.6.1): a fixed table of names already warned about. When it fills, no new
/// names are reported - a deliberate ceiling, because the alternative is an unbounded log on a serial
/// wire that is already the system's bottleneck. Sixteen distinct names is more than the whole managed
/// roster, so filling it means something is acquiring names nobody declared.
fn warn_once_for(name: &str) -> bool {
    const MAX_NAMES: usize = 16;
    const NAME_MAX: usize = 32;
    static SEEN: crate::smp::SpinLock<([[u8; NAME_MAX]; MAX_NAMES], [u8; MAX_NAMES], usize)> =
        crate::smp::SpinLock::new(([[0; NAME_MAX]; MAX_NAMES], [0; MAX_NAMES], 0));
    let b = name.as_bytes();
    if b.len() > NAME_MAX { return true; } // too long to remember; report it every time rather than never
    let mut g = SEEN.lock();
    let n = g.2;
    for i in 0..n {
        if g.1[i] as usize == b.len() && g.0[i][..b.len()] == *b { return false; }
    }
    if n == MAX_NAMES { return false; }
    let len = b.len();
    g.0[n][..len].copy_from_slice(b);
    g.1[n] = len as u8;
    g.2 = n + 1;
    true
}

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

    // §3.1: minting a SEND cap to a named service is privileged. Ungated, this was ambient send
    // authority (any task could acquire send rights to any service). Allowed only if the caller holds
    // the ACQUIRE_ANY capability (the operator/test instruments - shell, supervisor, probes - that
    // legitimately reach arbitrary services, e.g. chaos flooding / pipe sinks), OR `name` is one of the
    // caller's contract-declared send-peers (recovery: reacquiring a peer after it restarted, §13/§14.2).
    let broad = scheduler::current_task_holds_resource(
        crate::capability::ACQUIRE_ANY_RESOURCE, Rights::WRITE);
    if !broad && !crate::task::current_task_declares_peer(name) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }

    let ep_id = match crate::ipc::names::lookup(name) {
        Some(id) => id,
        None     => {
            // SAID, not swallowed - but ONCE PER NAME, because saying it every time was worse than
            // not saying it.
            //
            // This and the cap-table refusal below both returned a bare -1, so a service that could
            // not reacquire a peer had no way to learn WHY, and reported the peer as unresponsive. On
            // hardware that sent the reader after a `block-driver` which was alive the whole time with
            // an empty queue (invariant 12).
            //
            // Unconditional, it then produced 394 lines in one chaos run - 2.5% of the log, 293 of
            // them for a single name - on a wire already carrying 73% of its capacity. This refusal is
            // TRANSIENT and EXPECTED: peers are acquired before they are spawned at boot, and again
            // while one is mid-restart. The caller retries and recovers. The FIRST occurrence of a
            // name is the diagnostic; the next three hundred are the instrument competing with the
            // thing it is measuring. The cap-table refusal below is NOT rate-limited: it is rare,
            // permanent, and the one that actually strands a service.
            if warn_once_for(name) {
                crate::kprintln!(
                    "acquire: '{}' is not in the name directory - nothing to acquire (further occurrences for this name are not logged)", name);
            }
            return -1;
        }
    };

    let resource_id = crate::capability::cap::ResourceId::from(ep_id);
    // SEC-6: the GRANT right is only for the operator/test instruments (ACQUIRE_ANY holders) that
    // legitimately re-delegate reached caps - chaos flooding, pipe-sink wiring, the cap-transfer
    // tests (P3). A declared-peer acquirer is an ordinary service reacquiring its OWN peer for
    // recovery (§14.2); it only sends to it, never re-delegates it, so it gets SEND-only regardless
    // of `include_grant`. This stops a service self-minting a re-delegatable cap to a declared peer
    // (narrow to need, §8.5) - GRANT now follows the instrument permission, not the caller's request.
    let rights = if include_grant != 0 && broad {
        crate::capability::Rights::SEND | crate::capability::Rights::GRANT
    } else {
        crate::capability::Rights::SEND
    };
    let cap = crate::capability::mint_cap(resource_id, rights);

    match scheduler::current_task_insert_cap(cap) {
        Ok(slot) => slot as i64,
        Err(_)   => {
            // The caller's own table is full - the peer is fine. This is the failure that makes a
            // transient outage PERMANENT: once a service cannot hold one more cap it can never
            // reacquire anything again, so it stays broken until it is restarted. Naming it is the
            // difference between diagnosing that in one boot and chasing the peer for several.
            crate::kprintln!(
                "acquire: '{}' resolved, but the caller's capability table is FULL - it cannot hold                  the cap. This service will not recover until it is restarted.", name);
            -1
        }
    }
}

/// Syscall: DeriveCap (29) - duplicate a capability the caller holds **with GRANT**
/// into a fresh slot. arg0 = held cap slot. Returns the new slot, or a negative
/// cap-error code.
///
/// This is the primitive that lets a service hand out many copies of one held endpoint
/// cap: it derives a copy per recipient and grants that copy away (via `SendWithCap`)
/// while retaining the original. Sound and
/// non-escalating (§7.3): the copy carries the *same* resource, generation, and
/// rights - never wider - and the GRANT gate means the caller could already transfer
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
// Syscall: SendWithCap (11) - send a message with an embedded capability.
// ---------------------------------------------------------------------------

/// arg0 = (grant_slot << 16) | endpoint_slot
/// arg1 = msg_ptr (user VA)
/// arg2 = msg_len
///
/// Validates SEND on the endpoint cap and GRANT on the cap to transfer.
/// Embeds the cap in the message, enqueues, then removes the cap from the
/// sender's table (§7.6 - cap moved exactly once).
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
        Err(e) => ipc_err_to_i64(e), // failure before delivery - cap stays
    }
}

// ---------------------------------------------------------------------------
// Syscall: Call (41) - synchronous request + death-aware wait for the reply (§8, §8.6).
// ---------------------------------------------------------------------------

/// arg0 = (reply_grant_slot << 16) | target_endpoint_slot
/// arg1 = buf_ptr (in/out, MAX_MESSAGE_SIZE): the request payload on entry; the reply is written
///        back into the same buffer on return.
/// arg2 = (recv_slot << 16) | request_length. Three 16-bit slots + the length are packed into two
///        32-bit-safe args (NOT one 48-bit arg): the ARM 32-bit syscall ABI carries each arg in one
///        register and would truncate a value above 32 bits, dropping `recv_slot` (userspace-audit
///        A-U1). A request never exceeds one 4 KiB message (< 0xFFFF), so `recv` rides the high half.
///
/// seL4-style synchronous CALL. Sends the request to the `target` endpoint carrying `reply_grant`
/// as a one-shot reply cap (the caller's own endpoint, so the peer can reply to it), then blocks on
/// the caller's own endpoint (`recv_slot`) for the reply. It wakes with the reply (normal), or - if
/// `target` dies before replying - with `ReplyDead` (returns -12). This is the reply-side twin of
/// the existing "blocked sender wakes with `EndpointDead` when its target endpoint closes" (§8.6):
/// same generation/liveness mechanism, surfaced on the *reply* wait. The kernel learns only about a
/// **reply cap** and its death semantics - never "request/reply" (§26.10): it tracks the target
/// endpoint a blocked caller awaits and wakes it if that endpoint dies. Lets an interdependent
/// service wait on truth without hanging (Commandment VIII).
///
/// Returns the reply length (>= 0), or a negative error: -12 `ReplyDead` (peer died awaiting reply),
/// -7 `EndpointDead` (peer dead before the request was delivered), or a cap error.
fn handle_call(packed: u64, buf_ptr: u64, recv_len: u64) -> i64 {
    let target_slot = (packed & 0xFFFF) as usize;
    let reply_slot  = ((packed >> 16) & 0xFFFF) as usize;
    let recv_slot   = ((recv_len >> 16) & 0xFFFF) as usize;
    let req_len     = recv_len & 0xFFFF;
    // `Call` (41): the SDK allocates its own MAX_PAYLOAD buffer internally, so the full message size
    // is the honest capacity here - this is the assumption `CallDeadline` could not make.
    do_call(target_slot, reply_slot, recv_slot, buf_ptr, req_len, 0, MAX_MESSAGE_SIZE)
}

/// `Call` with a deadline in SECONDS. Three cap slots pack into `a0` (8 bits each - a task holds at
/// most `MAX_CAPS_PER_TASK` = 64, so six bits suffice and eight leave room), because the ARM 32-bit
/// ABI truncates every argument to one register and the original three-16-bit-field layout had no
/// space left for a fourth value. Length and deadline share `a2`.
fn handle_call_deadline(slots: u64, buf_ptr: u64, len_secs: u64) -> i64 {
    let target_slot = (slots & 0xFF) as usize;
    let reply_slot  = ((slots >> 8) & 0xFF) as usize;
    let recv_slot   = ((slots >> 16) & 0xFF) as usize;
    let req_len     = len_secs & 0xFFFF;
    // How big the caller's reply buffer is, as a power-of-two class the SDK packs into arg0's spare
    // nibble (see `call_deadline_into`). 0 means an SDK that predates this and could not say - treat
    // that as the old assumption so an unpatched caller is no worse off than before.
    let reply_buf_cap = { let c = (slots >> 24) & 0xF; if c == 0 { MAX_MESSAGE_SIZE } else { (1usize << c).min(MAX_MESSAGE_SIZE) } };
    let secs        = (len_secs >> 16) & 0xFFFF;
    do_call(target_slot, reply_slot, recv_slot, buf_ptr, req_len, secs, reply_buf_cap)
}

/// The body both share. `deadline_secs` of 0 blocks forever, exactly as `Call` always has.
fn do_call(
    target_slot: usize, reply_slot: usize, recv_slot: usize,
    buf_ptr: u64, req_len: u64, deadline_secs: u64, reply_buf_cap: usize,
) -> i64 {

    // 1. Validate the three caps: SEND to the peer, GRANT on the reply cap, RECV on our own endpoint.
    let target_cap = match scheduler::current_task_lookup_cap(target_slot, Rights::SEND) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let target_ep = EndpointId(target_cap.resource_id.0);

    // CapInsufficientRights -> CapNotGrantable so the caller learns the reply cap was NOT transferred.
    let reply_cap = match scheduler::current_task_lookup_cap(reply_slot, Rights::GRANT) {
        Ok(c)  => c,
        Err(CapError::CapInsufficientRights) => return cap_err_to_i64(CapError::CapNotGrantable),
        Err(e) => return cap_err_to_i64(e),
    };

    let recv_cap = match scheduler::current_task_lookup_cap(recv_slot, Rights::RECV) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    let recv_ep = EndpointId(recv_cap.resource_id.0);

    // §3.1 (no ambient authority): every leg below required a validated cap (SEND/GRANT/RECV).
    crate::invariants::assertions::assert_cap_validated(&Ok(()));

    // 2. The buffer is in/out: read the request from it now, write the reply back into it later, so
    //    validate it for the full reply capacity (the SDK always passes a MAX_MESSAGE_SIZE buffer).
    if req_len as usize > MAX_MESSAGE_SIZE { return ipc_err_to_i64(IpcError::MessageTooLarge); }
    if !validate_user_ptr(buf_ptr, MAX_MESSAGE_SIZE) { return -1; }

    // 3. Build the request with the reply cap embedded (mirrors SendWithCap).
    let mut msg = match build_message(buf_ptr, req_len) {
        Ok(m)  => m,
        Err(e) => return e,
    };
    msg.caps[0]   = Some(reply_cap);
    msg.cap_count = 1;

    let my_slot = scheduler::current_task_slot();

    // 4. Send the request to the peer, removing the reply cap once it is delivered/enqueued (exactly
    //    as SendWithCap). On a full queue we block as a sender; the peer dying there wakes us with
    //    EndpointDead via the existing kill_endpoint blocked-sender path (§8.6).
    match crate::ipc::routing::enqueue(target_ep, msg, target_cap.generation, Some(my_slot)) {
        Ok(Some(receiver_slot)) => {
            scheduler::current_task_remove_cap(reply_slot);
            scheduler::wake_by_slot(receiver_slot, 0);
        }
        Ok(None) => {
            scheduler::current_task_remove_cap(reply_slot);
        }
        Err(IpcError::QueueFull) => {
            // The request (with reply cap) is now the routing table's pending-send; remove our copy.
            scheduler::current_task_remove_cap(reply_slot);
            let err = scheduler::block_and_reschedule(TaskState::BlockedOnSend);
            if err != 0 { return err; }   // peer died before the request was delivered
            // else: queue drained, request delivered - fall through to await the reply.
        }
        // Failure before delivery (peer already dead): the reply cap was NOT transferred and stays in
        // the caller's table; the SDK reclaims it.
        Err(e) => return ipc_err_to_i64(e),
    }

    // 5. Await the reply on our own endpoint, waking on the reply OR on the peer's death (ReplyDead).
    //    The loop re-evaluates after every wake, so a reply that arrived just before the peer died
    //    still wins over ReplyDead (call_dequeue returns the queued reply first).
    // Same shape as `handle_recv_timeout`'s bounded wait, deliberately: a deadline in monotonic
    // ticks, checked before each block, armed with `set_wake_deadline` so the timer wakes us, and
    // cleared on every exit. Reusing the proven pattern rather than inventing a second one.
    //
    // A tick IS a scheduler quantum and the quantum is 10 ms (§9.1), so a second is 100 of them.
    const TICKS_PER_SEC: u64 = 100;
    let deadline = if deadline_secs == 0 {
        0
    } else {
        scheduler::monotonic_ticks().wrapping_add(deadline_secs.saturating_mul(TICKS_PER_SEC))
    };

    // WHERE A SLOW CALL ACTUALLY SPENDS ITS TIME - blocked, or going round this loop.
    //
    // `fs` measures ~870 ms for one 512-byte block round trip while `block-driver` measures under
    // 5 ms for the same request, so the time is in this path and not in the device. Two shapes
    // remain and they are opposite bugs: ONE block that nobody woke for 870 ms (a wake that was
    // never delivered, recovered later by a timer tick - an idle core's tick is deliberately
    // slowed, which would also explain why this is FASTER when the machine is busy), or MANY
    // blocks and wakes that each found nothing (a livelock round this loop). The count separates
    // them, and reading the wake path has not: every link in it is correct.
    let call_c0 = read_cycle_counter();
    let call_h0 = scheduler::core_idle_halts(scheduler::current_core_id());
    let mut call_blocks: u32 = 0;
    let result = loop {
        match crate::ipc::routing::call_dequeue(recv_ep, recv_cap.generation, target_ep, my_slot) {
            Ok((reply, sender_to_wake)) => {
                if let Some(slot) = sender_to_wake {
                    scheduler::wake_by_slot(slot, 0);
                }
                scheduler::set_last_recv_badge(reply.badge_id, reply.badge_right);
                let n_caps = reply.cap_count.min(reply.caps.len());
                for i in 0..n_caps {
                    if let Some(embedded_cap) = reply.caps[i] {
                        if let Ok(new_slot) = scheduler::current_task_insert_cap(narrow_embedded_for_receiver(embedded_cap)) {
                            scheduler::push_pending_recv_cap(new_slot as u32);
                        }
                    }
                }
                let payload  = reply.payload_bytes();
                // REFUSE, do not truncate. The caller told us how much room it has; a reply that does
                // not fit is a protocol error the caller must see, and writing what fits would smash
                // whatever sits past the end of its buffer. Silent truncation on a message path is
                // the one thing §26.7 forbids outright.
                if payload.len() > reply_buf_cap {
                    crate::kprintln!(
                        "call: reply of {} bytes exceeds the caller's {}-byte buffer - refused (not truncated)",
                        payload.len(), reply_buf_cap);
                    break ipc_err_to_i64(IpcError::MessageTooLarge);
                }
                let copy_len = payload.len().min(reply_buf_cap);
                if !write_user_bytes(buf_ptr, &payload[..copy_len]) { break -1; }
                break copy_len as i64;
            }
            Err(IpcError::QueueEmpty) => {
                call_blocks = call_blocks.saturating_add(1);
                // call_dequeue recorded us as blocked-in-call awaiting target_ep; block now. The wake
                // result is intentionally ignored: we loop and let call_dequeue re-derive the terminal
                // condition (queued reply -> Ok; target dead -> ReplyDead; our endpoint dead -> EndpointDead).
                if deadline != 0 && scheduler::monotonic_ticks() >= deadline {
                    break RECV_TIMED_OUT;
                }
                if deadline != 0 {
                    scheduler::set_wake_deadline(my_slot, deadline);
                }
                let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
                if err != 0 { break err; }
            }
            Err(e) => break ipc_err_to_i64(e),   // ReplyDead (-12) or EndpointDead (-7)
        }
    };
    scheduler::clear_wake_deadline(my_slot);

    // Bounded and quiet: 100 ms is far past any healthy call, so a healthy machine prints nothing,
    // and past it only every 256th, so a slow system cannot drown itself in reports about being
    // slow - at 16 it did exactly that, adding a serial write inside the IPC path of a machine that
    // was already a second per round trip, and the keyboard stopped answering entirely
    // (26.6, 26.7). Uses the cycle counter this file already reads safely for InspectKernel, so the
    // grandfathered `unsafe` floor here (18.5) is untouched.
    let call_us = scheduler::cycles_to_us(read_cycle_counter().wrapping_sub(call_c0));
    if call_us >= 100_000 {
        static SLOW_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
        let n = SLOW_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 3 || n % 256 == 0 {
            crate::kprintln!(
                "call: slot {} waited {} us across {} blocks, {} core halts (slow #{})",
                my_slot, call_us, call_blocks,
                scheduler::core_idle_halts(scheduler::current_core_id())
                    .saturating_sub(call_h0),
                n);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Syscall: ResourceMint (30) - allocate a delegated resource + mint a cap (§7.10, P2).
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
        Err(_) => { delegated::release(id); return -1; } // cap table full - don't leak the id
    };
    if !write_user_bytes(out_id_ptr, &id.0.to_le_bytes()) {
        return -1;
    }
    slot as i64
}

// ---------------------------------------------------------------------------
// Syscall: ResourceInvoke (31) - use a delegated (file) cap (§7.10, P2).
// ---------------------------------------------------------------------------

/// arg0 = (right_bits << 24) | (reply_grant_slot << 12) | file_cap_slot - ONE 32-bit word, because a
/// syscall argument is one register and that register is 32 bits on a 32-bit target (see the SDK's
/// `resource_invoke`: a field above bit 31 is truncated away on arm32).
/// arg1 = msg_ptr (user VA), arg2 = msg_len.
///
/// The "use = send" of a delegated resource cap. Validates the file cap carries `right_bits`
/// (a READ-only cap invoking with WRITE fails `CapInsufficientRights` - non-escalation, §7.3),
/// then routes the message to the owning service's endpoint with the badge carried in the
/// **kernel-set `Message` fields** `badge_id`/`badge_right` (unforgeable - an ordinary `send`
/// leaves them 0), and an embedded reply cap exactly as `SendWithCap`. The owner reads the badge
/// (via `LastRecvBadge`) to know which resource + which right the kernel validated; it never
/// trusts the client, and the kernel never learns the operation.
fn handle_resource_invoke(packed: u64, msg_ptr: u64, msg_len: u64) -> i64 {
    use crate::capability::delegated;
    let file_slot  = (packed & 0xFFF) as usize;
    let reply_slot = ((packed >> 12) & 0xFFF) as usize;
    let right_bits = ((packed >> 24) & 0xFF) as u8;
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
    //    only this handler - after validating the cap above - sets it; an ordinary `send`
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
// Syscall: ResourceRevoke (32) - revoke a delegated resource you own (§7.10, P2).
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
// Syscall: TakePendingCap (12) - retrieve the next received cap slot.
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
// Syscall: AllocMem (6) - dynamic page allocation within the task's budget.
// ---------------------------------------------------------------------------

/// arg0 = size in bytes to allocate (must be > 0).
///
/// No capability required - the task's budget is implicitly granted at spawn
/// from the memory limit in its contract (§10.2, implicit authority).
///
/// Returns the virtual address of the newly-mapped region on success, or a
/// negative error code:
///   -11  AllocDenied - request would exceed the task's memory limit.
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
        // SEC-21: zero the frame before it becomes user-readable. `alloc_frame` may return a frame
        // still holding a dead task's contents (the allocator zeroes neither on alloc nor on free),
        // and AllocMem needs no capability, so an un-zeroed page would leak stale cross-task memory.
        // `zero_frame` keeps the `unsafe` in the permitted memory/ layer (§18.5); this stays safe.
        zero_frame(phys);
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
// Syscall: InspectKernel (13) - structured kernel state queries.
// ---------------------------------------------------------------------------

/// arg0 = query_id, arg1/arg2 = query-specific args.
///
/// query_id = 2: endpoint generation by name.
///   arg1 = name_ptr (user VA), arg2 = name_len.
///   Returns the current generation of the named endpoint as a non-negative
///   i64, or -1 if the name is not registered.
fn handle_inspect_kernel(query_id: u64, arg1: u64, arg2: u64) -> i64 {
    // Self-state (0 = own alloc bytes), the clock (3 = TSC), and console geometry
    // are ungated, as are the
    // boot/RTC reads (10, 11). Every other query discloses another task's or
    // system-wide state and requires the INTROSPECT capability with READ (§3.1;
    // docs/introspection-capability.md).
    if !matches!(query_id, 0 | 3 | 9 | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 | 19 | 20 | 21 | 22 | 23)
        && !scheduler::current_task_holds_resource(
            crate::capability::INTROSPECT_RESOURCE, Rights::READ)
    {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    match query_id {
        0 => scheduler::current_task_alloc_bytes() as i64,
        1 => crate::ipc::routing::count_live_endpoints() as i64,
        3 => read_cycle_counter() as i64,
        // (9 was the framebuffer console's rows/cols. DELETED: terminal geometry is derived from the
        // safe-area inset, the cell size and the font-scale rule, which now live in the `console`
        // SERVICE - so the kernel cannot answer it without keeping a second copy of facts it does not
        // own. The shell asks the service. docs/console-service.md 9.7.)
        // Input-ready flag - set by the xHCI driver when it finishes setup (the
        // last boot step). The shell watches it to auto-clear the boot screen.
        10 => crate::arch::imp::input_ready() as i64,
        // Wall-clock date/time from the hardware RTC, packed (see rtc.rs). Ungated
        // - the time of day is task-neutral hardware info, like the TSC (query 3).
        11 => crate::arch::imp::rtc::read_datetime() as i64,
        // Wall-clock datetime captured at boot (same packed layout as query 11). Pairs with
        // query 11 for `uptime` = now − boot, a portable wall-clock delta (a tick counter's rate
        // varies with the APIC timer mode). Task-neutral hardware info like the RTC, so ungated.
        12 => crate::arch::imp::rtc::boot_datetime() as i64,
        // Is the CALLING task the console foreground owner (may its console reads return bytes)? 1 if
        // foreground or unclaimed (normal), 0 if a foreground app (e.g. `chaos`, syscall 40) owns it.
        // Caller-specific, so ungated (like query 0). The muted shell polls this to stay quiet + redraw
        // its prompt only when it regains the keyboard.
        13 => crate::arch::imp::console_foreground_allows(scheduler::current_task_slot() as u32) as i64,
        // NIC vendor | device<<16 from the PCI scan (0 if no NIC). Task-neutral hardware info, ungated:
        // nic-driver reads it to know which chip it drives (e1000 vs RTL8168). Networking Phase 4.
        14 => crate::arch::imp::pci::NIC_VENDOR_DEVICE.load(core::sync::atomic::Ordering::Relaxed) as i64,
        // NIC MMIO base (the register-space BAR the PCI scan chose), 0 if none. Ungated hardware fact;
        // a diagnostic for the driver (which BAR did the memory-BAR scan pick). Networking Phase 4.
        15 => crate::arch::imp::pci::NIC_MMIO_BASE.load(core::sync::atomic::Ordering::Relaxed) as i64,
        // TSC ticks per 10 ms quantum, from the boot-time CPUID calibration (boot.rs). Ungated,
        // task-neutral timing (like the raw TSC, query 3): userspace turns a TSC delta into milliseconds
        // as `delta * 10 / this`. `ping` uses it for round-trip time. 0 if the TSC was not calibrated.
        16 => crate::arch::imp::boot::tsc_ticks_per_quantum() as i64,
        // Deglitched monotonic "now" in epoch seconds (rtc.rs now_epoch_monotonic): the wall clock with
        // backward / huge-forward CMOS misreads dropped. For time-DELTA deadlines
        // (request_with_reply_deadline) and pacing, where a raw RTC glitch (the "4383d" misread) would
        // expire a deadline instantly. Ungated task-neutral timing, like the raw RTC (query 11).
        17 => crate::arch::imp::rtc::now_epoch_monotonic(),
        // Hardware-driver presence from the PCI scan, packed: bit0 = xHCI, bit1 = EHCI, bit2 = a NIC
        // this build can actually drive (found AND an e1000 or RTL8168). Ungated task-neutral hardware
        // fact (like the NIC identity, query 14). The supervisor reads it to skip spawning a driver
        // whose hardware is absent (e.g. the Wyse 5070 has no EHCI; a diskless/NIC-less box), so an idle
        // driver does not busy-hold a whole core.
        18 => {
            use core::sync::atomic::Ordering::Relaxed;
            use crate::arch::imp::pci;
            let x = pci::XHCI_FOUND.load(Relaxed) as i64;
            let e = pci::EHCI_FOUND.load(Relaxed) as i64;
            // Only a NIC nic-driver can actually drive counts - an unsupported NIC leaves it idling
            // exactly like an absent one (matches the MMIO-grant gate).
            let nic = (pci::NIC_FOUND.load(Relaxed)
                && matches!(pci::NIC_VENDOR_DEVICE.load(Relaxed), 0x100E_8086 | 0x8168_10EC)) as i64;
            x | (e << 1) | (nic << 2)
        }
        // A hardware-random u32 (the SoC RNG on ARM, RDRAND on x86), or -1 if this build has no hardware
        // RNG. Ungated: reading entropy confers no authority (like the raw TSC, query 3). The `random`
        // shell utility consumes it. A u32 is always 0..2^32, so it never collides with the -1 sentinel.
        19 => match crate::arch::imp::hw_random() { Some(v) => v as i64, None => -1 },
        // 20 = the SD/EMMC controller's base clock in Hz, learned from the platform at boot. Task-neutral
        // hardware info like the console geometry (9) and the RTC (10/11), so ungated. The block driver
        // needs it to compute its clock divider: the controller's own capability register reports this
        // wrongly on the BCM283x, and a guessed divider runs the card's identification clock at the wrong
        // speed - which fails silently on hardware and not at all under emulation. 0 = unknown, and the
        // driver then reports rather than guessing.
        20 => crate::arch::imp::emmc_base_clock_hz() as i64,
        // 23 = THE BOARD'S OWN MAC ADDRESS, packed little-endian (byte 0 in bits 7:0), or -1.
        //
        // Ungated with the other board-identity reads (20 = the EMMC base clock): it discloses no
        // task's state and no system-wide state, only a number etched on the hardware and printed
        // on the sticker - which is on the wire in the source field of every frame the machine
        // sends, so it is not a secret being handed out.
        //
        // It exists because the NIC driver is a userspace service and this address lives in the
        // VideoCore mailbox, outside the DWC2 register window the service is granted. The driver
        // was therefore falling back to a locally-administered address that is HARDCODED and so
        // identical on every board running this system - two Pis on one network would claim the
        // same MAC. The alternative to this query is granting the driver a second, much wider MMIO
        // window for one identity fact, which is authority out of all proportion to the need.
        23 => match crate::arch::imp::board_mac_packed() { Some(v) => v as i64, None => -1 },
        // Clock PROVENANCE, packed: bits 0-7 = source (0 unset / 1 rtc / 2 ntp), bits 8.. = seconds since
        // the network last set it (0 if never). Ungated task-neutral timing info like the RTC (11) itself.
        // `date` reports this so a displayed time says where it came from - a fallback chain is only
        // mechanism, not magic, while its choice is visible (§26.4/§26.9).
        // 21: pop one byte from the COM2 operator channel, -1 when empty. TRANSPORT ONLY - the kernel
        // owns the UART (§11.4 sanctions it owning a serial console) and hands bytes out; the `control`
        // SERVICE decides what they mean (C1-6). This is the whole of what replaced a 123-line command
        // interpreter in ring 0.
        21 => match crate::arch::imp::com2_try_read_byte() { Some(b) => b as i64, None => -1 },
        // 22 REMOVED (clock slice 3): the wall clock's provenance, sync age and floor belong to
        // the `time` service now. The kernel still answers 11 (the raw RTC register read, which no
        // service can perform) and 17 (monotonic seconds, which paces deadlines) - transport and
        // scheduling. What it no longer answers is what the reading MEANS.
        // The persisted clock FLOOR in epoch seconds (0 = none known). A "we ran at least this late" bound,
        // never a reading - `date` shows it only when the time is unknown, explicitly labelled.
        4 => crate::memory::allocator::free_frame_count() as i64,
        5 => crate::memory::allocator::total_frame_count() as i64,
        6 => scheduler::core_active_ticks(arg1 as usize) as i64,
        7 => scheduler::core_total_ticks(arg1 as usize) as i64,
        8 => crate::smp::core::ready_count() as i64,
        // 24/25: the two facts the blocked-chain walk needs (`utilities/46_trace.md`). Both EXPOSE
        // state the kernel already keeps for correctness - 24 is the endpoint a task owns (already
        // read to compute its queue depth), 25 is the endpoint it is blocked-in-CALL awaiting, which
        // exists so a dead replier can wake it with `ReplyDead` (§8.6). Nothing new is recorded and
        // nothing is written; `trace` is a READER of the kernel, not a tracer in it.
        //
        // INTROSPECT-gated by falling outside the ungated list above, which is the right default:
        // both disclose another task's state.
        24 => scheduler::task_stat(arg1 as usize).endpoint as i64,
        25 => crate::ipc::routing::call_await_endpoint(arg1 as usize) as i64,
        // 26: fault diagnostics written WITHOUT the serial lock, and so possibly spliced into another
        // line (audits/kernel-audit.md Audit 10). A residual, not a defect count: when a fault
        // interrupts a `kprintln` on its own core the handler cannot wait for the lock it already
        // holds, and writing unlocked is the only way to report the fault at all. This says how often
        // that happened, so a corrupted line in the log is explainable rather than mysterious - the
        // whole reason the splice cost two wrong diagnoses before it was understood. 0 means every
        // diagnostic this boot was emitted cleanly.
        26 => crate::arch::imp::serial_unlocked_emit_count() as i64,
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
            // concurrent respawns - reading routing::get_generation after a kill
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
// Syscall: QueryCapRights (14) - read the rights bitfield of a cap slot.
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
// Syscall: RemoveCap (15) - remove a cap slot from the calling task's table.
// ---------------------------------------------------------------------------

/// arg0 = cap_slot.
///
/// Clears the cap at `slot`. Always returns 0; out-of-range slots are silently
/// ignored (idempotent - the slot is already empty).
fn handle_remove_cap(slot: u64) -> i64 {
    scheduler::current_task_remove_cap(slot as usize);
    0
}

// ---------------------------------------------------------------------------
// Syscall: TaskStat (16) - read task state for a given slot index.
// ---------------------------------------------------------------------------

/// arg0 = slot (u32), arg1 = buf_ptr (user VA), arg2 = buf_len (must be ≥ 80).
///
/// Requires the INTROSPECT capability (READ) - discloses any task's state (§3.1).
///
/// Buffer layout (80 bytes = STAT_SIZE - kept in sync with the writes below):
///   [0]       valid:         u8  (1 = live, 0 = dead/unused)
///   [1]       state:         u8  (0=Ready, 1=Running, 2=BlockedOnRecv, 3=BlockedOnSend, 4=Dead)
///   [2]       core:          u8
///   [3]       queue_depth:   u8
///   [4..8]    name_len:      u32 LE
///   [8..16]   mem_used:      u64 LE
///   [16..24]  mem_limit:     u64 LE
///   [24..56]  name:          [u8; 32] (truncated; zero-padded)
///   [56..64]  restart_count: u64 LE
///   [64..72]  run_ticks:     u64 LE
///   [72..80]  uptime_secs:   u64 LE
///
/// Returns 0 on success, -1 on invalid args.
fn handle_task_stat(slot: u64, buf_ptr: u64, buf_len: u64) -> i64 {
    const STAT_SIZE: usize = 80;
    // TaskStat discloses any task's full snapshot - requires INTROSPECT (READ)
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
    buf[3] = stat.queue_depth; // moved here (was buf[60]) to free 8 contiguous bytes for restart_count
    buf[4..8].copy_from_slice(&name_len.to_le_bytes());
    buf[8..16].copy_from_slice(&stat.mem_used.to_le_bytes());
    buf[16..24].copy_from_slice(&stat.mem_limit.to_le_bytes());
    buf[24..24 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    // restart_count is u64 (was a u32 endpoint generation at buf[56..60]); queue_depth moved to buf[3].
    buf[56..64].copy_from_slice(&stat.restart_count.to_le_bytes());
    buf[64..72].copy_from_slice(&stat.run_ticks.to_le_bytes());
    buf[72..80].copy_from_slice(&stat.uptime_secs.to_le_bytes());

    if write_user_bytes(buf_ptr, &buf) { 0 } else { -1 }
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleRead (17) - block until one byte is available on COM1 RX.
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
    crate::arch::imp::CONSOLE_READ_WAITER.store(my_slot as u32, Ordering::Release);

    loop {
        // Only consume a byte while we own (or share) the console foreground. While a foreground app
        // (e.g. `chaos`, syscall 40) owns it, stay blocked so ITS keystrokes (its `q`) reach it, not us.
        // This closes the race where the shell's loop gate passed is-foreground, then it blocked here
        // just as the app claimed. We are woken by the RX IRQ (a byte) OR by release/owner-death.
        if crate::arch::imp::console_foreground_allows(my_slot as u32) {
            // Drain the UART FIFO ourselves (a starved timer ISR under chaos max-carnage would otherwise
            // leave the serial byte stranded in the FIFO). See uart_rx_drain_now.
            crate::arch::imp::uart_rx_drain_now();
            if let Some(b) = crate::arch::imp::uart_rx_pop() {
                crate::arch::imp::CONSOLE_READ_WAITER.store(u32::MAX, Ordering::Release);
                return b as i64;
            }
        }

        // Block until the IRQ handler (a byte) or release/owner-death (foreground changed) wakes us.
        let err = scheduler::block_and_reschedule(TaskState::BlockedOnRecv);
        if err != 0 {
            crate::arch::imp::CONSOLE_READ_WAITER.store(u32::MAX, Ordering::Release);
            return err;
        }
        // Woken by uart_rx_irq_handler; loop to pop the byte.
    }
}

// ---------------------------------------------------------------------------
// Syscall: TryConsoleRead (24) - non-blocking console read.
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
    // Console foreground exclusivity: while another task owns the foreground (e.g. the `chaos`
    // service running its TUI), a poll from any OTHER task reads empty - so a resurrected shell
    // cannot swallow the foreground app's `q`. Unclaimed (the normal state) allows everyone, so
    // ordinary shell input is unchanged.
    if !crate::arch::imp::console_foreground_allows(scheduler::current_task_slot() as u32) {
        return NO_CONSOLE_BYTE;
    }
    // Drain the UART FIFO ourselves before popping: `chaos max-carnage` starves the timer-ISR poll
    // (the normal FIFO->ring drain), so without this the serial `q`-to-abort sits stranded in the FIFO
    // and the storm cannot be stopped. This makes the chaos runner's q-poll independent of the ISR.
    crate::arch::imp::uart_rx_drain_now();
    match crate::arch::imp::uart_rx_pop() {
        Some(b) => b as i64,
        None    => NO_CONSOLE_BYTE,
    }
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleForeground (40) - claim/release exclusive console input.
// ---------------------------------------------------------------------------

/// Claim (`op == 1`) or release (`op == 0`) exclusive console input for the calling
/// task. While claimed, only this task's console polls return bytes; every other task
/// reads empty (see `handle_try_console_read`). The reusable primitive behind the
/// `chaos` TUI owning the keyboard while it kills and resurrects the shell, and a
/// future foreground/TUI switcher. Gated by CONSOLE_READ (only a task that may consume
/// the keyboard may seize it exclusively). Returns 0 on success.
fn handle_console_foreground(cap_slot: u64, op: u64) -> i64 {
    use crate::capability::CONSOLE_READ_RESOURCE;

    let cap = match scheduler::current_task_lookup_cap(cap_slot as usize, Rights::READ) {
        Ok(c)  => c,
        Err(e) => return cap_err_to_i64(e),
    };
    if cap.resource_id != CONSOLE_READ_RESOURCE {
        return cap_err_to_i64(CapError::CapWrongScope);
    }
    if op == 0 {
        crate::arch::imp::release_console_foreground();
        // WAKE whoever is blocked reading the console, so the prompt comes back on its own.
        //
        // The shell checks the foreground at the TOP of its loop and then BLOCKS in `console_read`. If a
        // foreground app claimed the console while the shell was already blocked there, the shell never
        // reached that check, so it is not "muted" - it is asleep on the ring. Releasing the foreground
        // then changes a flag nobody is looking at, and the shell stays asleep until a key arrives.
        // That is why a finished `chaos max-carnage` needed an Enter press to get `gsh>` back: the
        // handover was correct and the sleeper was never told.
        //
        // A newline is the right wake: the blocked read returns it, the shell finishes an empty line and
        // draws a fresh prompt, and an empty command runs nothing. Exactly the remedy the USB hot-plug
        // path uses for the same reason - a working machine that presents as a dead one is what
        // invariant 12 is about, and it is measured in what the OPERATOR can see.
        crate::arch::imp::console_push_byte(b'\n');
    } else {
        crate::arch::imp::claim_console_foreground(scheduler::current_task_slot() as u32);
    }
    0
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleEcho (25) - enable/disable keystroke echo.
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
    crate::arch::imp::set_console_echo(on != 0);
    0
}

// ---------------------------------------------------------------------------
// Syscall: ConsoleBootComplete (26) - end boot-log mirroring + clear the screen.
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
    crate::arch::imp::console_boot_complete();
    0
}

// ---------------------------------------------------------------------------
// Syscall: SignalInputReady (27) - input driver reports setup complete.
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
    crate::arch::imp::set_input_ready();
    0
}

// ---------------------------------------------------------------------------
// Syscall: TaskCaps (28) - list the capabilities held by a task.
// ---------------------------------------------------------------------------

/// arg0 = slot, arg1 = buf_ptr (user VA), arg2 = buf_len (bytes).
///
/// Writes up to `buf_len / 16` entries describing the target task's held caps,
/// returns the count. Each 16-byte entry: [0..8] resource_id u64 LE, [8] rights
/// u8, [9..16] pad. Requires INTROSPECT (READ) - discloses a task's authority
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
// Syscall: ConsolePush (20) - inject a byte into the console input ring.
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
    crate::arch::imp::console_push_byte(byte as u8);
    0
}

// ---------------------------------------------------------------------------
// Syscall: Reboot (18) - hardware reset via keyboard controller CPU reset line.
// ---------------------------------------------------------------------------

/// No arguments. Does not return (on success).
///
/// A hardware reset is a denial-of-service, so it is gated by the `REBOOT` capability (§3.1) - held
/// only by the legitimate rebooters: the `shell` (its `reboot` command) and the USB drivers
/// `xhci`/`ehci` (the Ctrl+Alt+Del secure-attention reboot). Any other caller gets `CapNotHeld`,
/// closing the ambient-authority gap this syscall used to have. Validated by holdings (no arguments →
/// no slot to pass, same form as `kill`/8). Logs to serial before resetting so the operator sees
/// confirmation before the line goes silent.
/// FireIrq (51): inject a test interrupt on `irq`. Gated by FIRE_IRQ, held only by the control service.
///
/// Exists so the COM2 command interpreter can leave the kernel (C1-6): `KILL` and `RESTART` were always
/// expressible as SERVICE_CONTROL + SPAWN, but this one had no capability, so the module could not move.
/// Naming the authority is more honest than leaving it unnamed inside ring 0.
fn handle_fire_irq(irq: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::FIRE_IRQ_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    if irq > u8::MAX as u64 {
        return -1;
    }
    crate::arch::imp::interrupts::fire_test_irq(irq as u8);
    0
}

fn handle_reboot() -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::REBOOT_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    crate::kprintln!("reboot: hardware reset");
    crate::arch::imp::hardware_reset();
}


/// Largest ethernet frame the USB-net bridge moves (matches nic-driver's FRAME_MAX).
const NET_FRAME_MAX: usize = 1600;

/// NetFrameTx (42): transmit a raw ethernet frame via the in-kernel USB-net device. `arg0` = frame ptr,
// ---------------------------------------------------------------------------
// PORT I/O (step D2) - the mechanism a userspace hardware enumerator needs.
//
// The kernel knows how to PERFORM an authorised port operation. It does not know what the operation
// MEANS - that 0xCF8 selects a PCI configuration register, how to walk a bus, what a class code
// identifies, or how to read a BAR. That knowledge is hardware semantics and lives in the service
// (§26.10, docs/service-ownership.md D2). This is the whole of the kernel's involvement.
// ---------------------------------------------------------------------------

/// PciCfgRead (53): `arg0` = configuration selector, `arg1` = register offset. Gated by
/// `PCI_CFG_RESOURCE` + READ.
///
/// Returns the 32-bit value read (as a positive i64 - a full u32 always widens non-negative, so
/// 0xFFFFFFFF is data and not an error), `CapNotHeld` without the capability, or `-1` for an access
/// the arch will not admit.
///
/// A REFUSAL IS LOUD (invariant 12) but not fatal to the caller: an enumerator walking a bus is
/// told where its authority ends and stops there, rather than being handed a plausible zero it would
/// report as an empty machine.
fn handle_pci_cfg_read(sel: u64, offset: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::PCI_CFG_RESOURCE, Rights::READ) {
        crate::kprintln!("pci-cfg: read sel {:#010x} refused - caller does not hold PCI_CFG",
                         sel as u32);
        return CapError::CapNotHeld as i64;
    }
    // The access, its lock and its admissibility check live in `arch` beside the registers they
    // guard, so the check and the I/O cannot drift apart and this file needs no `unsafe`
    // (§18.5 - its floor may only shrink).
    match crate::arch::imp::pci_cfg_read32(sel as u32, offset as u16) {
        Some(v) => v as i64,
        None => {
            crate::kprintln!(
                "pci-cfg: read sel {:#010x} off {:#06x} refused - outside what this machine admits",
                sel as u32, offset as u16);
            -1
        }
    }
}

/// `arg1` = length. Gated by NET_DEVICE (validated by holdings - the args fill the ABI, no slot to pass).
/// Returns 0 on success, -1 on error. On non-ARM arches `net_frame_tx` is a stub returning false.
fn handle_net_frame_tx(ptr: u64, len: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::NET_DEVICE_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let len = len as usize;
    if len == 0 || len > NET_FRAME_MAX { return -1; }
    let frame = match read_user_bytes(ptr, len) { Some(b) => b, None => return -1 };
    if crate::arch::imp::net_frame_tx(frame) { 0 } else { -1 }
}

/// NetFrameRx (43): receive one raw ethernet frame into the user buffer. `arg0` = dst ptr, `arg1` = max
/// length. Gated by NET_DEVICE. Returns the frame length (0 if none is available), -1 on error.
fn handle_net_frame_rx(ptr: u64, max: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::NET_DEVICE_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let max = (max as usize).min(NET_FRAME_MAX);
    if max == 0 { return -1; }
    let mut buf = [0u8; NET_FRAME_MAX];
    // `net_frame_rx` returns bytes written into the `[..max]` slice; clamp defensively so a future buggy
    // arch impl returning n > max can never index-panic `buf[..n]` (kernel-audit Audit 6, INFO-1).
    let n = crate::arch::imp::net_frame_rx(&mut buf[..max]).min(max);
    if n == 0 { return 0; }
    if !write_user_bytes(ptr, &buf[..n]) { return -1; }
    n as i64
}

/// NetInfo (44): write `[mac(6), link(1)]` (7 bytes) of the USB-net device to `arg0`. Gated by NET_DEVICE.
/// Returns 1 if a net device is up, 0 if none, -1 on error.
fn handle_net_info(ptr: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::NET_DEVICE_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    match crate::arch::imp::net_info() {
        Some((mac, link)) => {
            let mut out = [0u8; 7];
            out[..6].copy_from_slice(&mac);
            out[6] = if link { 1 } else { 0 };
            if write_user_bytes(ptr, &out) { 1 } else { -1 }
        }
        None => 0,
    }
}

/// Gpio (45): drive a SoC GPIO pin. `op` = 0 input / 1 output / 2 set-high / 3 set-low / 4 read; `pin` =
/// 0..53. Gated by GPIO_DEVICE (validated by holdings - the args fill the ABI). Returns the level (0/1) for
/// a read, 0 on success, -1 on a bad pin / unsupported arch. On non-ARM `gpio_op` is an inert `-1` stub.
fn handle_gpio(op: u64, pin: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::GPIO_DEVICE_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    if op > 4 || pin > 53 { return -1; } // BCM2835 has 54 GPIO lines (0..53)
    crate::arch::imp::gpio_op(op as u32, pin as u32)
}

/// One block of the USB mass-storage device. The whole storage stack is 512-byte blocks, and the kernel
/// only claims a device whose sectors are that size (`dwc2::probe_mass_storage`), so this is fixed.
const USB_DISK_BLOCK: usize = 512;

/// UsbDiskInfo (46): capacity of the attached USB mass-storage device in 512-byte sectors, 0 if none.
/// Gated by USB_DISK (validated by holdings - no slot to pass). On non-ARM arches this is always 0.
fn handle_usb_disk_info() -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::USB_DISK_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    crate::arch::imp::usb_disk_sectors() as i64
}

/// A USB-disk syscall's "the device NAKed, re-ask" answer.
///
/// Deliberately OUTSIDE the capability-error range (-2..-7, `cap_err_to_i64`). BUSY was first given
/// `-2`, which is `CapNotHeld` - so a task calling these syscalls WITHOUT the `USB_DISK` capability got
/// the same answer as one whose device was merely occupied. `block-driver` believes the second reading
/// and re-asks 6000 times before reporting "the device stayed busy, it did not fail", which is a false
/// diagnosis of an authority failure, and `fs` then degrades storage on the strength of it (Invariant
/// 12: a failure must name its own cause). Reachable whenever a respawned driver comes back without its
/// cap wired - exactly the case a kill-storm produces.
const USB_DISK_BUSY: i64 = -20;

/// No device is attached at all - as opposed to one that is present and asking us to wait.
///
/// These are opposite instructions to the caller and were being answered with the same word. BUSY means
/// "come back, I am working"; ABSENT means "there is nothing here, and re-asking cannot change that -
/// only a hot-plug can". Conflated, `block-driver` did the right thing with the wrong fact: it waited out
/// its full 6000-attempt, ~30-second budget against a socket the operator had emptied, for every block of
/// every request, and then reported "the device stayed busy, it did not fail" - a statement that was not
/// true of a stick sitting on the desk.
///
/// The conflation was structural, not a missing branch. `usb_disk_busy()` reads `LAST_FAIL`, which
/// records the last TRANSFER's outcome - and a refusal short-circuits before any transfer, so it left
/// whatever the previous one wrote. A stick pulled mid-command leaves a NAK there, so "absent" inherited
/// "busy" from the transfer that was in flight when it was pulled. The state was stale rather than wrong,
/// which is why it read as plausible (§26.4 - a derived value must reduce to a current truth).
const USB_DISK_ABSENT: i64 = -21;

/// UsbDiskRead (47): read the 512-byte block at `arg0` (LBA) into the user buffer at `arg1`.
/// Gated by USB_DISK. Returns 0 on success, `USB_DISK_BUSY` if the device NAKed (re-ask), -1 on a real
/// failure (no device, LBA past the end, I/O error).
fn handle_usb_disk_read(lba: u64, ptr: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::USB_DISK_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let mut buf = [0u8; USB_DISK_BLOCK];
    // -2 = BUSY (the device NAKed; nothing is wrong, re-ask). Distinct from -1 = failed, because the
    // two need opposite responses and collapsing them is what turned a busy stick into a "broken" one.
    if !crate::arch::imp::usb_disk_read(lba, &mut buf) {
        // ABSENT first: it is the stronger fact. `usb_disk_busy` reads the last TRANSFER's outcome, which
        // a refusal never updates, so asking it about a device that is not there answers from stale state.
        if crate::arch::imp::usb_disk_absent() { return USB_DISK_ABSENT; }
        return if crate::arch::imp::usb_disk_busy() { USB_DISK_BUSY } else { -1 };
    }
    if !write_user_bytes(ptr, &buf) { return -1; }
    0
}

/// UsbDiskWrite (48): write the 512-byte block at the user buffer `arg1` to LBA `arg0`.
/// Gated by USB_DISK. Returns 0 on success, `USB_DISK_BUSY` if the device NAKed, -1 on a real failure.
fn handle_usb_disk_write(lba: u64, ptr: u64) -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::USB_DISK_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    let src = match read_user_bytes(ptr, USB_DISK_BLOCK) { Some(b) => b, None => return -1 };
    if crate::arch::imp::usb_disk_write(lba, src) { 0 }
    else if crate::arch::imp::usb_disk_absent() { USB_DISK_ABSENT }  // nothing there; re-asking cannot help
    else if crate::arch::imp::usb_disk_busy() { USB_DISK_BUSY }   // re-ask, do not treat as a failure
    else { -1 }
}

/// UsbDiskFlush (49): flush the device's write cache to the medium (SCSI SYNCHRONIZE CACHE).
/// Gated by the same USB_DISK WRITE right as a write - making data durable is part of writing it,
/// and a caller that cannot write has nothing to flush. Returns 0 on success, -1 on failure; a
/// failure is reported, never swallowed, because the caller is about to rely on durability (§26.7).
fn handle_usb_disk_flush() -> i64 {
    if !scheduler::current_task_holds_resource(crate::capability::USB_DISK_RESOURCE, Rights::WRITE) {
        return cap_err_to_i64(CapError::CapNotHeld);
    }
    if crate::arch::imp::usb_disk_flush() { 0 } else { -1 }
}

fn ipc_err_to_i64(e: IpcError) -> i64 {
    match e {
        IpcError::EndpointDead    => -7,
        IpcError::QueueFull       => -8,
        IpcError::QueueEmpty      => -9,
        IpcError::MessageTooLarge => -10,
        IpcError::ReplyDead       => -12,
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
