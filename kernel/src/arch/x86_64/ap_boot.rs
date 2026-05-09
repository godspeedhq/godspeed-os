//! AP (Application Processor) startup — §11.1, §11.2.
//!
//! Limine has already run each AP through real mode → long mode and left them
//! spinning on their `goto_addr` field.  We simply write the entry function
//! address via `MpInfo::bootstrap` — no INIT+SIPI trampoline required.

use limine::mp::{MpGotoFunction, MpInfo};

/// Start all non-BSP cores and wait for them to reach `mark_ready`.
///
/// Returns the number of APs that responded within the timeout.
///
/// # Safety
/// Must be called after BSP APIC init, before the BSP enters the scheduler.
pub unsafe fn start_all_aps(boot_info: &super::BootInfo) -> u32 {
    if boot_info.ap_ids.is_empty() {
        return 0;
    }

    let resp = match super::SMP_REQUEST.response() {
        Some(r) => r,
        None => {
            crate::kprintln!("smp: no SMP response from bootloader");
            return 0;
        }
    };

    let bsp_lapic = resp.bsp_lapic_id;

    // Store BSP's LAPIC ID for core 0.
    crate::smp::core::set_core_lapic_id(0, bsp_lapic);

    // Assign sequential core IDs to APs (BSP = 0, first AP = 1, …).
    let mut core_idx: u32 = 1;
    for cpu in resp.cpus() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }
        // Record this AP's LAPIC ID before starting it so `lapic_to_core_id`
        // works as soon as the AP reads its own LAPIC register.
        crate::smp::core::set_core_lapic_id(core_idx, cpu.lapic_id);
        // Pass the assigned core_id via Limine's extra_argument field.
        // SAFETY: we have exclusive access to the response at this point; APs
        //         are still spinning and have not read extra_argument yet.
        cpu.bootstrap(ap_limine_entry as MpGotoFunction, core_idx as u64);
        core_idx += 1;
    }

    // Wait for all APs to call mark_ready (or timeout after ~200 ms).
    let expected_ready = boot_info.ap_ids.len() as u32 + 1; // +1 for BSP
    let mut spins: u64 = 0;
    while crate::smp::core::ready_count() < expected_ready && spins < 200_000_000 {
        core::hint::spin_loop();
        spins += 1;
    }

    let actual = crate::smp::core::ready_count().saturating_sub(1); // exclude BSP
    let expected = boot_info.ap_ids.len() as u32;
    if actual < expected {
        crate::kprintln!("smp: warning — only {}/{} APs responded", actual, expected);
    }
    actual
}

/// Entry function Limine calls on each AP after long-mode setup.
///
/// Limine passes the `MpInfo` pointer for this CPU; we read back the
/// `core_id` we stored in `extra_argument` before calling `bootstrap`.
///
/// # Safety
/// Called by Limine on each AP; runs before any kernel state is set up
/// for that core (no stack protection, no IDT, no APIC yet).
unsafe extern "C" fn ap_limine_entry(info: &MpInfo) -> ! {
    let core_id = info.extra_argument() as u32;
    // SAFETY: core_id is valid; this is the standard AP entry path.
    unsafe { crate::ap_main(core_id) }
}
