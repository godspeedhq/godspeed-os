// SPDX-License-Identifier: GPL-2.0-only
//! Spawn a **real service** through the neutral spawn path, and run it.
//!
//! Every EL0 task so far has been a hand-written blob this file copied into a frame. This one is an
//! actual service - `services/logger`, compiled from Rust against the SDK for `aarch64-unknown-none`,
//! embedded in the kernel image, and loaded by `task::spawn_service_with_config`: the exact machinery
//! the supervisor's spawn syscall uses.
//!
//! That path exercises, in one call, nearly everything the port has built:
//!
//! - the **ELF loader** parsing real program headers and mapping them into a new address space
//! - **`page_tables::PageTable`**, since the loader creates and maps through it
//! - **`sync_instruction_cache`**, because the loader writes a service's text and the core must not
//!   fetch whatever the I-cache held for those frames before
//! - the **kernel stack pool**, capability wiring, and the service context page
//! - **`TaskContext::new_user`**, to enter it at EL0
//! - the **SDK ABI** and **syscall dispatch**, the moment the service makes its first call
//!
//! `logger: ready` on the console therefore means far more than the logger working: it means a
//! compiled Rust service ran on this board and talked to the kernel.

use super::put_str;

/// Spawn the logger through the neutral path, then enter the scheduler. Does not return.
pub fn run() -> ! {
    put_str(b"sched-spawn: spawning the logger service through the NEUTRAL spawn path\r\n");
    crate::task::arm_spawn_logger_neutral();

    // Mask IRQs before arming the neutral scheduler: the timer must not preempt into the scheduler
    // context before `run(0)` has seeded it, or the first task to block wedges the core. The 32-bit
    // port hit exactly that (kernel-audit Audit 5).
    super::interrupts::disable_interrupts();
    super::exceptions::NEUTRAL_SCHED.store(true, core::sync::atomic::Ordering::Relaxed);
    put_str(b"sched-spawn: entering scheduler::run(0) - watch for 'logger: ready'\r\n");
    crate::task::scheduler::run(0)
}
