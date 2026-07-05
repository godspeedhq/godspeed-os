// SPDX-License-Identifier: GPL-2.0-only
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
        ("SUPERVISOR", "supervisor"),
        ("LOGGER",     "logger"),
        ("MEM_PRESSURE",    "mem-pressure"),
        ("CHAOS",      "chaos"),
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
        ("NIC_DRIVER",  "nic-driver"),
        ("NET_STACK",   "net-stack"),
        ("FS",         "fs"),
        ("COUNTER",    "counter"),  // examples/counter: stateful service, survives its own restart
        ("REPLY_SERVER", "reply-server"), // examples/reply-server: request/reply (RPC) server
        ("ASKER",      "asker"),    // examples/asker: the request/reply CLIENT that exercises reply-server
        ("RESOURCE_SERVER", "resource-server"), // examples/resource-server: MINTs a delegated resource cap (§7.10)
        ("HOLDER",     "holder"),   // examples/holder: the CLIENT that USEs the granted resource cap
    ];

    for (env_name, bin_name) in services {
        let elf = target_dir.join(bin_name);
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
