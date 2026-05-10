//! ELF64 service loader — §14.1.
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
const PF_X: u32 = 1;
const PF_W: u32 = 2;

// ---------------------------------------------------------------------------
// ELF64 header and program header (packed; fields read via addr_of!).
// ---------------------------------------------------------------------------

#[repr(C, packed)]
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

// ---------------------------------------------------------------------------
// Public result types.
// ---------------------------------------------------------------------------

/// The output of a successful ELF load: a populated page table and the entry VA.
pub struct LoadedElf {
    pub page_table: PageTable,
    pub entry_va:   u64,
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

    // SAFETY: length checked; Elf64Ehdr is packed so no alignment constraint.
    let ehdr = bytes.as_ptr() as *const Elf64Ehdr;

    // Read packed fields without creating unaligned references (§18.3).
    // SAFETY: all addr_of! targets are within the bounds-checked ehdr.
    let e_ident = unsafe { core::ptr::addr_of!((*ehdr).e_ident).read_unaligned() };
    if &e_ident[..4] != ELF_MAGIC { return Err(LoadError::BadMagic);     }
    if e_ident[4]   != ELFCLASS64 { return Err(LoadError::NotElf64);     }
    if e_ident[5]  != ELFDATA2LSB { return Err(LoadError::NotElf64);     }

    let e_type    = unsafe { core::ptr::addr_of!((*ehdr).e_type).read_unaligned()    };
    let e_machine = unsafe { core::ptr::addr_of!((*ehdr).e_machine).read_unaligned() };
    if e_type    != ET_EXEC   { return Err(LoadError::NotExecutable); }
    if e_machine != EM_X86_64 { return Err(LoadError::WrongArch);    }

    let e_entry     = unsafe { core::ptr::addr_of!((*ehdr).e_entry).read_unaligned()     };
    let e_phoff     = unsafe { core::ptr::addr_of!((*ehdr).e_phoff).read_unaligned()     };
    let e_phentsize = unsafe { core::ptr::addr_of!((*ehdr).e_phentsize).read_unaligned() };
    let e_phnum     = unsafe { core::ptr::addr_of!((*ehdr).e_phnum).read_unaligned()     };

    if (e_phentsize as usize) < core::mem::size_of::<Elf64Phdr>() {
        return Err(LoadError::BadProgramHeader);
    }

    let mut pt = PageTable::new()?;

    let ph_base  = e_phoff as usize;
    let ph_step  = e_phentsize as usize;
    let ph_count = e_phnum as usize;

    for i in 0..ph_count {
        let off = ph_base
            .checked_add(i.checked_mul(ph_step).ok_or(LoadError::BadProgramHeader)?)
            .ok_or(LoadError::BadProgramHeader)?;

        if off.checked_add(core::mem::size_of::<Elf64Phdr>())
               .ok_or(LoadError::BadProgramHeader)? > bytes.len()
        {
            return Err(LoadError::BadProgramHeader);
        }

        // SAFETY: bounds checked above; packed struct.
        let phdr = unsafe { bytes.as_ptr().add(off) as *const Elf64Phdr };

        let p_type = unsafe { core::ptr::addr_of!((*phdr).p_type).read_unaligned() };
        if p_type != PT_LOAD { continue; }

        let p_flags  = unsafe { core::ptr::addr_of!((*phdr).p_flags).read_unaligned()  };
        let p_offset = unsafe { core::ptr::addr_of!((*phdr).p_offset).read_unaligned() } as usize;
        let p_vaddr  = unsafe { core::ptr::addr_of!((*phdr).p_vaddr).read_unaligned()  };
        let p_filesz = unsafe { core::ptr::addr_of!((*phdr).p_filesz).read_unaligned() } as usize;
        let p_memsz  = unsafe { core::ptr::addr_of!((*phdr).p_memsz).read_unaligned()  } as usize;

        if p_filesz > p_memsz {
            return Err(LoadError::BadProgramHeader);
        }
        if p_offset.checked_add(p_filesz).ok_or(LoadError::SegmentOutOfBounds)? > bytes.len() {
            return Err(LoadError::SegmentOutOfBounds);
        }

        // Derive page flags from ELF segment flags.
        let mut flags = PageFlags::PRESENT | PageFlags::USER;
        if p_flags & PF_W != 0 { flags |= PageFlags::WRITABLE; }
        if p_flags & PF_X == 0 { flags |= PageFlags::NO_EXEC;  }

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
                // Zero first — this covers BSS (memsz > filesz) for free.
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

            // Frames are owned by the page table; freed at task death (Phase 5).
            core::mem::forget(frame);

            va += PAGE_SIZE as u64;
        }
    }

    Ok(LoadedElf { page_table: pt, entry_va: e_entry })
}
