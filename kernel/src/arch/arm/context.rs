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
//! **This is a cooperative switch: it is called, not forced.** Preemptive switching - swapping
//! contexts from inside the timer IRQ - needs the *full* register file saved, because an interrupt
//! can land between any two instructions with anything live. That is the next increment, and it
//! builds on this rather than replacing it.
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
