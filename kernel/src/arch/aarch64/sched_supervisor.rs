// SPDX-License-Identifier: GPL-2.0-only
//! The real OS bootstrap: the kernel's **one** direct spawn, and the supervisor takes it from there.
//!
//! Every earlier demo spawned services from the arch layer by hand. This does what §11.1 actually
//! describes: the kernel spawns exactly one thing - the `supervisor` - and the supervisor spawns the
//! rest from its own manifest, wiring each service's capabilities from its `name -> cap` map. The arch
//! layer's involvement ends at the first spawn.
//!
//! That is the whole point of the milestone. Everything after this line is arch-neutral code doing what
//! it already does on x86 and on the 32-bit ARM port; if the shell comes up, the port is running the
//! same OS those do, not a special case.
//!
//! ## The one thing this port must say for itself
//!
//! **Console input is ready.** On x86 the shell's prompt is gated on an input-ready signal raised when
//! `xhci` comes up, because a USB keyboard is the input path there. Here the input path is the PL011
//! receiver, which has been up since milestone 1 - and `xhci` is a placeholder that never spawns. Left
//! unsaid, the shell would wait for a prompt that never comes, with nothing in the log to explain it.
//! The 32-bit port raises the same flag for the same reason.

use super::put_str;

/// Make the kernel's one direct spawn and enter the scheduler. Does not return.
pub fn run() -> ! {
    put_str(b"sched-supervisor: the kernel's ONE direct spawn - the supervisor\r\n");

    // §11.1: the kernel spawns the supervisor and nothing else. A failure here is fatal by design
    // (§11.3) - the supervisor is the trusted root, and a system without one has nothing to recover it.
    crate::task::spawn_supervisor();

    // Console input is up (see the module header). Raised BEFORE the scheduler runs, so the shell finds
    // it true the first time it looks rather than racing.
    super::set_input_ready();

    // Mask IRQs before arming the neutral scheduler: the timer must not preempt this bootstrap into the
    // scheduler context before `run(0)` has seeded it. The 32-bit port hit exactly that.
    super::interrupts::disable_interrupts();
    super::exceptions::NEUTRAL_SCHED.store(true, core::sync::atomic::Ordering::Relaxed);
    put_str(b"sched-supervisor: entering scheduler::run(0) - watch for the gsh prompt\r\n");
    crate::task::scheduler::run(0)
}
