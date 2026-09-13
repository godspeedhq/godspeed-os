// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // Stamp the short git commit SHA into the build so the `version` command can report the exact
    // build (e.g. "GodspeedOS 0.3.0 (a1b2c3d)"). Best-effort: a checkout with no git, or a build
    // from a tarball, reports "unknown". `.git/logs/HEAD` is appended on every commit/checkout, so
    // watching it re-runs this and refreshes the SHA when HEAD moves.
    //
    // `--short=8` with the length spelled out, and the kernel's stamp (`kernel/build.rs`) matches.
    // A bare `--short` auto-sizes to the shortest prefix unambiguous in the repository it runs
    // against, so the width follows the CLONE rather than the commit: a shallow CI checkout prints
    // 7, a full clone prints 8. `version` and the kernel's boot line are meant to be the same fact
    // stated twice, so neither may vary by how the tree was fetched.
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GODSPEED_GIT_SHA={}", sha);

    // THE PROJECT'S NAME FOR THIS ARCHITECTURE, derived rather than listed.
    //
    // `CARGO_CFG_TARGET_ARCH` is already the answer for every target that exists or ever will, so
    // there is no arch list here and a new port needs no edit - it reports its real name the day it
    // first builds. One rename: Rust calls 32-bit ARMv7 `arm`, and this project calls it **arm32**
    // everywhere else - 95 boot lines in `kernel/src/arch/arm/` print `arm32:`, plus
    // `docs/multi-arch.md`, `docs/arm32-status.md` and the README.
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_else(|_| "unknown".into());
    let arch = if arch == "arm" { "arm32".to_string() } else { arch };
    println!("cargo:rustc-env=GODSPEED_ARCH={arch}");
    let git_log = workspace.join(".git").join("logs").join("HEAD");
    if git_log.exists() {
        println!("cargo:rerun-if-changed={}", git_log.display());
    }
}
