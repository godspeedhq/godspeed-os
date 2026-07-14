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
    let kernel_ld_aarch64 = workspace.join("kernel").join("kernel-aarch64.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld_aarch64.display());
    if target == "aarch64-unknown-none" {
        println!("cargo:rustc-link-arg=-T{}", kernel_ld_aarch64.display());
    }
    let kernel_ld_riscv64 = workspace.join("kernel").join("kernel-riscv64.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld_riscv64.display());
    if target == "riscv64imac-unknown-none-elf" {
        println!("cargo:rustc-link-arg=-T{}", kernel_ld_riscv64.display());
    }
    let kernel_ld_loongarch64 = workspace.join("kernel").join("kernel-loongarch64.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld_loongarch64.display());
    if target == "loongarch64-unknown-none-softfloat" {
        println!("cargo:rustc-link-arg=-T{}", kernel_ld_loongarch64.display());
    }
    let profile   = std::env::var("PROFILE").unwrap(); // "debug" or "release"

    let target_dir = workspace
        .join("target")
        .join("x86_64-unknown-none")
        .join(&profile);

    // AArch64 demarcation build (docs/aarch64.md): real aarch64 service ELFs don't exist yet, so embed
    // an empty placeholder - the arch-neutral kernel must still COMPILE for aarch64 (the boundary test).
    // x86 embeds the real service binaries. When aarch64 services are built, point this at their dir.
    let is_aarch64 = target == "aarch64-unknown-none";
    let is_riscv64 = target == "riscv64imac-unknown-none-elf";
    let is_loongarch64 = target == "loongarch64-unknown-none-softfloat";
    let use_placeholder = is_aarch64 || is_riscv64 || is_loongarch64;
    let placeholder = workspace.join("kernel").join("svc-placeholder.bin");

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
        let elf = if use_placeholder { placeholder.clone() } else { target_dir.join(bin_name) };
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
