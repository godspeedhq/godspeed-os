//! Inter-Processor Interrupts — §9.4, §8.4, §10.5.
//!
//! Three distinct purposes:
//!   1. Wake a task blocked on `recv` after a cross-core `send` enqueues.
//!   2. TLB shootdown after a page is unmapped (§10.5).
//!   3. Cross-core scheduler preemption (timer overflow).

use core::sync::atomic::{AtomicU32, Ordering};

/// Vector numbers for each IPI purpose.
pub mod vectors {
    pub const WAKE_RECEIVER:  u8 = 0xF0;
    pub const TLB_SHOOTDOWN:  u8 = 0xF1;
    pub const SCHEDULER_TICK: u8 = 0xF2;
}

// xAPIC ICR register offsets (relative to APIC base).
const APIC_ICR_HIGH: u64 = 0x310;
const APIC_ICR_LOW:  u64 = 0x300;

// Counts acknowledgments from remote cores during a TLB shootdown.
static TLB_ACK: AtomicU32 = AtomicU32::new(0);
// Virtual address being invalidated in the current shootdown broadcast.
static mut TLB_SHOOTDOWN_ADDR: u64 = 0;

/// Send a fixed-delivery IPI to a specific core.
///
/// # Safety
/// `core_id` must refer to a ready core; the local APIC must be mapped.
pub unsafe fn send_ipi(core_id: u32, vector: u8) {
    let lapic_id  = crate::smp::core::core_lapic_id(core_id);
    let apic_base = unsafe { crate::arch::x86_64::boot::get_apic_virt_base() };

    // xAPIC ICR write protocol: write high word (destination) first, then
    // low word (vector + delivery mode), which triggers delivery.
    // Fixed delivery, edge-triggered, assert (bit 14), no shorthand.
    // SAFETY: apic_base is a valid MMIO address set during init_local_apic.
    unsafe {
        write_apic_reg(apic_base + APIC_ICR_HIGH, (lapic_id & 0xFF) << 24);
        write_apic_reg(apic_base + APIC_ICR_LOW,  (vector as u32) | (1 << 14));
    }
}

/// Broadcast a TLB shootdown IPI to all other cores and wait for acks (§10.5).
///
/// # Safety
/// Interrupts should be disabled on the calling core before this call.
pub unsafe fn broadcast_tlb_shootdown(virt_addr: u64) {
    let ncores = crate::smp::core::ready_count();
    if ncores <= 1 {
        return; // single-core; no remote TLBs to invalidate
    }

    // SAFETY: single writer; caller guarantees IF=0.
    unsafe { TLB_SHOOTDOWN_ADDR = virt_addr; }
    TLB_ACK.store(0, Ordering::SeqCst);

    let apic_base = unsafe { crate::arch::x86_64::boot::get_apic_virt_base() };

    // ICR shorthand = 0b11 (all-excluding-self, bits 19:18), fixed delivery,
    // edge trigger, assert (bit 14).
    // SAFETY: APIC mapped; caller holds IF=0.
    unsafe {
        write_apic_reg(apic_base + APIC_ICR_HIGH, 0);
        write_apic_reg(
            apic_base + APIC_ICR_LOW,
            (vectors::TLB_SHOOTDOWN as u32) | (1 << 14) | (0b11 << 18),
        );
    }

    // Spin until every other core has acknowledged.
    let expected = ncores - 1;
    while TLB_ACK.load(Ordering::SeqCst) < expected {
        core::hint::spin_loop();
    }
}

/// IPI handler — invoked from the IDT stub (`ipi_dispatch`) on the receiving core.
///
/// # Safety
/// Called from raw interrupt context with interrupts disabled.
pub unsafe fn ipi_handler(vector: u8) {
    match vector {
        vectors::WAKE_RECEIVER => {
            // wake_by_slot already set the task's state to Ready.  The IPI's
            // sole purpose is to wake the target core from `hlt`.  After iretq
            // the scheduler loop calls pick_next and switches to the ready task.
            // Calling yield_current here would save the ISR stack into
            // CORE_SCHED_CTX and corrupt the scheduler context (§9.4).
        }
        vectors::TLB_SHOOTDOWN => handle_tlb_shootdown(),
        vectors::SCHEDULER_TICK => {
            // Same reasoning: just let the loop pick up new work after iretq.
        }
        _ => {}
    }
    // Send EOI to allow the next interrupt to be delivered.
    // SAFETY: called from interrupt context; APIC is mapped.
    unsafe { crate::arch::x86_64::boot::apic_send_eoi() }
}

/// Broadcast a full TLB flush to all other cores and flush locally (§10.5).
///
/// Used when an entire address space is torn down (task death) so that all
/// non-global TLB entries are invalidated on every core before the backing
/// frames are returned to the allocator.
///
/// Saves and restores the caller's interrupt flag, so this may be called
/// with interrupts either enabled or disabled.
///
/// # Safety
/// The local APIC must be initialised.
pub unsafe fn broadcast_full_tlb_flush() {
    // Save interrupt flag and disable interrupts for the shootdown protocol.
    // SAFETY: pushfq/cli are always valid in ring 0.
    let rflags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nostack));
        core::arch::asm!("cli", options(nostack, nomem));
    }

    // Flush locally: reload CR3 invalidates all non-global TLB entries on this core.
    // SAFETY: CR3 reload with the same value is always valid in ring 0.
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, nomem));
    }

    let ncores = crate::smp::core::ready_count();
    if ncores > 1 {
        // !0u64 is the sentinel that tells remote handlers to do a full CR3
        // reload rather than a single-page invlpg.
        // SAFETY: single writer; IF=0 prevents a racing per-page shootdown from
        // overwriting TLB_SHOOTDOWN_ADDR between our write and the remote reads.
        unsafe { TLB_SHOOTDOWN_ADDR = !0u64; }
        TLB_ACK.store(0, Ordering::SeqCst);

        let apic_base = unsafe { crate::arch::x86_64::boot::get_apic_virt_base() };

        // All-excluding-self broadcast, fixed delivery, edge trigger, assert (bit 14).
        // SAFETY: APIC mapped; IF=0.
        unsafe {
            write_apic_reg(apic_base + APIC_ICR_HIGH, 0);
            write_apic_reg(
                apic_base + APIC_ICR_LOW,
                (vectors::TLB_SHOOTDOWN as u32) | (1 << 14) | (0b11 << 18),
            );
        }

        let expected = ncores - 1;
        while TLB_ACK.load(Ordering::SeqCst) < expected {
            core::hint::spin_loop();
        }
    }

    // Restore the caller's interrupt flag.
    // SAFETY: push/popfq restores exactly the flags in effect on entry.
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) rflags, options(nostack));
    }
}

fn handle_tlb_shootdown() {
    // SAFETY: TLB_SHOOTDOWN_ADDR is written before the broadcast and read-only
    // until TLB_ACK reaches the expected count.
    let addr = unsafe { TLB_SHOOTDOWN_ADDR };
    if addr == !0u64 {
        // Full-flush sentinel: reload CR3 to invalidate all non-global TLB entries.
        // SAFETY: CR3 reload with the same value is always valid in ring 0.
        unsafe {
            let cr3: u64;
            core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
            core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, nomem));
        }
    } else {
        // SAFETY: invlpg is always safe in ring 0; addr is a virtual address.
        unsafe { core::arch::asm!("invlpg [{addr}]", addr = in(reg) addr, options(nostack)); }
    }
    TLB_ACK.fetch_add(1, Ordering::SeqCst);
}

#[inline]
unsafe fn write_apic_reg(addr: u64, val: u32) {
    // SAFETY: addr is a valid MMIO register inside the mapped APIC page.
    unsafe { (addr as *mut u32).write_volatile(val) }
}
