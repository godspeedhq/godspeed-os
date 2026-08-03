// SPDX-License-Identifier: GPL-2.0-only
//! AArch64 context switch for the Raspberry Pi 4.
//!
//! Saves the **callee-saved** registers only, which is what AAPCS64 requires of a function that
//! behaves like a call. `switch_context` is reached by a `bl`, so the compiler has already spilled
//! anything caller-saved it cared about; duplicating that here would cost cycles on every switch to
//! preserve values nobody is going to read.
//!
//! Per AAPCS64 the callee-saved set is:
//!
//! - **`x19`-`x28`** - general purpose
//! - **`x29`** (frame pointer) and **`x30`** (link register)
//! - **`SP`**
//! - **`d8`-`d15`** - the low 64 bits of `v8`-`v15`
//!
//! **The `d8`-`d15` half is easy to skip and wrong to skip.** The kernel is compiled for a target with
//! FP/SIMD available, and LLVM emits NEON for bulk copies and struct moves without being asked; the
//! Pi 2 port hit exactly that with `memcpy`. Omitting them produces corruption that appears only when
//! a switch lands between a NEON spill and its reload - rare, load-dependent, and close to impossible
//! to attribute later. The eight extra pairs cost far less than that bug.
//!
//! Switching is *not* the same as taking an exception. The IRQ path in `exceptions.rs` saves the
//! **full** register set because it interrupts arbitrary code at an arbitrary instruction; this saves
//! only the callee-saved set because it happens at a call boundary the compiler already knows about.

use core::arch::global_asm;

/// A saved execution context. `#[repr(C)]` and the field order are load-bearing: the assembly below
/// addresses these by byte offset, so reordering the struct silently corrupts the switch.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct TaskContext {
    /// x19-x28, at offsets 0..80.
    pub x: [u64; 10],
    /// x29 (frame pointer), offset 80.
    pub fp: u64,
    /// x30 (link register) - where the switch returns to, offset 88.
    pub lr: u64,
    /// Stack pointer, offset 96.
    pub sp: u64,
    /// Padding, so the `d8`-`d15` pairs below stay 16-byte aligned. Offset 104.
    _pad: u64,
    /// d8-d15, at offsets 112..176.
    pub d: [u64; 8],
}

impl TaskContext {
    pub const fn empty() -> Self {
        TaskContext { x: [0; 10], fp: 0, lr: 0, sp: 0, _pad: 0, d: [0; 8] }
    }

    /// Prepare a context that, when switched to, begins executing `entry` on `stack_top`.
    ///
    /// `lr` is the whole trick: `switch_context` ends in `ret`, which jumps to whatever `lr` holds. A
    /// fresh context therefore just needs `lr` pointing at the entry function and `sp` at its stack,
    /// and the first switch into it "returns" into a function that was never called.
    ///
    /// `stack_top` is rounded down to a 16-byte boundary because AArch64 faults on a misaligned SP the
    /// moment anything uses it - a failure that surfaces as an unrelated-looking exception inside the
    /// new task rather than at the point the stack was set.
    pub fn init(&mut self, entry: extern "C" fn() -> !, stack_top: u64) {
        *self = TaskContext::empty();
        self.lr = entry as usize as u64;
        self.sp = stack_top & !0xF;
    }
}

global_asm!(
    r#"
    .section .text
    .globl aarch64_switch_context
// aarch64_switch_context(current: *mut TaskContext, next: *const TaskContext)
//   x0 = where to save the outgoing context, x1 = the context to resume.
// Returns into `next`'s lr, so from the caller's point of view this returns only when something
// switches back.
aarch64_switch_context:
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]
    mov  x2, sp
    str  x2,       [x0, #96]
    stp  d8,  d9,  [x0, #112]
    stp  d10, d11, [x0, #128]
    stp  d12, d13, [x0, #144]
    stp  d14, d15, [x0, #160]

    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]
    ldr  x2,       [x1, #96]
    mov  sp, x2
    ldp  d8,  d9,  [x1, #112]
    ldp  d10, d11, [x1, #128]
    ldp  d12, d13, [x1, #144]
    ldp  d14, d15, [x1, #160]
    ret
"#
);

extern "C" {
    fn aarch64_switch_context(current: *mut TaskContext, next: *const TaskContext);
}

/// Save the running context into `current` and resume `next`.
///
/// # Safety
/// Both pointers must be valid `TaskContext`s that outlive the switch, and `next` must either be a
/// context previously saved by this function or one prepared by [`TaskContext::init`] - anything else
/// resumes into a `ret` with garbage in `lr`. `next` must not alias `current`: switching a context to
/// itself would save over the registers mid-restore.
pub unsafe fn switch(current: *mut TaskContext, next: *const TaskContext) {
    // SAFETY: the caller's contract above.
    unsafe { aarch64_switch_context(current, next) }
}
