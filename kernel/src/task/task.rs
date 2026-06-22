// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Task structure - §9, §14.1.
//!
//! A task is the kernel's unit of execution. It has:
//!   - Its own virtual address space (page table root).
//!   - A capability table populated from its service contract at spawn.
//!   - A saved context for context switching.
//!   - A fixed core assignment (never migrates - §9.1).

use crate::arch::x86_64::context_switch::TaskContext;
use crate::arch::x86_64::page_tables::PageTable;
use crate::capability::table::CapTable;
use crate::memory::ownership::TaskMemoryOwner;
use crate::task::state::TaskState;

/// Kernel-assigned unique task identifier.
/// Stable for the lifetime of one task instance; not reused within a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub u64);

pub struct Task {
    pub id: TaskId,
    /// Human-readable service name (from the contract `name` field).
    pub name: &'static str,
    /// Core this task is pinned to. Immutable after spawn.
    pub core_id: u32,
    pub state: TaskState,
    pub context: TaskContext,
    pub page_table: PageTable,
    pub caps: CapTable,
    pub memory: TaskMemoryOwner,
}
