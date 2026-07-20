// SPDX-License-Identifier: GPL-2.0-only
//! Flattened Device Tree (FDT) parsing - learning the machine's real memory map.
//!
//! Every layer so far could get away with a constant. `mmu.rs` hardcodes `RAM_END = 0x3B40_0000`,
//! copied from what the firmware told Linux, with a comment admitting that is not how a real port
//! should learn it. That was fine while nothing depended on it; it stops being fine the moment the
//! neutral kernel's frame allocator does, because a wrong constant there hands out frames backed by
//! memory that does not exist.
//!
//! The firmware already knows the answer and passes it: `r2` at entry holds a **device tree blob**,
//! captured by `_start` into `DTB_PTR`. This module reads the memory node out of it.
//!
//! **FDT is big-endian, on a little-endian CPU.** Every u32 in the blob needs byte-swapping. That is
//! the single most common way to get nonsense out of this format, so every read goes through `be32`
//! rather than being done by hand at each site.
//!
//! The parser is deliberately minimal: walk the structure block, find `/memory`, read its `reg`
//! property. No property lookup by path, no `#address-cells` generality beyond what a check confirms,
//! no allocation. It answers one question, and a bad answer is reported rather than guessed at.

use super::pl011_write;
use super::exceptions::write_hex32;
use super::timer::write_dec_pub;

const FDT_MAGIC: u32 = 0xd00d_feed;

// Structure-block tokens.
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// A memory range the firmware says is real RAM.
#[derive(Clone, Copy)]
pub struct MemRange {
    pub base: u32,
    pub size: u32,
}

/// Read a big-endian u32 from the blob.
fn be32(p: usize) -> u32 {
    // SAFETY: Callers only pass offsets already bounds-checked against the blob's `totalsize`, and
    // the DTB sits in RAM identity-mapped by `mmu.rs`. An unaligned or out-of-range blob is handled
    // by the checks in `memory_range`, not here.
    let v = unsafe { (p as *const u32).read_volatile() };
    v.swap_bytes()
}

/// Does `name` at `p` match `want` (NUL-terminated, and also matching `want@...` unit addresses)?
///
/// Device tree node names carry a unit address - the memory node is `memory@0` on this board and
/// plain `memory` on others - so a bare `==` comparison would miss depending on firmware version.
fn name_matches(p: usize, want: &[u8]) -> bool {
    for (i, &w) in want.iter().enumerate() {
        // SAFETY: The caller guarantees `p` is inside the blob; the loop stops at the first mismatch
        // or NUL, so it cannot run past the string it is comparing.
        let c = unsafe { *((p + i) as *const u8) };
        if c != w {
            return false;
        }
    }
    // SAFETY: As above - one byte past the matched prefix, still within the blob.
    let after = unsafe { *((p + want.len()) as *const u8) };
    after == 0 || after == b'@'
}

/// Find the first `/memory` node's `reg` property and return the range it describes.
///
/// Returns `None` if there is no blob, the magic is wrong, or no memory node is found - each of which
/// the caller reports rather than silently substituting a guess.
pub fn memory_range() -> Option<MemRange> {
    // SAFETY: Reading a `static mut` written once in `_start` before any other core or task exists.
    let dtb = unsafe { core::ptr::addr_of!(super::DTB_PTR).read_volatile() } as usize;
    if dtb == 0 || dtb & 3 != 0 {
        return None; // no blob, or not 4-byte aligned as the spec requires
    }

    if be32(dtb) != FDT_MAGIC {
        return None;
    }

    let totalsize = be32(dtb + 4) as usize;
    let off_struct = be32(dtb + 8) as usize;
    let size_struct = be32(dtb + 36) as usize;

    // Bound everything by the blob's own declared size before walking it. A corrupt header pointing
    // outside the blob is exactly the case where a parser wanders into unmapped memory.
    if totalsize < 40 || off_struct + size_struct > totalsize {
        return None;
    }

    let struct_start = dtb + off_struct;
    let struct_end = struct_start + size_struct;

    let mut p = struct_start;
    let mut in_memory_node = false;

    while p + 4 <= struct_end {
        let token = be32(p);
        p += 4;

        match token {
            FDT_BEGIN_NODE => {
                in_memory_node = name_matches(p, b"memory");
                // Skip the NUL-terminated name, then re-align to 4 bytes.
                let mut q = p;
                // SAFETY: Bounded by struct_end; the blob's strings are NUL-terminated by the spec.
                while q < struct_end && unsafe { *(q as *const u8) } != 0 {
                    q += 1;
                }
                p = (q + 1 + 3) & !3;
            }
            FDT_END_NODE => {
                in_memory_node = false;
            }
            FDT_PROP => {
                if p + 8 > struct_end {
                    return None;
                }
                let len = be32(p) as usize;
                let nameoff = be32(p + 4) as usize;
                let data = p + 8;

                // The property name lives in the strings block, indexed by nameoff.
                let off_strings = be32(dtb + 12) as usize;
                let name_ptr = dtb + off_strings + nameoff;

                if in_memory_node && name_matches(name_ptr, b"reg") && len >= 8 {
                    // `reg` is <address size> pairs. The Pi 2 uses one address cell and one size
                    // cell (32-bit each), which is what a 1 GiB 32-bit board wants; anything else
                    // would need #address-cells/#size-cells handling this parser deliberately does
                    // not pretend to have.
                    return Some(MemRange { base: be32(data), size: be32(data + 4) });
                }

                p = data + ((len + 3) & !3);
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => return None, // unknown token: the blob is not what we think it is
        }
    }

    None
}

/// Report the discovered memory map, or say clearly that we are falling back.
///
/// Returns the usable RAM end. A missing or unparsable DTB is **not** silently papered over: the
/// fallback is announced, because a wrong memory size becomes frame-allocator corruption later, far
/// from the cause (invariant 12).
pub fn report_memory(fallback_end: u32) -> u32 {
    match memory_range() {
        Some(m) => {
            let end = m.base.wrapping_add(m.size);
            pl011_write(b"arm32: DTB memory node - base ");
            write_hex32(m.base);
            pl011_write(b", size ");
            write_hex32(m.size);
            pl011_write(b" (");
            write_dec_pub(m.size / (1024 * 1024));
            pl011_write(b" MiB), end ");
            write_hex32(end);
            pl011_write(b"\r\n");
            end
        }
        None => {
            pl011_write(b"arm32: WARNING - no usable device tree (DTB_PTR = ");
            // SAFETY: Reading a `static mut` written once in `_start`.
            write_hex32(unsafe { core::ptr::addr_of!(super::DTB_PTR).read_volatile() });
            pl011_write(b"). Falling back to a\r\n       hardcoded RAM end of ");
            write_hex32(fallback_end);
            pl011_write(b" - correct for a Pi 2, wrong on any other board.\r\n");
            fallback_end
        }
    }
}
