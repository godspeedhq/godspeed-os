// SPDX-License-Identifier: GPL-2.0-only
//! Context-switch proof: two kernel tasks ping-ponging, then handing control back to boot.
//!
//! The 32-bit port used the same staging (`arch/arm/sched_demo.rs`), and for the same reason: the
//! context switch is the first piece that can corrupt state *silently*. A wrong offset or a missing
//! callee-saved register does not fault - it resumes a task with one register holding someone else's
//! value, and the damage surfaces later somewhere unrelated. So it gets proven on its own, with a
//! deliberately observable pattern, before anything is built on top of it.
//!
//! **What makes this a real test rather than a smoke test:** each task keeps a counter in `x19`-`x28`
//! and `d8`-`d15` territory (via locals the compiler pins to callee-saved registers across the switch
//! call), and checks on resume that it still holds what it left. A switch that dropped a register
//! would print a mismatch rather than merely failing to crash. A test that cannot fail is worth
//! nothing - the A9-1 lesson.

use super::context::TaskContext;
use super::{put_dec, put_str};

/// Stacks for the two demo tasks. 16 KiB each, in `.bss`, sized once and visible (§26.6.1).
const DEMO_STACK: usize = 16 * 1024;
static mut STACK_A: [u8; DEMO_STACK] = [0; DEMO_STACK];
static mut STACK_B: [u8; DEMO_STACK] = [0; DEMO_STACK];

static mut CTX_BOOT: TaskContext = TaskContext::empty();
static mut CTX_A: TaskContext = TaskContext::empty();
static mut CTX_B: TaskContext = TaskContext::empty();

/// How many times each task runs before handing back.
const ROUNDS: u64 = 3;

/// Set to true by task A when every round completed with its registers intact.
static mut CLEAN: bool = true;

extern "C" fn task_a() -> ! {
    // Values the compiler must keep across the `switch` call, i.e. in callee-saved registers. If the
    // switch fails to preserve them, these come back wrong.
    let mut counter: u64 = 0xA000_0000;
    let mut fp_witness: f64 = 1.5;

    for round in 0..ROUNDS {
        put_str(b"    [A] round ");
        put_dec(round);
        put_str(b"\r\n");
        counter += 1;
        fp_witness *= 2.0;

        // SAFETY: single-threaded demo; the contexts and stacks are statics owned by this module and
        // no other core is running.
        unsafe { super::context::switch(&raw mut CTX_A, &raw const CTX_B) };

        // Back from B. Everything below is the actual assertion.
        if counter != 0xA000_0000 + round + 1 {
            unsafe { CLEAN = false };
            put_str(b"    [A] CORRUPT: integer register not preserved across the switch\r\n");
        }
        if fp_witness != 1.5 * ((1u64 << (round + 1)) as f64) {
            unsafe { CLEAN = false };
            put_str(b"    [A] CORRUPT: d8-d15 not preserved across the switch\r\n");
        }
    }

    put_str(b"    [A] done, returning to boot\r\n");
    // SAFETY: as above; CTX_BOOT was saved by `run` before it switched here.
    unsafe { super::context::switch(&raw mut CTX_A, &raw const CTX_BOOT) };
    // `run` never switches back, so this is unreachable - but a task entry must not return.
    loop {
        // SAFETY: WFE is always valid.
        unsafe { core::arch::asm!("wfe") };
    }
}

extern "C" fn task_b() -> ! {
    let mut counter: u64 = 0xB000_0000;
    loop {
        put_str(b"    [B] resumed, counter=");
        put_dec(counter - 0xB000_0000);
        put_str(b"\r\n");
        counter += 1;
        // SAFETY: single-threaded demo, statics owned by this module.
        unsafe { super::context::switch(&raw mut CTX_B, &raw const CTX_A) };
    }
}

/// Run the demo. Returns true if every switch preserved the callee-saved state.
pub fn run() -> bool {
    // SAFETY: single-threaded boot. The stacks are `.bss` arrays owned here; taking the top address of
    // each is in-bounds (one past the end is a valid stack top, and `init` aligns it down).
    unsafe {
        let a_top = (&raw const STACK_A as u64) + DEMO_STACK as u64;
        let b_top = (&raw const STACK_B as u64) + DEMO_STACK as u64;
        (*(&raw mut CTX_A)).init(task_a, a_top);
        (*(&raw mut CTX_B)).init(task_b, b_top);

        put_str(b"aarch64: context-switch demo - two kernel tasks ping-ponging\r\n");
        // Save the boot context and jump into A. Control comes back here when A switches to CTX_BOOT.
        super::context::switch(&raw mut CTX_BOOT, &raw const CTX_A);

        CLEAN
    }
}
