// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! ELF64 service loader - §14.1.
//!
//! Parses ELF64 PT_LOAD segments from a flat byte slice (embedded in the
//! kernel via `include_bytes!(env!("SVC_*_ELF"))`) and maps each segment into
//! a fresh `PageTable`.  BSS (p_memsz > p_filesz) is zero-filled automatically.
//!
//! Only the ELF64 subset used by static services is handled:
//!   - ET_EXEC, EM_X86_64, ELFCLASS64, little-endian
//!   - PT_LOAD program headers (all others are skipped)
//!   - PF_X / PF_W / PF_R flags → PageFlags

use crate::arch::x86_64::page_tables::{
    get_hhdm_offset, MapError, PageFlags, PageTable, VirtAddr, PAGE_SIZE,
};
use crate::memory::allocator::alloc_frame;
use crate::memory::frame::PhysAddr;

// ---------------------------------------------------------------------------
// ELF64 constants.
// ---------------------------------------------------------------------------

const ELF_MAGIC:   [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64:  u8  = 2;
const ELFDATA2LSB: u8  = 1;
const ET_EXEC:     u16 = 2;
const EM_X86_64:   u16 = 62;
const PT_LOAD:     u32 = 1;
// PF_X / PF_W / PF_R and the W^X permission decision live in `crate::elf_flags`
// (host-testable; see hardening H4a).

// ---------------------------------------------------------------------------
// ELF64 header and program header.
// ---------------------------------------------------------------------------

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Elf64Ehdr {
    e_ident:     [u8; 16],
    e_type:      u16,
    e_machine:   u16,
    e_version:   u32,
    e_entry:     u64,
    e_phoff:     u64,
    e_shoff:     u64,
    e_flags:     u32,
    e_ehsize:    u16,
    e_phentsize: u16,
    e_phnum:     u16,
    e_shentsize: u16,
    e_shnum:     u16,
    e_shstrndx:  u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Elf64Phdr {
    p_type:   u32,
    p_flags:  u32,
    p_offset: u64,
    p_vaddr:  u64,
    p_paddr:  u64,
    p_filesz: u64,
    p_memsz:  u64,
    p_align:  u64,
}

/// Copy `size_of::<Elf64Ehdr>()` bytes from the start of `bytes` into a local
/// `Elf64Ehdr`.  After this call every field access is a safe copy out of a
/// local value - no unaligned pointer dereferences at the call site.
///
/// Caller must guarantee `bytes.len() >= size_of::<Elf64Ehdr>()`.
fn read_ehdr(bytes: &[u8]) -> Elf64Ehdr {
    // SAFETY: length pre-checked by caller; Elf64Ehdr is repr(C, packed) so any
    // byte sequence is a valid bit-pattern; read_unaligned requires no alignment.
    unsafe { (bytes.as_ptr() as *const Elf64Ehdr).read_unaligned() }
}

/// Copy `size_of::<Elf64Phdr>()` bytes from `bytes[off..]` into a local
/// `Elf64Phdr`.
///
/// Caller must guarantee `off + size_of::<Elf64Phdr>() <= bytes.len()`.
fn read_phdr(bytes: &[u8], off: usize) -> Elf64Phdr {
    // SAFETY: bounds pre-checked by caller; same reasoning as read_ehdr.
    unsafe { (bytes.as_ptr().add(off) as *const Elf64Phdr).read_unaligned() }
}

// ---------------------------------------------------------------------------
// Public result types.
// ---------------------------------------------------------------------------

/// The output of a successful ELF load: a populated page table and the entry VA.
pub struct LoadedElf {
    pub page_table: PageTable,
    pub entry_va:   u64,
    /// Total page-aligned bytes mapped for the binary's PT_LOAD segments (code + data + BSS).
    pub mapped_bytes: u64,
}

#[derive(Debug)]
pub enum LoadError {
    TooSmall,
    BadMagic,
    NotElf64,
    WrongArch,
    NotExecutable,
    BadProgramHeader,
    SegmentOutOfBounds,
    MapFailed(MapError),
    FrameAllocFailed,
}

impl From<MapError> for LoadError {
    fn from(e: MapError) -> Self {
        LoadError::MapFailed(e)
    }
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

/// Load an ELF64 static executable from `bytes` into a fresh address space.
///
/// Creates a new `PageTable` (with kernel half copied from current CR3), maps
/// all PT_LOAD segments, and returns the entry VA.
///
/// # Safety
/// Must be called after `memory::init` and `page_tables::set_hhdm_offset`.
pub fn load(bytes: &[u8]) -> Result<LoadedElf, LoadError> {
    if bytes.len() < core::mem::size_of::<Elf64Ehdr>() {
        return Err(LoadError::TooSmall);
    }

    let ehdr = read_ehdr(bytes);

    if &ehdr.e_ident[..4] != ELF_MAGIC { return Err(LoadError::BadMagic);     }
    if ehdr.e_ident[4]   != ELFCLASS64 { return Err(LoadError::NotElf64);     }
    if ehdr.e_ident[5]  != ELFDATA2LSB { return Err(LoadError::NotElf64);     }
    if ehdr.e_type    != ET_EXEC   { return Err(LoadError::NotExecutable); }
    if ehdr.e_machine != EM_X86_64 { return Err(LoadError::WrongArch);    }

    if (ehdr.e_phentsize as usize) < core::mem::size_of::<Elf64Phdr>() {
        return Err(LoadError::BadProgramHeader);
    }

    let mut pt = PageTable::new()?;
    let mut mapped_bytes = 0u64;

    let ph_base  = ehdr.e_phoff as usize;
    let ph_step  = ehdr.e_phentsize as usize;
    let ph_count = ehdr.e_phnum as usize;

    for i in 0..ph_count {
        let off = ph_base
            .checked_add(i.checked_mul(ph_step).ok_or(LoadError::BadProgramHeader)?)
            .ok_or(LoadError::BadProgramHeader)?;

        if off.checked_add(core::mem::size_of::<Elf64Phdr>())
               .ok_or(LoadError::BadProgramHeader)? > bytes.len()
        {
            return Err(LoadError::BadProgramHeader);
        }

        let phdr = read_phdr(bytes, off);

        if phdr.p_type != PT_LOAD { continue; }

        let p_flags  = phdr.p_flags;
        let p_offset = phdr.p_offset as usize;
        let p_vaddr  = phdr.p_vaddr;
        let p_filesz = phdr.p_filesz as usize;
        let p_memsz  = phdr.p_memsz as usize;

        if p_filesz > p_memsz {
            return Err(LoadError::BadProgramHeader);
        }
        if p_offset.checked_add(p_filesz).ok_or(LoadError::SegmentOutOfBounds)? > bytes.len() {
            return Err(LoadError::SegmentOutOfBounds);
        }

        // Derive page flags from ELF segment flags, ENFORCING W^X (hardening H4a):
        // a page is executable only if its segment is executable AND not writable.
        // The loader enforces the invariant rather than mirroring the ELF, so a
        // W+X segment is forced NO_EXEC (and the anomaly logged) - a malformed or
        // hostile binary cannot obtain a writable-executable mapping.
        let mut flags = PageFlags::PRESENT | PageFlags::USER;
        if crate::elf_flags::segment_writable(p_flags) { flags |= PageFlags::WRITABLE; }
        if crate::elf_flags::segment_no_exec(p_flags)  { flags |= PageFlags::NO_EXEC;  }
        if crate::elf_flags::segment_is_wx(p_flags) {
            crate::kprintln!(
                "loader: W^X - segment p_flags={:#x} was W+X, forced NO_EXEC", p_flags);
        }

        // Map full page-aligned VA range covering [p_vaddr .. p_vaddr + p_memsz).
        let va_start = p_vaddr & !(PAGE_SIZE as u64 - 1);
        let va_end   = (p_vaddr + p_memsz as u64 + PAGE_SIZE as u64 - 1)
                       & !(PAGE_SIZE as u64 - 1);

        let mut va = va_start;
        while va < va_end {
            let frame = alloc_frame().ok_or(LoadError::FrameAllocFailed)?;
            let phys  = frame.phys_addr().0;

            // SAFETY: phys from allocator; HHDM is set up before load() is called.
            unsafe {
                let dst = (get_hhdm_offset() + phys) as *mut u8;
                // Zero first - this covers BSS (memsz > filesz) for free.
                core::ptr::write_bytes(dst, 0, PAGE_SIZE);
            }

            // Copy file bytes for the overlap of this page with the file data range.
            let page_end   = va + PAGE_SIZE as u64;
            let copy_start = va.max(p_vaddr);
            let copy_end   = page_end.min(p_vaddr + p_filesz as u64);

            if copy_start < copy_end {
                let copy_len = (copy_end - copy_start) as usize;
                let src_off  = p_offset + (copy_start - p_vaddr) as usize;
                let dst_off  = (copy_start - va) as usize;

                // SAFETY: src_off + copy_len ≤ bytes.len() (validated above);
                // dst within the just-zeroed frame.
                unsafe {
                    let dst = (get_hhdm_offset() + phys) as *mut u8;
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(src_off),
                        dst.add(dst_off),
                        copy_len,
                    );
                }
            }

            pt.map(VirtAddr(va), PhysAddr(phys), flags)?;

            // Frame ownership passes to the page table; freed at task death
            // (Phase 5). `Frame` is `Copy`/no-Drop, so there is nothing to
            // release here - not freeing it is the leak.
            va += PAGE_SIZE as u64;
        }
        mapped_bytes += va_end - va_start;
    }

    Ok(LoadedElf { page_table: pt, entry_va: ehdr.e_entry, mapped_bytes })
}

// ---------------------------------------------------------------------------
// ELF-loader fuzz - §22 Fuzz F3.  Compiled only with `--features test-bad-elf`.
// ---------------------------------------------------------------------------

/// Run 77 malformed-ELF inputs through `load()`, assert no kernel panic,
/// print the pass string, and halt.  Called once from `kernel_main` before
/// any services are spawned.
#[cfg(feature = "test-bad-elf")]
pub fn run_elf_fuzz() -> ! {
    // Base valid ELF64 header, 64 bytes, e_phnum=0.
    // Passes every check in load() and produces an empty LoadedElf.
    // Used as the mutation seed for the 64 single-byte-flip cases.
    let base: [u8; 64] = [
        // e_ident[16]
        0x7f, b'E', b'L', b'F',     // magic
        2, 1, 1, 0,                  // ELFCLASS64, ELFDATA2LSB, ABI=SysV, v1
        0, 0, 0, 0, 0, 0, 0, 0,     // padding
        // e_type[2], e_machine[2], e_version[4]
        2, 0,                        // ET_EXEC
        0x3e, 0,                     // EM_X86_64 = 62
        1, 0, 0, 0,                  // EV_CURRENT
        // e_entry[8]
        0, 0, 0, 0, 0, 0, 0, 0,
        // e_phoff[8]
        0, 0, 0, 0, 0, 0, 0, 0,
        // e_shoff[8]
        0, 0, 0, 0, 0, 0, 0, 0,
        // e_flags[4]
        0, 0, 0, 0,
        // e_ehsize[2], e_phentsize[2], e_phnum[2], e_shentsize[2], e_shnum[2], e_shstrndx[2]
        64, 0,  // e_ehsize  = 64
        56, 0,  // e_phentsize = sizeof(Elf64Phdr)
        0, 0,   // e_phnum  = 0
        64, 0,  // e_shentsize
        0, 0,   // e_shnum
        0, 0,   // e_shstrndx
    ];

    let mut n: u32 = 0;

    // ── Specific bad-input cases (13) ────────────────────────────────────────

    let _ = load(&[]);                                            n += 1; // 1: empty → TooSmall
    let _ = load(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0]); n += 1; // 2: 10 B → TooSmall
    let _ = load(&[0u8; 64]);                                    n += 1; // 3: all-zero → BadMagic
    let _ = load(&[0xffu8; 64]);                                  n += 1; // 4: all-0xFF → BadMagic

    let mut b = base; b[4] = 1;
    let _ = load(&b); n += 1;              // 5: ELFCLASS32 → NotElf64

    let mut b = base; b[5] = 2;
    let _ = load(&b); n += 1;              // 6: big-endian → NotElf64

    let mut b = base; b[16] = 3;
    let _ = load(&b); n += 1;              // 7: ET_DYN → NotExecutable

    let mut b = base; b[18] = 3; b[19] = 0;
    let _ = load(&b); n += 1;              // 8: EM_386 → WrongArch

    // 9: e_phentsize=32 with e_phnum=1 → BadProgramHeader (fires before PageTable::new)
    let mut b = base; b[54] = 32; b[56] = 1;
    let _ = load(&b); n += 1;

    // 10: e_phnum=1, e_phoff=u64::MAX → phdr offset overflow → BadProgramHeader
    let mut b = base; b[56] = 1;
    b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    let _ = load(&b); n += 1;

    // 11: e_phnum=1, e_phoff=64, file is only 64 bytes → phdr OOB → BadProgramHeader
    let mut b = base; b[56] = 1; b[32] = 64;
    let _ = load(&b); n += 1;

    // 12: PT_LOAD with p_filesz(10) > p_memsz(5) → BadProgramHeader
    {
        let mut buf = [0u8; 120];
        buf[..64].copy_from_slice(&base);
        buf[32] = 64;   // e_phoff = 64 (Elf64Phdr follows the header)
        buf[56] = 1;    // e_phnum = 1
        // Elf64Phdr layout at buf[64]: p_type[4] p_flags[4] p_offset[8]
        //   p_vaddr[8] p_paddr[8] p_filesz[8] p_memsz[8] p_align[8]
        buf[64] = 1;    // p_type  = PT_LOAD
        buf[68] = 4;    // p_flags = PF_R
        buf[96] = 10;   // p_filesz (phdr+32) = 10
        buf[104] = 5;   // p_memsz  (phdr+40) = 5 → filesz > memsz
        let _ = load(&buf); n += 1;
    }

    // 13: PT_LOAD, p_offset(60)+p_filesz(100)=160 > file_len(120) → SegmentOutOfBounds
    {
        let mut buf = [0u8; 120];
        buf[..64].copy_from_slice(&base);
        buf[32] = 64;   // e_phoff = 64
        buf[56] = 1;    // e_phnum = 1
        buf[64] = 1;    // p_type  = PT_LOAD
        buf[68] = 4;    // p_flags = PF_R
        buf[72] = 60;   // p_offset (phdr+8)  = 60
        buf[96] = 100;  // p_filesz (phdr+32) = 100 → 60+100=160 > 120
        buf[104] = 100; // p_memsz  (phdr+40) = 100
        let _ = load(&buf); n += 1;
    }

    // ── Byte-flip cases (64): flip each byte of the base header once ─────────
    for i in 0..64usize {
        let mut b = base;
        b[i] = !b[i];
        let _ = load(&b);
        n += 1;
    }

    crate::kprintln!("fuzz: F3 pass ({}/{})", n, n);
    crate::arch::x86_64::halt_all_cores()
}

/// Brutal ELF loader fuzz - Milestone 17 BF3.
///
/// Runs the same 13 specific bad-input cases as `run_elf_fuzz`, then adds
/// 200 xorshift-random single-byte mutations (vs. 64 sequential flips) and
/// 50 random multi-byte corruption cases (2–4 bytes flipped per variant).
/// Total: 13 + 200 + 50 = 263 inputs.
#[cfg(feature = "test-bad-elf-brutal")]
pub fn run_elf_fuzz_brutal() -> ! {
    let base: [u8; 64] = [
        0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        2, 0, 0x3e, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 64, 0, 56, 0, 0, 0, 64, 0, 0, 0, 0, 0,
    ];

    let mut n: u32 = 0;

    // ── Same 13 specific bad-input cases as F3 ───────────────────────────────
    let _ = load(&[]);                                            n += 1;
    let _ = load(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0]); n += 1;
    let _ = load(&[0u8; 64]);                                    n += 1;
    let _ = load(&[0xffu8; 64]);                                  n += 1;
    let mut b = base; b[4] = 1;  let _ = load(&b); n += 1;
    let mut b = base; b[5] = 2;  let _ = load(&b); n += 1;
    let mut b = base; b[16] = 3; let _ = load(&b); n += 1;
    let mut b = base; b[18] = 3; b[19] = 0; let _ = load(&b); n += 1;
    let mut b = base; b[54] = 32; b[56] = 1; let _ = load(&b); n += 1;
    let mut b = base; b[56] = 1;
    b[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    let _ = load(&b); n += 1;
    let mut b = base; b[56] = 1; b[32] = 64; let _ = load(&b); n += 1;
    {
        let mut buf = [0u8; 120]; buf[..64].copy_from_slice(&base);
        buf[32] = 64; buf[56] = 1; buf[64] = 1; buf[68] = 4;
        buf[96] = 10; buf[104] = 5; let _ = load(&buf); n += 1;
    }
    {
        let mut buf = [0u8; 120]; buf[..64].copy_from_slice(&base);
        buf[32] = 64; buf[56] = 1; buf[64] = 1; buf[68] = 4;
        buf[72] = 60; buf[96] = 100; buf[104] = 100;
        let _ = load(&buf); n += 1;
    }

    // ── 200 xorshift random single-byte mutations ─────────────────────────────
    let mut rng: u64 = 0xBF3_FEED_u64;
    for _ in 0..200usize {
        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
        let idx = (rng >> 32) as usize % 64;
        let val = rng as u8;
        let mut b = base; b[idx] = val;
        let _ = load(&b); n += 1;
    }

    // ── 50 random multi-byte (2–4 byte) mutations ─────────────────────────────
    for _ in 0..50usize {
        rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
        let count = 2 + (rng % 3) as usize; // 2, 3, or 4 bytes
        let mut b = base;
        for k in 0..count {
            rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17;
            let idx = ((rng >> 32) as usize + k * 17) % 64;
            b[idx] = rng as u8;
        }
        let _ = load(&b); n += 1;
    }

    crate::kprintln!("fuzz: BF3 pass ({}/{})", n, n);
    crate::arch::x86_64::halt_all_cores()
}
