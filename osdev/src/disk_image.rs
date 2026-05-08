//! Bootable disk image creation for Limine BIOS boot.
//!
//! Creates a raw disk image with:
//!   - MBR partition table (one FAT32 partition, LBA)
//!   - FAT32 filesystem containing limine-bios.sys, limine.conf, kernel.elf
//!   - Limine BIOS bootloader installed via `limine bios-install`
//!
//! Prerequisites (§17): download a Limine release and extract to tools/limine/.
//! Releases: https://github.com/limine-bootloader/limine/releases
//! Required file: limine-bios.sys (and limine.exe / limine on Windows/Linux)

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const IMAGE_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB
const SECTOR_SIZE: u64 = 512;
const PART_START_LBA: u32 = 2048; // 1 MiB aligned

const LIMINE_CONF: &str = r#"timeout=0

/GodspeedOS
    protocol=limine
    kernel_path=boot():/kernel.elf
"#;

/// Build `build/os.img`: partition table + FAT32 + files.
/// Returns the path to the created image.
pub fn create(kernel_elf: &Path, limine_dir: &Path) -> PathBuf {
    std::fs::create_dir_all("build").expect("failed to create build/");
    let image_path = PathBuf::from("build/os.img");

    let total_sectors = (IMAGE_SIZE / SECTOR_SIZE) as u32;
    let part_sectors = total_sectors - PART_START_LBA;

    // Create and size the image file.
    let img = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&image_path)
        .expect("failed to create build/os.img");
    img.set_len(IMAGE_SIZE).expect("failed to set image size");

    // Write the MBR partition table.
    write_mbr(&img, PART_START_LBA, part_sectors);

    // Format the partition as FAT32 and populate it.
    let part_offset = PART_START_LBA as u64 * SECTOR_SIZE;
    let part_size = part_sectors as u64 * SECTOR_SIZE;
    {
        let partition = OffsetFile::new(img, part_offset, part_size);
        fatfs::format_volume(
            partition,
            fatfs::FormatVolumeOptions::new().volume_label(*b"GODSPEEDOS "),
        )
        .expect("failed to format FAT32 partition");
    }

    // Re-open to populate the filesystem (fatfs takes ownership).
    let img2 = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&image_path)
        .expect("failed to re-open image");
    let partition = OffsetFile::new(img2, part_offset, part_size);
    let fs = fatfs::FileSystem::new(partition, fatfs::FsOptions::new())
        .expect("failed to open FAT32 filesystem");
    {
        let root = fs.root_dir();

        // limine-bios.sys must be at the root for Limine's BIOS boot.
        copy_into_fat(&root, &limine_dir.join("limine-bios.sys"), "limine-bios.sys");

        // Boot configuration.
        let mut conf = root.create_file("limine.conf").expect("create limine.conf");
        conf.write_all(LIMINE_CONF.as_bytes()).expect("write limine.conf");

        // Kernel binary.
        copy_into_fat(&root, kernel_elf, "kernel.elf");
    }

    println!("run: image created at {}", image_path.display());
    image_path
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
