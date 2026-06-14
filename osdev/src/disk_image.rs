// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Bootable disk image creation — BIOS (MBR) and UEFI (GPT) paths.
//!
//! BIOS path (`create` / `create_at`):
//!   MBR partition table + FAT32 + limine-bios.sys + limine bios-install.
//!   Used by `osdev run` (QEMU).
//!
//! UEFI path (`create_uefi` / `create_uefi_at`):
//!   GPT partition table + EFI System Partition (FAT32) + BOOTX64.EFI.
//!   Used by `osdev image` (bare-metal USB).
//!
//! Prerequisites: tools/limine/ must contain:
//!   limine-bios.sys, limine.exe  — for BIOS path
//!   BOOTX64.EFI                  — for UEFI path

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const IMAGE_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB
const SECTOR_SIZE: u64 = 512;
const PART_START_LBA: u32 = 2048; // 1 MiB aligned

const LIMINE_CONF: &str = r#"timeout: -1

/GodspeedOS
    protocol: limine
    kernel_path: boot():/kernel.elf
"#;

/// Build a bootable disk image at `image_path`.
///
/// Lower-level version of `create`; used when a non-default image path is
/// needed (e.g. the bad-registry test image for §22 Test 1B).
pub fn create_at(kernel_elf: &Path, limine_dir: &Path, image_path: &Path) -> PathBuf {
    if let Some(parent) = image_path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create image parent dir");
    }
    let image_path = image_path.to_path_buf();

    let total_sectors = (IMAGE_SIZE / SECTOR_SIZE) as u32;
    let part_sectors = total_sectors - PART_START_LBA;

    let img = std::fs::OpenOptions::new()
        .read(true).write(true).create(true).truncate(true)
        .open(&image_path)
        .unwrap_or_else(|e| panic!("failed to create {}: {}", image_path.display(), e));
    img.set_len(IMAGE_SIZE).expect("failed to set image size");

    write_mbr(&img, PART_START_LBA, part_sectors);

    let part_offset = PART_START_LBA as u64 * SECTOR_SIZE;
    let part_size = part_sectors as u64 * SECTOR_SIZE;
    {
        let partition = OffsetFile::new(img, part_offset, part_size);
        fatfs::format_volume(
            partition,
            fatfs::FormatVolumeOptions::new().volume_label(*b"GODSPEED_OS"),
        ).expect("failed to format FAT32 partition");
    }

    let img2 = std::fs::OpenOptions::new().read(true).write(true).open(&image_path)
        .expect("failed to re-open image");
    let partition = OffsetFile::new(img2, part_offset, part_size);
    let fs = fatfs::FileSystem::new(partition, fatfs::FsOptions::new())
        .expect("failed to open FAT32 filesystem");
    {
        let root = fs.root_dir();
        copy_into_fat(&root, &limine_dir.join("limine-bios.sys"), "limine-bios.sys");
        let mut conf = root.create_file("limine.conf").expect("create limine.conf");
        conf.write_all(LIMINE_CONF.as_bytes()).expect("write limine.conf");
        copy_into_fat(&root, kernel_elf, "kernel.elf");
    }

    println!("run: image created at {}", image_path.display());
    image_path
}

/// Build `build/os.img`: partition table + FAT32 + files.
/// Returns the path to the created image.
pub fn create(kernel_elf: &Path, limine_dir: &Path) -> PathBuf {
    create_at(kernel_elf, limine_dir, Path::new("build/os.img"))
}

/// Install the Limine BIOS bootloader into the MBR of the disk image.
///
/// Requires `tools/limine/limine.exe` (Windows) or `tools/limine/limine` (Linux).
/// If not found, prints instructions and exits.
pub fn install_bootloader(limine_dir: &Path, image_path: &Path) {
    let limine_bin = if cfg!(windows) {
        limine_dir.join("limine.exe")
    } else {
        limine_dir.join("limine")
    };

    if !limine_bin.exists() {
        eprintln!("Limine binary not found at {}", limine_bin.display());
        eprintln!();
        eprintln!("To set up Limine:");
        eprintln!("  1. Download a release from:");
        eprintln!("     https://github.com/limine-bootloader/limine/releases");
        eprintln!("  2. Extract to tools/limine/");
        eprintln!("  3. Ensure limine-bios.sys and limine.exe are present");
        std::process::exit(1);
    }

    let status = std::process::Command::new(&limine_bin)
        .args(["bios-install", &image_path.to_string_lossy()])
        .status()
        .expect("failed to run limine bios-install");

    if !status.success() {
        eprintln!("limine bios-install failed");
        std::process::exit(1);
    }
    println!("run: Limine BIOS bootloader installed");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_mbr(mut img: &std::fs::File, part_start: u32, part_sectors: u32) {
    // Seek to partition table (offset 446).
    (&mut img as &mut dyn Write)
        .write_all(&[])
        .ok(); // ensure type inference
    img.seek(SeekFrom::Start(446)).expect("seek to partition table");

    // Partition entry 1: bootable, FAT32 LBA (type 0x0C).
    let entry: [u8; 16] = [
        0x80,                                   // bootable
        0xFF, 0xFF, 0xFF,                       // CHS first (ignored with LBA)
        0x0C,                                   // type: FAT32 LBA
        0xFF, 0xFF, 0xFF,                       // CHS last (ignored with LBA)
        (part_start & 0xFF) as u8,              // LBA start
        ((part_start >> 8) & 0xFF) as u8,
        ((part_start >> 16) & 0xFF) as u8,
        ((part_start >> 24) & 0xFF) as u8,
        (part_sectors & 0xFF) as u8,            // LBA sector count
        ((part_sectors >> 8) & 0xFF) as u8,
        ((part_sectors >> 16) & 0xFF) as u8,
        ((part_sectors >> 24) & 0xFF) as u8,
    ];
    img.write_all(&entry).expect("write partition entry");
    img.write_all(&[0u8; 48]).expect("write empty partition entries"); // entries 2-4
    img.write_all(&[0x55, 0xAA]).expect("write MBR signature");
}

fn copy_into_fat<IO>(root: &fatfs::Dir<IO>, src: &Path, dst_name: &str)
where
    IO: Read + Write + Seek,
{
    let data = std::fs::read(src)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", src.display(), e));
    let mut file = root
        .create_file(dst_name)
        .unwrap_or_else(|e| panic!("failed to create {} in FAT: {}", dst_name, e));
    file.write_all(&data)
        .unwrap_or_else(|e| panic!("failed to write {} to FAT: {}", dst_name, e));
}

// ---------------------------------------------------------------------------
// OffsetFile — presents a byte range of a File as a ReadWriteSeek impl.
// ---------------------------------------------------------------------------

struct OffsetFile {
    inner: std::fs::File,
    base:  u64,
    size:  u64,
    pos:   u64,
}

impl OffsetFile {
    fn new(inner: std::fs::File, base: u64, size: u64) -> Self {
        Self { inner, base, size, pos: 0 }
    }

    fn abs_pos(&self) -> u64 {
        self.base + self.pos
    }
}

impl Read for OffsetFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.seek(SeekFrom::Start(self.abs_pos()))?;
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for OffsetFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.seek(SeekFrom::Start(self.abs_pos()))?;
        let n = self.inner.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for OffsetFile {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new_pos: u64 = match pos {
            SeekFrom::Start(p)   => p,
            SeekFrom::End(p)     => (self.size as i64 + p).max(0) as u64,
            SeekFrom::Current(p) => (self.pos  as i64 + p).max(0) as u64,
        };
        self.pos = new_pos.min(self.size);
        Ok(self.pos)
    }
}

// ---------------------------------------------------------------------------
// UEFI image — GPT partition table + EFI System Partition
// ---------------------------------------------------------------------------
//
// Disk layout (64 MiB, 512-byte sectors, 131072 sectors total):
//   LBA 0           — Protective MBR
//   LBA 1           — Primary GPT header
//   LBA 2–33        — Primary GPT partition entries (128 × 128 B = 32 sectors)
//   LBA 34–2047     — Unused (alignment gap to 1 MiB)
//   LBA 2048–131038 — EFI System Partition (FAT32, ~63 MiB)
//   LBA 131039–131070 — Secondary GPT partition entries
//   LBA 131071      — Secondary GPT header
//
// The ESP contains:
//   EFI/BOOT/BOOTX64.EFI — Limine UEFI loader
//   limine.conf           — boot menu
//   kernel.elf            — GodspeedOS kernel

const TOTAL_SECTORS: u64    = IMAGE_SIZE / SECTOR_SIZE;     // 131072
const GPT_ENTRY_SECTS: u64  = 32;                           // 128 entries × 128 B
const ESP_START_LBA: u64    = 2048;
const SEC_ENTRIES_LBA: u64  = TOTAL_SECTORS - 1 - GPT_ENTRY_SECTS; // 131039
const LAST_USABLE_LBA: u64  = SEC_ENTRIES_LBA - 1;          // 131038
const ESP_END_LBA: u64      = LAST_USABLE_LBA;

/// Build a UEFI-bootable disk image at `build/os.img`. Returns the path.
pub fn create_uefi(kernel_elf: &Path, limine_dir: &Path) -> PathBuf {
    create_uefi_at(kernel_elf, limine_dir, Path::new("build/os.img"))
}

pub fn create_uefi_at(kernel_elf: &Path, limine_dir: &Path, image_path: &Path) -> PathBuf {
    if let Some(p) = image_path.parent() {
        std::fs::create_dir_all(p).expect("create image parent dir");
    }

    // Create zeroed image.
    {
        let img = std::fs::OpenOptions::new()
            .read(true).write(true).create(true).truncate(true)
            .open(image_path)
            .unwrap_or_else(|e| panic!("create {}: {e}", image_path.display()));
        img.set_len(IMAGE_SIZE).expect("set image size");
    }

    // Sector 0: Protective MBR.
    {
        let mut mbr = [0u8; 512];
        mbr[446] = 0x00;                                          // not bootable
        mbr[447..450].copy_from_slice(&[0x00, 0x02, 0x00]);       // CHS first (ignored)
        mbr[450] = 0xEE;                                          // type: GPT protective
        mbr[451..454].copy_from_slice(&[0xFF, 0xFF, 0xFF]);       // CHS last (ignored)
        mbr[454..458].copy_from_slice(&1u32.to_le_bytes());       // LBA start = 1
        let prot = ((TOTAL_SECTORS - 1) as u32).min(0xFFFF_FFFF);
        mbr[458..462].copy_from_slice(&prot.to_le_bytes());
        mbr[510] = 0x55; mbr[511] = 0xAA;
        write_at(image_path, 0, &mbr);
    }

    // Build GPT partition entries array (128 × 128 bytes, all but first zeroed).
    let mut gpt_entries = [0u8; 128 * 128];
    {
        // EFI System Partition type GUID: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
        // GPT stores the first three fields little-endian, last two big-endian.
        let type_guid: [u8; 16] = [
            0x28, 0x73, 0x2A, 0xC1,  // C12A7328 LE
            0x1F, 0xF8,              // F81F LE
            0xD2, 0x11,              // 11D2 LE
            0xBA, 0x4B,              // BA4B BE
            0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B, // 00A0C93EC93B BE
        ];
        // Unique partition GUID (fixed, deterministic).
        let part_guid: [u8; 16] = *b"GodspeedOS-ESP-\x00";
        let e = &mut gpt_entries[0..128];
        e[0..16].copy_from_slice(&type_guid);
        e[16..32].copy_from_slice(&part_guid);
        e[32..40].copy_from_slice(&ESP_START_LBA.to_le_bytes());
        e[40..48].copy_from_slice(&ESP_END_LBA.to_le_bytes());
        // Partition name "EFI System Partition" in UTF-16LE at offset 56.
        for (i, ch) in "EFI System Partition".encode_utf16().enumerate() {
            let off = 56 + i * 2;
            e[off..off + 2].copy_from_slice(&ch.to_le_bytes());
        }
    }
    let entries_crc = gpt_crc32(&gpt_entries);

    // Disk GUID (fixed, deterministic).
    let disk_guid: [u8; 16] = *b"GodspeedOS-Disk\x00";

    // Sector 1: Primary GPT header.
    let primary_hdr = build_gpt_header(
        1, TOTAL_SECTORS - 1, 34, LAST_USABLE_LBA, &disk_guid, 2, entries_crc,
    );
    write_at(image_path, SECTOR_SIZE, &primary_hdr);

    // Sectors 2–33: Primary GPT entries.
    write_at(image_path, 2 * SECTOR_SIZE, &gpt_entries);

    // Format EFI System Partition as FAT32.
    let esp_byte_off  = ESP_START_LBA * SECTOR_SIZE;
    let esp_byte_size = (ESP_END_LBA - ESP_START_LBA + 1) * SECTOR_SIZE;
    {
        let part = OffsetFile::new(
            std::fs::OpenOptions::new().read(true).write(true).open(image_path).unwrap(),
            esp_byte_off, esp_byte_size,
        );
        fatfs::format_volume(part, fatfs::FormatVolumeOptions::new().volume_label(*b"GODSPEED_OS"))
            .expect("format ESP FAT32");
    }

    // Populate ESP with bootloader + config + kernel.
    {
        let part = OffsetFile::new(
            std::fs::OpenOptions::new().read(true).write(true).open(image_path).unwrap(),
            esp_byte_off, esp_byte_size,
        );
        let fs   = fatfs::FileSystem::new(part, fatfs::FsOptions::new()).expect("open ESP FAT32");
        let root = fs.root_dir();

        let efi_dir  = root.create_dir("EFI").expect("create EFI/");
        let boot_dir = efi_dir.create_dir("BOOT").expect("create EFI/BOOT/");
        copy_into_fat(&boot_dir, &limine_dir.join("BOOTX64.EFI"), "BOOTX64.EFI");

        let mut conf = root.create_file("limine.conf").expect("create limine.conf");
        conf.write_all(LIMINE_CONF.as_bytes()).expect("write limine.conf");
        copy_into_fat(&root, kernel_elf, "kernel.elf");
    }

    // Secondary GPT entries.
    write_at(image_path, SEC_ENTRIES_LBA * SECTOR_SIZE, &gpt_entries);

    // Last sector: Secondary GPT header.
    let secondary_hdr = build_gpt_header(
        TOTAL_SECTORS - 1, 1, 34, LAST_USABLE_LBA, &disk_guid, SEC_ENTRIES_LBA, entries_crc,
    );
    write_at(image_path, (TOTAL_SECTORS - 1) * SECTOR_SIZE, &secondary_hdr);

    println!("run: UEFI image created at {}", image_path.display());
    image_path.to_path_buf()
}

/// Write `data` into the image at `byte_offset`. Opens a fresh file handle each time.
fn write_at(image_path: &Path, byte_offset: u64, data: &[u8]) {
    let mut f = std::fs::OpenOptions::new().write(true).open(image_path)
        .unwrap_or_else(|e| panic!("open {} for write: {e}", image_path.display()));
    f.seek(SeekFrom::Start(byte_offset)).expect("seek");
    f.write_all(data).expect("write");
}

/// Build a 512-byte sector containing a GPT header (92 bytes used, rest zeroed).
fn build_gpt_header(
    my_lba: u64, alt_lba: u64,
    first_usable: u64, last_usable: u64,
    disk_guid: &[u8; 16],
    entries_lba: u64,
    entries_crc: u32,
) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[0..8].copy_from_slice(b"EFI PART");                  // signature
    h[8..12].copy_from_slice(&[0x00, 0x00, 0x01, 0x00]);   // revision 1.0
    h[12..16].copy_from_slice(&92u32.to_le_bytes());        // header size
    // h[16..20] = CRC32 — computed below with field zeroed
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
    h[40..48].copy_from_slice(&first_usable.to_le_bytes());
    h[48..56].copy_from_slice(&last_usable.to_le_bytes());
    h[56..72].copy_from_slice(disk_guid);
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());       // num entries
    h[84..88].copy_from_slice(&128u32.to_le_bytes());       // entry size
    h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let crc = gpt_crc32(&h[0..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}

fn gpt_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}
