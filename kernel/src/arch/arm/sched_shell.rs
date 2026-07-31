// SPDX-License-Identifier: GPL-2.0-only
//! The interactive shell on ARM (increment 5): a `gsh>` prompt reading serial input.
//!
//! Everything below the shell is now in place (kernel, MMU, scheduler, syscalls, user mode, the neutral
//! spawn path, console I/O). This brings up the user's interface: the kernel spawns the `logger` and the
//! `shell` (through the neutral spawn path, with a CONSOLE_READ cap), tells the shell the input path is
//! up (`set_input_ready` - the PL011 RX is always available), and enters the scheduler. The shell logs
//! `shell: ready`, prints `gsh> `, and blocks in `console_read`; a keystroke on the serial line lands in
//! the PL011 RX FIFO, the timer tick (or the blocked read itself) drains it into the input ring and
//! wakes the shell, which reads the byte, echoes it, and runs the command on Enter.
//!
//! It also brings all cores online (`smp_bringup`) so the machine boots to "4 cores ready", the same as
//! the supervisor path - the APs park idle behind the shell and stay quiet until there is work to run.
//!
//! There is no `fs` on the Pi 2 yet, so file/history commands degrade; every other command works. This
//! is deliberately a direct spawn (not via the supervisor) so the prompt is clean - no ping/pong output
//! competing with the line editor.

use core::sync::atomic::Ordering;

use super::pl011_write;

/// Bring up the neutral subsystems, spawn logger + shell, signal input-ready, and enter the scheduler.
/// Does not return.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);

    // Bring the other cores online (same order as the supervisor path: APs park idle, ready to schedule
    // once NEUTRAL_SCHED flips). The shell + logger still run on core 0; this just makes "4 cores ready"
    // true and lets any spawned work spread. The APs are quiet until there is something to run.
    super::smp_bringup();

    pl011_write(b"sched-shell: spawning logger + shell...\r\n");
    crate::task::arm_spawn_logger_neutral();
    crate::task::arm_spawn_shell_neutral();

    // The PL011 RX is the input driver, and it is always up - tell the shell so it presents its prompt
    // (the deterministic end-of-boot signal it waits on).
    super::set_input_ready();

    // Mask IRQs before arming the neutral scheduler and entering run(0). CRITICAL: once NEUTRAL_SCHED is
    // set, the timer ISR preempts whatever core 0 is running - and here that is still THIS bootstrap, not
    // a task. If the timer fires in the window between the store below and run(0) seeding the scheduler
    // context, it preempts the bootstrap into `CORE_SCHED_CTX[0]` WITHOUT seeding its `cr3` (switch_context
    // never saves cr3), leaving cr3=0. The first task that then blocks switches to that context, loads
    // TTBR0=0, and wedges core 0 - the shell can never be rescheduled, so serial/keyboard input hangs.
    // With IRQs masked, run(0) reaches its cr3 seeding uninterrupted; the scheduler loop re-enables IRQs
    // once it switches to the first task.
    super::irq::disable_interrupts();
    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-shell: entering scheduler::run(0) - type at the serial console for 'gsh> '.\r\n");
    crate::task::scheduler::run(0)
}
