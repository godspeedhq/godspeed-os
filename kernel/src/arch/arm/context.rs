// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7-A kernel-mode context switch.
//!
//! The step where a ticking kernel becomes a *multitasking* one. Everything before this programmed
//! hardware and checked it responded; this saves and restores execution state, which is the
//! foundation the scheduler stands on.
//!
//! **Only ten registers need saving, and that is not a shortcut.** AAPCS divides the register file
//! into caller-saved (`r0-r3`, `r12`) and callee-saved (`r4-r11`, `sp`, `lr`). Because a switch
//! happens inside an ordinary function call, the compiler has *already* spilled anything live in the
//! caller-saved set at the call site. So the switch is responsible only for the callee-saved half -
//! exactly as the x86 side saves its callee-saved registers and CR3 and no more.
//!
//! **Both halves live here.** The cooperative switch above is *called*, so AAPCS lets it save ten
//! registers. The preemptive switch below is *forced* from the timer IRQ, where an interrupt can land
//! between any two instructions with anything live - so it saves the entire register file plus the
//! resume PC and `SPSR` as a **trap frame on the interrupted task's own stack**. That placement is
//! what makes a task switch cheap: the state is already parked where it belongs, so switching tasks
//! is switching `sp`, and the scheduler need only return a different frame pointer.
//!
//! No address-space switch here either. Every context shares the one identity mapping from `mmu.rs`;
//! per-task page tables (a `TTBR0` write plus the TLB maintenance the SEC-26/27 port contract
//! demands) arrive with real tasks.

use core::sync::atomic::{AtomicU32, Ordering};

use super::pl011_write;
use super::timer::write_dec_pub;

/// Saved execution state: the callee-saved registers, in the order `stm`/`ldm` transfer them.
///
/// **Field order is load-bearing.** `stmia`/`ldmia` always transfer in increasing register number
/// regardless of how the register list is written, so this must read `r4..r11`, then `sp` (r13), then
/// `lr` (r14). `repr(C)` pins the layout; reordering these fields would silently corrupt every
/// switch, restoring registers into the wrong slots.
#[repr(C)]
#[derive(Default)]
pub struct Context {
    pub r4: u32,
    pub r5: u32,
    pub r6: u32,
    pub r7: u32,
    pub r8: u32,
    pub r9: u32,
    pub r10: u32,
    pub r11: u32,
    pub sp: u32,
    pub lr: u32,
}

impl Context {
    pub const fn new() -> Self {
        Self { r4: 0, r5: 0, r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0, sp: 0, lr: 0 }
    }

    /// Prepare a context that has never run, so the first switch into it starts at `entry`.
    ///
    /// The trick is that a switch ends by branching to the restored `lr`. A context that *has* run
    /// carries the address after its own `switch()` call; a fresh one is simply handed `entry`
    /// instead, so the same restore path starts it from the beginning. No special-casing in the
    /// switch itself.
    ///
    /// `stack_top` is rounded down to an 8-byte boundary, which AAPCS requires at any public
    /// interface - and which a misaligned static would otherwise quietly violate.
    pub fn prepare(&mut self, entry: extern "C" fn() -> !, stack_top: usize) {
        self.sp = (stack_top & !7) as u32;
        self.lr = entry as usize as u32;
    }
}

/// Save the current callee-saved state into `from`, restore `to`, and continue as `to`.
///
/// Three instructions, and the third is the switch itself: `bx lr` branches to the *restored* `lr`,
/// so control resumes wherever `to` last left off (or at its entry point, for a fresh context).
///
/// # Safety
/// Both pointers must be valid, aligned `Context`s. `to` must hold either state saved by a previous
/// call here or a context set up by [`Context::prepare`] - anything else transfers control to a
/// fabricated address with a fabricated stack. `to`'s stack must still be live: switching to a
/// context whose stack has been freed or reused is a use-after-free of execution state.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn arm_context_switch(from: *mut Context, to: *const Context) {
    core::arch::naked_asm!(
        "stmia r0, {{r4-r11, sp, lr}}", // save callee-saved into *from
        "ldmia r1, {{r4-r11, sp, lr}}", // restore *to
        "bx    lr",                     // resume `to` where it left off
    )
}

// ---- Self-test: two kernel contexts ping-ponging ----

/// 4 KiB stack for the test context. Fixed and static, in BSS - no allocator exists yet, and a fixed
/// bound is the point (§26.6.1) rather than a limitation to apologise for.
#[repr(align(8))]
struct TestStack([u8; 4096]);
static mut TEST_STACK: TestStack = TestStack([0; 4096]);

static mut MAIN_CTX: Context = Context::new();
static mut TEST_CTX: Context = Context::new();

/// Switches performed *by the test context*. The main side counts its own, so a mismatch tells us
/// which direction failed rather than merely that something did.
static YIELDS: AtomicU32 = AtomicU32::new(0);

const ROUNDS: u32 = 10;

/// The test context: bump a counter, hand control back, repeat. Never returns - a context that ran
/// off its entry function would return into whatever `lr` happened to hold.
extern "C" fn test_context_entry() -> ! {
    loop {
        YIELDS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: Both contexts are statics that outlive every switch, and MAIN_CTX holds state saved
        // by the switch that entered here, so returning to it resumes a live stack. Single-threaded:
        // the other cores are parked and preemption does not exist yet.
        unsafe {
            arm_context_switch(core::ptr::addr_of_mut!(TEST_CTX), core::ptr::addr_of!(MAIN_CTX));
        }
    }
}

/// Ping-pong between two kernel contexts and check both sides ran the expected number of times.
///
/// A switch that half-works is the dangerous case - control transfers but registers come back
/// corrupted - so this checks more than "we got here": the counter is incremented *by the other
/// context* and read back here, which requires the round trip to preserve state in both directions.
pub fn selftest() {
    // SAFETY: Single-threaded boot context (secondaries parked, no preemption). The statics below are
    // touched only here and in `test_context_entry`, which cannot run until we switch to it.
    unsafe {
        let stack_top = core::ptr::addr_of!(TEST_STACK) as usize + core::mem::size_of::<TestStack>();
        (*core::ptr::addr_of_mut!(TEST_CTX)).prepare(test_context_entry, stack_top);

        for _ in 0..ROUNDS {
            arm_context_switch(core::ptr::addr_of_mut!(MAIN_CTX), core::ptr::addr_of!(TEST_CTX));
        }
    }

    let yields = YIELDS.load(Ordering::Relaxed);
    pl011_write(b"arm32: context selftest - ");
    write_dec_pub(yields);
    pl011_write(b" round trips of ");
    write_dec_pub(ROUNDS);
    pl011_write(b"\r\n");

    if yields == ROUNDS {
        pl011_write(b"arm32: context selftest PASS (two kernel contexts switch and resume)\r\n");
    } else {
        pl011_write(b"arm32: context selftest FAIL - the switch did not round-trip cleanly\r\n");
    }
}

// ============================ Preemptive switching ============================

/// The full interrupted state, as `stub_irq` lays it out on the task's own stack.
///
/// **Field order mirrors the push order exactly** and is as load-bearing as `Context`'s: the two
/// USER-banked words (`SP_usr`/`LR_usr`, saved via `stmdb {sp, lr}^`) sit lowest, then `push {r0-r12,
/// lr}` stores in increasing register number, and the `srsdb` above leaves the resume PC then `SPSR`
/// at the top. Get this wrong and tasks resume with scrambled registers - random corruption far from
/// the cause. 18 words / 72 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TrapFrame {
    pub usr_sp: u32,  // SP_usr - the interrupted task's USER-banked stack pointer
    pub usr_lr: u32,  // LR_usr - the interrupted task's USER-banked link register
    pub r: [u32; 13], // r0-r12
    pub lr: u32,      // LR_svc - the interrupted task's own link register
    pub pc: u32,      // resume address (LR_irq, already adjusted by -4)
    pub spsr: u32,    // the interrupted CPSR: mode + interrupt masks
}

/// CPSR for a fresh kernel task: SVC mode, IRQs **enabled** (so it can itself be preempted), FIQ
/// masked. A task started with IRQs disabled would run to completion and never yield - preemption
/// silently dead, with no error anywhere.
const SPSR_SVC_IRQ_ON: u32 = 0x13 | 0x40;

const MAX_TASKS: usize = 3;
const TASK_STACK_SIZE: usize = 4096;

#[repr(align(8))]
struct TaskStacks([[u8; TASK_STACK_SIZE]; MAX_TASKS]);
static mut TASK_STACKS: TaskStacks = TaskStacks([[0; TASK_STACK_SIZE]; MAX_TASKS]);

/// Per-task saved frame pointer. A task is "saved" precisely when its `sp` is recorded here; the rest
/// of its state is already sitting on its own stack, which is the elegance of the trap-frame approach
/// - there is no separate register-save area to manage.
static TASK_SP: [AtomicU32; MAX_TASKS] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

/// How many times each task has been scheduled - the selftest's evidence that round-robin actually
/// rotates rather than favouring one task.
static TASK_RUNS: [AtomicU32; MAX_TASKS] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];

static CURRENT: AtomicU32 = AtomicU32::new(0);
static PREEMPT_ON: AtomicU32 = AtomicU32::new(0);

/// Build a trap frame on a fresh task's stack so the ordinary `pop` + `rfeia` path starts it.
///
/// The same trick as `Context::prepare`, one layer down: rather than special-casing "has never run"
/// inside the switch, fabricate the state the switch expects to find.
fn prepare_task(index: usize, entry: extern "C" fn() -> !) {
    // SAFETY: Boot-time, single-threaded, before preemption is armed. Each task's stack is a distinct
    // static slice; we write one frame at its top and record the pointer. `addr_of_mut!` avoids
    // taking a reference to a `static mut`.
    unsafe {
        let base = core::ptr::addr_of_mut!(TASK_STACKS.0[index]) as usize;
        let top = (base + TASK_STACK_SIZE) & !7;
        let frame_addr = top - core::mem::size_of::<TrapFrame>();
        let frame = frame_addr as *mut TrapFrame;
        (*frame) = TrapFrame {
            // Kernel task: the USER-banked sp/lr are unused (it runs in SVC), so zero them. They are
            // restored into the System/USER bank on entry and never read, which is harmless.
            usr_sp: 0,
            usr_lr: 0,
            r: [0; 13],
            lr: task_returned as usize as u32,
            pc: entry as usize as u32,
            spsr: SPSR_SVC_IRQ_ON,
        };
        TASK_SP[index].store(frame_addr as u32, Ordering::Relaxed);
    }
}

/// A kernel task ran off the end of its entry function. Nothing can recover it - there is no task
/// table to reap into yet - so stop loudly rather than branching somewhere undefined.
extern "C" fn task_returned() -> ! {
    pl011_write(b"arm32: a kernel task returned from its entry function - halting\r\n");
    loop {
        // SAFETY: WFI is always architecturally valid.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Round-robin, called from the timer IRQ with the outgoing task's frame.
///
/// Saves the current task's `sp`, picks the next, returns *its* `sp`. `stub_irq` adopts whatever
/// comes back, so returning a different frame is precisely what makes the switch happen.
pub(super) fn schedule(frame_sp: u32) -> u32 {
    if PREEMPT_ON.load(Ordering::Relaxed) == 0 {
        return frame_sp; // not armed: resume exactly what we interrupted
    }

    let cur = CURRENT.load(Ordering::Relaxed) as usize;
    TASK_SP[cur].store(frame_sp, Ordering::Relaxed);

    let next = (cur + 1) % MAX_TASKS;
    CURRENT.store(next as u32, Ordering::Relaxed);
    TASK_RUNS[next].fetch_add(1, Ordering::Relaxed);

    TASK_SP[next].load(Ordering::Relaxed)
}

// Demo task bodies. Each spins forever; the TIMER takes control away, which is the whole point -
// none of them cooperates, yields, or is aware the others exist.
extern "C" fn task_a() -> ! { loop { core::hint::spin_loop(); } }
extern "C" fn task_b() -> ! { loop { core::hint::spin_loop(); } }
extern "C" fn task_c() -> ! { loop { core::hint::spin_loop(); } }

/// Prove preemption: start three non-cooperating tasks and check the timer rotates between them.
///
/// The bar is "all three ran", not "something ran". A switch that always picked the same task, or
/// that worked once and then wedged, would still show a task running - checking that EVERY task was
/// scheduled is what requires a working rotation.
pub fn preempt_selftest() {
    prepare_task(0, task_a);
    prepare_task(1, task_b);
    prepare_task(2, task_c);

    // Slot 0 is notionally current, so the first preemption saves THIS boot context into it and moves
    // on; boot's own state is what slot 0 carries from then on.
    CURRENT.store(0, Ordering::Relaxed);
    PREEMPT_ON.store(1, Ordering::Relaxed);

    // Let the tick rotate for a while. This delay is itself preempted - which is the point.
    super::timer::delay_us(300_000);

    PREEMPT_ON.store(0, Ordering::Relaxed);

    let runs = [
        TASK_RUNS[0].load(Ordering::Relaxed),
        TASK_RUNS[1].load(Ordering::Relaxed),
        TASK_RUNS[2].load(Ordering::Relaxed),
    ];

    pl011_write(b"arm32: preempt selftest - task runs: ");
    let mut first = true;
    for r in runs.iter() {
        if !first { pl011_write(b" / "); }
        first = false;
        write_dec_pub(*r);
    }
    pl011_write(b"\r\n");

    if runs.iter().all(|&r| r > 0) {
        pl011_write(b"arm32: preempt selftest PASS (timer rotates between tasks that never yield)\r\n");
    } else {
        pl011_write(b"arm32: preempt selftest FAIL - not every task was scheduled\r\n");
    }
}
