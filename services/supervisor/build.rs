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
        "counter", "shell", "fs", "net-stack", "block-driver", "console", "nic-driver"];

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
    for flag in ["has_xhci", "has_ehci", "has_dwc2", "has_hw_enumerator", "xhci_msi", "nic_on_pci"] {
        println!("cargo::rustc-check-cfg=cfg({flag}, values(none()))");
    }
    for name in usb.iter().chain(enumerator.iter()) {
        println!("cargo:rustc-cfg=has_{}", name.replace('-', "_"));
    }
    // Whether the kernel can route this xHCI an MSI vector from its pool, which is what decides
    // between the `pci_irq` hardware class and the plain one. NOT the same question as "is it on
    // PCI": the Pi 4's VL805 is a PCIe device and still takes the plain class, because what it lacks
    // is the routable vector, not the bus. Asking for an interrupt that can never arrive is the
    // failure invariant 12 exists to prevent, which is why this is its own fact.
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

    for name in EMBEDDED.iter().chain(usb.iter()).chain(enumerator.iter()).chain(probe.iter()) {
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
