//! Task management — §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

/// Spawn the `init` service on Core 0. Called once by `kernel_main` (§11.1).
pub fn spawn_init() {
    todo!("load init binary, mint its capabilities from the init contract, add to Core 0 run queue")
}

/// Kill the currently-running task (called from page-fault handler — §10.3).
pub fn kill_current() {
    todo!("mark current task dead, notify supervisor, reclaim memory, run scheduler")
}
