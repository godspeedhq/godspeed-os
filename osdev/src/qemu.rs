//! QEMU invocation for `osdev run` (§17).
//!
//! Launches `qemu-system-x86_64` with the given disk image and SMP count.
//! Serial output is forwarded to stdio so kernel log messages appear in the
//! terminal. On Windows expects QEMU at its default install path; on other
//! platforms expects it on PATH.

use std::path::Path;

/// Launch QEMU with the given raw disk image.
pub fn run(image_path: &Path, smp: u32) {
    let qemu = qemu_binary();

    println!("run: launching QEMU (smp={smp}, image={})", image_path.display());

    let status = std::process::Command::new(&qemu)
        .args([
            "-drive",
            &format!("format=raw,file={},if=ide", image_path.display()),
            "-smp",
            &smp.to_string(),
            "-m",
            "512M",
            "-serial",
            "stdio",
            "-no-reboot",
            "-no-shutdown",
        ])
        .status()
        .unwrap_or_else(|e| {
            eprintln!("failed to launch QEMU at {}: {}", qemu, e);
            eprintln!("Install QEMU from https://www.qemu.org/download/");
            std::process::exit(1);
        });

    if !status.success() {
        eprintln!("QEMU exited with status: {}", status);
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
