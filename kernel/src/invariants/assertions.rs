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
pub fn assert_tcb_alive() {
    todo!("check init, supervisor, registry liveness; panic if any are dead")
}

/// Assert the capability table is consistent: no two tasks hold the same
/// writable cap to the same resource with different generations.
/// Invariant §7.8.
pub fn assert_cap_table_consistent() {
    todo!("walk all per-task cap tables; verify generation agreement with global table")
}
