//! Per-task `hw_pio` grant: the I/O-port ranges a driver task may touch via the
//! `PortRead`/`PortWrite` syscalls (§12, docs/persistence.md §5).
//!
//! The grant IS the capability for port I/O — there is no cap slot to pass, so
//! it is validated by range-check on every access (§3.1, like `Kill` /
//! `InspectKernel` validate by holdings). Empty for every non-driver task, so a
//! stray port access is denied by default (no ambient authority).
//!
//! This lives in the `capability/` layer — a permitted `unsafe` layer (§18.1) —
//! so the per-task static-mut store does not grow the grandfathered `unsafe`
//! floor in `task/` (§18.5).

use crate::task::scheduler::MAX_TASKS;

/// (base, len) port ranges granted to each task slot. Set at spawn from the
/// task's `ServiceConfig` grant; cleared when a slot is reused.
static mut TASK_PIO_RANGES: [&'static [(u16, u16)]; MAX_TASKS] = [&[]; MAX_TASKS];

/// Record the port ranges granted to `slot` (spawn time, single writer).
pub fn set(slot: usize, ranges: &'static [(u16, u16)]) {
    if slot < MAX_TASKS {
        // SAFETY: called from spawn for this reserved slot before the task runs;
        // there is no concurrent access to this slot's entry. The slice is
        // `&'static` (a kernel `service_config` constant).
        unsafe { TASK_PIO_RANGES[slot] = ranges; }
    }
}

/// Clear `slot`'s grant so a reused slot never inherits a dead driver's ports.
pub fn clear(slot: usize) {
    if slot < MAX_TASKS {
        // SAFETY: called under the task-slot lock in `reserve_task_slot`; single
        // writer, no task is running in this slot yet.
        unsafe { TASK_PIO_RANGES[slot] = &[]; }
    }
}

/// True iff the task in `slot` holds an `hw_pio` grant covering `port`.
pub fn allowed(slot: usize, port: u16) -> bool {
    if slot >= MAX_TASKS {
        return false;
    }
    // SAFETY: read of a `&'static` slice reference for this slot; the caller
    // passes the currently-running task's slot (IF=0 syscall context), and the
    // entry is only written at spawn/clear for an unoccupied slot.
    let ranges = unsafe { TASK_PIO_RANGES[slot] };
    ranges
        .iter()
        .any(|&(base, len)| port >= base && port < base.saturating_add(len))
}
