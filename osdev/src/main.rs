// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

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
//!   osdev image [--mode M]  — build + create bootable disk image (build/os.img); M=bare-metal|perf|perf-brutal|identity|stress|adv|chaos|fuzz|s8

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
    /// Format a disk image with a GodspeedOS filesystem superblock (docs/persistence.md §6).
    Mkfs { image: String },
    /// Build a flashable GSFS data disk with a `.gs` script baked in (run it on hardware).
    ScriptDisk { out: String, script: String },
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
        Commands::Mkfs { image }     => cmd_mkfs(&image),
        Commands::ScriptDisk { out, script } => cmd_script_disk(&out, &script),
    }
}

// On-disk format — MUST match `services/fs` (docs/persistence.md §6.4, GSFS0003).
// 512-byte blocks (= one AHCI sector = one block-IPC request), so block number = LBA.
// Three structures: superblock + free bitmap + self-describing directory tree (no inode
// table, no global file cap). All capacity-bearing fields are u64.
//   Superblock @ LBA 0: magic[8] "GSFS0003", version u32, block_size u32=512,
//     total_blocks u64, bitmap_start u64=1, bitmap_blocks u64, data_start u64,
//     root_first_block u64, root_block_count u64, free_blocks u64, flags u32,
//     label_len u8, label[31].
//   Free bitmap @ LBA bitmap_start..data_start: 1 bit/block (set=used), 4096 bits/block.
//   Directory entry (file_record, 64 B): type u8 (0 free|1 file|2 dir) @0, name_len u8 @1,
//     name[38] @2, size u64 @40, first_block u64 @48, block_count u64 @56. 8 per block.
//   Root is a dir at root_first_block (its extent lives in the superblock; it has no parent).
const FS_SB_MAGIC: &[u8; 8] = b"GSFS0003";
const FS_BLOCK_SIZE: u32 = 512;
const FS_BITS_PER_BMBLOCK: u64 = (FS_BLOCK_SIZE as u64) * 8; // 4096
const FS_ITYPE_DIR: u8 = 2;

/// Write a GodspeedOS (GSFS0003) superblock + free bitmap + an empty root directory into
/// `path`, preserving the rest of the image. Geometry is derived from the image size.
fn format_superblock(path: &str) {
    let mut data = std::fs::read(path)
        .unwrap_or_else(|e| { eprintln!("mkfs: cannot read {}: {}", path, e); std::process::exit(1); });
    let total_blocks = data.len() as u64 / FS_BLOCK_SIZE as u64;
    let bitmap_start: u64 = 1;
    let bitmap_blocks = (total_blocks + FS_BITS_PER_BMBLOCK - 1) / FS_BITS_PER_BMBLOCK; // ceil
    let data_start = bitmap_start + bitmap_blocks;
    let root_first_block = data_start;
    let root_block_count: u64 = 1;
    let used_through = data_start + root_block_count; // blocks [0..used_through) are used
    if total_blocks < used_through + 1 {
        eprintln!("mkfs: image too small ({} bytes)", data.len());
        std::process::exit(1);
    }
    let free_blocks = total_blocks - used_through;

    // Superblock (LBA 0).
    let mut sb = [0u8; 512];
    sb[0..8].copy_from_slice(FS_SB_MAGIC);
    sb[8..12].copy_from_slice(&3u32.to_le_bytes());            // version
    sb[12..16].copy_from_slice(&FS_BLOCK_SIZE.to_le_bytes());  // block_size
    sb[16..24].copy_from_slice(&total_blocks.to_le_bytes());
    sb[24..32].copy_from_slice(&bitmap_start.to_le_bytes());
    sb[32..40].copy_from_slice(&bitmap_blocks.to_le_bytes());
    sb[40..48].copy_from_slice(&data_start.to_le_bytes());
    sb[48..56].copy_from_slice(&root_first_block.to_le_bytes());
    sb[56..64].copy_from_slice(&root_block_count.to_le_bytes());
    sb[64..72].copy_from_slice(&free_blocks.to_le_bytes());
    sb[72..76].copy_from_slice(&0u32.to_le_bytes());           // flags (DEFAULT clear)
    sb[76] = 0;                                                // label_len
    data[0..512].copy_from_slice(&sb);

    // Zero the bitmap region, then mark blocks [0..used_through) used (superblock +
    // bitmap + the root directory block).
    let bm = (bitmap_start as usize) * 512;
    let bm_end = (data_start as usize) * 512;
    for b in &mut data[bm..bm_end] { *b = 0; }
    for blk in 0..used_through as usize {
        data[bm + blk / 8] |= 1 << (blk % 8);
    }

    // Zero the root directory block (no entries yet).
    let rd = (root_first_block as usize) * 512;
    for b in &mut data[rd..rd + 512] { *b = 0; }

    std::fs::write(path, &data)
        .unwrap_or_else(|e| { eprintln!("mkfs: cannot write {}: {}", path, e); std::process::exit(1); });
    println!("mkfs: formatted {} GSFS0003 ({} blocks, bitmap {}..{}, data from {}, {} free)",
             path, total_blocks, bitmap_start, data_start, root_first_block, free_blocks);
}

fn cmd_mkfs(image: &str) {
    format_superblock(image);
}

/// Bake a file into a GSFS0003 image (host-side mirror of the `fs` write path) — used to ship a
/// `.gs` script on a flashable data disk, so the OS can `run /suite.gs` on hardware with no
/// on-device authoring. Allocates a contiguous extent, writes the content, adds a root
/// `file_record`, and updates the free count. Intended right after `format_superblock` (minimal,
/// unfragmented layout). `name` ≤ 38 bytes; fits in the single root directory block (8 entries).
fn gsfs_add_file(path: &str, name: &str, content: &[u8]) {
    let mut data = std::fs::read(path)
        .unwrap_or_else(|e| { eprintln!("bake: cannot read {}: {}", path, e); std::process::exit(1); });
    if data.len() < 512 || &data[0..8] != FS_SB_MAGIC {
        eprintln!("bake: {} is not a GSFS0003 image", path); std::process::exit(1);
    }
    if name.len() > 38 { eprintln!("bake: name '{}' too long (max 38)", name); std::process::exit(1); }
    let rdu = |d: &[u8], o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let total_blocks = rdu(&data, 16);
    let bitmap_start = rdu(&data, 24) as usize;
    let data_start   = rdu(&data, 40);
    let root_first   = rdu(&data, 48) as usize;
    let mut free_blocks = rdu(&data, 64);

    let nblocks = ((content.len() as u64) + 511) / 512;
    // First run of `nblocks` free, contiguous blocks at/after data_start (set bit = used).
    let mut first = data_start.max(1);
    loop {
        if first + nblocks > total_blocks {
            eprintln!("bake: image too small for /{} ({} blocks needed)", name, nblocks);
            std::process::exit(1);
        }
        let mut all_free = true;
        for k in 0..nblocks {
            let blk = first + k;
            if data[bitmap_start * 512 + (blk / 8) as usize] & (1u8 << (blk % 8)) != 0 {
                all_free = false; break;
            }
        }
        if all_free { break; }
        first += 1;
    }
    // Mark the extent used; update the free count.
    for k in 0..nblocks {
        let blk = first + k;
        data[bitmap_start * 512 + (blk / 8) as usize] |= 1u8 << (blk % 8);
    }
    free_blocks -= nblocks;
    data[64..72].copy_from_slice(&free_blocks.to_le_bytes());
    // Write the content into the extent (the tail of the last block stays zero).
    let off = (first as usize) * 512;
    data[off..off + content.len()].copy_from_slice(content);
    // Add a root file_record into the first free 64-byte slot (type 0 = free).
    let rd = root_first * 512;
    let mut placed = false;
    for slot in 0..8 {
        let r = rd + slot * 64;
        if data[r] == 0 {
            data[r] = 1;                                    // type = file
            data[r + 1] = name.len() as u8;                 // name_len
            data[r + 2..r + 2 + name.len()].copy_from_slice(name.as_bytes());
            data[r + 40..r + 48].copy_from_slice(&(content.len() as u64).to_le_bytes()); // size
            data[r + 48..r + 56].copy_from_slice(&first.to_le_bytes());                  // first_block
            data[r + 56..r + 64].copy_from_slice(&nblocks.to_le_bytes());                // block_count
            placed = true;
            break;
        }
    }
    if !placed { eprintln!("bake: root directory is full"); std::process::exit(1); }
    std::fs::write(path, &data)
        .unwrap_or_else(|e| { eprintln!("bake: cannot write {}: {}", path, e); std::process::exit(1); });
    println!("bake: wrote /{} ({} bytes, {} block(s) at {}) into {}", name, content.len(), nblocks, first, path);
}

/// `osdev script-disk <out> <script>` — produce a flashable GSFS data disk with `<script>` baked
/// in as `/<basename>`. Boot the OS with this disk attached and `run /<basename>` — the way to
/// ship a self-checking suite to hardware (`dd` it to the data drive). Default 16 MiB.
fn cmd_script_disk(out: &str, script: &str) {
    let content = std::fs::read(script)
        .unwrap_or_else(|e| { eprintln!("script-disk: cannot read {}: {}", script, e); std::process::exit(1); });
    let name = std::path::Path::new(script).file_name()
        .and_then(|s| s.to_str()).unwrap_or("suite.gs");
    if let Some(parent) = std::path::Path::new(out).parent() { let _ = std::fs::create_dir_all(parent); }
    std::fs::write(out, vec![0u8; 16 * 1024 * 1024])
        .unwrap_or_else(|e| { eprintln!("script-disk: cannot create {}: {}", out, e); std::process::exit(1); });
    format_superblock(out);
    gsfs_add_file(out, name, &content);
    println!("script-disk: {} ready — flash it to the data drive, then `run /{}`", out, name);
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
        "init", "supervisor", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs",
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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

/// Like `cmd_build_stress` but uses `--features fuzz-only` for a self-contained
/// hardware fuzz run (§22 F1/F2/F5/F6/F7/F8 + brutal BF1/BF2/BF5/BF6/BF7/BF8). All
/// fuzz probes self-run and print "fuzz: F* pass (n/n)" over serial with no QEMU
/// control port. F3/BF3 (ELF-loader fuzz) need a separate test-bad-elf kernel build;
/// F4 is host-side contract validation. Watch COM1: a clean run shows every
/// "fuzz: F* pass" line and never "KERNEL PANIC".
pub fn cmd_build_fuzz() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
               "--features", "supervisor/fuzz-only"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() {
        eprintln!("build: supervisor (fuzz-only) FAILED");
        std::process::exit(1);
    }
    println!("build: supervisor (fuzz-only) OK");

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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
        "iommu-fault" => cmd_build_iommu_fault(),
        "blockdev"    => cmd_build_blockdev(),
        "perf"        => cmd_build_perf(),
        "perf-brutal" => cmd_build_brutal_perf(),
        "identity"    => cmd_build_identity(),
        "stress"      => cmd_build_stress(),
        "adv"         => cmd_build_adv(),
        "chaos"       => cmd_build_chaos(),
        "fuzz"        => cmd_build_fuzz(),
        "b2-only"     => cmd_build_b2_only(),
        "bp2-only"    => cmd_build_bp2_only(),
        "iso-bp3"     => cmd_build_perf_iso("iso-bp3"),
        "iso-bp5"     => cmd_build_perf_iso("iso-bp5"),
        "iso-bp7"     => cmd_build_perf_iso("iso-bp7"),
        "iso-bp9"     => cmd_build_perf_iso("iso-bp9"),
        "iso-bp10"    => cmd_build_perf_iso("iso-bp10"),
        "iso-s3"      => cmd_build_perf_iso("iso-s3"),
        "iso-s5"      => cmd_build_perf_iso("iso-s5"),
        "iso-s9"      => cmd_build_perf_iso("iso-s9"),
        "iso-c7"      => cmd_build_perf_iso("iso-c7"),
        "iso-xsend"   => cmd_build_perf_iso("iso-xsend"),
        "iso-xlife"   => cmd_build_perf_iso("iso-xlife"),
        "iso-reg"     => cmd_build_perf_iso("iso-reg"),
        "s8"          => cmd_build_idle(),
        other => {
            eprintln!("image: unknown --mode '{}'; valid: bare-metal, iommu-fault, blockdev, perf, perf-brutal, identity, stress, adv, chaos, fuzz, b2-only, bp2-only, iso-bp3, iso-bp5, iso-bp7, iso-bp9, iso-bp10, iso-s3, iso-s5, iso-s9, iso-c7, iso-xsend, iso-xlife, iso-reg, s8", other);
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
        "iommu"        => run_iommu_test(),
        "blockdev"     => run_blockdev_test(),
        "blockdev-reboot" => run_blockdev_reboot_test(),
        "drives-raw"   => run_drives_raw_test(),
        "drives"       => run_drives_scripted_test(),
        "files"        => run_files_test(),
        "script"       => run_script_test(),
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

/// Build like bare-metal, but compile the kernel with `--features
/// iommu-fault-test` so the first confined driver (xhci) is confined to an EMPTY
/// IOMMU domain — its init DMA then faults, the deterministic live proof for
/// §22 Test 12 / H1 §6.4.
fn cmd_build_iommu_fault() {
    clean_supervisor();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name, "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() { eprintln!("build: {} FAILED", crate_name); std::process::exit(1); }
        println!("build: {} OK", crate_name);
    }
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor", "--target", "x86_64-unknown-none",
               "--features", "supervisor/bare-metal"])
        .status().unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() { eprintln!("build: supervisor (bare-metal) FAILED"); std::process::exit(1); }
    println!("build: supervisor (bare-metal) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none",
               "--features", "kernel/iommu-fault-test"])
        .status().expect("failed to run cargo build for kernel");
    if !status.success() { eprintln!("build: kernel (iommu-fault-test) FAILED"); std::process::exit(1); }
    println!("build: kernel (iommu-fault-test) OK");
}

/// §22 Test 12 (H1 §6.4): boot with an emulated AMD-Vi IOMMU and a USB device on
/// qemu-xhci, and verify the confinement guarantee structurally on a live, enabled
/// IOMMU — the confined driver's I/O page table maps **exactly** its arena and the
/// page just outside it is **unmapped** (so an out-of-arena DMA has no translation
/// and would fault), AND the driver still operates *through* the confined domain
/// (its keyboard enumerates), AND the kernel does not panic.
///
/// QEMU's `amd-iommu` does not actually *enforce* translation faults (it is lenient
/// where real AMD-Vi is strict), so the live `IO_PAGE_FAULT` cannot be reproduced in
/// emulation — it is hardware-verified on the T630 (and reproducible there with the
/// kernel `iommu-fault-test` feature, which confines xhci to an empty domain). The
/// `selftest` line is a CPU-side page-table walk QEMU cannot fake, so it is the
/// portable executable form of the guarantee. Requires q35 + `-device amd-iommu`,
/// which the BIOS/i440fx test path can't provide, so this launches QEMU itself.
fn run_iommu_test() {
    println!("\n=== §22 Test 12: confined driver — out-of-arena is unmapped (H1 §6.4) ===");
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let serial = "build/tests/iommu_test_serial.log";
    let _ = std::fs::remove_file(serial);
    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");

    let mut cmd = std::process::Command::new(qemu::qemu_binary());
    cmd.args([
        "-machine", "q35", "-m", "512M", "-smp", "2",
        "-device", "amd-iommu",
        "-device", "qemu-xhci,id=xhci",
        "-device", "usb-kbd,bus=xhci.0",
        "-drive", &format!("format=raw,file={img_str},if=ide"),
        "-serial", &format!("file:{serial}"),
        "-serial", "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]);
    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("iommu: failed to launch QEMU: {e}"); std::process::exit(1); });
    println!("iommu: booting (q35 + amd-iommu + qemu-xhci + usb-kbd), ~28s …");
    std::thread::sleep(std::time::Duration::from_secs(28));
    let _ = child.kill();
    let _ = child.wait();

    let log = std::fs::read_to_string(serial).unwrap_or_default();
    let log = log.replace('\r', "");
    let confined      = log.contains("confined BDF");
    // The selftest proves the confined page table maps exactly the arena and the
    // page just outside it is unmapped — the structural form of "out-of-arena DMA
    // would fault".
    let outside_unmapped = log.contains("selftest PASS") && log.contains("(outside) unmapped");
    let works_confined   = log.contains("keyboard found");          // driver operates through the domain
    let no_panic         = !log.contains("KERNEL PANIC");

    let sel = log.lines().find(|l| l.contains("selftest PASS")).unwrap_or("(none)");
    println!("iommu:   driver confined to its arena ... {}", if confined { "yes" } else { "NO" });
    println!("iommu:   out-of-arena UNMAPPED (would fault) ... {}", if outside_unmapped { "yes" } else { "NO" });
    println!("iommu:   driver operates through the confined domain ... {}", if works_confined { "yes" } else { "NO" });
    println!("iommu:   kernel did not panic ... {}", if no_panic { "yes" } else { "NO" });
    println!("iommu:   selftest: {}", sel.trim());

    if confined && outside_unmapped && works_confined && no_panic {
        println!("\n  [12]  confined_driver_dma_faults  (§22 Test 12)  … PASS\n\n  1 passed  0 failed");
    } else {
        println!("\n  [12]  confined_driver_dma_faults  (§22 Test 12)  … FAIL\n\n  0 passed  1 failed");
        std::process::exit(1);
    }
}

/// Boot the blockdev image once with `persist` on the ATA secondary channel,
/// capture the serial log, and return it. The persist disk is NOT recreated —
/// the caller controls its lifecycle (key for the reboot-survival test).
fn boot_blockdev_qemu(img_str: &str, persist_str: &str, serial: &str, secs: u64) -> String {
    let _ = std::fs::remove_file(serial);
    let mut cmd = std::process::Command::new(qemu::qemu_binary());
    cmd.args([
        "-m", "512M", "-smp", "2",
        "-drive", &format!("format=raw,file={img_str},if=ide,index=0"),
        "-drive", &format!("format=raw,file={persist_str},if=ide,index=2"),
        "-serial", &format!("file:{serial}"),
        "-serial", "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]);
    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("blockdev: failed to launch QEMU: {e}"); std::process::exit(1); });
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let _ = child.kill();
    let _ = child.wait();
    std::fs::read_to_string(serial).unwrap_or_default().replace('\r', "")
}

/// Build the AHCI block-driver variant: block-driver with its `ahci` feature,
/// supervisor spawns block-driver + fs (bare-metal,blockdev). AHCI by default.
fn cmd_build_blockdev() {
    clean_supervisor();
    // Force a fresh `fs` so its `selftest` feature (added below) is compiled in even if a
    // prior plain build cached it — otherwise the blockdev tests miss the self-test logs.
    let _ = std::process::Command::new("cargo")
        .args(["clean", "--release", "-p", "fs", "--target", "x86_64-unknown-none"]).status();
    let non_supervisor = ["init", "registry", "logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver"];
    for crate_name in &non_supervisor {
        let status = std::process::Command::new("cargo")
            .args(["build", "--release", "-p", crate_name, "--target", "x86_64-unknown-none"])
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() { eprintln!("build: {} FAILED", crate_name); std::process::exit(1); }
        println!("build: {} OK", crate_name);
    }
    // fs WITH the selftest feature — the blockdev tests assert its self-test log lines.
    // (Production `osdev image` builds fs without it, so it never writes to a real disk.)
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "fs", "--target", "x86_64-unknown-none", "--features", "selftest"])
        .status().unwrap_or_else(|e| panic!("failed to run cargo build for fs: {}", e));
    if !status.success() { eprintln!("build: fs FAILED"); std::process::exit(1); }
    println!("build: fs (selftest) OK");
    // Spawn block-driver + fs (blockdev) so fs mounts the AHCI disk over IPC.
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "supervisor", "--target", "x86_64-unknown-none",
               "--features", "supervisor/bare-metal,supervisor/blockdev"])
        .status().unwrap_or_else(|e| panic!("failed to run cargo build for supervisor: {}", e));
    if !status.success() { eprintln!("build: supervisor FAILED"); std::process::exit(1); }
    println!("build: supervisor (bare-metal,blockdev) OK");

    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "kernel", "--target", "x86_64-unknown-none"])
        .status().expect("failed to run cargo build for kernel");
    if !status.success() { eprintln!("build: kernel FAILED"); std::process::exit(1); }
    println!("build: kernel OK");
}

/// AHCI steps A-D (docs/ahci.md): boot from a legacy-IDE disk, put the persistence
/// disk ALONE on an ich9-ahci controller (so block-driver targets it on port 0 —
/// mirroring the T630, where the SSD is the only SATA disk and boot is elsewhere),
/// and verify detection, IDENTIFY, and the full fs stack (mount + file round-trip)
/// running over AHCI READ/WRITE DMA EXT.
fn run_blockdev_test() {
    println!("\n=== AHCI steps A-D: detect + IDENTIFY + fs over AHCI ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    // Persistence disk, formatted, ALONE on the AHCI controller (→ block-driver
    // port 0). The boot image is on legacy IDE so it is not a SATA disk.
    let persist = "build/tests/persist_ahci.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create persist disk");
    format_superblock(persist);

    let serial = "build/tests/ahci_test_serial.log";
    let _ = std::fs::remove_file(serial);
    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    let mut cmd = std::process::Command::new(qemu::qemu_binary());
    cmd.args([
        "-m", "512M", "-smp", "2",
        // Boot disk on legacy IDE (PIIX3) — SeaBIOS boots it; it is NOT SATA so
        // block-driver's AHCI scan ignores it.
        "-drive", &format!("format=raw,file={img_str},if=ide"),
        // The persistence disk ALONE on an explicit AHCI controller → port 0.
        "-device", "ich9-ahci,id=ahci",
        "-drive", &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device", "ide-hd,drive=data,bus=ahci.0",
        "-serial", &format!("file:{serial}"),
        "-serial", "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]);
    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("ahci: failed to launch QEMU: {e}"); std::process::exit(1); });
    println!("ahci: booting (IDE boot + ich9-ahci data disk), ~25s …");
    std::thread::sleep(std::time::Duration::from_secs(25));
    let _ = child.kill();
    let _ = child.wait();

    let log = std::fs::read_to_string(serial).unwrap_or_default().replace('\r', "");
    let pci_found  = log.contains("pci: AHCI at");
    let identify   = log.contains("IDENTIFY OK");
    let serving    = log.contains("AHCI serving block I/O");
    let fs_mounted = log.contains("fs: mounted GSFS");            // step C: AHCI ReadBlock
    let fs_file    = log.contains("fs: file round-trip OK")       // step D: AHCI WriteBlock+ReadBlock
        || log.contains("fs: persisted file 'greeting' verified");
    // step E: hierarchical GSFS — mkdir + a file nested inside it, walked by path.
    let fs_hier = (log.contains("fs: mkdir /etc OK") && log.contains("fs: nested file round-trip OK (/etc/motd)"))
        || log.contains("fs: nested '/etc/motd' verified across boot");
    let no_panic   = !log.contains("KERNEL PANIC");

    for l in log.lines().filter(|l| l.contains("AHCI") || l.contains("block-driver") || l.contains("fs:")) {
        println!("ahci:   | {}", l.trim());
    }
    println!("ahci:   detect + IDENTIFY ... {}", if pci_found && identify { "yes" } else { "NO" });
    println!("ahci:   serving block I/O ... {}", if serving { "yes" } else { "NO" });
    println!("ahci:   fs mounted (AHCI ReadBlock) ... {}", if fs_mounted { "yes" } else { "NO" });
    println!("ahci:   fs file round-trip (AHCI WriteBlock+ReadBlock) ... {}", if fs_file { "yes" } else { "NO" });
    println!("ahci:   fs hierarchy (mkdir + nested path) ... {}", if fs_hier { "yes" } else { "NO" });
    println!("ahci:   kernel did not panic ... {}", if no_panic { "yes" } else { "NO" });

    let ab = if pci_found && identify && no_panic { "PASS" } else { "FAIL" };
    let c  = if fs_mounted && no_panic { "PASS" } else { "FAIL" };
    let d  = if fs_file && no_panic { "PASS" } else { "FAIL" };
    let e  = if fs_hier && no_panic { "PASS" } else { "FAIL" };
    println!("\n  [AHCI.A/B]  detect + port init + IDENTIFY        … {ab}");
    println!("  [AHCI.C]    read (fs mounts over AHCI)            … {c}");
    println!("  [AHCI.D]    write (fs file round-trip over AHCI)  … {d}");
    println!("  [GSFS.E]    hierarchy (mkdir + nested path walk)  … {e}");
    let passed = (ab == "PASS") as u32 + (c == "PASS") as u32 + (d == "PASS") as u32 + (e == "PASS") as u32;
    println!("\n  {passed} passed  {} failed", 4 - passed);
    if passed != 4 {
        std::process::exit(1);
    }
}

/// Boot the AHCI image once with `persist` alone on an ich9-ahci controller and
/// the boot image on legacy IDE; capture and return the serial log.
fn boot_ahci_qemu(img_str: &str, persist_str: &str, serial: &str, secs: u64) -> String {
    let _ = std::fs::remove_file(serial);
    let mut cmd = std::process::Command::new(qemu::qemu_binary());
    cmd.args([
        "-m", "512M", "-smp", "2",
        "-drive", &format!("format=raw,file={img_str},if=ide"),
        "-device", "ich9-ahci,id=ahci",
        "-drive", &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device", "ide-hd,drive=data,bus=ahci.0",
        "-serial", &format!("file:{serial}"),
        "-serial", "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]);
    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("ahci: failed to launch QEMU: {e}"); std::process::exit(1); });
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let _ = child.kill();
    let _ = child.wait();
    std::fs::read_to_string(serial).unwrap_or_default().replace('\r', "")
}

/// AHCI reboot survival: format once, boot (fs writes `greeting` over AHCI), then
/// reboot on the SAME SATA disk image — fs must read it back over AHCI.
fn run_blockdev_reboot_test() {
    println!("\n=== AHCI reboot survival (write → reboot → read, over SATA) ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_ahci_reboot.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create persist disk");
    format_superblock(persist);

    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    println!("ahci: boot 1 — fs creates 'greeting' over AHCI, ~25s …");
    let log1 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/ahci_reboot_1.log", 25);
    let created = log1.contains("fs: file round-trip OK (greeting)");

    println!("ahci: boot 2 — SAME SATA disk, no reformat, fs reads it back, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/ahci_reboot_2.log", 25);
    // Match the END of the line — the "fs:" prefix can be clobbered by a concurrent
    // shell write on the shared serial (cosmetic interleaving), but the tail is safe.
    let survived = log2.contains("verified across boot");
    for l in log2.lines().filter(|l| l.contains("fs:") || l.contains("IDENTIFY")) {
        println!("ahci:   boot2 | {}", l.trim());
    }
    let panic = log1.contains("KERNEL PANIC") || log2.contains("KERNEL PANIC");
    println!("ahci:   boot 1 created 'greeting' ... {}", if created { "yes" } else { "NO" });
    println!("ahci:   boot 2 read it back from SATA ... {}", if survived { "yes" } else { "NO" });

    if created && survived && !panic {
        println!("\n  [AHCI.R]  reboot survival over SATA  … PASS\n\n  1 passed  0 failed");
    } else {
        println!("\n  [AHCI.R]  reboot survival over SATA  … FAIL\n\n  0 passed  1 failed");
        std::process::exit(1);
    }
}

/// Step 3a: raw-disk tolerance. Boot with an UNFORMATTED disk on the AHCI controller
/// (no `mkfs`): `fs` must learn the disk's capacity (OP_CAPACITY), recognise there is no
/// filesystem, NOT auto-format (§3.12), and stay up serving — never panic, never hang.
fn run_drives_raw_test() {
    println!("\n=== drives 3a: raw-disk tolerance (capacity + no auto-format) ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    // A RAW disk — zeros, deliberately NOT formatted with `format_superblock`.
    let persist = "build/tests/persist_ahci_raw.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create raw disk");

    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    println!("drives: booting with a RAW (unformatted) AHCI disk, ~25s …");
    let log = boot_ahci_qemu(&img_str, &persist_str, "build/tests/drives_raw.log", 25);

    let capacity = log.contains("fs: disk capacity =");           // OP_CAPACITY round-trip
    let no_fs    = log.contains("fs: no filesystem") && log.contains("awaiting drives flash");
    let serving  = log.contains("fs: serving file API");          // fs stayed up
    let not_formatted = !log.contains("fs: mounted GSFS");         // did NOT auto-format
    let no_panic = !log.contains("KERNEL PANIC");

    for l in log.lines().filter(|l| l.contains("fs:") || l.contains("IDENTIFY OK")) {
        println!("drives:   | {}", l.trim());
    }
    println!("drives:   learned capacity (OP_CAPACITY) ... {}", if capacity { "yes" } else { "NO" });
    println!("drives:   recognised raw disk, no auto-format ... {}", if no_fs && not_formatted { "yes" } else { "NO" });
    println!("drives:   fs stayed up serving ... {}", if serving { "yes" } else { "NO" });
    println!("drives:   kernel did not panic ... {}", if no_panic { "yes" } else { "NO" });

    if capacity && no_fs && serving && not_formatted && no_panic {
        println!("\n  [DRIVES.raw]  raw-disk tolerance  … PASS\n\n  1 passed  0 failed");
    } else {
        println!("\n  [DRIVES.raw]  raw-disk tolerance  … FAIL\n\n  0 passed  1 failed");
        std::process::exit(1);
    }
}

/// Step 3b: scripted `drives` end-to-end. Build the bare-metal image (now spawns
/// block-driver + fs before the shell), attach a RAW AHCI disk, and script
/// `drives` → `drives flash data` (with [y/N] confirm) → list → `drives label`.
fn run_drives_scripted_test() {
    println!("\n=== drives 3b: flash + label from the shell (RAW AHCI disk) ===");
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_drives_raw.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create raw disk");

    crate::shell_test::run_drives(&image_path, persist, 4);
}

/// Step 4: scripted file commands (ls/read/write/mkdir/cd). Build bare-metal, attach a
/// RAW AHCI disk, flash it, then exercise the file commands incl. relative paths + `..`.
fn run_files_test() {
    println!("\n=== files 4: ls / read / write / mkdir / cd (RAW AHCI disk) ===");
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_files_raw.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create raw disk");

    crate::shell_test::run_files(&image_path, persist, 4);
}

/// Host-baked-script test: bake a self-checking `.gs` suite into a GSFS data disk (the way a
/// suite ships to hardware), boot with it attached, and `run /suite.gs`. Proves the whole
/// flash-and-run loop AND piped asserts inside a script (which can't be authored on-device).
fn run_script_test() {
    println!("\n=== script: a host-baked .gs suite, run on boot (GSFS AHCI disk) ===");
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    // The suite is the SAME file you bake for hardware (`scripts/t630_selfcheck.gs`) — CI verifies
    // exactly what flashes. It creates its own files (a freshly-baked disk has only the suite).
    let script_path = "scripts/t630_selfcheck.gs";
    let suite = std::fs::read(script_path)
        .unwrap_or_else(|e| { eprintln!("script-test: cannot read {}: {}", script_path, e); std::process::exit(1); });
    let name = std::path::Path::new(script_path).file_name().and_then(|s| s.to_str()).unwrap_or("suite.gs");

    let _ = std::fs::create_dir_all("build/tests");
    let disk = "build/tests/script_disk.img";
    std::fs::write(disk, vec![0u8; 16 * 1024 * 1024]).expect("failed to create disk");
    format_superblock(disk);
    gsfs_add_file(disk, name, &suite);

    crate::shell_test::run_script(&image_path, disk, name, 4);
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
