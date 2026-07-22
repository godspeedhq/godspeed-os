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
    let kernel_ld_arm = workspace.join("kernel").join("kernel-arm.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld_arm.display());
    if target == "armv7a-none-eabi" {
        println!("cargo:rustc-link-arg=-T{}", kernel_ld_arm.display());
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
    let is_s390x = target == "s390x-unknown-none-softfloat";
    let is_riscv32 = target == "riscv32imac-unknown-none-elf";
    let is_arm = target == "armv7a-none-eabi";
    let use_placeholder = is_aarch64 || is_riscv64 || is_loongarch64 || is_s390x || is_riscv32 || is_arm;
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

    // ARM userspace is being brought up incrementally (docs/multi-arch.md): a service is embedded
    // for real only once it is built for armv7a-none-eabi. Any not yet ported keep the empty
    // placeholder, so the kernel still links. As each is ported, drop its name in here.
    // Userspace services that use only the arch-neutral SDK + syscalls (no hardware probe) run on ARM
    // as-is. The hardware drivers (block-driver, fs, nic-driver, net-stack, xhci, ehci) compile but hunt
    // for x86 hardware (PCI/AHCI/Realtek/xHCI) absent on the Pi 2, so they stay placeholders until real
    // Pi drivers (SD/EMMC, DWC2, LAN9514) exist. `probe` does not build for ARM (x86-only fault module).
    let arm_built: &[&str] = &[
        "logger", "ping", "pong", "supervisor", "shell",
        "observe", "chaos", "mem-pressure",
        "counter", "greet", "upper", "roster",
        "reply-server", "asker", "resource-server", "holder",
    ];
    let arm_dir = workspace
        .join("target")
        .join("armv7a-none-eabi")
        .join(&profile);

    for (env_name, bin_name) in services {
        let elf = if is_arm {
            // A ported ARM service if its binary exists; otherwise the placeholder.
            let arm_bin = arm_dir.join(bin_name);
            if arm_built.contains(bin_name) && arm_bin.exists() { arm_bin } else { placeholder.clone() }
        } else if use_placeholder {
            placeholder.clone()
        } else {
            target_dir.join(bin_name)
        };
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
