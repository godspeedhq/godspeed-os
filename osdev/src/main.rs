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
//!   osdev test chaos        — run §22 chaos / graceful-degradation test suite (Milestone 14)

mod disk_image;
mod qemu;
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
        Commands::Validate           => cmd_validate(),
    }
}

fn cmd_new(name: &str) {
    todo!("scaffold service directory, Cargo.toml, src/main.rs, contracts/{name}.toml from template")
}

pub fn cmd_build() {
    // Services must be compiled before the kernel — kernel/build.rs embeds
    // the service ELF bytes via include_bytes!(env!("SVC_*_ELF")).
    let service_crates = [
        "init", "supervisor", "registry", "logger", "ping", "pong", "probe",
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
        "perf-brutal"   => crate::validator::run_brutal_perf_tests(),
        "adv"      => crate::validator::run_adv_tests(),
        "chaos"    => crate::validator::run_chaos_tests(),
        other => eprintln!("unknown test suite: {}", other),
    }
}

fn cmd_validate() {
    crate::validator::validate_all_contracts();
}
