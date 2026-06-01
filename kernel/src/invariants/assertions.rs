//! Runtime enforcement of constitutional invariants — §3, §22.
//!
//! These assertions are the executable form of the constitution. If any one
//! fires in a build, the system is no longer the system the spec describes.
//! They run in both debug and release builds; they are not behind cfg(debug).

/// Assert that a syscall's cap slot is valid before any privileged action.
/// Panic if not — this is invariant §3.1 (no ambient authority).
#[inline(always)]
pub fn assert_cap_validated(result: &Result<(), crate::capability::cap::CapError>) {
    if let Err(e) = result {
        panic!("invariant violation: syscall executed without valid capability: {:?}", e);
    }
}

/// Assert that a service's core assignment does not change mid-execution.
/// Invariant §3.11 (identity is stable; location is not — but location
/// must be stable *within* a single execution lifetime).
#[inline(always)]
pub fn assert_no_mid_execution_migration(original_core: u32, current_core: u32) {
    assert_eq!(
        original_core, current_core,
        "invariant violation: task migrated between cores during execution"
    );
}

/// Assert the kernel's TCB services are still alive. Called at key checkpoints.
/// Invariant §6.2.
///
/// Checks that each TCB service's endpoint is still registered in the IPC name
/// registry and alive in the routing table. Death of any TCB service requires
/// an immediate system reboot — §6.2.
pub fn assert_tcb_alive() {
    const TCB: &[&str] = &["init", "supervisor", "registry"];
    const DEAD: u8 = crate::task::state::TaskState::Dead as u8;
    // §6.2 governs the *death of a service that exists*, not the *omission* of
    // one: identity-test manifests are minimal and spawn only the subset a given
    // test needs (e.g. cross-core tests run without `registry`). So a TCB name
    // that is simply absent from this configuration is skipped — only a service
    // that exists and is Dead (or whose endpoint was killed) is a violation.
    //
    // This absence-tolerance is NOT fail-open: the only syscall that can kill a
    // service (`handle_kill`) rejects TCB targets before any kill happens, so a
    // TCB name absent here can only mean "never spawned in this configuration",
    // never "killed-and-reclaimed". A TCB service that dies by fault (kill_current)
    // is caught while still present-and-Dead by the `state == DEAD` check above.
    'next: for &name in TCB {
        for slot in 0..crate::task::scheduler::MAX_TASKS {
            let stat = crate::task::scheduler::task_stat(slot);
            if stat.valid && stat.name == name {
                // Found the task. Liveness by task state works uniformly, including
                // for `init`, which persists (`loop { yield }`) but registers no
                // named IPC endpoint and so cannot be checked by endpoint.
                if stat.state == DEAD {
                    panic!("invariant violation: TCB service '{}' is Dead (§6.2)", name);
                }
                // A TCB service that also exposes a named endpoint must have it
                // alive — a live task with a killed endpoint is also a §6.2 break.
                if let Some(ep_id) = crate::ipc::names::lookup(name) {
                    if !crate::ipc::routing::is_endpoint_alive(ep_id) {
                        panic!("invariant violation: TCB service '{}' endpoint is dead (§6.2)", name);
                    }
                }
                continue 'next;
            }
        }
        // `name` not present in this configuration — not a §6.2 violation.
    }
}

/// Assert the capability table is consistent: no cap carries a generation that
/// exceeds its resource's current generation in the global table. Such a cap
/// would be from the future — impossible under correct minting. Invariant §7.8.
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
