// SPDX-License-Identifier: GPL-2.0-only
//! Link this service, and embed the dongle's firmware in its own binary - for the reasons
//! `services/wifi-driver/build.rs` gives at length: a live system carries what it uploads, and reading it
//! through `fs` would make the radio depend on storage. One file today, `rtl8192cufw_TMSC.bin`
//! (`nonfree/rtl8192cu/PROVENANCE`), with an FNV-1a hash passed through so the service can check at start
//! that the bytes really reached the binary.

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/wifi-usb -> services/ -> workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    let path = workspace.join("nonfree").join("rtl8192cu").join("rtl8192cufw_TMSC.bin");
    if !path.exists() {
        panic!(
            "wifi-usb: {} is missing. It is the RTL8188CUS's 8051 firmware, vendored in this repository - see \
             nonfree/rtl8192cu/PROVENANCE. Without it the dongle cannot be brought past power-on.",
            path.display()
        );
    }
    println!("cargo:rustc-env=RTL_FW_TMSC={}", path.display());
    println!("cargo:rerun-if-changed={}", path.display());
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("wifi-usb: cannot read {}: {}", path.display(), e));
    // FNV-1a, as `wifi-driver` does it: the service recomputes it over what it embedded, which is the only
    // check that a blob reached the binary (a length is a constant whether or not the bytes are kept).
    let mut h: u32 = 0x811c_9dc5;
    for b in &bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    println!("cargo:rustc-env=RTL_FW_TMSC_FNV={}", h);
}
