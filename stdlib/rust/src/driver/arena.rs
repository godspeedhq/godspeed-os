// SPDX-License-Identifier: Apache-2.0
//! A driver's ARENA: the memory its device reads and writes, granted at spawn, and laid out by name.
//!
//! # Why this exists
//!
//! A device that does DMA is given an arena: physically contiguous memory the kernel mapped into the
//! driver, whose physical addresses the driver programs into the device (CLAUDE.md 12.3, 6.4). Every
//! driver reached past the standard library for it, with the SDK's `ctx.dma_region()`, and then laid it out
//! by hand (AHCI and GENET were the first to come through `gs`, 2026-10-19; the rest still do,
//! `docs/driver-library.md` step 3): seven drivers placed their rings, buffers and tables at offset constants
//! of their own, 33 of them, and two asserts between them checked that nothing overlapped. The day this
//! was written, the RTL8168's 24 receive buffers filled its arena to the last 4 KiB, and one hand-written
//! assert was all that kept its counters from landing inside a buffer.
//!
//! So the arena comes through `gs` (`docs/driver-library.md`, step 3), with the one mechanism every one
//! of those drivers wrote out: **parts, taken by name, size and alignment, that cannot overlap.**
//!
//! # The shape
//!
//! ```ignore
//! let Some(arena) = gs::driver::arena(&ctx) else { /* say so and park */ };
//! let mut parts = arena.layout();
//! let rx_ring = parts.take("rx ring", 24 * 16, 256)?;
//! let rx_bufs = parts.take("rx buffers", 24 * 2048, 8)?;
//! let tally = parts.take("tally", 64, 64)?;
//!
//! rx_ring.write32(i * 16, own | len);      // offsets are within the part
//! let buf = rx_bufs.phys_at(i * 2048);     // the address the device is given
//! ```
//!
//! - **A part that does not fit is refused, with its name** ([`NoRoom`]), before anything is written:
//!   ``the arena (64 KiB) has no room for `tally` (64 bytes, aligned to 64, after 64 KiB)``.
//! - **Alignment is of the PHYSICAL address**, because that is what the device checks. A ring a device
//!   requires on a 256-byte boundary is on one, whatever the arena's base.
//! - **Every access is bounded to its part.** An offset past the part ends the driver loudly, naming the
//!   part, as an offset past the arena always did (SEC-4) - so a ring's index running off its end is
//!   caught at the ring, not when it reaches someone else's buffer.
//! - **Laying out takes the arena**: once laid out it is reached only through its parts. A driver that
//!   wants the whole arena, one buffer and nothing else, uses the [`Arena`] directly and needs no layout.
//!
//! What a part HOLDS - a descriptor's fields, a ring's ownership bits - is the device's, and stays in its
//! driver. What is here is only where things are, which is the same for every device.
//!
//! # Coherency
//!
//! None is done here, as none is done beneath (the SDK's `Dma`, SEC-28): each port maps the arena so the
//! CPU and the device agree - uncached where DMA is not coherent. A part is a view of that mapping.

use core::fmt;

/// Why a part was refused. Its `Display` is a sentence a driver can log as it stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom {
    /// The part asked for.
    pub name: &'static str,
    /// Its size in bytes.
    pub len: usize,
    /// The alignment it asked for.
    pub align: usize,
    /// How much of the arena the parts before it had taken.
    pub used: usize,
    /// The arena's size.
    pub arena: usize,
    /// What was wrong.
    pub why: Why,
}

/// What made a part impossible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// It does not fit in what is left.
    Full,
    /// Its alignment is not a power of two.
    Alignment,
    /// It asked for no bytes, which is a mistake in the driver rather than a part.
    Empty,
}

impl fmt::Display for NoRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.why {
            Why::Full => write!(
                f,
                "the arena ({}) has no room for `{}` ({} bytes, aligned to {}, after {})",
                Size(self.arena), self.name, self.len, self.align, Size(self.used)
            ),
            Why::Alignment => write!(
                f,
                "`{}` asks for an alignment of {}, which is not a power of two",
                self.name, self.align
            ),
            Why::Empty => write!(f, "`{}` asks for no bytes", self.name),
        }
    }
}

/// A size for a sentence: bytes below 1 KiB, KiB to one decimal above.
struct Size(usize);

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 1024 {
            return write!(f, "{} bytes", self.0);
        }
        let tenths = self.0 as u64 * 10 / 1024;
        if tenths % 10 == 0 {
            write!(f, "{} KiB", tenths / 10)
        } else {
            write!(f, "{}.{} KiB", tenths / 10, tenths % 10)
        }
    }
}

/// Where a part of `len` bytes, aligned to `align`, goes in an arena of `arena` bytes whose physical base
/// is `phys` and whose first `used` bytes are taken: its offset, or why it cannot go anywhere.
///
/// Pure, so it is pinned by host tests; [`Layout::take`] is this and nothing more.
pub fn place(phys: u64, arena: usize, used: usize, name: &'static str, len: usize, align: usize)
    -> Result<usize, NoRoom> {
    let refuse = |why| Err(NoRoom { name, len, align, used, arena, why });
    if len == 0 {
        return refuse(Why::Empty);
    }
    if align == 0 || !align.is_power_of_two() {
        return refuse(Why::Alignment);
    }
    // The first offset at or after `used` whose PHYSICAL address is a multiple of `align`.
    let mask = align as u64 - 1;
    let at = match phys.checked_add(used as u64) {
        Some(p) => p,
        None => return refuse(Why::Full),
    };
    let gap = ((align as u64 - (at & mask)) & mask) as usize;
    match used.checked_add(gap).and_then(|start| start.checked_add(len).map(|end| (start, end))) {
        // Within the arena, and with a physical END the address space holds: a part whose address
        // wrapped past zero would send the device somewhere else entirely.
        Some((start, end)) if end <= arena && phys.checked_add(end as u64).is_some() => Ok(start),
        _ => refuse(Why::Full),
    }
}

#[cfg(not(test))]
pub use granted::*;

#[cfg(not(test))]
mod granted {
    use super::{place, NoRoom};
    use godspeed_sdk::dma::Dma;
    use godspeed_sdk::service_context::ServiceContext;

    /// The DMA arena the kernel granted this driver. Not `Copy`: [`Arena::layout`] takes it, so a laid-out
    /// arena is reached only through its parts.
    pub struct Arena {
        dma: Dma,
    }

    /// This driver's arena, or `None` when the spawn granted none.
    pub fn arena(ctx: &ServiceContext) -> Option<Arena> {
        ctx.dma_region().map(|dma| Arena { dma })
    }

    impl Arena {
        /// The arena's size in bytes.
        pub fn len(&self) -> usize {
            self.dma.len()
        }

        /// The physical address of byte `off`: what the device is given.
        pub fn phys_at(&self, off: usize) -> u64 {
            self.dma.phys_at(off)
        }

        /// Zero the whole arena, with aligned volatile stores (never a `memset`, which may fault on the
        /// Device memory a non-coherent port maps it as).
        pub fn zero(&self) {
            self.dma.zero()
        }

        /// Lay the arena out in named parts, from its start. The arena is taken: from here on it is
        /// reached through the parts.
        pub fn layout(self) -> Layout {
            Layout { dma: self.dma, used: 0 }
        }

        /// Read 8 bits at `off`.
        pub fn read8(&self, off: usize) -> u8 { self.dma.read8(off) }
        /// Read 16 bits at `off`.
        pub fn read16(&self, off: usize) -> u16 { self.dma.read16(off) }
        /// Read 32 bits at `off`.
        pub fn read32(&self, off: usize) -> u32 { self.dma.read32(off) }
        /// Read 64 bits at `off`.
        pub fn read64(&self, off: usize) -> u64 { self.dma.read64(off) }
        /// Write 8 bits at `off`.
        pub fn write8(&self, off: usize, val: u8) { self.dma.write8(off, val) }
        /// Write 16 bits at `off`.
        pub fn write16(&self, off: usize, val: u16) { self.dma.write16(off, val) }
        /// Write 32 bits at `off`.
        pub fn write32(&self, off: usize, val: u32) { self.dma.write32(off, val) }
        /// Write 64 bits at `off`.
        pub fn write64(&self, off: usize, val: u64) { self.dma.write64(off, val) }
    }

    /// An arena being laid out: parts are taken in order, each after the last, and never overlap.
    pub struct Layout {
        dma: Dma,
        used: usize,
    }

    impl Layout {
        /// Take the next part: `len` bytes, its PHYSICAL address a multiple of `align` (a power of two).
        /// Refused, with the reason, when it does not fit in what is left, asks for no bytes, or names an
        /// alignment that is not a power of two.
        pub fn take(&mut self, name: &'static str, len: usize, align: usize) -> Result<Part, NoRoom> {
            let off = place(self.dma.phys_base(), self.dma.len(), self.used, name, len, align)?;
            self.used = off + len;
            Ok(Part { dma: self.dma, name, off, len })
        }

        /// Bytes taken so far, alignment gaps included.
        pub fn used(&self) -> usize {
            self.used
        }

        /// The arena's size.
        pub fn len(&self) -> usize {
            self.dma.len()
        }
    }

    /// One named part of the arena. Offsets are within the part, and an access past it ends the driver
    /// naming the part. Copies are views of the same bytes.
    #[derive(Clone, Copy)]
    pub struct Part {
        dma: Dma,
        name: &'static str,
        off: usize,
        len: usize,
    }

    impl Part {
        /// The name it was taken with.
        pub fn name(&self) -> &'static str { self.name }
        /// Its size in bytes.
        pub fn len(&self) -> usize { self.len }
        /// Where it starts in the arena, for a log line.
        pub fn offset(&self) -> usize { self.off }

        /// The physical address of its first byte.
        pub fn phys(&self) -> u64 { self.phys_at(0) }

        /// The physical address of byte `off` within it: what the device is given.
        pub fn phys_at(&self, off: usize) -> u64 {
            self.check(off, 0);
            self.dma.phys_at(self.off + off)
        }

        /// Zero the part, with aligned volatile stores where it is aligned and bytes where it is not.
        pub fn zero(&self) {
            let mut i = 0;
            while i < self.len {
                if (self.off + i) % 8 == 0 && i + 8 <= self.len {
                    self.dma.write64(self.off + i, 0);
                    i += 8;
                } else {
                    self.dma.write8(self.off + i, 0);
                    i += 1;
                }
            }
        }

        fn check(&self, off: usize, size: usize) {
            if !off.checked_add(size).map_or(false, |end| end <= self.len) {
                panic!("arena part `{}`: {} bytes at {} is past its {} bytes", self.name, size, off, self.len);
            }
        }

        /// Read 8 bits at `off` within the part.
        pub fn read8(&self, off: usize) -> u8 { self.check(off, 1); self.dma.read8(self.off + off) }
        /// Read 16 bits at `off` within the part.
        pub fn read16(&self, off: usize) -> u16 { self.check(off, 2); self.dma.read16(self.off + off) }
        /// Read 32 bits at `off` within the part.
        pub fn read32(&self, off: usize) -> u32 { self.check(off, 4); self.dma.read32(self.off + off) }
        /// Read 64 bits at `off` within the part.
        pub fn read64(&self, off: usize) -> u64 { self.check(off, 8); self.dma.read64(self.off + off) }
        /// Write 8 bits at `off` within the part.
        pub fn write8(&self, off: usize, val: u8) { self.check(off, 1); self.dma.write8(self.off + off, val) }
        /// Write 16 bits at `off` within the part.
        pub fn write16(&self, off: usize, val: u16) { self.check(off, 2); self.dma.write16(self.off + off, val) }
        /// Write 32 bits at `off` within the part.
        pub fn write32(&self, off: usize, val: u32) { self.check(off, 4); self.dma.write32(self.off + off, val) }
        /// Write 64 bits at `off` within the part.
        pub fn write64(&self, off: usize, val: u64) { self.check(off, 8); self.dma.write64(self.off + off, val) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARENA: usize = 64 * 1024;
    const PAGE: u64 = 0x4361000;

    #[test]
    fn parts_follow_one_another() {
        assert_eq!(place(PAGE, ARENA, 0, "ring", 384, 256), Ok(0));
        assert_eq!(place(PAGE, ARENA, 384, "bufs", 2048, 8), Ok(384));
    }

    #[test]
    fn alignment_is_of_the_physical_address() {
        // 384 rounds up to 512 for a 256-byte boundary on a page-aligned arena...
        assert_eq!(place(PAGE, ARENA, 384, "ring", 16, 256), Ok(512));
        // ...and an arena whose base is only 64-aligned is padded to the PHYSICAL boundary, not to its offset.
        assert_eq!(place(0x1040, ARENA, 0, "ring", 16, 256), Ok(0xC0));
    }

    #[test]
    fn the_rtl8168_layout_fits_exactly_and_one_more_byte_does_not() {
        // TX ring, TX buffer, RX ring, 24 x 2 KiB RX buffers, the tally: the RTL8168's layout when its ring was
        // 24 (it is 256 now, in 132 pages); kept as a layout that fills its arena exactly.
        let mut used = 0;
        for (name, len, align) in [("tx ring", 128, 256), ("tx buffer", 2048, 4096), ("rx ring", 384, 4096),
                                   ("rx buffers", 24 * 2048, 4096), ("tally", 64, 64)] {
            let off = place(PAGE, ARENA, used, name, len, align).expect(name);
            used = off + len;
        }
        assert_eq!(used, 0xF040);
        let r = place(PAGE, ARENA, ARENA - 63, "one more", 64, 1);
        assert_eq!(r.map_err(|e| e.why), Err(Why::Full));
    }

    #[test]
    fn a_part_that_does_not_fit_is_named() {
        let e = place(PAGE, ARENA, ARENA, "tally", 64, 64).unwrap_err();
        assert_eq!(e.why, Why::Full);
        let mut s = String::new();
        use core::fmt::Write;
        write!(s, "{}", e).unwrap();
        assert_eq!(s, "the arena (64 KiB) has no room for `tally` (64 bytes, aligned to 64, after 64 KiB)");
    }

    #[test]
    fn nonsense_is_refused_as_nonsense() {
        assert_eq!(place(PAGE, ARENA, 0, "x", 16, 3).map_err(|e| e.why), Err(Why::Alignment));
        assert_eq!(place(PAGE, ARENA, 0, "x", 16, 0).map_err(|e| e.why), Err(Why::Alignment));
        assert_eq!(place(PAGE, ARENA, 0, "x", 0, 8).map_err(|e| e.why), Err(Why::Empty));
    }

    #[test]
    fn no_arithmetic_wraps() {
        assert_eq!(place(PAGE, ARENA, usize::MAX - 2, "x", 16, 8).map_err(|e| e.why), Err(Why::Full));
        assert_eq!(place(u64::MAX - 4, ARENA, 0, "x", 16, 8).map_err(|e| e.why), Err(Why::Full));
        assert_eq!(place(PAGE, ARENA, 0, "x", usize::MAX, 1).map_err(|e| e.why), Err(Why::Full));
    }

    #[test]
    fn sizes_read_as_people_say_them() {
        let s = |n| { let mut t = String::new(); use core::fmt::Write; write!(t, "{}", Size(n)).unwrap(); t };
        assert_eq!(s(64), "64 bytes");
        assert_eq!(s(64 * 1024), "64 KiB");
        assert_eq!(s(0xF000 + 900), "60.8 KiB");
    }
}
