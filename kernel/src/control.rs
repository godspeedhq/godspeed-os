//! Control serial channel — receives `osdev restart` commands via COM2.
//!
//! COM2 is connected to `tcp::5555` in the QEMU configuration.
//! `osdev restart <name> [<core>]` sends `RESTART <name> <core>\n`.
//!
//! The `process_pending` function is called from Core 0's scheduler idle loop.
//! It drains COM2 bytes into a line buffer and processes complete commands.

use core::sync::atomic::{AtomicBool, Ordering};

const BUF_SIZE: usize = 128;

/// Per-core lock ensures only Core 0 calls `process_pending`.
static CTRL_LOCKED: AtomicBool = AtomicBool::new(false);

/// Command line buffer (written only from Core 0).
static mut LINE_BUF: [u8; BUF_SIZE] = [0u8; BUF_SIZE];
static mut LINE_LEN: usize = 0;

/// Drain COM2 and process any complete `\n`-terminated commands.
///
/// Called from Core 0's scheduler idle loop only — never from interrupt context.
pub fn process_pending() {
    if CTRL_LOCKED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    // SAFETY: only called from Core 0 under CTRL_LOCKED; single-writer.
    unsafe {
        while let Some(b) = crate::arch::x86_64::com2_try_read_byte() {
            if b == b'\n' || b == b'\r' {
                if LINE_LEN > 0 {
                    let line = core::str::from_utf8(&LINE_BUF[..LINE_LEN]).unwrap_or("");
                    execute_command(line);
                    LINE_LEN = 0;
                }
            } else if LINE_LEN < BUF_SIZE - 1 {
                LINE_BUF[LINE_LEN] = b;
                LINE_LEN += 1;
            }
        }
    }

    CTRL_LOCKED.store(false, Ordering::Release);
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
        Some(other) => crate::kprintln!("control: unknown command '{}'", other),
        None => {}
    }
}
