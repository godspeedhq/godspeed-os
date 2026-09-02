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
        "ping", "time", "logger", "asker", "resource-server", "chaos", "control", "observe", "greet",
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
        _         => &[],
    };

    // `hw-enumerator` is x86-only for the same reason, one layer down: its authority is legacy PCI
    // CF8/CFC PORT I/O, and ARM has no port I/O address space at all. Embedding it there would ship a
    // service that comes up, is refused by the kernel on its first config read, says so, and idles -
    // a service that cannot work on the machine it is on. A hardware enumerator for those ports would
    // reach its devices another way (device tree, ECAM), which is a different implementation behind
    // the same service contract.
    // Embedded where configuration space is REACHABLE: x86 through the CF8/CFC ports, aarch64
    // through the Pi 4's memory-mapped INDEX/DATA window. Not arm32 - the Pi 2 has no PCI at all,
    // so there is nothing there for it to read.
    let enumerator: &[&str] =
        if arch == "x86_64" || arch == "aarch64" { &["hw-enumerator"] } else { &[] };

    // OUT_DIR is <target>/<triple>/<profile>/build/<pkg>-<hash>/out, so the binaries this build
    // needs sit four levels up. Derived rather than assumed, so it holds for every triple.
    let out = std::env::var("OUT_DIR").unwrap();
    let target_dir = std::path::Path::new(&out)
        .ancestors().nth(3).expect("OUT_DIR shallower than expected").to_path_buf();

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
