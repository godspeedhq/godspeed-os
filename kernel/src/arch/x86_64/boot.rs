// SPDX-License-Identifier: GPL-2.0-only
//! BSP/AP hardware initialisation - §11.1, §11.2.

use core::sync::atomic::{AtomicBool, Ordering};
use portable_atomic::AtomicU64;

use super::BootInfo;
use crate::smp::percpu::PerCoreMut;

// ---------------------------------------------------------------------------
// GDT - eight 64-bit descriptors (per core).
//
// Slot  Selector  Descriptor
//   0    0x00      null
//   1    0x08      kernel code: 64-bit, ring-0, execute/read
//   2    0x10      kernel data: ring-0, read/write
//   3    0x18      placeholder - SYSRETQ needs the 0x18 base to derive
//                  user SS (0x18+8=0x20) and CS (0x18+16=0x28)
//   4    0x20      user data:   ring-3, read/write
//   5    0x28      user code:   64-bit, ring-3, execute/read
//   6    0x30  ]   TSS descriptor (16-byte system descriptor = 2 slots)
//   7    0x38  ]
//
// STAR MSR encodes kernel CS at [47:32]=0x08 and SYSRETQ base at [63:48]=0x18.
//
// The CPU writes the Accessed bit into segment descriptors, and `ltr` writes
// the "busy" bit into the TSS descriptor - both require .data (writable).
// ---------------------------------------------------------------------------

const GDT_TEMPLATE: [u64; 8] = [
    0x0000_0000_0000_0000, // null
    0x00AF_9A00_0000_FFFF, // kernel code  (0x08): 64-bit, ring-0, X/R
    0x00CF_9200_0000_FFFF, // kernel data  (0x10): ring-0, R/W
    0x0000_0000_0000_0000, // placeholder  (0x18): required for SYSRETQ alignment
    0x008F_F300_0000_FFFF, // user data    (0x20): ring-3, R/W; D/B=0 required by VMX (SS.D/B must be 0 in 64-bit guest mode)
    0x00AF_FA00_0000_FFFF, // user code    (0x28): 64-bit, ring-3, X/R
    0x0000_0000_0000_0000, // TSS low      (0x30): filled by init_gdt(core_id)
    0x0000_0000_0000_0000, // TSS high     (0x38): filled by init_gdt(core_id)
];

/// The BSP's single-core bootstrap GDT, set up in `init_gdt(0)` BEFORE the frame allocator exists and
/// used by the BSP for its lifetime. Only the BSP needs a static; the APs use `GDT_ARENA` (§26.6.1),
/// boot-sized to the real core count instead of `[_; MAX_CORES]`.
#[link_section = ".data"]
static mut BSP_GDT: [u64; 8] = GDT_TEMPLATE;
/// Per-AP GDT arena, boot-allocated, sized to N (slot 0 unused - the BSP uses `BSP_GDT`).
static GDT_ARENA: PerCoreMut<[u64; 8]> = PerCoreMut::new();

// ---------------------------------------------------------------------------
// TSS (Task State Segment) - one per core.
//
// The CPU uses TSS.rsp0 when a ring-3 task is interrupted (hardware switches
// to ring-0 and pushes the interrupt frame starting at rsp0).  We update rsp0
// before every switch to a ring-3 task so each task has its own kernel stack.
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct Tss {
    _res0:       u32,        // offset   0
    rsp0:        u64,        // offset   4  ← ring-0 stack on ring-3 interrupt
    rsp1:        u64,        // offset  12  (unused)
    rsp2:        u64,        // offset  20  (unused)
    _res1:       u64,        // offset  28
    ist:         [u64; 7],   // offset  36..92  (IST stacks, unused in v1)
    _res2:       u64,        // offset  92
    _res3:       u16,        // offset 100
    io_map_base: u16,        // offset 102  (104 = past limit → no IOPB → ring-3 I/O faults)
}

// io_map_base = 104: the IOPB base is past the TSS limit (103), so the CPU
// denies all port I/O from ring-3 (services must use MMIO caps instead).
/// Initial TSS: `io_map_base = 104` (past the limit -> ring-3 port I/O faults). Shared by the BSP
/// bootstrap and each AP arena slot.
const TSS_INIT: Tss = Tss {
    _res0: 0, rsp0: 0, rsp1: 0, rsp2: 0,
    _res1: 0, ist: [0; 7], _res2: 0, _res3: 0,
    io_map_base: 104,
};
/// The BSP's single-core bootstrap TSS (see `BSP_GDT`). The APs use `TSS_ARENA`.
#[link_section = ".data"]
static mut BSP_TSS: Tss = TSS_INIT;
/// Per-AP TSS arena, boot-allocated, sized to N (slot 0 unused).
static TSS_ARENA: PerCoreMut<Tss> = PerCoreMut::new();

/// Allocate the per-AP GDT/TSS arenas for `n` cores. Call ONCE at boot after the frame allocator is up
/// and before any AP starts (the BSP already runs on `BSP_GDT`/`BSP_TSS`). Slot 0 is unused.
pub fn init_gdt_arenas(n: usize) {
    GDT_ARENA.init_with(n, |_| GDT_TEMPLATE);
    TSS_ARENA.init_with(n, |_| TSS_INIT);
}

/// `*mut` to core `cid`'s GDT - the BSP's static bootstrap for core 0, the arena for an AP.
#[inline]
fn gdt_for(cid: usize) -> *mut [u64; 8] {
    if cid == 0 { core::ptr::addr_of_mut!(BSP_GDT) } else { GDT_ARENA.as_mut_ptr(cid) }
}

/// `*mut` to core `cid`'s TSS - the BSP's static bootstrap for core 0, the arena for an AP.
#[inline]
fn tss_for(cid: usize) -> *mut Tss {
    if cid == 0 { core::ptr::addr_of_mut!(BSP_TSS) } else { TSS_ARENA.as_mut_ptr(cid) }
}

// ---------------------------------------------------------------------------
// IDT - 256 interrupt gates.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_low:  u16,
    selector:    u16, // kernel code segment = 0x08
    ist:         u8,  // 0 = use current RSP
    type_attr:   u8,  // 0x8E = present, ring 0, interrupt gate
    offset_mid:  u16,
    offset_high: u32,
    _reserved:   u32,
}

impl IdtEntry {
    const ABSENT: Self = Self {
        offset_low: 0, selector: 0, ist: 0, type_attr: 0,
        offset_mid: 0, offset_high: 0, _reserved: 0,
    };

    fn new(handler: u64) -> Self {
        Self {
            offset_low:  handler as u16,
            selector:    0x08,
            ist:         0,
            type_attr:   0x8E, // P=1, DPL=0, interrupt gate (IF cleared on entry)
            offset_mid:  (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved:   0,
        }
    }

    /// Like `new` but DPL=3 - ring-3 code may invoke this vector via `int N`.
    fn new_user(handler: u64) -> Self {
        Self { type_attr: 0xEE, ..Self::new(handler) } // P=1, DPL=3, interrupt gate
    }
}

// SAFETY: written only during init_idt before APs start; read-only after.
static mut IDT: [IdtEntry; 256] = [IdtEntry::ABSENT; 256];

// ---------------------------------------------------------------------------
// Shared descriptor table pointer format.
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct TableDescriptor {
    limit: u16,
    base:  u64,
}

// ---------------------------------------------------------------------------
// Local APIC MMIO - set during init_local_apic, read by apic_send_eoi.
// ---------------------------------------------------------------------------

static mut APIC_VIRT_BASE: u64 = 0;

// APIC register offsets (xAPIC MMIO, 32-bit accesses).
const APIC_ID:           u64 = 0x020;
const APIC_TPR:          u64 = 0x080; // Task Priority Register - must be 0 to accept all vectors
const APIC_EOI:          u64 = 0x0B0;
const APIC_SPURIOUS:     u64 = 0x0F0;
const APIC_LVT_TIMER:    u64 = 0x320;
const APIC_TIMER_INIT:   u64 = 0x380;
const APIC_TIMER_CURRENT: u64 = 0x390;
const APIC_TIMER_DIVIDE: u64 = 0x3E0;

// TSC-Deadline timer MSR (IA32_TSC_DEADLINE).  Writing a 64-bit TSC value
// here arms a one-shot timer that fires when RDTSC reaches that value.
// The TSC runs in all C-states, so delivery is guaranteed even on Goldmont+
// where the APIC counter is power-gated in deep package C-states.
const MSR_IA32_TSC_DEADLINE: u32 = 0x6E0;

/// Set by `init_local_apic` when TSC-Deadline mode is selected.
/// Read by `timer_tick_from_irq` to decide whether to re-arm.
pub static TSC_DEADLINE_MODE: AtomicBool = AtomicBool::new(false);

/// TSC ticks per 10 ms quantum.  Set once during BSP boot; read by every core.
static TSC_TICKS_PER_QUANTUM: AtomicU64 = AtomicU64::new(0);

/// Calibrated LAPIC periodic-timer initial count for one ~10 ms quantum, or 0 if calibration was
/// unavailable (then `PERIODIC_TIMER_COUNT` is used). Measured once on the BSP against the PIT; APs
/// read the stored value.
///
/// This exists because the periodic period is `init_count * divisor / f_apic`, and `f_apic` is
/// machine-dependent, so no single hardcoded count can be right everywhere. The previous fixed
/// 6_250_000 yields ~100 ms on QEMU (1 GHz APIC clock) but ~1 s on the T630 (~100 MHz) - 100x the
/// intended 10 ms quantum. That coarse quantum is what made `sleep` bottom out near a second,
/// preemption land ~1 s apart, and every timed wake (`recv_timeout`, hot-plug, auto-repeat) equally
/// coarse on that machine.
static PERIODIC_TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Diagnostic exception flags - set as the very first action inside each
// exception stub, before any serial output that might stall with IF=0.
// Reported by timer ISR ticks 10/11/12 on whichever cores are still alive.
// ---------------------------------------------------------------------------

/// Set (to 1) by the `exception_halt` stub before `cli`.
/// Tick 10 reports [EXC-HALT-YES/NO].
pub static EXCEPTION_HALT_REACHED: AtomicBool = AtomicBool::new(false);

/// Set (to 1) by `pf_stub` as its very first instruction (before swapgs).
/// Tick 11 reports [PF-YES/NO] and the stored CR2 value.
pub static PF_REACHED: AtomicBool = AtomicBool::new(false);

/// CR2 at the time of the first #PF; written by `pf_stub` before swapgs.
pub static PF_CR2_STORED: AtomicU64 = AtomicU64::new(0);

/// Set (to 1) by `gpf_stub` as its very first instruction.
/// Tick 12 reports [GP-YES/NO].
pub static GP_REACHED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Public init surface.
// ---------------------------------------------------------------------------

/// BSP-only initialisation: GDT, TSS, IDT, paging, SYSCALL MSRs.
/// APIC timer is programmed separately via `init_local_apic` after memory init.
///
/// # Safety
/// Called exactly once before any other kernel subsystem.
pub unsafe fn init_bsp(boot_info: &BootInfo) {
    unsafe {
        mask_pic();   // silence 8259 before IDT is live; avoids vector-8 collision
        init_gdt(0);
        init_idt();
        init_paging(boot_info);
        init_syscall(0);
    }
}

/// Boot-time W^X audit + lock-in assertion (hardening H4b). The kernel inherits all of its own page
/// tables from Limine (`init_paging` is a no-op), so this logs the NO-EXECUTE (NX, bit 63) and WRITABLE
/// (W, bit 1) status of three representative pages and *asserts* the W^X invariant on EACH - no mapped
/// page may be both writable AND executable:
///   - the HHDM alias of a RAM page - a read/write alias of physical memory; after `harden_hhdm_nx` it
///     MUST be non-executable, else every writable page has an executable alias (a kernel-wide bypass).
///   - a kernel code (.text) page - expected executable (NX=0), read-only (W=0).
///   - a kernel data (.bss) page - expected writable (W=1), non-exec (NX=1).
/// Run after `harden_hhdm_nx`. A regression that leaves ANY of them W+X now fails the boot loudly (§3.12)
/// rather than shipping a silent hole. (The W^X FOUNDATION - EFER.NXE, without which these NX bits are
/// ignored - is set + asserted per-core in `init_syscall`.)
pub fn audit_wx() {
    use crate::arch::x86_64::page_tables::{entry_for_va, get_hhdm_offset};
    const NX: u64 = 1 << 63;
    const W: u64 = 1 << 1;
    let report = |name: &str, va: u64| -> Option<u64> {
        match entry_for_va(va) {
            Some(e) => {
                crate::kprintln!(
                    "wx-audit: {:<11} va={:#018x} W={} NX={} {}",
                    name, va, (e & W != 0) as u8, (e & NX != 0) as u8,
                    if e & W != 0 && e & NX == 0 { "<<< W+X" } else { "ok" });
                // W^X (H4): NO sampled page may be both writable AND executable. Assert EVERY page now,
                // not only the HHDM - a regression that leaves one W+X fails the boot loudly (§3.12)
                // instead of shipping a silent hole. (kernel-text is RX: W=0; kernel-data + HHDM are
                // RW-NX: NX=1 - all pass today.)
                assert!(!(e & W != 0 && e & NX == 0),
                    "W^X violation: {} (va={:#018x}) is WRITABLE and EXECUTABLE", name, va);
                Some(e)
            }
            None => {
                crate::kprintln!("wx-audit: {:<11} va={:#018x} UNMAPPED", name, va);
                None
            }
        }
    };
    // HHDM alias of a real RAM page. One frame is allocated to guarantee a
    // usable-RAM physical address; it is intentionally leaked - one 4 KiB page,
    // once at boot (there is no free_frame in v1, and this runs exactly once).
    let _ = crate::memory::allocator::alloc_frame()
        .and_then(|f| report("hhdm-ram", get_hhdm_offset() + f.phys_addr().0));
    report("kernel-text", audit_wx as usize as u64);
    report("kernel-data", core::ptr::addr_of!(TSC_DEADLINE_MODE) as usize as u64);
    // Every page sampled above is asserted W^X inside `report` (no W+X) - the HHDM, historically the
    // one left W+X, is covered there too (it runs after harden_hhdm_nx).
}

/// Program the local APIC timer for a ~10 ms periodic interrupt on vector 32.
/// Must be called after `memory::init` (needs HHDM offset).
///
/// # Safety
/// Called once per core (BSP from kernel_main, APs from ap_main) after HHDM is set.
pub unsafe fn init_local_apic() {
    // Read APIC base physical address from IA32_APIC_BASE MSR (0x1B).
    let (lo, hi): (u32, u32);
    // SAFETY: RDMSR is privileged; ring 0 throughout kernel boot.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0x1Bu32,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem),
        );
    }
    let apic_phys = ((hi as u64) << 32) | (lo as u64 & !0xFFF_u64);
    let apic_virt = crate::arch::x86_64::page_tables::get_hhdm_offset() + apic_phys;

    // Limine's HHDM maps RAM but not MMIO regions.  Ensure the APIC frame is
    // reachable by adding it to the active page tables before the first write.
    // PCD (bit 4) + PWT (bit 3) disable caching for MMIO correctness.
    {
        use crate::arch::x86_64::page_tables::{PageFlags, map_in_active_tables};
        let mmio_flags = PageFlags::PRESENT.bits()
                       | PageFlags::WRITABLE.bits()
                       | (1 << 3)   // PWT
                       | (1 << 4);  // PCD
        // SAFETY: called after set_hhdm_offset; APIC page is MMIO.
        unsafe { map_in_active_tables(apic_virt, apic_phys, mmio_flags) }
            .unwrap_or_else(|_| {
                // If the mapping already exists (second core, or pre-mapped),
                // that is fine - we just proceed.
            });
    }

    // SAFETY: APIC_VIRT_BASE written once per core before apic_send_eoi is called.
    unsafe { APIC_VIRT_BASE = apic_virt };

    // UEFI may leave the Task Priority Register (TPR) non-zero on the BSP,
    // which blocks delivery of any interrupt vector whose priority class ≤ TPR.
    // Vector 0xF0 (WAKE_RECEIVER) has priority class 0xF; if TPR = 0xF0 the APIC
    // silently discards it while the LVT timer still fires (LVT ignores TPR).
    // Explicitly zero TPR so all interrupt classes are accepted.
    // SAFETY: apic_virt is mapped above via map_in_active_tables; APIC_TPR offset
    // is within the 4 KiB APIC MMIO page established by that mapping.
    unsafe { write_apic(apic_virt, APIC_TPR, 0x00) };

    // Enable APIC software: set bit 8 of the spurious interrupt vector register.
    // Spurious vector = 0xFF (unused; interrupt gates handle real vectors).
    write_apic(apic_virt, APIC_SPURIOUS, 0x1FF);

    // SAFETY: ring-0; called once per core; APIC is already initialised above.
    let lapic_id = unsafe { get_lapic_id() };

    // Probe for TSC-Deadline timer support (CPUID.1.ECX[24]).
    // On Goldmont+ (Wyse 5070 J5005), the periodic APIC timer is silenced when
    // the firmware promotes the SoC package to PC6+ (APIC power-gated).  The
    // TSC continues running in all C-states, so TSC-Deadline fires even then.
    let tsc_deadline_supported = unsafe { cpuid_tsc_deadline_supported() };

    // Decide whether idle cores may HALT (run cool) rather than spin. A halted
    // core is safe only if its scheduler tick survives the C-state: TSC-Deadline
    // fires from the always-running TSC, and ARAT (CPUID.06H:EAX[2]) means the
    // LAPIC timer keeps ticking too. The T630 uses the LAPIC periodic timer, so
    // ARAT is its signal. Without either, halting would drop ticks (Goldmont
    // APIC power-gate) - keep the sti-only spin. Idempotent across cores.
    let arat = unsafe { cpuid_arat_supported() };
    // (The halt decision is deferred to AFTER the C-state limit is attempted below - it depends on
    // whether the APIC can be power-gated, which we only learn from limit_package_cstates.)

    // Calibrate TSC ticks/10ms ONLY where TSC-Deadline mode uses it (the AMD T630). The BSP measures via
    // the PIT (portable ground truth - CPUID 0x15/0x16 give a garbage frequency on AMD, which stored a
    // ~1000x-too-small quantum: a ~10 us deadline interrupt storm PLUS ctx.sleep stretched to seconds and
    // ping RTT inflated ~1000x); APs read the stored value. QEMU has no TSC-Deadline, so it stays periodic
    // with the quantum uncalibrated (0) and cycles_to_ticks uses its 1-tick fallback exactly as before -
    // deliberately NOT calibrated on QEMU: its tick clock runs ~10 Hz, so an accurate quantum would make
    // recv_timeout deadlines ~10x too long. The wrong value only ever existed in the TSC-Deadline path.
    // NOW CALIBRATED ON EVERY MACHINE, not only in TSC-Deadline mode. The paragraph above described
    // why it used to be skipped in periodic mode: the quantum there was an uncalibrated ~100 ms
    // (QEMU) / ~1 s (T630) while this value describes 10 ms, so an "accurate" figure made every
    // cycles_to_ticks conversion wrong. The periodic timer is now PIT-calibrated to a true ~10 ms
    // (see PERIODIC_TIMER_TICKS), so the two finally agree and calibrating always is the correct
    // thing rather than a hazard.
    //
    // This also repairs what silently depended on it. `KeyRepeat::new_calibrated` derives its
    // ~600 ms initial delay and ~50 ms typematic interval from `tsc_ticks_per_10ms()`; returning 0
    // collapsed both to zero, so a held key repeated on EVERY wake - which read as choppy while the
    // quantum was ~1 s, and would have become a ~100/s character spam once the quantum was fixed.
    let tsc_ticks = {
        let existing = TSC_TICKS_PER_QUANTUM.load(Ordering::Relaxed);
        // SAFETY: ring-0; interrupts are disabled during early per-core init.
        if existing > 0 { existing } else { unsafe { calibrate_tsc_ticks_per_10ms(lapic_id) } }
    };
    if tsc_ticks > 0 {
        TSC_TICKS_PER_QUANTUM.store(tsc_ticks, Ordering::Relaxed);
    }

    if tsc_deadline_supported && tsc_ticks > 0 {
        // TSC-Deadline mode: LVT bits[18:17] = 10b (mode 2), vector 0x20. No DIVIDE/INIT in this mode.
        // First deadline arm is deferred to scheduler::run() (after CORE_SCHED_CTX[cid].cr3 is seeded)
        // to avoid the timer firing with cr3=0 and triple-faulting silently.
        write_apic(apic_virt, APIC_LVT_TIMER, (1 << 18) | 0x20);
        // Store per-quantum tick count for re-arm in the timer ISR. Written once on the BSP before APs
        // start, read after - so Relaxed is fine.
        TSC_TICKS_PER_QUANTUM.store(tsc_ticks, Ordering::Relaxed);
        TSC_DEADLINE_MODE.store(true, Ordering::Relaxed);
        crate::kprintln!("apic: core {} TSC-Deadline timer ({} ticks/quantum)", lapic_id, tsc_ticks);
    } else {
        // Periodic mode, CALIBRATED against the PIT (the BSP measures once; APs reuse the stored
        // value, exactly like the TSC path). The period is init_count * divisor / f_apic and f_apic
        // is machine-dependent, so the previous hardcoded count could not be right everywhere: it
        // gave ~100 ms on QEMU (1 GHz APIC clock) but ~1 s on the T630 (~100 MHz) - 100x the intended
        // 10 ms quantum, which is what made `sleep` bottom out near a second, preemption land ~1 s
        // apart, and every timed wake equally coarse there. Falls back to the fixed count if the
        // measurement is unavailable or implausible, so a dead PIT degrades rather than misprograms.
        let measured = {
            let existing = PERIODIC_TIMER_TICKS.load(Ordering::Relaxed);
            if existing > 0 { existing } else { pit_calibrate_apic_ticks_per_10ms(apic_virt) }
        };
        let init = if measured > 0 {
            PERIODIC_TIMER_TICKS.store(measured, Ordering::Relaxed);
            measured as u32
        } else {
            PERIODIC_TIMER_COUNT
        };
        write_apic(apic_virt, APIC_LVT_TIMER, (1 << 17) | 0x20);
        write_apic(apic_virt, APIC_TIMER_DIVIDE, 0x03);
        write_apic(apic_virt, APIC_TIMER_INIT, init);
        crate::kprintln!(
            "apic: core {} periodic timer, init={} ({})",
            lapic_id,
            init,
            if measured > 0 { "PIT-calibrated ~10ms" } else { "uncalibrated fallback" }
        );
    }

    // Apply the package C-state limit (keeps the APIC powered on Intel), and THEN decide whether idle
    // cores may HALT. A halted core only wakes on interrupt delivery, so halting is safe ONLY if a wake
    // is guaranteed: either the C-state limit was applied so the APIC never power-gates, OR ARAT keeps
    // the periodic LAPIC timer ticking (meaningful only in periodic mode - not for TSC-Deadline). On the
    // Wyse 5070 (Goldmont+) the C-state MSR is BIOS-LOCKED so the limit can't be applied AND it runs
    // TSC-Deadline, so NEITHER holds - there we must NOT halt. Halting let the firmware power-gate the
    // APIC and DROP the wake IPI/deadline, freezing the core silently (the chaos max-carnage wedge that
    // no lock/shootdown watchdog caught, because nothing was spinning). The sti-only spin (correct, just
    // hotter) is selected instead.
    // SAFETY: ring-0; APIC initialised above.
    let cstate_ok = unsafe { limit_package_cstates(lapic_id) };
    super::interrupts::set_idle_can_halt(cstate_ok || (arat && !tsc_deadline_supported));
}

/// Check CPUID leaf 1, ECX bit 24 for TSC-Deadline timer support.
///
/// # Safety
/// Ring-0 only.
unsafe fn cpuid_tsc_deadline_supported() -> bool {
    // SAFETY: __cpuid(1) is universally safe on x86_64; CPUID.01H:ECX[24] =
    // APIC TSC-Deadline timer support (Intel SDM Vol. 2A).
    let result = core::arch::x86_64::__cpuid(1);
    (result.ecx >> 24) & 1 != 0
}

/// Check CPUID leaf 6, EAX bit 2 for ARAT (Always Running APIC Timer) - the
/// LAPIC timer keeps ticking through C-states, so an idle core may `hlt` and
/// still receive its scheduler tick.
///
/// # Safety
/// Ring-0 only.
unsafe fn cpuid_arat_supported() -> bool {
    // SAFETY: __cpuid(6) is safe on x86_64; CPUID.06H:EAX[2] = ARAT.
    let result = core::arch::x86_64::__cpuid(6);
    (result.eax >> 2) & 1 != 0
}

/// Determine TSC ticks per 10 ms quantum using CPUID leaves 0x15 and 0x16.
///
/// Returns 0 if the TSC frequency cannot be reliably determined (caller falls
/// back to periodic APIC timer mode).
///
/// Leaf 0x15 (TSC / Crystal Clock Ratio) on Goldmont+:
///   EAX = denominator, EBX = numerator, ECX = crystal_hz (19 200 000 on J5005)
///   tsc_hz = crystal_hz × EBX / EAX
///
/// Leaf 0x16 (Processor Frequency) fallback:
///   EAX[15:0] = Core Base Frequency in MHz
///   tsc_hz ≈ base_mhz × 1 000 000
///
/// # Safety
/// Ring-0 only.  Called once during `init_local_apic`.
unsafe fn compute_tsc_ticks_per_10ms(lapic_id: u32) -> u64 {
    // SAFETY: __cpuid_count is safe on x86_64; leaf 0x15, sub-leaf 0.
    let r15 = core::arch::x86_64::__cpuid_count(0x15, 0);
    if r15.ecx != 0 && r15.ebx != 0 && r15.eax != 0 {
        // tsc_hz = crystal_hz × numerator / denominator
        let tsc_hz = (r15.ecx as u64) * (r15.ebx as u64) / (r15.eax as u64);
        let ticks  = tsc_hz / 100; // 10 ms = tsc_hz / 100
        crate::kprintln!(
            "apic: core {} CPUID.0x15 crystal={}Hz ratio={}/{} tsc_hz={} ticks/10ms={}",
            lapic_id, r15.ecx, r15.ebx, r15.eax, tsc_hz, ticks
        );
        return ticks;
    }

    // Fallback: CPUID.0x16 Base Frequency (MHz).
    // SAFETY: __cpuid(0) returns max_leaf; always available.
    let max_leaf = core::arch::x86_64::__cpuid(0).eax;
    if max_leaf >= 0x16 {
        // SAFETY: leaf 0x16 exists per max_leaf check above.
        let r16 = core::arch::x86_64::__cpuid(0x16);
        let base_mhz = r16.eax & 0xFFFF;
        if base_mhz > 0 {
            let tsc_hz = base_mhz as u64 * 1_000_000;
            let ticks  = tsc_hz / 100;
            crate::kprintln!(
                "apic: core {} CPUID.0x16 base={}MHz tsc_hz={} ticks/10ms={}",
                lapic_id, base_mhz, tsc_hz, ticks
            );
            return ticks;
        }
    }

    crate::kprintln!("apic: core {} TSC frequency unknown (CPUID.0x15 and 0x16 both zero)", lapic_id);
    0
}

/// Calibrate TSC ticks per 10 ms. The PIT is the portable ground truth and is tried first; CPUID's
/// Intel-only leaves are a fallback. Each candidate is sanity-bounded (a real CPU is 100 MHz .. 10 GHz,
/// i.e. 1e6 .. 1e8 ticks per 10 ms), so a garbage value (the AMD T630's CPUID reports ~1000x too small)
/// is rejected rather than trusted. Returns 0 if neither is plausible - the caller then runs periodic
/// mode and `cycles_to_ticks` uses its 1-tick fallback.
///
/// # Safety
/// Ring-0 only. Interrupts must be disabled (early per-core init). Touches PIT ports 0x42/0x43/0x61.
unsafe fn calibrate_tsc_ticks_per_10ms(lapic_id: u32) -> u64 {
    const SANE_LO: u64 = 1_000_000;    // 100 MHz
    const SANE_HI: u64 = 100_000_000;  // 10 GHz

    let pit = pit_calibrate_tsc_ticks_per_10ms();
    if (SANE_LO..=SANE_HI).contains(&pit) {
        crate::kprintln!(
            "apic: core {} PIT-calibrated tsc_hz={} ticks/10ms={}",
            lapic_id, pit.saturating_mul(100), pit
        );
        return pit;
    }

    // PIT implausible or absent: fall back to CPUID's Intel leaves (correct on Intel).
    let cpuid = compute_tsc_ticks_per_10ms(lapic_id);
    if (SANE_LO..=SANE_HI).contains(&cpuid) {
        return cpuid;
    }

    crate::kprintln!(
        "apic: core {} TSC calibration failed (pit={} cpuid={}); periodic 1-tick fallback",
        lapic_id, pit, cpuid
    );
    0
}

/// Measure TSC ticks per 10 ms by gating PIT channel 2 for a known 50 ms interval and counting TSC
/// cycles across it. Portable - correct on AMD (which exposes no usable CPUID TSC frequency) and Intel
/// alike. Returns 0 if the PIT never reaches terminal count, so the caller can fall back.
///
/// # Safety
/// Ring-0 only. Interrupts disabled. Uses PIT channel 2 (data 0x42, command 0x43) and control port 0x61;
/// channel 0 (the legacy tick) is untouched. Saves and restores 0x61.
/// Measure the LAPIC timer against the PIT and return the periodic initial count for one ~10 ms
/// quantum, or 0 if the measurement is unavailable or implausible (the caller then keeps the
/// `PERIODIC_TIMER_COUNT` fallback).
///
/// The period is `init_count * divisor / f_apic`, and `f_apic` is machine-dependent, so a hardcoded
/// count cannot be right everywhere - see `PERIODIC_TIMER_TICKS`. The PIT is the portable ground
/// truth here exactly as it is for the TSC.
///
/// The timer LVT is **masked** for the measurement so calibration cannot deliver an interrupt, and
/// one-shot mode is used so the counter cannot wrap: 0xFFFF_FFFF ticks is far more than a 50 ms
/// window costs even at a 2 GHz APIC clock (~6.25M ticks after the /16 divider).
///
/// # Safety
/// Ring-0 only; the local APIC must be mapped at `apic_virt`. Touches PIT ports 0x42/0x43/0x61 and
/// the APIC timer registers, and leaves the timer masked and stopped - the caller reprograms
/// LVT/DIVIDE/INIT afterwards.
unsafe fn pit_calibrate_apic_ticks_per_10ms(apic_virt: u64) -> u64 {
    const PIT_HZ: u64 = 1_193_182;
    const CAL_MS: u64 = 50;
    let count: u16 = ((PIT_HZ * CAL_MS) / 1000) as u16;

    // Mask the timer (LVT bit 16) so no interrupt fires mid-calibration; divide by 16 to match the
    // divider the periodic mode actually runs with, so the measured rate is directly usable.
    write_apic(apic_virt, APIC_LVT_TIMER, (1 << 16) | 0x20);
    write_apic(apic_virt, APIC_TIMER_DIVIDE, 0x03);

    let saved_61 = inb(0x61);
    outb(0x61, (saved_61 & 0xFC) | 0x01);
    outb(0x43, 0b1011_0000);
    outb(0x42, (count & 0xFF) as u8);
    // Start the APIC countdown immediately before the PIT's high byte starts its countdown, so the
    // two windows line up as closely as the port writes allow.
    write_apic(apic_virt, APIC_TIMER_INIT, 0xFFFF_FFFF);
    outb(0x42, (count >> 8) as u8);

    let t0 = core::arch::x86_64::_rdtsc();
    loop {
        if inb(0x61) & 0x20 != 0 { break; }
        // Same stuck-PIT guard as the TSC calibration: never hang boot on dead hardware.
        if core::arch::x86_64::_rdtsc().wrapping_sub(t0) > 100_000_000_000 {
            outb(0x61, saved_61);
            return 0;
        }
    }
    let current = read_apic(apic_virt, APIC_TIMER_CURRENT) as u64;
    outb(0x61, saved_61);

    let elapsed = 0xFFFF_FFFFu64.saturating_sub(current);
    let per_10ms = elapsed / (CAL_MS / 10);   // 50 ms window -> one 10 ms quantum

    // Plausibility gate. A real APIC clock is roughly 25 MHz .. 2 GHz, so a 10 ms quantum lands
    // around 15k .. 1.25M ticks after the /16 divider. Reject anything outside a generous band
    // rather than program a wild period: a count that is far too SMALL would make the timer fire
    // faster than the ISR can complete, which is the cascade this file's periodic comment records.
    if per_10ms < 10_000 || per_10ms > 4_000_000 {
        return 0;
    }
    per_10ms
}

unsafe fn pit_calibrate_tsc_ticks_per_10ms() -> u64 {
    const PIT_HZ: u64 = 1_193_182;      // i8254 input clock
    const CAL_MS: u64 = 50;             // 50 ms window = 59_659 counts, well under the 16-bit max
    let count: u16 = ((PIT_HZ * CAL_MS) / 1000) as u16;

    // Channel 2: speaker off (bit1=0), gate on (bit0=1). Save 0x61 to restore afterwards.
    let saved_61 = inb(0x61);
    outb(0x61, (saved_61 & 0xFC) | 0x01);

    // Channel 2, access lo+hi byte, mode 0 (interrupt on terminal count), binary.
    outb(0x43, 0b1011_0000);
    outb(0x42, (count & 0xFF) as u8);
    outb(0x42, (count >> 8) as u8);      // writing the high byte starts the countdown (gate is high)

    let t0 = core::arch::x86_64::_rdtsc();
    // Wait for channel-2 output (0x61 bit5) to go high at terminal count. Bail on a stuck PIT via a TSC
    // guard ~1000x the expected window, so boot never hangs - the caller then uses CPUID.
    loop {
        if inb(0x61) & 0x20 != 0 { break; }
        if core::arch::x86_64::_rdtsc().wrapping_sub(t0) > 100_000_000_000 {
            outb(0x61, saved_61);
            return 0;
        }
    }
    let t1 = core::arch::x86_64::_rdtsc();
    outb(0x61, saved_61);                // restore the control port

    let elapsed = t1.wrapping_sub(t0);   // TSC cycles across CAL_MS of PIT time
    // tsc_hz = elapsed * PIT_HZ / count ; ticks per 10 ms = tsc_hz / 100
    let tsc_hz = elapsed.saturating_mul(PIT_HZ) / (count as u64);
    tsc_hz / 100
}

/// Write the TSC-Deadline MSR to fire `ticks` TSC counts from now.
///
/// # Safety
/// Ring-0 only.  TSC-Deadline must be supported (CPUID.1.ECX[24]=1).
#[inline]
unsafe fn arm_tsc_deadline_now(ticks: u64) {
    // SAFETY: _rdtsc() reads the processor TSC; always available on x86_64 ring-0.
    let now      = unsafe { core::arch::x86_64::_rdtsc() };
    let deadline = now.wrapping_add(ticks);
    // Intel SDM Vol. 3A §10.5.4.2: MFENCE is required before every WRMSR to
    // IA32_TSC_DEADLINE.  Without it, Goldmont+'s out-of-order store buffers
    // can commit the WRMSR before a preceding LVT write is visible to APIC
    // hardware, causing the ISR to fire with stale LVT state.
    // SAFETY: MFENCE is a full memory barrier; always valid in ring-0.
    unsafe { core::arch::asm!("mfence", options(nostack)) };
    // SAFETY: MSR_IA32_TSC_DEADLINE (0x6E0) is writable in ring-0 when
    // TSC-Deadline is supported; writing 0 disarms, non-zero arms one-shot.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") MSR_IA32_TSC_DEADLINE,
            in("eax") deadline as u32,
            in("edx") (deadline >> 32) as u32,
            options(nostack, nomem),
        );
    }
}

/// Re-arm the TSC-Deadline timer for the next ~10 ms quantum.
///
/// Called from the timer ISR (`timer_tick_from_irq`) immediately after EOI
/// when `TSC_DEADLINE_MODE` is true.  TSC-Deadline is one-shot; the kernel
/// must explicitly reload it after every tick.
///
/// # Safety
/// Must be called from interrupt context (IF=0) in ring-0.
/// Only valid when `TSC_DEADLINE_MODE` is `true`.
#[inline]
pub unsafe fn rearm_tsc_deadline() {
    let ticks = TSC_TICKS_PER_QUANTUM.load(Ordering::Relaxed);
    // SAFETY: delegated to arm_tsc_deadline_now - same preconditions.
    unsafe { arm_tsc_deadline_now(ticks) };
}

/// Quantum multiplier for an IDLE core's timer in **TSC-Deadline** mode (Phase 2a,
/// `docs/power.md` §14). MUST stay well under the liveness watchdog threshold (300 quanta, ~3 s) -
/// the slow tick is what keeps stamping `CORE_LAST_TICK_TSC`, so an idle core still reads as alive
/// and the watchdog needs no change.
pub const IDLE_QUANTUM_MULT: u64 = 100;

/// LAPIC periodic-timer initial count: the normal preemption period. ~50 ms on the AMD GX-420GI
/// (T630), ~100 ms at 1 GHz APIC bus / 16 divider (QEMU). The APIC bus frequency is not calibrated,
/// so the absolute period is machine-dependent - only the ratio below is under our control.
const PERIODIC_TIMER_COUNT: u32 = 6_250_000;

/// Multiplier for an IDLE core's **periodic** timer. Deliberately smaller than `IDLE_QUANTUM_MULT`
/// because the periodic period is already far longer than a 10 ms quantum (~50 ms on the T630), so
/// 20x lands the idle tick near ~1 s there (~2 s on QEMU) instead of an unhelpfully long ~5 s. The
/// liveness watchdog is inactive in periodic mode (it is gated on a calibrated
/// `TSC_TICKS_PER_QUANTUM`), so the binding constraint here is lost-wake recovery latency rather
/// than the wedge threshold.
const IDLE_PERIODIC_MULT: u32 = 20;

/// The periodic-timer initial count actually in use: the PIT-calibrated ~10 ms quantum, or the
/// fixed fallback if calibration was unavailable. Everything that reprograms the periodic timer
/// reads this, so the idle/restore path can never disagree with what boot programmed.
/// Initial count for a SLOWED (idle) periodic timer.
///
/// When the timer is PIT-calibrated the base really is ~10 ms, so 100 quanta lands the idle tick at
/// ~1 s - the same target the TSC-Deadline path uses, and the value `docs/power.md` §14 designed
/// for. Before calibration existed this used a small multiplier only because the base period was
/// unknown and could already be ~1 s on some machines; that guard is kept for the uncalibrated
/// fallback, where stretching by 100x could otherwise mean a minutes-long idle tick.
fn idle_timer_count() -> u32 {
    let calibrated = PERIODIC_TIMER_TICKS.load(Ordering::Relaxed);
    if calibrated > 0 {
        (calibrated as u32).saturating_mul(IDLE_QUANTUM_MULT as u32)
    } else {
        PERIODIC_TIMER_COUNT.saturating_mul(IDLE_PERIODIC_MULT)
    }
}

fn periodic_timer_count() -> u32 {
    let t = PERIODIC_TIMER_TICKS.load(Ordering::Relaxed);
    if t > 0 { t as u32 } else { PERIODIC_TIMER_COUNT }
}

/// Re-arm this core's timer for the long IDLE interval (~1 s) instead of the normal preemption
/// period (Phase 2a). Handles **both** timer modes:
///  - **TSC-Deadline** (one-shot, software re-armed each tick): arm the next deadline
///    `IDLE_QUANTUM_MULT` quanta out.
///  - **Periodic** (hardware auto-reload): reprogram the LAPIC initial count, which restarts the
///    countdown with the longer period; the hardware then keeps reloading it until restored.
///
/// Only ever called for a core that can `hlt` (enforced by the scheduler's idle path). That gate
/// matters in periodic mode: a core that spins instead of halting would rewrite the initial count
/// on every loop iteration, restarting the countdown forever so the timer never fires.
///
/// Safe per §18.5: writing a timer register is not memory-unsafe, and the only precondition is
/// ordering (call from the steady-state scheduler loop) - a documented contract, not an unsafe one.
pub fn rearm_idle_timer() {
    if TSC_DEADLINE_MODE.load(Ordering::Relaxed) {
        let ticks = TSC_TICKS_PER_QUANTUM
            .load(Ordering::Relaxed)
            .saturating_mul(IDLE_QUANTUM_MULT);
        // SAFETY: ring-0; TSC_DEADLINE_MODE=true implies the CPUID check passed and
        // TSC_TICKS_PER_QUANTUM was set (both together in init_local_apic).
        unsafe { arm_tsc_deadline_now(ticks) };
    } else {
        // SAFETY: ring-0; APIC_VIRT_BASE is valid after init_local_apic (the same pattern
        // apic_send_eoi uses), and this writes THIS core's own LAPIC - no cross-core race.
        unsafe {
            write_apic(
                APIC_VIRT_BASE,
                APIC_TIMER_INIT,
                idle_timer_count(),
            )
        };
    }
}

/// Restore this core's timer to the normal preemption period after an idle wake (Phase 2a), so a
/// task scheduled off that wake is preemptible on schedule rather than running until the idle
/// deadline. Mirrors `rearm_idle_timer` across both timer modes.
pub fn rearm_quantum_timer() {
    if TSC_DEADLINE_MODE.load(Ordering::Relaxed) {
        // SAFETY: as `rearm_idle_timer` - ring-0, TSC-Deadline confirmed active.
        unsafe { rearm_tsc_deadline() };
    } else {
        // SAFETY: as `rearm_idle_timer` - ring-0, this core's own LAPIC initial count.
        unsafe { write_apic(APIC_VIRT_BASE, APIC_TIMER_INIT, periodic_timer_count()) };
    }
}

/// TSC cycles per scheduler quantum (the timer period), or 0 before the local APIC timer is
/// calibrated. Used to convert a cycle-based `recv_timeout` into a count of timer ticks for the
/// core-independent timed-wake clock (§12) - a TSC deadline can't be compared across cores
/// whose TSCs need not be synchronised, so the timed-wake counts ticks of the BSP timer instead.
#[inline]
pub fn tsc_ticks_per_quantum() -> u64 {
    TSC_TICKS_PER_QUANTUM.load(Ordering::Relaxed)
}

/// Returns true when running on a GenuineIntel CPU.
///
/// Used to gate Intel-specific MSR accesses (e.g. MSR 0xE2) that do not exist
/// on AMD and would cause #GP(0) if accessed there.
fn is_intel_cpu() -> bool {
    // CPUID leaf 0: EBX/ECX/EDX encode the 12-byte vendor string.
    // "GenuineIntel" → EBX=0x756e6547 EDX=0x49656e69 ECX=0x6c65746e
    // SAFETY: __cpuid(0) is universally safe on x86_64.
    let r = core::arch::x86_64::__cpuid(0);
    r.ebx == 0x756e_6547 && r.edx == 0x4965_6e69 && r.ecx == 0x6c65_746e
}

/// Limit package C-states to PC1 to prevent APIC power-gate on Goldmont+.
///
/// On Intel Atom / Goldmont+ (Gemini Lake, Wyse 5070 J5005), the firmware
/// autonomously promotes the SoC package to PC6+, which power-gates the local
/// APIC - silencing both the periodic APIC timer and cross-core IPIs even when
/// the cores are actively executing code (no PAUSE/HLT required to trigger it).
///
/// MSR_PKG_CST_CONFIG_CONTROL (0xE2) bits:
///   [2:0] package C-state limit  (0=PC0, 1=PC1, 2=PC2, …; higher = deeper)
///   [15]  CFG_LOCK - if set, MSR is read-only (WRMSR → #GP)
///
/// Writes bits[2:0]=1 (PC1 limit) if the MSR is not locked.  PC1 keeps the
/// APIC powered; PC2+ may not.  If the MSR is locked we cannot help via this
/// path and must fall back to TSC-Deadline timer mode (see TODO).
///
/// # Safety
/// Ring-0 only.  Called once per core from `init_local_apic` after APIC setup.
/// Returns whether a halted core's wake is safe from the Goldmont+ APIC power-gate: `true` if the APIC
/// will not be power-gated (AMD has no such gate, or the Intel C-state limit was applied), `false` if it
/// could not be applied (Intel BIOS-locked the MSR) - in which case idle cores must NOT halt (see
/// `init_apic_timer`), or a power-gated APIC drops the wake and freezes the core.
unsafe fn limit_package_cstates(core_id: u32) -> bool {
    // MSR 0xE2 (MSR_PKG_CST_CONFIG_CONTROL) is Intel-specific.
    // On AMD processors this MSR does not exist; RDMSR/WRMSR cause #GP(0). AMD has no Goldmont+ APIC
    // power-gate, so halting there is governed by ARAT elsewhere - report "no gate concern".
    if !is_intel_cpu() {
        return true;
    }

    const MSR_PKG_CST_CONFIG_CONTROL: u32 = 0xE2;
    const CFG_LOCK: u64 = 1 << 15;
    const PC1_LIMIT: u64 = 1; // bits[2:0] = 001 → max package C-state = PC1

    // SAFETY: RDMSR in ring-0; Intel vendor confirmed above; MSR 0xE2 exists
    // on all Intel Goldmont+ platforms.
    let (lo, hi): (u32, u32);
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx")  MSR_PKG_CST_CONFIG_CONTROL,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem),
        );
    }
    let current = ((hi as u64) << 32) | (lo as u64);
    crate::kprintln!("cstate: core {} MSR 0xE2 = {:#018x} (lock={})",
        core_id, current, (current >> 15) & 1);

    if current & CFG_LOCK != 0 {
        // BIOS locked the MSR - cannot write; APIC timer may still be gated in
        // deep package C-states.  A TSC-Deadline timer does not require this MSR.
        crate::kprintln!("cstate: core {} MSR 0xE2 locked - C-state limit cannot be set via MSR", core_id);
        return false;
    }

    let new_val = (current & !0x7u64) | PC1_LIMIT;
    // SAFETY: MSR is not locked (checked above); WRMSR in ring-0 is valid.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx")  MSR_PKG_CST_CONFIG_CONTROL,
            in("eax")  new_val as u32,
            in("edx")  (new_val >> 32) as u32,
            options(nostack, nomem),
        );
    }
    crate::kprintln!("cstate: core {} limited to PC1 (MSR 0xE2 = {:#018x})", core_id, new_val);
    true
}

/// Send an End-Of-Interrupt signal to the local APIC.
///
/// # Safety
/// Must be called from interrupt context (after `init_local_apic`).
pub unsafe fn apic_send_eoi() {
    // SAFETY: APIC_VIRT_BASE is valid after init_local_apic; write 0 to EOI reg.
    unsafe { write_apic(APIC_VIRT_BASE, APIC_EOI, 0) };
}

/// Return the virtual base address of this core's local APIC.
///
/// # Safety
/// Valid only after `init_local_apic` has been called on this core.
pub unsafe fn get_apic_virt_base() -> u64 {
    // SAFETY: APIC_VIRT_BASE is set in init_local_apic before any use.
    unsafe { APIC_VIRT_BASE }
}

/// Read the local APIC ID register and return the ID (bits 31:24).
///
/// # Safety
/// Valid only after `init_local_apic` has been called on this core.
pub unsafe fn get_lapic_id() -> u32 {
    // SAFETY: APIC_VIRT_BASE is set in init_local_apic; read_volatile is safe for MMIO.
    unsafe {
        let val = ((APIC_VIRT_BASE + APIC_ID) as *const u32).read_volatile();
        (val >> 24) & 0xFF
    }
}

// ICR (Interrupt Command Register) offsets - the local APIC's cross-core IPI-send registers.
const APIC_ICR_LOW:  u64 = 0x300;
const APIC_ICR_HIGH: u64 = 0x310;

/// Poll the ICR Delivery-Status bit (ICR_LOW bit 12) until clear, bounded. Writing a new IPI while
/// DELIVS=1 silently drops it on some xAPICs (observed on Goldmont+ under concurrent IPI load); the
/// cap keeps a wedged APIC from spinning forever (SDM 10.6.1).
///
/// # Safety
/// APIC must be mapped (after `init_local_apic`).
#[inline]
unsafe fn apic_wait_icr_idle() {
    let mut tries = 0u32;
    // SAFETY: APIC_VIRT_BASE valid; volatile MMIO read of ICR_LOW.
    while unsafe { ((APIC_VIRT_BASE + APIC_ICR_LOW) as *const u32).read_volatile() >> 12 } & 1 != 0 {
        core::hint::spin_loop();
        tries += 1;
        if tries >= 10_000 { break; }
    }
}

/// Send a fixed-delivery IPI to the core with local-APIC id `lapic_id`, raising `vector` on it. The arch
/// seam for a TARGETED cross-core IPI (AArch64 maps this to a GIC SGI, RISC-V to a CLINT MSIP write).
///
/// # Safety
/// The local APIC must be mapped (after `init_local_apic`); `lapic_id` must be a valid target core.
#[inline]
pub unsafe fn send_ipi_to_lapic(lapic_id: u32, vector: u8) {
    // xAPIC ICR write protocol: high word (destination) first, then low word (vector + assert, bit 14),
    // which triggers delivery. SAFETY: APIC mapped; DELIVS polled before the write per SDM 10.6.1.
    unsafe {
        write_apic(APIC_VIRT_BASE, APIC_ICR_HIGH, (lapic_id & 0xFF) << 24);
        apic_wait_icr_idle();
        write_apic(APIC_VIRT_BASE, APIC_ICR_LOW, (vector as u32) | (1 << 14));
    }
}

/// Broadcast `vector` as an IPI to all cores EXCEPT this one (xAPIC all-excluding-self shorthand). The
/// arch seam for a BROADCAST IPI (AArch64: GIC SGI with the broadcast target-list filter).
///
/// # Safety
/// The APIC must be mapped; the caller holds IF=0 (or has saved/disabled it).
#[inline]
pub unsafe fn broadcast_ipi_all_but_self(vector: u8) {
    // SAFETY: APIC mapped; IF=0. Shorthand 0b11 (all-excluding-self, ICR_LOW bits 19:18), fixed
    // delivery, edge, assert (bit 14); DELIVS polled first (SDM 10.6.1).
    unsafe {
        write_apic(APIC_VIRT_BASE, APIC_ICR_HIGH, 0);
        apic_wait_icr_idle();
        write_apic(APIC_VIRT_BASE, APIC_ICR_LOW, (vector as u32) | (1 << 14) | (0b11 << 18));
    }
}

/// Broadcast an NMI to all cores EXCEPT this one. Unlike `broadcast_ipi_all_but_self` (a maskable
/// fixed-delivery vector), an NMI reaches a core even when it is running with interrupts disabled -
/// e.g. spinning on a lock. The panic path (`halt_all_cores`, SEC-18) uses this so a panic on one core
/// actually stops the machine (§6.2, §19); `idt[2]` routes the NMI to `exception_halt`.
///
/// # Safety
/// The APIC must be mapped; the caller holds IF=0 (or has saved/disabled it).
#[inline]
pub unsafe fn broadcast_nmi_all_but_self() {
    // NMI delivery mode (ICR_LOW bits 10:8 = 0b100), all-excluding-self shorthand (bits 19:18 = 0b11),
    // edge, assert (bit 14). The vector field is ignored for NMI delivery. SAFETY: APIC mapped; IF=0;
    // DELIVS polled before the write per SDM 10.6.1.
    unsafe {
        write_apic(APIC_VIRT_BASE, APIC_ICR_HIGH, 0);
        apic_wait_icr_idle();
        write_apic(APIC_VIRT_BASE, APIC_ICR_LOW, (0b100 << 8) | (1 << 14) | (0b11 << 18));
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Lock-free serial helpers - used in fault handlers where LOG_LOCK may
// already be held (nested kprintln → deadlock with IF=0).
// ---------------------------------------------------------------------------

/// Iteration cap for the lock-free fault-path THRE poll. An absent or wedged COM1
/// (transmit-holding-register-empty never asserts) must not hang a fault handler
/// forever - a silent kernel freeze, which invariant 12 forbids. After the cap we
/// proceed best-effort exactly like the bounded `serial_thre_wait`; the worst case
/// is one possibly-dropped diagnostic byte, never a wedge. A live UART empties its
/// holding register in microseconds, so the cap is never reached in practice.
const SERIAL_THRE_NOLCK_CAP: u32 = 1_000_000;

#[inline]
unsafe fn serial_poll_thre() {
    // SAFETY: port I/O in ring-0; 0x3FD is COM1 LSR.
    unsafe {
        let mut spins: u32 = 0;
        loop {
            let lsr: u8;
            core::arch::asm!(
                "in al, dx",
                out("al") lsr,
                in("dx") 0x3FDu16,
                options(nostack, nomem),
            );
            if lsr & 0x20 != 0 { break; }
            spins += 1;
            if spins >= SERIAL_THRE_NOLCK_CAP { break; }
            core::hint::spin_loop();
        }
    }
}

#[inline]
unsafe fn serial_putc_nolck(c: u8) {
    // SAFETY: called from panic/fault path; interrupts disabled; serial port exclusively owned by kernel.
    unsafe {
        serial_poll_thre();
        outb(0x3F8, c);
    }
}

unsafe fn serial_puts_nolck(s: &[u8]) {
    for &c in s {
        // SAFETY: called within an unsafe fn; caller has guaranteed serial port exclusive ownership.
        unsafe { serial_putc_nolck(c) };
    }
}

unsafe fn serial_hex64_nolck(val: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = ((val >> ((15 - i) * 4)) & 0xF) as u8;
        buf[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
    // SAFETY: called within an unsafe fn; caller has guaranteed serial port exclusive ownership.
    unsafe { serial_puts_nolck(&buf) };
}

/// Write a byte to an x86 I/O port.
#[inline]
unsafe fn outb(port: u16, val: u8) {
    // SAFETY: caller selects ports that are safe to write (PIC, diagnostic).
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") val,
            options(nostack, nomem),
        );
        // Short I/O delay via the diagnostic port so old hardware settles.
        core::arch::asm!(
            "out 0x80, al",
            in("al") 0u8,
            options(nostack, nomem),
        );
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
/// Ring-0 only. Caller selects ports that are safe to read (PIT status, diagnostic).
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: `in al, dx` reads the selected port; no memory touched.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") val,
            in("dx") port,
            options(nostack, nomem),
        );
    }
    val
}

/// Remap the legacy 8259 PIC to vectors 0x20-0x2F then mask all IRQs.
///
/// Without remapping, the PIC's IRQ0 (system timer) fires at vector 8, which
/// is the double-fault entry.  We use the APIC for all timing so the 8259
/// must be silenced before we enable interrupts.
unsafe fn mask_pic() {
    // SAFETY: ICW/OCW writes to 0x20/0xA0 (command) and 0x21/0xA1 (data)
    //         are the standard 8259 programming sequence; no side effects on
    //         non-existent PIC (virtual QEMU environment).
    unsafe {
        outb(0x20, 0x11);     // ICW1: init master PIC with ICW4
        outb(0xA0, 0x11);     // ICW1: init slave  PIC with ICW4
        outb(0x21, 0x20);     // ICW2: master IRQ0-7 → vectors 32-39
        outb(0xA1, 0x28);     // ICW2: slave  IRQ8-15 → vectors 40-47
        outb(0x21, 0x04);     // ICW3: master has slave on IRQ2
        outb(0xA1, 0x02);     // ICW3: slave cascade identity = 2
        outb(0x21, 0x01);     // ICW4: 8086 mode
        outb(0xA1, 0x01);     // ICW4: 8086 mode
        outb(0x21, 0xFF);     // OCW1: mask all master IRQs
        outb(0xA1, 0xFF);     // OCW1: mask all slave  IRQs
    }
}

#[inline]
/// Read a local-APIC register.
///
/// # Safety
/// Ring-0 only; `base + reg` must be the mapped APIC MMIO address.
#[inline]
unsafe fn read_apic(base: u64, reg: u64) -> u32 {
    // SAFETY: base + reg is the APIC MMIO address; volatile read is required.
    unsafe { ((base + reg) as *const u32).read_volatile() }
}

unsafe fn write_apic(base: u64, reg: u64, val: u32) {
    // SAFETY: base + reg is the APIC MMIO address; volatile write is required.
    unsafe { ((base + reg) as *mut u32).write_volatile(val) };
}


/// Build a 16-byte TSS descriptor (two 8-byte GDT slots) for `tss_ptr`.
///
/// Returns `(low_qword, high_qword)` to be stored at GDT[6..=7].
fn make_tss_descriptor(tss_ptr: *const Tss) -> (u64, u64) {
    let base  = tss_ptr as u64;
    let limit = (core::mem::size_of::<Tss>() - 1) as u64; // 103 = 0x67

    // Low qword bit fields (Intel manual vol.3 §3.4.5, §7.2.3):
    //   [15:0]   limit[15:0]
    //   [31:16]  base[15:0]
    //   [39:32]  base[23:16]
    //   [47:40]  type/dpl/present: 0x89 = P=1, DPL=0, S=0, type=9 (64-bit avail TSS)
    //   [51:48]  limit[19:16]       (0 for a 104-byte TSS)
    //   [55:52]  flags              (G=0, D=0, L=0, AVL=0)
    //   [63:56]  base[31:24]
    let lo: u64 = (limit & 0xFFFF)
        | ((base & 0xFFFF) << 16)
        | (((base >> 16) & 0xFF) << 32)
        | (0x89u64 << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);

    // High qword: base[63:32] in the low 32 bits; upper 32 bits reserved = 0.
    let hi: u64 = base >> 32;

    (lo, hi)
}

/// Load the per-core GDT (with TSS descriptor), reload segment registers,
/// and install the TSS via `ltr`.
///
/// # Safety
/// Called once per core (BSP from `init_bsp`, APs from `ap_init`) with a
/// valid stack. Invalidates the current CS/DS/ES/SS until they are reloaded.
pub(super) unsafe fn init_gdt(core_id: u32) {
    let cid = core_id as usize;
    // BSP bootstrap for core 0 (the arena does not exist yet), this AP's arena slot otherwise.
    let gdt = gdt_for(cid); // *mut [u64; 8]
    let tss = tss_for(cid); // *mut Tss

    // Fill the TSS descriptor into slots 6 and 7 of this core's GDT.
    // SAFETY: single writer per core during init; gdt/tss point to this core's own GDT/TSS.
    unsafe {
        let (lo, hi) = make_tss_descriptor(tss as *const Tss);
        (*gdt)[6] = lo;
        (*gdt)[7] = hi;
    }

    let desc = TableDescriptor {
        limit: (core::mem::size_of::<[u64; 8]>() - 1) as u16,
        base:  gdt as u64,
    };

    // SAFETY: gdt points to valid GDT memory; desc outlives the lgdt.
    unsafe {
        core::arch::asm!(
            "lgdt [{desc}]",
            desc = in(reg) &desc as *const TableDescriptor as u64,
            options(nostack, readonly)
        );

        // Load TSS: selector 0x30 = index 6 * 8 (GDT, RPL=0).
        // ltr marks the TSS descriptor as "busy" in the per-core GDT.
        core::arch::asm!(
            "ltr ax",
            in("ax") 0x30u16,
            options(nostack, nomem),
        );

        core::arch::asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            in("ax") 0x10u16,
            options(nostack)
        );
        // Reload CS via far return: push [new CS selector, next RIP]; retfq pops
        // RIP then CS - the only way to change CS in 64-bit mode.
        core::arch::asm!(
            "push {sel}",
            "lea {tmp}, [rip + 99f]",
            "push {tmp}",
            "retfq",
            "99:",
            sel = in(reg)  0x08u64,
            tmp = lateout(reg) _,
            options(nostack)
        );
    }
}

/// Enable SYSCALL/SYSRETQ and configure the SYSCALL entry MSRs for this core.
///
/// MSRs written:
///   EFER.SCE   - enables SYSCALL/SYSRETQ instructions.
///   STAR        - kernel CS (0x08) and SYSRETQ user-segment base (0x18).
///   LSTAR       - address of `syscall_entry` (our SYSCALL handler).
///   SFMASK      - RFLAGS bits to clear on SYSCALL entry (clears IF = bit 9).
///
/// Also writes `IA32_KERNEL_GS_BASE` via `init_per_core_syscall` so the entry
/// stub can access per-core data via `swapgs`.
///
/// # Safety
/// Called once per core after `init_gdt` (requires GDT and segment registers
/// to be valid so that SYSCALL/SYSRETQ can use the configured selectors).
pub(super) unsafe fn init_syscall(core_id: u32) {
    // STAR: bits [47:32] = kernel CS on SYSCALL (0x08) → SS = 0x08+8 = 0x10.
    //       bits [63:48] = SYSRETQ base (0x18) → user CS = 0x18+16 = 0x28,
    //                                             user SS = 0x18+8  = 0x20.
    const STAR: u64 = (0x0018u64 << 48) | (0x0008u64 << 32);

    // SFMASK: clear IF (bit 9) on SYSCALL so the stub always runs with IF=0.
    const SFMASK: u64 = 1 << 9;

    // SAFETY: all WRMSR/RDMSR in ring-0 are always valid on x86_64.
    unsafe {
        // EFER bit 0 (SCE) enables SYSCALL/SYSRETQ. Bit 11 (NXE) is the W^X FOUNDATION: without it the
        // NO_EXEC bit (63) in every PTE is IGNORED, so the whole W^X scheme silently does not hold. The
        // kernel SETS it explicitly here rather than trusting the bootloader - a security invariant must
        // not depend on the boot environment. (Limine does set it today; asserted at the readback below.)
        let (efer_lo, efer_hi): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx")  0xC000_0080u32,
            out("eax") efer_lo,
            out("edx") efer_hi,
            options(nostack, nomem),
        );
        let efer = ((efer_hi as u64) << 32) | (efer_lo as u64) | 1u64 | (1u64 << 11);
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0080u32,
            in("eax") efer as u32,
            in("edx") (efer >> 32) as u32,
            options(nostack, nomem),
        );

        // STAR MSR.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0081u32,
            in("eax") STAR as u32,
            in("edx") (STAR >> 32) as u32,
            options(nostack, nomem),
        );

        // LSTAR MSR - address of the SYSCALL entry point.
        let lstar = super::syscall_entry::syscall_entry as *const () as u64;
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0082u32,
            in("eax") lstar as u32,
            in("edx") (lstar >> 32) as u32,
            options(nostack, nomem),
        );

        // LSTAR readback - confirm the WRMSR took effect (AMD quirk guard).
        let (lstar_rd_lo, lstar_rd_hi): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx")  0xC000_0082u32,
            out("eax") lstar_rd_lo,
            out("edx") lstar_rd_hi,
            options(nostack, nomem),
        );
        let lstar_rd = ((lstar_rd_hi as u64) << 32) | (lstar_rd_lo as u64);
        crate::kprintln!(
            "init_syscall: core={} LSTAR={:#018x} (expected={:#018x})",
            core_id, lstar_rd, lstar as u64);

        // SFMASK MSR.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0084u32,
            in("eax") SFMASK as u32,
            in("edx") (SFMASK >> 32) as u32,
            options(nostack, nomem),
        );

        // CSTAR MSR (0xC000_0083) - AMD compat-mode SYSCALL dispatch target.
        // On AMD CPUs a `syscall` from compat mode (CS.L=0) dispatches here, not LSTAR.
        // GodspeedOS ring-3 runs in 64-bit mode (CS.L=1) and uses `ud2` for syscalls,
        // so `cstar_entry` should never be reached; it halts loudly if it ever is.
        let cstar = super::syscall_entry::cstar_entry as *const () as u64;
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0083u32,
            in("eax") cstar as u32,
            in("edx") (cstar >> 32) as u32,
            options(nostack, nomem),
        );

        // Diagnostic: read back EFER to confirm SCE (bit 0) and NXE (bit 11) are set.
        // NXE absence on Limine-BIOS boot (AMD T630) makes bit 63 in PTEs a reserved
        // bit, causing silent #PF on first ring-3 stack access.
        let (efer_rd_lo, efer_rd_hi): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx")  0xC000_0080u32,
            out("eax") efer_rd_lo,
            out("edx") efer_rd_hi,
            options(nostack, nomem),
        );
        let efer_rd = ((efer_rd_hi as u64) << 32) | (efer_rd_lo as u64);
        crate::kprintln!(
            "init_syscall: core={} EFER={:#010x} SCE={} NXE={} LME={} LMA={}",
            core_id,
            efer_rd,
            (efer_rd >> 0) & 1,
            (efer_rd >> 11) & 1,
            (efer_rd >> 8) & 1,
            (efer_rd >> 10) & 1,
        );
        // W^X foundation enforcement (H4): we just set NXE; refuse to boot if it did not take, rather than
        // run with every PTE NO_EXEC bit silently ignored (§3.12, loud failure). All x86_64 support NX, so
        // this passes on real hardware - it is the backstop against a boot environment that doesn't.
        assert!((efer_rd >> 11) & 1 == 1,
            "W^X foundation missing: EFER.NXE not enabled on core {} (EFER={:#010x})", core_id, efer_rd);
    }

    // Set IA32_KERNEL_GS_BASE for this core's SYSCALL stub (§8.2).
    // SAFETY: called during init, before any ring-3 task runs.
    unsafe { super::syscall_entry::init_per_core_syscall(core_id as usize) };

    // Diagnostic: read back IA32_KERNEL_GS_BASE and IA32_GS_BASE to verify.
    unsafe {
        let (kgs_lo, kgs_hi): (u32, u32);
        let (gs_lo, gs_hi): (u32, u32);
        core::arch::asm!("rdmsr",
            in("ecx") 0xC000_0102u32, out("eax") kgs_lo, out("edx") kgs_hi,
            options(nostack, nomem));
        core::arch::asm!("rdmsr",
            in("ecx") 0xC000_0101u32, out("eax") gs_lo, out("edx") gs_hi,
            options(nostack, nomem));
        let kgs = ((kgs_hi as u64) << 32) | (kgs_lo as u64);
        let gs  = ((gs_hi  as u64) << 32) | (gs_lo  as u64);
        crate::kprintln!("init_syscall: core={} GS.base={:#x} KERNEL_GS_BASE={:#x}", core_id, gs, kgs);
    }
}

/// Read TSS.rsp0 for `core_id`.
///
/// # Safety
/// Caller must ensure `core_id < num_cores()`.
pub unsafe fn get_tss_rsp0(core_id: usize) -> u64 {
    // SAFETY: tss_for returns this core's TSS (BSP bootstrap or its arena slot); rsp0 is at byte
    // offset 4 of the packed struct. read_unaligned is used because packed structs have alignment 1.
    unsafe {
        let tss = tss_for(core_id);
        let rsp0_ptr = core::ptr::addr_of!((*tss).rsp0);
        rsp0_ptr.read_unaligned()
    }
}

/// Update TSS.rsp0 for `core_id` to `rsp`.
///
/// Called by the scheduler before every context switch TO a ring-3 task so
/// that hardware interrupts hitting ring-3 code push their frame onto the
/// correct per-task kernel stack (§9.2, §14.1).
///
/// # Safety
/// Caller must ensure `core_id < num_cores()` and `rsp` is a valid kernel stack
/// top for the incoming ring-3 task.
pub unsafe fn set_tss_rsp0(core_id: usize, rsp: u64) {
    // SAFETY: tss_for returns this core's TSS (BSP bootstrap or its arena slot); rsp0 is at byte
    // offset 4 of the packed struct. write_unaligned is used because packed structs have alignment
    // 1 - taking a &mut reference would be UB.
    unsafe {
        let tss = tss_for(core_id);
        let rsp0_ptr = core::ptr::addr_of_mut!((*tss).rsp0);
        rsp0_ptr.write_unaligned(rsp);
    }
}

/// Install ISR stubs in all 256 IDT slots, then load the IDT.
///
/// - All vectors default to `exception_halt`.
/// - Vector 13  → GPF diagnostic handler (prints error + RIP, halts).
/// - Vector 14  → Page-fault diagnostic handler (prints CR2 + error, halts).
/// - Vector 32  → APIC timer preemption (§9.1).
/// - Vector 0xF0 → WAKE_RECEIVER IPI.
/// - Vector 0xF1 → TLB_SHOOTDOWN IPI.
/// - Vector 0xF2 → SCHEDULER_TICK IPI.
///
/// # Safety
/// Must be called after `init_gdt` (entries reference the kernel CS = 0x08).
pub(super) unsafe fn init_idt() {
    let halt    = exception_halt as *const () as u64;
    let timer   = super::interrupts::timer_isr_stub as *const () as u64;

    // SAFETY: IDT is a kernel-lifetime static; APs haven't started yet.
    unsafe {
        // &mut *addr_of_mut! is the sanctioned way to get a &mut to a `static mut`
        // without materialising a direct reference (avoids static_mut_refs lint).
        // Fn-item handlers are cast via `*const ()` first so the fn-item→integer
        // lint does not fire (the numeric value is the entry-point address).
        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        for entry in idt.iter_mut() {
            *entry = IdtEntry::new(halt);
        }
        // CPU-exception vectors (0-31): route to the CPL-discriminating stubs so a RING-3 exception
        // KILLS the task instead of wedging the kernel (invariant 12). 6/13/14 keep their dedicated
        // handlers (set below); 8/10/11/12/17/21 push an error code (different CS offset -> the _ec
        // stub). Interrupt vectors (32+) stay at `exception_halt` - a spurious IRQ is not a task fault.
        const EC_VECTORS: [usize; 6] = [8, 10, 11, 12, 17, 21];
        let exc_noec = exc_stub_noec as *const () as u64;
        let exc_ec   = exc_stub_ec   as *const () as u64;
        for v in 0..32usize {
            if v == 6 || v == 13 || v == 14 { continue; } // dedicated handlers below
            idt[v] = IdtEntry::new(if EC_VECTORS.contains(&v) { exc_ec } else { exc_noec });
        }
        // IDT[2] = NMI: the panic path (SEC-18) broadcasts an NMI to every other core so a panic on one
        // core actually stops the machine (§6.2, §19) - NMI reaches a core even spinning IF=0 on a lock.
        // Route it to the UNCONDITIONAL `exception_halt`, NOT the CPL-discriminating `exc_noec` the loop
        // just set: the receiving core must HALT regardless of ring, never kill-a-task-and-continue. No
        // other NMI source exists in the kernel, so any NMI means "stop".
        idt[2]    = IdtEntry::new(halt);
        // IDT[6] = #UD handler: used as the syscall entry on AMD GX-420GI where
        // both SYSCALL and int $0x80 silently stall from ring-3.  DPL=0 is
        // correct - CPU exceptions bypass the DPL check, so ud2 from ring-3
        // always dispatches here; int 6 from ring-3 would #GP (intended).
        idt[6]    = IdtEntry::new(super::syscall_entry::ud2_syscall_entry as *const () as u64);
        idt[13]   = IdtEntry::new(gpf_stub  as *const () as u64);
        idt[14]   = IdtEntry::new(pf_stub   as *const () as u64);
        idt[32]   = IdtEntry::new(timer);
        idt[36]   = IdtEntry::new(super::interrupts::uart_rx_isr_stub as *const () as u64); // IRQ 4 = COM1 RX
        // xHCI MSI (§12) - routed to the userspace driver via interrupt::route. The device
        // delivers here once its interrupter is enabled (P2); harmless until then.
        idt[super::interrupts::XHCI_MSI_VECTOR as usize] =
            IdtEntry::new(super::interrupts::xhci_msi_isr_stub as *const () as u64);
        idt[super::interrupts::EHCI_MSI_VECTOR as usize] =
            IdtEntry::new(super::interrupts::ehci_msi_isr_stub as *const () as u64);
        idt[0x80] = IdtEntry::new_user(super::syscall_entry::int80_entry as *const () as u64);
        idt[0xF0] = IdtEntry::new(ipi_wake_stub   as *const () as u64);
        idt[0xF1] = IdtEntry::new(ipi_tlb_stub    as *const () as u64);
        idt[0xF2] = IdtEntry::new(ipi_tick_stub   as *const () as u64);
        // IDT[0xFF] = the APIC spurious-interrupt vector (SVR = 0x1FF, above). A spurious IRQ is a normal
        // hardware event (an IRQ de-asserted between CPU-ack and vector-read); the architecturally-correct
        // response is to IRET WITHOUT an EOI - NOT to hit the default `exception_halt` and wedge the whole
        // machine (audit K3; north-star: a non-fatal hardware event must never wedge the kernel, inv12).
        idt[0xFF] = IdtEntry::new(spurious_stub    as *const () as u64);

        let desc = TableDescriptor {
            limit: (core::mem::size_of_val(idt) - 1) as u16,
            base:  idt.as_ptr() as u64,
        };
        core::arch::asm!(
            "lidt [{desc}]",
            desc = in(reg) &desc as *const TableDescriptor as u64,
            options(nostack, readonly)
        );
    }
}

// ---------------------------------------------------------------------------
// IPI ISR stubs (§9.4) - one naked stub per vector.
// ---------------------------------------------------------------------------

/// Dispatch target called from all three IPI stubs with the vector number.
///
/// # Safety
/// Called from raw interrupt context (IF=0).
#[no_mangle]
unsafe extern "C" fn ipi_dispatch(vector: u64) {
    // SAFETY: called from raw ISR with IF=0; ipi_handler is safe to call here.
    unsafe { crate::smp::ipi::ipi_handler(vector as u8) }
}

/// Spurious-interrupt ISR stub (APIC vector 0xFF). A spurious interrupt carries no work and must NOT
/// be acknowledged with an EOI, so this touches nothing and simply returns. It clobbers no registers
/// and no GS-relative state, so a bare `iretq` (no register save, no swapgs) is correct from either
/// ring. This replaces the default `exception_halt` for 0xFF so a spurious IRQ is a no-op, not a wedge.
#[unsafe(naked)]
unsafe extern "C" fn spurious_stub() {
    core::arch::naked_asm!("iretq")
}

/// WAKE_RECEIVER ISR stub (vector 0xF0) - cross-core task wakeup (§9.4).
///
/// Mirrors `timer_isr_stub` exactly: conditional swapgs, save caller-saved
/// registers, call `timer_tick_from_irq` (which sends EOI and reschedules),
/// restore registers, conditional swapgs, iretq.
///
/// Calling `timer_tick_from_irq` here (rather than going through `ipi_dispatch`)
/// ensures the same swapgs invariant and the same context-switch path as a
/// real timer preemption, avoiding GS corruption when a cross-core wakeup
/// interrupts a ring-3 task that is in the middle of a SYSCALL.
#[unsafe(naked)]
unsafe extern "C" fn ipi_wake_stub() {
    core::arch::naked_asm!(
        // Conditional swapgs: ring-3 interrupts have GS.base = 0 (user);
        // load kernel GS before any gs:[...] access inside timer_tick_from_irq.
        "test byte ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "call timer_tick_from_irq",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        // Conditional swapgs before iretq: if the frame at RSP is ring-3
        // (possibly a new task after a context switch), restore user GS.
        "test byte ptr [rsp + 8], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "iretq",
    )
}

#[unsafe(naked)]
unsafe extern "C" fn ipi_tlb_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "mov rdi, 0xF1",
        "call ipi_dispatch",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        "iretq",
    )
}

#[unsafe(naked)]
unsafe extern "C" fn ipi_tick_stub() {
    core::arch::naked_asm!(
        "push rax", "push rcx", "push rdx",
        "push rdi", "push rsi", "push r8",
        "push r9",  "push r10", "push r11",
        "mov rdi, 0xF2",
        "call ipi_dispatch",
        "pop r11", "pop r10", "pop r9",
        "pop r8",  "pop rsi", "pop rdi",
        "pop rdx", "pop rcx", "pop rax",
        "iretq",
    )
}

/// No-op: Limine sets up identity-mapped paging before calling _start.
unsafe fn init_paging(_boot_info: &BootInfo) {}

// ---------------------------------------------------------------------------
// Diagnostic exception stubs - vectors 13 (GPF) and 14 (#PF).
//
// Both exceptions push an error code before RIP on the stack, so on entry:
//   [RSP+0]  = error_code
//   [RSP+8]  = saved RIP (fault address)
//   [RSP+16] = saved CS
//   ...
// ---------------------------------------------------------------------------

/// GPF stub: read error code + RIP, call diagnostic handler.
#[unsafe(naked)]
unsafe extern "C" fn gpf_stub() -> ! {
    // SAFETY: vector 13 pushes error_code then RIP; reads are before any RSP change.
    // GP_REACHED is set before any other work so timer ISR on other cores can
    // report it even if gpf_handler stalls.
    core::arch::naked_asm!(
        // Raw 'G' to COM1 as absolute first instruction.
        "mov dx, 0x3fd",
        // Bounded THRE poll (mirrors SERIAL_THRE_NOLCK_CAP): a present-but-wedged COM1 (THRE stuck
        // clear) must NOT hang a fault handler forever - a ring-3 fault would otherwise spin this core
        // with IF=0 (invariant 12, audit K1). ecx is a safe scratch here (the stubs that need rcx
        // reload it from the stack after this poll). On timeout, emit the breadcrumb best-effort.
        "mov ecx, 1000000",
        "88: in al, dx",
        "test al, 0x20",
        "jnz 89f",
        "dec ecx",
        "jnz 88b",
        "89:",
        "mov dx, 0x3f8",
        "mov al, 0x47",   // 'G'
        "out dx, al",
        "mov byte ptr [{gp_flag}], 1",
        // #GP pushes an error code, so the saved CS is at [rsp+16] (bits 1:0 = CPL). A ring-3 #GP
        // (a service ran a privileged instruction or made a non-canonical access) must KILL THE TASK,
        // not the kernel - so swapgs to the kernel GS first, exactly like pf_stub, so kill_current and
        // the scheduler can reach per-core data. A kernel (CPL=0) #GP leaves GS alone and will halt.
        "test byte ptr [rsp + 16], 3", // CPL: non-zero = ring-3
        "jz 1f",                       // kernel #GP → skip swapgs
        "swapgs",                      // ring-3 #GP → install kernel GS
        "1:",
        "mov rdi, [rsp]",      // error_code → first arg
        "mov rsi, [rsp + 8]",  // saved RIP  → second arg
        "mov rdx, [rsp + 16]", // saved CS   → third arg (CPL for kill-vs-halt)
        "call gpf_handler",
        "2: hlt",
        "jmp 2b",
        gp_flag = sym GP_REACHED,
    )
}

/// Page-fault stub: read error code + RIP, call diagnostic handler.
#[unsafe(naked)]
unsafe extern "C" fn pf_stub() -> ! {
    // SAFETY: vector 14 pushes error_code before the standard frame:
    //   [RSP+0]  error_code
    //   [RSP+8]  saved RIP
    //   [RSP+16] saved CS  (bits 1:0 = CPL)
    //   [RSP+24] saved RFLAGS
    //   [RSP+32] saved RSP  (user RSP; only present on ring-3 → ring-0 transition)
    //   [RSP+40] saved SS   (only on ring-3 → ring-0 transition)
    // Interrupt gates clear IF; GS is NOT swapped by the CPU.  We must
    // swapgs manually when coming from ring-3 so pf_handler (and any
    // kernel code it calls, including kill_current/switch_context) can
    // access per-core data via gs:[...].  A kernel fault (CPL=0) means
    // GS.base is already the kernel pointer - skip swapgs.
    core::arch::naked_asm!(
        // Raw 'P' to COM1 as absolute first instruction - fires before any
        // flag-set or push.  A fault→iretq→fault loop produces a visible
        // flood independent of all other handler logic.  dx/al are scratch;
        // this handler never returns to the interrupted context.
        "mov dx, 0x3fd",
        // Bounded THRE poll (mirrors SERIAL_THRE_NOLCK_CAP): a present-but-wedged COM1 (THRE stuck
        // clear) must NOT hang a fault handler forever - a ring-3 fault would otherwise spin this core
        // with IF=0 (invariant 12, audit K1). ecx is a safe scratch here (the stubs that need rcx
        // reload it from the stack after this poll). On timeout, emit the breadcrumb best-effort.
        "mov ecx, 1000000",
        "88: in al, dx",
        "test al, 0x20",
        "jnz 89f",
        "dec ecx",
        "jnz 88b",
        "89:",
        "mov dx, 0x3f8",
        "mov al, 0x50",   // 'P'
        "out dx, al",
        // Store CR2 and set PF_REACHED before any swapgs or serial output.
        // rax is clobbered transiently; saved/restored so the exception frame
        // offsets below are unchanged after the pops.
        "push rax",
        "mov rax, cr2",
        "mov [{pf_cr2}], rax",
        "mov byte ptr [{pf_flag}], 1",
        "pop rax",
        // After pop, RSP is back at exception-frame base:
        //   [RSP+0]  error_code
        //   [RSP+8]  saved RIP
        //   [RSP+16] saved CS  (bits 1:0 = CPL)
        //   [RSP+24] saved RFLAGS
        //   [RSP+32] saved RSP (user RSP, ring-3→ring-0 only)
        //   [RSP+40] saved SS
        "xor edx, edx",                // hw_user_rsp = 0 for kernel faults (3rd arg)
        "test byte ptr [rsp + 16], 3", // CPL in saved CS: non-zero = ring-3
        "jz 1f",                       // kernel fault → skip swapgs + rsp load
        "swapgs",                      // ring-3 fault → install kernel GS
        "mov rdx, [rsp + 32]",         // hw-saved user RSP from interrupt frame
        "1:",
        "mov rdi, [rsp]",              // error_code → first arg
        "mov rsi, [rsp + 8]",          // saved RIP  → second arg
        "call pf_handler",
        "2: hlt",
        "jmp 2b",
        pf_cr2  = sym PF_CR2_STORED,
        pf_flag = sym PF_REACHED,
    )
}

/// Handle a #GP: a RING-3 #GP kills the offending task (the system continues, invariant 12 / §10.3);
/// a RING-0 #GP is genuine kernel-state corruption and halts loudly (§6.2). `saved_cs` bits 1:0 are the
/// CPL. Uses lock-free serial (the fault may have interrupted a kprintln holding LOG_LOCK), mirroring
/// pf_handler.
#[no_mangle]
unsafe extern "C" fn gpf_handler(error_code: u64, fault_rip: u64, saved_cs: u64) -> ! {
    // SAFETY: raw fault context, IF=0, kernel GS installed (swapgs in gpf_stub for ring-3).
    unsafe {
        if saved_cs & 3 != 0 {
            serial_puts_nolck(b"USER GPF (killing task): error_code=");
        } else {
            serial_puts_nolck(b"KERNEL GPF: error_code=");
        }
        serial_hex64_nolck(error_code);
        serial_puts_nolck(b" rip=");
        serial_hex64_nolck(fault_rip);
        serial_puts_nolck(b" cs=");
        serial_hex64_nolck(saved_cs);
        serial_puts_nolck(b"\n");
    }
    // Ring-3 #GP: the service misbehaved, not the kernel - kill it and reschedule (kill_current does
    // not return for a ring-3 fault). Only a ring-0 #GP (or a defensive fall-through) halts all cores.
    if saved_cs & 3 != 0 {
        crate::task::kill_current();
    }
    crate::arch::x86_64::halt_all_cores()
}

/// Print page-fault info and halt all cores (or kill the faulting task).
///
/// `hw_user_rsp` is the hardware-saved user RSP from the interrupt frame
/// ([rsp+32] on ring-3 faults); 0 for kernel faults.
#[no_mangle]
unsafe extern "C" fn pf_handler(error_code: u64, fault_rip: u64, hw_user_rsp: u64) -> ! {
    let cr2: u64;
    let (gs_lo, gs_hi): (u32, u32);
    let (kgs_lo, kgs_hi): (u32, u32);
    // SAFETY: reading CR2 and MSRs in ring 0 is always valid.
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem));
        core::arch::asm!("rdmsr",
            in("ecx")  0xC000_0101u32,
            out("eax") gs_lo,
            out("edx") gs_hi,
            options(nostack, nomem));
        core::arch::asm!("rdmsr",
            in("ecx")  0xC000_0102u32,
            out("eax") kgs_lo,
            out("edx") kgs_hi,
            options(nostack, nomem));
    }
    let gs_base  = ((gs_hi  as u64) << 32) | (gs_lo  as u64);
    let kgs_base = ((kgs_hi as u64) << 32) | (kgs_lo as u64);
    // Read PER_CORE_SYSCALL[0].user_rsp directly for diagnostics.
    // SAFETY: GS.base is the kernel ptr (swapgs done in pf_stub for ring-3 faults;
    //         unchanged for kernel faults).  user_rsp is at GS offset 0.
    let per_core_ursp: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[0]",
            out(reg) per_core_ursp,
            options(nostack, nomem),
        );
    }
    // V1 (kernel-audit-2): a CPL0 fault (error-code bit 2 clear) at a USER virtual
    // address while a guarded user-copy is in progress on this core is a BAD USER
    // POINTER passed to a syscall (read_user_bytes / write_user_bytes copying an
    // unmapped/read-only user page), not kernel-state corruption. Attribute it to the
    // caller and kill that task (like a ring-3 fault), instead of halting all cores.
    // Narrowly gated on the per-core user-copy flag AND cr2 < USER_END, so a genuine
    // kernel bug faulting elsewhere still halts loudly (§6.2 / invariant 12).
    let uc_core = crate::task::scheduler::current_core_id();
    let user_copy_fault = (error_code & (1 << 2) == 0)
        && cr2 < crate::arch::x86_64::syscall_entry::USER_END
        && crate::arch::x86_64::syscall_entry::user_copy_active(uc_core);
    if user_copy_fault {
        // The faulting copy never returns to clear its own flag; clear it here so it
        // does not leak to the next task scheduled on this core.
        crate::arch::x86_64::syscall_entry::clear_user_copy_active(uc_core);
    }
    // Use lock-free serial to avoid a deadlock if LOG_LOCK is already held
    // by the kprintln that was interrupted (interrupt gate: IF=0).
    // Bit 2 of error_code is the user/supervisor flag: 1 = fault from ring 3.
    // Use different prefixes so monitors can distinguish: USER PF and USER-COPY PF are
    // graceful (offending task killed, system continues); KERNEL PF is a fatal crash.
    unsafe {
        if error_code & (1 << 2) != 0 {
            serial_puts_nolck(b"USER PF: fault_addr=");
        } else if user_copy_fault {
            serial_puts_nolck(b"USER-COPY PF (killing caller): fault_addr=");
        } else {
            serial_puts_nolck(b"KERNEL PF: fault_addr=");
        }
        serial_hex64_nolck(cr2);
        serial_puts_nolck(b" error_code=");
        serial_hex64_nolck(error_code);
        serial_puts_nolck(b" rip=");
        serial_hex64_nolck(fault_rip);
        serial_puts_nolck(b" hw_user_rsp=");
        serial_hex64_nolck(hw_user_rsp);
        serial_puts_nolck(b" per_core_ursp=");
        serial_hex64_nolck(per_core_ursp);
        serial_puts_nolck(b" GS.base=");
        serial_hex64_nolck(gs_base);
        serial_puts_nolck(b" KERNEL_GS=");
        serial_hex64_nolck(kgs_base);
        serial_puts_nolck(b"\n");
    }
    // Ring-3 #PF (error_code bit 2 = U/S set): the service touched unmapped memory, not the kernel -
    // kill it and reschedule (§10.3; kill_current does not return for a ring-3 fault). A user-copy
    // fault (CPL0 fault on a user pointer the kernel was copying for a syscall, V1) likewise kills the
    // CALLER, not the kernel. Only a genuine ring-0 #PF (or a defensive fall-through, should kill_current
    // ever return) halts all cores - halt is the fail-safe outcome, never a resume of a killed task.
    // Mirrors gpf_handler / exc_dispatch.
    if error_code & (1 << 2) != 0 || user_copy_fault {
        crate::task::kill_current();
    }
    crate::arch::x86_64::halt_all_cores()
}

// ---------------------------------------------------------------------------
// Exception stub - all unhandled vectors point here.
//
// Reads the top four stack words and passes them to exception_halt_handler,
// which prints them over lock-free serial before halting.  This converts
// every silent freeze into a labelled failure (constitutional invariant 12).
//
// Stack layout on entry:
//   No error code: [RSP+0]=RIP  [RSP+8]=CS  [RSP+16]=RFLAGS  [RSP+24]=RSP
//   Error code:    [RSP+0]=err  [RSP+8]=RIP [RSP+16]=CS      [RSP+24]=RFLAGS
// The CS value (0x08=kernel, 0x28=user) identifies which layout applies.
// ---------------------------------------------------------------------------

#[unsafe(naked)]
unsafe extern "C" fn exception_halt() -> ! {
    // EXCEPTION_HALT_REACHED set BEFORE cli so timer ISR on other cores
    // can observe the flag even though Core 0 will lose its timer after cli.
    core::arch::naked_asm!(
        // Raw '?' to COM1 as absolute first instruction - fires for every
        // unhandled exception vector before cli or flag-set.
        "mov dx, 0x3fd",
        // Bounded THRE poll (mirrors SERIAL_THRE_NOLCK_CAP): a present-but-wedged COM1 (THRE stuck
        // clear) must NOT hang a fault handler forever - a ring-3 fault would otherwise spin this core
        // with IF=0 (invariant 12, audit K1). ecx is a safe scratch here (the stubs that need rcx
        // reload it from the stack after this poll). On timeout, emit the breadcrumb best-effort.
        "mov ecx, 1000000",
        "88: in al, dx",
        "test al, 0x20",
        "jnz 89f",
        "dec ecx",
        "jnz 88b",
        "89:",
        "mov dx, 0x3f8",
        "mov al, 0x3f",   // '?'
        "out dx, al",
        "mov byte ptr [{flag}], 1",
        "cli",
        "mov rdi, [rsp]",
        "mov rsi, [rsp + 8]",
        "mov rdx, [rsp + 16]",
        "mov rcx, [rsp + 24]",
        "call exception_halt_handler",
        "2: hlt",
        "jmp 2b",
        flag = sym EXCEPTION_HALT_REACHED,
    )
}

/// Print the four frame words over lock-free serial, then return to the
/// `hlt` loop in the naked stub above.
///
/// # Safety
/// Called from raw exception context (IF=0, ring-0).  Uses only lock-free
/// serial helpers to avoid deadlocking on LOG_LOCK if the exception fired
/// inside a `kprintln!`.
#[no_mangle]
unsafe extern "C" fn exception_halt_handler(w0: u64, w1: u64, w2: u64, w3: u64) {
    // Identify the likely frame layout by finding the CS slot.
    // CS is zero-extended to 64 bits on the stack: 0x08 (kernel) or 0x28 (user).
    unsafe {
        serial_puts_nolck(b"\nEXCEPTION: [");
        serial_hex64_nolck(w0);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w1);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w2);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w3);
        serial_puts_nolck(b"]");
        if w1 == 0x08 || w1 == 0x28 {
            // No error code pushed: w0=RIP, w1=CS
            serial_puts_nolck(b" RIP=");
            serial_hex64_nolck(w0);
        } else if w2 == 0x08 || w2 == 0x28 {
            // Error code pushed by CPU: w0=errcode, w1=RIP, w2=CS
            serial_puts_nolck(b" errcode=");
            serial_hex64_nolck(w0);
            serial_puts_nolck(b" RIP=");
            serial_hex64_nolck(w1);
        }
        serial_puts_nolck(b"\n");
    }
}

// ---------------------------------------------------------------------------
// CPU-EXCEPTION stubs for vectors 0-31 (invariant 12: a ring-3 exception KILLS
// the task, it does NOT wedge the kernel).
//
// The old catch-all `exception_halt` above halted the whole machine for EVERY
// unhandled vector, so a single ring-3 instruction (`cli` -> #GP; `div` by 0 ->
// #DE; an unmasked FP fault -> #MF/#XM; ...) took down the kernel - the class the
// commandment audit flagged. These stubs mirror pf_stub: check the saved-CS CPL,
// swapgs to the kernel GS for a ring-3 fault, and let exc_dispatch KILL the task
// (system continues); a ring-0 exception is genuine kernel corruption and halts.
//
// Two variants because the CS slot differs: an error-code exception pushes err
// first (CS at [rsp+16]); a no-error-code exception has CS at [rsp+8]. Interrupt
// vectors (32+) are NOT routed here - a spurious IRQ is not the task's fault and
// keeps the `exception_halt` behaviour.
// ---------------------------------------------------------------------------

/// No-error-code exception frame: [rsp+0]=RIP [rsp+8]=CS [rsp+16]=RFLAGS [rsp+24]=RSP.
#[unsafe(naked)]
unsafe extern "C" fn exc_stub_noec() -> ! {
    core::arch::naked_asm!(
        // Raw '?' to COM1, then set the reached-flag BEFORE cli (so other cores can observe it).
        "mov dx, 0x3fd",
        // Bounded THRE poll (mirrors SERIAL_THRE_NOLCK_CAP): a present-but-wedged COM1 (THRE stuck
        // clear) must NOT hang a fault handler forever - a ring-3 fault would otherwise spin this core
        // with IF=0 (invariant 12, audit K1). ecx is a safe scratch here (the stubs that need rcx
        // reload it from the stack after this poll). On timeout, emit the breadcrumb best-effort.
        "mov ecx, 1000000",
        "88: in al, dx",
        "test al, 0x20",
        "jnz 89f",
        "dec ecx",
        "jnz 88b",
        "89:",
        "mov dx, 0x3f8",
        "mov al, 0x3f",   // '?'
        "out dx, al",
        "mov byte ptr [{flag}], 1",
        "cli",
        // CPL from saved CS at [rsp+8]; swapgs for a ring-3 fault so kill_current reaches per-core GS.
        "test byte ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "mov rdi, [rsp]",       // w0 = RIP
        "mov rsi, [rsp + 8]",   // w1 = CS
        "mov rdx, [rsp + 16]",  // w2 = RFLAGS
        "mov rcx, [rsp + 24]",  // w3 = RSP
        "mov r8,  [rsp + 8]",   // cs (5th arg) for the kill-vs-halt decision
        "call exc_dispatch",
        "2: hlt",
        "jmp 2b",
        flag = sym EXCEPTION_HALT_REACHED,
    )
}

/// Error-code exception frame: [rsp+0]=err [rsp+8]=RIP [rsp+16]=CS [rsp+24]=RFLAGS.
#[unsafe(naked)]
unsafe extern "C" fn exc_stub_ec() -> ! {
    core::arch::naked_asm!(
        "mov dx, 0x3fd",
        // Bounded THRE poll (mirrors SERIAL_THRE_NOLCK_CAP): a present-but-wedged COM1 (THRE stuck
        // clear) must NOT hang a fault handler forever - a ring-3 fault would otherwise spin this core
        // with IF=0 (invariant 12, audit K1). ecx is a safe scratch here (the stubs that need rcx
        // reload it from the stack after this poll). On timeout, emit the breadcrumb best-effort.
        "mov ecx, 1000000",
        "88: in al, dx",
        "test al, 0x20",
        "jnz 89f",
        "dec ecx",
        "jnz 88b",
        "89:",
        "mov dx, 0x3f8",
        "mov al, 0x3f",   // '?'
        "out dx, al",
        "mov byte ptr [{flag}], 1",
        "cli",
        // CPL from saved CS at [rsp+16] (error code pushed first); swapgs for a ring-3 fault.
        "test byte ptr [rsp + 16], 3",
        "jz 1f",
        "swapgs",
        "1:",
        "mov rdi, [rsp]",       // w0 = error code
        "mov rsi, [rsp + 8]",   // w1 = RIP
        "mov rdx, [rsp + 16]",  // w2 = CS
        "mov rcx, [rsp + 24]",  // w3 = RFLAGS
        "mov r8,  [rsp + 16]",  // cs (5th arg)
        "call exc_dispatch",
        "2: hlt",
        "jmp 2b",
        flag = sym EXCEPTION_HALT_REACHED,
    )
}

/// A ring-3 CPU exception (`cs & 3 != 0`) kills the offending task and the system continues
/// (invariant 12 / §10.3); a ring-0 exception is genuine kernel-state corruption and halts all cores
/// (§6.2). Lock-free serial: the fault may have interrupted a kprintln holding LOG_LOCK.
#[no_mangle]
unsafe extern "C" fn exc_dispatch(w0: u64, w1: u64, w2: u64, w3: u64, cs: u64) -> ! {
    // SAFETY: raw fault context, IF=0, kernel GS installed (swapgs in the stub for ring-3).
    unsafe {
        if cs & 3 != 0 {
            serial_puts_nolck(b"\nUSER EXCEPTION (killing task): [");
        } else {
            serial_puts_nolck(b"\nKERNEL EXCEPTION: [");
        }
        serial_hex64_nolck(w0);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w1);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w2);
        serial_puts_nolck(b"] [");
        serial_hex64_nolck(w3);
        serial_puts_nolck(b"] cs=");
        serial_hex64_nolck(cs);
        serial_puts_nolck(b"\n");
    }
    if cs & 3 != 0 {
        crate::task::kill_current(); // ring-3: task dies, reschedule (does not return for a ring-3 fault)
    }
    crate::arch::x86_64::halt_all_cores() // ring-0, or a defensive fall-through
}
