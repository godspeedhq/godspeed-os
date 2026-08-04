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

/// 16 KiB kernel stack for the echo task.
const ECHO_KSTACK: usize = 16 * 1024;
#[repr(align(16))]
struct EchoStack([u8; ECHO_KSTACK]);
static mut ECHO_STACK: EchoStack = EchoStack([0; ECHO_KSTACK]);

/// Echo whatever arrives on the serial line.
///
/// The only way to prove serial INPUT works end to end - FIFO to ring to pop - short of a shell, and
/// the shell cannot exist until this does. It is a kernel task rather than a service because it needs
/// no capability and exists only to make the path observable.
unsafe extern "C" fn echo_task() -> ! {
    super::put_str(b"echo: type on the serial console - characters should come back\r\n");
    let mut count: u32 = 0;
    loop {
        // Drain directly as well as relying on the tick: under load the timer ISR can be late, and a
        // byte stranded in the FIFO with nothing polling looks exactly like broken input.
        super::uart_rx::drain();
        while let Some(b) = super::uart_rx::pop() {
            count += 1;
            super::put_str(b"echo: got ");
            // Printable characters echo as themselves; anything else as its value, so a line that is
            // delivering junk is visibly delivering junk rather than silently blanking.
            if (0x20..0x7F).contains(&b) {
                super::put_byte(b'\'');
                super::put_byte(b);
                super::put_byte(b'\'');
            } else {
                super::put_hex(b as u64);
            }
            super::put_str(b" (total ");
            super::put_dec(count as u64);
            super::put_str(b")\r\n");
        }
        crate::task::scheduler::yield_current();
    }
}

/// Spawn the logger through the neutral path, then enter the scheduler. Does not return.
pub fn run() -> ! {
    super::uart_rx::report();

    // A kernel task that echoes serial input, so the RX path is observable without a shell.
    if let Some(slot) = crate::task::scheduler::reserve_task_slot(0) {
        // SAFETY: single-threaded boot; the stack is this module's static and the slot was just
        // reserved for this task alone. It shares the kernel address space (cr3 = the live TTBR0,
        // which is 0 now that the low map is retired - `switch_context` then leaves TTBR0 alone).
        unsafe {
            let top = (&raw mut ECHO_STACK.0 as *mut u8).add(ECHO_KSTACK);
            let ctx = crate::arch::imp::context_switch::TaskContext::new_kernel(echo_task, top, 0);
            let _ = crate::task::scheduler::task_cap_init_empty(slot);
            crate::task::scheduler::commit_task(slot, "echo", ctx, false, top as u64, None);
        }
    }

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
