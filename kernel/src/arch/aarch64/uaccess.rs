// SPDX-License-Identifier: GPL-2.0-only
//! The user-pointer copy seam: where the kernel touches memory a task chose the address of.
//!
//! This is the boundary a syscall ABI lives or dies on. A task passes a pointer; the kernel must not
//! trust it, must not be crashed by it, and must not be tricked into reading or writing its own memory
//! through it. Three separate defences, because each catches what the others cannot:
//!
//! 1. **A range check** ([`validate_user_ptr`]) - the pointer and its whole length must lie inside the
//!    user half, with the addition checked for overflow. Cheap, and rejects the obvious attack of
//!    handing the kernel one of its own addresses.
//! 2. **`ldtr` / `sttr`, not `ldr` / `str`** - the *unprivileged* load and store. Executed at EL1 they
//!    perform the access **with EL0 permissions**, so the hardware itself refuses a kernel address even
//!    if the range check were wrong. This is defence in depth that costs nothing: the same instruction
//!    count, the same speed, and a bug in check (1) stops being exploitable.
//! 3. **A fault fixup** - a range-valid pointer can still be unmapped, and the resulting data abort
//!    lands at EL1 where it looks exactly like a kernel bug. Without a fixup the machine halts, which
//!    any service could trigger at will. The helpers below register a recovery address around *only*
//!    the faulting instruction, and `aarch64_sync_current_dispatch` resumes there, turning a bad
//!    pointer into a failed syscall instead of a dead machine.
//!
//! Defence 2 is the one worth dwelling on. `validate_user_ptr` is a range check and nothing more - it
//! cannot know whether a page is mapped, and on a port where the kernel shares no addresses with the
//! task it also cannot know whether the mapping belongs to the caller. `ldtr`/`sttr` closes that by
//! asking the MMU the same question EL0 would have asked.
//!
//! ## Why the data is copied rather than borrowed
//!
//! [`read_user_bytes`] copies into a per-core kernel buffer and returns a slice to *that*. Handing the
//! rest of the kernel a pointer into user memory would leave every subsequent read racing the task
//! itself: a second thread (or the task after a preemption) can change the bytes between the kernel's
//! validation and its use. Copying once makes the kernel's view immutable, which is the only version
//! of the check that means anything.

use core::sync::atomic::Ordering;
// `portable_atomic`, not `core` - the repo-wide rule that keeps the kernel word-size portable
// (`arch/CLAUDE.md`). Nothing here is 32-bit yet, but the rule exists so nobody has to remember.
use portable_atomic::AtomicU64;

/// The BCM2711 has exactly four Cortex-A72 cores, so per-core state here is a fixed array indexed by
/// `MPIDR_EL1.Aff0`.
///
/// **Deliberately not the general per-core arena**, and the reason is a bug this cost. The arena is
/// allocated at boot and indexed via `current_core_id()`, which needs tables that are not up yet when
/// the first user copy runs - and, far worse, the *fault handler* calls into this module. Indexing an
/// unallocated arena from a fault handler faults again, and the second fault happens while reporting
/// the first, so the machine goes completely silent with nothing printed at all.
///
/// A fault handler must not depend on initialisation order. Reading `MPIDR_EL1` needs no setup, cannot
/// fail, and is correct from the first instruction after reset.
const MAX_CORES: usize = 4;

#[inline]
fn core_index() -> usize {
    let mpidr: u64;
    // SAFETY: reading MPIDR_EL1 at EL1 is a side-effect-free system-register read, valid at any time.
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    ((mpidr & 0xFF) as usize).min(MAX_CORES - 1)
}

/// One past the highest address a task may name. Matches `syscall_entry::USER_END`.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// Largest single user copy. A syscall message is one page (§8.5), so this bounds the staging buffer at
/// a fixed, visible size (§26.6.1) rather than letting a caller name a length and get an arena to match.
pub const MAX_USER_COPY: usize = 4096;

/// Per-core staging buffer for [`read_user_bytes`]. Per-core because two cores may be in syscalls at
/// once and a shared buffer would let one overwrite the other's argument mid-call.
static mut STAGE: [[u8; MAX_USER_COPY]; MAX_CORES] = [[0; MAX_USER_COPY]; MAX_CORES];

/// Per-core fault fixup: the address to resume at if the in-progress user copy faults, or 0 for "no
/// copy in progress, so a fault here is a real kernel bug".
#[allow(clippy::declare_interior_mutable_const)]
const FIXUP_INIT: AtomicU64 = AtomicU64::new(0);
static FIXUP: [AtomicU64; MAX_CORES] = [FIXUP_INIT; MAX_CORES];

/// How many user-copy faults have been recovered. Reported by the selftest so "the unmapped pointer
/// survived" is backed by evidence that the fixup FIRED, rather than by something upstream having
/// quietly rejected the pointer before any access happened.
static RECOVERED: AtomicU64 = AtomicU64::new(0);

/// Number of user-copy faults recovered so far.
pub fn recovered_count() -> u64 { RECOVERED.load(Ordering::Relaxed) }

/// Range-check a user pointer. **A range check only** - see the module header for what it cannot know.
pub fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if ptr == 0 || len == 0 {
        return false;
    }
    if len > MAX_USER_COPY {
        return false;
    }
    if ptr >= USER_END {
        return false;
    }
    // Checked, because `ptr + len` wrapping is exactly how a caller would try to make a huge length
    // look like a small one.
    match ptr.checked_add(len as u64) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

/// Consume the current core's fault fixup, if a user copy is in progress.
///
/// Called from the same-EL synchronous handler. Consuming rather than peeking means a second fault
/// after recovery is treated as a genuine kernel bug, so a broken helper cannot loop silently.
pub fn take_fixup() -> Option<u64> {
    match FIXUP[core_index()].swap(0, Ordering::AcqRel) {
        0 => None,
        addr => {
            RECOVERED.fetch_add(1, Ordering::Relaxed);
            Some(addr)
        }
    }
}

/// Copy `len` bytes from a user address into this core's staging buffer.
///
/// Returns a slice of the copy, or `None` if the pointer failed validation or faulted. The `'static`
/// lifetime reflects the buffer's storage, not an unlimited borrow: it is valid until this core's next
/// user copy, and callers consume it within the syscall that produced it.
pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
    if !validate_user_ptr(ptr, len) {
        return None;
    }
    // SAFETY: this core's staging slot, and interrupts do not re-enter a user copy on the same core (a
    // syscall runs with the caller's core to itself until it returns).
    let dst = unsafe { (&raw mut STAGE[core_index()]) as *mut u8 };
    if copy_from_user(dst, ptr, len) {
        // SAFETY: `len <= MAX_USER_COPY` was checked, and `copy_from_user` filled exactly that many
        // bytes of this core's buffer.
        Some(unsafe { core::slice::from_raw_parts(dst as *const u8, len) })
    } else {
        None
    }
}

/// Copy `src` to a user address. Returns false if the pointer failed validation or faulted.
pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
    if !validate_user_ptr(dst, src.len()) {
        return false;
    }
    copy_to_user(dst, src.as_ptr(), src.len())
}

/// Read one 32-bit MMIO register, tolerating a **synchronous external abort**.
///
/// Not every address the kernel believes is a device actually decodes to one. The BCM2711's PCIe root
/// complex is at a fixed address on a real Pi 4 and at *nothing* under QEMU's `raspi4b`, and the
/// difference does not show up as a translation fault - the mapping is perfectly valid - but as an
/// external abort from the interconnect, which at EL1 is indistinguishable from a kernel bug and halts
/// the machine. That is the right default and the wrong answer for a **probe**, whose entire question
/// is "is anything there?".
///
/// So this reuses the user-copy seam's fixup for exactly one instruction: `None` means the read
/// aborted, and the caller degrades (no USB, still a serial console) instead of taking the boot down.
/// The window is one load wide, so a fault anywhere else still halts loudly - which is the property
/// that makes this a probe rather than a blanket "ignore kernel faults".
///
/// # Safety
/// `addr` must be 4-byte aligned and inside a Device mapping the kernel owns.
pub unsafe fn probe_read32(addr: u64) -> Option<u32> {
    let fixup = &FIXUP[core_index()] as *const AtomicU64;
    let ok: u64;
    let val: u64;
    // SAFETY: caller's contract for `addr`; the fixup is armed around the single load and cleared on
    // both exit paths, so no later fault is silently absorbed.
    unsafe {
        core::arch::asm!(
            "adr  {tmp}, 3f",
            "str  {tmp}, [{fixup}]",
            "mov  {ok}, #1",
            "ldr  {val:w}, [{addr}]",
            "str  xzr, [{fixup}]",
            "b    4f",
            "3:",
            "str  xzr, [{fixup}]",
            "mov  {ok}, #0",
            "mov  {val}, #0",
            "4:",
            addr = in(reg) addr,
            fixup = in(reg) fixup,
            tmp = out(reg) _,
            ok = out(reg) ok,
            val = out(reg) val,
            options(nostack),
        );
    }
    if ok != 0 { Some(val as u32) } else { None }
}

/// The actual read, one byte at a time through `ldtrb`.
///
/// Byte-at-a-time is deliberate for now: it is obviously correct, and the fixup has to cover exactly
/// the faulting instruction. A wider copy is a later optimisation that must keep that property.
fn copy_from_user(dst: *mut u8, src: u64, len: usize) -> bool {
    let fixup = &FIXUP[core_index()] as *const AtomicU64;
    let ok: u64;
    // SAFETY: `src` passed the range check, and every access below is `ldtrb` - an UNPRIVILEGED load,
    // so the MMU applies EL0 permissions and refuses a kernel address regardless of what the range
    // check concluded. `dst` is this core's staging buffer, sized `MAX_USER_COPY`, and `len` was
    // checked against it. The fixup registered around the loop turns a fault into `ok = 0` rather than
    // a halt; it is cleared on every exit path so a later kernel bug still halts loudly.
    unsafe {
        core::arch::asm!(
            "adr  {tmp}, 3f",              // recovery label
            "str  {tmp}, [{fixup}]",       // arm the fixup
            "mov  {ok}, #1",
            "cbz  {len}, 2f",
            "1:",
            "ldtrb {tmp:w}, [{src}]",     // UNPRIVILEGED load: EL0 permissions apply
            "strb  {tmp:w}, [{dst}], #1",
            "add   {src}, {src}, #1",
            "subs  {len}, {len}, #1",
            "b.ne  1b",
            "2:",
            "str   xzr, [{fixup}]",        // disarm
            "b     4f",
            "3:",                          // fault landed here
            "str   xzr, [{fixup}]",
            "mov   {ok}, #0",
            "4:",
            src = inout(reg) src => _,
            dst = inout(reg) dst => _,
            len = inout(reg) len => _,
            fixup = in(reg) fixup,
            tmp = out(reg) _,
            ok = out(reg) ok,
            options(nostack),
        );
    }
    ok != 0
}

/// The actual write, one byte at a time through `sttrb`. See [`copy_from_user`].
fn copy_to_user(dst: u64, src: *const u8, len: usize) -> bool {
    let fixup = &FIXUP[core_index()] as *const AtomicU64;
    let ok: u64;
    // SAFETY: as `copy_from_user`, with the direction reversed - `sttrb` is the UNPRIVILEGED store, so
    // a kernel address is refused by the MMU even though this executes at EL1.
    unsafe {
        core::arch::asm!(
            "adr  {tmp}, 3f",
            "str  {tmp}, [{fixup}]",
            "mov  {ok}, #1",
            "cbz  {len}, 2f",
            "1:",
            "ldrb  {tmp:w}, [{src}], #1",
            "sttrb {tmp:w}, [{dst}]",     // UNPRIVILEGED store
            "add   {dst}, {dst}, #1",
            "subs  {len}, {len}, #1",
            "b.ne  1b",
            "2:",
            "str   xzr, [{fixup}]",
            "b     4f",
            "3:",
            "str   xzr, [{fixup}]",
            "mov   {ok}, #0",
            "4:",
            src = inout(reg) src => _,
            dst = inout(reg) dst => _,
            len = inout(reg) len => _,
            fixup = in(reg) fixup,
            tmp = out(reg) _,
            ok = out(reg) ok,
            options(nostack),
        );
    }
    ok != 0
}

/// Exercise the seam, including the paths that only fire on bad input.
///
/// A user-copy helper that has only ever been given good pointers is untested where it matters. This
/// drives all four outcomes: a good read, a good write, a rejected out-of-range pointer, and - the one
/// that would otherwise halt the machine - a **range-valid but unmapped** pointer, which must come back
/// as a failed copy with the kernel still running.
pub fn selftest(mapped_user_va: u64) -> (bool, bool, bool, bool) {
    let mut scratch = [0u8; 8];

    // 1. Write to a mapped user page, then read it back.
    let wrote = write_user_bytes(mapped_user_va, b"GODSPEED");
    let read_back = read_user_bytes(mapped_user_va, 8)
        .map(|s| {
            scratch.copy_from_slice(s);
            &scratch == b"GODSPEED"
        })
        .unwrap_or(false);

    // 2. A kernel address must be refused by the range check, before any access happens.
    let kernel_refused = !validate_user_ptr(super::mmu::KERNEL_VA_BASE, 8)
        && read_user_bytes(super::mmu::KERNEL_VA_BASE, 8).is_none();

    // 3. Range-valid but UNMAPPED. This is the one that halts an unguarded kernel: the abort lands at
    //    EL1 and looks like a kernel bug. It must come back as a failed copy instead.
    //    It must ALSO have gone through the fixup: if the count does not move, the pointer was
    //    rejected somewhere upstream and this test proved nothing about the recovery path.
    let faults_before = recovered_count();
    let unmapped_survived = read_user_bytes(0x4000_0000, 8).is_none();
    let fixup_fired = recovered_count() > faults_before;

    (wrote && read_back, kernel_refused, unmapped_survived, fixup_fired)
}
