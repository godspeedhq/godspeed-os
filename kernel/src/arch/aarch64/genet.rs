// SPDX-License-Identifier: GPL-2.0-only
//! GENET DISCOVERY for the Raspberry Pi 4 (BCM2711) - and nothing else.
//!
//! This file was a ~1500-line Ethernet driver: MDIO, UMAC bring-up, PHY clock delays, RX and TX
//! descriptor rings, the address filter, the frame data path. All of it ran in ring 0, parsing
//! frames that arrive unbidden from anywhere on the network. That broke Commandment I - "Thou
//! shalt not expand the responsibilities of the kernel. It is complete. Use a service." - and
//! §4.4, which says the kernel contains no drivers.
//!
//! The driver now lives in `services/nic-driver/src/genet.rs` as a restartable userspace service
//! with NO `unsafe`, reaching the controller only through an MMIO capability and a DMA arena
//! granted by name. Hardware-proven: PHY at 1000 Mbit, DHCP, ARP, SNTP, ping at 0% loss - and
//! chaos killed it 46 times during a carnage run while the machine stayed up. That last part is
//! the property the kernel copy could never have had: a fault in a kernel driver is a dead
//! machine, a fault in a service is a restart.
//!
//! ## Why anything remains here
//!
//! DISCOVERY is the kernel's job; DRIVING is not. The kernel must know a controller is present
//! before it can grant its register window to anybody - the same reason x86 enumerates PCI. So
//! what is left reads exactly one register and writes none, which is the honest boundary between
//! "the kernel knows what hardware exists" and "the kernel operates the hardware".
//!
//! ## The claim that kept the driver here
//!
//! The violation rested on "ARM does not yet route device IRQs to userspace". That was never a
//! hardware limit. It was four pieces of unwritten code - an unwired branch in the GIC handler, a
//! grant returning None, a flag nobody set for a non-PCI NIC, and the port itself. Three were one
//! line each. When a violation is justified by "the platform cannot do X yet", VERIFY THAT CLAIM.

use super::{put_hex, put_str};

/// GENET register base on the BCM2711 (low-peripheral mode).
const GENET_BASE: u64 = 0xFD58_0000;
/// `SYS_REV_CTRL`, the first register in the SYS block - the only one this file reads.
const SYS_REV_CTRL: u64 = 0x0000;

/// Whether a controller answered the probe. Gates the register-window grant, so it records what
/// the HARDWARE said: handing a service a window to a device that is not there makes its first
/// register read an external abort, which userspace cannot survive - it would respawn forever.
static GENET_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub struct GenetInfo {
    /// Major version, already normalised (the raw field encodes v4 as 5 and v5 as 6).
    pub major: u32,
    pub minor: u32,
    pub raw: u32,
}

fn reg(off: u64) -> *mut u32 {
    super::mmio((GENET_BASE + off) as usize) as *mut u32
}

/// Did a controller answer, and may the grant name its window?
pub fn present() -> bool {
    GENET_PRESENT.load(core::sync::atomic::Ordering::Acquire)
}

/// Find the controller and report which revision it is.
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

    // A real controller answered. Record it before anything else, because the register-window grant
    // (see `GENET_PRESENT`) depends on the answer and not on whether this kernel goes on to drive it.
    GENET_PRESENT.store(true, core::sync::atomic::Ordering::Release);
    Some(GenetInfo { major, minor, raw })
}
