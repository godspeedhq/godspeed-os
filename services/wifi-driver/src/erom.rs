// SPDX-License-Identifier: GPL-2.0-only
//! Phase 2 step 1: enumerate the chip's internal cores by walking its EROM.
//!
//! **Why this is the first step of the firmware upload.** The image goes into the chip's RAM, and nothing
//! so far knows where that is. The chip publishes a table - the EROM - describing every core on its
//! internal bus with an ID, a revision and a register base. Finding the ARM core and the RAM core in that
//! table is what makes an address to write to, so the enumeration comes before the upload rather than
//! beside it.
//!
//! It is also a step that can be VERIFIED on its own: it prints a table, and the table is either the
//! plausible contents of a CYW43455 or it is not. Phase 1 was won by steps of that shape.
//!
//! ## Reference
//!
//! Every value and every branch below is read from `brcmf_chip_dmp_erom_scan` and
//! `brcmf_chip_dmp_get_regaddr` in `brcmfmac/chip.c`, with the descriptor constants from the same file
//! and the core IDs from `include/linux/bcma/bcma.h`, all quoted into this session rather than recalled
//! (§26.14). The `eromptr` register's position - chipcommon `+0xFC` - was obtained by counting
//! `struct chipcregs` in `brcm80211/include/chipcommon.h` field by field, because an offset guessed is an
//! offset wrong.
//!
//! No code is taken. What is borrowed is the format of a table the silicon publishes, which is a fact
//! about the part.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use godspeed_wifi::sdio::SdioHost;

/// `eromptr` in chipcommon: the backplane address at which the core table lives.
///
/// `0xFC` from counting `struct chipcregs`, not from memory. `bp_addrlow`/`bp_addrhigh`/`bp_data` sit at
/// `0xD0`/`0xD4`/`0xD8` in the same struct - the indirect backplane window a later step may want - and
/// are noted here so that count is not repeated.
const CC_EROMPTR: u32 = 0xFC;

/// Descriptor tags, from `chip.c`. The type is the low nibble of each 32-bit descriptor.
mod desc {
    pub const TYPE_MSK: u32 = 0x0000_000F;
    pub const EMPTY: u8 = 0x0;
    pub const VALID: u32 = 0x0000_0001;
    pub const COMPONENT: u8 = 0x1;
    pub const MASTER_PORT: u8 = 0x3;
    pub const ADDRESS: u8 = 0x5;
    pub const ADDRSIZE_GT32: u32 = 0x0000_0008;
    pub const EOT: u8 = 0xF;
}

/// Fields of the two COMPONENT descriptors that open each core's entry.
mod comp {
    pub const PARTNUM: u32 = 0x000F_FF00;
    pub const PARTNUM_S: u32 = 8;
    /// The reference calls this `DMP_COMP_REVISION`. Suffixed `_MASK` here for two reasons: it IS a
    /// mask, and `cccr::REVISION` in `sdio.rs` is a register ADDRESS - two facts wearing one name in one
    /// crate, which a reader cannot tell apart and which Commandment III refuses.
    pub const REVISION_MASK: u32 = 0xFF00_0000;
    pub const REVISION_S: u32 = 24;
    pub const NUM_SWRAP: u32 = 0x00F8_0000;
    pub const NUM_SWRAP_S: u32 = 19;
    pub const NUM_MWRAP: u32 = 0x0007_C000;
    pub const NUM_MWRAP_S: u32 = 14;
}

/// Fields of an ADDRESS descriptor.
mod slave {
    pub const ADDR_BASE: u32 = 0xFFFF_F000;
    pub const TYPE: u32 = 0x0000_00C0;
    pub const TYPE_S: u32 = 6;
    pub const TYPE_SLAVE: u32 = 0;
    pub const TYPE_SWRAP: u32 = 2;
    pub const TYPE_MWRAP: u32 = 3;
    pub const SIZE_TYPE: u32 = 0x0000_0030;
    pub const SIZE_TYPE_S: u32 = 4;
    pub const SIZE_4K: u32 = 0;
    pub const SIZE_8K: u32 = 1;
    pub const SIZE_DESC: u32 = 3;
}

/// Core IDs, from `include/linux/bcma/bcma.h`. Only the ones this driver has a reason to recognise.
mod core_id {
    pub const CHIPCOMMON: u16 = 0x800;
    /// SOCRAM - where the firmware goes on a chip that has one.
    pub const INTERNAL_MEM: u16 = 0x80E;
    pub const WLAN: u16 = 0x812;
    pub const PMU: u16 = 0x827;
    pub const SDIO_DEV: u16 = 0x829;
    pub const ARM_CM3: u16 = 0x82A;
    /// The ARM core on a 4345. Which of the three ARM cores is present decides how the firmware is
    /// loaded and where, so naming all three is the point of this list.
    pub const ARM_CR4: u16 = 0x83E;
    pub const GCI: u16 = 0x840;
    pub const ARM_CA7: u16 = 0x847;
    pub const SYS_MEM: u16 = 0x849;

    /// A name for the log, or `None` for a core this driver has no reason to know.
    pub fn name(id: u16) -> Option<&'static str> {
        Some(match id {
            CHIPCOMMON => "chipcommon",
            INTERNAL_MEM => "SOCRAM (internal memory)",
            WLAN => "802.11",
            PMU => "PMU",
            SDIO_DEV => "SDIO device",
            ARM_CM3 => "ARM CM3",
            ARM_CR4 => "ARM CR4",
            GCI => "GCI",
            ARM_CA7 => "ARM CA7",
            SYS_MEM => "system memory",
            _ => return None,
        })
    }
}

/// The most cores this walk will describe.
///
/// A chip of this family has on the order of a dozen; the walk is driven by descriptors the CHIP supplies,
/// so it needs a bound, and the store below is sized by the same one so the two cannot disagree.
const MAX_CORES: u32 = 32;

/// How far a core's WRAPPER sits above its register base.
///
/// **A rule, not a pattern.** `cyw43-driver` - the chip vendor's own driver - does not read wrappers out of
/// the EROM at all; it computes them:
///
/// ```c
/// #define WRAPPER_REGISTER_OFFSET  (0x100000)
///
/// static uint32_t get_core_address(int core_id) {
///     if (core_id == CORE_WLAN_ARM) {
///         return WLAN_ARMCM3_BASE_ADDRESS + WRAPPER_REGISTER_OFFSET;
///     ...
/// }
/// ```
///
/// Its own base constants are for a different part of the family, so the BASE still comes from this
/// chip's EROM - which is per-chip - and only the offset is borrowed. That is the split §26.14 asks for:
/// the offset is a property of the silicon, the bases are a property of this die.
///
/// **It is verified rather than trusted.** `Cores::check_wrappers` compares this against every wrapper the
/// EROM did publish, so a wrong offset is caught on the boot it is introduced rather than at the first
/// reset.
const WRAPPER_OFFSET: u32 = 0x10_0000;

/// One core, as the table describes it.
#[derive(Clone, Copy)]
pub struct Core {
    pub id: u16,
    pub rev: u8,
    /// Register base on the backplane.
    pub base: u32,
    /// Wrapper base AS THE TABLE GAVE IT, or 0 if it gave none.
    ///
    /// **PRIVATE on purpose.** `wrapper()` is the only way to obtain a wrapper address, because this field
    /// is 0 for cores the EROM publishes none for - including the ARM CR4 on this chip - and a caller that
    /// reads it directly gets a plausible-looking zero. That is not hypothetical: the report did exactly
    /// that and told the operator there was no wrapper one line after the derivation was verified.
    ///
    /// The self-check does not need this field: the walk records the published values in `seen`, whose
    /// whole purpose is being compared against the derivation.
    ///
    /// **A core is reset through its WRAPPER, not its register base**, so for the ARM core this is the
    /// address the next step needs in order to halt it before writing firmware into it. That is the whole
    /// reason the EROM walk bothers to find a second address per core.
    wrap: u32,
}

impl Core {
    /// This core's wrapper address: its base plus `WRAPPER_OFFSET`.
    ///
    /// **Derived rather than read**, because the EROM does not publish one for every core - the ARM CR4 on
    /// this chip is exactly such a case - while the vendor driver computes every wrapper it uses. `None`
    /// only when the core has no register base at all, since there is then nothing to offset from.
    pub fn wrapper(&self) -> Option<u32> {
        if self.base == 0 {
            None
        } else {
            Some(self.base + WRAPPER_OFFSET)
        }
    }
}

/// What the enumeration found that a firmware upload needs.
pub struct Cores {
    /// The ARM core - CR4, CA7 or CM3 - whichever this chip has.
    pub arm: Option<Core>,
    /// The memory core the firmware is written into, if there is one.
    pub mem: Option<Core>,
    /// The SDIO device core (`0x829`), whose register block carries `INTSTATUS` and the host-to-chip
    /// mailbox the firmware reads its protocol version from.
    ///
    /// The walk already FOUND and NAMED this core - `core 0x829 rev 21 base 0x18004000` is in every boot
    /// log. Nothing held on to it, and `docs/wifi.md` §26 then recorded "the EROM walk has not identified
    /// the SDIOD core's base" as a limitation, which the same log disproved. Kept now, so the claim cannot
    /// be made again.
    pub sdiod: Option<Core>,
    /// The 802.11 core (`0x812`, `core_id::WLAN`), kept because a RESPAWN has to reset it before the
    /// firmware upload: the dead instance's firmware left it running, and the reference's passive
    /// step resets it alongside the ARM (`aicore::D11_PHYRESET`).
    pub wlan: Option<Core>,
    /// How many cores the table described in total.
    pub count: u32,
    /// `(id, base, wrapper AS PUBLISHED)` for each core found, for the derivation self-check.
    ///
    /// Kept because the check needs the EROM's own answer to compare `base + WRAPPER_OFFSET` against, and
    /// that answer is gone once the walk moves on. Fixed-size and stack-only: 32 entries of ten bytes.
    seen: [(u16, u32, u32); MAX_CORES as usize],
}

/// Read one descriptor and advance, skipping EMPTY ones as `brcmf_chip_dmp_get_desc` does.
///
/// Returns `None` if the read itself failed, which is different from a descriptor that says EOT - a caller
/// must not treat a dead bus as the end of a table.
fn get_desc(h: &dyn SdioHost, w: &mut Window, at: &mut u32, ctx: &ServiceContext) -> Option<(u32, u8)> {
    /// A bound, because the loop below is driven by values the CHIP supplies. An EMPTY run longer than
    /// this is a table that is not a table.
    const MAX_EMPTY: u32 = 64;
    for _ in 0..MAX_EMPTY {
        let val = w.read32(h, *at, ctx)?;
        *at += 4;
        let ty = (val & desc::TYPE_MSK) as u8;
        if ty != desc::EMPTY {
            return Some((val, ty));
        }
    }
    ctx.log("wifi-driver: the EROM returned nothing but EMPTY descriptors - that is not a core table");
    None
}

/// Find a core's register and wrapper bases, following `brcmf_chip_dmp_get_regaddr`.
///
/// Returns `None` when the entry has no usable address pair, which the reference treats as "skip this
/// core" rather than as a failure - so a caller continues.
fn get_regaddr(h: &dyn SdioHost, w: &mut Window, at: &mut u32, ctx: &ServiceContext) -> Option<(u32, u32)> {
    let (_, d) = get_desc(h, w, at, ctx)?;
    let wraptype = if d == desc::MASTER_PORT {
        slave::TYPE_MWRAP
    } else if d == desc::ADDRESS {
        // Put it back: this descriptor belongs to the loop below.
        *at -= 4;
        slave::TYPE_SWRAP
    } else {
        *at -= 4;
        return None;
    };
    // WHICH WRAPPER TYPE THIS ENTRY IS LOOKING FOR, said out loud. A leading `MASTER_PORT` descriptor
    // means the wrapper will be published as a MASTER wrapper; a leading `ADDRESS` means a SLAVE one. An
    // entry that publishes the other kind yields a wrapper of 0, and distinguishing that from a walk that
    // skipped one is exactly why this and the per-descriptor lines below exist.
    ctx.log_fmt(format_args!(
        "wifi-driver:     entry opens with {}, so the wrapper must be a {} wrapper",
        if wraptype == slave::TYPE_MWRAP { "MASTER_PORT" } else { "ADDRESS" },
        if wraptype == slave::TYPE_MWRAP { "MASTER" } else { "SLAVE" }
    ));

    let mut regbase = 0u32;
    let mut wrapbase = 0u32;
    /// Bounded, because every step is driven by chip-supplied descriptors.
    const MAX_DESCS: u32 = 64;
    let mut seen = 0u32;
    while regbase == 0 || wrapbase == 0 {
        seen += 1;
        if seen > MAX_DESCS {
            return None;
        }
        // Locate an ADDRESS descriptor, stopping if the next COMPONENT starts.
        let (val, d) = loop {
            let (v, d) = get_desc(h, w, at, ctx)?;
            if d == desc::EOT {
                *at -= 4;
                return None;
            }
            if d == desc::ADDRESS || d == desc::COMPONENT {
                break (v, d);
            }
        };
        if d == desc::COMPONENT {
            // Crossed into the next core's entry; hand it back and report what we have.
            *at -= 4;
            return Some((regbase, wrapbase));
        }
        // A 64-bit address spends a second descriptor.
        if val & desc::ADDRSIZE_GT32 != 0 {
            get_desc(h, w, at, ctx)?;
        }
        let sztype = (val & slave::SIZE_TYPE) >> slave::SIZE_TYPE_S;
        let stype_raw = (val & slave::TYPE) >> slave::TYPE_S;
        // RAW AND DECODED, both, and BOUNDED so a malformed table cannot flood the console. This is the
        // measurement that says whether a missing wrapper is absent from the chip's table or was skipped
        // by this walk - the two have different fixes and a zero in a summary line cannot tell them apart.
        if seen <= 6 {
            ctx.log_fmt(format_args!(
                "wifi-driver:     addr desc {:#010x}: base {:#010x} type {} ({}) size {} ({})",
                val,
                val & slave::ADDR_BASE,
                stype_raw,
                match stype_raw {
                    0 => "slave",
                    1 => "bridge",
                    2 => "swrap",
                    _ => "mwrap",
                },
                sztype,
                match sztype {
                    0 => "4K",
                    1 => "8K",
                    2 => "16K",
                    _ => "described separately",
                }
            ));
        }
        if sztype == slave::SIZE_DESC {
            let (szd, _) = get_desc(h, w, at, ctx)?;
            if szd & desc::ADDRSIZE_GT32 != 0 {
                get_desc(h, w, at, ctx)?;
            }
        }
        // Only 4K and 8K register regions are the ones being looked for.
        if sztype != slave::SIZE_4K && sztype != slave::SIZE_8K {
            continue;
        }
        let stype = (val & slave::TYPE) >> slave::TYPE_S;
        if regbase == 0 && stype == slave::TYPE_SLAVE {
            regbase = val & slave::ADDR_BASE;
        }
        if wrapbase == 0 && stype == wraptype {
            wrapbase = val & slave::ADDR_BASE;
        }
    }
    Some((regbase, wrapbase))
}

/// Walk the EROM and report every core, returning the four the bring-up uses: the ARM, memory, SDIO
/// device and 802.11 cores.
pub fn scan(h: &dyn SdioHost, w: &mut Window, ctx: &ServiceContext) -> Option<Cores> {
    let eromaddr_reg = crate::backplane::CHIPCOMMON_BASE + CC_EROMPTR;
    let mut at = match w.read32(h, eromaddr_reg, ctx) {
        Some(v) if v != 0 && v != 0xFFFF_FFFF => v,
        Some(v) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: eromptr reads {:#010x}, which is the bus answering with nothing rather \
                 than a table address",
                v
            ));
            return None;
        }
        None => {
            ctx.log("wifi-driver: could not read eromptr, so the core table cannot be found");
            return None;
        }
    };
    ctx.log_fmt(format_args!("wifi-driver: EROM at {:#010x}, walking it", at));

    let mut out = Cores {
        arm: None,
        mem: None,
        sdiod: None,
        wlan: None,
        count: 0,
        seen: [(0, 0, 0); MAX_CORES as usize],
    };

    loop {
        if out.count >= MAX_CORES {
            ctx.log_fmt(format_args!(
                "wifi-driver: the EROM walk stopped at its {}-core bound without reaching the end of \
                 the table",
                MAX_CORES
            ));
            break;
        }
        let (val, ty) = match get_desc(h, w, &mut at, ctx) {
            Some(x) => x,
            None => break,
        };
        if ty == desc::EOT {
            ctx.log_fmt(format_args!(
                "wifi-driver: the EROM ended properly after {} core(s)", out.count
            ));
            break;
        }
        if val & desc::VALID == 0 || ty != desc::COMPONENT {
            continue;
        }
        let id = ((val & comp::PARTNUM) >> comp::PARTNUM_S) as u16;

        // The second COMPONENT descriptor carries the revision and the port counts.
        let (val2, ty2) = match get_desc(h, w, &mut at, ctx) {
            Some(x) => x,
            None => break,
        };
        if ty2 != desc::COMPONENT {
            ctx.log_fmt(format_args!(
                "wifi-driver: core {:#05x}'s second descriptor is type {:#x}, not a component - the \
                 table is malformed and the walk stops here",
                id, ty2
            ));
            break;
        }
        let rev = ((val2 & comp::REVISION_MASK) >> comp::REVISION_S) as u8;
        let nmw = (val2 & comp::NUM_MWRAP) >> comp::NUM_MWRAP_S;
        let nsw = (val2 & comp::NUM_SWRAP) >> comp::NUM_SWRAP_S;

        // Cores with no ports have no registers to find. The PMU and GCI are exceptions the reference
        // makes explicitly, because they matter and have none.
        if nmw + nsw == 0 && id != core_id::PMU && id != core_id::GCI {
            continue;
        }
        let (base, wrap) = match get_regaddr(h, w, &mut at, ctx) {
            Some(x) => x,
            None => continue,
        };

        let core = Core { id, rev, base, wrap };
        // RECORD WHAT THE TABLE SAID, before anything derives anything. This is the evidence the wrapper
        // rule is checked against, and it is only available here.
        out.seen[out.count as usize] = (id, base, wrap);
        out.count += 1;
        ctx.log_fmt(format_args!(
            "wifi-driver:   core {:#05x} rev {:<3} base {:#010x} wrap {:#010x}  {}",
            id,
            rev,
            base,
            wrap,
            core_id::name(id).unwrap_or("(not a core this driver names)")
        ));

        // WHICH ARM, because the answer decides how the firmware is loaded. A CR4 or CA7 runs from its
        // own TCM and is reset through the core itself; a CM3 chip writes into a separate SOCRAM. Only
        // the first match is kept - a chip has one.
        if out.arm.is_none()
            && (id == core_id::ARM_CR4 || id == core_id::ARM_CA7 || id == core_id::ARM_CM3)
        {
            out.arm = Some(core);
        }
        if out.mem.is_none() && (id == core_id::INTERNAL_MEM || id == core_id::SYS_MEM) {
            out.mem = Some(core);
        }
        if out.sdiod.is_none() && id == core_id::SDIO_DEV {
            out.sdiod = Some(core);
        }
        if out.wlan.is_none() && id == core_id::WLAN {
            out.wlan = Some(core);
        }
    }

    if out.count == 0 {
        ctx.log("wifi-driver: the EROM described no cores at all, which cannot be true of a live chip");
        return None;
    }
    Some(out)
}

impl Cores {
    /// Check the derived wrapper rule against every wrapper the EROM actually published.
    ///
    /// **This is what stops `WRAPPER_OFFSET` being taken on the vendor driver's word.** Where the table
    /// gave a wrapper, `base + WRAPPER_OFFSET` must equal it; those are independent data points from this
    /// die, and a mismatch means the offset is wrong for this part and every derived wrapper with it. Kept
    /// as a check rather than a fallback on purpose: silently preferring the published value where there
    /// is one would hide the disagreement, and the disagreement is the interesting thing.
    pub fn check_wrappers(&self, ctx: &ServiceContext) {
        let mut agree = 0u32;
        let mut disagree = 0u32;
        for &(id, base, pub_wrap) in self.seen[..self.count as usize].iter() {
            if pub_wrap == 0 || base == 0 {
                continue;
            }
            if base + WRAPPER_OFFSET == pub_wrap {
                agree += 1;
            } else {
                disagree += 1;
                ctx.log_fmt(format_args!(
                    "wifi-driver: core {:#05x} publishes wrapper {:#010x} but base {:#010x} + {:#08x} is \
                     {:#010x} - the wrapper OFFSET is wrong for this part and every derived wrapper with it",
                    id, pub_wrap, base, WRAPPER_OFFSET, base + WRAPPER_OFFSET
                ));
            }
        }
        if disagree == 0 && agree > 0 {
            ctx.log_fmt(format_args!(
                "wifi-driver: the wrapper rule (base + {:#08x}) agrees with all {} wrapper(s) the EROM \
                 published, so deriving the ones it did not is sound on this die",
                WRAPPER_OFFSET, agree
            ));
        } else if agree == 0 {
            ctx.log(
                "wifi-driver: the EROM published no wrappers to check the derivation against, so it rests \
                 on the reference alone",
            );
        }
    }

    /// Say what the table means for the upload, in the terms the next step needs.
    pub fn report(&self, ctx: &ServiceContext) {
        match self.arm {
            // THE WRAPPER IS DERIVED, and the EROM not publishing one for this core is expected
            // rather than a defect. The entry opens with a `MASTER_PORT` descriptor so `get_regaddr`
            // requires a MASTER wrapper, and the only one the chip publishes for this core is a SLAVE
            // wrapper at `0x18105000` - which belongs to the entry's SECOND slave region (`0x18005000`
            // + `WRAPPER_OFFSET`), not to its first. The reference's walk reads 0 here too.
            //
            // A PREVIOUS VERSION OF THIS COMMENT CALLED THE DERIVED VALUE WRONG. It is not: the vendor
            // driver computes every wrapper it uses as base + 0x100000, so the pattern was the rule and
            // what was missing was a source for it. Refusing an unsourced pattern was right; calling it
            // wrong was not.
            // THE DERIVED WRAPPER, and it says which it is. The EROM publishing none for this core is
            // expected rather than a fault - its entry opens with a `MASTER_PORT` descriptor so
            // `get_regaddr` requires a MASTER wrapper, and the only one the chip publishes for it is a
            // SLAVE wrapper belonging to the entry's SECOND slave region. The reference reads 0 here
            // too, and computes the address instead (`WRAPPER_OFFSET`).
            Some(c) => ctx.log_fmt(format_args!(
                "wifi-driver: the ARM core is {} rev {} at {:#010x}, wrapper {:#010x} ({}) - that is \
                 where it is halted before the upload and released after",
                core_id::name(c.id).unwrap_or("?"),
                c.rev,
                c.base,
                c.wrapper().unwrap_or(0),
                if c.wrap == 0 {
                    "derived; the EROM published none for this core"
                } else {
                    "derived, and the EROM published the same"
                }
            )),
            None => ctx.log(
                "wifi-driver: NO ARM core in the table, which a chip that runs uploaded firmware must \
                 have. Either the walk is wrong or this is not the part it claims to be",
            ),
        }
        match self.mem {
            Some(c) => ctx.log_fmt(format_args!(
                "wifi-driver: a memory core is present ({} rev {} at {:#010x}, wrapper {:#010x})",
                core_id::name(c.id).unwrap_or("?"),
                c.rev,
                c.base,
                c.wrapper().unwrap_or(0)
            )),
            // Not a fault. A CR4/CA7 chip runs from TCM inside the ARM core and has no separate memory
            // core, which is exactly the case the next step has to handle differently.
            None => ctx.log(
                "wifi-driver: no separate memory core, so this chip runs from TCM inside its ARM core - \
                 the firmware address comes from that core rather than from a SOCRAM",
            ),
        }
    }
}
