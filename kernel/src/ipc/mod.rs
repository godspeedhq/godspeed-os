// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! IPC subsystem — §8.
//!
//! Exposes `send`, `recv`, and `try_send` to the syscall dispatcher.
//! Internally owns the routing table, per-endpoint queues, and name registry.

pub mod endpoint;
pub mod message;
pub mod names;
pub mod queue;
pub mod routing;

pub use endpoint::{Endpoint, EndpointId};
pub use message::{IpcError, Message};

use core::sync::atomic::{AtomicU64, Ordering};

/// Endpoint IDs below 100 are reserved for kernel tests.
static NEXT_ENDPOINT_ID: AtomicU64 = AtomicU64::new(100);

/// Allocate a fresh unique endpoint ID.
pub fn alloc_endpoint_id() -> EndpointId {
    EndpointId(NEXT_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed))
}

pub fn init() {
    routing::init();
    crate::kprintln!("ipc: routing table ready");
}
