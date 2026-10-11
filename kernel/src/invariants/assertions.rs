// SPDX-License-Identifier: GPL-2.0-only
//! Runtime enforcement of constitutional invariants - §3, §22.
//!
//! These assertions are the executable form of the constitution. If any one
//! fires in a build, the system is no longer the system the spec describes.
//! They run in both debug and release builds; they are not behind cfg(debug).

// (A cap-validated assertion sat here. Every call passed it a literal `Ok(())` after the real
// check had already returned, so it could not fire - a check that cannot fail is an instrument that
// reports a pass while measuring nothing. Deleted 2026-10-10 with its nine call sites, `backlog/80`
// K20; the validation it marked is `current_task_lookup_cap` / `current_task_holds_resource` at each
// handler's head.)

/// Assert that a service's core assignment does not change mid-execution.
/// Invariant §3.11 (identity is stable; location is not - but location
/// must be stable *within* a single execution lifetime).
#[inline(always)]
pub fn assert_no_mid_execution_migration(original_core: u32, current_core: u32) {
    assert_eq!(
        original_core, current_core,
        "invariant violation: task migrated between cores during execution"
    );
}

// (A TCB-alive assertion sat here, over an empty TCB set: since Path C / Phase 6 the only thing that
// cannot die is the kernel, which is not a task, so it checked nothing. Deleted 2026-10-10,
// `backlog/80` K20. If a component ever becomes unkillable again, its check is written then.)

/// Assert the capability table is consistent: no cap carries a generation that
/// exceeds its resource's current generation in the global table. Such a cap
/// would be from the future - impossible under correct minting. Invariant §7.8.
///
/// Stale caps (generation < current) are expected after endpoint death and are
/// not flagged here; they fail with `EndpointDead` / `CapRevoked` on next use.
pub fn assert_cap_table_consistent() {
    crate::task::scheduler::for_each_active_cap(|cap| {
        if let Some(current_gen) = crate::capability::get_resource_generation(cap.resource_id) {
            if cap.generation.0 > current_gen.0 {
                panic!(
                    "invariant violation: cap for {:?} carries generation {} \
                     but resource is at generation {} (§7.8)",
                    cap.resource_id, cap.generation.0, current_gen.0,
                );
            }
        }
    });
}
