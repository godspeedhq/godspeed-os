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
use crate::smp::SpinLock;

/// Endpoint IDs below 100 are reserved for kernel tests.
static NEXT_ENDPOINT_ID: AtomicU64 = AtomicU64::new(100);

/// Free list of reclaimed endpoint IDs, available for reuse (§14.2). Without reclamation the ID
/// counter only ever climbs, so sustained restart churn — a `chaos max-carnage` of a million rounds
/// does ~4000 respawns — marches it into the delegated/file-cap band and panics. Reuse keeps the live
/// ID range bounded by the concurrent-endpoint count (≤ `MAX_ENDPOINTS`), so the counter no longer
/// grows without bound. Sized just above `MAX_ENDPOINTS` (96): the free set can never exceed the
/// number of endpoints that were ever simultaneously live, so it never overflows in practice; if it
/// somehow did, the surplus ID is simply dropped (it falls back to the monotonic counter — bounded,
/// loud, never silent corruption).
const FREE_ID_CAP: usize = 128;
struct EndpointIdFreeList {
    ids: [u64; FREE_ID_CAP],
    len: usize,
}
static FREE_IDS: SpinLock<EndpointIdFreeList> =
    SpinLock::new(EndpointIdFreeList { ids: [0; FREE_ID_CAP], len: 0 });

/// Allocate an endpoint ID, **reusing a reclaimed one first** so the ID space stays bounded under
/// restart churn (§14.2). Reuse is safe by the generation discipline: a reused ID's new endpoint is
/// seeded at a generation strictly above the previous holder's (`task::spawn`, via the resource
/// table's persistent per-ID generation), so a stale cap to the old endpoint can never match the new
/// one (§7.5 — closes the ABA hole). Only when the free list is empty does the monotonic counter
/// advance; the delegated-band guard remains the loud backstop (invariant 12).
pub fn alloc_endpoint_id() -> EndpointId {
    {
        let mut fl = FREE_IDS.lock();
        let n = fl.len;
        if n > 0 {
            let id = fl.ids[n - 1];
            fl.len = n - 1;
            return EndpointId(id);
        }
    }
    let id = NEXT_ENDPOINT_ID.fetch_add(1, Ordering::Relaxed);
    if id >= crate::capability::delegated::DELEGATED_BASE {
        panic!(
            "endpoint id space exhausted (reached the delegated/file-cap band at {})",
            crate::capability::delegated::DELEGATED_BASE
        );
    }
    EndpointId(id)
}

/// Reclaim a dead endpoint's ID for reuse — called from the task-kill path once the endpoint is
/// destroyed (its routing entry Dead and its resource generation bumped). The next `alloc_endpoint_id`
/// hands this ID back out; its new endpoint's generation is seeded strictly above this dead one's, so
/// reuse is invisible to clients (stale caps still fail). Reserved IDs (< 100) and anything in the
/// delegated band are never reclaimed here.
pub fn free_endpoint_id(id: EndpointId) {
    if id.0 < 100 || id.0 >= crate::capability::delegated::DELEGATED_BASE {
        return;
    }
    let mut fl = FREE_IDS.lock();
    let n = fl.len;
    if n < FREE_ID_CAP {
        fl.ids[n] = id.0;
        fl.len = n + 1;
    }
    // Free list full (never in practice — see FREE_ID_CAP): drop the ID; the counter covers it.
}

pub fn init() {
    routing::init();
    crate::kprintln!("ipc: routing table ready");
}
