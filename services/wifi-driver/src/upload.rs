// SPDX-License-Identifier: GPL-2.0-only
//! Write the firmware into the chip's RAM and start its processor.
//!
//! ## The shape, and why it is not one transfer
//!
//! The backplane window is **32 KiB**, so 609 KB cannot be written in one go.
//! `brcmf_sdiod_ramrw` chunks by `SBSDIO_SB_OFT_ADDR_LIMIT` and **sets the window once per chunk**, and
//! `cyw43_download_resource` does the same:
//!
//! ```c
//! for (size_t offset = 0; offset < len; offset += block_size) {
//!     size_t sz = block_size;
//!     if (offset + sz > len) sz = len - offset;
//!     uint32_t dest_addr = addr + offset;
//!     cyw43_set_backplane_window(self, dest_addr);
//!     int ret = cyw43_write_bytes(self, BACKPLANE_FUNCTION, dest_addr, sz, src);
//! }
//! ```
//!
//! `Window::set` already caches, so a window write only costs commands when the window actually moves.
//!
//! **And it must be BLOCK mode, for an arithmetic reason.** A byte-mode CMD53 carries at most 512 bytes, and
//! the `write32` this driver had moves four - 609 KB that way is about 152,000 transactions. Block mode
//! moves `blocks x 64` bytes in one command, so a 2 KiB chunk is one transaction and the whole image is
//! about 300. Function 1's 64-byte block size was set in step 2 for exactly this.
//!
//! ## The RAM layout, from two references that agree
//!
//! The firmware goes at `rambase`. The NVRAM goes at the **TOP**:
//!
//! ```c
//! /* brcmfmac */  address = bus->ci->ramsize - varsz + bus->ci->rambase;
//! /* cyw43    */  cyw43_download_resource(self, CYW43_RAM_SIZE - 4 - wifi_nvram_len, ...);
//!                 cyw43_write_backplane(self, CYW43_RAM_SIZE - 4, 4, sz);
//! ```
//!
//! so the last four bytes of RAM are a length token and the NVRAM sits just below it. On this chip that is
//! `0x198000 + 800 KiB = 0x260000`, token at `0x25FFFC`, leaving about 200 KiB clear between the image and
//! the NVRAM.
//!
//! ## The NVRAM is TRANSFORMED, not copied
//!
//! `brcmf_fw_nvram_strip` parses the text file: `#` starts a comment, whitespace is not kept, and each
//! `key=value` entry is terminated by a **NUL** rather than a newline. Then it pads and appends the token:
//!
//! ```c
//! pad = nvp.nvram_len;
//! *new_length = roundup(nvp.nvram_len + 1, 4);
//! while (pad != *new_length) { nvp.nvram[pad] = 0; pad++; }
//! token = *new_length / 4;
//! token = (~token << 16) | (token & 0x0000FFFF);
//! ```
//!
//! Writing the raw `.txt` would put comments and newlines where the firmware expects NUL-separated entries.

use godspeed_sdk::ServiceContext;

use crate::aicore;
use crate::armcr4::Ram;
use crate::backplane::Window;
use crate::firmware;
use crate::host::{blk_block_mode, blk_byte_mode, Host};
use crate::sdio;

/// Function 1's block size, as set in step 2 and as `SDIO_FUNC1_BLOCKSIZE` in brcmfmac.
const BLOCK: usize = 64;

/// How much is moved per CMD53. Sixteen blocks, so the whole image is about 600 transactions rather than
/// 152,000 - and 1 KiB of stack for the word-aligned copy, which is bounded and visible (§26.6.1).
const CHUNK: usize = 16 * BLOCK;

/// The backplane window, from `SBSDIO_SB_OFT_ADDR_MASK` being `0x07FFF`.
const WINDOW: u32 = 0x8000;

/// Write `data` to backplane `addr`, chunked by the window and by `CHUNK`.
///
/// **The source is not word-aligned** - it is `include_bytes!` data - so each chunk is copied into a stack
/// buffer of `u32` first. The FIFO is 32 bits wide and this driver's transfer path takes words; copying a
/// bounded chunk is the honest way to bridge that, rather than casting a `&[u8]` whose alignment nobody
/// promised.
pub fn write_bytes(
    h: &Host,
    w: &mut Window,
    addr: u32,
    data: &[u8],
    what: &str,
    ctx: &ServiceContext,
) -> bool {
    let mut buf = [0u32; CHUNK / 4];
    let mut off = 0usize;
    let mut commands = 0u32;

    while off < data.len() {
        let at = addr + off as u32;
        // Never straddle a window boundary: the window is set for `at`, so a transfer may only run to the
        // end of that 32 KiB region.
        let room = (WINDOW - (at & (WINDOW - 1))) as usize;
        let mut want = core::cmp::min(CHUNK, data.len() - off);
        want = core::cmp::min(want, room);

        // Whole blocks where possible; the tail goes byte mode, which is what `sdio_io_rw_ext_helper` does
        // (block mode for the bulk, byte mode for the remainder).
        let (n, blocks) = if want >= BLOCK {
            let blocks = want / BLOCK;
            (blocks * BLOCK, blocks)
        } else {
            (want, 0)
        };

        // ROUNDED UP TO A WORD, because the FIFO moves 32 bits at a time and the image length is not a
        // multiple of four (609,309 bytes). The up-to-three extra bytes are zeros written just past the
        // image, which is 200 KiB below the NVRAM - stated rather than left for a reader to wonder about.
        let words = (n + 3) / 4;
        for (i, wd) in buf[..words].iter_mut().enumerate() {
            let b = i * 4;
            let mut v = 0u32;
            for k in 0..4 {
                if b + k < n {
                    v |= (data[off + b + k] as u32) << (8 * k);
                }
            }
            *wd = v;
        }

        if !w.set_for(h, at, ctx) {
            return false;
        }
        let win_off = (at & (WINDOW - 1)) | WINDOW; // the wide-access flag, as every backplane access needs
        let ok = if blocks > 0 {
            sdio::write_extended(h, 1, win_off, &mut buf[..words],
                                 blk_block_mode(blocks as u32, BLOCK as u32),
                                 Some(blocks as u32), ctx)
        } else {
            sdio::write_extended(h, 1, win_off, &mut buf[..words],
                                 blk_byte_mode((words * 4) as u32), None, ctx)
        };
        if !ok {
            ctx.log_fmt(format_args!(
                "wifi-driver: {} write failed {} bytes in, at backplane {:#08x} after {} command(s)",
                what, off, at, commands
            ));
            return false;
        }
        commands += 1;
        off += n;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: {} written - {} bytes to {:#08x} in {} command(s)",
        what, data.len(), addr, commands
    ));
    true
}

/// Turn the NVRAM text into what the firmware expects, into `out`. Returns the length written.
///
/// Comments (`#` to end of line) and whitespace are dropped; each surviving entry is terminated by a NUL.
/// Then the whole thing is padded to `roundup(len + 1, 4)` and the token appended, per
/// `brcmf_fw_nvram_strip`.
///
/// `None` if it does not fit `out`, which is a bounded buffer rather than a growing one.
pub fn nvram_prepare(text: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        // Skip whitespace and blank lines.
        while i < text.len() && (text[i] == b' ' || text[i] == b'\t' || text[i] == b'\r' || text[i] == b'\n') {
            i += 1;
        }
        if i >= text.len() {
            break;
        }
        // A comment runs to the end of the line and contributes nothing.
        if text[i] == b'#' {
            while i < text.len() && text[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Copy the entry, stopping at a newline or a comment, dropping trailing whitespace.
        let start = n;
        while i < text.len() && text[i] != b'\n' && text[i] != b'#' {
            let c = text[i];
            // `is_nvram_char`: printable, and `#` already excluded above.
            if c >= 0x20 && c < 0x7f {
                if n >= out.len() {
                    return None;
                }
                out[n] = c;
                n += 1;
            }
            i += 1;
        }
        while n > start && (out[n - 1] == b' ' || out[n - 1] == b'\t') {
            n -= 1;
        }
        // NUL-TERMINATED, not newline-terminated. This is the difference between the file on disk and what
        // the firmware reads, and copying the file verbatim would get it wrong.
        if n > start {
            if n >= out.len() {
                return None;
            }
            out[n] = 0;
            n += 1;
        }
    }
    // Pad to `roundup(len + 1, 4)`, then the token.
    let padded = (n + 1 + 3) & !3;
    if padded + 4 > out.len() {
        return None;
    }
    while n < padded {
        out[n] = 0;
        n += 1;
    }
    let words = (padded / 4) as u32;
    let token = ((!words & 0xFFFF) << 16) | (words & 0xFFFF);
    out[n..n + 4].copy_from_slice(&token.to_le_bytes());
    Some(padded + 4)
}

/// Halt the ARM, write the firmware and the NVRAM, and let it run.
pub fn run(
    h: &Host,
    w: &mut Window,
    arm_wrapper: u32,
    ram: &Ram,
    ctx: &ServiceContext,
) -> bool {
    // 1. HALT, and refuse to continue if it does not confirm. A running core owns the memory the image goes
    //    into, so writing anyway would be corrupting live state rather than loading firmware.
    ctx.log("wifi-driver: halting the ARM before writing into its memory");
    if !aicore::disable(h, w, arm_wrapper, true, ctx) {
        ctx.log("wifi-driver: the ARM would not halt, so nothing is written");
        return false;
    }

    // 2. The image, at the load address the chip's family table gives.
    if !write_bytes(h, w, ram.base, firmware::IMAGE, "firmware", ctx) {
        return false;
    }

    // 3. The NVRAM, transformed, just below the top of RAM, with the token as the last four bytes.
    let mut nv = [0u8; 4096];
    let len = match nvram_prepare(firmware::NVRAM, &mut nv) {
        Some(l) => l,
        None => {
            ctx.log(
                "wifi-driver: the NVRAM did not fit its 4 KiB working buffer after stripping, so it is \
                 not written. The image is loaded but the chip has no board calibration",
            );
            return false;
        }
    };
    let top = ram.base + ram.size;
    let nv_at = top - len as u32;
    ctx.log_fmt(format_args!(
        "wifi-driver: NVRAM {} bytes of text stripped to {} bytes (including the 4-byte token), going to \
         {:#08x}..{:#08x} - the token is the last word of RAM",
        firmware::NVRAM.len(),
        len,
        nv_at,
        top
    ));
    if !write_bytes(h, w, nv_at, &nv[..len], "NVRAM", ctx) {
        return false;
    }

    // 4. RELEASE. `halt = false`, so the CPU runs.
    ctx.log("wifi-driver: releasing the ARM");
    if !aicore::reset(h, w, arm_wrapper, false, ctx) {
        ctx.log(
            "wifi-driver: the ARM did not come out of reset. If everything above succeeded, the first \
             thing to read is `brcmf_sdio_buscore_activate` - it writes the firmware's reset vector to \
             backplane address 0, and that function could not be fetched (see `aicore`)",
        );
        return false;
    }
    ctx.log(
        "wifi-driver: the ARM is running its firmware. There is no control channel yet, so nothing has \
         asked it anything - that is the next step, not a result of this one",
    );
    true
}
