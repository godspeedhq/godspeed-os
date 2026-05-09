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
// Kernel-task stacks — 16-byte aligned; replaced by frame-allocator stacks
// in Milestone 7.
// ---------------------------------------------------------------------------

// 64 KiB per task: Message is 4208 bytes; a single recv loop iteration puts
// ~5 KiB on the stack (Message copy + call chain), so 16 KiB overflows.
#[repr(C, align(16))]
struct KernelStack([u8; 64 * 1024]);

static mut STACK_PING: KernelStack = KernelStack([0; 64 * 1024]);
static mut STACK_PONG: KernelStack = KernelStack([0; 64 * 1024]);

// ---------------------------------------------------------------------------
// IPC demo endpoint constants — Milestone 5.
// ---------------------------------------------------------------------------

/// ResourceId / EndpointId shared value for the ping→pong endpoint.
/// Must not overlap with LOG_WRITE_RESOURCE (1) or any test resource.
const PONG_RESOURCE_ID: capability::cap::ResourceId = capability::cap::ResourceId(200);
const PONG_ENDPOINT_ID: ipc::endpoint::EndpointId   = ipc::endpoint::EndpointId(200);
/// Cap-table slot for the IPC endpoint cap (slot 0 = log_write).
const IPC_CAP_SLOT: u64 = 1;

// ---------------------------------------------------------------------------
// IPC demo tasks — Milestone 5.
// ---------------------------------------------------------------------------

/// Sender: loops, building an 8-byte counter message and calling `send`.
/// Prints every 100th send so the log remains readable without flooding.
unsafe extern "C" fn task_ping() -> ! {
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        let bytes = n.to_le_bytes();
        let r = crate::syscall::dispatch::syscall_handler(
            1,            // SyscallNumber::Send
            IPC_CAP_SLOT,
            bytes.as_ptr() as u64,
            8,
        );
        if r == 0 && n % 100 == 0 {
            kprintln!("ping: sent {}", n);
        }
    }
}

/// Receiver: calls `recv` (blocking) and prints every 100th received counter.
unsafe extern "C" fn task_pong() -> ! {
    kprintln!("pong: task started");
    let mut count: u64 = 0;
    loop {
        let r = crate::syscall::dispatch::syscall_handler(
            2,            // SyscallNumber::Recv
            IPC_CAP_SLOT,
            0, 0,
        );
        if r == 0 {
            if let Some(msg) = crate::task::scheduler::take_recv_message() {
                if msg.payload_len == 8 {
                    let mut arr = [0u8; 8];
                    arr.copy_from_slice(&msg.payload[..8]);
                    let n = u64::from_le_bytes(arr);
                    count = count.wrapping_add(1);
                    if count % 100 == 0 {
                        kprintln!("pong: received {} (total {})", n, count);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IPC routing table tests — Milestone 5 (synchronous, before scheduler).
// Tests the routing mechanics without blocking (no tasks yet).
// ---------------------------------------------------------------------------

fn test_ipc_routing() {
    use ipc::endpoint::EndpointId;
    use ipc::message::IpcError;
    use capability::generation::Generation;

    kprintln!("ipc-test: starting routing table tests");

    // Use a scratch endpoint that won't interfere with the demo endpoint (200).
    let ep = EndpointId(999);
    ipc::routing::register(ep, 0, Generation::INITIAL);

    // --- enqueue on empty queue returns Ok(None) (no blocked receiver) ----
    let msg = ipc::Message::new(b"hello").expect("msg");
    match ipc::routing::enqueue(ep, msg, Generation::INITIAL, None) {
        Ok(None) => kprintln!("ipc-test: enqueue ok — message queued"),
        other    => panic!("ipc-test: enqueue unexpected: {:?}", other),
    }

    // --- dequeue returns the message ----------------------------------------
    match ipc::routing::dequeue(ep, Generation::INITIAL, None) {
        Ok((m, None)) => {
            assert_eq!(m.payload_bytes(), b"hello", "payload mismatch");
            kprintln!("ipc-test: dequeue ok — received 'hello'");
        }
        other => panic!("ipc-test: dequeue unexpected: {:?}", other),
    }

    // --- dequeue on empty queue returns QueueEmpty --------------------------
    match ipc::routing::dequeue(ep, Generation::INITIAL, None) {
        Err(IpcError::QueueEmpty) => kprintln!("ipc-test: queue-empty ok"),
        other => panic!("ipc-test: expected QueueEmpty, got: {:?}", other),
    }

    // --- fill to capacity (16 messages) then assert QueueFull ---------------
    for i in 0u8..16 {
        let m = ipc::Message::new(&[i]).expect("fill");
        ipc::routing::enqueue(ep, m, Generation::INITIAL, None).expect("fill enqueue");
    }
    let overflow = ipc::Message::new(b"overflow").expect("overflow");
    match ipc::routing::enqueue(ep, overflow, Generation::INITIAL, None) {
        Err(IpcError::QueueFull) =>
            kprintln!("ipc-test: queue-full ok — QueueFull after 16 msgs"),
        other => panic!("ipc-test: expected QueueFull, got: {:?}", other),
    }

    // --- kill_endpoint → EndpointDead on next send --------------------------
    let ep2 = EndpointId(998);
    ipc::routing::register(ep2, 0, Generation::INITIAL);
    ipc::routing::kill_endpoint(ep2);
    let m2 = ipc::Message::new(b"dead").expect("m2");
    match ipc::routing::enqueue(ep2, m2, Generation::INITIAL, None) {
        Err(IpcError::EndpointDead) =>
            kprintln!("ipc-test: endpoint-dead ok — EndpointDead after kill"),
        other => panic!("ipc-test: expected EndpointDead, got: {:?}", other),
    }

    kprintln!("ipc-test: all routing tests passed");
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

    arch::x86_64::init(boot_info);
    memory::init(boot_info);

    // Program the APIC timer now that HHDM offset is available (§9.1).
    arch::x86_64::init_timer();

    capability::init();
    ipc::init();

    // Synchronous tests (no scheduler running yet, single-core still).
    test_cap_enforcement();
    test_ipc_routing();

    // --- Set up the IPC demo endpoint (Milestone 6) -------------------------
    // Register the endpoint resource in the cap subsystem so caps can be minted.
    capability::register_resource(PONG_RESOURCE_ID);
    // pong lives on core 1 — register the endpoint there so the routing table
    // routes cross-core sends to the right queue.
    ipc::routing::register(
        PONG_ENDPOINT_ID,
        1,
        capability::generation::Generation::INITIAL,
    );

    // SAFETY: reading CR3 is always valid in ring 0.
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem)) };

    // Build capability tables.
    // Slot 0: log_write (both tasks), Slot 1: SEND (ping) or RECV (pong).
    let mut caps_ping = capability::CapTable::empty();
    let log_cap = capability::mint_cap(capability::LOG_WRITE_RESOURCE, capability::Rights::WRITE);
    caps_ping.insert(log_cap).expect("ping log cap");
    let send_cap = capability::mint_cap(PONG_RESOURCE_ID, capability::Rights::SEND);
    let s = caps_ping.insert(send_cap).expect("ping send cap");
    assert_eq!(s, 1, "SEND cap must land in slot 1");

    let mut caps_pong = capability::CapTable::empty();
    let log_cap2 = capability::mint_cap(capability::LOG_WRITE_RESOURCE, capability::Rights::WRITE);
    caps_pong.insert(log_cap2).expect("pong log cap");
    let recv_cap = capability::mint_cap(PONG_RESOURCE_ID, capability::Rights::RECV);
    let r = caps_pong.insert(recv_cap).expect("pong recv cap");
    assert_eq!(r, 1, "RECV cap must land in slot 1");

    // Build initial task contexts.
    let ctx_ping = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_ping,
            STACK_PING.0.as_mut_ptr().add(STACK_PING.0.len()),
            cr3,
        )
    };
    let ctx_pong = unsafe {
        arch::x86_64::context_switch::TaskContext::new_kernel(
            task_pong,
            STACK_PONG.0.as_mut_ptr().add(STACK_PONG.0.len()),
            cr3,
        )
    };

    // ping on core 0 (BSP), pong on core 1 (first AP) — cross-core IPC demo.
    task::scheduler::enqueue("ping", ctx_ping, caps_ping, 0);
    task::scheduler::enqueue("pong", ctx_pong, caps_pong, 1);

    kprintln!("scheduler: ping (core 0) and pong (core 1) enqueued");

    // Start APs after tasks are enqueued so each AP finds its tasks immediately.
    smp::init(boot_info);

    kprintln!("kernel: {} cores ready", smp::core::ready_count());

    // BSP enters the per-core scheduler on core 0; never returns.
    task::scheduler::run(0)
}

/// AP entry point — called by each secondary core after long-mode setup.
#[no_mangle]
pub extern "C" fn ap_main(core_id: u32) -> ! {
    arch::x86_64::ap_init(core_id);
    smp::core::mark_ready(core_id);
    task::scheduler::run(core_id)
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("KERNEL PANIC: {}", info);
    arch::x86_64::halt_all_cores();
}
