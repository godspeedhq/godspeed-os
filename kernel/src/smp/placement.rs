//! Static service placement — §9.2.
//!
//! Placement is decided once at spawn time and never changes during execution.
//! On restart the decision is re-evaluated from scratch; the previous core is
//! not remembered (§9.2, §14.2).

use crate::smp::core;

/// Error returned when a contracted core is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementInvalid {
    pub requested_core: u32,
}

/// Resolve the target core for a new service instance.
///
/// - If `contract_core` is `Some(n)`: requires core `n`; returns
///   `Err(PlacementInvalid)` if that core is not ready.
/// - If `contract_core` is `None`: round-robin across ready cores.
pub fn resolve(contract_core: Option<u32>) -> Result<u32, PlacementInvalid> {
    match contract_core {
        Some(n) => {
            if core::is_ready(n) {
                Ok(n)
            } else {
                Err(PlacementInvalid { requested_core: n })
            }
        }
        None => Ok(round_robin_next()),
    }
}

// SAFETY: written only from single-threaded spawn path; real impl uses atomics.
static mut RR_COUNTER: u32 = 0;

fn round_robin_next() -> u32 {
    let total = core::ready_count();
    // SAFETY: single-threaded spawn path in v1 stub.
    unsafe {
        let n = RR_COUNTER;
        RR_COUNTER = RR_COUNTER.wrapping_add(1);
        n % total.max(1)
    }
}
