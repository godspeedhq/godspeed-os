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
    // THE OS RELEASE THIS MACHINE RUNS, for `godspeed info`'s "built for": the workspace's version, the one
    // source the shell's `version` reads too. This crate keeps its own version, so it is read here.
    let cargo = workspace.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", cargo.display());
    let text = std::fs::read_to_string(&cargo).unwrap_or_default();
    let mut in_pkg = false;
    let mut release = String::from("unknown");
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') { in_pkg = l == "[workspace.package]"; continue; }
        if in_pkg {
            if let Some(v) = l.strip_prefix("version").map(str::trim).and_then(|v| v.strip_prefix('=')) {
                release = v.trim().trim_matches('"').to_string();
                break;
            }
        }
    }
    println!("cargo:rustc-env=GODSPEED_OS_RELEASE={}", release);
}
