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
//! **And it must be F1_BLOCK mode, for an arithmetic reason.** A byte-mode CMD53 carries at most 512 bytes, and
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
use crate::backplane::{Window, ACCESS_WIDE, OFFSET_MASK};
use crate::firmware;
use crate::host::{blk_block_mode, blk_byte_mode, Host};
use crate::sdio;

/// Function 1's block size, as set in step 2 and as `SDIO_FUNC1_BLOCKSIZE` in brcmfmac.
const F1_BLOCK: usize = 64;

/// How much is moved per CMD53. Sixteen blocks, so the whole image is about 600 transactions rather than
/// 152,000 - and 1 KiB of stack for the word-aligned copy, which is bounded and visible (§26.6.1).
const CHUNK: usize = 16 * F1_BLOCK;

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
        let (n, blocks) = if want >= F1_BLOCK {
            let blocks = want / F1_BLOCK;
            (blocks * F1_BLOCK, blocks)
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
                                 blk_block_mode(blocks as u32, F1_BLOCK as u32),
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
/// Find out WHICH of three things a failing block write is, in one boot rather than three.
///
/// **What is already known.** `services/block-driver/src/sdhci.rs` does single-block PIO on this same
/// controller family and works, at 512 bytes a block. Our own byte-mode CMD53 moves 4 bytes and works.
/// Nothing in this repository has ever issued a MULTI-block transfer on it, and that is exactly where the
/// firmware write fails - with the command accepted, no error bit set, and no data phase ever starting.
///
/// **So there are three separable candidates**, and guessing between them costs a flash each:
///
/// 1. the 64-byte block SIZE (the working driver only ever used 512),
/// 2. the `SDHCI_TRNS_MULTI` bit,
/// 3. the block COUNT.
///
/// This walks them in order, smallest difference first. The first rung to fail names the culprit:
///
/// | rung | mode  | MULTI | blocks | means, if this is the first to fail             |
/// |------|-------|-------|--------|--------------------------------------------------|
/// | 1    | byte  | no    | -      | the bus or the window is wrong, not block mode    |
/// | 2    | block | no    | 1      | the 64-byte F1_BLOCK SIZE is the problem             |
/// | 3    | block | yes   | 2      | the MULTI bit is the problem                      |
/// | 4    | block | yes   | 16     | the block COUNT is the problem                    |
///
/// **Every rung reads back the first word it wrote**, because a write that reports success and lands
/// nothing is a silent failure and worse than a loud one (§26.7). The read-back uses byte mode, which rung
/// 1 has just proved on this very address.
///
/// It writes into the halted ARM's TCM at the firmware's own load address - where the bulk write is about
/// to go anyway - so it needs no scratch region and costs nothing but the transfers themselves.
/// Ask the chip whether its firmware BOOTED, rather than inferring it from the reset controller.
///
/// `RESETCTRL 0` says the CPU is fetching. A CPU fetching garbage says the same thing, so releasing the
/// core is not evidence that firmware is running. The reference has a real test, and its comment is the
/// whole idea:
///
/// ```c
///	/* NVRAM length at the end of memory should have been overwritten. */
///	shaddr = bus->ci->rambase + bus->ci->ramsize - 4;
///	rv = brcmf_sdiod_ramrw(bus->sdiodev, false, shaddr, (u8 *)&addr_le, 4);
/// ```
///
/// The last word of RAM carries the NVRAM length token the HOST wrote, so the firmware can find its
/// calibration at boot. Having consumed it, the firmware **overwrites that word** with the address of its
/// own SDPCM shared structure. So that one word answers the question: still our token means the firmware
/// never ran; a plausible TCM address means it booted and that is where its structure lives.
///
/// This is a STRONGER check than the reference's. brcmfmac cannot know which token was written and uses a
/// generic pattern test; we wrote it, so the comparison is exact.
///
/// It retries where the reference does not, and the reason is honest rather than defensive: brcmfmac
/// reaches this point late in a longer sequence, while here it is milliseconds after release, so a
/// firmware still starting would be called dead. If the word never changes that is REPORTED as a fact -
/// a loaded chip whose firmware did not start is precisely the state worth naming (§26.7).
fn firmware_alive(h: &Host, w: &mut Window, ram: &Ram, token: u32, ctx: &ServiceContext) -> bool {
    /// `SDPCM_SHARED_VERSION_MASK`.
    const VERSION_MASK: u32 = 0x0000_00FF;
    /// `SDPCM_SHARED_TRAP` - the firmware took a trap and filled in the record `trap_addr` points at.
    const TRAP: u32 = 0x0000_0400;
    /// `SDPCM_SHARED_VERSION` - the newest the reference understands.
    const VERSION: u32 = 0x0003;
    const LIVENESS_TRIES: u32 = 20;

    let shaddr = ram.base + ram.size - 4;
    let mut last = token;
    for attempt in 0..LIVENESS_TRIES {
        match w.read32(h, shaddr, ctx) {
            Some(v) => {
                last = v;
                if v != token {
                    // IN RANGE? A word that changed to something outside TCM is not a shared-structure
                    // pointer, and saying "alive" on it would be worse than saying nothing.
                    if v < ram.base || v >= ram.base + ram.size {
                        ctx.log_fmt(format_args!(
                            "wifi-driver: the last word of RAM changed from our token {:#010x} to \
                             {:#010x}, which is OUTSIDE the chip's RAM ({:#08x}..{:#08x}) - so something \
                             ran, but that is not a shared-structure address",
                            token, v, ram.base, ram.base + ram.size
                        ));
                        return false;
                    }
                    let flags = w.read32(h, v, ctx);
                    match flags {
                        Some(f) => {
                            let ver = f & VERSION_MASK;
                            ctx.log_fmt(format_args!(
                                "wifi-driver: THE FIRMWARE IS ALIVE - it overwrote our NVRAM token with \
                                 {:#010x} after {} read(s), and its shared structure reports flags \
                                 {:#010x} (SDPCM version {}, this driver understands up to {})",
                                v, attempt + 1, f, ver, VERSION
                            ));
                            if ver > VERSION {
                                ctx.log(
                                    "wifi-driver: that version is NEWER than the layout this driver \
                                     knows, so the structure's fields past `flags` are not safe to read",
                                );
                            } else if f & TRAP != 0 {
                                // THE FIRMWARE TRAPPED, and it says where. `SDPCM_SHARED_TRAP` in the
                                // flags means `trap_addr` (the word after `flags`) points at a
                                // `brcmf_trap_info`: type, epc, cpsr, spsr, r0-r7, pc, sp, lr. Every
                                // respawn under `chaos max-carnage` came alive with this bit set and
                                // the boot never did; reading it out is what turns "function 2 never
                                // came ready" into an address.
                                let ta = w.read32(h, v + 4, ctx).unwrap_or(0);
                                let mut rd = |off: u32| w.read32(h, ta + off, ctx).unwrap_or(0xFFFF_FFFF);
                                if ta >= ram.base && ta < ram.base + ram.size {
                                    ctx.log_fmt(format_args!(
                                        "wifi-driver: THE FIRMWARE TRAPPED AT START - trap type {:#x}, epc {:#010x}, pc {:#010x}, lr {:#010x}, sp {:#010x} (trap info at {:#010x})",
                                        rd(0), rd(4), rd(48), rd(56), rd(52), ta));
                                } else {
                                    ctx.log_fmt(format_args!(
                                        "wifi-driver: THE FIRMWARE TRAPPED AT START, and its trap pointer {:#010x} is outside RAM, so there is nothing more to read",
                                        ta));
                                }
                            }
                            return true;
                        }
                        None => {
                            ctx.log_fmt(format_args!(
                                "wifi-driver: the firmware published a shared structure at {:#010x} but \
                                 reading it failed, so whether it is alive is unknown",
                                v
                            ));
                            return false;
                        }
                    }
                }
            }
            None => {
                ctx.log("wifi-driver: could not read the last word of RAM, so firmware liveness is \
                         unknown");
                return false;
            }
        }
        ctx.sleep_ms(10);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the last word of RAM still holds OUR NVRAM token {:#010x} after {} reads over \
         ~{} ms, so THE FIRMWARE HAS NOT RUN. The image and the NVRAM are in the chip and its CPU is out \
         of reset, so what is missing is the reset vector - `brcmf_sdio_buscore_activate` writes the \
         image's first four bytes to backplane address 0 before the core is restored, and this driver \
         does not (see `aicore`)",
        last, LIVENESS_TRIES, LIVENESS_TRIES * 10
    ));
    false
}

fn ladder(h: &Host, w: &mut Window, addr: u32, ctx: &ServiceContext) -> bool {
    // Distinct per rung, so a read-back cannot pass on a stale value another rung left behind.
    const MARKS: [u32; 4] = [0xA1A1_0001, 0xB2B2_0002, 0xC3C3_0003, 0xD4D4_0004];
    // (name, blocks-or-none, words, what a failure here means)
    let rungs: [(&str, Option<u32>, usize, &str); 4] = [
        ("byte mode, 4 bytes", None, 1, "the bus or the window - not block mode at all"),
        ("block mode, ONE 64-byte block, no MULTI", Some(1), F1_BLOCK / 4,
         "the 64-BYTE F1_BLOCK SIZE (the working SD driver only ever used 512)"),
        ("block mode, TWO 64-byte blocks, MULTI", Some(2), 2 * F1_BLOCK / 4,
         "the MULTI bit"),
        ("block mode, SIXTEEN 64-byte blocks, MULTI", Some(16), 16 * F1_BLOCK / 4,
         "the block COUNT"),
    ];

    ctx.log("wifi-driver: probing the data path before the bulk write - four rungs, smallest first");
    for (rung, (name, blocks, words, means)) in rungs.iter().enumerate() {
        if !w.set_for(h, addr, ctx) {
            ctx.log("wifi-driver:   the window would not set, so the probe says nothing about block mode");
            return false;
        }
        let off = addr & OFFSET_MASK;
        let mut buf = [0u32; CHUNK / 4];
        for (i, word) in buf[..*words].iter_mut().enumerate() {
            *word = MARKS[rung] ^ (i as u32);
        }
        let blk = match blocks {
            Some(n) => blk_block_mode(*n, F1_BLOCK as u32),
            None => blk_byte_mode(*words as u32 * 4),
        };
        let ok = sdio::write_extended(
            h, 1, off | ACCESS_WIDE, &mut buf[..*words], blk, *blocks, ctx,
        );
        if !ok {
            ctx.log_fmt(format_args!(
                "wifi-driver:   rung {} FAILED ({}), and it is the first to fail - so the fault is {}",
                rung + 1, name, means
            ));
            return false;
        }
        // AND IT MUST HAVE LANDED. A rung that reports success and wrote nothing would send the bulk
        // write off with a false green light.
        match w.read32(h, addr, ctx) {
            Some(v) if v == MARKS[rung] => ctx.log_fmt(format_args!(
                "wifi-driver:   rung {} ok ({}) - and {:#010x} read back",
                rung + 1, name, v
            )),
            Some(v) => {
                ctx.log_fmt(format_args!(
                    "wifi-driver:   rung {} ({}) reported success but {:#010x} came back where {:#010x} \
                     was written - the transfer is being ACCEPTED and DISCARDED, which no error bit says",
                    rung + 1, name, v, MARKS[rung]
                ));
                return false;
            }
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver:   rung {} ({}) wrote without error but the read-back itself failed, so \
                     whether it landed is unknown",
                    rung + 1, name
                ));
                return false;
            }
        }
    }
    ctx.log("wifi-driver: all four rungs pass, so the data path carries 16 blocks of 64 - the bulk write \
             should work");
    true
}

pub fn run(
    h: &Host,
    w: &mut Window,
    arm_wrapper: u32,
    ram: &Ram,
    ctx: &ServiceContext,
) -> bool {
    // 1. HALT THE CPU, but leave the CORE OUT OF RESET - which is not the same thing, and getting it
    //    wrong is why the first write into TCM never completed.
    //
    //    A running core owns the memory the image goes into, so its CPU must be stopped before anything
    //    is written. But the TCM is INSIDE the core, so a core held in reset does not answer backplane
    //    accesses to its own memory: the card accepts the command, the host FIFO drains, and the
    //    transaction never completes. Measured exactly that way - `WRITE_RDY` and `BUFFER_WRITE_ENABLE`
    //    both seen, the word written, and `TRANSFER_COMPLETE` never arriving.
    //
    //    `brcmf_chip_disable_arm` makes the distinction explicit, and it is the whole answer: a CM3 gets
    //    `brcmf_chip_coredisable(core, 0, 0)` and stays in reset, while a CR4 gets
    //    `brcmf_chip_resetcore(core, val, ARMCR4_BCMA_IOCTL_CPUHALT, ARMCR4_BCMA_IOCTL_CPUHALT)` - reset,
    //    not disable, so the core ends up out of reset and clocked with the CPU held halted.
    //
    //    `aicore::reset(halt = true)` is that sequence; it was being called one level too low.
    ctx.log("wifi-driver: bringing the ARM out of reset with its CPU HALTED - its TCM is only reachable \
             when the core itself is running");
    if !aicore::reset(h, w, arm_wrapper, true, ctx) {
        ctx.log("wifi-driver: the ARM would not come out of reset with its CPU halted, so nothing is \
                 written");
        return false;
    }

    // 2. PROVE THE DATA PATH before committing 609 KB to it. This is a bisection, not a precaution: a
    //    block write is failing with the command accepted and no error reported, and there are three
    //    separable candidates. The ladder names which one in a single boot, and writing into the halted
    //    core's TCM at the load address costs nothing because that is where the image goes next.
    if !ladder(h, w, ram.base, ctx) {
        ctx.log(
            "wifi-driver: the data path does not carry what the upload needs, so no firmware is written. \
             The rung that failed, just above, says which of block size, the MULTI bit, or the block \
             count is the fault",
        );
        return false;
    }

    // 3. The image, at the load address the chip's family table gives.
    if !write_bytes(h, w, ram.base, firmware::IMAGE, "firmware", ctx) {
        return false;
    }

    // 4. The NVRAM, transformed, just below the top of RAM, with the token as the last four bytes.
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
    // KEEP THE TOKEN. It is the last four bytes just written, and it is the exact value the firmware is
    // expected to overwrite - so comparing against it later is stronger evidence than the reference's
    // generic pattern test, which cannot know what the host put there.
    let token = u32::from_le_bytes([nv[len - 4], nv[len - 3], nv[len - 2], nv[len - 1]]);

    // 5. THE RESET VECTOR, to backplane address 0, while the CPU is still halted.
    //
    //    The image sits at `0x198000`, but the CR4 begins fetching from backplane address 0 when it comes
    //    out of reset. The first word of the image is the branch that gets it from there to the loaded
    //    code, so without this write the CPU executes whatever address 0 happens to hold - which is
    //    precisely the state the last boot measured: `CPU EXECUTING`, and the NVRAM token at the top of
    //    RAM untouched because no firmware ever ran to consume it.
    //
    //    From OpenBSD's `bwfm`, a clean-room reimplementation of brcmfmac, because Linux's `sdio.c`
    //    truncates before `brcmf_sdio_buscore_activate` on every fetch:
    //
    //    ```c
    //    if (rstvec)
    //            bwfm_sdio_ram_read_write(sc, 0, (char *)&rstvec, sizeof(rstvec), 1);
    //    ```
    //
    //    and its caller gives the value: `bwfm_chip_set_active(bwfm, *(uint32_t *)ucode)` - the FIRST FOUR
    //    BYTES of the image. Linux agrees independently, `rstvec = get_unaligned_le32(fw->data)`.
    //
    //    ORDER IS THE REFERENCE'S: `brcmf_chip_cr4_set_active` calls `activate(..., rstvec)` and only THEN
    //    `resetcore(core, ARMCR4_BCMA_IOCTL_CPUHALT, 0, 0)`, so this happens before the release below.
    //
    //    DIVERGENCE, corrected: the reference first clears the SDIO device core's `INTSTATUS` with
    //    `0xFFFFFFFF`. This was originally skipped here on the stated grounds that the EROM walk had not
    //    identified the SDIOD core's base - which was simply FALSE, and the same boot log disproved it
    //    (`core 0x829 rev 21 base 0x18004000 wrap 0x18104000 SDIO device`). The walk found it and this
    //    driver even printed its name. The clear now happens in `bus::bring_up`, where the reference also
    //    puts it: immediately before the protocol version goes to the mailbox.
    if firmware::IMAGE.len() < 4 {
        ctx.log("wifi-driver: the embedded image is too short to contain a reset vector");
        return false;
    }
    let rstvec = u32::from_le_bytes([
        firmware::IMAGE[0],
        firmware::IMAGE[1],
        firmware::IMAGE[2],
        firmware::IMAGE[3],
    ]);
    ctx.log_fmt(format_args!(
        "wifi-driver: reset vector {:#010x} (the image's first four bytes) going to backplane address 0 - \
         the CR4 fetches from there on release, not from {:#08x}",
        rstvec, ram.base
    ));
    if rstvec == 0 {
        // The reference guards on this too (`if (rstvec)`), and a zero vector would mean the image does
        // not begin with a branch - worth saying rather than writing a zero and wondering later.
        ctx.log(
            "wifi-driver: the image's first word is ZERO, so there is no reset vector to write. The \
             reference skips the write in this case and so does this - but for this chip that is \
             unexpected, and it would explain a core that runs nothing",
        );
    } else if !write_bytes(h, w, 0, &rstvec.to_le_bytes(), "reset vector", ctx) {
        return false;
    }

    // 6. RELEASE. `halt = false`, so the CPU runs.
    ctx.log("wifi-driver: releasing the ARM");
    if !aicore::reset(h, w, arm_wrapper, false, ctx) {
        ctx.log(
            "wifi-driver: the ARM did not come out of reset. If everything above succeeded, the first \
             thing to read is `brcmf_sdio_buscore_activate` - it writes the firmware's reset vector to \
             backplane address 0, and that function could not be fetched (see `aicore`)",
        );
        return false;
    }
    // 7. ASK THE CHIP, rather than asserting from the reset controller. `RESETCTRL 0` means the CPU is
    //    fetching; it does not mean the firmware booted, and a CPU fetching garbage reports the same
    //    thing. The firmware overwrites the NVRAM token at the last word of RAM once it has consumed it,
    //    so that word is the answer.
    if !firmware_alive(h, w, ram, token, ctx) {
        ctx.log(
            "wifi-driver: the image and the NVRAM are in the chip and its CPU is out of reset, but \
             nothing confirms the firmware is running - so this is NOT reported as a working radio",
        );
        return false;
    }
    true
}
