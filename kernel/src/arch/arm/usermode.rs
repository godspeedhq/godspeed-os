// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7 user mode (PL0) - the increment where a task first runs UNPRIVILEGED.
//!
//! Everything so far ran in SVC (PL1, kernel-privileged). This is the pivot: enter USR mode, run
//! code that cannot touch kernel memory, and have it `svc` back in - the page-table USER permissions
//! (increment on `page_tables.rs`) and the SVC entry (`syscall.rs`) meeting for the first time.
//!
//! **Entering USR mode on ARMv7 is a fabricated exception return.** There is no `iret`; instead you
//! set `SPSR` to the target mode (USR, IRQs enabled), set `LR` to the entry PC, arrange the USR
//! banked `SP`, and `movs pc, lr` - which copies `SPSR -> CPSR` and `LR -> PC` atomically, dropping
//! privilege. This is the ARM analogue of x86's IRETQ-to-ring-3.
//!
//! **The proof that it is real is the saved `SPSR` at the `svc`.** Running the code proves nothing
//! about privilege; the CPU records the caller's mode in `SPSR_svc`, and `SPSR.mode == USR (0x10)` is
//! unforgeable evidence the code executed at PL0. The selftest checks exactly that, and separately
//! probes the page permissions with the *unprivileged*-access translation ops (`ATS1CPUR`/`ATS1CPUW`)
//! - the PL0 counterpart of the page-table selftest's privileged probes.
//!
//! **Getting back out** without a scheduler: `enter_user` saves the kernel (boot) callee-saved state
//! and `sp`/`lr` into `RESUME` first; the magic `svc` restores them and branches back, so
//! `enter_user` "returns" to its caller even though the user task never falls off its own code.

use core::sync::atomic::{AtomicU32, Ordering};

use super::pl011_write;
use super::exceptions::write_hex32;
use super::context::Context;
use super::page_tables::{map_in_active_tables, PageFlags};

/// Syscall number the user stub raises to signal "I ran; hand control back". Chosen far outside the
/// real syscall range so it can never collide with a genuine call.
pub const USER_TEST_SVC: u32 = 0x5555_0001;

/// The CPSR the SVC entry saw at the last `svc` - stored by the entry stub so the selftest can read
/// the caller's privilege mode. `SPSR.mode` low 5 bits: USR = 0x10, SVC = 0x13.
#[no_mangle]
pub static mut USER_SPSR_SAVE: u32 = 0;

/// The kernel context to resume when the magic `svc` fires - saved by `enter_user`, restored by
/// `resume_boot`. Layout matches `Context` (r4-r11, sp, lr) so the same `ldmia` restores it.
static mut RESUME: Context = Context::new();

/// Set to the caller mode (`SPSR & 0x1f`) observed at the magic `svc`. `0x10` means USR - the proof.
static ENTERED_MODE: AtomicU32 = AtomicU32::new(0);

// Two spare VAs for the user pages, chosen ABOVE every identity-mapped region so the L1 slot is always
// empty (map_in_active_tables refuses to clobber a live section). The mapped regions are RAM (0..RAM_END),
// the framebuffer (dynamic, in the RAM/peripheral gap - QEMU lands it ~0x3C00_0000, which the old
// 0x3C10_0000 collided with, failing the selftest under QEMU while passing on HW), peripherals
// (0x3F00_0000..0x4000_0000), and core-local (0x4000_0000..0x4100_0000). 0x5000_0000 is clear of all of
// them on both QEMU and real hardware.
const USER_CODE_VA: u32 = 0x5000_0000;
const USER_STACK_VA: u32 = 0x5000_1000;

/// The user stub, PL0 code: raise the magic syscall, then spin (the kernel takes over before this
/// spins for real). `naked` so it is pure position-independent instructions we can copy into a page.
///
/// `movw`/`movt` build the 32-bit magic number without a literal pool (which would not be copied
/// along with the code).
#[unsafe(naked)]
unsafe extern "C" fn user_stub() -> ! {
    core::arch::naked_asm!(
        "movw r0, #0x0001",   // r0 = USER_TEST_SVC low half
        "movt r0, #0x5555",   //      ... high half
        "svc  #0",
        "1:",
        "b    1b",
    )
}

/// Length of the stub to copy (4 instructions x 4 bytes, plus slack rounded to a cache line).
const STUB_LEN: usize = 64;

/// Save the kernel context, then drop to USR mode at `entry` with stack `stack_top`.
///
/// `naked` and hand-written because it must not touch the stack between saving `sp` and the mode
/// switch. `r0 = entry`, `r1 = stack_top`.
#[unsafe(naked)]
unsafe extern "C" fn enter_user(entry: u32, stack_top: u32) {
    core::arch::naked_asm!(
        "ldr   r2, ={resume}",
        "stmia r2, {{r4-r11}}",       // save kernel callee-saved
        "str   sp, [r2, #0x20]",      // save kernel sp
        "str   lr, [r2, #0x24]",      // save return address (where enter_user "returns" to)
        "cps   #0x1f",                // system mode: shares the USR banked SP
        "mov   sp, r1",               // set the user stack
        "cps   #0x13",                // back to SVC
        "mov   r3, #0x10",            // SPSR = USR mode, I+F clear (interrupts enabled)
        "msr   spsr_cxsf, r3",
        "mov   lr, r0",               // LR = user entry
        "movs  pc, lr",               // drop to PL0: CPSR <- SPSR, PC <- entry
        resume = sym RESUME,
    )
}

/// Restore the kernel context saved by `enter_user` and branch back to its caller. Called from the
/// magic-`svc` path; never returns to the SVC handler.
#[unsafe(naked)]
pub(super) unsafe extern "C" fn resume_boot() -> ! {
    core::arch::naked_asm!(
        "ldr   r2, ={resume}",
        "ldmia r2, {{r4-r11}}",
        "ldr   sp, [r2, #0x20]",
        "ldr   lr, [r2, #0x24]",
        "bx    lr",
        resume = sym RESUME,
    )
}

/// Called from `arm_svc_dispatch` when the magic syscall fires: record the mode the caller was in,
/// then resume the kernel. Returns `!` via `resume_boot`.
pub(super) fn on_magic_svc() -> ! {
    // SAFETY: `USER_SPSR_SAVE` was written by the SVC entry stub for this very trap.
    let spsr = unsafe { core::ptr::addr_of!(USER_SPSR_SAVE).read_volatile() };
    ENTERED_MODE.store(spsr & 0x1f, Ordering::Relaxed);
    // SAFETY: `RESUME` holds the kernel state saved by `enter_user`; restoring it returns to boot.
    unsafe { resume_boot() }
}

/// Unprivileged translation probe (`ATS1CPUR` = user read, or `ATS1CPUW` = user write); `None` if the
/// access is not permitted at PL0. The PL0 counterpart of the page-table selftest's privileged probes,
/// and non-faulting for the same reason: the result lands in `PAR.F`, not an exception.
fn translate_user(va: u32, write: bool) -> Option<u32> {
    let par: u32;
    // SAFETY: ATS1CPUR (`c7, c8, 2`) / ATS1CPUW (`c7, c8, 3`) run an unprivileged-access translation
    // with no memory side effects; a denied access sets PAR.F rather than faulting.
    unsafe {
        if write {
            core::arch::asm!("mcr p15, 0, {v}, c7, c8, 3", "isb", "mrc p15, 0, {p}, c7, c4, 0",
                v = in(reg) va, p = out(reg) par, options(nostack));
        } else {
            core::arch::asm!("mcr p15, 0, {v}, c7, c8, 2", "isb", "mrc p15, 0, {p}, c7, c4, 0",
                v = in(reg) va, p = out(reg) par, options(nostack));
        }
    }
    if par & 1 != 0 { None } else { Some((par & 0xFFFF_F000) | (va & 0xFFF)) }
}

/// Clean a code page to the PoU and invalidate the I-cache for it, so freshly-copied instructions
/// execute. Writing code as data leaves it in the D-cache and stale in the I-cache; this is the
/// instruction-side twin of `page_tables::clean_dcache`.
fn sync_icache(addr: u32, len: u32) {
    let mut p = addr & !31;
    let end = addr + len;
    while p < end {
        // SAFETY: DCCMVAU (`c7,c11,1`) cleans D-cache to PoU; ICIMVAU (`c7,c5,1`) invalidates I-cache
        // by MVA. Both are side-effect-free maintenance ops on a page we own.
        unsafe {
            core::arch::asm!("mcr p15, 0, {a}, c7, c11, 1", "mcr p15, 0, {a}, c7, c5, 1",
                a = in(reg) p, options(nostack));
        }
        p += 32;
    }
    // SAFETY: barriers order the maintenance before the code is fetched.
    unsafe { core::arch::asm!("dsb", "isb", options(nostack)) }
}

/// Prove PL0: map user code + stack with USER permissions, check the permission model with the
/// unprivileged probes, then actually enter USR mode and confirm - via the saved SPSR - that the code
/// ran unprivileged.
pub fn selftest() {
    use crate::memory::allocator::alloc_frame;

    let (code_frame, stack_frame) = match (alloc_frame(), alloc_frame()) {
        (Some(c), Some(s)) => (c.phys_addr().0 as u32, s.phys_addr().0 as u32),
        _ => { pl011_write(b"arm32: usermode FAIL - no frames for user pages\r\n"); return; }
    };

    // Copy the stub into the code frame (identity-mapped, so writable at its physical address), then
    // make it executable from the I-cache's point of view.
    // SAFETY: `code_frame` is a fresh frame we own; copying STUB_LEN bytes of position-independent
    // stub code into it and syncing the caches is exactly the ELF-loader pattern in miniature.
    unsafe {
        core::ptr::copy_nonoverlapping(user_stub as *const u8, code_frame as *mut u8, STUB_LEN);
    }
    sync_icache(code_frame, STUB_LEN as u32);

    // Map: code USER + executable (no WRITABLE, no NO_EXEC); stack USER + WRITABLE + NO_EXEC.
    let code_flags = PageFlags::PRESENT | PageFlags::USER;
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    // SAFETY: mapping into the active tables at VAs in the unmapped gap; single-threaded boot.
    let m1 = unsafe { map_in_active_tables(USER_CODE_VA as u64, code_frame as u64, code_flags.bits()) };
    let m2 = unsafe { map_in_active_tables(USER_STACK_VA as u64, stack_frame as u64, stack_flags.bits()) };
    if m1.is_err() || m2.is_err() {
        pl011_write(b"arm32: usermode FAIL - could not map user pages\r\n");
        return;
    }

    let mut pass = true;

    // Permission model, via unprivileged probes: user code is user-readable; user stack is
    // user-writable; and a KERNEL page (the identity-mapped low RAM) is NOT user-accessible.
    if translate_user(USER_CODE_VA, false).is_none() {
        pl011_write(b"arm32:   user code page is not user-readable\r\n"); pass = false;
    }
    if translate_user(USER_STACK_VA, true).is_none() {
        pl011_write(b"arm32:   user stack page is not user-writable\r\n"); pass = false;
    }
    if translate_user(0x0010_0000, false).is_some() {
        pl011_write(b"arm32:   a KERNEL page is user-accessible - isolation broken\r\n"); pass = false;
    }

    // Now actually run at PL0. enter_user drops to USR at the stub; the stub raises USER_TEST_SVC;
    // the SVC handler records the caller mode and resumes us here.
    // SAFETY: the code/stack pages are mapped USER; enter_user saves the kernel context and returns
    // via the magic svc. Single-threaded; interrupts are enabled in USR so the tick still runs.
    unsafe { enter_user(USER_CODE_VA, USER_STACK_VA + 0x1000); }

    let mode = ENTERED_MODE.load(Ordering::Relaxed);
    pl011_write(b"arm32: usermode - svc taken from CPSR mode ");
    write_hex32(mode);
    pl011_write(b" (0x10 = USR = unprivileged)\r\n");
    if mode != 0x10 {
        pl011_write(b"arm32:   the stub did not run at PL0\r\n"); pass = false;
    }

    if pass {
        pl011_write(b"arm32: usermode PASS (code ran at PL0; USER perms enforced; svc back to kernel)\r\n");
    } else {
        pl011_write(b"arm32: usermode FAIL - see above\r\n");
    }
}
