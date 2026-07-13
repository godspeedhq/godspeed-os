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
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GODSPEED_GIT_SHA={}", sha);
    let git_log = workspace.join(".git").join("logs").join("HEAD");
    if git_log.exists() {
        println!("cargo:rerun-if-changed={}", git_log.display());
    }
}
