// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/block-driver -> services/ -> workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // ---- WHICH STORAGE TOPOLOGY THIS BOARD HAS. --------------------------------------------------
    //
    // This crate asks two questions about its machine, and both used to be asked as an ISA, in EIGHT
    // places across three files:
    //
    //     #[cfg(not(any(target_arch = "arm", target_arch = "aarch64", target_arch = "riscv64")))]
    //
    // repeated six times in `src/main.rs` (module gate, two `backend_run` definitions, the call
    // site), plus `#[cfg(target_arch = "arm")]` in `src/xhciblk.rs` for the host service's NAME, plus
    // an eighth spelling of the same question inline in a log line in `src/usbdisk.rs`.
    //
    // Adding a fifth ISA meant editing all eight, and getting one wrong is a build that either has
    // two backends or none.
    //
    // Neither question is about an instruction set. They are:
    //
    //   * `storage_is_usb`  - does the disk hang off a USB host, or off a controller this service
    //                         drives directly through an MMIO window? (Pi 2, Pi 4 and VisionFive 2
    //                         all boot from their one card slot and take their storage from a USB
    //                         stick; the x86 boxes have AHCI on PCI.)
    //   * `STORAGE_HOST`    - if it is USB, WHICH SERVICE owns the host controller. Same wire format
    //                         either way (`services/xhci/src/msc.rs`), so only the name differs.
    //
    // The ISA is still what answers them, because a board has no runtime identity to ask - the same
    // wall `backlog/21` records for the NIC, and the same answer as the supervisor's `mod board`
    // (`a0392632`): name the fact ONCE, where a reader can see the whole mapping at a time.
    //
    // NOT A CARGO FEATURE, and the difference matters - `src/xhciblk.rs` records why the feature this
    // replaced was a footgun: a feature had to be set by hand in three crates, and setting some of
    // them gave two drivers on one controller. This is DERIVED from the target cargo is already
    // building for, so it cannot be set wrong, cannot be half-set, and needs no flag on any build
    // command. `arm_build.py` and the others pass nothing new.
    println!("cargo::rustc-check-cfg=cfg(storage_is_usb)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let host = match arch.as_str() {
        // Pi 2: the stick hangs off the DesignWare core, driven by the `dwc2` service.
        "arm" => Some("dwc2"),
        // Pi 4 (VL805 over PCIe) and VisionFive 2 (the JH7110's Cadence USB3, host half).
        "aarch64" | "riscv64" => Some("xhci"),
        // x86: AHCI over PCI. This service drives the controller itself through its granted BAR.
        _ => None,
    };
    if let Some(host) = host {
        println!("cargo:rustc-cfg=storage_is_usb");
        println!("cargo:rustc-env=STORAGE_HOST={host}");
    } else {
        // Still defined, so `xhciblk.rs` compiles on x86 (it is built everywhere - a module that only
        // compiles on its own board is a module that quietly rots). Nothing reaches it there: the
        // AHCI backend takes the call and never returns.
        println!("cargo:rustc-env=STORAGE_HOST=none");
    }
}
