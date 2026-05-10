/// Emits `cargo:rustc-env=SVC_<NAME>_ELF=<path>` for each service binary.
///
/// `osdev build` compiles the service crates BEFORE the kernel so these
/// paths exist by the time the kernel's `include_bytes!` macros run.
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace = std::path::Path::new(&manifest).parent().unwrap();
    let profile   = std::env::var("PROFILE").unwrap(); // "debug" or "release"

    let target_dir = workspace
        .join("target")
        .join("x86_64-unknown-none")
        .join(&profile);

    // (env-var suffix, binary name in target dir)
    let services: &[(&str, &str)] = &[
        ("INIT",       "init"),
        ("SUPERVISOR", "supervisor"),
        ("REGISTRY",   "registry"),
        ("LOGGER",     "logger"),
        ("PING",       "ping"),
        ("PONG",       "pong"),
    ];

    for (env_name, bin_name) in services {
        let elf = target_dir.join(bin_name);
        println!("cargo:rustc-env=SVC_{}_ELF={}", env_name, elf.display());
        // Rerun if the service binary changes (osdev build rebuilds services first).
        println!("cargo:rerun-if-changed={}", elf.display());
    }
}
