// SPDX-License-Identifier: GPL-2.0-only
//! Phase 2 step 2: ask the ARM CR4 core how much TCM RAM it has, and where the firmware goes in it.
//!
//! **This is the address the upload needs, and it does NOT need the core's wrapper.** That matters because
//! the EROM walk found no master wrapper for this core (step 1), and the first report called that a
//! blocker. It is not one for this step: `brcmf_chip_get_raminfo`'s CR4 branch reads through
//! `brcmf_chip_core_read32`, which is
//!
//! ```c
//! return core->chip->ops->read32(core->chip->ctx, core->pub.base + reg);
//! ```
//!
//! - the core's **base**, not its wrapper. The wrapper is only needed to HALT the CPU
//! (`brcmf_chip_disable_arm`), which is a later sub-step. So the size and the address can be had now.
//!
//! ## What the size is made of
//!
//! The CR4 does not report its memory as one number. `ARMCR4_CAP` says how many banks there are - `nab`
//! "A" banks plus `nbb` "B" banks - and then each bank is selected in turn through `ARMCR4_BANKIDX` and
//! described by `ARMCR4_BANKINFO`: a size field in units of either 8 KiB or 1 KiB depending on one bit.
//! Summing those is the RAM size. Quoted from `brcmf_chip_tcm_ramsize`.
//!
//! **Reading it requires a backplane WRITE** (selecting the bank), which is the first write this driver
//! performs into the chip rather than into its SDIO registers. That path is needed for the upload anyway.
//!
//! ## Where the firmware goes
//!
//! Not computed - looked up. `brcmf_chip_tcm_rambase` is a `switch` on the chip id, and for
//! `BRCM_CC_4345_CHIP_ID` it returns **`0x198000`**. There is no formula; it is a per-part constant, and
//! the reference says so by being a table. A chip this driver has no entry for gets no address rather
//! than a guessed one, exactly as the reference returns `INVALID_RAMBASE`.
//!
//! ## Reference
//!
//! `brcmf_chip_tcm_ramsize`, `brcmf_chip_tcm_rambase`, `brcmf_chip_core_read32` and every `ARMCR4_*`
//! constant quoted from `brcmfmac/chip.c` into this session (§26.14). No code taken; the bank encoding is
//! a property of the core.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use godspeed_wifi::sdio::SdioHost;

/// Register offsets from the CR4 core's base, quoted from `chip.c`.
mod reg {
    /// Capability: how many banks, in two nibbles.
    pub const CAP: u32 = 0x04;
    /// Which bank the next `BANKINFO` read describes.
    pub const BANKIDX: u32 = 0x40;
    /// The selected bank's size and flags.
    pub const BANKINFO: u32 = 0x44;
}

/// Bit fields of those registers, quoted.
mod bits {
    /// `ARMCR4_CAP`: the count of "A" banks.
    pub const TCBANB_MASK: u32 = 0xF;
    pub const TCBANB_SHIFT: u32 = 0;
    /// `ARMCR4_CAP`: the count of "B" banks.
    pub const TCBBNB_MASK: u32 = 0xF0;
    pub const TCBBNB_SHIFT: u32 = 4;
    /// `ARMCR4_BANKINFO`: the bank's size, in units of `BSZ_MULT` (or an eighth of it).
    pub const BSZ_MASK: u32 = 0x7F;
    pub const BSZ_MULT: u32 = 8192;
    /// `ARMCR4_BANKINFO`: this bank's unit is 1 KiB rather than 8 KiB.
    pub const BLK_1K_MASK: u32 = 0x200;
}

/// Where the firmware is written, for the chips this driver has an entry for.
///
/// **A table, not a formula**, because the reference is a table: `brcmf_chip_tcm_rambase` is a `switch` on
/// the chip id with a different constant per part and no arithmetic relating them. A chip with no entry
/// gets `None` rather than a plausible-looking number, which is what `INVALID_RAMBASE` means there.
///
/// Only the parts this driver could actually meet are listed. Adding one means reading its line out of
/// that switch, not extrapolating from these.
pub fn rambase(chip_id: u16) -> Option<u32> {
    Some(match chip_id {
        // BRCM_CC_4345_CHIP_ID - the CYW43455's family, which is this board.
        0x4345 => 0x0019_8000,
        // BRCM_CC_4335 / 4339 / 4354 / 4356 / 4358 / 43602 / 4371 all share this one.
        0x4335 | 0x4339 | 0x4354 | 0x4356 | 0x4358 | 0x4371 => 0x0018_0000,
        // CY_CC_4373_CHIP_ID.
        0x4373 => 0x0016_0000,
        _ => return None,
    })
}

/// What the core says about its memory.
pub struct Ram {
    /// Total TCM bytes, summed over the banks.
    pub size: u32,
    /// How many banks contributed.
    pub banks: u32,
    /// Where the firmware image starts.
    pub base: u32,
}

/// Read the CR4's TCM size by walking its banks, then pair it with the per-part base address.
///
/// `core_base` is the CR4's register base from the EROM walk - **not** its wrapper, which this does not
/// need and which that walk did not find.
pub fn probe(
    h: &dyn SdioHost,
    w: &mut Window,
    core_base: u32,
    chip_id: u16,
    ctx: &ServiceContext,
) -> Option<Ram> {
    let cap = match w.read32(h, core_base + reg::CAP, ctx) {
        Some(v) => v,
        None => {
            ctx.log("wifi-driver: could not read ARMCR4_CAP, so the TCM size is unknown");
            return None;
        }
    };
    let nab = (cap & bits::TCBANB_MASK) >> bits::TCBANB_SHIFT;
    let nbb = (cap & bits::TCBBNB_MASK) >> bits::TCBBNB_SHIFT;
    let totb = nab + nbb;
    ctx.log_fmt(format_args!(
        "wifi-driver: ARMCR4_CAP {:#010x} - {} A bank(s) + {} B bank(s) = {} total",
        cap, nab, nbb, totb
    ));
    if totb == 0 {
        ctx.log(
            "wifi-driver: the core reports ZERO memory banks, which a chip that runs uploaded firmware \
             cannot be. Either the read is wrong or this is not the core it says it is",
        );
        return None;
    }

    let mut size = 0u32;
    // Bounded by the count the core itself reported, and by the field's own width: `nab` and `nbb` are
    // nibbles, so `totb` cannot exceed 30 whatever the register says.
    for idx in 0..totb.min(30) {
        // SELECTING A BANK IS A WRITE, and it is the first write this driver makes into the chip rather
        // than into its SDIO registers. A failure here is reported rather than silently producing a
        // short total, because a size that is quietly too small would be believed.
        if w.write32(h, core_base + reg::BANKIDX, idx, ctx).is_none() {
            ctx.log_fmt(format_args!(
                "wifi-driver: could not select memory bank {} (ARMCR4_BANKIDX), so the total would be \
                 short and is not reported",
                idx
            ));
            return None;
        }
        let bx = match w.read32(h, core_base + reg::BANKINFO, ctx) {
            Some(v) => v,
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: could not read bank {}'s ARMCR4_BANKINFO", idx
                ));
                return None;
            }
        };
        // The unit is 8 KiB unless this bank says otherwise, in which case it is an eighth of that.
        let blksize = if bx & bits::BLK_1K_MASK != 0 {
            bits::BSZ_MULT >> 3
        } else {
            bits::BSZ_MULT
        };
        let bank = ((bx & bits::BSZ_MASK) + 1) * blksize;
        size += bank;
        ctx.log_fmt(format_args!(
            "wifi-driver:   bank {}: BANKINFO {:#010x} -> {} KiB (unit {} B)",
            idx, bx, bank / 1024, blksize
        ));
    }

    let base = match rambase(chip_id) {
        Some(b) => b,
        None => {
            ctx.log_fmt(format_args!(
                "wifi-driver: {} KiB of TCM found, but this driver has no RAM base for chip {:#06x}. \
                 The reference is a per-part TABLE with no formula, so no address is guessed - read that \
                 chip's line out of `brcmf_chip_tcm_rambase`",
                size / 1024,
                chip_id
            ));
            return None;
        }
    };
    Some(Ram { size, banks: totb, base })
}

impl Ram {
    /// Say what this means for the upload, including whether the vendored image fits.
    pub fn report(&self, ctx: &ServiceContext) {
        ctx.log_fmt(format_args!(
            "wifi-driver: the chip has {} KiB of TCM across {} bank(s), and the firmware goes at \
             {:#08x}",
            self.size / 1024,
            self.banks,
            self.base
        ));
        // THE FIT CHECK LIVES WITH THE FIRMWARE NOW, in `firmware::report`, because that module has
        // the actual image and this one had a COPY of its size - `IMAGE_BYTES: u32 = 609_309`, taken
        // out of `nonfree/brcm43455/PROVENANCE`. A number copied from a document is a second truth
        // (Commandment III), and this one would have gone stale silently the moment the blob changed,
        // reporting a comfortable fit for an image of the wrong size.
    }
}
