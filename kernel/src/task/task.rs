// SPDX-License-Identifier: GPL-2.0-only
//! Task structure - §9, §14.1.
//!
//! A task is the kernel's unit of execution. It has:
//!   - Its own virtual address space (page table root).
//!   - A capability table populated from its spawn request at spawn (§13.6).
//!   - A saved context for context switching.
//!   - A fixed core assignment (never migrates - §9.1).
//!
//! The `Task` struct below is NOT CONSTRUCTED anywhere: the live per-task state is the scheduler's
//! slot-indexed `TASK_*` arrays (`scheduler.rs`). Only `TaskId` is used (by `ipc::endpoint`).

use crate::arch::imp::context_switch::TaskContext;
use crate::arch::imp::page_tables::PageTable;
use crate::capability::table::CapTable;
use crate::memory::ownership::TaskMemoryOwner;
use crate::task::state::TaskState;

/// Kernel-assigned unique task identifier.
/// Stable for the lifetime of one task instance; not reused within a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

pub struct Task {
    pub id: TaskId,
    /// Human-readable service name (from the spawn request).
    pub name: &'static str,
    /// Core this task is pinned to. Immutable after spawn.
    pub core_id: u32,
    pub state: TaskState,
    pub context: TaskContext,
    pub page_table: PageTable,
    pub caps: CapTable,
    pub memory: TaskMemoryOwner,
}
