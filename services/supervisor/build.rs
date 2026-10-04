// SPDX-License-Identifier: GPL-2.0-only
//! Emits `cargo:rustc-env=SVC_<NAME>_ELF` for the service images the SUPERVISOR embeds.
//!
//! Step C moves service images out of the kernel and into here (`docs/service-ownership.md`). The
//! kernel's `build.rs` does the same job for the shrinking set it still holds; this is the other end
//! of that move, and the two will trade entries until the kernel's list is `supervisor` alone.
//!
//! Build ORDER makes this work: `osdev` builds every service before the supervisor, and the
//! supervisor before the kernel, so a service binary is already on disk when this runs.

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace = std::path::Path::new(&manifest).parent().unwrap().parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // The images this supervisor carries.
    const EMBEDDED: &[&str] = &["pong", "roster", "reply-server", "holder", "upper", "mem-pressure",
        "ping", "time", "events", "recorder", "copier", "asker", "resource-server", "chaos", "control", "observe", "greet",
        "counter", "shell", "fs", "net-stack", "block-driver", "console", "nic-driver", "power"];

    // The USB host drivers exist only where their controller does, so they are embedded PER ARCH -
    // the same split, for the same reasons, that `scripts/service_embed_check.py` spells out:
    //   x86_64  - xhci (front ports) + ehci (USB 2.0 back ports); no DWC2 on a PC.
    //   arm     - dwc2 alone; the Pi 2 has no PCIe, no xHCI and no EHCI.
    //   aarch64 - xhci alone; the Pi 4 drives the VL805 over PCIe, and DWC2 is arm32-only.
    //
    // This list was flat and unconditional first, and that was WORSE THAN WRONG: on x86 the absent
    // `dwc2` did not trip the panic below, it resolved to a TWELVE-DAY-OLD binary left in the target
    // directory by an earlier ARM-era build. The guard only fires when a file is missing, and a stale
    // file is not missing - so a supervisor would have shipped an image nothing rebuilds. Embedding
    // only what this arch actually runs removes the question.
    // The probe image is TEST TOOLING and is embedded only where a harness can run it (§4.4). A
    // bare-metal image ships no adversary - see `PROBE_ELF` in main.rs.
    let bare_metal = std::env::var("CARGO_FEATURE_BARE_METAL").is_ok();
    let probe: &[&str] = if bare_metal { &[] } else { &["probe"] };

    // The five examples nothing else ever spawns, embedded ONLY for `osdev test examples`. Before
    // this they compiled on four architectures and had never been executed - which made "example" a
    // weaker word than it reads, since two of them are what a newcomer is pointed at first. Behind a
    // feature for the same reason as `counter-test`: the daily-driver image should not carry a
    // service whose whole job is to log that it started.
    let examples: &[&str] = if std::env::var("CARGO_FEATURE_EXAMPLES_TEST").is_ok() {
        &["hello", "stdlib-hello", "cap-grant", "e1000", "driver-skeleton"]
    } else {
        &[]
    };

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let usb: &[&str] = match arch.as_str() {
        "x86_64"  => &["xhci", "ehci"],
        "arm"     => &["dwc2"],
        "aarch64" => &["xhci"],
        // The VisionFive 2's USB is a Cadence USB3 controller on the SoC bus, and its host half IS an
        // xHCI - so the same driver that runs on a PC card and on the Pi 4's VL805 runs here, which is
        // the whole point of the class the kernel resolves. This arm was absent, and an absent arm
        // falls to the empty list: the supervisor shipped with no image, and `spawn xhci FAILED` was
        // never about the controller at all.
        "riscv64" => &["xhci"],
        _         => &[],
    };

    // Embedded where configuration space is REACHABLE: x86 through the CF8/CFC ports, aarch64
    // through the Pi 4's memory-mapped INDEX/DATA window, riscv64 through a flat ECAM window. Not
    // arm32 - the Pi 2 has no PCI at all, so there is nothing there for it to read.
    //
    // riscv64 was absent for a reason that had nothing to do with the machine: its
    // `pci_cfg_read32` seam member returned `None` unconditionally, so the service would have been
    // embedded, spawned, and had nothing to answer with. The kernel's own ECAM walk had worked since
    // the window was found; only the seam the SERVICE reaches through was missing. That gap was the
    // last thing keeping PCI semantics - what a class code means, where BARs live, how to walk a bus
    // - inside ring 0 on this port, which is exactly what step D2 exists to move out (4.4, 26.10).
    let enumerator: &[&str] = if arch == "x86_64" || arch == "aarch64" || arch == "riscv64" {
        &["hw-enumerator"]
    } else {
        &[]
    };

    // The onboard WiFi radio, which on this board sits on an SD host controller rather than any bus
    // that enumerates. aarch64 alone: the Pi 4 is the one machine here with a full-MAC part soldered to
    // SDIO. Split out as its own list rather than added to `usb` for the same reason `enumerator` is -
    // it answers a different question about the board, and a list that answers two questions stops
    // being readable as either.
    //
    // A BOARD fact, not an ISA one, exactly as `nic_on_pci` is: another aarch64 machine with no radio
    // would want this empty, and would get that by saying so here rather than by becoming an exception
    // inside `main.rs`. The kernel still refuses the MMIO grant on a board whose census found no
    // controller, so an embedded-but-radioless build reports "no radio" and serves rather than dying.
    let radio: &[&str] = if arch == "aarch64" { &["wifi-driver"] } else { &[] };

    // The audio driver (docs/audio.md), a BOARD fact like `radio`: an Intel High Definition Audio
    // controller on x86 (the T630's chipset audio, QEMU's `intel-hda`), and on the Pis a 3.5 mm jack
    // driven by PWM - a different driver for a different device, speaking the same protocol. The
    // VisionFive 2 Lite has no audio output at all, so it embeds neither.
    let audio: &[&str] = match arch.as_str() {
        "x86_64" => &["audio-driver"],
        "arm" | "aarch64" => &["pwm-audio"],
        _ => &[],
    };

    // ---- ONE CFG PER IMAGE THIS BUILD ACTUALLY EMBEDS. ------------------------------------------
    //
    // Derived from the SAME two lists that decide the embedding, three lines above - so `main.rs`
    // cannot disagree with this file. That mattered: `main.rs` restated the arch split for the USB
    // images FIVE times (a `USB_IMAGES` table per arch, plus one empty catch-all) and for
    // `hw-enumerator` SEVEN times, and its own comment says what that cost - "four places had to
    // agree, and each one was silent about the others", written after `spawn xhci FAILED` survived
    // three separate fixes on the VisionFive.
    //
    // The cfgs name the BOARD FACT rather than the instruction set, which is the axis that actually
    // decides these:
    //
    //   has_xhci / has_ehci / has_dwc2   this board has that host controller, so its driver is here
    //   has_hw_enumerator                configuration space is reachable, so the reporter is here
    //
    // A fifth ISA therefore adds ONE arm to `usb` / `enumerator` above and touches nothing in
    // `main.rs`. Note also that these are STRICTLY more correct than what they replace, not just
    // tidier: `main.rs` gated the ehci spawn on `not(any(arm, aarch64))`, which is TRUE on riscv64 -
    // a board that has never had an EHCI image embedded.
    //
    // `values(none())` because these are bare flags: `#[cfg(has_xhci)]`, never `has_xhci = "..."`.
    for flag in ["has_xhci", "has_ehci", "has_dwc2", "has_hw_enumerator", "has_wifi_driver", "has_audio_driver",
                 "has_pwm_audio", "pwm_audio_pi4",
                 "xhci_msi", "nic_on_pci"] {
        println!("cargo::rustc-check-cfg=cfg({flag}, values(none()))");
    }
    for name in usb.iter().chain(enumerator.iter()).chain(radio.iter()).chain(audio.iter()) {
        println!("cargo:rustc-cfg=has_{}", name.replace('-', "_"));
    }
    // Whether the kernel can route this xHCI an MSI vector from its pool, which is what decides
    // between the `pci_irq` hardware class and the plain one. NOT the same question as "is it on
    // PCI": the Pi 4's VL805 is a PCIe device and still takes the plain class, because what it lacks
    // is the routable vector, not the bus. Asking for an interrupt that can never arrive is the
    // failure invariant 12 exists to prevent, which is why this is its own fact.
    // WHICH Pi the jack is on, for `pwm-audio`'s `mode`: the PWM block, its DMA request line and the
    // PWM clock differ between the Pi 2 and the Pi 4 (docs/audio.md, "The Pis"). Stated here, once,
    // as the board fact it is, so the service never infers its board from its instruction set.
    if arch == "aarch64" {
        println!("cargo:rustc-cfg=pwm_audio_pi4");
    }
    if arch == "x86_64" {
        println!("cargo:rustc-cfg=xhci_msi");
        // This board's ethernet controller is on the PCI bus, so `nic-driver` is addressed by CLASS
        // CODE (0x020000) and the kernel resolves the BAR from its own scan. Everywhere else the MAC
        // is on the SoC and there is no class code to name: GENET on the Pi 4, dwmac on the
        // VisionFive, a LAN9514 behind USB on the Pi 2.
        //
        // A board fact, not an ISA one, and the distinction is the whole point of class addressing:
        // an aarch64 board with a PCIe NIC would want the class form, and would get it here by
        // saying so rather than by being an exception inside `main.rs`.
        println!("cargo:rustc-cfg=nic_on_pci");
    }

    // Find the PROFILE directory, which is where the service binaries sit.
    //
    // OUT_DIR is <target>/<triple>/<profile>/build/<pkg>-<hash>/out, so this used to take
    // `ancestors().nth(3)` and call it "derived rather than assumed, so it holds for every triple".
    // It held for every triple and NOT for every platform: on a Linux CI runner that index landed on
    // `<profile>/build` instead of `<profile>`, so every service ELF resolved to a cargo build-script
    // DIRECTORY and the supervisor failed to compile with 26 "Is a directory (os error 21)".
    //
    // Counting levels is the fragile part, so the count is gone. Walk up to the nearest ancestor
    // actually NAMED `build` and take its parent: that is the profile directory by construction,
    // whatever cargo nests in between.
    let out = std::env::var("OUT_DIR").unwrap();
    let target_dir = std::path::Path::new(&out)
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "build"))
        .and_then(|b| b.parent())
        .unwrap_or_else(|| panic!("supervisor/build.rs: no `build` component in OUT_DIR ({out}) - \
                                   cannot locate the profile directory"))
        .to_path_buf();

    for name in EMBEDDED.iter().chain(usb.iter()).chain(enumerator.iter()).chain(radio.iter()).chain(audio.iter())
                        .chain(probe.iter()).chain(examples.iter()) {
        let elf = target_dir.join(name);
        // LOUD, not a fallback (invariant 12). An embedded image that silently resolved to nothing
        // would produce a supervisor that cannot start the service, failing far from the cause.
        if !elf.exists() {
            panic!("supervisor/build.rs: '{}' not found at {} - services must be built before the \
                    supervisor (osdev does this; a bare `cargo build -p supervisor` does not)",
                   name, elf.display());
        }
        println!("cargo:rustc-env=SVC_{}_ELF={}", name.to_uppercase().replace('-', "_"), elf.display());
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
