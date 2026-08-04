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
/// `SYS_PORT_CTRL`, which selects the interface the MAC drives. The Pi 4 has an external gigabit PHY.
const SYS_PORT_CTRL: u64 = GENET_SYS_OFF + 0x04;
const PORT_MODE_EXT_GPHY: u32 = 3;
#[allow(dead_code)]
const GENET_EXT_OFF: u64 = 0x0080;
#[allow(dead_code)]
const GENET_INTRL2_0_OFF: u64 = 0x0200;
#[allow(dead_code)]
const GENET_RBUF_OFF: u64 = 0x0300;
const GENET_UMAC_OFF: u64 = 0x0800;

// UniMAC registers. These live in Linux's shared `unimac.h`, NOT in `bcmgenet.h` - the GENET header
// only carries the blocks and the MDIO command. Looking for them in the obvious file finds nothing,
// which is worth writing down because the obvious conclusion ("this part has no UMAC_CMD") is wrong.
const UMAC_CMD: u64 = GENET_UMAC_OFF + 0x008;
const UMAC_MAC0: u64 = GENET_UMAC_OFF + 0x00C;
const UMAC_MAC1: u64 = GENET_UMAC_OFF + 0x010;
const UMAC_MAX_FRAME_LEN: u64 = GENET_UMAC_OFF + 0x014;
/// The MDIO command register.
///
/// **`0x614` is UMAC-RELATIVE, not absolute.** `bcmgenet.h` defines it as a bare `0x614`, which reads
/// like an offset from the register base - and I wrote it down that way. It is not: every `UMAC_*`
/// constant in that header is consumed by `bcmgenet_umac_writel()`, which adds `GENET_UMAC_OFF` before
/// touching the bus. The real address is `0x800 + 0x614 = 0xE14`, and the Pi 4's own device tree names
/// the node **`mdio@e14`**, which settles it beyond argument.
///
/// The wrong address did not fail loudly. The bus went idle, `READ_FAIL` stayed clear, and the read
/// returned a clean `0x0` - a well-formed answer from a register that is not the MDIO controller. An
/// offset taken from a header without checking how the header USES it is a whole class of this.
const UMAC_MDIO_CMD: u64 = GENET_UMAC_OFF + 0x614;

const CMD_TX_EN: u32 = 1 << 0;
const CMD_RX_EN: u32 = 1 << 1;
const CMD_SPEED_SHIFT: u32 = 2;
const CMD_SPEED_MASK: u32 = 3;
const CMD_PROMISC: u32 = 1 << 4;
const CMD_SW_RESET: u32 = 1 << 13;
const CMD_LCL_LOOP_EN: u32 = 1 << 15;
const CMD_TX_PAUSE_IGNORE: u32 = 1 << 28;
const CMD_RX_PAUSE_IGNORE: u32 = 1 << 8;

const MDIO_START_BUSY: u32 = 1 << 29;
const MDIO_READ_FAIL: u32 = 1 << 28;
const MDIO_RD: u32 = 2 << 26;
const MDIO_WR: u32 = 1 << 26;
const MDIO_PMD_SHIFT: u32 = 21;
const MDIO_REG_SHIFT: u32 = 16;

/// The largest frame the MAC will accept. 1536 is what Linux programs: a 1500-byte payload plus
/// headers, VLAN tag and FCS, rounded up.
const MAX_FRAME: u32 = 1536;

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

fn rd(off: u64) -> u32 {
    // SAFETY: a volatile read of a GENET register, in the kernel's Device mapping.
    unsafe { reg(off).read_volatile() }
}

fn wr(off: u64, v: u32) {
    // SAFETY: a volatile write of a GENET register, in the kernel's Device mapping.
    unsafe { reg(off).write_volatile(v) }
}

fn delay_us(us: u64) {
    let hz = super::timer::frequency().max(1);
    let ticks = (hz * us) / 1_000_000;
    let start = super::read_cycle_counter();
    while super::read_cycle_counter().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// One MDIO transaction against the external PHY. `None` if the bus does not answer.
///
/// MDIO is how the MAC talks to a PHY that lives on a separate chip - here a BCM54213PE. It is a
/// serial bus driven by one register: write the command with `START_BUSY` set, wait for the controller
/// to clear it, then read the low half for the result.
///
/// **`READ_FAIL` matters and is easy to miss.** A read of an absent PHY returns a perfectly plausible
/// `0xFFFF` with the fail bit set, and a driver that checks only the data believes the PHY answered
/// with every capability bit on. The wait is bounded (invariant 12): a bus that never clears BUSY must
/// not hang the boot.
fn mdio(phy: u32, reg_num: u32, write: Option<u16>) -> Option<u16> {
    let mut cmd = (phy << MDIO_PMD_SHIFT) | (reg_num << MDIO_REG_SHIFT) | MDIO_START_BUSY;
    cmd |= match write {
        Some(v) => MDIO_WR | v as u32,
        None => MDIO_RD,
    };
    wr(UMAC_MDIO_CMD, cmd);

    let mut n = 0;
    while n < 10_000 {
        if rd(UMAC_MDIO_CMD) & MDIO_START_BUSY == 0 {
            break;
        }
        delay_us(10);
        n += 1;
    }
    let done = rd(UMAC_MDIO_CMD);
    if done & MDIO_START_BUSY != 0 {
        return None; // the bus never went idle
    }
    if write.is_some() {
        return Some(0);
    }
    if done & MDIO_READ_FAIL != 0 {
        return None; // an absent PHY answers 0xFFFF with this set; the data alone would look valid
    }
    Some((done & 0xFFFF) as u16)
}

/// Reset the MAC and put it in a known, quiet state.
///
/// The reset bit is **self-clearing and must be given time**; Linux writes it, waits, then clears the
/// command register outright. Leaving TX or RX enabled through a reset is how a MAC comes back still
/// holding half a frame.
fn umac_reset() {
    wr(UMAC_CMD, CMD_SW_RESET);
    delay_us(10);
    wr(UMAC_CMD, 0);
    delay_us(10);
}

/// Program the station address the MAC filters on.
///
/// `MAC0` takes the first four bytes big-endian and `MAC1` the last two - not the little-endian layout
/// the rest of this file uses, so a byte-order slip here produces a MAC that looks right in a log and
/// matches nothing on the wire.
fn set_mac_address(mac: [u8; 6]) {
    wr(
        UMAC_MAC0,
        ((mac[0] as u32) << 24) | ((mac[1] as u32) << 16) | ((mac[2] as u32) << 8) | mac[3] as u32,
    );
    wr(UMAC_MAC1, ((mac[4] as u32) << 8) | mac[5] as u32);
}

/// Bring the MAC and the PHY up far enough to report the link. Frames come later, with the DMA rings.
///
/// Returns the PHY id read over MDIO, which is the milestone: a real id means the MAC is alive, the
/// MDIO bus is clocking, and the PHY is answering - three separate things that a later DMA failure
/// would otherwise be blamed for.
pub fn umac_init(mac: [u8; 6]) -> Option<u32> {
    // Drive the external gigabit PHY, not one of the internal modes. Wrong here and the MAC talks to
    // something that is not on this board.
    wr(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);

    umac_reset();
    wr(UMAC_MAX_FRAME_LEN, MAX_FRAME);
    set_mac_address(mac);

    // Ignore pause frames in both directions for now: flow control is a policy the stack above has no
    // way to express yet, and honouring pause without a stack that can act on it stalls transmission
    // for reasons nothing can explain. TX/RX stay DISABLED - this milestone brings up the MAC, not the
    // data path, and a receiver enabled with no ring behind it fills a FIFO nobody drains.
    let cmd = rd(UMAC_CMD) & !(CMD_TX_EN | CMD_RX_EN | CMD_PROMISC | CMD_LCL_LOOP_EN);
    wr(UMAC_CMD, cmd | CMD_TX_PAUSE_IGNORE | CMD_RX_PAUSE_IGNORE);

    // The PHY id lives in MII registers 2 and 3. An id of 0 or all-ones is the signature of nothing
    // answering, which is exactly what a mis-clocked MDIO bus looks like.
    let id_hi = mdio(1, 2, None)?;
    let id_lo = mdio(1, 3, None)?;
    let phy_id = ((id_hi as u32) << 16) | id_lo as u32;
    if phy_id == 0 || phy_id == 0xFFFF_FFFF {
        put_str(b"genet: MDIO answered but the PHY id is ");
        put_hex(phy_id as u64);
        put_str(b" - no PHY on the bus\r\n");
        return None;
    }

    put_str(b"genet: MAC configured, PHY id ");
    put_hex(phy_id as u64);
    put_str(b" (");
    // BCM54213PE reports 0x600d84a2; the low nibbles are a revision, so compare the model bits only.
    put_str(if phy_id & 0xFFFF_FFF0 == 0x600D_84A0 {
        b"BCM54213PE, the Pi 4's gigabit PHY" as &[u8]
    } else {
        b"unrecognised - the register map assumes a BCM54213PE" as &[u8]
    });
    put_str(b")\r\n");

    // Link state, from the basic status register (MII register 1, bit 2). Read TWICE: the link bit is
    // latching-low, so a single read reports a link that has been up all along as DOWN.
    let _ = mdio(1, 1, None);
    let bmsr = mdio(1, 1, None)?;
    put_str(if bmsr & (1 << 2) != 0 {
        b"genet: link is UP\r\n" as &[u8]
    } else {
        b"genet: link is down (no cable?)\r\n" as &[u8]
    });

    Some(phy_id)
}

/// The speed field the MAC needs, from the PHY's negotiated speed. Kept next to the register bits it
/// encodes so the two cannot drift; used when the data path is enabled.
#[allow(dead_code)]
fn cmd_speed(mbps: u32) -> u32 {
    let sel = match mbps {
        1000 => 2,
        100 => 1,
        _ => 0,
    };
    (sel & CMD_SPEED_MASK) << CMD_SPEED_SHIFT
}

// --- DMA rings: confirmed descriptor format, offsets still to be read ---------------------------
//
// These constants ARE verified against Linux's `bcmgenet.h`. The ring REGISTER offsets - the RDMA and
// TDMA block bases for v5, and the per-ring `DMA_START_ADDR` / `DMA_PROD_INDEX` / `DMA_CONS_INDEX` /
// `DMA_RING_BUF_SIZE` - are NOT yet read, and nothing below programs the hardware.
//
// That split is deliberate. Ring setup tells the controller where to WRITE into system memory, so a
// wrong base does not return a wrong value the way a bad MDIO offset did - it corrupts RAM. This driver
// has already had one offset taken from a `#define` without checking how the header used it
// (`UMAC_MDIO_CMD`, which is UMAC-relative); repeating that on a DMA base is not a read that comes back
// zero, it is a device writing frames into whatever happens to live there.
//
// So the format goes in now, where it is checked, and the addresses come with the code that uses them.

/// Descriptors per ring, and the ring register block stride. `TOTAL_DESC` is Linux's figure.
const TOTAL_DESC: usize = 256;
#[allow(dead_code)]
const DMA_RING_SIZE: u64 = 0x40;

/// One descriptor is three words: length+status, address low, address high.
#[allow(dead_code)]
const DMA_DESC_LENGTH_STATUS: u64 = 0x00;
#[allow(dead_code)]
const DMA_DESC_ADDRESS_LO: u64 = 0x04;
#[allow(dead_code)]
const DMA_DESC_ADDRESS_HI: u64 = 0x08;

// Bits in the length/status word. The buffer length sits in the UPPER half, which is the detail most
// likely to be got wrong by assuming a length field starts at bit 0.
#[allow(dead_code)]
const DMA_BUFLENGTH_MASK: u32 = 0x0FFF;
#[allow(dead_code)]
const DMA_BUFLENGTH_SHIFT: u32 = 16;
/// Ownership: set means the CONTROLLER owns the descriptor, not us.
#[allow(dead_code)]
const DMA_OWN: u32 = 0x8000;
#[allow(dead_code)]
const DMA_EOP: u32 = 0x4000;
#[allow(dead_code)]
const DMA_SOP: u32 = 0x2000;
#[allow(dead_code)]
const DMA_TX_QTAG_SHIFT: u32 = 7;

/// The master DMA enable, in each of the RDMA and TDMA control registers.
#[allow(dead_code)]
const DMA_EN: u32 = 1 << 0;
#[allow(dead_code)]
const DMA_RING_BUF_EN_SHIFT: u32 = 1;

// --- Ring register map, and proving the base addresses before trusting them ---------------------

/// Per-ring register offsets, verified from Linux's `genet_dma_ring_regs_v4` table (v4 and v5 share
/// it). A ring's register address is `block_base + index * DMA_RING_SIZE + reg`.
const DMA_RING_BUF_SIZE: u64 = 0x10;
const DMA_START_ADDR: u64 = 0x14;
const DMA_END_ADDR: u64 = 0x1C;
const TDMA_PROD_INDEX: u64 = 0x0C;
const RDMA_CONS_INDEX: u64 = 0x08;
#[allow(dead_code)]
const DMA_MBUF_DONE_THRESH: u64 = 0x24;

/// Words per descriptor on v4/v5: length+status, address low, address high.
const WORDS_PER_BD: u64 = 3;
/// The descriptor area occupies the start of each DMA block; the ring CONTROL REGISTERS follow it.
///
/// **This is what the last "bases confirmed" line actually missed.** `bcmgenet.h` spells the register
/// base as `rdma_offset + TOTAL_DESC * WORDS_PER_BD * sizeof(u32)` - so `0x2000` is where the
/// DESCRIPTORS live, and the registers begin `0xC00` further on. The write-readback test passed at
/// `0x2000 + 0x14` for a perfectly good reason that was not the one claimed: descriptor memory is
/// read/write, so a pattern written there reads back intact whether or not it is a ring register.
///
/// The test proved the block exists and is writable. It did NOT prove the register offset, and saying
/// it did would have put every subsequent ring write 0xC00 low - into descriptor storage, silently, with
/// the controller later fetching descriptors from whatever those writes displaced.
const DMA_REGS_OFF: u64 = (TOTAL_DESC as u64) * WORDS_PER_BD * 4;

/// The RX and TX DMA block bases for GENET v4/v5.
///
/// **These are the one thing in this file not confirmed from source in the session that wrote it.**
/// They come from `bcmgenet_hw_params`, which two attempts to extract did not surface, and this driver
/// has already been bitten twice by an address recalled rather than read (`UMAC_MDIO_CMD`, and the PCIe
/// `BASE_LIMIT` field order before it).
///
/// So they are treated as a HYPOTHESIS the hardware gets to refute. [`verify_dma_base`] writes a known
/// pattern to a ring register, reads it back, and restores it - and **nothing enables DMA unless that
/// readback matches**. A wrong base then costs a printed line instead of a controller writing frames
/// into whatever lives at that address, which on a board with no IOMMU is the whole of memory.
const RDMA_OFFSET: u64 = 0x2000;
const TDMA_OFFSET: u64 = 0x4000;

/// Address of one ring CONTROL register - past the descriptor area, not at the block base.
fn ring_reg(block: u64, index: u64, reg: u64) -> u64 {
    block + DMA_REGS_OFF + index * DMA_RING_SIZE + reg
}

/// Address of descriptor `n`'s first word, at the START of the block.
fn desc_word(block: u64, n: u64, word: u64) -> u64 {
    block + n * WORDS_PER_BD * 4 + word
}

/// Does this block base actually address a DMA ring?
///
/// Writes a pattern to a harmless ring register (the start address of a ring nothing has enabled),
/// reads it back, and restores whatever was there. A base that points at the wrong block reads back
/// something other than what was written - or reads back zero, which is the common case for a register
/// that is not there at all.
///
/// This is the check the MDIO offset did not have. `0x614` produced a clean `0x0` with every error bit
/// clear, and there was nothing to catch it because a read alone cannot tell a wrong address from a
/// register that legitimately holds zero. A WRITE-then-read can.
fn verify_dma_base(block: u64, name: &[u8]) -> bool {
    // Ring 0's start address, in the CONTROL register area past the descriptors. The previous version
    // of this test addressed the descriptor area instead and passed on writable memory rather than on a
    // register - a pass for the wrong reason, which is the failure mode this whole check exists against.
    let addr = ring_reg(block, 0, DMA_START_ADDR);
    let saved = rd(addr);
    const PATTERN: u32 = 0x0000_5A5A;
    wr(addr, PATTERN);
    let got = rd(addr);
    wr(addr, saved);
    if got != PATTERN {
        put_str(b"genet: ");
        put_str(name);
        put_str(b" base does not behave like a DMA ring (wrote 0x5a5a, read ");
        put_hex(got as u64);
        put_str(b") - NOT enabling DMA\r\n");
        return false;
    }
    true
}

/// Prove the DMA block bases on this hardware, without enabling anything.
///
/// The milestone deliberately stops at proof. Ring setup tells the controller where to WRITE into
/// system memory, so the address has to be established before a single buffer is handed over - and
/// establishing it is a self-contained thing that one boot can answer.
pub fn verify_dma_layout() -> bool {
    let rx_ok = verify_dma_base(RDMA_OFFSET, b"RDMA");
    let tx_ok = verify_dma_base(TDMA_OFFSET, b"TDMA");
    if rx_ok && tx_ok {
        put_str(b"genet: DMA ring registers respond at RDMA 0x2c00 / TDMA 0x4c00 \
                  (descriptors at 0x2000 / 0x4000) - bases confirmed\r\n");
    }
    // Leave both blocks disabled either way. Nothing here has given the controller a buffer, and a
    // controller enabled without one is a device writing to whatever its registers happen to hold.
    rx_ok && tx_ok
}

/// The ring geometry the next stage will program, kept here so the readback above and the setup below
/// cannot drift apart. Unused until frames are wired up.
#[allow(dead_code)]
fn describe_ring(block: u64, index: u64) -> (u64, u64, u64, u64) {
    (
        ring_reg(block, index, DMA_RING_BUF_SIZE),
        ring_reg(block, index, DMA_START_ADDR),
        ring_reg(block, index, DMA_END_ADDR),
        ring_reg(block, index, if block == TDMA_OFFSET { TDMA_PROD_INDEX } else { RDMA_CONS_INDEX }),
    )
}
