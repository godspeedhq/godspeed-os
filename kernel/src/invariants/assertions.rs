// SPDX-License-Identifier: GPL-2.0-only
//! Runtime enforcement of constitutional invariants - §3, §22.
//!
//! These assertions are the executable form of the constitution. If any one
//! fires in a build, the system is no longer the system the spec describes.
//! They run in both debug and release builds; they are not behind cfg(debug).

// (A cap-validated assertion sat here. Every call passed it a literal `Ok(())` after the real
// check had already returned, so it could not fire - a check that cannot fail is an instrument that
// reports a pass while measuring nothing. Deleted 2026-10-10 with its nine call sites, `backlog/fixed/80`
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
// `backlog/fixed/80` K20. If a component ever becomes unkillable again, its check is written then.)

/// Assert the capability table is consistent: no cap carries a generation that
/// exceeds its resource's current generation in the global table. Such a cap
/// would be from the future - impossible under correct minting. Invariant §7.8.
///
/// Stale caps (generation < current) are expected after endpoint death and are
/// not flagged here; they fail with `EndpointDead` / `CapRevoked` on next use.
///
/// **Only the CALLER's own table can panic.** This walked every task's table and panicked on what it
/// found, but every other table is read with no lock while its owner inserts and removes caps on its
/// own core, so a torn read could look like a cap from the future and take the kernel down from a
/// userspace `kill` - a Rust data race deciding a panic (backlog/90 D24). The caller's table is changed
/// only by the caller's own syscalls, and the caller is inside this one, so that reading is sound and a
/// violation there still halts. Everyone else's is a snapshot: a violation is SAID, the first and every
/// 64th, naming the slot, and the machine stays up. A real violation does not go away when re-read, so
/// it keeps being said.
pub fn assert_cap_table_consistent() {
    use crate::task::scheduler;
    let me = scheduler::current_task_slot();
    for slot in 0..scheduler::MAX_TASKS {
        scheduler::for_each_cap_of(slot, |cap| {
            if let Some(current_gen) = crate::capability::get_resource_generation(cap.resource_id) {
                if cap.generation.0 > current_gen.0 {
                    if slot == me {
                        panic!(
                            "invariant violation: cap for {:?} carries generation {} but resource is at generation {} (§7.8)",
                            cap.resource_id, cap.generation.0, current_gen.0,
                        );
                    }
                    static SEEN: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
                    let n = SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    if n == 1 || n % 64 == 0 {
                        crate::kprintln!(
                            "invariant: slot {}'s cap for {:?} reads generation {} with the resource at {} (§7.8) - a cap from the future, or a torn read of a table changing on another core; not a halt, since that read is unsynchronised ({} seen)",
                            slot, cap.resource_id, cap.generation.0, current_gen.0, n);
                    }
                }
            }
        });
    }
}
