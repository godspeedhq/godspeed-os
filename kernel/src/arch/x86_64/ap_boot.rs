// SPDX-License-Identifier: GPL-2.0-only
//! AP (Application Processor) startup - §11.1, §11.2.
//!
//! Limine has already run each AP through real mode → long mode and left them
//! spinning on their `goto_addr` field.  We simply write the entry function
//! address via `MpInfo::bootstrap` - no INIT+SIPI trampoline required.

use limine::mp::{MpGotoFunction, MpInfo};
/// Publish the BSP's own LAPIC id as core 0's, and as the fallback destination.
///
/// This work used to happen ONLY inside `start_all_aps`, which a `single-core` build never calls - so
/// on that build `CORE_LAPIC_ID[0]` kept its initial 0 for the life of the machine while the BSP's real
/// id was whatever the silicon said. aarch64 has always published core 0 unconditionally
/// (`set_core_lapic_id(0, 0)`); x86 buried it in AP startup, so removing the APs removed it too.
///
/// What that broke: `pci_msi_vector` aims a device's MSI at the core its driver is pinned to, and on a
/// single-core boot that core IS core 0 - so `is_ready(0)` is true, the BSP fallback is never reached,
/// and the destination came from this unwritten 0. On the T630, whose BSP is LAPIC id 16, every MSI was
/// addressed to a core that does not answer: `0 MSI` for the life of the boot, with every stage of the
/// setup reporting success. The EHCI was unaffected because it routes INTx through `bsp_lapic_id()`
/// directly, and QEMU was unaffected because its BSP really is id 0 - so the wrong value was
/// accidentally right on the only machine the tests run on.
///
/// Read from the LAPIC's own ID register rather than the boot protocol: it is the hardware's answer,
/// it needs no response struct, and on this path we are unambiguously running on the BSP.
pub fn publish_bsp_lapic_id() {
    // SAFETY: called on the BSP from `smp::init`, after `init_local_apic` and after the per-core
    // arenas exist (`smp::percpu_init` runs earlier in `main`).
    let id = unsafe { super::boot::get_lapic_id() };
    crate::smp::core::set_core_lapic_id(0, id);
    super::ioapic::set_bsp_lapic_id(id as u8);
    crate::kprintln!("smp: BSP is LAPIC id {} (core 0)", id);
}


/// Start all non-BSP cores and wait for them to reach `mark_ready`.
///
/// Returns the number of APs that responded within the timeout.
///
/// # Safety
/// Must be called after BSP APIC init, before the BSP enters the scheduler.
pub unsafe fn start_all_aps(_boot_info: &super::BootInfo) -> u32 {
    let ap_count = super::ap_count();
    if ap_count == 0 {
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

    // The APs are filtered against XAPIC_MAX_LAPIC_ID below; the BSP must be held to the same ceiling
    // (audit K2). If the BSP's own LAPIC id exceeds 8 bits, `lapic_id & 0xFF` in a targeted IPI would
    // silently mis-address it, so an AP->BSP wake (cross-core recv) would land on the wrong core. Say
    // so LOUDLY (§26.7) rather than let cross-core IPC fail invisibly - the honest ceiling until x2APIC.
    if bsp_lapic > super::XAPIC_MAX_LAPIC_ID {
        crate::kprintln!(
            "smp: WARNING - BSP LAPIC id {} > {}: AP->BSP IPIs will mis-address (needs x2APIC)",
            bsp_lapic, super::XAPIC_MAX_LAPIC_ID
        );
    }

    // Store BSP's LAPIC ID for core 0.
    crate::smp::core::set_core_lapic_id(0, bsp_lapic);

    // Assign sequential core IDs to APs (BSP = 0, first AP = 1, …).
    let mut core_idx: u32 = 1;
    let mut unaddressable = 0u32;
    for cpu in resp.cpus() {
        if cpu.lapic_id == bsp_lapic {
            continue;
        }
        // xAPIC can only target an 8-bit LAPIC id (§ ap_count / XAPIC_MAX_LAPIC_ID). A core beyond that
        // cannot receive a targeted IPI, so exclude it LOUDLY (§26.7) rather than silently mis-address
        // it - the honest ceiling until the APIC layer gains x2APIC. `ap_count` filters identically, so
        // the per-core arenas were sized for exactly the cores started here.
        if cpu.lapic_id > super::XAPIC_MAX_LAPIC_ID {
            unaddressable += 1;
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
    if unaddressable > 0 {
        crate::kprintln!(
            "smp: {} core(s) have LAPIC id > {} - not addressable by xAPIC IPI, excluded (needs x2APIC)",
            unaddressable, super::XAPIC_MAX_LAPIC_ID
        );
    }

    // Wait for all APs to call mark_ready, or time out (~10x the old 200M-spin
    // budget). On the HP T630 (AMD GX-420GI) the APs come up just *after* the
    // old budget expired: the BSP declared single-core, started the scheduler,
    // then the late APs joined and raced the committed single-core state →
    // triple fault → silent reset. A generous budget lets every AP check in
    // before the BSP proceeds, so the loop exits on ready_count (not timeout)
    // and there is no late-join race. (Spin-count is CPU-speed-dependent; a
    // TSC-based wall-clock bound would be more robust - future work.)
    let expected_ready = ap_count as u32 + 1; // +1 for BSP
    let mut spins: u64 = 0;
    while crate::smp::core::ready_count() < expected_ready && spins < 2_000_000_000 {
        core::hint::spin_loop();
        spins += 1;
    }

    let actual = crate::smp::core::ready_count().saturating_sub(1); // exclude BSP
    let expected = ap_count as u32;
    if actual < expected {
        crate::kprintln!("smp: warning - only {}/{} APs responded", actual, expected);
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
    // core_id is valid; this is the standard AP entry path.
    crate::ap_main(core_id)
}
