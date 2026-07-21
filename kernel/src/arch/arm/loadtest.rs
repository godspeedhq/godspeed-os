// SPDX-License-Identifier: GPL-2.0-only
//! Loader selftest - proving the neutral ELF loader parses and maps a real 32-bit ARM service.
//!
//! Increment 5 wired the neutral `loader.rs` to parse ELF32 (as well as ELF64) and check the machine
//! against an arch constant, and `build.rs` now embeds the ARM-built `logger` ELF. This proves the two
//! meet: the embedded logger is handed to `loader::load`, which parses its ELF32 headers, allocates
//! frames from the (live) neutral allocator, maps its segments into a fresh page table, and returns
//! the entry VA - the exact path the spawner will drive in increment 6, minus running it.

use super::pl011_write;
use super::exceptions::write_hex32;
use super::timer::write_dec_pub;

/// The ARM `logger` service, embedded by `build.rs` (`SVC_LOGGER_ELF`). On a not-yet-ported arch this
/// is the empty placeholder; on ARM with the logger built it is the real 32-bit ARM EXEC ELF.
static LOGGER_ELF: &[u8] = include_bytes!(env!("SVC_LOGGER_ELF"));

/// Load the embedded ARM service ELF and confirm the loader parsed and mapped it.
///
/// The checks are the ones that distinguish "parsed a real ELF" from "returned something": the entry
/// VA must be the `user.ld` base (`0x400000`), and a non-trivial number of bytes must have been
/// mapped (a service is several pages of text/rodata/data/bss). A parser that silently accepted
/// garbage, or mis-read the ELF32 field offsets, would fail one of these.
pub fn selftest() {
    if LOGGER_ELF.len() < 64 {
        pl011_write(b"arm32: loader selftest SKIP - no ARM logger ELF embedded (placeholder)\r\n");
        return;
    }

    match crate::loader::load(LOGGER_ELF) {
        Ok(loaded) => {
            pl011_write(b"arm32: loader - parsed ARM ELF (");
            write_dec_pub(LOGGER_ELF.len() as u32);
            pl011_write(b" bytes), entry ");
            write_hex32(loaded.entry_va as u32);
            pl011_write(b", mapped ");
            write_dec_pub((loaded.mapped_bytes / 1024) as u32);
            pl011_write(b" KiB into a fresh page table\r\n");

            let entry_ok = loaded.entry_va == 0x0040_0000;
            let mapped_ok = loaded.mapped_bytes >= 4096;
            if entry_ok && mapped_ok {
                pl011_write(b"arm32: loader PASS (neutral loader maps a real 32-bit ARM service)\r\n");
            } else {
                if !entry_ok { pl011_write(b"arm32:   entry VA is not the user.ld base 0x400000\r\n"); }
                if !mapped_ok { pl011_write(b"arm32:   suspiciously little mapped for a service\r\n"); }
                pl011_write(b"arm32: loader FAIL - see above\r\n");
            }
        }
        Err(e) => {
            use crate::loader::LoadError::*;
            let reason: &[u8] = match e {
                TooSmall => b"TooSmall", BadMagic => b"BadMagic", NotElf64 => b"wrong class/data",
                WrongArch => b"WrongArch (e_machine)", NotExecutable => b"NotExecutable",
                BadProgramHeader => b"BadProgramHeader", SegmentOutOfBounds => b"SegmentOutOfBounds",
                MapFailed(_) => b"MapFailed", FrameAllocFailed => b"FrameAllocFailed",
            };
            pl011_write(b"arm32: loader FAIL - load rejected the ARM ELF: ");
            pl011_write(reason);
            pl011_write(b"\r\n");
        }
    }
}
