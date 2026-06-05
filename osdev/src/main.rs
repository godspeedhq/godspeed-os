//! `osdev` — host-side developer CLI (§17).
//!
//! Commands:
//!   osdev new <name>        — scaffold a new service
//!   osdev build             — build kernel + all services
//!   osdev run               — boot in QEMU (--smp N)
//!   osdev publish           — package + serve a service
//!   osdev restart <service> — restart a service in the running OS
//!   osdev logs <service>    — tail service logs
//!   osdev status <service>  — show state + assigned core
//!   osdev caps <service>    — show held capabilities
//!   osdev test identity         — run §22 identity test suite (20 tests)
//!   osdev test identity-brutal  — run brutal identity tests + SMP escalation (Milestone 15)
//!   osdev test property         — run §22 property test suite
//!   osdev test property-brutal  — run brutal property tests BP1–BP10 (Milestone 16)
//!   osdev test fuzz         — run §22 fuzz test suite (Milestone 10)
//!   osdev test fuzz-brutal  — run brutal fuzz tests BF1–BF8 (Milestone 17)
//!   osdev test stress       — run §22 stress test suite (Milestone 11)
//!   osdev test stress-brutal — run brutal stress tests BS1–BS10 (Milestone 18)
//!   osdev test perf         — run §22 performance benchmark suite (Milestone 12)
//!   osdev test perf-brutal  — run brutal performance benchmarks BP1–BP10 (Milestone 19)
//!   osdev test adv          — run §22 adversarial / red-team test suite (Milestone 13)
//!   osdev test adv-brutal   — run brutal adversarial tests BA1–BA10 (Milestone 20)
//!   osdev test chaos        — run §22 chaos / graceful-degradation test suite (Milestone 14)
//!   osdev test chaos-brutal — run brutal chaos tests BC1–BC7 (Milestone 21)
//!   osdev test shell        — scripted shell smoke-test (help, cores, status, unknown)
//!   osdev image [--mode M]  — build + create bootable disk image (build/os.img); M=bare-metal|perf|perf-brutal|identity|stress|adv|chaos|s8

mod disk_image;
mod qemu;
mod shell_test;
mod validator;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "osdev", about = "GodspeedOS developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a new service.
    New { name: String },
    /// Build the kernel and all services.
    Build,
    /// Boot the OS in QEMU.
    Run {
        #[arg(long, default_value = "4")]
        smp: u32,
    },
    /// Package and publish a service update.
    Publish { service: Option<String> },
    /// Restart a running service (sends command to OS via control serial port).
    Restart {
        service: String,
        /// Core to restart the service on (§9.2).  Omit for kernel round-robin.
        #[arg(long)]
        core: Option<u32>,
    },
    /// Tail log output for a service.
    Logs { service: String },
    /// Show service state and assigned core.
    Status { service: String },
    /// Show capabilities held by a service.
    Caps { service: String },
    /// Run the identity test suite (§22).
    Test { suite: String },
    /// Build + create bootable disk image at build/os.img without launching QEMU.
    /// Flash to USB with Rufus (DD mode) or `dd`.
    Image {
        /// Supervisor feature baked into the image.
        ///
        /// bare-metal  — pong + ping + observe; no probe services (default; S6 24-hour stability)
        /// perf        — regular perf probes B1–B10
        /// perf-brutal — brutal perf probes BP1–BP10
        /// identity    — identity-only probes (WatchSerial tests; WithRestart needs COM2)
        /// stress      — S1–S10 stress probes; self-contained, no harness required
        /// adv         — A1–A10 adversarial probes; self-contained, no harness required
        /// chaos       — C2–C7 chaos probes; self-contained, no harness required (C1/C4 use bare-metal + HW reconfiguration)
        #[arg(long, default_value = "bare-metal")]
        mode: String,
    },
    /// Boot the OS in QEMU with an interactive shell on stdin/stdout.
    Shell {
        #[arg(long, default_value = "4")]
        smp: u32,
    },
    /// Validate all service contracts against the JSON schema.
    Validate,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::New { name }       => cmd_new(&name),
        Commands::Build              => cmd_build(),
        Commands::Run { smp }        => cmd_run(smp),
        Commands::Publish { service}        => cmd_publish(service.as_deref()),
        Commands::Restart { service, core } => cmd_restart(&service, core),
        Commands::Logs { service }   => cmd_logs(&service),
        Commands::Status { service } => cmd_status(&service),
        Commands::Caps { service }   => cmd_caps(&service),
        Commands::Test { suite }     => cmd_test(&suite),
        Commands::Image { mode }     => cmd_image(&mode),
        Commands::Shell { smp }      => cmd_shell(smp),
        Commands::Validate           => cmd_validate(),
    }
}

fn cmd_new(name: &str) {
    todo!("scaffold service directory, Cargo.toml, src/main.rs, contracts/{name}.toml from template")
}

/// Force a clean rebuild of the supervisor (kernel target) before a build mode runs.
///
/// Every build mode compiles the supervisor with a different spawn-set feature.
/// When switching modes, cargo can return a `supervisor.elf` whose mtime is OLDER
/// than a previously-built kernel, so the kernel's `rerun-if-changed` on
/// `supervisor.elf` never fires and the kernel keeps a STALE embedded supervisor —
/// the resulting image/test then runs the *previous* mode's spawn set. Cleaning
/// guarantees a fresh mtime so the kernel re-embeds the supervisor this mode built.
/// Every `cmd_build_*` calls this first; `cmd_image` therefore does not need to.
fn clean_supervisor() {
    let _ = std::process::Command::new("cargo")
        .args(["clean", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none"])
        .status();
}

pub fn cmd_build() {
    clean_supervisor();
    // Services must be compiled before the kernel — kernel/build.rs embeds
    // the service ELF bytes via include_bytes!(env!("SVC_*_ELF")).
    let service_crates = [
        "init", "supervisor", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci",
    ];
    for crate_name in &service_crates {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Build for bare-metal USB: supervisor with `--features bare-metal` (pong + ping only,
/// no probe services that require the QEMU harness control port to complete).
pub fn cmd_build_bare_metal() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/bare-metal"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (bare-metal) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (bare-metal) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Build for S8 idle-stability run: supervisor with `--features idle-only`.
/// Spawns only observe — no pong, no ping, no probes.  The kernel idles on all
/// cores; observe snapshots system state every ~500 yields.
/// Bar: no panic, no resource leak after 24 hours.
pub fn cmd_build_idle() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/idle-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (idle-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (idle-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build` but compiles supervisor with `--features identity-only`.
/// Used by `run_identity_tests` so the supervisor spawn loop takes < 10 s on
/// TCG instead of 30–200 s with the full 160+ probe service set.
pub fn cmd_build_identity() {
    clean_supervisor();
    // Build every service crate except supervisor first.
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    // Build supervisor with identity-only feature so only the 15 identity
    // probe services are spawned; supervisor: ready appears in < 10 s on TCG.
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/identity-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (identity-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (identity-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build` but compiles supervisor with `--features perf-only`.
/// Spawns only the ~13 regular perf probe services instead of all 178, cutting
/// the TCG spawn-wait from 18–120 s down to ~2–5 s and giving each benchmark
/// maximum headroom before its timeout fires.
pub fn cmd_build_perf() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/perf-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (perf-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (perf-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build_perf` but uses `--features stress-only` for a self-contained
/// hardware stress run (S1–S10). All stress probes use ctx.kill/ctx.spawn
/// internally — no QEMU control port required.
pub fn cmd_build_stress() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/stress-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (stress-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (stress-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build_adv` but uses `--features chaos-only` for a self-contained
/// hardware chaos run (C2–C7). C1 and C4 use bare-metal + hardware reconfiguration.
pub fn cmd_build_chaos() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/chaos-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (chaos-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (chaos-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// B2 isolation build: spawns only perf-b2 + perf-b2-echo alongside pong/ping.
/// Eliminates concurrent IPI noise from other benchmarks (B5 spawn/kill, B6 restart)
/// that triggers the Goldmont+ BSP IPI delivery quirk on the blocking round-trip.
pub fn cmd_build_b2_only() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/b2-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (b2-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (b2-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// BP2 brutal-isolation build: spawns only perf-bp2 + perf-bp2-echo alongside pong/ping.
/// Brutal equivalent of b2-only — 1000-sample iteration count, same isolation rationale.
/// Per-probe isolation build (`perf-iso` umbrella + one `iso-bpN` sub-feature).
/// Spawns exactly one brutal perf probe (+ its partners), no ping/pong, no other
/// probes — for clean, uncontended per-op latency on hardware. `feature` is the
/// supervisor sub-feature, e.g. "iso-bp5".
pub fn cmd_build_perf_iso(feature: &str) {
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    clean_supervisor();
    let sup_feature = format!("supervisor/{}", feature);
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", &sup_feature])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor ({}) FAILED", feature);
        std::process::exit(1);
    }
    println!("build: supervisor ({}) OK", feature);

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

pub fn cmd_build_bp2_only() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/bp2-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (bp2-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (bp2-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build_stress` but uses `--features adv-only` for a self-contained
/// hardware adversarial run (A1–A10). All adversarial probes are self-contained —
/// no QEMU control port required.
pub fn cmd_build_adv() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/adv-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (adv-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (adv-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

/// Like `cmd_build_perf` but uses `--features perf-brutal-only` for the brutal
/// benchmark suite (BP1–BP10).
pub fn cmd_build_brutal_perf() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "probe", "observe", "shell", "xhci", "ehci"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name,
                   "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() {
            eprintln!("build: {} FAILED", crate_name);
            std::process::exit(1);
        }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor",
               "--target", "x86_64-unknown-none",
               "--features", "supervisor/perf-brutal-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (perf-brutal-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (perf-brutal-only) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build for kernel");
    if !status.success() {
        eprintln!("build: kernel FAILED");
        std::process::exit(1);
    }
    println!("build: kernel OK");
}

fn cmd_run(smp: u32) {
    cmd_build();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    qemu::run(&image_path, smp);
}

fn cmd_image(mode: &str) {
    // Each dispatched `cmd_build_*` calls `clean_supervisor()` first, which forces
    // the kernel to re-embed the supervisor this mode built (see that helper for
    // the stale-embed rationale). So no clean is needed here.
    match mode {
        "bare-metal"  => cmd_build_bare_metal(),
        "perf"        => cmd_build_perf(),
        "perf-brutal" => cmd_build_brutal_perf(),
        "identity"    => cmd_build_identity(),
        "stress"      => cmd_build_stress(),
        "adv"         => cmd_build_adv(),
        "chaos"       => cmd_build_chaos(),
        "b2-only"     => cmd_build_b2_only(),
        "bp2-only"    => cmd_build_bp2_only(),
        "iso-bp3"     => cmd_build_perf_iso("iso-bp3"),
        "iso-bp5"     => cmd_build_perf_iso("iso-bp5"),
        "iso-bp7"     => cmd_build_perf_iso("iso-bp7"),
        "iso-bp9"     => cmd_build_perf_iso("iso-bp9"),
        "iso-bp10"    => cmd_build_perf_iso("iso-bp10"),
        "iso-s3"      => cmd_build_perf_iso("iso-s3"),
        "iso-s9"      => cmd_build_perf_iso("iso-s9"),
        "s8"          => cmd_build_idle(),
        other => {
            eprintln!("image: unknown --mode '{}'; valid: bare-metal, perf, perf-brutal, identity, stress, adv, chaos, b2-only, bp2-only, iso-bp3, iso-bp5, iso-bp7, iso-bp9, iso-bp10, iso-s3, iso-s9, s8", other);
            std::process::exit(1);
        }
    }

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = std::path::Path::new("tools/limine");

    let bootx64 = limine_dir.join("BOOTX64.EFI");
    if !bootx64.exists() {
        eprintln!("BOOTX64.EFI not found at {} — UEFI image requires it", bootx64.display());
        std::process::exit(1);
    }

    // UEFI GPT image: no limine bios-install needed.
    let image_path = disk_image::create_uefi(kernel_elf, limine_dir);

    let abs = std::fs::canonicalize(&image_path)
        .unwrap_or_else(|_| image_path.to_path_buf());
    println!("image: [{mode}] ready at {}", abs.display());
    println!("image: flash with Rufus (DD Image mode) or:");
    println!("image:   dd if={} of=/dev/sdX bs=4M status=progress", image_path.display());
}

fn cmd_publish(service: Option<&str>) {
    todo!("build service binary, validate contract, package for osdev restart delivery")
}

/// Connect to the OS control serial port (TCP port 5555) and send a RESTART command.
///
/// The kernel listens on COM2 (mapped to `tcp::5555` by QEMU) and processes
/// `RESTART <service> [<core>]\n` in its scheduler idle loop.
fn cmd_restart(service: &str, core: Option<u32>) {
    use std::io::Write;

    let addr = "127.0.0.1:5555";

    let mut stream = match std::net::TcpStream::connect(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("restart: could not connect to OS control port at {}: {}", addr, e);
            eprintln!("restart: is the OS running? (`osdev run` must be active)");
            std::process::exit(1);
        }
    };

    let cmd = match core {
        Some(c) => format!("RESTART {} {}\n", service, c),
        None    => format!("RESTART {}\n", service),
    };

    if let Err(e) = stream.write_all(cmd.as_bytes()) {
        eprintln!("restart: failed to send command: {}", e);
        std::process::exit(1);
    }

    println!("restart: sent '{}' to OS", cmd.trim());
    println!("restart: watch build/serial.log for confirmation");
}

fn cmd_logs(service: &str) {
    use std::io::{BufRead, Seek, SeekFrom};

    let path = std::path::Path::new(crate::qemu::SERIAL_LOG);

    let mut file = match std::fs::File::open(path) {
        Ok(f)  => f,
        Err(_) => {
            eprintln!("logs: serial log not found at {} — is `osdev run` active?", path.display());
            std::process::exit(1);
        }
    };

    // Seek to end so we only tail new output (like `tail -f`).
    let _ = file.seek(SeekFrom::End(0));

    println!("logs: tailing {} for '{}' (Ctrl-C to stop)", path.display(), service);

    let prefix = format!("{service}:");
    let mut reader = std::io::BufReader::new(file);
    let mut line   = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // No new data yet; wait briefly and retry.
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                if trimmed.contains(&prefix) {
                    println!("{trimmed}");
                }
            }
            Err(e) => {
                eprintln!("logs: read error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn cmd_status(service: &str) {
    todo!("query supervisor IPC endpoint for service state and core assignment")
}

fn cmd_caps(service: &str) {
    todo!("query supervisor for the named service's live cap table")
}

fn cmd_test(suite: &str) {
    match suite {
        "identity"        => crate::validator::run_identity_tests(),
        "identity-brutal" => crate::validator::run_brutal_identity_tests(),
        "property"        => crate::validator::run_property_tests(),
        "property-brutal" => crate::validator::run_brutal_property_tests(),
        "fuzz"        => crate::validator::run_fuzz_tests(),
        "fuzz-brutal" => crate::validator::run_brutal_fuzz_tests(),
        "stress"      => crate::validator::run_stress_tests(),
        "stress-brutal" => crate::validator::run_brutal_stress_tests(),
        "perf"          => crate::validator::run_perf_tests(),
        s if s.starts_with("perf:") => {
            let id = s.trim_start_matches("perf:");
            crate::validator::run_perf_tests_filtered(Some(id));
        }
        "perf-brutal"   => crate::validator::run_brutal_perf_tests(),
        "adv"        => crate::validator::run_adv_tests(),
        "adv-brutal" => crate::validator::run_brutal_adv_tests(),
        "chaos"        => crate::validator::run_chaos_tests(),
        "chaos-brutal" => crate::validator::run_chaos_brutal_tests(),
        "shell"        => run_shell_test(),
        other => eprintln!("unknown test suite: {}", other),
    }
}

/// Boot the OS in QEMU with stdin/stdout wired to COM1 for the shell service.
///
/// Uses `-serial stdio` (bidirectional) so the shell's console_read syscall
/// receives bytes typed in the terminal, and shell output (via ctx.log) appears
/// on stdout. The control port (COM2) is still on TCP:5555 for `osdev restart`.
fn cmd_shell(smp: u32) {
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    qemu::run_shell(&image_path, smp);
}

/// Build bare-metal image and run the scripted shell smoke-test.
fn run_shell_test() {
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    crate::shell_test::run(&image_path, 4);
}

fn cmd_validate() {
    crate::validator::validate_all_contracts();
}
