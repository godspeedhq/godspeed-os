//! QEMU invocation for `osdev run` (§17).
//!
//! Launches `qemu-system-x86_64` with the given disk image and SMP count.
//! Serial output is written to build/serial.log so it survives across platforms
//! (on Windows, qemu-system is a GUI app and doesn't reliably pipe stdio).
//! On Windows expects QEMU at its default install path; on other platforms
//! expects it on PATH.

use std::path::Path;

const SERIAL_LOG: &str = "build/serial.log";
const DEBUGCON_LOG: &str = "build/debugcon.txt";
const QEMU_DEBUG_LOG: &str = "build/qemu-debug.log";

/// Launch QEMU with the given raw disk image.
/// Serial output lands in build/serial.log; its contents are printed after exit.
pub fn run(image_path: &Path, smp: u32) {
    let qemu = qemu_binary();

    println!("run: launching QEMU (smp={smp}, image={})", image_path.display());
    println!("run: serial output → {SERIAL_LOG}");

    let image_str = image_path.to_string_lossy().replace('\\', "/");

    let status = std::process::Command::new(&qemu)
        .args([
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
        ])
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

fn qemu_binary() -> String {
    if cfg!(windows) {
        let default = r"C:\Program Files\qemu\qemu-system-x86_64.exe";
        if std::path::Path::new(default).exists() {
            return default.to_string();
        }
    }
    "qemu-system-x86_64".to_string()
}
