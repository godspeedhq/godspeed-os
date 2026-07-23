// SPDX-License-Identifier: GPL-2.0-only
//! Spawn a service through the NEUTRAL spawn path on ARM (increment 4a).
//!
//! The earlier demos (`sched_user`, `sched_ipc`) hand-wired everything a spawn does - loading the ELF,
//! mapping the stack/ctx, minting caps, cloning the kernel identity - in ARM-local code. But the real
//! OS spawns services through the supervisor, which calls the **spawn syscall**, which calls the
//! neutral `task::spawn_service_with_config`. This proves that neutral machinery works unchanged on ARM
//! (the foundation the supervisor stands on): it runs the neutral bootstrap, spawns the `logger`
//! through `spawn_service_with_config`, and enters `scheduler::run`. The two ARM-specific steps the
//! neutral path needs are now arch-seam hooks it calls itself: `page_tables::finalize_service_address_
//! space` (clone the kernel identity + clean the D-cache; the kernel is not shared higher-half as on
//! x86) and `note_user_task` (record the slot for the atomic-syscall timer check). If `logger: ready`
//! appears, the neutral spawn - kstack pool, cap wiring, ctx page, per-task address space - all work.

use core::sync::atomic::Ordering;

use super::pl011_write;

/// Bring up the neutral subsystems, spawn the logger through the neutral spawn path, and enter the
/// scheduler. Does not return.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);

    pl011_write(b"sched-spawn: spawning logger through the NEUTRAL spawn path...\r\n");
    crate::task::arm_spawn_logger_neutral();

    // The neutral spawn already ran finalize_service_address_space (kernel-identity + D-cache clean)
    // per service inside spawn_service_with_config, so no extra one-shot clean is needed here.
    // Mask IRQs before arming the neutral scheduler: the timer must not preempt into the scheduler
    // context before run(0) seeds its cr3/TTBR0, or the first task to block wedges the core (kernel-audit
    // Audit 5 (C); matches the shipping sched_shell/sched_supervisor guard).
    super::irq::disable_interrupts();
    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-spawn: entering scheduler::run(0) - watch for 'logger: ready'.\r\n");
    crate::task::scheduler::run(0)
}
