//! AP (Application Processor) startup — §11.1, §11.2.
//!
//! x86 APs start in real mode. The BSP copies a 16-bit trampoline to a
//! page below 1 MiB and sends INIT+SIPI IPIs. Each AP executes:
//!   real mode → protected mode → long mode → ap_main(core_id)
//!
//! The trampoline page is reclaimed once all APs have called ap_main.

/// Page-aligned physical address for the real-mode trampoline (below 1 MiB).
const TRAMPOLINE_PHYS: u64 = 0x8000;

/// Send INIT+SIPI IPIs to all non-BSP local APIC IDs.
///
/// Returns the number of APs that successfully called `ap_main` within
/// the timeout window. If any AP fails to respond, a warning is logged
/// and startup continues with the available cores (§11.3).
///
/// # Safety
/// Must be called after BSP APIC init, before any AP-targeted syscall.
pub unsafe fn start_all_aps(boot_info: &super::BootInfo) -> u32 {
    // SAFETY: caller guarantees APIC is initialised.
    unsafe {
        install_trampoline();
        let ap_count = send_sipi_sequence(boot_info);
        wait_for_aps(ap_count)
    }
}

unsafe fn install_trampoline() {
    todo!("copy 16-bit trampoline blob to TRAMPOLINE_PHYS; patch in long-mode GDT ptr and ap_main address")
}

unsafe fn send_sipi_sequence(boot_info: &super::BootInfo) -> u32 {
    todo!("for each AP APIC ID in boot_info.ap_ids: send INIT, delay, send SIPI twice")
}

unsafe fn wait_for_aps(expected: u32) -> u32 {
    todo!("spin until smp::core::ready_count() == expected or 200 ms timeout; log warning for missing APs")
}
