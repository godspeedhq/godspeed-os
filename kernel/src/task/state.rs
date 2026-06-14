// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Task lifecycle states — §14.

/// All states a task can be in on its owning core's run queue.
///
/// `#[repr(u8)]` so the discriminant can be stored in `AtomicU8` and
/// round-tripped through `From<u8>` for the CAS in `block_and_reschedule`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// On the run queue; will receive CPU time at next scheduling point.
    Ready         = 0,
    /// Currently executing on the core.
    Running       = 1,
    /// Blocked on `recv` waiting for a message.
    BlockedOnRecv = 2,
    /// Blocked on `send` waiting for queue space.
    BlockedOnSend = 3,
    /// Terminated; memory not yet reclaimed.
    Dead          = 4,
}

impl From<u8> for TaskState {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Ready,
            1 => Self::Running,
            2 => Self::BlockedOnRecv,
            3 => Self::BlockedOnSend,
            _ => Self::Dead,
        }
    }
}
