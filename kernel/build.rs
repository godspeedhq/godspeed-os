// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

/// Emits `cargo:rustc-env=SVC_<NAME>_ELF=<path>` for each service binary.
///
/// `osdev build` compiles the service crates BEFORE the kernel so these
/// paths exist by the time the kernel's `include_bytes!` macros run.
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace = std::path::Path::new(&manifest).parent().unwrap();

    // Apply the kernel linker script only when building the bare-metal binary.
    // Host builds (cargo test, cargo llvm-cov) use the default linker and do
    // not understand the GNU -T flag.
    let target = std::env::var("TARGET").unwrap_or_default();
    let kernel_ld = workspace.join("kernel").join("kernel.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld.display());
    if target == "x86_64-unknown-none" {
        println!("cargo:rustc-link-arg=-T{}", kernel_ld.display());
    }
    let profile   = std::env::var("PROFILE").unwrap(); // "debug" or "release"

    let target_dir = workspace
        .join("target")
        .join("x86_64-unknown-none")
        .join(&profile);

    // (env-var suffix, binary name in target dir)
    let services: &[(&str, &str)] = &[
        ("SUPERVISOR", "supervisor"),  // init removed (Phase 5); registry retired (Phase 4)
        ("LOGGER",     "logger"),
        ("PING",       "ping"),
        ("PONG",       "pong"),
        ("GREET",      "greet"),
        ("UPPER",      "upper"),
        ("ROSTER",     "roster"),
        ("PROBE",      "probe"),
        ("OBSERVE",    "observe"),
        ("SHELL",      "shell"),
        ("XHCI",       "xhci"),
        ("EHCI",       "ehci"),
        ("BLOCK_DRIVER", "block-driver"),
        ("FS",         "fs"),
    ];

    for (env_name, bin_name) in services {
        let elf = target_dir.join(bin_name);
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
