// SPDX-License-Identifier: Apache-2.0
fn main() {
    // Every userspace service links with the ring-3 service linker script and
    // enters at `service_main`. Identical to every other example/service.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // examples/driver-skeleton -> examples/ -> workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");
}
