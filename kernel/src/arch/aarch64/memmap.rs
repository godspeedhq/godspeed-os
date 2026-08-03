// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory map for the Raspberry Pi 4, from the device tree the firmware hands over.
//!
//! On x86 Limine supplies a classified memory map and the kernel consumes it. There is no equivalent
//! here, so this module builds one, from the two sources the board actually offers:
//!
//! 1. **The device tree** (pointer in `x0` at entry, preserved by `_start`). Authoritative: its
//!    `/memory` node describes every bank, at full 64-bit width.
//! 2. **The mailbox** `GET ARM MEMORY` tag. A fallback, and a deliberately weak one: the tag returns a
//!    32-bit base and size, so it cannot describe RAM above 4 GiB and, on the usual firmware
//!    configuration, reports only the low portion of a board with more than 1 GiB. Using it means
//!    running on less RAM than the machine has - which is safe, and is why it is an acceptable
//!    fallback, but it is not the truth and is reported as such.
//!
//! ## Parsing firmware input defensively
//!
//! The device tree is **untrusted input** in the same sense a device descriptor is: it arrives from
//! outside the kernel, and a malformed one must produce a refusal rather than a wild read. Every
//! offset is bounds-checked against the header's `totalsize` before use, the walk is iteration-bounded,
//! and anything that does not parse yields `None` - never a partially-believed map. A memory map that
//! is wrong in the safe direction (too small) loses capacity; one that is wrong in the unsafe direction
//! hands the allocator RAM that does not exist, and the failure surfaces much later as corruption.
//!
//! ## What this does NOT do yet
//!
//! It does not carve out the VideoCore's reserved region beyond what the device tree already excludes,
//! and it does not read the `/reserved-memory` node. Both matter before the allocator is handed this
//! map for real. Recorded here rather than silently assumed away (§26.3).

use super::mailbox::BoardInfo;
use super::{put_dec, put_hex, put_str};
use super::{MemoryKind, MemoryRegion};

/// Device-tree magic, big-endian in the file, compared after byte-swapping.
const FDT_MAGIC: u32 = 0xD00D_FEED;

// Structure block tokens.
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Iteration bound for the structure walk. A malformed tree must not spin forever (invariant 12); this
/// is far above any real tree's token count, so hitting it means the input is broken, not large.
const MAX_TOKENS: u32 = 200_000;

/// How many memory banks we record. The Pi 4 has one; the bound exists so a strange tree truncates
/// loudly rather than writing past an array (§26.6.1 - the footprint is fixed and visible).
const MAX_BANKS: usize = 8;

/// How many regions the assembled map can hold: the banks, split around the kernel image, plus the
/// kernel region itself.
const MAX_REGIONS: usize = MAX_BANKS * 2 + 3; // banks split around the kernel, the kernel, and page 0

extern "C" {
    static __kernel_end: u8;
}

/// The kernel image occupies `[0x80000, __kernel_end)`. The load address is fixed by the firmware's
/// flat-binary contract (see the linker script), so it is a constant rather than a symbol.
const KERNEL_PHYS_START: u64 = 0x8_0000;

/// One frame. The first one is reserved rather than handed out - see `build`.
const PAGE_SIZE: u64 = 4096;

fn kernel_phys_end() -> u64 {
    // SAFETY: linker-provided symbol; taking its address does not dereference it. The kernel is linked
    // high, so this is a TTBR1 address and must be converted - a memory map built from the raw symbol
    // would carve a hole in the wrong place entirely and hand the allocator the kernel's own RAM.
    unsafe { super::mmu::virt_to_phys(core::ptr::addr_of!(__kernel_end) as u64) }
}

/// A bank of RAM as the firmware describes it.
#[derive(Clone, Copy, Default)]
struct Bank {
    base: u64,
    len: u64,
}

/// A bounds-checked cursor over the flattened device tree blob.
struct Fdt {
    base: u64,
    total: u32,
}

impl Fdt {
    /// Validate the header and return a cursor, or `None` if this is not a device tree.
    fn open(ptr: u64) -> Option<Fdt> {
        if ptr == 0 || ptr & 0x7 != 0 {
            return None; // the spec requires 8-byte alignment; a misaligned pointer is not a DTB
        }
        // SAFETY: reading 4 bytes at the firmware-supplied pointer. Identity-mapped RAM, MMU off or
        // identity-on. If this is not a DTB the magic check below rejects it and nothing else is read.
        let magic = u32::from_be(unsafe { (ptr as *const u32).read_volatile() });
        if magic != FDT_MAGIC {
            return None;
        }
        // SAFETY: the magic matched, so the header is present; totalsize is at offset 4.
        let total = u32::from_be(unsafe { ((ptr + 4) as *const u32).read_volatile() });
        // A tree smaller than its own header, or implausibly large, is malformed.
        if !(40..=(8 * 1024 * 1024)).contains(&total) {
            return None;
        }
        Some(Fdt { base: ptr, total })
    }

    /// Read a big-endian u32 at a byte offset, refusing anything outside the blob.
    fn u32_at(&self, off: u32) -> Option<u32> {
        if off.checked_add(4)? > self.total {
            return None;
        }
        // SAFETY: `off + 4 <= total`, so this read is inside the blob the header describes.
        Some(u32::from_be(unsafe {
            ((self.base + off as u64) as *const u32).read_volatile()
        }))
    }

    /// Read one byte at a byte offset, refusing anything outside the blob.
    fn u8_at(&self, off: u32) -> Option<u8> {
        if off >= self.total {
            return None;
        }
        // SAFETY: `off < total`, so this read is inside the blob.
        Some(unsafe { ((self.base + off as u64) as *const u8).read_volatile() })
    }

    /// Compare a NUL-terminated string in the blob against `want`, without copying it out.
    fn str_eq_at(&self, off: u32, want: &[u8]) -> bool {
        for (i, w) in want.iter().enumerate() {
            match self.u8_at(off + i as u32) {
                Some(c) if c == *w => {}
                _ => return false,
            }
        }
        matches!(self.u8_at(off + want.len() as u32), Some(0))
    }

    /// Does the NUL-terminated string at `off` start with `want`?
    fn str_starts_at(&self, off: u32, want: &[u8]) -> bool {
        want.iter()
            .enumerate()
            .all(|(i, w)| self.u8_at(off + i as u32) == Some(*w))
    }

    /// Byte offset one past the NUL of the string at `off`, 4-byte aligned (token stream rule).
    fn skip_str(&self, off: u32) -> Option<u32> {
        let mut o = off;
        loop {
            match self.u8_at(o) {
                Some(0) => return Some((o + 1 + 3) & !3),
                Some(_) => o += 1,
                None => return None,
            }
        }
    }
}

/// Walk the tree and collect the `/memory` banks. Returns how many were recorded.
///
/// Root `#address-cells` / `#size-cells` are **read, not assumed** - they decide how wide each field in
/// a `reg` property is, and guessing them mis-parses every address on a board that differs.
fn collect_banks(fdt: &Fdt, banks: &mut [Bank; MAX_BANKS]) -> usize {
    let (Some(struct_off), Some(strings_off)) = (fdt.u32_at(8), fdt.u32_at(12)) else {
        return 0;
    };

    let mut addr_cells: u32 = 2; // spec default
    let mut size_cells: u32 = 1; // spec default
    let mut depth: i32 = 0;
    let mut in_memory = false;
    let mut found = 0usize;

    let mut off = struct_off;
    for _ in 0..MAX_TOKENS {
        let Some(token) = fdt.u32_at(off) else { return found };
        off += 4;

        match token {
            FDT_NOP => {}
            FDT_END => return found,
            FDT_BEGIN_NODE => {
                depth += 1;
                // A memory bank is a child of the root named `memory` or `memory@<addr>`.
                in_memory = depth == 2
                    && (fdt.str_eq_at(off, b"memory") || fdt.str_starts_at(off, b"memory@"));
                let Some(next) = fdt.skip_str(off) else { return found };
                off = next;
            }
            FDT_END_NODE => {
                depth -= 1;
                in_memory = false;
                if depth < 0 {
                    return found; // unbalanced tree
                }
            }
            FDT_PROP => {
                let (Some(len), Some(nameoff)) = (fdt.u32_at(off), fdt.u32_at(off + 4)) else {
                    return found;
                };
                let value = off + 8;
                let name = strings_off + nameoff;

                if depth == 1 {
                    // Root cell counts govern every `reg` below.
                    if fdt.str_eq_at(name, b"#address-cells") {
                        if let Some(v) = fdt.u32_at(value) {
                            addr_cells = v;
                        }
                    } else if fdt.str_eq_at(name, b"#size-cells") {
                        if let Some(v) = fdt.u32_at(value) {
                            size_cells = v;
                        }
                    }
                } else if in_memory && fdt.str_eq_at(name, b"reg") {
                    // Only 1- and 2-cell fields exist in practice; anything else we refuse rather
                    // than truncate, because a truncated address is a plausible-looking lie.
                    if (1..=2).contains(&addr_cells) && (1..=2).contains(&size_cells) {
                        let pair = (addr_cells + size_cells) * 4;
                        let mut c = value;
                        while c + pair <= value + len && found < MAX_BANKS {
                            let (Some(base), Some(size)) =
                                (read_cells(fdt, c, addr_cells), read_cells(fdt, c + addr_cells * 4, size_cells))
                            else {
                                break;
                            };
                            if size > 0 {
                                banks[found] = Bank { base, len: size };
                                found += 1;
                            }
                            c += pair;
                        }
                    }
                }

                // Property values are padded to a 4-byte boundary.
                let Some(next) = value.checked_add((len + 3) & !3) else { return found };
                off = next;
            }
            _ => return found, // an unknown token means we have lost the stream; stop rather than guess
        }
    }
    put_str(b"memmap: WARN device tree walk hit its token bound - treating as malformed\r\n");
    found
}

/// Read a 1- or 2-cell big-endian value.
fn read_cells(fdt: &Fdt, off: u32, cells: u32) -> Option<u64> {
    match cells {
        1 => Some(fdt.u32_at(off)? as u64),
        2 => Some(((fdt.u32_at(off)? as u64) << 32) | fdt.u32_at(off + 4)? as u64),
        _ => None,
    }
}

/// The assembled map, stored in `.bss` so it can be handed out as `&'static [MemoryRegion]`.
static mut REGIONS: [MemoryRegion; MAX_REGIONS] = [MemoryRegion {
    base: 0,
    len: 0,
    kind: MemoryKind::Reserved,
}; MAX_REGIONS];
static mut REGION_COUNT: usize = 0;

/// Where the map came from. Reported, because "1 GiB" means something different depending on whether
/// it is the machine's RAM or merely all the fallback could describe.
#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    DeviceTree,
    Mailbox,
    None,
}

/// Build the physical memory map. Call once, early, before the allocator exists.
pub fn build(dtb: u64, board: &BoardInfo) -> (&'static [MemoryRegion], Source) {
    let mut banks = [Bank::default(); MAX_BANKS];
    let mut n = 0usize;
    let mut source = Source::None;

    if let Some(fdt) = Fdt::open(dtb) {
        n = collect_banks(&fdt, &mut banks);
        if n > 0 {
            source = Source::DeviceTree;
        }
    }

    if n == 0 {
        if let Some((base, size)) = board.arm_memory {
            banks[0] = Bank { base: base as u64, len: size as u64 };
            n = 1;
            source = Source::Mailbox;
        }
    }

    // Force the over-4-GiB case. QEMU's `raspi4b` is fixed at 2 GiB, so the clamp below cannot be
    // reached by configuration, and a guard never seen firing is not evidence that it fires.
    #[cfg(feature = "memmap-clamp-test")]
    {
        put_str(b"memmap: CLAMP TEST - injecting synthetic banks past the identity-map limit\r\n");
        banks[0] = Bank { base: 0, len: 8 * 1024 * 1024 * 1024 }; // straddles the limit: truncated
        banks[1] = Bank { base: 6 * 1024 * 1024 * 1024, len: 1024 * 1024 * 1024 }; // wholly above: dropped
        n = 2;
        source = Source::DeviceTree;
    }

    let kstart = KERNEL_PHYS_START;
    let kend = (kernel_phys_end() + 0xFFF) & !0xFFF; // round up: no usable page may straddle the image

    // SAFETY: single-threaded boot; these statics are this module's and nothing else reads them until
    // `build` returns the slice.
    let count = unsafe {
        let regions = &raw mut REGIONS;
        let mut w = 0usize;
        let mut push = |base: u64, len: u64, kind: MemoryKind| {
            if len > 0 && w < MAX_REGIONS {
                (*regions)[w] = MemoryRegion { base, len, kind };
                w += 1;
            }
        };

        // The first page is never usable. Physical 0 is a legitimate frame on this board (RAM starts
        // at 0 and the firmware leaves the low 512 KiB alone), and the allocator will hand it out - it
        // did, as the root of the first task page table, which then printed `TTBR0=0x0`.
        //
        // That works, and works by coincidence. `0` is a sentinel in several places (`switch_context`
        // treats `cr3 == 0` as "no address space to install"), and a null pointer must never be a valid
        // allocation. Reserving the page costs 4 KiB and removes a whole class of ambiguity.
        push(0, PAGE_SIZE, MemoryKind::Reserved);

        for b in banks.iter().take(n) {
            // Clamp to what the identity map actually reaches. An 8 GiB Pi 4 reports banks above
            // 4 GiB, and recording them as usable would hand the allocator RAM with no translation -
            // a fault much later, blamed on whatever touched it. Dropping capacity is the safe
            // direction, and the drop is reported rather than silently taken (invariant 12).
            let (lo, hi) = (b.base.max(PAGE_SIZE), (b.base + b.len).min(super::mmu::IDENTITY_MAP_LIMIT));
            if lo >= hi {
                put_str(b"memmap: NOTE bank at ");
                put_hex(b.base);
                put_str(b" is entirely above the identity map - dropped\r\n");
                continue;
            }
            if hi < b.base + b.len {
                put_str(b"memmap: NOTE bank at ");
                put_hex(b.base);
                put_str(b" truncated at the identity-map limit, ");
                put_dec((b.base + b.len - hi) / (1024 * 1024));
                put_str(b" MiB not used\r\n");
            }
            let b = Bank { base: lo, len: hi - lo };
            // Split each bank around the kernel image rather than trusting it to sit at the start.
            if kend <= lo || kstart >= hi {
                push(lo, b.len, MemoryKind::Usable);
            } else {
                push(lo, kstart.saturating_sub(lo), MemoryKind::Usable);
                push(kstart.max(lo), kend.min(hi) - kstart.max(lo), MemoryKind::KernelImage);
                push(kend, hi.saturating_sub(kend), MemoryKind::Usable);
            }
        }
        REGION_COUNT = w;
        w
    };

    // SAFETY: `REGIONS` lives for the program's lifetime and `count` entries were just written. The
    // slice is handed out read-only and never rebuilt.
    let map = unsafe { core::slice::from_raw_parts((&raw const REGIONS) as *const MemoryRegion, count) };
    (map, source)
}

/// The map built by [`build`], re-reachable after the boot abandons its low stack.
///
/// The jump into the high half discards every local, so the continuation cannot be handed the slice as
/// an argument. `REGIONS` is a static and the kernel is linked high, so its address is a high one that
/// translates on both sides of the jump - which is why re-deriving is sound rather than a workaround.
pub fn current_map() -> &'static [MemoryRegion] {
    // SAFETY: `build` has run and wrote `REGION_COUNT` entries; nothing rebuilds the map afterwards, so
    // the slice is immutable for the rest of the boot.
    unsafe {
        core::slice::from_raw_parts((&raw const REGIONS) as *const MemoryRegion, REGION_COUNT)
    }
}

/// Assemble the arch `BootInfo` the neutral kernel consumes.
///
/// `hhdm_offset` is the kernel's high-half base. Once the kernel moved to `TTBR1`, physical memory is
/// reachable only through that direct map - a physical address is not a pointer any more, because
/// `TTBR0` belongs to whatever task is running. This is exactly the role Limine's HHDM plays on x86,
/// so the neutral allocator's existing handling applies unchanged and `PHYS_IS_IDENTITY` is `false`
/// again, as it is there.
///
/// `rsdp_addr = 0` because there is no ACPI on this board. The IOMMU path that consumes it is x86-only
/// (AMD-Vi via IVRS), and the Pi 4 has no usable SMMU regardless - see `docs/aarch64.md` on why the
/// §6.4 posture does not travel here.
pub fn boot_info(map: &'static [MemoryRegion]) -> super::BootInfo {
    super::BootInfo {
        memory_map: map,
        kernel_phys_start: KERNEL_PHYS_START,
        kernel_phys_end: (kernel_phys_end() + 0xFFF) & !0xFFF,
        hhdm_offset: super::mmu::KERNEL_VA_BASE,
        rsdp_addr: 0,
    }
}

/// Report the map, with enough detail that a reader can check it against the board they are holding.
pub fn report(map: &[MemoryRegion], source: Source) {
    match source {
        Source::DeviceTree => put_str(b"memmap: source = device tree (authoritative)\r\n"),
        Source::Mailbox => put_str(
            b"memmap: source = mailbox FALLBACK - 32-bit tag, may under-report a >1 GiB board\r\n",
        ),
        Source::None => {
            put_str(b"memmap: WARN no memory map from either source - RAM size unknown\r\n");
            return;
        }
    }

    let mut usable = 0u64;
    for r in map {
        put_str(b"memmap:   ");
        put_hex(r.base);
        put_str(b"..");
        put_hex(r.base + r.len);
        put_str(match r.kind {
            MemoryKind::Usable => b"  usable       " as &[u8],
            MemoryKind::KernelImage => b"  kernel image " as &[u8],
            _ => b"  reserved     " as &[u8],
        });
        put_dec(r.len / 1024);
        put_str(b" KiB\r\n");
        if matches!(r.kind, MemoryKind::Usable) {
            usable += r.len;
        }
    }
    put_str(b"memmap: usable RAM ");
    put_dec(usable / (1024 * 1024));
    put_str(b" MiB across ");
    put_dec(map.len() as u64);
    put_str(b" regions\r\n");
}
