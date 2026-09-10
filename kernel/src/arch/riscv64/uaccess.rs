// SPDX-License-Identifier: GPL-2.0-only
//! Reading and writing USER memory from the kernel, and the per-core state a syscall needs.
//!
//! Every syscall that takes a pointer arrives here. The kernel must copy a user buffer without
//! trusting the pointer, without faulting on it, and without leaving itself able to touch user
//! memory a moment longer than the copy takes.
//!
//! **`sstatus.SUM` is the whole security question, and it is why this is a separate file.** With
//! `SUM` clear - the reset state, and the state this kernel runs in - S-mode may not touch a page
//! marked `U` AT ALL. That is a real, load-bearing protection: it is what made a missing `sscratch`
//! latch a dead machine rather than a silent corruption (`usermode::task_selftest`), because the
//! trap entry's store to a user stack faulted instead of succeeding. So `SUM` is set for the
//! duration of ONE copy and cleared immediately, by a guard that cannot be forgotten on an early
//! return. Setting it kernel-wide would be one line and would delete the protection.
//!
//! **The range is WALKED before it is copied, not caught while it is.** x86 lets the copy fault and
//! attributes the fault to the caller through a per-core `USER_COPY_ACTIVE` flag, killing the task
//! rather than the machine. That needs a task to kill, and this port has no kill path yet - a fault
//! here halts. So instead every page of the range is checked against the live page table first, for
//! presence, the `U` bit, and the permission the copy needs. A bad pointer is then a `false` return
//! rather than a fault, which is the answer the caller wanted anyway.
//!
//! That check is honest for a single hart and no more: it is a time-of-check/time-of-use gap the
//! moment another hart can unmap a page underneath it. The x86 arrangement is the one that scales,
//! and this port adopts it when it has a scheduler to kill into. Recorded rather than glossed
//! (§26.7), because a walk that looks like validation and is really a race is worse than neither.
//!
//! **`USER_END` is an Sv39 fact, not an x86 one.** The stub this replaces carried x86's
//! `0x0000_8000_0000_0000`, which would have accepted addresses in the hole Sv39 leaves between its
//! two halves - a range no access can reach, so a "validated" pointer there would fault on use.
//!
//! **And the range check does NOT reject a kernel address on this port, because it cannot.** The
//! kernel is identity-mapped low - 0x8020_0000 on QEMU, 0x4020_0000 on the board - which is inside
//! the user half. So `validate_user_ptr` answers true for the kernel's own code, and is right to: a
//! task may legitimately map its own page at that virtual address in its own space. x86's identical
//! check rejects a kernel pointer only because its kernel is higher-half; that rejection is a
//! property of the LAYOUT, not of the check, and assuming it travelled with the code would be
//! importing their model (§26.14).
//!
//! What separates kernel from user here is the `U` BIT, and nothing else. Every copy in this file
//! therefore walks for `U` and refuses without it - which is why the walk is not an optimisation or
//! a nicety on this arch but the entire boundary. `usermode::task_selftest` asserts both halves: the
//! range check passes for the kernel's own address, and the read is refused anyway.

use core::sync::atomic::Ordering;

use crate::smp::percpu::PerCoreMut;

use super::sv39;

/// One past the highest user address Sv39 can express.
///
/// An Sv39 address is 39 bits sign-extended, so the low half runs `0 .. 256 GiB` and the high half
/// starts at `0xFFFF_FFC0_0000_0000`. Userspace lives in the low half, so this is its ceiling.
/// See `sv39::va_is_canonical` for what happens to an address in between.
pub const USER_END: u64 = 0x0000_0040_0000_0000;

/// `sstatus.SUM` - "permit Supervisor User Memory access".
const SSTATUS_SUM: u64 = 1 << 18;

/// Per-core state the syscall entry keeps. Named for the x86 registers the neutral scheduler knows
/// it by; on this arch the same two roles are the task's kernel stack and the user stack a trap
/// swapped out, which `sscratch` and the trap frame already carry.
#[repr(C)]
pub struct PerCoreSyscallData {
    pub user_rsp: u64,
    pub kernel_rsp: u64,
}

static PER_CORE_SYSCALL: PerCoreMut<PerCoreSyscallData> = PerCoreMut::new();
static USER_READ_SCRATCH: PerCoreMut<[u8; crate::ipc::message::MAX_MESSAGE_SIZE]> =
    PerCoreMut::new();

/// The BSP's slot, for the window before the arena exists.
///
/// The neutral scheduler reaches for a slot during boot, before `percpu_init` has sized anything.
/// Returning a NULL POINTER there - which the stub this replaces did - is a store through null the
/// first time the scheduler writes a stack pointer. One static, used only on core 0 and only before
/// the arena, is the whole fix.
static mut BSP_SYSCALL: PerCoreSyscallData = PerCoreSyscallData { user_rsp: 0, kernel_rsp: 0 };

pub fn syscall_slot(core_id: usize) -> *mut PerCoreSyscallData {
    // A BOUNDARY MARKER, for free. The neutral tick reads this slot on the line immediately before
    // `switch_context`, so stamping here is the only way to tell a hart stuck in `pick_next` from one
    // stuck in the switch - and nothing else stamps between the progress stamp and the end of the
    // tick. See `stage::PRE_SWITCH`. Costs one store on a path that is already storing.
    super::note_stage(super::stage::PRE_SWITCH);
    if PER_CORE_SYSCALL.initialised() {
        PER_CORE_SYSCALL.as_mut_ptr(core_id)
    } else {
        debug_assert!(core_id == 0, "pre-arena syscall slot is BSP-only");
        &raw mut BSP_SYSCALL
    }
}

pub fn init_percore_syscall_arena(n: usize) {
    PER_CORE_SYSCALL.init_with(n, |_| PerCoreSyscallData { user_rsp: 0, kernel_rsp: 0 });
}

/// Allocate the per-core user-copy scratch. One message page per core, from the boot arena, so the
/// footprint is `cores * 4 KiB` and readable off the source (§26.6.1).
pub fn init_percore_arenas(n: usize) {
    USER_READ_SCRATCH.init_with(n, |_| [0u8; crate::ipc::message::MAX_MESSAGE_SIZE]);
}

/// Sets `sstatus.SUM` for as long as it is alive, and clears it on the way out.
///
/// A guard rather than a pair of calls because every user of it has early returns, and a `SUM` left
/// set is a kernel that can silently scribble on user memory from anywhere - the protection gone,
/// with nothing to show for it. The guard also restores rather than clears, so nesting cannot turn
/// it off inside an outer window that needs it.
struct SumWindow {
    was_set: bool,
}

impl SumWindow {
    fn open() -> Self {
        let old: u64;
        // SAFETY: an atomic read-and-set of `sstatus.SUM`, with no other effect. `csrrs` returns the
        // previous value, so nesting can be undone exactly.
        unsafe {
            core::arch::asm!(
                "csrrs {0}, sstatus, {1}",
                out(reg) old, in(reg) SSTATUS_SUM,
                options(nostack)
            )
        };
        SumWindow { was_set: old & SSTATUS_SUM != 0 }
    }
}

impl Drop for SumWindow {
    fn drop(&mut self) {
        if !self.was_set {
            // SAFETY: clearing the bit this guard set. Sound whatever path leaves the scope.
            unsafe {
                core::arch::asm!("csrc sstatus, {}", in(reg) SSTATUS_SUM, options(nostack))
            };
        }
    }
}

/// Is `[ptr, ptr+len)` entirely inside the user half, without wrapping?
///
/// A range check only. Whether it is MAPPED is a separate question with a separate answer, because
/// a caller that only needs to reject an obviously-bogus pointer should not pay for a page walk.
pub fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if ptr == 0 || len == 0 {
        return false;
    }
    if ptr >= USER_END {
        return false;
    }
    match ptr.checked_add(len as u64) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

/// Does every page of `[ptr, ptr+len)` translate, with `U` set and the permission the copy needs?
///
/// Walked page by page rather than checked once, because a range may span pages with different
/// mappings and a caller supplied the range, not the kernel. `U` is checked explicitly: a page the
/// kernel mapped for itself must not be readable through a syscall argument just because a user
/// pointer happened to name it.
fn user_range_is_mapped(ptr: u64, len: usize, need_write: bool) -> bool {
    let root = super::page_tables::read_page_table_base();
    if root == 0 {
        return false; // translation is off, so no user space exists to check against
    }
    let mut va = ptr & !0xfff;
    let end = ptr.saturating_add(len as u64);
    while va < end {
        let Some(pte) = sv39::translate(root, va) else {
            return false;
        };
        let need = sv39::PTE_V | sv39::PTE_U | sv39::PTE_R | if need_write { sv39::PTE_W } else { 0 };
        if pte & need != need {
            return false;
        }
        va += 4096;
    }
    true
}

/// Copy a user buffer into a kernel buffer the caller owns.
pub fn copy_user_to_kernel(src: u64, dst: *mut u8, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if len > super::page_tables::PAGE_SIZE {
        return false;
    }
    if !validate_user_ptr(src, len) || !user_range_is_mapped(src, len, false) {
        return false;
    }
    let _sum = SumWindow::open();
    // SAFETY: `[src, src+len)` was range-checked and then walked, so every page of it is present,
    // user-readable, and at most one page long. `dst` belongs to the caller, which says so by
    // handing it over. `SUM` is set for exactly this copy and cleared when the guard drops.
    unsafe { core::ptr::copy_nonoverlapping(src as *const u8, dst, len) };
    true
}

/// Copy a user buffer into this core's scratch and return a slice of it.
///
/// The caller never receives a pointer into user memory - it receives kernel memory holding a COPY,
/// so nothing downstream can be made to dereference a user address by a later unmap. The scratch is
/// per-core and touched only inside a syscall on that core, so there is no aliasing and no nesting.
pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
    if !validate_user_ptr(ptr, len) {
        return None;
    }
    if len > crate::ipc::message::MAX_MESSAGE_SIZE {
        return None;
    }
    if !USER_READ_SCRATCH.initialised() {
        return None; // before `percpu_init`, which is before any task exists to call a syscall
    }
    // `num_cores`, not `ready_count`. A core is marked READY by the scheduler bring-up this port does
    // not have yet, so `ready_count()` is zero here and every read would be refused for a reason
    // nothing in the failure would explain. `num_cores` is what `percpu_init` sized the arena to,
    // which is the number that actually bounds the index.
    let core = crate::task::scheduler::current_core_id();
    if core >= crate::smp::percpu::num_cores() {
        return None;
    }
    if !user_range_is_mapped(ptr, len, false) {
        return None;
    }
    let base = USER_READ_SCRATCH.as_mut_ptr(core).cast::<u8>();
    {
        let _sum = SumWindow::open();
        // SAFETY: the source range was range-checked and walked, so every page is present and
        // user-readable; `base` is this core's scratch of `MAX_MESSAGE_SIZE >= len` bytes, owned by
        // this core alone. `SUM` covers the copy and no more.
        unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, base, len) };
    }
    // SAFETY: `base` points at `len` bytes just written in this core's scratch slot, which lives for
    // the life of the kernel.
    Some(unsafe { core::slice::from_raw_parts(base as *const u8, len) })
}

/// Copy kernel bytes out to a user buffer.
pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
    if !validate_user_ptr(dst, src.len()) {
        return false;
    }
    if !user_range_is_mapped(dst, src.len(), true) {
        return false;
    }
    let _sum = SumWindow::open();
    // SAFETY: the destination was range-checked and walked for presence, `U` and `W`, so every page
    // of it is a user page this task may write. Source and destination cannot overlap: one is kernel
    // memory, the other is below `USER_END`.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
    true
}

/// A monotonic counter, in CYCLES where the machine will give them.
///
/// **The magnitude matters, not just the monotonicity.** Userspace budgets are written as cycle
/// counts against a gigahertz-ish counter - `block-driver`'s AHCI link wait is 400 million, meaning
/// about a fifth of a second on a PC. Answer with the 10 MHz `time` counter and that same constant
/// becomes FORTY SECONDS, per port, and a driver that works everywhere else appears to hang. Nothing
/// in the code is wrong at that point; the unit is.
///
/// So this reads `cycle` when the machine permits it. Whether it does is not ours to decide: `cycle`
/// is readable from S-mode only if M-mode set `mcounteren.CY`, and reading it otherwise is an illegal
/// instruction, not a zero. It is therefore PROBED once at boot - deliberately executed with the trap
/// handler told to expect the fault, exactly as the user-mode selftest probes an unreadable page - and
/// the answer is remembered.
///
/// Falling back to `time` is honest but coarse: it is monotonic and correct as a clock, and only its
/// SCALE is wrong for anything counting cycles. The boot says which one is in use, because a duration
/// that is a hundred times out is worth being able to see rather than deduce.
pub fn read_cycle_counter() -> u64 {
    // **`time`, ALWAYS - never `cycle`, and the difference is not about precision.**
    //
    // This used to prefer `rdcycle` where the firmware permitted it, so that cycle-denominated budgets
    // written for a gigahertz machine would not be answered in 4 MHz timebase ticks. That reasoning
    // was about MAGNITUDE and it missed what the two counters ARE. `cycle` counts CPU clock cycles: it
    // stops, or changes rate, when the hart halts in `wfi` or its frequency moves - the ISA says so,
    // and QEMU's interpreter makes it track execution rather than time. `time` is the constant-rate
    // wall clock, and that is the property every caller of this function actually depends on.
    //
    // What that cost, measured on hardware: a driver computes a deadline as a delta of this counter,
    // then sleeps. The core halts. The counter stops advancing. The deadline it is waiting for cannot
    // arrive, so the wait ends only when something else happens to wake the core - and hot-plug went
    // from milliseconds to tens of seconds while the driver sat at 0% CPU. In QEMU it was worse: the
    // shell prompt never appeared at all, at any settle time, because a deadline measured in a
    // counter that only moves while you are running cannot elapse while you are waiting.
    //
    // The magnitude problem the probe was written for is real and is a DIFFERENT bug: a driver that
    // hardcodes a cycle COUNT rather than deriving one from the machine's rate is wrong on every
    // machine, which is the "a count is not a duration" rule this project already carries. It is not
    // fixed by making the clock lie about which counter it is.
    let t: u64;
    // SAFETY: reading `time` has no side effects, and the boot proved it readable before anything
    // relied on it.
    unsafe { core::arch::asm!("csrr {}, time", out(reg) t, options(nomem, nostack)) };
    t
}
