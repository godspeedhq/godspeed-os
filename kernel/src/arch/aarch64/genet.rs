// SPDX-License-Identifier: GPL-2.0-only
//! The BCM2711's built-in Ethernet controller (GENET), milestone 1: find it and identify it.
//!
//! ## Why the Pi 4's networking is nothing like the Pi 2's
//!
//! The Pi 2 has no Ethernet MAC at all - its network port hangs off a LAN9514 behind the USB hub, which
//! is why that port's networking rides on the in-kernel DWC2 stack. The Pi 4 has a **real MAC on the
//! SoC**: GENET v5 at `0xFD58_0000`, with its own DMA rings and an external RGMII PHY (a BCM54213PE)
//! reached over MDIO. Nothing about the Pi 2's network path transfers; only the seam above it does.
//!
//! ## What this milestone does, and why it stops here
//!
//! It reads `SYS_REV_CTRL` and reports the controller's revision. That is a small thing to ship on its
//! own, and it is the right first step for the same reason the mailbox was: it proves the MMIO window
//! is mapped and decoding **before** any of the driver above it can blame its own logic for a dead
//! read. The PCIe bring-up on this board spent four hardware rounds on a window that was silently not
//! forwarding, and every one of those rounds was spent looking at the driver instead.
//!
//! The revision also picks the register layout: GENET v1..v5 move the DMA rings and rename fields, so a
//! driver written against v5 offsets and run on a v3 part reads plausible values from the wrong places.
//! Confirming v5 on the board is what makes the rest of the map safe to trust.
//!
//! ## What comes next
//!
//! UMAC configuration and the MAC address, MDIO to bring up the external PHY, then the TX and RX DMA
//! rings, then the bridge to the existing `nic-driver` through the `NET_DEVICE` syscalls (42-44) - the
//! same seam the 32-bit port already uses, so `net-stack` and everything above it needs no changes.
//! Written against Linux's `drivers/net/ethernet/broadcom/genet/` as an executable datasheet, per the
//! doctrine in `arch/CLAUDE.md`, and read before writing rather than after failing.

use super::{put_hex, put_str};

/// GENET register base on the BCM2711 (low-peripheral mode).
const GENET_BASE: u64 = 0xFD58_0000;

// Block offsets within the register file. These are the values Linux's `bcmgenet.h` defines, and they
// are the same across v1..v5 - it is the contents that move, not the blocks.
const GENET_SYS_OFF: u64 = 0x0000;
#[allow(dead_code)]
const GENET_EXT_OFF: u64 = 0x0080;
#[allow(dead_code)]
const GENET_INTRL2_0_OFF: u64 = 0x0200;
#[allow(dead_code)]
const GENET_RBUF_OFF: u64 = 0x0300;
#[allow(dead_code)]
const GENET_UMAC_OFF: u64 = 0x0800;

/// `SYS_REV_CTRL`, the first register in the SYS block.
const SYS_REV_CTRL: u64 = GENET_SYS_OFF + 0x00;

/// What the probe found.
#[derive(Clone, Copy)]
pub struct GenetInfo {
    /// Major version, already normalised (the raw field encodes v4 as 5 and v5 as 6).
    pub major: u32,
    pub minor: u32,
    pub raw: u32,
}

fn reg(off: u64) -> *mut u32 {
    super::mmio((GENET_BASE + off) as usize) as *mut u32
}

/// Find the controller and report which revision it is.
///
/// Returns `None` - loudly - when nothing answers, rather than letting a later stage discover it. A
/// board with no GENET is a board with no network, which is a degradation the machine can carry.
pub fn probe() -> Option<GenetInfo> {
    // Probed rather than read, for the reason the PCIe root complex is: an address that decodes to
    // nothing does not fault here, and on a machine without this controller the read would be an
    // external abort that surfaces later as an SError blaming something unrelated.
    // SAFETY: 4-byte aligned, inside the peripheral Device mapping the kernel built.
    let Some(raw) = (unsafe { super::uaccess::probe_read32(reg(SYS_REV_CTRL) as u64) }) else {
        // `?` here would have returned silently, which contradicts the sentence directly above it. A
        // machine with no network needs to say so once; a reader who finds no `genet:` line at all
        // cannot tell "absent" from "this code never ran".
        put_str(b"genet: no controller at 0xFD580000 (the read aborted) - no on-board ethernet\r\n");
        return None;
    };

    // All-ones and all-zeros are the two ways "nothing is there" presents. Neither is a revision.
    if raw == 0 || raw == 0xFFFF_FFFF {
        put_str(b"genet: no controller at 0xFD580000 (read ");
        put_hex(raw as u64);
        put_str(b") - no on-board ethernet\r\n");
        return None;
    }

    // The revision field encoding, from Linux's `bcmgenet_probe`: bits 27:24 hold the major, offset by
    // one from v4 onward (4 means v4 is reported as 5, 5 as 6), and bits 19:16 hold the minor. The
    // offset is not a detail to skip - reading the raw field gives a version number one higher than the
    // part actually is, and picking a register layout from that is how a driver ends up addressing the
    // wrong block on the right chip.
    let mut major = (raw >> 24) & 0x0F;
    major = match major {
        6 => 5,
        5 => 4,
        0 => 1,
        other => other,
    };
    let minor = (raw >> 16) & 0x0F;

    put_str(b"genet: GENET v");
    super::put_dec(major as u64);
    put_str(b".");
    super::put_dec(minor as u64);
    put_str(b" at 0xFD580000 (rev ");
    put_hex(raw as u64);
    put_str(b")\r\n");

    if major != 5 {
        // Not fatal - reporting it is the point. The register map this driver will be written against
        // is v5's, and a different part means the map is wrong in ways that read as plausible values.
        put_str(b"genet: WARNING expected v5 on a BCM2711 - the register map is written for v5\r\n");
    }

    Some(GenetInfo { major, minor, raw })
}
