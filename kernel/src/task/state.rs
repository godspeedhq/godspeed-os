//! Task lifecycle states — §14.

/// All states a task can be in on its owning core's run queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// On the run queue; will receive CPU time at next scheduling point.
    Ready,
    /// Currently executing on the core.
    Running,
    /// Blocked on `recv` waiting for a message.
    BlockedOnRecv,
    /// Blocked on `send` waiting for queue space.
    BlockedOnSend,
    /// Terminated; memory not yet reclaimed.
    Dead,
}
