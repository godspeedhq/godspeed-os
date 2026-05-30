//! BSP/AP hardware initialisation — §11.1, §11.2.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::BootInfo;

const MAX_CORES: usize = crate::smp::core::MAX_CORES;

// ---------------------------------------------------------------------------
// GDT — eight 64-bit descriptors (per core).
//
// Slot  Selector  Descriptor
//   0    0x00      null
//   1    0x08      kernel code: 64-bit, ring-0, execute/read
//   2    0x10      kernel data: ring-0, read/write
//   3    0x18      placeholder — SYSRETQ needs the 0x18 base to derive
//                  user SS (0x18+8=0x20) and CS (0x18+16=0x28)
//   4    0x20      user data:   ring-3, read/write
//   5    0x28      user code:   64-bit, ring-3, execute/read
//   6    0x30  ]   TSS descriptor (16-byte system descriptor = 2 slots)
//   7    0x38  ]
//
// STAR MSR encodes kernel CS at [47:32]=0x08 and SYSRETQ base at [63:48]=0x18.
//
// The CPU writes the Accessed bit into segment descriptors, and `ltr` writes
// the "busy" bit into the TSS descriptor — both require .data (writable).
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

#[link_section = ".data"]
static mut GDT_PER_CORE: [[u64; 8]; MAX_CORES] = [GDT_TEMPLATE; MAX_CORES];

// ---------------------------------------------------------------------------
// TSS (Task State Segment) — one per core.
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
#[link_section = ".data"]
static mut TSS_PER_CORE: [Tss; MAX_CORES] = [const {
    Tss {
        _res0: 0, rsp0: 0, rsp1: 0, rsp2: 0,
        _res1: 0, ist: [0; 7], _res2: 0, _res3: 0,
        io_map_base: 104,
    }
}; MAX_CORES];

// ---------------------------------------------------------------------------
// IDT — 256 interrupt gates.
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

    /// Like `new` but DPL=3 — ring-3 code may invoke this vector via `int N`.
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
// Local APIC MMIO — set during init_local_apic, read by apic_send_eoi.
// ---------------------------------------------------------------------------

static mut APIC_VIRT_BASE: u64 = 0;

// APIC register offsets (xAPIC MMIO, 32-bit accesses).
const APIC_ID:           u64 = 0x020;
const APIC_TPR:          u64 = 0x080; // Task Priority Register — must be 0 to accept all vectors
const APIC_EOI:          u64 = 0x0B0;
const APIC_SPURIOUS:     u64 = 0x0F0;
const APIC_LVT_TIMER:    u64 = 0x320;
const APIC_TIMER_INIT:   u64 = 0x380;
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

// ---------------------------------------------------------------------------
// Diagnostic exception flags — set as the very first action inside each
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

/// Per-core interrupted RIP saved by `timer_tick_from_irq` on every tick.
/// Index = core ID (0–3).  Low 2 bits of the matching INTERRUPTED_CS entry = CPL.
pub static INTERRUPTED_RIP: [AtomicU64; 4] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];

/// Per-core interrupted CS saved by `timer_tick_from_irq` on every tick.
/// Low 2 bits are CPL: 0 = ring-0, 3 = ring-3.
pub static INTERRUPTED_CS: [AtomicU64; 4] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];

/// Per-core interrupted user RSP saved by `timer_tick_from_irq` on every tick.
/// Only meaningful when the matching INTERRUPTED_CS entry shows CPL=3.
/// If RSP == 0x80000000 (init's initial stack top) the task was interrupted
/// before executing its very first instruction (`push rbx` at 0x400000).
pub static INTERRUPTED_RSP: [AtomicU64; 4] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];

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
                // that is fine — we just proceed.
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
    let tsc_ticks = if tsc_deadline_supported {
        // Compute from CPUID.0x15; returns 0 if frequency cannot be determined.
        unsafe { compute_tsc_ticks_per_10ms(lapic_id) }
    } else {
        0
    };

    if tsc_ticks > 0 {
        // TSC-Deadline mode: LVT bits[18:17] = 10b (mode 2), vector 0x20.
        // No DIVIDE or INIT registers used in this mode.
        write_apic(apic_virt, APIC_LVT_TIMER, (1 << 18) | 0x20);

        // Store per-quantum tick count for re-arm in the timer ISR.
        // Relaxed is fine: written once before APs start, read after.
        TSC_TICKS_PER_QUANTUM.store(tsc_ticks, Ordering::Relaxed);
        TSC_DEADLINE_MODE.store(true, Ordering::Relaxed);

        // First deadline arm is deferred to scheduler::run(), after
        // CORE_SCHED_CTX[cid].cr3 is seeded.  Arming here would risk the
        // timer firing before the scheduler context is initialised, causing
        // switch_context to load cr3=0 and triple-fault silently.
        crate::kprintln!("apic: core {} TSC-Deadline timer ({} ticks/quantum)", lapic_id, tsc_ticks);

        // TSC-Deadline mode still needs the package C-state limit.
        // Without it the firmware can promote the package to PC6+ between
        // timer deadlines, power-gating the APIC and silently dropping the
        // next TSC-Deadline interrupt.  Limiting to PC1 keeps the APIC
        // powered across all C-states the OS sees.
        // SAFETY: ring-0; APIC initialised above.
        unsafe { limit_package_cstates(lapic_id) };
    } else {
        // Periodic mode: fires every ~10 ms regardless of C-states on supported HW.
        // On QEMU (no TSC-Deadline in default cpu model) this is the normal path.
        write_apic(apic_virt, APIC_LVT_TIMER, (1 << 17) | 0x20);
        write_apic(apic_virt, APIC_TIMER_DIVIDE, 0x03);
        // ~100 ms at 1 GHz APIC bus / 16 divider (QEMU).
        // ~50 ms on AMD GX-420GI (Jaguar): APIC timer ≈ CPU_CLOCK/16 ≈ 125 MHz.
        // Increased 10× from 625_000 to break the timer-ISR cascade on AMD hardware
        // where verbose diagnostic output (~18 ms) exceeded the prior 5 ms period.
        write_apic(apic_virt, APIC_TIMER_INIT, 6_250_000);
        if tsc_deadline_supported {
            crate::kprintln!("apic: core {} periodic timer (TSC freq unknown)", lapic_id);
        }
        // SAFETY: ring-0; APIC initialised above.
        unsafe { limit_package_cstates(lapic_id) };
    }
}

/// Check CPUID leaf 1, ECX bit 24 for TSC-Deadline timer support.
///
/// # Safety
/// Ring-0 only.
unsafe fn cpuid_tsc_deadline_supported() -> bool {
    // SAFETY: __cpuid(1) is universally safe on x86_64; CPUID.01H:ECX[24] =
    // APIC TSC-Deadline timer support (Intel SDM Vol. 2A).
    let result = unsafe { core::arch::x86_64::__cpuid(1) };
    (result.ecx >> 24) & 1 != 0
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
    let r15 = unsafe { core::arch::x86_64::__cpuid_count(0x15, 0) };
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
    let max_leaf = unsafe { core::arch::x86_64::__cpuid(0) }.eax;
    if max_leaf >= 0x16 {
        // SAFETY: leaf 0x16 exists per max_leaf check above.
        let r16 = unsafe { core::arch::x86_64::__cpuid(0x16) };
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
    // SAFETY: delegated to arm_tsc_deadline_now — same preconditions.
    unsafe { arm_tsc_deadline_now(ticks) };
}

/// Returns true when running on a GenuineIntel CPU.
///
/// Used to gate Intel-specific MSR accesses (e.g. MSR 0xE2) that do not exist
/// on AMD and would cause #GP(0) if accessed there.
fn is_intel_cpu() -> bool {
    // CPUID leaf 0: EBX/ECX/EDX encode the 12-byte vendor string.
    // "GenuineIntel" → EBX=0x756e6547 EDX=0x49656e69 ECX=0x6c65746e
    // SAFETY: __cpuid(0) is universally safe on x86_64.
    let r = unsafe { core::arch::x86_64::__cpuid(0) };
    r.ebx == 0x756e_6547 && r.edx == 0x4965_6e69 && r.ecx == 0x6c65_746e
}

/// Limit package C-states to PC1 to prevent APIC power-gate on Goldmont+.
///
/// On Intel Atom / Goldmont+ (Gemini Lake, Wyse 5070 J5005), the firmware
/// autonomously promotes the SoC package to PC6+, which power-gates the local
/// APIC — silencing both the periodic APIC timer and cross-core IPIs even when
/// the cores are actively executing code (no PAUSE/HLT required to trigger it).
///
/// MSR_PKG_CST_CONFIG_CONTROL (0xE2) bits:
///   [2:0] package C-state limit  (0=PC0, 1=PC1, 2=PC2, …; higher = deeper)
///   [15]  CFG_LOCK — if set, MSR is read-only (WRMSR → #GP)
///
/// Writes bits[2:0]=1 (PC1 limit) if the MSR is not locked.  PC1 keeps the
/// APIC powered; PC2+ may not.  If the MSR is locked we cannot help via this
/// path and must fall back to TSC-Deadline timer mode (see TODO).
///
/// # Safety
/// Ring-0 only.  Called once per core from `init_local_apic` after APIC setup.
unsafe fn limit_package_cstates(core_id: u32) {
    // MSR 0xE2 (MSR_PKG_CST_CONFIG_CONTROL) is Intel-specific.
    // On AMD processors this MSR does not exist; RDMSR/WRMSR cause #GP(0).
    if !is_intel_cpu() {
        return;
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
        // BIOS locked the MSR — cannot write; APIC timer may still be gated in
        // deep package C-states.  A TSC-Deadline timer does not require this MSR.
        crate::kprintln!("cstate: core {} MSR 0xE2 locked — C-state limit cannot be set via MSR", core_id);
        return;
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

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Lock-free serial helpers — used in fault handlers where LOG_LOCK may
// already be held (nested kprintln → deadlock with IF=0).
// ---------------------------------------------------------------------------

#[inline]
unsafe fn serial_poll_thre() {
    // SAFETY: port I/O in ring-0; 0x3FD is COM1 LSR.
    unsafe {
        loop {
            let lsr: u8;
            core::arch::asm!(
                "in al, dx",
                out("al") lsr,
                in("dx") 0x3FDu16,
                options(nostack, nomem),
            );
            if lsr & 0x20 != 0 { break; }
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

/// Remap the legacy 8259 PIC to vectors 0x20–0x2F then mask all IRQs.
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
        outb(0x21, 0x20);     // ICW2: master IRQ0–7 → vectors 32–39
        outb(0xA1, 0x28);     // ICW2: slave  IRQ8–15 → vectors 40–47
        outb(0x21, 0x04);     // ICW3: master has slave on IRQ2
        outb(0xA1, 0x02);     // ICW3: slave cascade identity = 2
        outb(0x21, 0x01);     // ICW4: 8086 mode
        outb(0xA1, 0x01);     // ICW4: 8086 mode
        outb(0x21, 0xFF);     // OCW1: mask all master IRQs
        outb(0xA1, 0xFF);     // OCW1: mask all slave  IRQs
    }
}

#[inline]
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

    // Fill the TSS descriptor into slots 6 and 7 of this core's GDT.
    // SAFETY: GDT_PER_CORE and TSS_PER_CORE live in .data; single writer per
    // core (only this core touches its own slot during init).
    unsafe {
        let tss_ptr = &raw const TSS_PER_CORE[cid];
        let (lo, hi) = make_tss_descriptor(tss_ptr);
        GDT_PER_CORE[cid][6] = lo;
        GDT_PER_CORE[cid][7] = hi;
    }

    let desc = TableDescriptor {
        limit: (core::mem::size_of::<[u64; 8]>() - 1) as u16,
        base:  unsafe { GDT_PER_CORE[cid].as_ptr() } as u64,
    };

    // SAFETY: GDT_PER_CORE[cid] is valid .data memory; desc outlives the lgdt.
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
        // RIP then CS — the only way to change CS in 64-bit mode.
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
///   EFER.SCE   — enables SYSCALL/SYSRETQ instructions.
///   STAR        — kernel CS (0x08) and SYSRETQ user-segment base (0x18).
///   LSTAR       — address of `syscall_entry` (our SYSCALL handler).
///   SFMASK      — RFLAGS bits to clear on SYSCALL entry (clears IF = bit 9).
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
        // Enable SYSCALL/SYSRETQ in EFER (bit 0 = SCE).
        let (efer_lo, efer_hi): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx")  0xC000_0080u32,
            out("eax") efer_lo,
            out("edx") efer_hi,
            options(nostack, nomem),
        );
        let efer = ((efer_hi as u64) << 32) | (efer_lo as u64) | 1u64;
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

        // LSTAR MSR — address of the SYSCALL entry point.
        let lstar = super::syscall_entry::syscall_entry as u64;
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0082u32,
            in("eax") lstar as u32,
            in("edx") (lstar >> 32) as u32,
            options(nostack, nomem),
        );

        // LSTAR readback — confirm the WRMSR took effect (AMD quirk guard).
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

        // CSTAR MSR (0xC000_0083) — AMD compat-mode SYSCALL dispatch target.
        // On AMD CPUs a `syscall` from compat mode (CS.L=0) dispatches here, not LSTAR.
        // GodspeedOS ring-3 runs in 64-bit mode (CS.L=1) and uses `ud2` for syscalls,
        // so `cstar_entry` should never be reached; it halts loudly if it ever is.
        let cstar = super::syscall_entry::cstar_entry as u64;
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
/// Caller must ensure `core_id < MAX_CORES`.
pub unsafe fn get_tss_rsp0(core_id: usize) -> u64 {
    // SAFETY: TSS_PER_CORE lives in .data; rsp0 is at byte offset 4 of the
    // packed struct. read_unaligned is used because packed structs have
    // alignment 1.
    unsafe {
        let tss = &raw const TSS_PER_CORE[core_id];
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
/// Caller must ensure `core_id < MAX_CORES` and `rsp` is a valid kernel stack
/// top for the incoming ring-3 task.
pub unsafe fn set_tss_rsp0(core_id: usize, rsp: u64) {
    // SAFETY: TSS_PER_CORE lives in .data; rsp0 is at byte offset 4 of the
    // packed struct. write_unaligned is used because packed structs have
    // alignment 1 — taking a &mut reference would be UB.
    unsafe {
        let tss = &raw mut TSS_PER_CORE[core_id];
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
    let halt    = exception_halt as u64;
    let timer   = super::interrupts::timer_isr_stub as u64;

    // SAFETY: IDT is a kernel-lifetime static; APs haven't started yet.
    unsafe {
        for entry in IDT.iter_mut() {
            *entry = IdtEntry::new(halt);
        }
        // IDT[6] = #UD handler: used as the syscall entry on AMD GX-420GI where
        // both SYSCALL and int $0x80 silently stall from ring-3.  DPL=0 is
        // correct — CPU exceptions bypass the DPL check, so ud2 from ring-3
        // always dispatches here; int 6 from ring-3 would #GP (intended).
        IDT[6]    = IdtEntry::new(super::syscall_entry::ud2_syscall_entry as u64);
        IDT[13]   = IdtEntry::new(gpf_stub  as u64);
        IDT[14]   = IdtEntry::new(pf_stub   as u64);
        IDT[32]   = IdtEntry::new(timer);
        IDT[36]   = IdtEntry::new(super::interrupts::uart_rx_isr_stub as u64); // IRQ 4 = COM1 RX
        IDT[0x80] = IdtEntry::new_user(super::syscall_entry::int80_entry as u64);
        IDT[0xF0] = IdtEntry::new(ipi_wake_stub   as u64);
        IDT[0xF1] = IdtEntry::new(ipi_tlb_stub    as u64);
        IDT[0xF2] = IdtEntry::new(ipi_tick_stub   as u64);

        let desc = TableDescriptor {
            limit: (core::mem::size_of_val(&IDT) - 1) as u16,
            base:  IDT.as_ptr() as u64,
        };
        core::arch::asm!(
            "lidt [{desc}]",
            desc = in(reg) &desc as *const TableDescriptor as u64,
            options(nostack, readonly)
        );
    }
}

// ---------------------------------------------------------------------------
// IPI ISR stubs (§9.4) — one naked stub per vector.
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

/// WAKE_RECEIVER ISR stub (vector 0xF0) — cross-core task wakeup (§9.4).
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
// Diagnostic exception stubs — vectors 13 (GPF) and 14 (#PF).
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
        "88: in al, dx",
        "test al, 0x20",
        "jz 88b",
        "mov dx, 0x3f8",
        "mov al, 0x47",   // 'G'
        "out dx, al",
        "mov byte ptr [{gp_flag}], 1",
        "mov rdi, [rsp]",      // error_code → first arg
        "mov rsi, [rsp + 8]",  // saved RIP  → second arg
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
    // GS.base is already the kernel pointer — skip swapgs.
    core::arch::naked_asm!(
        // Raw 'P' to COM1 as absolute first instruction — fires before any
        // flag-set or push.  A fault→iretq→fault loop produces a visible
        // flood independent of all other handler logic.  dx/al are scratch;
        // this handler never returns to the interrupted context.
        "mov dx, 0x3fd",
        "88: in al, dx",
        "test al, 0x20",
        "jz 88b",
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

/// Print GPF info and halt all cores.
#[no_mangle]
unsafe extern "C" fn gpf_handler(error_code: u64, fault_rip: u64) -> ! {
    crate::kprintln!(
        "KERNEL GPF: error_code={:#x} rip={:#x}",
        error_code, fault_rip
    );
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
    // Use lock-free serial to avoid a deadlock if LOG_LOCK is already held
    // by the kprintln that was interrupted (interrupt gate: IF=0).
    // Bit 2 of error_code is the user/supervisor flag: 1 = fault from ring 3.
    // Use different prefixes so monitors can distinguish: USER PF is graceful
    // (service killed, system continues); KERNEL PF is a fatal crash.
    unsafe {
        if error_code & (1 << 2) != 0 {
            serial_puts_nolck(b"USER PF: fault_addr=");
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
    // User-mode faults kill the task (§10.3); kernel faults are fatal panics.
    if error_code & (1 << 2) != 0 {
        crate::task::kill_current();
    }
    crate::arch::x86_64::halt_all_cores()
}

// ---------------------------------------------------------------------------
// Exception stub — all unhandled vectors point here.
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
        // Raw '?' to COM1 as absolute first instruction — fires for every
        // unhandled exception vector before cli or flag-set.
        "mov dx, 0x3fd",
        "88: in al, dx",
        "test al, 0x20",
        "jz 88b",
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
