// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! QEMU invocation for `osdev run` (§17).
//!
//! Launches `qemu-system-x86_64` with the given disk image and SMP count.
//! Serial output is written to build/serial.log so it survives across platforms
//! (on Windows, qemu-system is a GUI app and doesn't reliably pipe stdio).
//! On Windows expects QEMU at its default install path; on other platforms
//! expects it on PATH.

use std::path::Path;

pub const SERIAL_LOG: &str = "build/serial.log";
const DEBUGCON_LOG: &str = "build/debugcon.txt";
const QEMU_DEBUG_LOG: &str = "build/qemu-debug.log";

/// Launch QEMU with the given raw disk image.
/// Serial output lands in build/serial.log; its contents are printed after exit.
pub fn run(image_path: &Path, smp: u32) {
    let qemu = qemu_binary();

    println!("run: launching QEMU (smp={smp}, image={})", image_path.display());
    println!("run: serial output → {SERIAL_LOG}");

    let image_str = image_path.to_string_lossy().replace('\\', "/");

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",
        &format!("format=raw,file={image_str},if=ide"),
        "-smp",
        &smp.to_string(),
        "-m",
        "512M",
        // COM1 → serial log file (kernel kprintln output).
        "-serial",
        &format!("file:{SERIAL_LOG}"),
        // COM2 → TCP server on port 5555 (`osdev restart` control channel).
        "-serial",
        "tcp::5555,server,nowait",
        "-debugcon",
        &format!("file:{DEBUGCON_LOG}"),
        "-display",
        "none",
        "-monitor",
        "telnet::4444,server,nowait",
        "-no-reboot",
        "-no-shutdown",
        "-d",
        "int,cpu_reset",
        "-D",
        QEMU_DEBUG_LOG,
    ]);
    if kvm_available() {
        cmd.arg("-enable-kvm");
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| {
            eprintln!("failed to launch QEMU at {}: {}", qemu, e);
            eprintln!("Install QEMU from https://www.qemu.org/download/");
            std::process::exit(1);
        });

    println!("run: QEMU exited ({})", status);

    // Print serial log so callers (and CI) can see kernel output without
    // having to read the file manually.
    match std::fs::read_to_string(SERIAL_LOG) {
        Ok(log) if !log.is_empty() => {
            println!("--- serial log ---");
            print!("{log}");
            println!("--- end serial log ---");
        }
        Ok(_) => println!("run: serial log is empty (kernel produced no output)"),
        Err(e) => println!("run: could not read serial log: {e}"),
    }

    // Print CPU exception/reset log to help diagnose triple faults.
    match std::fs::read_to_string(QEMU_DEBUG_LOG) {
        Ok(log) if !log.is_empty() => {
            println!("--- qemu debug log (last 100 lines) ---");
            let lines: Vec<&str> = log.lines().collect();
            let start = lines.len().saturating_sub(100);
            for line in &lines[start..] {
                println!("{line}");
            }
            println!("--- end qemu debug log ---");
        }
        _ => {}
    }

    if !status.success() {
        eprintln!("QEMU exited with non-zero status: {status}");
    }
}

/// Launch QEMU with stdin/stdout wired to COM1 for the interactive shell.
///
/// COM1 → stdio (bidirectional): shell output comes to the terminal,
/// terminal input goes to COM1 RX (the ConsoleRead syscall).
/// COM2 → TCP:5555 so `osdev restart` still works.
/// `-nographic` suppresses QEMU's VGA window and SDL/GTK dependencies.
pub fn run_shell(image_path: &Path, smp: u32) {
    let qemu = qemu_binary();

    println!("shell: launching QEMU (smp={smp}) - type 'help' at the gsh> prompt");
    println!("shell: press Ctrl-A X to quit QEMU");

    let image_str = image_path.to_string_lossy().replace('\\', "/");

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",
        &format!("format=raw,file={image_str},if=ide"),
        "-smp",
        &smp.to_string(),
        "-m",
        "512M",
        // COM1 → stdio: bidirectional for the shell.
        "-serial",
        "stdio",
        // COM2 → TCP control channel for `osdev restart`.
        "-serial",
        "tcp::5555,server,nowait",
        "-display",
        "none",
        "-nographic",
        "-no-reboot",
        "-no-shutdown",
    ]);
    if kvm_available() {
        cmd.arg("-enable-kvm");
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| {
            eprintln!("failed to launch QEMU at {}: {}", qemu, e);
            eprintln!("Install QEMU from https://www.qemu.org/download/");
            std::process::exit(1);
        });

    println!("shell: QEMU exited ({})", status);
}

// ---------------------------------------------------------------------------
// Test harness support.
// ---------------------------------------------------------------------------

/// A QEMU process spawned for a single test run.
pub struct QemuTestInstance {
    child:       std::process::Child,
    serial_path: std::path::PathBuf,
}

impl QemuTestInstance {
    pub fn serial_path(&self) -> &std::path::Path { &self.serial_path }

    /// Kill the QEMU process and wait for it to exit.
    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn QEMU for a test run (non-blocking).
///
/// COM1 (serial) is written to `serial_path`.
/// COM2 is bound to TCP `control_port` if `Some`; otherwise discarded.
pub fn spawn_for_test(
    image_path:   &Path,
    smp:          u32,
    serial_path:  &Path,
    control_port: Option<u16>,
) -> QemuTestInstance {
    let qemu       = qemu_binary();
    let image_str  = image_path.to_string_lossy().replace('\\', "/");
    let serial_str = format!("file:{}", serial_path.to_string_lossy().replace('\\', "/"));
    let com2_str   = match control_port {
        Some(p) => format!("tcp::{p},server,nowait"),
        None    => "null".to_string(),
    };

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        "-serial",  &serial_str,
        "-serial",  &com2_str,
        "-display", "none",
        "-no-reboot",
        "-no-shutdown",
    ]);
    if kvm_available() {
        cmd.arg("-enable-kvm");
    }
    let child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("identity: failed to launch QEMU at {}: {}", qemu, e);
        std::process::exit(1);
    });

    QemuTestInstance { child, serial_path: serial_path.to_owned() }
}

/// Like `spawn_for_test` but with configurable SMP count and RAM size.
/// Used by chaos tests that need degraded boot environments (C1, C4).
pub fn spawn_for_test_custom(
    image_path:   &Path,
    smp:          u32,
    ram_mib:      u32,
    serial_path:  &Path,
    control_port: Option<u16>,
) -> QemuTestInstance {
    let qemu       = qemu_binary();
    let image_str  = image_path.to_string_lossy().replace('\\', "/");
    let serial_str = format!("file:{}", serial_path.to_string_lossy().replace('\\', "/"));
    let com2_str   = match control_port {
        Some(p) => format!("tcp::{p},server,nowait"),
        None    => "null".to_string(),
    };

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(),
        "-m",       &format!("{ram_mib}M"),
        "-serial",  &serial_str,
        "-serial",  &com2_str,
        "-display", "none",
        "-no-reboot",
        "-no-shutdown",
    ]);
    if kvm_available() {
        cmd.arg("-enable-kvm");
    }
    let child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("chaos: failed to launch QEMU at {}: {}", qemu, e);
        std::process::exit(1);
    });

    QemuTestInstance { child, serial_path: serial_path.to_owned() }
}

pub fn qemu_binary() -> String {
    if cfg!(windows) {
        let default = r"C:\Program Files\qemu\qemu-system-x86_64.exe";
        if std::path::Path::new(default).exists() {
            return default.to_string();
        }
    }
    "qemu-system-x86_64".to_string()
}

/// Returns true when `/dev/kvm` is accessible (Linux + KVM available).
/// Enables hardware-accelerated QEMU; falls back to TCG otherwise.
fn kvm_available() -> bool {
    std::fs::metadata("/dev/kvm").is_ok()
}
