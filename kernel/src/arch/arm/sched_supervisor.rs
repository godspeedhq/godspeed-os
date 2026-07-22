// SPDX-License-Identifier: GPL-2.0-only
//! The real OS bootstrap on ARM: the kernel spawns the SUPERVISOR, which spawns the services (4b/4c).
//!
//! This is the last hand-off from "demo harness" to "the actual OS". Instead of ARM-local code wiring
//! endpoints and loading ELFs (`sched_ipc`), the kernel makes its ONE direct spawn - the supervisor
//! (`task::spawn_supervisor`, Path C / Phase 5) - and the supervisor, a userspace service, reads its
//! boot manifest and spawns everything else through the spawn syscall (which routes to the neutral
//! `spawn_service_with_config`, proven on ARM in 4a). On the Pi 2 the supervisor spawns the services
//! whose ARM ELFs exist (`logger`, `pong`, `ping`); the hardware services (xhci/ehci/nic/block/fs) are
//! empty placeholders here, so those spawns fail and are skipped (the supervisor ignores spawn errors),
//! exactly the "system continues with the services that did start" behaviour §9.2/§11.3 specify. The
//! kernel wires `ping`'s SEND cap to `pong` from the name directory at spawn, so ping->pong IPC runs -
//! the same message flow as `sched_ipc`, now driven by the real supervisor rather than the kernel.

use core::sync::atomic::Ordering;

use super::pl011_write;

/// Bring up the neutral subsystems, make the kernel's one direct spawn (the supervisor), and enter the
/// scheduler. Does not return. The supervisor takes over from here, spawning the services.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);

    // Bring the other three A7s online BEFORE spawning services, so the supervisor's `spawn_on(pong, 1)`
    // finds core 1 ready and pong actually runs there (real cross-core IPC), instead of falling back to
    // core 0. The APs come up idle in the neutral scheduler and pick up work once it is placed on them.
    super::smp_bringup();

    pl011_write(b"sched-supervisor: the kernel's ONE direct spawn - the supervisor...\r\n");
    // The kernel's one direct spawn (§11.1). Panics if it fails (TCB, §6.2/§11.3) - but on the Pi 2
    // the supervisor ELF is real (arm_built), so it spawns; it then spawns the rest.
    crate::task::spawn_supervisor();

    // On x86 the shell's input-ready signal is raised by `xhci` coming up (a USB keyboard is the input
    // path). On the Pi 2 the input path is the PL011 RX, which is ALWAYS up, and `xhci` is a placeholder
    // that fails to spawn - so nothing would ever raise input-ready and the supervisor-spawned shell
    // would wait for its prompt forever. Raise it here: on ARM, console input is ready the moment the
    // UART is (arch-appropriate - readiness is not gated on a USB driver that does not exist here).
    super::set_input_ready();

    // Mask IRQs before arming the neutral scheduler: the timer must not preempt this bootstrap into the
    // scheduler context before run(0) seeds its cr3, or that context is left with TTBR0=0 and the first
    // task to block wedges core 0. Same fix as sched_shell; the scheduler loop re-enables IRQs.
    super::irq::disable_interrupts();
    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-supervisor: entering scheduler::run(0) - the supervisor now drives the boot.\r\n");
    crate::task::scheduler::run(0)
}
