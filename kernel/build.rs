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
    // Two aarch64 layouts: QEMU `virt` loads at 0x4008_0000, the Pi 4's VideoCore firmware loads a
    // flat kernel8.img at 0x80000. The `pi4` feature selects the board.
    let kernel_ld_aarch64 = workspace.join("kernel").join("kernel-aarch64.ld");
    let kernel_ld_aarch64_pi4 = workspace.join("kernel").join("kernel-aarch64-pi4.ld");
    println!("cargo:rerun-if-changed={}", kernel_ld_aarch64.display());
    println!("cargo:rerun-if-changed={}", kernel_ld_aarch64_pi4.display());
    if target == "aarch64-unknown-none" {
        let script = if std::env::var("CARGO_FEATURE_PI4").is_ok() {
            &kernel_ld_aarch64_pi4
        } else {
            &kernel_ld_aarch64
        };
        println!("cargo:rustc-link-arg=-T{}", script.display());
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
    let use_placeholder = is_riscv64 || is_loongarch64 || is_s390x || is_riscv32 || is_arm || is_aarch64;
    let placeholder = workspace.join("kernel").join("svc-placeholder.bin");

    // (env-var suffix, binary name in target dir)
/// Services that exist only on one architecture, so their absence from another target's build is a
/// FACT rather than an omission. `dwc2` drives the BCM283x USB controller: there is no such device on
/// x86, and no reason to carry the driver there.
const ARM_ONLY: &[&str] = &["dwc2"];

    let services: &[(&str, &str)] = &[
        ("SUPERVISOR", "supervisor"),
        ("DWC2",       "dwc2"),
        ("LOGGER",     "logger"),
        ("CONSOLE",    "console"),
        ("TIME",       "time"),
        ("CONTROL",    "control"),
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
        "logger", "console", "ping", "pong", "supervisor", "shell",
        "observe", "chaos", "mem-pressure",
        "counter", "greet", "upper", "roster",
        "reply-server", "asker", "resource-server", "holder",
        // The userspace USB host driver (arm32 Phase 2). A SKELETON today: it holds the DWC2 MMIO
        // window, a DMA arena and the USB vector, and reports whether the interrupt arrives. Built
        // and embedded unconditionally; whether it SPAWNS is the supervisor's decision.
        "dwc2",
        // The wall clock (C1-6). REQUIRED on the Pi 2 rather than optional: the board has no
        // battery-backed RTC, so SNTP is the only way it learns the time, and the `SetClock` syscall
        // net-stack used to call is deleted. Without this embedded, `date sync` fails silently.
        "time",
        // The COM2 operator channel (C1-6). Inert on the Pi, which is driven from its own console, but
        // embedded so the service set does not differ per arch without a reason.
        "control",
        // Persistence on the Pi 2: block-driver's ARM backend is the BCM2835 EMMC (SDHCI, PIO); fs is
        // arch-neutral and rides on it. The kernel grants block-driver the EMMC MMIO window at spawn
        // (arch::arm::map_fixed_driver_mmio).
        "block-driver", "fs",
        // Networking on the Pi 2: nic-driver's ARM backend bridges the frame IPC to the in-kernel DWC2
        // CDC-ECM USB-net device (NET_DEVICE syscalls); net-stack is arch-neutral and rides on it.
        "nic-driver", "net-stack",
    ];
    let arm_dir = workspace
        .join("target")
        .join("armv7a-none-eabi")
        .join(&profile);

    // AArch64 (Raspberry Pi 4) userspace is being brought up the same way, service by service, for the
    // same reason: a service is embedded for real only once it is built for aarch64-unknown-none. Any
    // not yet ported keep the empty placeholder so the kernel still links. The hardware drivers stay
    // placeholders until real Pi 4 drivers exist (SD/EMMC, GENET, VL805 xHCI over PCIe) - they compile,
    // but hunt for x86 hardware that is not there.
    // `ping`/`pong` are DEMO services: they send as fast as the scheduler runs them (they pace with
    // `yield_cpu`, not a sleep), which is ~500 log lines a second. That is fine when the point is to
    // prove IPC, and unusable when the point is to type at a prompt - the shell's output scrolls past
    // faster than a human can read it. Cross-service IPC is already hardware-proven (63,579 messages),
    // so they are opt-in via `pi4-demo-services` rather than always present.
    let aarch64_demo = std::env::var("CARGO_FEATURE_PI4_DEMO_SERVICES").is_ok();
    // Networking on the Pi 4: nic-driver reaches the hardware through the NET_DEVICE syscalls, which
    // the aarch64 arch layer now backs with the GENET driver (receive and transmit both hardware
    // proven); net-stack is arch-neutral and rides on nic-driver without touching hardware at all.
    // Neither needed porting - they needed BUILDING and then listing HERE. Being built is not enough:
    // a service missing from this list embeds the empty placeholder however well it compiled, and the
    // boot reports `LoadFailed(TooSmall)` - which reads like a broken binary rather than an absent one.
    // `xhci` is the userspace USB host-controller driver for the Pi 4's VL805. It is embedded
    // unconditionally rather than behind `xhci-userspace`, and that is deliberate: an ELF the kernel
    // carries but never spawns costs image bytes and nothing else, whereas a service the supervisor
    // spawns and the kernel embedded as a PLACEHOLDER fails with `LoadFailed(TooSmall)` - which reads
    // like a broken binary rather than a build-list omission, and is exactly the trap this comment
    // block warns about two paragraphs up. Cheap insurance against the failure mode with the worst
    // diagnostic. The spawn is what the feature gates (`services/supervisor`), not the embedding.
    // `time` and `control` are here for the same reason they are in `arm_built`, and their absence was
    // the Pi 4's first boot failure after the arm32 work: the supervisor SPAWNS both on every port, so
    // leaving them out of this list embedded placeholders and the boot reported two `LoadFailed(TooSmall)`
    // lines - the exact "reads like a broken binary rather than an absent one" trap the paragraph above
    // warns about. `time` is REQUIRED, not optional: the Pi 4 has no battery-backed RTC either, so SNTP
    // is the only way it learns the wall clock. `control` is inert on a board driven from its own
    // console, and is embedded so the service set does not differ per arch without a reason.
    let aarch64_built: &[&str] = if aarch64_demo {
        &["logger", "console", "time", "control", "ping", "pong", "supervisor", "shell",
          "chaos", "observe", "mem-pressure",
          "block-driver", "fs", "nic-driver", "net-stack", "xhci",
          "counter", "greet", "upper", "roster", "reply-server", "asker", "resource-server", "holder"]
    } else {
        // `chaos` and `observe` are not demo services: chaos is how the port is proven to survive
        // carnage, and observe is how it is watched while it does. Both are arch-neutral.
        &["logger", "console", "time", "control", "supervisor", "shell",
          "chaos", "observe", "mem-pressure",
          "block-driver", "fs", "nic-driver", "net-stack", "xhci",
          "counter", "greet", "upper", "roster", "reply-server", "asker", "resource-server", "holder"]
    };
    let aarch64_dir = workspace
        .join("target")
        .join("aarch64-unknown-none")
        .join(&profile);

    for (env_name, bin_name) in services {
        let elf = if is_arm {
            // A ported ARM service if its binary exists; otherwise the placeholder.
            let arm_bin = arm_dir.join(bin_name);
            if arm_built.contains(bin_name) && arm_bin.exists() { arm_bin } else { placeholder.clone() }
        } else if is_aarch64 {
            // A ported AArch64 service if its binary exists; otherwise the placeholder.
            let a64_bin = aarch64_dir.join(bin_name);
            if aarch64_built.contains(bin_name) && a64_bin.exists() { a64_bin } else { placeholder.clone() }
        } else if use_placeholder {
            placeholder.clone()
        } else if ARM_ONLY.contains(bin_name) {
            // An ARM-ONLY driver on an x86 build. The placeholder is the right answer and this branch
            // is explicit rather than a fallback-if-missing, because "the binary is not there" has two
            // causes with opposite fixes: a service that does not APPLY to this target (fine, embed
            // the placeholder) and a service that was left out of the build list (a bug, and one that
            // surfaces as `LoadFailed(TooSmall)` - which reads like a broken binary, exactly the trap
            // the comment above warns about). Silently accepting a missing file would merge the two.
            placeholder.clone()
        } else {
            target_dir.join(bin_name)
        };
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
