//! Control serial channel — receives `osdev restart` commands via COM2.
//!
//! COM2 is connected to `tcp::5555` in the QEMU configuration.
//! `osdev restart <name> [<core>]` sends `RESTART <name> <core>\n`.
//!
//! The `process_pending` function is called from Core 0's scheduler idle loop.
//! It drains COM2 bytes into a line buffer and processes complete commands.

use crate::smp::SpinLock;

const BUF_SIZE: usize = 128;

struct LineBuf {
    buf: [u8; BUF_SIZE],
    len: usize,
}

impl LineBuf {
    const fn new() -> Self {
        Self { buf: [0u8; BUF_SIZE], len: 0 }
    }
}

/// Per-core try-lock ensures only one caller processes pending bytes at a time.
static LINE: SpinLock<LineBuf> = SpinLock::new(LineBuf::new());

/// Drain COM2 and process any complete `\n`-terminated commands.
///
/// Called from Core 0's timer ISR (IF=0) on every tick, and from the scheduler
/// idle loop. Because it runs with interrupts disabled, the drain loop MUST be
/// bounded: on bare-metal hardware with no usable COM2 the LSR reads 0xFF, so
/// `com2_try_read_byte` returns `Some(0xFF)` indefinitely. An unbounded loop
/// here hard-wedges core 0 — it never returns from the timer ISR, so it can
/// never take the cross-core WAKE_RECEIVER IPI and any task blocked on `recv`
/// on the BSP stalls forever. The budget (256) far exceeds any real command
/// line (BUF_SIZE = 128); a stuck LSR just drains 256 junk bytes and returns.
pub fn process_pending() {
    let mut state = match LINE.try_lock() {
        Some(g) => g,
        None    => return,
    };

    let mut budget = 256u32;
    while budget > 0 {
        budget -= 1;
        let b = match crate::arch::x86_64::com2_try_read_byte() {
            Some(b) => b,
            None    => break,
        };
        if b == b'\n' || b == b'\r' {
            if state.len > 0 {
                let line = core::str::from_utf8(&state.buf[..state.len]).unwrap_or("");
                execute_command(line);
                state.len = 0;
            }
        } else if state.len < BUF_SIZE - 1 {
            let idx = state.len;
            state.buf[idx] = b;
            state.len += 1;
        }
    }
}

/// Parse and execute a single control command.
fn execute_command(cmd: &str) {
    let mut parts = cmd.split_ascii_whitespace();
    match parts.next() {
        Some("RESTART") => {
            let name = match parts.next() {
                Some(n) => n,
                None    => { crate::kprintln!("control: RESTART missing name"); return; }
            };
            let core_override: Option<u32> = parts.next()
                .and_then(|s| s.parse().ok());

            crate::kprintln!("control: RESTART {} core={:?}", name, core_override);

            // Kill the running instance (if any).
            crate::task::kill_by_name(name);

            // Respawn.
            match crate::task::spawn_service_by_name(name, core_override) {
                Ok(()) => crate::kprintln!("control: {} restarted", name),
                Err(e) => crate::kprintln!("control: restart failed: {:?}", e),
            }
        }
        Some("KILL") => {
            let name = match parts.next() {
                Some(n) => n,
                None    => { crate::kprintln!("control: KILL missing name"); return; }
            };
            crate::kprintln!("control: KILL {}", name);
            if crate::task::kill_by_name(name) {
                crate::kprintln!("control: {} killed", name);
            } else {
                crate::kprintln!("control: {} not found", name);
            }
        }
        Some("FIRE_IRQ") => {
            let irq: u8 = match parts.next().and_then(|s| s.parse().ok()) {
                Some(n) => n,
                None    => { crate::kprintln!("control: FIRE_IRQ missing irq"); return; }
            };
            crate::kprintln!("control: FIRE_IRQ {}", irq);
            crate::arch::x86_64::interrupts::fire_test_irq(irq);
        }
        Some(other) => crate::kprintln!("control: unknown command '{}'", other),
        None => {}
    }
}
