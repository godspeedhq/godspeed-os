// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/godspeed-pm -> services/ -> workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");
    // WHICH BINARY OF A PACKAGE IS THIS MACHINE'S: the triple this program is built for. Each Godspeed target
    // triple begins with its package architecture's name (`x86_64-unknown-none`, `armv7a-none-eabi`, ...), so
    // the program picks the artifact by that prefix and asks no `cfg` (`packages.rs`, `this_arch`).
    println!("cargo:rustc-env=GODSPEED_PM_TARGET={}", std::env::var("TARGET").unwrap());
}
