// SPDX-License-Identifier: GPL-2.0-only
//! BCM2711 SD host controller census - **which controller is the WiFi radio behind.**
//!
//! `docs/wifi.md` records this as the fact the whole Pi 4 phase-1 estimate rests on, and as something
//! that needed a device tree read. It does not: the machine can be asked, and asking it is both safer
//! and more honest than reading a specification and believing it.
//!
//! **THE QUESTION.** The BCM2711 has more than one SD host controller. The claim phase 1 depends on is
//! that the SD *card* is on one of them and the **CYW43455 radio is on the other** - specifically the
//! older Arasan block, which is the controller `services/block-driver/src/sdhci.rs` already drives (25
//! KB of working polled code, written for the Pi 2 and never compiled in because there the Arasan IS
//! the boot card). If that holds, phase 1 is small. If it does not, phase 1 starts with a new
//! controller driver.
//!
//! **WHY A PROBE RATHER THAN A CONSTANT.** Two addresses are involved and they have different standing,
//! which matters more than it looks:
//!
//!   * `+0x30_0000` is an **in-repo fact**: `arch/arm/mod.rs` grants `block-driver` the Arasan EMMC at
//!     `PERIPHERAL_BASE + 0x30_0000` on the BCM2836, and `sdhci.rs` documents the same offset. The
//!     peripheral base for this SoC, `0xFE00_0000`, is likewise already here (`mmu.rs`, and `PL011_BASE`
//!     is derived from it).
//!   * `+0x34_0000` was **NOT** an in-repo fact when this was written - it was recollection, and it was
//!     probed and labelled UNVERIFIED rather than granted on that basis.
//!
//! **Both are settled now, from the vendor's own device tree** (Raspberry Pi kernel `rpi-6.6.y`), which
//! is what the firmware and Linux both act on:
//!
//! ```text
//! bcm2711-rpi-4-b.dts:  &mmcnr { pinctrl-0 = <&sdio_pins>; bus-width = <4>; status = "okay"; }  <- WiFi
//!                       &emmc2 { ... status = "okay"; }                                    <- SD card
//!                       &sdhost { status = "disabled"; }
//! bcm270x.dtsi:         mmcnr: mmcnr@7e300000 { reg = <0x7e300000 0x100>; }
//!                       sdhci: mmc@7e300000   { reg = <0x7e300000 0x100>; }   SAME CONTROLLER, twice
//! bcm2711.dtsi:         emmc2 ... reg = <0x0 0x7e340000 0x100>;
//! ```
//!
//! `mmcnr` and `sdhci` are ONE controller described by two nodes - same `reg`, differing only in which
//! driver claims it. Bus `0x7e300000` is ARM physical `0xFE30_0000` in low-peripheral mode. So **the
//! radio is on the Arasan block `sdhci.rs` already drives**, and the SD card is on `emmc2` - which is
//! `docs/wifi.md` section 4's central claim, confirmed rather than assumed.
//!
//! The UNVERIFIED label was still right to have been there. It described the EVIDENCE, not the value,
//! and being lucky about a number is not the same as knowing it.
//!
//! The census stays, because a document is not a board: it now CONFIRMS what the device tree says, and
//! a disagreement between the two would be the most interesting thing this probe could find.
//!
//! **NOTHING IS GRANTED BY THIS FILE**, deliberately. It reads, it prints, it returns. The
//! `map_fixed_driver_mmio` table stays as it is until the boot log says which window to name, because
//! that table's own comment records the cost of getting this wrong: a service handed a range whose first
//! register read is an external abort dies on that read, and the supervisor respawns it forever.
//!
//! **THE RESIDUAL RISK, stated rather than glossed.** An abort-safe read protects against an address
//! that decodes to nothing. It does not protect against an address that decodes to a DIFFERENT
//! peripheral, where a read could in principle have a side effect. The two offsets read here are the
//! documented SD-controller region of this SoC and the registers chosen are SDHCI's read-only ones, so
//! the exposure is small - but it is not zero, and it is the reason this reads two registers rather
//! than sweeping a range.

use super::uaccess;

/// In-repo: `mmu.rs` and `PL011_BASE` both derive from the BCM2711 low-peripheral base.
const PERIPHERAL_BASE: u64 = 0xFE00_0000;

/// SDHCI `CAPABILITIES` (spec 2.2.15) - read-only, and the most recognisable word a host controller has.
const SDHCI_CAPABILITIES: u64 = 0x40;
/// A 32-bit read here spans `SLOT_INT_STATUS` (0xFC, 16-bit) and `HOST_CONTROLLER_VERSION` (0xFE,
/// 16-bit). Both read-only; the version's low byte is the spec revision and is the sanity check.
const SDHCI_SLOTINT_VERSION: u64 = 0xFC;

/// One candidate window: what it is believed to be, where, and how well that belief is grounded.
struct Candidate {
    /// Offset from the peripheral base.
    off: u64,
    /// What this port expects to find, for the log line.
    what: &'static str,
    /// Is the offset an in-repo fact, or this author's recollection? Printed, because a reader deciding
    /// what to trust needs to know which.
    grounded: bool,
}

/// The two controllers worth asking about.
const CANDIDATES: &[Candidate] = &[
    Candidate {
        off: 0x30_0000,
        what: "Arasan - THE RADIO (dt: mmcnr@7e300000, bus-width 4, sdio_pins)",
        grounded: true,
    },
    Candidate {
        off: 0x34_0000,
        what: "emmc2 - the SD CARD (dt: emmc2@7e340000, brcm,bcm2711-emmc2)",
        grounded: true,
    },
];

/// Read one candidate and say what answered. Returns true if something plausibly SDHCI is there.
fn ask(c: &Candidate) -> bool {
    let phys = PERIPHERAL_BASE + c.off;

    // TRANSLATED, because peripherals are not reached at their physical address once this kernel has
    // relocated. mmio() adds MMIO_OFF, which mmio_go_high() sets to mmu::KERNEL_VA_BASE, and the low
    // map is retired afterwards - so a raw physical address is UNMAPPED and every read of one aborts.
    // genet.rs does exactly this; its reg() is the pattern.
    //
    // The first version read the physical address directly, and on the Pi 4 it reported NEITHER
    // candidate answering - which looked like a finding about the board and was a finding about this
    // function. Kept because the RESULT was the tell: this board boots from an SD card, so it has a
    // working SD host controller by construction and zero was never a possible true answer. An
    // implausible reading is an instrument to check before it is a fact to act on.
    let base = super::mmio(phys as usize) as u64;

    // PROBED, not read, for the reason `genet::probe` and `pcie::init` are: an address that decodes to
    // nothing is an external abort here, and an abort during boot surfaces later as an SError blaming
    // something unrelated.
    // SAFETY: 4-byte aligned, inside the peripheral Device mapping the kernel built at boot.
    let caps = unsafe { uaccess::probe_read32(base + SDHCI_CAPABILITIES) };
    // SAFETY: as above.
    let ver = unsafe { uaccess::probe_read32(base + SDHCI_SLOTINT_VERSION) };

    super::put_str(b"sdio: ");
    super::put_hex(phys);
    super::put_str(b" ");
    super::put_str(c.what.as_bytes());
    if !c.grounded {
        super::put_str(b" [UNVERIFIED ADDRESS]");
    }

    match (caps, ver) {
        (Some(cap), Some(v)) => {
            // PRESENCE IS THE VERSION REGISTER, NOT CAPABILITIES - and that is a correction, not a
            // preference. The first version of this test read CAPABILITIES alone and reported the
            // Arasan as "nothing there" on `CAPS=0x0` while its version register was answering
            // `0x99020000`. The device tree says a controller is there; the probe said otherwise; the
            // probe was wrong.
            //
            // `drivers/mmc/host/sdhci-iproc.c` says why: `bcm2835_data` carries
            // `.missing_caps = true` and supplies `.caps`/`.caps1` from the DRIVER, because the
            // hardware CAPABILITIES register cannot be relied upon on this family. So a zero there is
            // expected behaviour for this part, and building a presence test on it was building it on
            // the one register the silicon does not populate. (Behaviour cited per 26.14; no code
            // taken.)
            //
            // A 32-bit read at 0xFC spans SLOT_INT_STATUS (0xFC) and HOST_CONTROLLER_VERSION (0xFE), so
            // the upper half is the version: vendor in the high byte, SDHCI spec revision in the low.
            // A spec revision of 0, 1 or 2 (1.00 / 2.00 / 3.00) from a register that is neither all-zeros
            // nor all-ones is a controller identifying itself.
            let spec_rev = (v >> 16) & 0xFF;
            let present = v != 0 && v != 0xFFFF_FFFF && spec_rev <= 2;
            super::put_str(b" CAPS=");
            super::put_hex(cap as u64);
            super::put_str(b" VER=");
            super::put_hex(v as u64);
            if present {
                super::put_str(b" - A CONTROLLER ANSWERED");
                // Said, not silently tolerated: a zero here is normal for the Arasan and would be odd
                // for anything else, so it is worth a reader's attention either way.
                if cap == 0 {
                    super::put_str(
                        b" (CAPS reads 0 - expected on this part; sdhci-iproc supplies them in \
                          software)",
                    );
                }
                super::put_str(b"\r\n");
                true
            } else {
                super::put_str(b" - no plausible SDHCI version, nothing there\r\n");
                false
            }
        }
        _ => {
            // Said out loud rather than returned silently, for `genet::probe`'s reason: a reader who
            // finds no line cannot tell "absent" from "this code never ran".
            super::put_str(b" - the read ABORTED, no controller at this address\r\n");
            false
        }
    }
}

/// Census both candidates at boot and print what each says. Grants nothing.
///
/// Called from the pi4 boot path beside the GENET and PCIe probes, and gated to that board: the QEMU
/// `virt` variant has no Pi peripherals at all, so there is nothing to ask.
pub fn census() {
    super::put_str(b"sdio: BCM2711 SD host controller census - which one is the WiFi radio behind?\r\n");
    let mut found = 0u32;
    for c in CANDIDATES {
        if ask(c) {
            found += 1;
        }
    }
    // The COUNT is the finding, not either individual line, and the three cases mean different things.
    match found {
        // ZERO IS AN INSTRUMENT FAULT BEFORE IT IS A BOARD FACT, and this line exists because the
        // first version of this census printed it while the fault was its own address translation.
        // A Pi 4 boots from an SD card, so it has a working host controller by construction and zero
        // cannot be a true reading. Presenting an impossible reading as a board fact is what sends
        // the next reader down the wrong path.
        0 => super::put_str(
            b"sdio: NEITHER answered - but this board BOOTED FROM AN SD CARD, so zero controllers \
              is not a possible truth. Suspect THIS PROBE first (address translation, the probe \
              window) before concluding anything about the hardware\r\n",
        ),
        1 => super::put_str(
            b"sdio: exactly ONE answered - if that is the Arasan, sdhci.rs is pointed at the \
              right controller and the second address was simply wrong\r\n",
        ),
        _ => super::put_str(
            b"sdio: BOTH answered - two SD host controllers, which is what docs/wifi.md \
              section 4 predicts (card on one, radio on the other)\r\n",
        ),
    }
    super::put_str(
        b"sdio: this is a CENSUS ONLY - nothing is granted, no card or device is touched, and \
          which one holds the radio is still unproven (that needs CMD5)\r\n",
    );
}
