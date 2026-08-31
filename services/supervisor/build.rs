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

    // The images this supervisor carries. One entry today (the step-C proof); the rest follow.
    const EMBEDDED: &[&str] = &["pong", "roster", "reply-server", "holder", "upper", "mem-pressure", "ping", "time", "logger"];

    // OUT_DIR is <target>/<triple>/<profile>/build/<pkg>-<hash>/out, so the binaries this build
    // needs sit four levels up. Derived rather than assumed, so it holds for every triple.
    let out = std::env::var("OUT_DIR").unwrap();
    let target_dir = std::path::Path::new(&out)
        .ancestors().nth(3).expect("OUT_DIR shallower than expected").to_path_buf();

    for name in EMBEDDED {
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
