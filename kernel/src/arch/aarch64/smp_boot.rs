// SPDX-License-Identifier: GPL-2.0-only
//! Bringing up the Pi 4's other three cores.
//!
//! ## How a secondary is released
//!
//! The stock armstub parks each secondary polling a fixed physical address (`0xE0`, `0xE8`, `0xF0` for
//! cores 1..3). Writing an entry point there and executing `sev` releases it. That is the mechanism this
//! file uses, and the only one.
//!
//! ## Why there is no PSCI attempt here
//!
//! PSCI would be the tidier answer, and an earlier version tried `CPU_ON` first and fell back. **It hung
//! the board.** An `smc` with no handler installed at EL3 does not return an error - it traps to a
//! vector that was never populated and the machine stops dead, before it can report anything. And there
//! is no way to ask first: a PSCI version query is itself an `smc`, so probing has exactly the failure
//! it is probing for.
//!
//! The guard that was there ("we booted at EL2, so something must own EL3") is not the same claim: the
//! stock Pi 4 armstub passes control at EL2 and implements no PSCI handler at all. QEMU, which hands
//! over at EL3, was the only environment where the guard did the right thing - so the bug was invisible
//! in the one place it was tested. The spin table is what this firmware implements, it is what the
//! board's own device tree describes, and it needs no `smc` to find out.
//!
//! ## Two phases, because the APs are ready before the kernel is
//!
//! A core released here comes up long before the per-core arenas exist - and `percpu_init` sizes those
//! arenas from *how many cores came up*, so it cannot run until they have. The circularity is broken by
//! parking them: each AP enables the MMU, moves to the high half, reports in, and then **spins on a go
//! flag** until the boot core has built everything a scheduler needs. Only then does it enter the
//! scheduler.
//!
//! ## Every wait is bounded
//!
//! A core that never checks in must not hang the boot (invariant 12). The machine continues with
//! however many came up, and says how many did not - which is exactly what §11.3 asks for: "kernel logs
//! warning, continues with available cores".

use core::sync::atomic::{AtomicU32, Ordering};

use super::{put_dec, put_str};

/// How many secondaries this board has. The BCM2711 is a fixed quad-core part; there is nothing to
/// discover and nothing that varies, so this is a constant rather than a probe.
const SECONDARIES: usize = 3;

/// Stack for each secondary. Fixed, in `.bss`, sized like the boot stack - a bounded, visible footprint
/// (§26.6.1) rather than an allocation that has to succeed this early.
const AP_STACK_SIZE: usize = 64 * 1024;
#[repr(C, align(16))]
struct ApStack([u8; AP_STACK_SIZE]);
static mut AP_STACKS: [ApStack; SECONDARIES] = [
    ApStack([0; AP_STACK_SIZE]),
    ApStack([0; AP_STACK_SIZE]),
    ApStack([0; AP_STACK_SIZE]),
];

/// Secondaries that have reached the parking loop.
static AP_READY: AtomicU32 = AtomicU32::new(0);
/// Raised by the boot core once the per-core arenas exist and a scheduler may be entered.
static AP_GO: AtomicU32 = AtomicU32::new(0);

/// Raised once the page tables exist, so a secondary may enable its MMU.
///
/// **A secondary can arrive here two entirely different ways.** Firmware that parks its cores releases
/// them when we write the spin table - by which time the boot core is long past building the tables.
/// But an environment that starts every core at the kernel entry (which is what QEMU does) delivers
/// them at `_start` immediately, racing the boot core before a single table exists. Both funnel into
/// the same entry and both wait here, so the early arrival is simply a longer wait rather than a
/// different code path that only one of the two ever exercises.
#[no_mangle]
pub static AP_TABLES_READY: AtomicU32 = AtomicU32::new(0);

/// How far each core got, written by that core and read by the boot core.
///
/// **Memory, not the UART.** Raw probe characters were the previous attempt and they lost the argument
/// to the hardware: four cores writing a shared FIFO with no lock drop bytes when it fills, so a
/// missing character meant either "that core never got there" or "the FIFO ate it" - the exact
/// ambiguity the probe existed to remove. One byte of memory per core has no such failure mode, and the
/// boot core prints all four in a single line that cannot interleave.
///
/// 0 = never arrived, 1 = reached the park, 2 = left the park, 3 = in the scheduler.
static AP_PROGRESS: [AtomicU32; 4] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

/// Spin-table release addresses for cores 1..3, as the Pi's armstub parks them.
const SPIN_TABLE: [u64; SECONDARIES] = [0xE0, 0xE8, 0xF0];

/// The physical address a secondary must start executing at.
///
/// The kernel is linked HIGH and loaded LOW, and a parked core starts with translation OFF - so it has
/// to be handed the entry's *physical* address, not the linker's. Getting this wrong hands a core an
/// address that means nothing to it, and the only symptom is a core that never checks in.
fn ap_entry_phys() -> u64 {
    super::mmu::virt_to_phys(ap_entry as usize as u64)
}

/// Release a core through the firmware's spin table.
fn spin_table_release(idx: usize, entry: u64) {
    let addr = SPIN_TABLE[idx];
    // SAFETY: a fixed low physical address the firmware reserves for exactly this, still covered by the
    // kernel's direct map. The parked core polls it with its caches off, so the write is cleaned to the
    // point of coherency before the `sev` that wakes it - without that it can poll a stale zero forever.
    unsafe {
        let p = (super::mmu::KERNEL_VA_BASE + addr) as *mut u64;
        p.write_volatile(entry);
        super::mmu::dma_sync(p as u64, 8);
        core::arch::asm!("dsb sy", "sev", options(nostack));
    }
}

/// Start the secondaries. Returns how many reached the parking loop.
pub fn start_secondaries() -> usize {
    // Cores that were already delivered to `_start` are waiting on this; cores the firmware parked will
    // see it set by the time the release below reaches them. Either way it is set before anything can
    // enable an MMU against a table that does not exist yet.
    AP_TABLES_READY.store(1, Ordering::Release);
    // **Clean the flag to memory, because the cores reading it have their caches OFF.**
    //
    // This store goes through the boot core's cacheable high mapping. A parked secondary polls the same
    // location with translation and caches disabled, so it reads physical memory directly - and a write
    // sitting in the boot core's D-cache is invisible to it. Nothing forces that line out, so whether
    // the secondaries ever start depends on when the cache happens to evict it: sometimes all three come
    // up, sometimes none do, from the same image. That intermittency was the whole bug.
    //
    // The spin-table write a few lines below already did this. Doing it there and not here is the same
    // lesson applied to one of the two places it applies to.
    // SAFETY: cache maintenance over this module's own static, then `sev` to re-check the poll.
    unsafe {
        super::mmu::dma_sync((&raw const AP_TABLES_READY) as u64, 4);
        core::arch::asm!("dsb sy", "sev", options(nostack));
    }

    let entry = ap_entry_phys();
    put_str(b"smp: releasing 3 secondary core(s), entry at ");
    super::put_hex(entry);
    put_str(b"\r\n");

    for i in 0..SECONDARIES {
        spin_table_release(i, entry);
    }
    put_str(b"smp: released via the firmware spin table\r\n");

    // Bounded wait. A core that never checks in leaves the machine running on the ones that did, which
    // is what §11.3 requires; hanging here would trade three cores for none.
    let hz = super::timer::frequency().max(1);
    let start = super::read_cycle_counter();
    while AP_READY.load(Ordering::Acquire) < SECONDARIES as u32 {
        if super::read_cycle_counter().wrapping_sub(start) > hz {
            break; // one second is far longer than a core needs to enable its MMU
        }
        core::hint::spin_loop();
    }
    let up = AP_READY.load(Ordering::Acquire) as usize;
    put_str(b"smp: ");
    put_dec(up as u64);
    put_str(b" of 3 secondaries checked in\r\n");
    if up < SECONDARIES {
        put_str(b"smp: WARNING some cores did not start - continuing with the ones that did\r\n");
    }
    up
}

/// Let the parked secondaries enter the scheduler. Called once the per-core arenas exist.
pub fn release_secondaries() {
    AP_GO.store(1, Ordering::Release);
    // The parked cores are in a `wfe` loop, so they need an event to look again.
    // SAFETY: `sev` is always valid; it only wakes cores waiting on an event.
    unsafe { core::arch::asm!("sev", options(nostack)) };
}

/// Report which cores actually entered the scheduler, in ONE line written by ONE core.
///
/// Per-core "I am up" messages cannot be trusted here: four cores share one UART, and even with a lock
/// on the log path the arch's own `put_str` is unlocked, so lines get shredded. The first four-core
/// hardware boot printed no "core N online" lines at all while QEMU printed every one - the cores were
/// running, the evidence was not surviving. A single line emitted by the boot core after the others have
/// settled cannot interleave with anything, so it is the one statement about core count worth believing.
pub fn report_cores_up() {
    // Give the released cores a moment to reach `mark_ready`. Bounded: this is a report, and a report
    // that hangs the boot is worse than an incomplete one.
    let hz = super::timer::frequency().max(1);
    let start = super::read_cycle_counter();
    while crate::smp::core::ready_count() < (AP_READY.load(Ordering::Acquire) + 1) {
        if super::read_cycle_counter().wrapping_sub(start) > hz / 2 {
            break;
        }
        core::hint::spin_loop();
    }
    let mut mask = 0u32;
    for c in 0..4u32 {
        if crate::smp::core::is_ready(c) {
            mask |= 1 << c;
        }
    }
    // ONE write, not five. Each `put_str` takes and releases the serial claim, so a five-call line is
    // five chances for another core to land in the middle of it - which is exactly what it did, over
    // and over: `smp: cores in the scheduler: smp: core 3 ready`. The lock was never the missing piece;
    // the line has to BE one write. `console_notice_fmt` renders it into a fixed buffer first and emits
    // it once.
    super::console_notice_fmt(format_args!(
        "smp: cores in the scheduler: {} (mask {:#x}); progress 0/1/2/3 = {}{}{}{}          (0=absent 1=parked 2=released 3=scheduling)",
        crate::smp::core::ready_count(),
        mask,
        AP_PROGRESS[0].load(Ordering::Acquire),
        AP_PROGRESS[1].load(Ordering::Acquire),
        AP_PROGRESS[2].load(Ordering::Acquire),
        AP_PROGRESS[3].load(Ordering::Acquire),
    ));
}

/// The symbol `_start` branches a secondary to. Same function; named for the assembly reference.
pub use ap_entry as ap_entry_sym;

/// Where a released secondary lands. Translation is OFF and this is running at its PHYSICAL address, so
/// everything here must be position-independent until the MMU is on.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.boot"]
pub unsafe extern "C" fn ap_entry() -> ! {
    core::arch::naked_asm!(
        // The same exception-level ladder the boot core runs. A secondary is parked at whatever level
        // the firmware was in, which is EL3 under QEMU and EL2 on the board - so neither can be assumed.
        "mrs  x0, CurrentEL",
        "lsr  x0, x0, #2",
        "cmp  x0, #3",
        "b.ne 5f",
        "mov  x0, #0x531",
        "msr  scr_el3, x0",
        "msr  cptr_el3, xzr",
        "mov  x0, #0x3c9",
        "msr  spsr_el3, x0",
        "adr  x0, 5f",
        "msr  elr_el3, x0",
        "eret",
        "5:",
        "mrs  x0, CurrentEL",
        "lsr  x0, x0, #2",
        "cmp  x0, #2",
        "b.ne 4f",
        "mov  x0, #(1 << 31)",
        "msr  hcr_el2, x0",
        "mrs  x0, cnthctl_el2",
        "orr  x0, x0, #3",
        "msr  cnthctl_el2, x0",
        "msr  cntvoff_el2, xzr",
        "mov  x0, #0x0800",
        "movk x0, #0x30d0, lsl #16",
        "msr  sctlr_el1, x0",
        "mov  x0, #0x3c5",
        "msr  spsr_el2, x0",
        "adr  x0, 4f",
        "msr  elr_el2, x0",
        "eret",
        "4:",
        // FP/SIMD, because Rust emits NEON for ordinary byte copies.
        "mov  x0, #(3 << 20)",
        "msr  cpacr_el1, x0",
        "isb",
        // Stack: AP_STACKS[core - 1], top down. Computed PC-relative so it is a physical address, which
        // is what this core can use until its MMU is on.
        "mrs  x1, mpidr_el1",
        "and  x1, x1, #0xff",
        "sub  x2, x1, #1",                   // secondary index
        "adrp x3, {stacks}",
        "add  x3, x3, :lo12:{stacks}",
        "mov  x4, {stack_size}",
        "madd x3, x2, x4, x3",               // &AP_STACKS[idx]
        "add  x3, x3, x4",                   // its top
        "and  x3, x3, #0xfffffffffffffff0",
        "mov  sp, x3",
        // Wait for the boot core to build the page tables. A core delivered straight to `_start` gets
        // here before they exist; one released by firmware finds this already set.
        "adrp x5, {ready}",
        "add  x5, x5, :lo12:{ready}",
        "2:",
        "ldr  w6, [x5]",
        "cbnz w6, 3f",
        "wfe",
        "b    2b",
        "3:",
        "mov  x0, x1",                       // arg0 = core id
        "bl   {main}",
        "1:  wfe",
        "b    1b",
        ready = sym AP_TABLES_READY,
        stacks = sym AP_STACKS,
        stack_size = const AP_STACK_SIZE,
        main = sym aarch64_ap_main,
    )
}

/// Rust side of a secondary's start-up. Runs with translation OFF on entry.
#[no_mangle]
extern "C" fn aarch64_ap_main(core_id: u64) -> ! {
    let _ = core_id;
    // SAFETY: the boot core has already built the tables; this installs them for this core only, and
    // then moves this core into the high half exactly as the boot core did.
    unsafe {
        super::mmu::enable_secondary();
        super::mmu::jump_high(ap_high_entry)
    }
}

/// Where a secondary continues once it is running in the high half.
extern "C" fn ap_high_entry() -> ! {
    let core_id = {
        let m: u64;
        // SAFETY: reading MPIDR_EL1 is a side-effect-free system-register read.
        unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) m, options(nomem, nostack)) };
        m & 0xFF
    };

    // Vectors before anything can fault. A secondary without them takes its first exception into
    // whatever VBAR happened to hold, which is not a fault report, it is a silent lockup.
    super::exceptions::init();

    AP_READY.fetch_add(1, Ordering::AcqRel);

    // Park until the boot core has built the per-core arenas. Entering the scheduler before they exist
    // indexes state that has not been allocated - the same class of bug as reaching the user-copy seam
    // before its per-core arena existed, and just as quiet.
    // A single raw character per core, straight at the UART, at three points on the way in.
    //
    // Every higher-level report has now failed to answer this question. The per-core `kprintln` lines
    // were shredded before the console had a lock; with the lock they can still be dropped by a
    // contended writer; and the one atomic line says only how many cores ARRIVED, not where the others
    // stopped. `put_byte` is a single store to the FIFO with nothing in front of it - no formatting, no
    // claim, no buffer to be lost - so a character that does not appear is a core that did not get
    // there, rather than a report that did not survive.
    //
    // `a`/`b`/`c` = entered the park, left the park, reached the scheduler. Three bytes per core is all
    // it takes to turn "one core is in the scheduler" into "core 2 stopped between the park and
    // `mark_ready`".
    AP_PROGRESS[core_id as usize & 3].store(1, Ordering::Release); // reached the park
    while AP_GO.load(Ordering::Acquire) == 0 {
        // SAFETY: `wfe` waits for the `sev` in `release_secondaries`. Spurious wakes just re-check.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
    }
    AP_PROGRESS[core_id as usize & 3].store(2, Ordering::Release); // left the park

    crate::kprintln!("smp: core {} online", core_id);

    // **Register this core's id before announcing ready.** The neutral `current_core_id` resolves a
    // core by searching the lapic-id table for its hardware id, and a core that is not in that table
    // falls through to `return 0` - so an unregistered core does not fail, it silently answers "I am
    // core 0" and starts using core 0's run queue, scheduler context and per-core state.
    //
    // On this SoC the hardware id IS the core index, so the mapping is the identity - which is exactly
    // why its absence was invisible on one core and catastrophic on four. The 32-bit port does the same
    // thing at the same point, one line before `mark_ready`, and comparing the two is what found it.
    crate::smp::core::set_core_lapic_id(core_id as u32, core_id as u32);
    crate::smp::core::mark_ready(core_id as u32);
    AP_PROGRESS[core_id as usize & 3].store(3, Ordering::Release); // in the scheduler

    // This core's own GIC interface and its own timer. The distributor is shared and already
    // configured; the CPU interface and the per-core timer are not.
    super::gic::init_secondary();
    super::timer::start(100);
    super::timer::unmask_irqs();
    super::interrupts::enable_interrupts();

    crate::task::scheduler::run(core_id as u32)
}
