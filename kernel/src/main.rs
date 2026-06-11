#![no_std]
#![no_main]

mod arch;
mod capability;
mod control;
mod elf_flags;
mod interrupt;
mod invariants;
mod ipc;
mod loader;
mod log;
mod memory;
mod smp;
mod syscall;
mod task;

use core::panic::PanicInfo;

// ---------------------------------------------------------------------------
// Capability enforcement tests — Milestone 4 (synchronous, pre-scheduler).
// ---------------------------------------------------------------------------

fn test_cap_enforcement() {
    use capability::{
        CapError, Rights, LOG_WRITE_RESOURCE,
        mint_cap, mark_dead_resource, revoke_resource, register_resource, ResourceId,
    };
    use capability::table::CapTable;

    kprintln!("cap-test: starting capability enforcement tests");

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

    let empty = CapTable::empty();
    match empty.get(0, Rights::WRITE) {
        Err(CapError::CapNotHeld) =>
            kprintln!("cap-test: 2B pass — no cap returns CapNotHeld"),
        other =>
            panic!("cap-test: 2B FAIL — expected CapNotHeld, got {:?}", other),
    }

    let read_only_cap = mint_cap(LOG_WRITE_RESOURCE, Rights::READ);
    let mut tbl2 = CapTable::empty();
    let slot2 = tbl2.insert(read_only_cap).expect("insert");
    match tbl2.get(slot2, Rights::WRITE) {
        Err(CapError::CapInsufficientRights) =>
            kprintln!("cap-test: 2C pass — wrong right returns CapInsufficientRights"),
        other =>
            panic!("cap-test: 2C FAIL — expected CapInsufficientRights, got {:?}", other),
    }

    let tmp_res = ResourceId(0xDEAD);
    register_resource(tmp_res);
    let tmp_cap = mint_cap(tmp_res, Rights::WRITE);
    let mut tbl3 = CapTable::empty();
    let slot3 = tbl3.insert(tmp_cap).expect("insert");
    assert!(tbl3.get(slot3, Rights::WRITE).is_ok(), "pre-bump should succeed");
    revoke_resource(tmp_res);
    match tbl3.get(slot3, Rights::WRITE) {
        Err(CapError::CapRevoked) =>
            kprintln!("cap-test: revoke pass — stale cap returns CapRevoked"),
        other =>
            panic!("cap-test: revoke FAIL — expected CapRevoked, got {:?}", other),
    }

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

    let grant_res = ResourceId(0xABCD);
    register_resource(grant_res);
    let grantable = mint_cap(grant_res, Rights::READ | Rights::GRANT);
    let mut sender = CapTable::empty();
    let s_slot = sender.insert(grantable).expect("insert");
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
// IPC routing tests — Milestone 5 (synchronous, pre-scheduler).
// ---------------------------------------------------------------------------

fn test_ipc_routing() {
    use ipc::endpoint::EndpointId;
    use ipc::message::IpcError;
    use capability::generation::Generation;

    kprintln!("ipc-test: starting routing table tests");

    let ep = EndpointId(999);
    ipc::routing::register(ep, 0, Generation::INITIAL);

    let msg = ipc::Message::new(b"hello").expect("msg");
    match ipc::routing::enqueue(ep, msg, Generation::INITIAL, None) {
        Ok(None) => kprintln!("ipc-test: enqueue ok — message queued"),
        other    => panic!("ipc-test: enqueue unexpected: {:?}", other),
    }

    match ipc::routing::dequeue(ep, Generation::INITIAL, None) {
        Ok((m, None)) => {
            assert_eq!(m.payload_bytes(), b"hello", "payload mismatch");
            kprintln!("ipc-test: dequeue ok — received 'hello'");
        }
        other => panic!("ipc-test: dequeue unexpected: {:?}", other),
    }

    match ipc::routing::dequeue(ep, Generation::INITIAL, None) {
        Err(IpcError::QueueEmpty) => kprintln!("ipc-test: queue-empty ok"),
        other => panic!("ipc-test: expected QueueEmpty, got: {:?}", other),
    }

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
// Kernel entry point.
// ---------------------------------------------------------------------------

// 512 KiB BSP kernel stack — Limine's boot stack is one page (4 KiB), which
// overflows when pre-scheduler tests pass 4 KiB Message objects by value.
// The linker places this in .bss, so it costs nothing in the image.
static mut BSP_BOOT_STACK: [u8; 512 * 1024] = [0u8; 512 * 1024];

#[no_mangle]
// Called from Limine assembly; safety is enforced by the bootloader contract,
// not by Rust's type system. The function cannot be `unsafe fn` because Limine
// requires a specific extern "C" signature.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn kernel_main(boot_info_ptr: *const arch::x86_64::BootInfo) -> ! {
    // Switch from Limine's tiny boot stack to our own 512 KiB stack before
    // any locals are allocated.  boot_info_ptr is in RDI (a register) so it
    // survives the RSP change.
    //
    // SAFETY: BSP_BOOT_STACK is a static 512 KiB buffer; top pointer is
    // 16-byte aligned.  boot_info_ptr remains valid — it points to data in
    // Limine's boot-time memory, not on the old stack.
    unsafe {
        let top = (core::ptr::addr_of_mut!(BSP_BOOT_STACK) as *mut u8).add(512 * 1024);
        let top_aligned = (top as usize & !15) as u64;
        core::arch::asm!(
            "mov rsp, {0}",
            in(reg) top_aligned,
            options(nostack),
        );
    }

    // SAFETY: bootloader guarantees boot_info_ptr is valid and aligned.
    let boot_info = unsafe { &*boot_info_ptr };

    arch::x86_64::init(boot_info);
    memory::init(boot_info);

    // Hardening: unmap a guard page below each kernel-stack slot so an overflow
    // faults loudly instead of corrupting the neighbouring stack. Done here — BSP
    // only, before APs and before any kstack is allocated, so no TLB shootdown is
    // needed and init's stack already carries its guard. (Safe fn — boot-ordering
    // contract, not UB; the page-unmap unsafe lives in the arch layer.)
    task::install_kstack_guards();

    // Stage 1 of the USB stack: locate the xHCI controller (§12). Records its
    // MMIO base + IRQ for a future userspace driver's hw_mmio/hw_interrupt caps.
    arch::x86_64::pci::init();

    // Take a USB controller from the firmware (BIOS→OS handoff) before the IOMMU
    // confines it — otherwise the firmware SMM keeps running its DMA out of
    // firmware memory, which faults under confinement and breaks the keyboard.
    //
    // Handoff is only needed for a controller we confine (otherwise firmware SMM
    // keeps running its DMA out of firmware memory, which faults under
    // confinement). It is gated on the same master switch as confinement: with
    // CONFINE_USB_DRIVERS off (the working daily-driver default) NO handoff runs,
    // so firmware keeps co-owning both controllers exactly as before the H1 branch
    // — the configuration in which both keyboards work. Flip the switch to
    // re-enable the xHCI confinement flagship (hands off + confines xHCI only).
    if task::CONFINE_USB_DRIVERS {
        arch::x86_64::pci::xhci_bios_handoff();
    }

    // H1 Phase 0: probe ACPI for an AMD-Vi IOMMU (IVRS). Detection only — reports
    // whether this machine can confine DMA-capable drivers to their granted
    // arena (the prerequisite for dropping xhci/ehci from the TCB).
    arch::x86_64::iommu::detect(boot_info.rsdp_addr, boot_info.hhdm_offset);
    // H1 Phase 1a: if an IOMMU was found, map its MMIO and read capabilities.
    arch::x86_64::iommu::bringup(boot_info.hhdm_offset);

    arch::x86_64::init_timer();
    arch::x86_64::com2_init();
    // COM1 RX is polled from the core-0 timer ISR (uart_rx_poll every 10 ms).
    // IRQ-driven reception was abandoned because the kernel masks all PIC IRQs
    // globally (APIC-only kernel). uart_rx_enable() must NOT be called: on real
    // hardware unmasking PIC IRQ 4 without proper PIC EOI in the handler causes
    // the PIC ISR to jam and lock up the interrupt controller before boot.

    capability::init();
    ipc::init();

    // Synchronous correctness tests (§22 Tests 2 and 3).
    test_cap_enforcement();
    test_ipc_routing();

    // ELF-loader fuzz mode (§22 Fuzz F3): run 77 malformed-ELF inputs and halt.
    // Never reaches the normal boot path when this feature is enabled.
    #[cfg(feature = "test-bad-elf")]
    loader::run_elf_fuzz();

    // ELF-loader brutal fuzz (§22 Fuzz BF3): 263 inputs (13 specific + 200 random
    // single-byte + 50 multi-byte mutations). Never reaches normal boot.
    #[cfg(feature = "test-bad-elf-brutal")]
    loader::run_elf_fuzz_brutal();

    // Normal boot: spawn init, bring up APs, enter per-core scheduler (never returns).
    #[cfg(not(any(feature = "test-bad-elf", feature = "test-bad-elf-brutal")))]
    {
        task::spawn_init();
        smp::init(boot_info);

        // Hardening H4b: Limine maps the HHDM W+X (RWX direct map of all RAM — a
        // kernel-wide W^X bypass). Force it NO_EXEC now that all APs are up: Limine's
        // AP long-mode bring-up runs through the executable direct map, so this must
        // come AFTER smp::init. From here nothing executes from the HHDM (the kernel
        // runs from its own .text), so the direct map is data-only for the rest of
        // runtime. audit_wx then confirms the HHDM reads NX=1.
        // Safe fn — boot-ordering contract (must follow smp::init), not UB; the
        // CR3/PTE unsafe lives in the arch layer.
        arch::x86_64::page_tables::harden_hhdm_nx();
        arch::x86_64::boot::audit_wx();

        kprintln!("kernel: {} cores ready", smp::core::ready_count());
        kprintln!(
            "idle: cores may halt = {} (cool when idle if true)",
            arch::x86_64::interrupts::idle_can_halt()
        );
        task::scheduler::run(0)
    }
}

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
