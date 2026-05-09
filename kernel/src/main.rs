#![no_std]
#![no_main]

mod arch;
mod capability;
mod interrupt;
mod invariants;
mod ipc;
mod log;
mod memory;
mod smp;
mod syscall;
mod task;

use core::panic::PanicInfo;

// ---------------------------------------------------------------------------
// Demo task stacks — Milestone 3.
// 16-byte aligned wrappers so the initial RSP is properly aligned.
// Replaced by frame-allocator stacks in Milestone 7.
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
struct KernelStack([u8; 16 * 1024]);

static mut STACK_A: KernelStack = KernelStack([0; 16 * 1024]);
static mut STACK_B: KernelStack = KernelStack([0; 16 * 1024]);

/// Demo task A: counts iterations and prints every ~5 M loops.
unsafe extern "C" fn task_a() -> ! {
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        if n % 5_000_000 == 0 {
            kprintln!("task-a: {}", n / 5_000_000);
        }
    }
}

/// Demo task B: counts iterations and prints every ~5 M loops.
unsafe extern "C" fn task_b() -> ! {
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        if n % 5_000_000 == 0 {
            kprintln!("task-b: {}", n / 5_000_000);
        }
    }
}

// ---------------------------------------------------------------------------
// Capability enforcement demo — Milestone 4.
// ---------------------------------------------------------------------------

/// Demonstrate and validate the capability enforcement machinery (§22 Test 2).
///
/// Runs synchronously before the scheduler starts so the assertions fire
/// loudly and early if the invariants are broken.
fn test_cap_enforcement() {
    use capability::{
        CapError, Rights, LOG_WRITE_RESOURCE,
        mint_cap, mark_dead_resource, revoke_resource, register_resource, ResourceId,
    };
    use capability::table::CapTable;

    kprintln!("cap-test: starting capability enforcement tests");

    // --- Test 2A: task with cap can validate it ---------------------
    let mut tbl = CapTable::empty();
    let cap = mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE);
    let slot = tbl.insert(cap).expect("insert");

    match tbl.get(slot, Rights::WRITE) {
        Ok(c) => {
            assert_eq!(c.resource_id, LOG_WRITE_RESOURCE);
            kprintln!("cap-test: 2A pass — held cap validates OK");
        }
        Err(e) => panic!("cap-test: 2A FAIL — expected Ok, got {:?}", e),
    }

    // --- Test 2B: task without cap returns CapNotHeld ---------------
    let empty = CapTable::empty();
    match empty.get(0, Rights::WRITE) {
        Err(CapError::CapNotHeld) =>
            kprintln!("cap-test: 2B pass — no cap returns CapNotHeld"),
        other =>
            panic!("cap-test: 2B FAIL — expected CapNotHeld, got {:?}", other),
    }

    // --- Test 2C: insufficient rights returns CapInsufficientRights -
    let read_only_cap = mint_cap(LOG_WRITE_RESOURCE, Rights::READ);
    let mut tbl2 = CapTable::empty();
    let slot2 = tbl2.insert(read_only_cap).expect("insert");
    match tbl2.get(slot2, Rights::WRITE) {
        Err(CapError::CapInsufficientRights) =>
            kprintln!("cap-test: 2C pass — wrong right returns CapInsufficientRights"),
        other =>
            panic!("cap-test: 2C FAIL — expected CapInsufficientRights, got {:?}", other),
    }

    // --- Test: generation bump → CapRevoked -------------------------
    // Use a fresh resource so we don't disturb LOG_WRITE_RESOURCE (gen 0 forever).
    let tmp_res = ResourceId(0xDEAD);
    register_resource(tmp_res);
    let tmp_cap = mint_cap(tmp_res, Rights::WRITE);
    let mut tbl3 = CapTable::empty();
    let slot3 = tbl3.insert(tmp_cap).expect("insert");

    // Validate OK before bump.
    assert!(tbl3.get(slot3, Rights::WRITE).is_ok(), "pre-bump should succeed");

    // Explicit revocation → CapRevoked.
    revoke_resource(tmp_res);
    match tbl3.get(slot3, Rights::WRITE) {
        Err(CapError::CapRevoked) =>
            kprintln!("cap-test: revoke pass — stale cap returns CapRevoked"),
        other =>
            panic!("cap-test: revoke FAIL — expected CapRevoked, got {:?}", other),
    }

    // --- Test: endpoint death → EndpointDead ------------------------
    let dead_res = ResourceId(0xDEAF);
    register_resource(dead_res);
    let dead_cap = mint_cap(dead_res, Rights::SEND);
    let mut tbl4 = CapTable::empty();
    let slot4 = tbl4.insert(dead_cap).expect("insert");

    mark_dead_resource(dead_res);
    match tbl4.get(slot4, Rights::SEND) {
        Err(CapError::EndpointDead) =>
            kprintln!("cap-test: endpoint-dead pass — dead endpoint returns EndpointDead"),
        other =>
            panic!("cap-test: endpoint-dead FAIL — expected EndpointDead, got {:?}", other),
    }

    // --- Test: GRANT transfer moves cap exactly once ----------------
    let grant_res = ResourceId(0xABCD);
    register_resource(grant_res);
    let grantable = mint_cap(grant_res, Rights::READ | Rights::GRANT);
    let mut sender = CapTable::empty();
    let s_slot = sender.insert(grantable).expect("insert");

    // Remove from sender (simulating the GRANT transfer).
    let transferred = sender.remove(s_slot).expect("remove");
    assert_eq!(sender.get(s_slot, Rights::READ), Err(CapError::CapNotHeld),
               "cap must be gone from sender after transfer");

    let mut receiver = CapTable::empty();
    let r_slot = receiver.insert(transferred).expect("insert into receiver");
    assert!(receiver.get(r_slot, Rights::READ).is_ok(),
            "cap must be valid in receiver after transfer");
    kprintln!("cap-test: grant pass — cap moved exactly once, sender empty");

    kprintln!("cap-test: all tests passed");
}

// ---------------------------------------------------------------------------
// Kernel entry point.
// ---------------------------------------------------------------------------

/// Called by the bootloader on the BSP only.
#[no_mangle]
pub extern "C" fn kernel_main(boot_info_ptr: *const arch::x86_64::BootInfo) -> ! {
    // SAFETY: bootloader guarantees boot_info_ptr is valid and aligned.
    let boot_info = unsafe { &*boot_info_ptr };

    // SAFETY: debug probe — port 0xe9 is QEMU debug console.
    unsafe { core::arch::asm!("out 0xe9, al", in("al") b'M', options(nostack, nomem)) };

    arch::x86_64::init(boot_info);

    unsafe { core::arch::asm!("out 0xe9, al", in("al") b'G', options(nostack, nomem)) };

    memory::init(boot_info);

    // Program the APIC timer now that HHDM offset is available (§9.1).
    arch::x86_64::init_timer();

    capability::init();
    ipc::init();
    smp::init(boot_info);

    kprintln!("kernel: all cores ready");

    // Run the synchronous capability enforcement test before scheduling starts.
    test_cap_enforcement();

    // Enqueue two demo tasks that prove round-robin preemption works (§22 Test 8).
    let cr3: u64;
    // SAFETY: reading CR3 is always valid in ring 0.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem)) };

    let ctx_a = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_a,
            STACK_A.0.as_mut_ptr().add(STACK_A.0.len()),
            cr3,
        )
    };
    let ctx_b = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_b,
            STACK_B.0.as_mut_ptr().add(STACK_B.0.len()),
            cr3,
        )
    };

    // task-a gets a log_write cap; task-b gets an empty table.
    // This directly encodes §22 Test 2 (cap enforcement) at the task level.
    let mut caps_a = capability::CapTable::empty();
    let log_cap = capability::mint_cap(capability::LOG_WRITE_RESOURCE, capability::Rights::WRITE);
    caps_a.insert(log_cap).expect("enqueue caps_a");

    task::scheduler::enqueue("task-a", ctx_a, caps_a);
    task::scheduler::enqueue("task-b", ctx_b, capability::CapTable::empty());

    kprintln!("scheduler: task-a and task-b enqueued");

    // BSP enters the scheduler; never returns.
    task::scheduler::run()
}

/// AP entry point — called by each secondary core after long-mode setup.
#[no_mangle]
pub extern "C" fn ap_main(core_id: u32) -> ! {
    arch::x86_64::ap_init(core_id);
    smp::core::mark_ready(core_id);
    task::scheduler::run()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("KERNEL PANIC: {}", info);
    arch::x86_64::halt_all_cores();
}
