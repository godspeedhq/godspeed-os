//! IPC subsystem — §8.
//!
//! Exposes `send`, `recv`, and `try_send` to the syscall dispatcher.
//! Internally owns the routing table and per-endpoint queues.

pub mod endpoint;
pub mod message;
pub mod queue;
pub mod routing;

pub use endpoint::{Endpoint, EndpointId};
pub use message::{IpcError, Message};

pub fn init() {
    routing::init();
    crate::kprintln!("ipc: routing table ready");
}
