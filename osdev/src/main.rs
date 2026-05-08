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
//!   osdev test identity     — run §22 identity test suite

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
    /// Restart a running service.
    Restart { service: String },
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
        Commands::Publish { service} => cmd_publish(service.as_deref()),
        Commands::Restart { service} => cmd_restart(&service),
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

fn cmd_build() {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status()
        .expect("failed to run cargo build");
    if !status.success() {
        eprintln!("kernel build failed");
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

fn cmd_restart(service: &str) {
    todo!("connect to running OS control socket; send restart command for service")
}

fn cmd_logs(service: &str) {
    todo!("tail serial output filtered to lines tagged with service name")
}

fn cmd_status(service: &str) {
    todo!("query supervisor IPC endpoint for service state and core assignment")
}

fn cmd_caps(service: &str) {
    todo!("query supervisor for the named service's live cap table")
}

fn cmd_test(suite: &str) {
    match suite {
        "identity" => crate::validator::run_identity_tests(),
        other => eprintln!("unknown test suite: {}", other),
    }
}

fn cmd_validate() {
    crate::validator::validate_all_contracts();
}
