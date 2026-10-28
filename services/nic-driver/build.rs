// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/nic-driver → services/ → workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // WHICH BACKENDS THIS IMAGE CARRIES - asked of the instruction set ONCE, here, and only for image
    // size. Which one RUNS is the board's answer, read at runtime from the kernel's boot probe
    // (`soc_nic`, InspectKernel query 29; `backlog/fixed/21`). Each SoC backend is a whole MAC - GENET is
    // ~1,450 lines - and compiling every one into every image would be dead weight in a service with a
    // 16 MiB limit. The PCI backends (e1000, RTL8168) are small and in every image.
    for fact in ["nic_genet", "nic_dwmac", "nic_usb_bridge"] {
        println!("cargo::rustc-check-cfg=cfg({}, values(none()))", fact);
    }
    let carried: &[&str] = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => &["nic_genet"],      // the Pi 4's GENET
        Ok("riscv64") => &["nic_dwmac"],      // the VisionFive 2's DesignWare MAC
        Ok("arm") => &["nic_usb_bridge"],     // the Pi 2's LAN9514, behind the `dwc2` service
        _ => &[],
    };
    for fact in carried {
        println!("cargo:rustc-cfg={}", fact);
    }
}
