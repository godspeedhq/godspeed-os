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
///
/// Endpoint ids must stay below the delegated-resource band (§7.10, P2): the global
/// resource table direct-indexes `[0, 8192)` and the file-cap band sits at
/// `[DELEGATED_BASE, …)`. Endpoint ids climb from 100 and never approach `DELEGATED_BASE`
/// in practice (the restart-storm tests do ≤200 spawns), so this guard is a correctness
/// backstop: a collision is a loud panic, never silent cap-table corruption (invariant 12).
pub fn alloc_endpoint_id() -> EndpointId {
    let id = NEXT_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed);
    if id >= crate::capability::delegated::DELEGATED_BASE {
        panic!(
            "endpoint id space exhausted (reached the delegated/file-cap band at {})",
            crate::capability::delegated::DELEGATED_BASE
        );
    }
    EndpointId(id)
}

pub fn init() {
    routing::init();
    crate::kprintln!("ipc: routing table ready");
}
