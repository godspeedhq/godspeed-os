// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `osdev` - host-side developer CLI (§17).
//!
//! Commands:
//!   osdev new <name>        - scaffold a new service
//!   osdev build             - build kernel + all services
//!   osdev run               - boot in QEMU (--smp N)
//!   osdev publish           - package + serve a service
//!   osdev restart <service> - restart a service in the running OS
//!   osdev logs <service>    - tail service logs
//!   osdev status <service>  - show state + assigned core
//!   osdev caps <service>    - show held capabilities
//!   osdev test identity         - run §22 identity test suite (20 tests)
//!   osdev test identity-brutal  - run brutal identity tests + SMP escalation (Milestone 15)
//!   osdev test property         - run §22 property test suite
//!   osdev test property-brutal  - run brutal property tests BP1–BP10 (Milestone 16)
//!   osdev test fuzz         - run §22 fuzz test suite (Milestone 10)
//!   osdev test fuzz-brutal  - run brutal fuzz tests BF1–BF8 (Milestone 17)
//!   osdev test stress       - run §22 stress test suite (Milestone 11)
//!   osdev test stress-brutal - run brutal stress tests BS1–BS10 (Milestone 18)
//!   osdev test perf         - run §22 performance benchmark suite (Milestone 12)
//!   osdev test perf-brutal  - run brutal performance benchmarks BP1–BP10 (Milestone 19)
//!   osdev test adv          - run §22 adversarial / red-team test suite (Milestone 13)
//!   osdev test adv-brutal   - run brutal adversarial tests BA1–BA10 (Milestone 20)
//!   osdev test chaos        - run §22 chaos / graceful-degradation test suite (Milestone 14)
//!   osdev test chaos-brutal - run brutal chaos tests BC1–BC7 (Milestone 21)
//!   osdev test shell        - scripted shell smoke-test (help, cores, status, unknown)
//!   osdev image [--mode M]  - build + create bootable disk image (build/os.img); M=bare-metal|perf|perf-brutal|identity|stress|adv|chaos|fuzz|s8

mod crc32;
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
        /// bare-metal  - pong + ping + observe; no probe services (default; S6 24-hour stability)
        /// perf        - regular perf probes B1–B10
        /// perf-brutal - brutal perf probes BP1–BP10
        /// identity    - identity-only probes (WatchSerial tests; WithRestart needs COM2)
        /// stress      - S1–S10 stress probes; self-contained, no harness required
        /// adv         - A1–A10 adversarial probes; self-contained, no harness required
        /// chaos       - C2–C7 chaos probes; self-contained, no harness required (C1/C4 use bare-metal + HW reconfiguration)
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
    /// Build a flashable GSFS data disk with a `.gsh` script baked in (run it on hardware).
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

// On-disk format - MUST match `services/fs` (docs/persistence.md §6.6/§6.10/§6.12/§6.15, GSFS0008).
// 512-byte blocks (= one AHCI sector = one block-IPC request), so block number = LBA.
// Three structures: superblock + free bitmap + self-describing directory tree (no inode
// table, no global file cap). All capacity-bearing fields are u64. GSFS0008: every block
// self-verifies with a CRC32 - superblock, each directory block, and each file-data block -
// and a BACKUP superblock copy sits at the last block (mount falls back to it). Files are a
// contiguous extent (`type`=1) or, when no contiguous run is free, fragmented (`type`=3) with
// `first_block` → a CRC'd extent block listing the runs. The host writer only ever bakes into
// a fresh disk, so it always writes contiguous files (`type`=1); the fragmented path is fs-only.
//   Superblock @ LBA 0 (and a copy @ total_blocks-1): magic[8] "GSFS0008" (FROZEN - versioning is
//     now the feature masks, never a new magic), version u32=8, block_size u32=512, total_blocks
//     u64, bitmap_start u64=1, bitmap_blocks u64, data_start u64, root_first_block u64,
//     root_block_count u64, free_blocks u64, flags u32, label_len u8, label[31],
//     journal_start u64 @108, journal_blocks u64 @116, feature_compat u32 @124,
//     feature_ro_compat u32 @128, feature_incompat u32 @132, sb_crc32 u32 @136 (CRC over @0..136).
//   Free bitmap @ LBA bitmap_start..journal_start: 1 bit/block (set=used), 4096 bits/block.
//     The last block (backup superblock) is reserved used.
//   Journal @ LBA journal_start..data_start: reserved crash-consistency region (Phase C).
//   Directory entry (file_record, 64 B): type u8 (0 free|1 file|2 dir|3 frag) @0, name_len u8 @1,
//     name[38] @2, size u64 @40, first_block u64 @48, block_count u64 @56. 7 per block;
//     bytes 448..452 hold the block's CRC32 over its 448-byte record region.
//   File-data block: 508 bytes payload + CRC32 @508. A file of N bytes spans ceil(N/508) blocks.
//   Root is a dir at root_first_block (its extent lives in the superblock; it has no parent).
const FS_SB_MAGIC: &[u8; 8] = b"GSFS0008";
const FS_BLOCK_SIZE: u32 = 512;
const FS_BITS_PER_BMBLOCK: u64 = (FS_BLOCK_SIZE as u64) * 8; // 4096
// GSFS0008 feature masks + the superblock CRC offset they pushed from @124 to @136.
const FS_FEAT_COMPAT_OFF: usize = 124;
const FS_FEAT_RO_COMPAT_OFF: usize = 128;
const FS_FEAT_INCOMPAT_OFF: usize = 132;
const FS_SB_CRC_OFF: usize = 136; // CRC32 over [0..136) - covers the masks
const FS_FEAT_COMPAT_BACKUP_SB: u32 = 0x1; // a fresh host-baked disk always has the backup superblock
const FS_ITYPE_DIR: u8 = 2;
const FS_JOURNAL_BLOCKS: u64 = 64; // reserved journal region (must match services/fs JOURNAL_BLOCKS)
const FS_RECS_PER_BLOCK: usize = 7; // directory records per block (8th slot region is the CRC trailer)
const FS_DIR_REC_REGION: usize = FS_RECS_PER_BLOCK * 64; // 448 - CRC covers [0..448)
const FS_DATA_PAYLOAD: usize = 508; // file-data block payload; CRC32 trailer @508

/// Stamp a directory block's CRC32 trailer (over its 448-byte record region) at offset 448.
fn fs_dir_stamp_crc(block: &mut [u8]) {
    let c = crc32::crc32(&block[..FS_DIR_REC_REGION]);
    block[FS_DIR_REC_REGION..FS_DIR_REC_REGION + 4].copy_from_slice(&c.to_le_bytes());
}

/// Write a GodspeedOS (GSFS0008) superblock + free bitmap + an empty root directory into
/// `path`, preserving the rest of the image. Geometry is derived from the image size.
fn format_superblock(path: &str) {
    let mut data = std::fs::read(path)
        .unwrap_or_else(|e| { eprintln!("mkfs: cannot read {}: {}", path, e); std::process::exit(1); });
    let total_blocks = data.len() as u64 / FS_BLOCK_SIZE as u64;
    let bitmap_start: u64 = 1;
    let bitmap_blocks = (total_blocks + FS_BITS_PER_BMBLOCK - 1) / FS_BITS_PER_BMBLOCK; // ceil
    let journal_start = bitmap_start + bitmap_blocks;
    let journal_blocks = FS_JOURNAL_BLOCKS;
    let data_start = journal_start + journal_blocks;
    let root_first_block = data_start;
    let root_block_count: u64 = 1;
    let used_through = data_start + root_block_count; // blocks [0..used_through) are used
    if total_blocks < used_through + 2 {
        eprintln!("mkfs: image too small ({} bytes)", data.len());
        std::process::exit(1);
    }
    let backup_lba = total_blocks - 1; // GSFS0008: backup superblock at the last block
    let free_blocks = total_blocks - used_through - 1; // -1 for the reserved backup block

    // Superblock (LBA 0).
    let mut sb = [0u8; 512];
    sb[0..8].copy_from_slice(FS_SB_MAGIC);
    sb[8..12].copy_from_slice(&8u32.to_le_bytes());            // version (frozen magic; masks evolve)
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
    sb[108..116].copy_from_slice(&journal_start.to_le_bytes());
    sb[116..124].copy_from_slice(&journal_blocks.to_le_bytes());
    // GSFS0008 feature masks: a fresh disk has the backup superblock (compat), no ro_compat, no
    // fragmented file yet (incompat). The host only bakes contiguous files, so incompat stays 0.
    sb[FS_FEAT_COMPAT_OFF..FS_FEAT_COMPAT_OFF + 4].copy_from_slice(&FS_FEAT_COMPAT_BACKUP_SB.to_le_bytes());
    sb[FS_FEAT_RO_COMPAT_OFF..FS_FEAT_RO_COMPAT_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    sb[FS_FEAT_INCOMPAT_OFF..FS_FEAT_INCOMPAT_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
    let sb_crc = crc32::crc32(&sb[..FS_SB_CRC_OFF]);
    sb[FS_SB_CRC_OFF..FS_SB_CRC_OFF + 4].copy_from_slice(&sb_crc.to_le_bytes());
    data[0..512].copy_from_slice(&sb);                         // primary
    let bk = (backup_lba as usize) * 512;                      // backup at the last block
    data[bk..bk + 512].copy_from_slice(&sb);

    // Zero the bitmap region, then mark blocks [0..used_through) used (superblock + bitmap +
    // journal + the root directory block), plus the reserved backup-superblock block.
    let bm = (bitmap_start as usize) * 512;
    let bm_end = (journal_start as usize) * 512;
    for b in &mut data[bm..bm_end] { *b = 0; }
    for blk in 0..used_through as usize {
        data[bm + blk / 8] |= 1 << (blk % 8);
    }
    data[bm + (backup_lba / 8) as usize] |= 1 << (backup_lba % 8);

    // Zero the root directory block, then stamp its CRC trailer (no entries yet).
    let rd = (root_first_block as usize) * 512;
    for b in &mut data[rd..rd + 512] { *b = 0; }
    fs_dir_stamp_crc(&mut data[rd..rd + 512]);

    std::fs::write(path, &data)
        .unwrap_or_else(|e| { eprintln!("mkfs: cannot write {}: {}", path, e); std::process::exit(1); });
    println!("mkfs: formatted {} GSFS0008 ({} blocks, bitmap {}..{}, journal {}..{}, data from {}, backup@{}, {} free)",
             path, total_blocks, bitmap_start, journal_start, journal_start, data_start, root_first_block, backup_lba, free_blocks);
}

fn cmd_mkfs(image: &str) {
    format_superblock(image);
}

/// Bake a file into a GSFS0008 image (host-side mirror of the `fs` write path) - used to ship a
/// `.gsh` script on a flashable data disk, so the OS can `run /suite.gsh` on hardware with no
/// on-device authoring. Allocates a contiguous extent, writes the content, adds a root
/// `file_record`, and updates the free count. Intended right after `format_superblock` (minimal,
/// unfragmented layout). `name` ≤ 38 bytes; fits in the single root directory block (7 entries).
fn gsfs_add_file(path: &str, name: &str, content: &[u8]) {
    let mut data = std::fs::read(path)
        .unwrap_or_else(|e| { eprintln!("bake: cannot read {}: {}", path, e); std::process::exit(1); });
    if data.len() < 512 || &data[0..8] != FS_SB_MAGIC {
        eprintln!("bake: {} is not a GSFS0008 image", path); std::process::exit(1);
    }
    if name.len() > 38 { eprintln!("bake: name '{}' too long (max 38)", name); std::process::exit(1); }
    let rdu = |d: &[u8], o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
    let total_blocks = rdu(&data, 16);
    let bitmap_start = rdu(&data, 24) as usize;
    let data_start   = rdu(&data, 40);
    let root_first   = rdu(&data, 48) as usize;
    let mut free_blocks = rdu(&data, 64);

    // GSFS0008: a data block holds 508 payload bytes + a CRC32 trailer, so a file spans
    // ceil(content/508) blocks.
    let nblocks = (((content.len() as u64) + FS_DATA_PAYLOAD as u64 - 1) / FS_DATA_PAYLOAD as u64).max(1);
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
    // Re-stamp the superblock CRC32 (@136, GSFS0008) - we just mutated a superblock field (free count).
    let sb_crc = crc32::crc32(&data[..FS_SB_CRC_OFF]);
    data[FS_SB_CRC_OFF..FS_SB_CRC_OFF + 4].copy_from_slice(&sb_crc.to_le_bytes());
    // Keep the backup superblock (last block, GSFS0008) in sync with the primary.
    let bk = ((total_blocks - 1) as usize) * 512;
    let sb_copy: Vec<u8> = data[0..512].to_vec();
    data[bk..bk + 512].copy_from_slice(&sb_copy);
    // Write the content into the extent as 508-byte payloads, each with its CRC32 trailer @508
    // (mirror of the on-disk `fs` data_write). The tail of the last block stays zero-padded.
    for k in 0..nblocks as usize {
        let blk_off = ((first as usize) + k) * 512;
        let s = k * FS_DATA_PAYLOAD;
        let e = (s + FS_DATA_PAYLOAD).min(content.len());
        if s < content.len() { data[blk_off..blk_off + (e - s)].copy_from_slice(&content[s..e]); }
        let crc = crc32::crc32(&data[blk_off..blk_off + FS_DATA_PAYLOAD]);
        data[blk_off + FS_DATA_PAYLOAD..blk_off + FS_DATA_PAYLOAD + 4].copy_from_slice(&crc.to_le_bytes());
    }
    // Add a root file_record into the first free 64-byte slot (type 0 = free).
    let rd = root_first * 512;
    let mut placed = false;
    for slot in 0..FS_RECS_PER_BLOCK {
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
    // Re-stamp the root directory block's CRC trailer after mutating its records.
    fs_dir_stamp_crc(&mut data[rd..rd + 512]);
    std::fs::write(path, &data)
        .unwrap_or_else(|e| { eprintln!("bake: cannot write {}: {}", path, e); std::process::exit(1); });
    println!("bake: wrote /{} ({} bytes, {} block(s) at {}) into {}", name, content.len(), nblocks, first, path);
}

/// `osdev script-disk <out> <script>` - produce a flashable GSFS data disk with `<script>` baked
/// in as `/<basename>`. Boot the OS with this disk attached and `run /<basename>` - the way to
/// ship a self-checking suite to hardware (`dd` it to the data drive). Default 16 MiB.
fn cmd_script_disk(out: &str, script: &str) {
    let content = std::fs::read(script)
        .unwrap_or_else(|e| { eprintln!("script-disk: cannot read {}: {}", script, e); std::process::exit(1); });
    let name = std::path::Path::new(script).file_name()
        .and_then(|s| s.to_str()).unwrap_or("suite.gsh");
    if let Some(parent) = std::path::Path::new(out).parent() { let _ = std::fs::create_dir_all(parent); }
    std::fs::write(out, vec![0u8; 16 * 1024 * 1024])
        .unwrap_or_else(|e| { eprintln!("script-disk: cannot create {}: {}", out, e); std::process::exit(1); });
    format_superblock(out);
    gsfs_add_file(out, name, &content);
    println!("script-disk: {} ready - flash it to the data drive, then `run /{}`", out, name);
}

fn cmd_new(name: &str) {
    todo!("scaffold service directory, Cargo.toml, src/main.rs, contracts/{name}.toml from template")
}

/// Force a clean rebuild of the supervisor (kernel target) before a build mode runs.
///
/// Every build mode compiles the supervisor with a different spawn-set feature.
/// When switching modes, cargo can return a `supervisor.elf` whose mtime is OLDER
/// than a previously-built kernel, so the kernel's `rerun-if-changed` on
/// `supervisor.elf` never fires and the kernel keeps a STALE embedded supervisor -
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
    // Services must be compiled before the kernel - kernel/build.rs embeds
    // the service ELF bytes via include_bytes!(env!("SVC_*_ELF")).
    let service_crates = [
        "supervisor", "logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs",
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
/// Spawns only observe - no pong, no ping, no probes.  The kernel idles on all
/// cores; observe snapshots system state every ~500 yields.
/// Bar: no panic, no resource leak after 24 hours.
pub fn cmd_build_idle() {
    clean_supervisor();
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
/// internally - no QEMU control port required.
pub fn cmd_build_stress() {
    clean_supervisor();
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
/// Brutal equivalent of b2-only - 1000-sample iteration count, same isolation rationale.
/// Per-probe isolation build (`perf-iso` umbrella + one `iso-bpN` sub-feature).
/// Spawns exactly one brutal perf probe (+ its partners), no ping/pong, no other
/// probes - for clean, uncontended per-op latency on hardware. `feature` is the
/// supervisor sub-feature, e.g. "iso-bp5".
pub fn cmd_build_perf_iso(feature: &str) {
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
/// hardware adversarial run (A1–A10). All adversarial probes are self-contained -
/// no QEMU control port required.
pub fn cmd_build_adv() {
    clean_supervisor();
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
        eprintln!("BOOTX64.EFI not found at {} - UEFI image requires it", bootx64.display());
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
            eprintln!("logs: serial log not found at {} - is `osdev run` active?", path.display());
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
        "fs-corrupt"   => run_fs_corruption_test(),
        "fs-large"     => run_fs_large_test(),
        "fs-frag"      => run_fs_frag_test(),
        "fs-journal"   => run_fs_journal_test(),
        "fs-djournal"  => run_fs_djournal_test(),
        "fs-restart"   => run_fs_restart_test(),
        "fs-check"     => run_fs_check_test(),
        "fs-scrub"     => run_fs_scrub_test(),
        "fs-compat"    => run_fs_compat_test(),
        "file-cap"     => run_fs_filecap_test(),
        "fs-ioretry"   => run_fs_ioretry_test(),
        "drives-raw"   => run_drives_raw_test(),
        "drives"       => run_drives_scripted_test(),
        "files"        => run_files_test(),
        "edit"         => run_edit_test(),
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
/// IOMMU domain - its init DMA then faults, the deterministic live proof for
/// §22 Test 12 / H1 §6.4.
fn cmd_build_iommu_fault() {
    clean_supervisor();
    let non_supervisor = ["logger", "mem-pressure", "chaos", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver", "fs"];
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
/// IOMMU - the confined driver's I/O page table maps **exactly** its arena and the
/// page just outside it is **unmapped** (so an out-of-arena DMA has no translation
/// and would fault), AND the driver still operates *through* the confined domain
/// (its keyboard enumerates), AND the kernel does not panic.
///
/// QEMU's `amd-iommu` does not actually *enforce* translation faults (it is lenient
/// where real AMD-Vi is strict), so the live `IO_PAGE_FAULT` cannot be reproduced in
/// emulation - it is hardware-verified on the T630 (and reproducible there with the
/// kernel `iommu-fault-test` feature, which confines xhci to an empty domain). The
/// `selftest` line is a CPU-side page-table walk QEMU cannot fake, so it is the
/// portable executable form of the guarantee. Requires q35 + `-device amd-iommu`,
/// which the BIOS/i440fx test path can't provide, so this launches QEMU itself.
fn run_iommu_test() {
    println!("\n=== §22 Test 12: confined driver - out-of-arena is unmapped (H1 §6.4) ===");
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
    // page just outside it is unmapped - the structural form of "out-of-arena DMA
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
/// capture the serial log, and return it. The persist disk is NOT recreated -
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
fn cmd_build_blockdev() { build_blockdev_fs("selftest", ""); }

/// Build the blockdev image with `fs` compiled with `fs_features` (`selftest` for the
/// round-trip/reboot tests, `journal-crash-test` for crash-consistency) and `block-driver`
/// compiled with `bd_features` (e.g. `io-error-test` to exercise the I/O retry path; `""` for
/// none).
fn build_blockdev_fs(fs_features: &str, bd_features: &str) {
    clean_supervisor();
    // Force a fresh `fs` (and `block-driver` if it gets a feature) so the test features are
    // compiled in even if a prior plain build cached them.
    let _ = std::process::Command::new("cargo")
        .args(["clean", "--release", "-p", "fs", "--target", "x86_64-unknown-none"]).status();
    if !bd_features.is_empty() {
        let _ = std::process::Command::new("cargo")
            .args(["clean", "--release", "-p", "block-driver", "--target", "x86_64-unknown-none"]).status();
    }
    let non_supervisor = ["logger", "ping", "pong", "greet", "upper", "roster", "probe", "observe", "shell", "xhci", "ehci", "block-driver"];
    for crate_name in &non_supervisor {
        let mut args = vec!["build", "--release", "-p", crate_name, "--target", "x86_64-unknown-none"];
        if *crate_name == "block-driver" && !bd_features.is_empty() {
            args.push("--features");
            args.push(bd_features);
        }
        let status = std::process::Command::new("cargo")
            .args(&args)
            .status()
            .unwrap_or_else(|e| panic!("failed to run cargo build for {}: {}", crate_name, e));
        if !status.success() { eprintln!("build: {} FAILED", crate_name); std::process::exit(1); }
        println!("build: {} OK", crate_name);
    }
    // fs WITH the requested test feature - the blockdev tests assert its self-test log lines.
    // (Production `osdev image` builds fs without it, so it never writes to a real disk.)
    let status = std::process::Command::new("cargo")
        .args(["build", "--release", "-p", "fs", "--target", "x86_64-unknown-none", "--features", fs_features])
        .status().unwrap_or_else(|e| panic!("failed to run cargo build for fs: {}", e));
    if !status.success() { eprintln!("build: fs FAILED"); std::process::exit(1); }
    println!("build: fs ({}) OK", fs_features);
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
/// disk ALONE on an ich9-ahci controller (so block-driver targets it on port 0 -
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
        // Boot disk on legacy IDE (PIIX3) - SeaBIOS boots it; it is NOT SATA so
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
    // step E: hierarchical GSFS - mkdir + a file nested inside it, walked by path.
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
/// reboot on the SAME SATA disk image - fs must read it back over AHCI.
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

    println!("ahci: boot 1 - fs creates 'greeting' over AHCI, ~25s …");
    let log1 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/ahci_reboot_1.log", 25);
    let created = log1.contains("fs: file round-trip OK (greeting)");

    println!("ahci: boot 2 - SAME SATA disk, no reformat, fs reads it back, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/ahci_reboot_2.log", 25);
    // Match the END of the line - the "fs:" prefix can be clobbered by a concurrent
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

/// GSFS0008 integrity: a corrupt block is a **loud refusal** (§3.12), never silently read
/// back as garbage. Three cases, all observable in the boot log:
///   (1) corrupt a superblock byte → `fs` mount fails its CRC check → "no filesystem".
///   (2) corrupt the root directory block → mount OK but the first dir op logs a
///       "directory block CRC mismatch" - never returns records from a bad block.
///   (3) corrupt a file's data block → reading it logs a "data block CRC mismatch" - never
///       returns bytes from a bad block (the GSFS0008 per-data-block CRC).
fn run_fs_corruption_test() {
    println!("\n=== fs: GSFS0008 integrity - corrupt block → loud refusal (§3.12) ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let _ = std::fs::create_dir_all("build/tests");

    // Helper: make a formatted 16 MiB GSFS0008 disk, corrupt one byte, return its abs path.
    let make = |name: &str, corrupt: &dyn Fn(&mut [u8])| -> String {
        let p = format!("build/tests/{}", name);
        std::fs::write(&p, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
        format_superblock(&p);
        let mut data = std::fs::read(&p).unwrap();
        corrupt(&mut data);
        std::fs::write(&p, &data).unwrap();
        let abs = std::fs::canonicalize(&p).unwrap_or_else(|_| std::path::PathBuf::from(&p));
        abs.to_string_lossy().replace('\\', "/")
    };

    // Case 1a (GSFS0008): corrupt ONLY the primary superblock (flip free_blocks @64 - a
    // CRC-covered byte, not the magic). Mount must RECOVER from the backup at the last block.
    let sb1_disk = make("fs_corrupt_sb1.img", &|d: &mut [u8]| d[64] ^= 0xFF);
    println!("fs: case 1a - corrupt PRIMARY superblock only, boot (~25s) …");
    let log1a = boot_ahci_qemu(&img_str, &sb1_disk, "build/tests/fs_corrupt_sb1.log", 25);
    let sb1_recovered = log1a.contains("recovered from backup superblock");
    // Match the geometry tail ("GSFS0008 (…") not "fs: mounted GSFS" - a concurrent shell write
    // can split the prefix on the shared serial. No "no filesystem" confirms it didn't refuse.
    let sb1_mounted = log1a.contains("GSFS0008 (") && !log1a.contains("fs: no filesystem");
    let sb1_no_panic = !log1a.contains("KERNEL PANIC");

    // Case 1b: corrupt BOTH copies (primary @64 and the backup at the last block). With no good
    // copy left, mount must refuse loudly and NOT mount.
    let sb2_disk = make("fs_corrupt_sb2.img", &|d: &mut [u8]| {
        let total = u64::from_le_bytes(d[16..24].try_into().unwrap()) as usize;
        d[64] ^= 0xFF;                       // primary
        d[(total - 1) * 512 + 64] ^= 0xFF;   // backup (last block)
    });
    println!("fs: case 1b - corrupt BOTH superblock copies, boot (~25s) …");
    let log1b = boot_ahci_qemu(&img_str, &sb2_disk, "build/tests/fs_corrupt_sb2.log", 25);
    let sb_refused = log1b.contains("checksum mismatch");
    let sb_nofs = log1b.contains("fs: no filesystem");
    let sb_not_mounted = !log1b.contains("fs: mounted GSFS");
    let sb_no_panic = !log1b.contains("KERNEL PANIC");

    // Case 2: leave the superblock valid; corrupt the ROOT directory block's record region.
    // root_first_block is the u64 at superblock offset 48.
    let dir_disk = make("fs_corrupt_dir.img", &|d: &mut [u8]| {
        let root = u64::from_le_bytes(d[48..56].try_into().unwrap()) as usize;
        d[root * 512] ^= 0xFF; // flip a byte in the 448-byte record region → trailer CRC fails
    });
    println!("fs: case 2 - corrupt root directory block, boot (~25s) …");
    let log2 = boot_ahci_qemu(&img_str, &dir_disk, "build/tests/fs_corrupt_dir.log", 25);
    let dir_mounted = log2.contains("fs: mounted GSFS");                 // superblock OK
    let dir_caught = log2.contains("directory block CRC mismatch");      // loud
    let dir_no_garbage = !log2.contains("round-trip OK (greeting)");     // never silently succeeded
    let dir_no_panic = !log2.contains("KERNEL PANIC");

    // Case 3 (GSFS0008): bake /probe.bin, then flip a PAYLOAD byte in its first data block.
    // The selftest reads /probe.bin → the per-data-block CRC catches it (loud), read fails.
    let data_disk = {
        let p = "build/tests/fs_corrupt_data.img";
        std::fs::write(p, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
        format_superblock(p);
        gsfs_add_file(p, "probe.bin", b"data-block integrity check payload - must read back exactly");
        let mut d = std::fs::read(p).unwrap();
        let root = u64::from_le_bytes(d[48..56].try_into().unwrap()) as usize;
        let rd = root * 512;
        let mut first_block = 0usize;
        for slot in 0..7 {
            let r = rd + slot * 64;
            if d[r] == 1 { first_block = u64::from_le_bytes(d[r + 48..r + 56].try_into().unwrap()) as usize; break; }
        }
        d[first_block * 512] ^= 0xFF; // flip a payload byte (not the CRC @508) → data CRC fails
        std::fs::write(p, &d).unwrap();
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
        abs.to_string_lossy().replace('\\', "/")
    };
    println!("fs: case 3 - corrupt a file data block, boot (~25s) …");
    let log3 = boot_ahci_qemu(&img_str, &data_disk, "build/tests/fs_corrupt_data.log", 25);
    let data_caught = log3.contains("data block CRC mismatch");           // loud
    let data_failed = log3.contains("probe.bin read FAILED");             // read refused
    let data_no_garbage = !log3.contains("probe.bin read OK");            // never silently returned bytes
    let data_no_panic = !log3.contains("KERNEL PANIC");

    let mut all = true;
    for (tag, ok) in [
        ("case1a: primary corrupt → recovered from backup", sb1_recovered),
        ("case1a: filesystem mounted after recovery", sb1_mounted),
        ("case1a: no kernel panic", sb1_no_panic),
        ("case1b: both copies corrupt → refused", sb_refused),
        ("case1b: reported no filesystem", sb_nofs),
        ("case1b: did NOT mount corrupt fs", sb_not_mounted),
        ("case1b: no kernel panic", sb_no_panic),
        ("case2: superblock intact (mounted)", dir_mounted),
        ("case2: directory CRC mismatch caught (loud)", dir_caught),
        ("case2: never silently returned records", dir_no_garbage),
        ("case2: no kernel panic", dir_no_panic),
        ("case3: data block CRC mismatch caught (loud)", data_caught),
        ("case3: read refused (no garbage)", data_failed && data_no_garbage),
        ("case3: no kernel panic", data_no_panic),
    ] {
        println!("  {} … {}", if ok { "PASS" } else { "FAIL" }, tag);
        all &= ok;
    }
    if all {
        println!("\n  [GSFS.crc]  integrity + backup superblock: corruption is loud or recovered  … PASS\n\n  14 passed  0 failed");
    } else {
        println!("\n  [GSFS.crc]  integrity  … FAIL\n");
        std::process::exit(1);
    }
}

/// Large files: a 200 KiB file (far past the 3584-byte single-message chunk) written and
/// read in streaming chunks via WRITE_NEW/WRITE_AT/READ_AT, then verified to **persist
/// across a reboot**. The fs self-test creates the file on boot 1 (and verifies the
/// round-trip) and re-verifies it on boot 2 on the SAME disk - proving both the streaming
/// path and durability of a large file.
fn run_fs_large_test() {
    println!("\n=== fs: large files (200 KiB streaming round-trip + reboot survival) ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_fs_large.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create persist disk");
    format_superblock(persist);

    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    println!("fs: boot 1 - write + read back a 200 KiB file (streaming), ~25s …");
    let log1 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/fs_large_1.log", 25);
    let wrote = log1.contains("large-file 204800 B round-trip OK");

    println!("fs: boot 2 - SAME disk, re-read the 200 KiB file, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/fs_large_2.log", 25);
    let survived = log2.contains("large-file 204800 B round-trip OK");
    let no_bad = !log1.contains("large-file MISMATCH") && !log2.contains("large-file MISMATCH")
        && !log1.contains("large write") && !log2.contains("large-file READ FAILED");
    let no_panic = !log1.contains("KERNEL PANIC") && !log2.contains("KERNEL PANIC");

    println!("fs:   boot 1 streaming write+read 200 KiB ... {}", if wrote { "yes" } else { "NO" });
    println!("fs:   boot 2 re-read across reboot ... {}", if survived { "yes" } else { "NO" });
    println!("fs:   no mismatch / no panic ... {}", if no_bad && no_panic { "yes" } else { "NO" });

    if wrote && survived && no_bad && no_panic {
        println!("\n  [FS.large]  200 KiB streaming round-trip + reboot survival  … PASS\n\n  1 passed  0 failed");
    } else {
        println!("\n  [FS.large]  large files  … FAIL\n\n  0 passed  1 failed");
        std::process::exit(1);
    }
}

/// Extent lists / fragmentation (Phase I, GSFS0008). A `frag-test` build fills a small SATA
/// disk with 2-block files, deletes every other one to scatter free space into ~2-block gaps,
/// then writes a 20-block `/frag.bin` - far bigger than any gap, so contiguous allocation must
/// fail and the file is stored FRAGMENTED across an extent list. We assert it became
/// `ITYPE_FILE_FRAG`, that the streaming read-back matches, and (boot 2) that the extent list
/// survives a reboot. Proves the fragmented data path + extent-block CRC + reboot durability.
fn run_fs_frag_test() {
    println!("\n=== fs: extent lists / fragmentation (GSFS0008 - forced fragmented file) ===");
    build_blockdev_fs("frag-test", "");

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    // Small disk (160 KiB) so the fill loop completes quickly but leaves plenty of free space
    // (~100 blocks) after the every-other delete for the fragmented 20-block file.
    let persist = "build/tests/persist_fs_frag.img";
    std::fs::write(persist, vec![0u8; 160 * 1024]).expect("failed to create persist disk");
    format_superblock(persist);

    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    let mut pass = 0; let mut fail = 0;
    let mut check = |ok: bool, label: &str| {
        if ok { println!("  PASS … {}", label); pass += 1; } else { println!("  FAIL … {}", label); fail += 1; }
    };

    println!("fs: boot 1 - fill, fragment free space, write a forced-fragmented file, ~40s …");
    let log1 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/fs_frag_1.log", 40);
    check(log1.contains("[frag] filled"), "boot1: filled the disk with small files");
    check(log1.contains("[frag] deleted"), "boot1: scattered free space (deleted every other)");
    check(log1.contains("/frag.bin is FRAGMENTED"), "boot1: file forced onto the fragmented (extent-list) path");
    check(!log1.contains("NOT fragmented"), "boot1: contiguous allocation genuinely failed");
    check(log1.contains("[frag] write+read round-trip OK"), "boot1: fragmented file reads back exactly");
    check(!log1.contains("CRC mismatch"), "boot1: no extent/data CRC failure");
    check(!log1.contains("KERNEL PANIC"), "boot1: no kernel panic");

    println!("fs: boot 2 - SAME disk, re-read the fragmented file across a reboot, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &persist_str, "build/tests/fs_frag_2.log", 25);
    check(log2.contains("/frag.bin present after reboot (FRAGMENTED"), "boot2: extent list persisted (still fragmented)");
    check(log2.contains("[frag] reboot re-read OK"), "boot2: re-read matches after reboot");
    check(!log2.contains("reboot re-read FAILED"), "boot2: no read failure");
    check(!log2.contains("KERNEL PANIC") && !log2.contains("CRC mismatch"), "boot2: no panic, no corruption");

    println!();
    if fail == 0 {
        println!("  [FS.frag]  extent lists / fragmentation  … PASS\n\n  {} passed  0 failed", pass);
    } else {
        println!("  [FS.frag]  extent lists / fragmentation  … FAIL\n\n  {} passed  {} failed", pass, fail);
        std::process::exit(1);
    }
}

/// Data journaling (Phase J, opt-in per write). A `data-journal-test` build creates `/jdata.bin`
/// (its data blocks left ZERO on disk), then issues ONE **journaled** `write_at` (`OP_WRITE_AT_J`)
/// through a transaction that halts right after the commit record - so the chunk's data lives only
/// in the journal, never written to its home LBAs. The next boot's mount must REPLAY it from the
/// journal. Airtight: the home blocks were zero (a zero block fails the data CRC), so reading the
/// file back correctly can only mean the journal supplied the data - proving the chunk was
/// crash-atomic, not torn. Contrast with `fs-journal` (metadata replay; here it's the DATA).
fn run_fs_djournal_test() {
    println!("\n=== fs: data journaling (Phase J - journaled write_at survives a crash) ===");
    build_blockdev_fs("data-journal-test", "");

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    let disk = "build/tests/persist_fs_djournal.img";
    std::fs::write(disk, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(disk);

    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let disk_abs = std::fs::canonicalize(disk).unwrap_or_else(|_| std::path::PathBuf::from(disk));
    let disk_str = disk_abs.to_string_lossy().replace('\\', "/");

    let mut pass = 0; let mut fail = 0;
    let mut check = |ok: bool, label: &str| {
        if ok { println!("  PASS … {}", label); pass += 1; } else { println!("  FAIL … {}", label); fail += 1; }
    };

    println!("fs: boot 1 - journaled write_at, halt after commit record (data only in journal), ~25s …");
    let log1 = boot_ahci_qemu(&img_str, &disk_str, "build/tests/fs_djournal_1.log", 25);
    check(log1.contains("halting before checkpoint"), "boot1: crashed right after the commit record");
    check(!log1.contains("jdata write_new FAILED") && !log1.contains("did NOT crash"), "boot1: journaled write actually started + crashed");
    check(!log1.contains("KERNEL PANIC"), "boot1: no kernel panic");

    println!("fs: boot 2 - SAME disk, mount must replay the DATA from the journal, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &disk_str, "build/tests/fs_djournal_2.log", 25);
    check(log2.contains("journal recovered"), "boot2: mount replayed the committed transaction");
    check(log2.contains("jdata RECOVERED+VERIFIED"), "boot2: recovered data is exactly correct (journal supplied it, not the zero home blocks)");
    check(!log2.contains("DATA MISMATCH") && !log2.contains("data not recovered"), "boot2: no mismatch, data was recovered");
    check(!log2.contains("KERNEL PANIC") && !log2.contains("CRC mismatch"), "boot2: no panic, no corruption");

    println!();
    if fail == 0 {
        println!("  [FS.djournal]  opt-in data journaling: journaled write survives a crash  … PASS\n\n  {} passed  0 failed", pass);
    } else {
        println!("  [FS.djournal]  data journaling  … FAIL\n\n  {} passed  {} failed", pass, fail);
        std::process::exit(1);
    }
}

/// Crash-consistency (Phase C). Two parts, both over a real SATA disk image:
///   Part 1 (REPLAY): a `journal-crash-test` build writes a file through a transaction that
///     halts right after the commit record is durable but before the checkpoint (simulated
///     power loss). On the next boot, `mount`'s recovery replays the committed transaction
///     from the journal - the file is present with the right bytes.
///   Part 2 (REJECT): a normal build boots a disk whose journal holds a commit record with a
///     BAD checksum (a torn/garbage commit). Recovery must IGNORE it - no replay, mount clean.
fn run_fs_journal_test() {
    println!("\n=== fs: crash-consistency (journal replay + reject invalid commit) ===");
    const FS_JOURNAL_MAGIC: u32 = 0x474A_3034; // "GJ04" - must match services/fs JOURNAL_MAGIC

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    let limine_dir = std::path::Path::new("tools/limine");
    let _ = std::fs::create_dir_all("build/tests");
    let mut pass = 0; let mut fail = 0;
    let mut check = |ok: bool, label: &str| {
        if ok { println!("  PASS … {}", label); pass += 1; } else { println!("  FAIL … {}", label); fail += 1; }
    };

    // ── Part 1: replay a committed-but-unfinished transaction ──
    build_blockdev_fs("journal-crash-test", "");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");

    let disk = "build/tests/fs_journal.img";
    std::fs::write(disk, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(disk);
    let disk_abs = std::fs::canonicalize(disk).unwrap_or_else(|_| std::path::PathBuf::from(disk));
    let disk_str = disk_abs.to_string_lossy().replace('\\', "/");

    println!("fs: boot 1 - write /jcrash.txt, halt after commit record (simulated crash), ~25s …");
    let log1 = boot_ahci_qemu(&img_str, &disk_str, "build/tests/fs_journal_1.log", 25);
    check(log1.contains("halting before checkpoint"), "part1 boot1: crashed right after the commit record");
    check(!log1.contains("did NOT crash"), "part1 boot1: the crash actually fired");
    check(!log1.contains("KERNEL PANIC"), "part1 boot1: no kernel panic");

    println!("fs: boot 2 - SAME disk, mount must replay the journal, ~25s …");
    let log2 = boot_ahci_qemu(&img_str, &disk_str, "build/tests/fs_journal_2.log", 25);
    check(log2.contains("journal recovered"), "part2 boot2: mount replayed the committed transaction");
    check(log2.contains("jcrash RECOVERED+VERIFIED"), "part2 boot2: recovered file has the right bytes");
    check(!log2.contains("DATA MISMATCH"), "part2 boot2: no data mismatch");
    check(!log2.contains("KERNEL PANIC") && !log2.contains("CRC mismatch"), "part2 boot2: no panic, no corruption");

    // ── Part 2: a normal build must REJECT a commit record with a bad CRC ──
    build_blockdev_fs("selftest", "");
    let image2 = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image2);
    let img2 = std::fs::canonicalize(&image2).unwrap_or_else(|_| image2.to_path_buf());
    let img2_str = img2.to_string_lossy().replace('\\', "/");

    let disk2 = "build/tests/fs_journal_bad.img";
    std::fs::write(disk2, vec![0u8; 16 * 1024 * 1024]).expect("create disk2");
    format_superblock(disk2);
    // Fabricate a commit record with a VALID magic but a DELIBERATELY WRONG crc at journal_start.
    {
        let mut data = std::fs::read(disk2).unwrap();
        let journal_start = u64::from_le_bytes(data[108..116].try_into().unwrap()) as usize;
        let off = journal_start * 512;
        data[off..off + 4].copy_from_slice(&FS_JOURNAL_MAGIC.to_le_bytes());
        data[off + 4..off + 8].copy_from_slice(&2u32.to_le_bytes()); // n = 2
        data[off + 8..off + 24].copy_from_slice(&[7u8; 16]);          // bogus home LBAs
        data[off + 508..off + 512].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // wrong CRC
        std::fs::write(disk2, &data).unwrap();
    }
    let disk2_abs = std::fs::canonicalize(disk2).unwrap_or_else(|_| std::path::PathBuf::from(disk2));
    let disk2_str = disk2_abs.to_string_lossy().replace('\\', "/");

    println!("fs: boot 3 - journal has a commit record with a BAD crc; recovery must ignore it, ~22s …");
    let log3 = boot_ahci_qemu(&img2_str, &disk2_str, "build/tests/fs_journal_bad.log", 22);
    check(log3.contains("mounted GSFS0008"), "reject: filesystem mounted cleanly");
    check(!log3.contains("journal recovered"), "reject: did NOT replay the bad-CRC commit");
    check(log3.contains("round-trip OK (greeting)") || log3.contains("verified across boot"),
          "reject: fs serves normally (greeting round-trip)");
    check(!log3.contains("KERNEL PANIC"), "reject: no kernel panic");

    if fail == 0 {
        println!("\n  [FS.journal]  crash-consistency: replay committed, reject torn  … PASS\n\n  {} passed  0 failed", pass);
    } else {
        println!("\n  [FS.journal]  crash-consistency  … FAIL\n\n  {} passed  {} failed", pass, fail);
        std::process::exit(1);
    }
}

/// Step 3a: raw-disk tolerance. Boot with an UNFORMATTED disk on the AHCI controller
/// (no `mkfs`): `fs` must learn the disk's capacity (OP_CAPACITY), recognise there is no
/// filesystem, NOT auto-format (§3.12), and stay up serving - never panic, never hang.
fn run_drives_raw_test() {
    println!("\n=== drives 3a: raw-disk tolerance (capacity + no auto-format) ===");
    cmd_build_blockdev();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    let _ = std::fs::create_dir_all("build/tests");
    // A RAW disk - zeros, deliberately NOT formatted with `format_superblock`.
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

/// `osdev test edit` - exercise the full-screen `edit` text editor over the serial console: open
/// a new file, type/backspace/newline, save (^S) + quit (^Q), read it back; edit the existing
/// file (insert at start); and quit with unsaved changes via the discard prompt. Bare-metal shell
/// + a RAW AHCI disk the test formats first (the editor needs a filesystem to save to).
fn run_edit_test() {
    println!("\n=== edit: full-screen text editor - type / save / quit / read-back (AHCI disk) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_edit.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create raw disk");
    // Pre-format GSFS and bake a MULTI-WINDOW text file (> several IO_CHUNK windows) so the editor
    // exercises the piece-table windowed-load + streaming-save path on a real large file. 400 lines
    // of a fixed shape (no "gsh>" substring - that's the harness sentinel); first "EDITLINE 0000",
    // last "EDITLINE 0399" → asserts the start-edit and the untouched tail both survive a save.
    format_superblock(persist);
    let mut big = String::new();
    for i in 0..400 { big.push_str(&format!("EDITLINE {i:04} the quick brown fox jumps\n")); }
    gsfs_add_file(persist, "big.txt", big.as_bytes());
    crate::shell_test::run_edit(&image_path, persist, 4);
}

/// Host-baked-script test: bake a self-checking `.gsh` suite into a GSFS data disk (the way a
/// suite ships to hardware), boot with it attached, and `run /suite.gsh`. Proves the whole
/// flash-and-run loop AND piped asserts inside a script (which can't be authored on-device).
fn run_script_test() {
    println!("\n=== script: a host-baked .gsh suite, run on boot (GSFS AHCI disk) ===");
    cmd_build_bare_metal();

    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);

    // Host-baked FILE path: bake the SMALL smoke suite (an on-disk .gsh is one ≤4 KiB IPC
    // message - MAX_FILE_BYTES - so the big extensive suite can't be a file). The extensive
    // coverage is the embedded suite, exercised below via `selfcheck` (run in memory). This
    // file proves the script-disk → run-from-file path incl. a piped assert.
    let script_path = "scripts/smoke.gsh";
    let suite = std::fs::read(script_path)
        .unwrap_or_else(|e| { eprintln!("script-test: cannot read {}: {}", script_path, e); std::process::exit(1); });
    let name = std::path::Path::new(script_path).file_name().and_then(|s| s.to_str()).unwrap_or("suite.gsh");

    let _ = std::fs::create_dir_all("build/tests");
    let disk = "build/tests/script_disk.img";
    std::fs::write(disk, vec![0u8; 16 * 1024 * 1024]).expect("failed to create disk");
    format_superblock(disk);
    gsfs_add_file(disk, name, &suite);

    crate::shell_test::run_script(&image_path, disk, name, 4);
}

/// §22 Test 13 (Phase D): fs survives its own restart. Bare-metal shell + AHCI disk; the
/// harness writes a file, KILLs fs over the control channel, and reads it back after the
/// supervisor respawns fs and the shell reacquires it via the registry.
fn run_fs_restart_test() {
    println!("\n=== fs: restartable - survives its own restart (Phase D, §22 Test 13) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_fs_restart.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("failed to create raw disk");
    crate::shell_test::run_fs_restart(&image_path, persist, 4);
}

/// Phase G: `drives check` (fsck) repairs a drifted free count. Bake a GSFS0008 disk with a
/// file, deliberately corrupt the superblock's free count (both copies, CRC re-stamped so it
/// still mounts), boot, and assert `drives check` rebuilds the correct free count from the tree
/// and the file survives.
fn run_fs_check_test() {
    println!("\n=== fs: drives check (fsck) - rebuild a drifted free count from the tree (Phase G) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");

    // Pre-format the disk with a couple of files (host-side), then read the CORRECT free count.
    let persist = "build/tests/persist_fs_check.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(persist);
    gsfs_add_file(persist, "alpha.txt", b"alpha-payload");
    gsfs_add_file(persist, "beta.txt", b"beta-payload");
    let mut data = std::fs::read(persist).unwrap();
    let correct_free = u64::from_le_bytes(data[64..72].try_into().unwrap());
    let total = u64::from_le_bytes(data[16..24].try_into().unwrap());

    // Drift the free count to a bogus value in BOTH superblock copies, re-stamping each CRC so
    // the filesystem still mounts (the lie is internally consistent until fsck recomputes it).
    let bogus = correct_free.wrapping_add(123_456);
    let stamp = |d: &mut [u8], off: usize| {
        d[off + 64..off + 72].copy_from_slice(&bogus.to_le_bytes());
        let crc = crc32::crc32(&d[off..off + FS_SB_CRC_OFF]);
        d[off + FS_SB_CRC_OFF..off + FS_SB_CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
    };
    stamp(&mut data, 0);                                   // primary @ LBA 0
    stamp(&mut data, ((total - 1) as usize) * 512);        // backup @ last LBA
    std::fs::write(persist, &data).unwrap();
    println!("fs-check: drifted free count {} -> {} (both copies), correct value is {}", bogus, bogus, correct_free);

    crate::shell_test::run_fs_check(&image_path, persist, correct_free, 4);
}

/// Phase K: `drives scrub` (read-only integrity sweep). Bake a clean file and a file with a
/// CORRUPTED data block (a flipped payload byte), boot, and `drives scrub`: it must report the
/// bad block (`1 bad`) without panicking, leave the disk UNCHANGED (a second scrub still reports
/// `1 bad` - read-only, no repair), and the clean file must still read back. Proves a routine,
/// non-destructive integrity sweep that detects bit-rot.
fn run_fs_scrub_test() {
    println!("\n=== fs: drives scrub (read-only integrity sweep) - detect bit-rot, change nothing (Phase K) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");

    let persist = "build/tests/persist_fs_scrub.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(persist);
    gsfs_add_file(persist, "good.txt", b"good-payload-survives-the-scrub");
    gsfs_add_file(persist, "bad.txt", b"this payload will be corrupted host-side to fail its data CRC");

    // Flip a payload byte in bad.txt's first data block (not the CRC @508) → its data CRC fails.
    let mut d = std::fs::read(persist).unwrap();
    let root = u64::from_le_bytes(d[48..56].try_into().unwrap()) as usize;
    let rd = root * 512;
    let mut bad_block = 0usize;
    for slot in 0..7 {
        let r = rd + slot * 64;
        if d[r] == 1 && &d[r + 2..r + 2 + d[r + 1] as usize] == b"bad.txt" {
            bad_block = u64::from_le_bytes(d[r + 48..r + 56].try_into().unwrap()) as usize;
            break;
        }
    }
    assert!(bad_block != 0, "could not locate bad.txt data block");
    d[bad_block * 512] ^= 0xFF;
    std::fs::write(persist, &d).unwrap();
    println!("fs-scrub: corrupted bad.txt's data block at LBA {} (good.txt left intact)", bad_block);

    crate::shell_test::run_fs_scrub(&image_path, persist, 4);
}

/// §22 Test 14 - file-as-capability (P2). Boot bare-metal + an AHCI disk, create a file, then run
/// the shell's `fcap` command, which opens the file as a real kernel capability and exercises every
/// property the model promises: read/write THROUGH the cap, non-escalation (a READ-only cap cannot
/// write - at both the kernel and fs layers), a forged handle rejected, and revocation on close.
fn run_fs_filecap_test() {
    println!("\n=== fs: file-as-capability - open→read/write via cap, non-escalation, revoke (§22 Test 14) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");

    let persist = "build/tests/persist_fs_filecap.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(persist);

    crate::shell_test::run_fs_filecap(&image_path, persist, 4);
}

/// Set an UNKNOWN feature bit in a superblock mask and re-stamp the CRC on BOTH copies, so the
/// disk still passes `sb_valid` but carries a feature this build doesn't recognise.
fn set_unknown_feature(path: &str, mask_off: usize, bit: u32) {
    let mut d = std::fs::read(path).unwrap();
    let total = u64::from_le_bytes(d[16..24].try_into().unwrap()) as usize;
    for off in [0usize, (total - 1) * 512] {
        let cur = u32::from_le_bytes(d[off + mask_off..off + mask_off + 4].try_into().unwrap());
        d[off + mask_off..off + mask_off + 4].copy_from_slice(&(cur | bit).to_le_bytes());
        let crc = crc32::crc32(&d[off..off + FS_SB_CRC_OFF]);
        d[off + FS_SB_CRC_OFF..off + FS_SB_CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
    }
    std::fs::write(path, &d).unwrap();
}

/// Phase L: GSFS0008 feature-flag compatibility policy. Bake three otherwise-identical disks
/// (each with a `/baked.txt`), then set an UNKNOWN bit in a different feature mask on each (CRC
/// re-stamped so they still validate), and boot each: an unknown `incompat` bit must REFUSE the
/// mount, an unknown `ro_compat` bit must mount READ-ONLY, an unknown `compat` bit must mount
/// normally. This is the mechanism that lets the format evolve past 0008 without reformatting.
fn run_fs_compat_test() {
    println!("\n=== fs: GSFS0008 feature flags - compat / ro_compat / incompat mount policy (Phase L) ===");
    cmd_build_bare_metal();
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");

    const UNKNOWN_BIT: u32 = 0x8000_0000; // bit 31 - defined in no KNOWN_* set
    let mk = |name: &str, mask_off: usize| -> String {
        let p = format!("build/tests/persist_fs_compat_{name}.img");
        std::fs::write(&p, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
        format_superblock(&p);
        gsfs_add_file(&p, "baked.txt", b"baked-payload-survives");
        set_unknown_feature(&p, mask_off, UNKNOWN_BIT);
        p
    };
    let d_incompat = mk("incompat", FS_FEAT_INCOMPAT_OFF);
    let d_ro       = mk("ro",       FS_FEAT_RO_COMPAT_OFF);
    let d_compat   = mk("compat",   FS_FEAT_COMPAT_OFF);

    crate::shell_test::run_fs_compat(&image_path, &d_incompat, &d_ro, &d_compat, 4);
}

/// Phase H: block I/O retry. Build block-driver with `io-error-test` (forces the first couple
/// of read/write commands to fail), boot, and assert the driver RETRIES + RECOVERS the
/// transient error (the boot self-test read still succeeds) - and that normal operation is
/// unaffected (fs mounts + round-trips). QEMU never fails a real disk read, so the fault must
/// be injected.
fn run_fs_ioretry_test() {
    println!("\n=== fs: block I/O retry - transient failure retried + recovered (Phase H) ===");
    build_blockdev_fs("selftest", "io-error-test");
    let kernel_elf = std::path::Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() { eprintln!("kernel ELF not found"); std::process::exit(1); }
    let limine_dir = std::path::Path::new("tools/limine");
    let image_path = disk_image::create(kernel_elf, limine_dir);
    disk_image::install_bootloader(limine_dir, &image_path);
    let _ = std::fs::create_dir_all("build/tests");
    let persist = "build/tests/persist_fs_ioretry.img";
    std::fs::write(persist, vec![0u8; 16 * 1024 * 1024]).expect("create disk");
    format_superblock(persist); // formatted so fs mounts + the selftest round-trips run
    let img = std::fs::canonicalize(&image_path).unwrap_or_else(|_| image_path.to_path_buf());
    let img_str = img.to_string_lossy().replace('\\', "/");
    let persist_abs = std::fs::canonicalize(persist).unwrap_or_else(|_| std::path::PathBuf::from(persist));
    let persist_str = persist_abs.to_string_lossy().replace('\\', "/");

    println!("fs: boot with forced transient I/O errors, ~25s …");
    let log = boot_ahci_qemu(&img_str, &persist_str, "build/tests/fs_ioretry.log", 25);
    let retried   = log.contains("failed (attempt 1/");             // a retry fired
    let recovered = log.contains("recovered after");                // the transient cleared
    let selftest_ok = log.contains("read self-test OK");            // the injected read ultimately succeeded
    let fs_works  = log.contains("round-trip OK");                  // normal operation unaffected
    let no_panic  = !log.contains("KERNEL PANIC");

    for (tag, ok) in [
        ("retried a failed command (attempt 1/N)", retried),
        ("recovered the transient error", recovered),
        ("the retried read ultimately succeeded", selftest_ok),
        ("normal fs operation unaffected (round-trip OK)", fs_works),
        ("no kernel panic", no_panic),
    ] {
        println!("  {} … {}", if ok { "PASS" } else { "FAIL" }, tag);
    }
    if retried && recovered && selftest_ok && fs_works && no_panic {
        println!("\n  [FS.ioretry]  bounded retry recovers a transient I/O error  … PASS\n\n  5 passed  0 failed");
    } else {
        println!("\n  [FS.ioretry]  block I/O retry  … FAIL\n\n  some failed");
        std::process::exit(1);
    }
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
