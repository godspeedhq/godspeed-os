// SPDX-License-Identifier: GPL-2.0-only
//! The BCM2711's built-in Ethernet MAC (Broadcom GENET v5), driven from USERSPACE.
//!
//! ## Why this file exists
//!
//! This is `kernel/src/arch/aarch64/genet.rs` where it belongs. That file is about 1500 lines of
//! ethernet driver running in ring 0, which Commandment I forbids in one sentence ("thou shalt not
//! expand the responsibilities of the kernel - it is complete; use a service") and §4.4 forbids again
//! by name ("the kernel does not contain ... a network stack, drivers"). x86 has never had the
//! problem: its NIC driver has always been a restartable, IOMMU-confinable userspace service. This is
//! aarch64 catching up, and the port is possible only because three things landed first - device IRQs
//! now route to userspace, `nic-driver` is granted the GENET register window by name, and its DMA
//! arena is mapped UNCACHED because AArch64 DMA is not coherent.
//!
//! ## What is the same, and what had to change
//!
//! The hardware facts are the whole asset and every one of them is carried over verbatim, comment and
//! all, because each cost a hardware round: the MAC ships held in software reset; `RGMII_MODE_EN` must
//! be set or not one bit crosses; the DMA control registers sit `DMA_RINGS_SIZE` past the ring
//! registers, which sit `DMA_REGS_OFF` past the descriptors; the PHY's internal transmit delay must be
//! disabled for `rgmii-rxid`. What changed is only how the driver reaches the silicon:
//!
//! - **Registers** go through the SDK's `Mmio` wrapper instead of raw volatile pointers, so this file
//!   contains no `unsafe` at all (§18.2). Offsets are relative to the granted window rather than to an
//!   absolute physical base; the numbers are otherwise identical.
//! - **Packet buffers** live in the 64 KiB DMA arena instead of frames from the kernel's allocator,
//!   which is what fixes the ring sizes below at something a reader can add up (§26.6.1).
//! - **Cache maintenance is gone**, because the arena is mapped Device-nGnRnE by the spawn path. The
//!   kernel driver had to `dma_sync` every buffer and had one vicious bug from getting it wrong (a
//!   `dc civac` writing a stale line back over a freshly-DMA'd frame). An uncached mapping removes the
//!   question rather than resting on the driver author remembering, which is exactly what SEC-28 asks
//!   a non-coherent port to do.
//! - **Waits are bounded by real time**, read from the same counter the kernel's `delay_us` used, but
//!   through `ctx.read_tsc()`. A bound has to mean what it says: an iteration count is not a duration,
//!   and the only place one appears here is the fallback for a machine that reports no calibration -
//!   where it is named as what it is.
//!
//! Written against Linux's `drivers/net/ethernet/broadcom/genet/` as an executable datasheet, per the
//! doctrine in `kernel/src/arch/CLAUDE.md`: the C driver says what the silicon wants, and we implement
//! that want as a capability service.

use godspeed_sdk::{Dma, Message, Mmio, ServiceContext};

// ---------------------------------------------------------------------------------------------
// Register map. Byte offsets into the 64 KiB window `ctx.mmio()` hands us, which the spawn path
// mapped to physical 0xFD58_0000. Block offsets are the values Linux's `bcmgenet.h` defines and are
// the same across v1..v5 - it is the contents that move, not the blocks.
// ---------------------------------------------------------------------------------------------

const GENET_SYS_OFF: usize = 0x0000;
/// `SYS_REV_CTRL`, the first register in the SYS block.
const SYS_REV_CTRL: usize = GENET_SYS_OFF + 0x00;
/// `SYS_PORT_CTRL`, which selects the interface the MAC drives. The Pi 4 has an external gigabit PHY.
const SYS_PORT_CTRL: usize = GENET_SYS_OFF + 0x04;
/// `SYS_RBUF_FLUSH_CTRL`. This register holds a `umac_sw_rst` bit, and **the part powers up with that
/// bit set** - Linux clears it as the first action of `reset_umac`, under the comment "7358a0/7552a0:
/// bad default in RBUF_FLUSH_CTRL.umac_sw_rst".
///
/// While it is set the MAC is held in software reset and `UMAC_CMD` silently discards every write,
/// reading back zero forever. MDIO keeps working throughout, because it is clocked separately - so the
/// failure looks like "the MAC is fine but refuses to receive" rather than "the MAC is in reset". That
/// combination cost several boots; the register is worth its comment.
///
/// Note this is in the **SYS** block on GENET v2 and later, not the RBUF block whose name it carries -
/// `bcmgenet_rbuf_ctrl_set` only writes `RBUF_FLUSH_CTRL_V1` on v1 silicon.
const SYS_RBUF_FLUSH_CTRL: usize = GENET_SYS_OFF + 0x08;
const PORT_MODE_EXT_GPHY: u32 = 3;

/// The EXT block, which holds the RGMII interface controls.
const GENET_EXT_OFF: usize = 0x0080;
/// `EXT_RGMII_OOB_CTRL`. This register switches on the RGMII block itself - the parallel data path
/// between the MAC and an external PHY.
///
/// Nothing else on the receive path substitutes for it. MDIO is a separate management bus, so the PHY
/// can negotiate, report 1000 Mbit and assert link while this register is clear and NOT ONE BIT of
/// frame data crosses to the MAC. That is precisely what the MAC's counters showed: rx_pkt 0,
/// broadcast 0, and fcs_err 0 - no frames, and not even damaged ones.
const EXT_RGMII_OOB_CTRL: usize = GENET_EXT_OFF + 0x0C;
const OOB_DISABLE: u32 = 1 << 5;
const RGMII_MODE_EN: u32 = 1 << 6;
/// Disables the internal RGMII delay. The Pi 4 runs `rgmii-rxid` (delay on receive), so this stays
/// CLEAR - Linux sets it only for plain `rgmii`, where the board provides the delay instead.
const ID_MODE_DIS: u32 = 1 << 16;

const GENET_UMAC_OFF: usize = 0x0800;

// UniMAC registers. These live in Linux's shared `unimac.h`, NOT in `bcmgenet.h` - the GENET header
// only carries the blocks and the MDIO command. Looking for them in the obvious file finds nothing,
// which is worth writing down because the obvious conclusion ("this part has no UMAC_CMD") is wrong.
const UMAC_CMD: usize = GENET_UMAC_OFF + 0x008;
const UMAC_MAC0: usize = GENET_UMAC_OFF + 0x00C;
const UMAC_MAC1: usize = GENET_UMAC_OFF + 0x010;
const UMAC_MAX_FRAME_LEN: usize = GENET_UMAC_OFF + 0x014;

// The MAC's own statistics counters. These are the measurement that splits the receive path in half:
// they count what the MAC itself took off the wire, BEFORE the RBUF and the DMA see any of it. A
// receive path that reports nothing tells you where to look only if you can ask each half separately,
// and the ring can only ever speak for the DMA end.
const UMAC_MIB_CTRL: usize = GENET_UMAC_OFF + 0x580;
const MIB_RESET_RX: u32 = 1 << 0;
const MIB_RESET_RUNT: u32 = 1 << 1;
const MIB_RESET_TX: u32 = 1 << 2;
/// Received packet count. Linux's `bcmgenet_rx_counters` places it after the ten packet-size buckets,
/// which is why it sits at `MIB_START + 0x28` rather than at the start of the block.
const UMAC_MIB_RX_PKT: usize = GENET_UMAC_OFF + 0x428;
/// Received broadcast count. On an idle network this is the counter that moves first.
const UMAC_MIB_RX_BCA: usize = GENET_UMAC_OFF + 0x434;
/// FCS errors: frames that arrived and failed their checksum. Nonzero here means the wire and the PHY
/// are delivering bits and something about how we clock or frame them is wrong.
const UMAC_MIB_RX_FCS: usize = GENET_UMAC_OFF + 0x438;
/// Receive overflow: the MAC took frames the downstream could not drain. Nonzero here would mean the
/// MAC is fine and the RBUF or DMA is the blockage.
const UMAC_MIB_RX_OVR: usize = GENET_UMAC_OFF + 0x458;
/// Transmitted packet count. The MAC's own tally, which is a different claim from the DMA consumer
/// index: that index says the engine took our descriptor, this says the MAC put a frame on the wire.
/// Linux's `bcmgenet_tx_counters` places `pkts` at `0x4a8`, past the whole RX block.
const UMAC_MIB_TX_PKT: usize = GENET_UMAC_OFF + 0x4A8;

/// The MDIO command register.
///
/// **`0x614` is UMAC-RELATIVE, not absolute.** `bcmgenet.h` defines it as a bare `0x614`, which reads
/// like an offset from the register base. It is not: every `UMAC_*` constant in that header is
/// consumed by `bcmgenet_umac_writel()`, which adds `GENET_UMAC_OFF` before touching the bus. The real
/// address is `0x800 + 0x614 = 0xE14`, and the Pi 4's own device tree names the node **`mdio@e14`**,
/// which settles it beyond argument.
///
/// The wrong address did not fail loudly. The bus went idle, `READ_FAIL` stayed clear, and the read
/// returned a clean `0x0` - a well-formed answer from a register that is not the MDIO controller. An
/// offset taken from a header without checking how the header USES it is a whole class of this.
const UMAC_MDIO_CMD: usize = GENET_UMAC_OFF + 0x614;

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
/// The external PHY's MDIO address on this board.
const PHY_ADDR: u32 = 1;

/// The largest frame the MAC will accept. 1536 is what Linux programs: a 1500-byte payload plus
/// headers, VLAN tag and FCS, rounded up.
const MAX_FRAME: u32 = 1536;

/// The destination-address filter. Two registers per address, UMAC-relative.
const UMAC_MDF_CTRL: usize = GENET_UMAC_OFF + 0x650;
const UMAC_MDF_ADDR: usize = GENET_UMAC_OFF + 0x654;
/// Filters enable from the TOP bit down: filter `i` is enabled by bit `MAX_MDF_FILTER - 1 - i`, which
/// is what Linux's `GENMASK(MAX_MDF_FILTER - 1, MAX_MDF_FILTER - nfilter)` computes. Reading that as
/// "bit i enables filter i" programs the addresses correctly and enables the wrong ones.
const MAX_MDF_FILTER: u32 = 17;

// ---------------------------------------------------------------------------------------------
// The DMA blocks: descriptors first, then the per-ring registers, then the block-wide controls.
// Three offsets stacked in that order, and getting any of them wrong produces confident fiction
// rather than an error, because every address involved is writable memory or a writable register.
// ---------------------------------------------------------------------------------------------

/// Descriptors per ring in the HARDWARE's own table, and the ring register block stride.
/// `TOTAL_DESC` is Linux's figure and is a property of the silicon, not of how many we use.
const TOTAL_DESC: usize = 256;
const DMA_RING_SIZE: usize = 0x40;

/// The default receive/transmit queue. GENET numbers its priority queues 0..15 and puts the catch-all
/// at 16; a single-queue driver uses that one, as Linux does.
const RING_INDEX: usize = 16;

/// The space the 17 per-ring register blocks occupy: rings 0 through `RING_INDEX` inclusive.
///
/// The block-wide DMA control registers (`DMA_RING_CFG`, `DMA_CTRL`, `DMA_STATUS`,
/// `DMA_SCB_BURST_SIZE`) live **after** those blocks, not at the start of the register area. Omitting
/// this lands every one of them inside RING 0's registers, which are perfectly writable and read back
/// whatever was written - so the mistake produces confident, entirely fictional readings rather than an
/// error. That is what happened: "ring_cfg 0x10000" was ring 0's write pointer, "dma_status 0x0" was
/// ring 0's producer index, and the write that was supposed to enable the receive DMA went into ring
/// 0's write-pointer high word. The engine was never switched on, and four rounds of diagnostics
/// reported it running.
const DMA_RINGS_SIZE: usize = DMA_RING_SIZE * (RING_INDEX + 1);

/// Words per descriptor on v4/v5: length+status, address low, address high.
const WORDS_PER_BD: usize = 3;
/// The descriptor area occupies the start of each DMA block; the ring CONTROL REGISTERS follow it.
///
/// `bcmgenet.h` spells the register base as `rdma_offset + TOTAL_DESC * WORDS_PER_BD * sizeof(u32)` -
/// so `0x2000` is where the DESCRIPTORS live, and the registers begin `0xC00` further on. A
/// write-readback test at `0x2000 + 0x14` passes for a reason that is not the one it looks like:
/// descriptor memory is read/write, so a pattern written there reads back intact whether or not it is
/// a ring register. The test proves the block exists and is writable. It does NOT prove the register
/// offset, and believing it did would put every subsequent ring write 0xC00 low - into descriptor
/// storage, silently, with the controller later fetching descriptors from whatever those writes
/// displaced.
const DMA_REGS_OFF: usize = TOTAL_DESC * WORDS_PER_BD * 4;

/// The RX and TX DMA block bases for GENET v4/v5.
const RDMA_OFFSET: usize = 0x2000;
const TDMA_OFFSET: usize = 0x4000;

/// One descriptor is three words: length+status, address low, address high.
const DMA_DESC_LENGTH_STATUS: usize = 0x00;
const DMA_DESC_ADDRESS_LO: usize = 0x04;
const DMA_DESC_ADDRESS_HI: usize = 0x08;

// Bits in the length/status word. The buffer length sits in the UPPER half, which is the detail most
// likely to be got wrong by assuming a length field starts at bit 0.
const DMA_BUFLENGTH_MASK: u32 = 0x0FFF;
const DMA_BUFLENGTH_SHIFT: u32 = 16;
const DMA_EOP: u32 = 0x4000;
const DMA_SOP: u32 = 0x2000;
const DMA_TX_QTAG_SHIFT: u32 = 7;
/// Let the MAC append the frame check sequence, so the driver never computes a CRC.
const DMA_TX_APPEND_CRC: u32 = 1 << 6;
/// The v5 QTAG mask, from this part's `hw_params`.
const DMA_TX_QTAG_MASK: u32 = 0x3F;

/// The master DMA enable, in each of the RDMA and TDMA control registers.
const DMA_EN: u32 = 1 << 0;
const DMA_RING_BUF_EN_SHIFT: u32 = 1;

// Per-ring register offsets, verified from Linux's `genet_dma_ring_regs_v4` table (v4 and v5 share
// it). A ring's register address is `block_base + DMA_REGS_OFF + index * DMA_RING_SIZE + reg`.
const DMA_RING_BUF_SIZE: usize = 0x10;
const DMA_START_ADDR: usize = 0x14;
const DMA_END_ADDR: usize = 0x1C;
const DMA_MBUF_DONE_THRESH: usize = 0x24;

// The ring POSITION registers. Like the index pair, these share offsets between the two directions and
// swap meaning: `RDMA_WRITE_PTR` and `TDMA_READ_PTR` are both 0x00, `RDMA_READ_PTR` and
// `TDMA_WRITE_PTR` are both 0x2C. Whoever writes the data owns the write pointer - the hardware on
// receive, the driver on transmit - so the same offset is the hardware's register on one ring and ours
// on the other. Reading the header down the wrong column is how the index pair was mis-named earlier.
const RDMA_WRITE_PTR: usize = 0x00;
const RDMA_WRITE_PTR_HI: usize = 0x04;
const RDMA_READ_PTR: usize = 0x2C;
const RDMA_READ_PTR_HI: usize = 0x30;
const TDMA_READ_PTR: usize = 0x00;
const TDMA_READ_PTR_HI: usize = 0x04;
const TDMA_WRITE_PTR: usize = 0x2C;
const TDMA_WRITE_PTR_HI: usize = 0x30;
/// Rate control. Zero disables it, which is what a driver with no shaping policy wants.
const TDMA_FLOW_PERIOD: usize = 0x28;
/// Flow-control thresholds, packed XOFF in the upper half and XON in the lower.
const RDMA_XON_XOFF_THRESH: usize = 0x28;

/// The index registers are paired by DIRECTION, and the pairing is easy to read backwards:
///
///     [TDMA_CONS_INDEX] = 0x08,   [RDMA_PROD_INDEX] = 0x08,
///     [TDMA_PROD_INDEX] = 0x0C,   [RDMA_CONS_INDEX] = 0x0C,
///
/// Whoever WRITES data owns the producer index. On TX that is the driver; on RX it is the hardware. So
/// the same offset is the driver's register on one ring and the hardware's on the other, and naming
/// `0x08` "RDMA_CONS_INDEX" inverts exactly that. The consequence upstream was not a wrong value but a
/// missing initialisation: the ring was armed with one end of it never set.
const TDMA_CONS_INDEX: usize = 0x08;
const RDMA_PROD_INDEX: usize = 0x08;
const TDMA_PROD_INDEX: usize = 0x0C;
const RDMA_CONS_INDEX: usize = 0x0C;

/// Linux computes these from its 256-descriptor pool: `DMA_FC_THRESH_HI` is `TOTAL_DESC >> 4` and
/// `DMA_FC_THRESH_LO` is a flat 5.
const DMA_FC_THRESH_HI: u32 = (TOTAL_DESC >> 4) as u32;
const DMA_FC_THRESH_LO: u32 = 5;
const DMA_XOFF_THRESHOLD_SHIFT: u32 = 16;

/// DMA control registers, from Linux's `bcmgenet_dma_regs_v3plus` table. Offsets into the ring-register
/// area past the ring blocks, not into the block base.
const DMA_RING_CFG: usize = 0x00;
const DMA_CTRL: usize = 0x04;
const DMA_STATUS: usize = 0x08;
/// `DMA_SCB_BURST_SIZE`, from the same verified table. Linux programs it as part of DMA init, and it
/// was one of the two things missing from the first receive enable that made it receive nothing.
const DMA_SCB_BURST_SIZE: usize = 0x0C;
/// Linux's `DMA_MAX_BURST_LENGTH`.
const DMA_MAX_BURST_LENGTH: u32 = 0x08;
/// `DMA_STATUS` bit 0 reads back set while the engine is STOPPED.
const DMA_DISABLED: u32 = 1 << 0;
/// `DMA_INDEX2RING_0`, the first of eight filter-to-ring steering registers in the RDMA block.
const DMA_INDEX2RING_0: usize = 0x70;

// The hardware filter block. It sits between the MAC and the receive DMA and drops what it is
// configured to drop, so a block left enabled with somebody else's filters discards frames that the
// MAC accepted and the DMA is waiting for - silently, and with every register upstream reading healthy.
//
// It is NOT safe to assume it is at its reset default: the Pi 4 firmware drives GENET itself (the board
// can netboot), and this port has already been bitten once by firmware leaving a block in a non-default
// state - the UMAC arrives held in software reset. Linux clears HFB unconditionally at init.
const HFB_OFF: usize = 0x8000;
const HFB_REG_OFF: usize = 0xFC00;
const HFB_CTRL: usize = 0x00;
const HFB_FLT_ENABLE_V3PLUS: usize = 0x04;
const HFB_FLT_LEN_V3PLUS: usize = 0x1C;
const HFB_FILTER_CNT: usize = 48;
const HFB_FILTER_SIZE: usize = 128;

// --- PHY: the BCM54213PE ---------------------------------------------------------------------
//
// RGMII carries data and its clock on separate traces, and the receiver samples the data against that
// clock. If the two are not skewed relative to each other by about 2 ns, sampling lands on the
// transition instead of the middle of the eye and frames arrive corrupt. Nothing about this shows up in
// link state: autonegotiation, the link bit, the speed, and the whole MDIO management bus are
// unaffected, because MDIO is a separate low-speed bus. The symptom is a perfect 1000 Mbit link that
// nobody answers.
//
// The Pi 4 runs `rgmii-rxid`: the PHY adds the RECEIVE delay, and the transmit delay is NOT the PHY's to
// add. Linux configures this explicitly in `bcm54xx_config_clock_delay` for every RGMII variant, which
// is the difference between a Pi 4 that gets a DHCP lease under Raspberry Pi OS and one that does not.

/// The BCM54213PE's auxiliary status register, where the NEGOTIATED mode lands once auto-negotiation
/// finishes. The standard MII registers say what both ends can do; this says what they agreed on.
const BCM_AUX_STATUS: u32 = 0x19;
/// Auxiliary control register, and the shadow selector within it.
const MII_BCM54XX_AUX_CTL: u32 = 0x18;
const AUXCTL_SHDWSEL_MISC: u16 = 0x07;
const AUXCTL_MISC_WREN: u16 = 0x8000;
const AUXCTL_MISC_RGMII_SKEW_EN: u16 = 0x0100;
const AUXCTL_SHDWSEL_MASK: u16 = 0x0007;
const AUXCTL_SHDWSEL_READ_SHIFT: u16 = 12;
/// Shadow register window, where the transmit clock control lives.
const MII_BCM54XX_SHD: u32 = 0x1C;
const SHD_WRITE: u16 = 0x8000;
const SHD_CLK_CTL: u16 = 0x3;
const SHD_CLK_CTL_GTXCLK_EN: u16 = 1 << 9;

// ---------------------------------------------------------------------------------------------
// Ring geometry, and the arena it has to fit in.
//
// The DMA arena is 64 KiB, fixed by the spawn path, and it holds ONLY packet buffers - GENET keeps its
// descriptors in the controller's own register file (that is what `RDMA_OFFSET`/`TDMA_OFFSET` address),
// so none of the arena goes on descriptor storage. 24 receive buffers plus 8 transmit buffers of 2 KiB
// each is exactly 64 KiB, and the const assertion below is what keeps that sentence true if anyone
// edits a number (§26.6.1: a bound you can read off the source).
//
// The in-kernel driver used 32 and 32, which is 128 KiB of frames from the general allocator. Fitting
// the arena is not a regression: transmit here is strictly one frame at a time (net-stack sends, then
// waits for the reply), so a deep transmit ring buys nothing, and 24 receive buffers still absorb a
// burst between drains.
// ---------------------------------------------------------------------------------------------

/// Bytes per buffer. Linux's `RX_BUF_LENGTH`; one 2 KiB buffer holds any Ethernet frame this MAC will
/// accept, so a frame never spans descriptors and the driver never has to reassemble.
const RX_BUF_LENGTH: usize = 2048;
const RX_RING_DESCS: usize = 24;
const TX_RING_DESCS: usize = 8;
const RX_BUF_BASE: usize = 0;
const TX_BUF_BASE: usize = RX_BUF_BASE + RX_RING_DESCS * RX_BUF_LENGTH;
const ARENA_NEEDED: usize = TX_BUF_BASE + TX_RING_DESCS * RX_BUF_LENGTH;
const _: () = assert!(ARENA_NEEDED == 64 * 1024, "the ring buffers must fit the 64 KiB DMA arena");

/// One Ethernet frame (at most 1518) with headroom. The frame-interface constant, matching every other
/// backend in this service so the reply shapes cannot drift.
const FRAME_MAX: usize = 1600;
const BATCH_MAX: u8 = 8;
const BATCH_MSG_MAX: usize = 3072;

// --- Bounded waits -----------------------------------------------------------------------------

/// How long the MDIO controller gets to clear `START_BUSY`. The kernel driver spent 10,000 iterations
/// of a 10 us delay here; this is the same 100 ms, said as a duration.
const MDIO_TIMEOUT_US: u64 = 100_000;
/// How long a DMA engine gets to report itself started.
const DMA_START_TIMEOUT_US: u64 = 100_000;
/// The fallback ceiling for a machine that reports no timer calibration, where a real deadline cannot
/// be computed. It is an iteration count and it is named as one: it bounds the loop, it does not
/// promise a duration. Every caller that hits it reports the failure the same way, so a machine in this
/// state is loud rather than merely slow.
const UNCALIBRATED_POLLS: u32 = 200_000;

/// The GENET controller, as a userspace driver sees it: a register window, a DMA arena, and the
/// service context that provides logging and the clock the waits are bounded by.
pub struct Genet<'a> {
    ctx: &'a ServiceContext,
    m: Mmio,
    a: Dma,
    /// Counter ticks in 10 ms, from the kernel's own calibration. Zero means uncalibrated, which the
    /// wait helpers handle explicitly rather than by dividing by it.
    per_10ms: u64,
}

/// Address of one ring CONTROL register - past the descriptors AND past nothing else, but past the
/// descriptors is the part that is easy to miss.
fn ring_reg(block: usize, index: usize, reg: usize) -> usize {
    block + DMA_REGS_OFF + index * DMA_RING_SIZE + reg
}

/// Address of descriptor `n`'s word, at the START of the block.
fn desc_word(block: usize, n: usize, word: usize) -> usize {
    block + n * WORDS_PER_BD * 4 + word
}

/// Address of a DMA control register (block-wide, not per-ring). See `DMA_RINGS_SIZE`.
fn dma_reg(block: usize, reg: usize) -> usize {
    block + DMA_REGS_OFF + DMA_RINGS_SIZE + reg
}

impl<'a> Genet<'a> {
    pub fn new(ctx: &'a ServiceContext, m: Mmio, a: Dma) -> Self {
        Genet { ctx, m, a, per_10ms: ctx.tsc_ticks_per_10ms() }
    }

    fn rd(&self, off: usize) -> u32 {
        self.m.read32(off)
    }

    fn wr(&self, off: usize, v: u32) {
        self.m.write32(off, v);
    }

    /// Counter ticks in `us` microseconds, floored at 1 so a bound is never zero.
    fn cycles_for_us(&self, us: u64) -> u64 {
        (self.per_10ms.saturating_mul(us) / 10_000).max(1)
    }

    /// Busy-wait `us` microseconds of REAL time. Terminates by construction: the counter is monotonic.
    fn delay_us(&self, us: u64) {
        if self.per_10ms == 0 {
            // No calibration to convert with. Yielding once is an honest "give the hardware a moment"
            // and, unlike a spin of guessed length, cannot silently become either nothing or minutes.
            self.ctx.yield_cpu();
            return;
        }
        let budget = self.cycles_for_us(us);
        let start = self.ctx.read_tsc();
        while self.ctx.read_tsc().wrapping_sub(start) < budget {
            core::hint::spin_loop();
        }
    }

    /// Spin until `read32(off) & mask` matches `want`, or the budget expires. Returns whether the
    /// condition was reached, so every caller can report its own failure in its own words (§26.7).
    fn wait_mask(&self, off: usize, mask: u32, want: bool, us: u64) -> bool {
        let budget = self.cycles_for_us(us);
        let start = self.ctx.read_tsc();
        let mut polls: u32 = 0;
        loop {
            if ((self.rd(off) & mask) != 0) == want {
                return true;
            }
            if self.per_10ms != 0 {
                if self.ctx.read_tsc().wrapping_sub(start) >= budget {
                    return false;
                }
            } else {
                polls += 1;
                if polls >= UNCALIBRATED_POLLS {
                    return false;
                }
            }
            core::hint::spin_loop();
        }
    }

    fn wait_clear(&self, off: usize, mask: u32, us: u64) -> bool {
        self.wait_mask(off, mask, false, us)
    }

    // --- MDIO ---------------------------------------------------------------------------------

    /// One MDIO transaction against the external PHY. `None` if the bus does not answer.
    ///
    /// MDIO is how the MAC talks to a PHY that lives on a separate chip - here a BCM54213PE. It is a
    /// serial bus driven by one register: write the command with `START_BUSY` set, wait for the
    /// controller to clear it, then read the low half for the result.
    ///
    /// **`READ_FAIL` matters and is easy to miss.** A read of an absent PHY returns a perfectly
    /// plausible `0xFFFF` with the fail bit set, and a driver that checks only the data believes the
    /// PHY answered with every capability bit on.
    fn mdio(&self, reg_num: u32, write: Option<u16>) -> Option<u16> {
        let mut cmd = (PHY_ADDR << MDIO_PMD_SHIFT) | (reg_num << MDIO_REG_SHIFT) | MDIO_START_BUSY;
        cmd |= match write {
            Some(v) => MDIO_WR | v as u32,
            None => MDIO_RD,
        };
        self.wr(UMAC_MDIO_CMD, cmd);

        if !self.wait_clear(UMAC_MDIO_CMD, MDIO_START_BUSY, MDIO_TIMEOUT_US) {
            return None; // the bus never went idle
        }
        let done = self.rd(UMAC_MDIO_CMD);
        if write.is_some() {
            return Some(0);
        }
        if done & MDIO_READ_FAIL != 0 {
            return None; // an absent PHY answers 0xFFFF with this set; the data alone would look valid
        }
        Some((done & 0xFFFF) as u16)
    }

    /// Read an auxiliary-control shadow register. The selector goes in TWICE - once in the low bits and
    /// once in the read-select field - which is the part that silently returns the wrong shadow if
    /// missed.
    fn auxctl_read(&self, sel: u16) -> Option<u16> {
        self.mdio(
            MII_BCM54XX_AUX_CTL,
            Some(AUXCTL_SHDWSEL_MASK | (sel << AUXCTL_SHDWSEL_READ_SHIFT)),
        )?;
        self.mdio(MII_BCM54XX_AUX_CTL, None)
    }

    fn auxctl_write(&self, sel: u16, val: u16) -> Option<u16> {
        self.mdio(MII_BCM54XX_AUX_CTL, Some(sel | val))
    }

    fn shadow_read(&self, shadow: u16) -> Option<u16> {
        self.mdio(MII_BCM54XX_SHD, Some((shadow & 0x1F) << 10))?;
        Some(self.mdio(MII_BCM54XX_SHD, None)? & 0x3FF)
    }

    fn shadow_write(&self, shadow: u16, val: u16) -> Option<u16> {
        self.mdio(
            MII_BCM54XX_SHD,
            Some(SHD_WRITE | ((shadow & 0x1F) << 10) | (val & 0x3FF)),
        )
    }

    /// Set the PHY's internal clock delays for `rgmii-rxid`, the mode the Pi 4 wires.
    ///
    /// Receive skew ON (the PHY supplies that delay) and the internal transmit clock delay OFF (it is
    /// not the PHY's to add in this mode). Exactly what `bcm54xx_config_clock_delay` does for
    /// `RGMII_RXID`.
    ///
    /// Both verdicts are announced. They used to be discarded and the success line printed
    /// unconditionally, so an MDIO timeout reported the delays as SET while the PHY kept the
    /// firmware's - precisely the fault this function exists to prevent, and the one that is invisible
    /// from the board (link, speed and MDIO all stay healthy while every frame arrives corrupt).
    fn config_phy_clock_delay(&self) {
        let Some(mut misc) = self.auxctl_read(AUXCTL_SHDWSEL_MISC) else {
            self.ctx.log("nic-driver: genet PHY auxctl read failed - clock delays left as the firmware set them");
            return;
        };
        misc |= AUXCTL_MISC_WREN | AUXCTL_MISC_RGMII_SKEW_EN;
        let wrote_rx = self.auxctl_write(AUXCTL_SHDWSEL_MISC, misc).is_some();

        let Some(clk) = self.shadow_read(SHD_CLK_CTL) else {
            self.ctx.log("nic-driver: genet PHY shadow read failed - transmit clock delay left as found");
            return;
        };
        let wrote_tx = self.shadow_write(SHD_CLK_CTL, clk & !SHD_CLK_CTL_GTXCLK_EN).is_some();

        if wrote_rx && wrote_tx {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet PHY clock delays set for rgmii-rxid (rx skew on, internal tx delay off) - was {:#x}",
                clk));
        } else {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet PHY clock delay write FAILED (rx ok {}, tx ok {}) - the PHY keeps the firmware's skew, frames may arrive corrupt",
                wrote_rx, wrote_tx));
        }
    }

    /// Read the speed the PHY actually negotiated, in Mbit.
    ///
    /// **The MAC does not learn this by itself.** It has a speed field that defaults to 10 Mbit, and on
    /// RGMII a MAC clocking at one speed while the PHY runs at another exchanges nothing at all - link
    /// up, ring armed, and not a single frame. That is exactly the state the first receive enable
    /// produced.
    fn negotiated_speed(&self) -> u32 {
        let Some(aux) = self.mdio(BCM_AUX_STATUS, None) else { return 0 };
        // Bits 10:8 hold the auto-negotiation result on this family.
        match (aux >> 8) & 0x7 {
            0b111 => 1000, // 1000BASE-T full duplex
            0b110 => 1000, // 1000BASE-T half duplex
            0b101 => 100,  // 100BASE-TX full duplex
            0b011 => 100,  // 100BASE-TX half duplex
            0b010 => 10,   // 10BASE-T full duplex
            0b001 => 10,   // 10BASE-T half duplex
            _ => 0,        // still negotiating, or no link
        }
    }

    /// Is the link up right now?
    ///
    /// Read live over MDIO rather than remembered from bring-up, so a cable pulled afterwards reports
    /// down. The basic status register's link bit is **latching-low**, so it is read twice: a single
    /// read of a link that has been up all along returns the latched DOWN and reports a healthy cable
    /// as dead.
    pub fn link_is_up(&self) -> bool {
        let _ = self.mdio(1, None);
        matches!(self.mdio(1, None), Some(bmsr) if bmsr & (1 << 2) != 0)
    }

    // --- MAC ----------------------------------------------------------------------------------

    /// The speed field the MAC needs, from the PHY's negotiated speed. Kept next to the register bits
    /// it encodes so the two cannot drift.
    fn cmd_speed(mbps: u32) -> u32 {
        let sel = match mbps {
            1000 => 2,
            100 => 1,
            _ => 0,
        };
        (sel & CMD_SPEED_MASK) << CMD_SPEED_SHIFT
    }

    /// Release the MAC from the software reset the part powers up holding it in. Until this lands,
    /// every `UMAC_CMD` write is discarded and the MAC never receives a thing. See
    /// [`SYS_RBUF_FLUSH_CTRL`].
    fn release_sw_reset(&self) {
        self.wr(SYS_RBUF_FLUSH_CTRL, 0);
        self.delay_us(10);
    }

    /// Reset the MAC and put it in a known, quiet state.
    ///
    /// The reset bit is **self-clearing and must be given time**; Linux writes it, waits, then clears
    /// the command register outright. Leaving TX or RX enabled through a reset is how a MAC comes back
    /// still holding half a frame.
    fn umac_reset(&self) -> bool {
        self.release_sw_reset();

        self.wr(UMAC_CMD, CMD_SW_RESET);
        self.delay_us(10);
        self.wr(UMAC_CMD, 0);
        self.delay_us(10);

        // Prove the release worked rather than trusting it. A register that cannot hold a bit is the
        // exact failure this function exists to clear, and it is invisible until frames fail to arrive
        // much later. Writing a bit that is harmless on its own (pause-ignore, with TX and RX still
        // disabled) turns a silent dead MAC into a loud one here, where the cause is still obvious.
        self.wr(UMAC_CMD, CMD_RX_PAUSE_IGNORE);
        if self.rd(UMAC_CMD) & CMD_RX_PAUSE_IGNORE == 0 {
            self.ctx.log("nic-driver: genet UMAC_CMD will not hold a write - the MAC is still held in reset");
            return false;
        }
        self.wr(UMAC_CMD, 0);
        true
    }

    /// Program the station address the MAC filters on.
    ///
    /// `MAC0` takes the first four bytes big-endian and `MAC1` the last two - not the little-endian
    /// layout the rest of this file uses, so a byte-order slip here produces a MAC that looks right in
    /// a log and matches nothing on the wire.
    fn set_mac_address(&self, mac: [u8; 6]) {
        self.wr(
            UMAC_MAC0,
            ((mac[0] as u32) << 24) | ((mac[1] as u32) << 16) | ((mac[2] as u32) << 8) | mac[3] as u32,
        );
        self.wr(UMAC_MAC1, ((mac[4] as u32) << 8) | mac[5] as u32);
    }

    /// The station address to run with.
    ///
    /// **This is the one thing the in-kernel driver could do that a service cannot.** The Pi's real
    /// address lives in the SoC's OTP and is reachable only through the VideoCore property mailbox
    /// (`GET_BOARD_MAC_ADDRESS`, tag `0x0001_0003`), which is a different peripheral at `0xFE00_B880`
    /// and one this service is deliberately not granted. Handing an ethernet driver the mailbox would
    /// hand it power, clocks, memory allocation and the framebuffer along with the address it wanted,
    /// which is not a trade worth making for six bytes (§3.1).
    ///
    /// So: adopt whatever the firmware left in the MAC's own station registers if it is a usable
    /// unicast address, and otherwise fall back to the locally-administered placeholder the kernel
    /// driver used through its own bring-up. Which one happened is REPORTED, because the two are
    /// indistinguishable afterwards and only one of them is worth investigating - and because a board
    /// running on the placeholder will transmit well-formed frames that nothing on the segment has any
    /// reason to answer, which is a failure that looks like a hardware fault (invariant 12).
    ///
    /// Read after the software-reset release and before the command reset, which is the only window
    /// where the registers both read back and still hold whatever the firmware put there.
    fn station_address(&self) -> [u8; 6] {
        const PLACEHOLDER: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

        let m0 = self.rd(UMAC_MAC0);
        let m1 = self.rd(UMAC_MAC1);
        let mac = [
            (m0 >> 24) as u8,
            (m0 >> 16) as u8,
            (m0 >> 8) as u8,
            m0 as u8,
            (m1 >> 8) as u8,
            m1 as u8,
        ];
        // All-zero and all-ones are the two ways "nothing was programmed" present; a multicast bit in
        // the first octet means whatever is in there is not a station address at all.
        let usable = mac != [0; 6] && mac != [0xFF; 6] && mac[0] & 0x01 == 0;
        if usable {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet adopted the station address the firmware left in UMAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));
            return mac;
        }
        self.ctx.log(
            "nic-driver: genet found NO station address in UMAC and cannot read the board's own \
             (that needs the VideoCore mailbox, which this service is not granted) - running on the \
             placeholder 02:00:00:00:00:01, which nothing on the segment has a reason to answer");
        PLACEHOLDER
    }

    /// Accept our own address and broadcast, and nothing else.
    ///
    /// This replaces the promiscuous mode bring-up uses, and it is not merely tidying. Promiscuous puts
    /// EVERY frame on the segment into a small ring that userspace drains one batch per request. On a
    /// live network that ring fills with other people's broadcast traffic, and the producer index's
    /// upper half - which counts DISCARDS - is the hardware saying so. A DHCP offer addressed to us is
    /// then thrown away before the stack ever asks for it.
    ///
    /// Broadcast is filter 0 and our own address filter 1, so ARP and DHCP both still arrive.
    fn set_rx_filter(&self, mac: [u8; 6]) {
        self.set_mdf_addr(0, [0xFF; 6]);
        self.set_mdf_addr(2, mac);

        // Two filters, enabled from the top: bits 16 and 15. See `MAX_MDF_FILTER`.
        let enable = ((1u32 << MAX_MDF_FILTER) - 1) & !((1u32 << (MAX_MDF_FILTER - 2)) - 1);
        self.wr(UMAC_MDF_CTRL, enable);

        // With a real filter in place, stop accepting everything.
        self.wr(UMAC_CMD, self.rd(UMAC_CMD) & !CMD_PROMISC);
    }

    /// Write one address into the filter registers starting at `i`. Each address occupies TWO
    /// registers, which is why the caller steps by two rather than by one.
    fn set_mdf_addr(&self, i: usize, mac: [u8; 6]) {
        self.wr(UMAC_MDF_ADDR + i * 4, ((mac[0] as u32) << 8) | mac[1] as u32);
        self.wr(
            UMAC_MDF_ADDR + (i + 1) * 4,
            ((mac[2] as u32) << 24)
                | ((mac[3] as u32) << 16)
                | ((mac[4] as u32) << 8)
                | mac[5] as u32,
        );
    }

    /// Disable every hardware filter, so nothing between the MAC and the DMA drops a frame.
    fn hfb_clear(&self) {
        self.wr(HFB_REG_OFF + HFB_CTRL, 0);
        self.wr(HFB_REG_OFF + HFB_FLT_ENABLE_V3PLUS, 0);
        self.wr(HFB_REG_OFF + HFB_FLT_ENABLE_V3PLUS + 4, 0);

        // Filter-to-ring steering: eight registers in the RDMA block, not the HFB one.
        for i in 0..8 {
            self.wr(dma_reg(RDMA_OFFSET, DMA_INDEX2RING_0 + i * 4), 0);
        }
        // Per-filter lengths, packed four to a register.
        for i in 0..(HFB_FILTER_CNT / 4) {
            self.wr(HFB_REG_OFF + HFB_FLT_LEN_V3PLUS + i * 4, 0);
        }
        // The filter RAM itself. Bounded and one-time: 48 filters of 128 words.
        for i in 0..(HFB_FILTER_CNT * HFB_FILTER_SIZE) {
            self.wr(HFB_OFF + i * 4, 0);
        }
    }

    // --- DMA ----------------------------------------------------------------------------------

    /// Does this block base actually address a DMA ring?
    ///
    /// Writes a pattern to a harmless ring register (the start address of a ring nothing has enabled),
    /// reads it back, and restores whatever was there. A base that points at the wrong block reads back
    /// something other than what was written - or reads back zero, which is the common case for a
    /// register that is not there at all.
    ///
    /// This is the check the MDIO offset did not have. `0x614` produced a clean `0x0` with every error
    /// bit clear, and there was nothing to catch it because a read alone cannot tell a wrong address
    /// from a register that legitimately holds zero. A WRITE-then-read can. The address used is in the
    /// CONTROL register area past the descriptors: an earlier version of this test addressed the
    /// descriptor area and passed on writable memory rather than on a register, which is a pass for the
    /// wrong reason and exactly the failure mode the check exists against.
    fn verify_dma_base(&self, block: usize, name: &str) -> bool {
        let addr = ring_reg(block, 0, DMA_START_ADDR);
        let saved = self.rd(addr);
        const PATTERN: u32 = 0x0000_5A5A;
        self.wr(addr, PATTERN);
        let got = self.rd(addr);
        self.wr(addr, saved);
        if got != PATTERN {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet {} base does not behave like a DMA ring (wrote 0x5a5a, read {:#x}) - NOT enabling DMA",
                name, got));
            return false;
        }
        true
    }

    /// Point every ring we are NOT using at nothing, before enabling anything.
    ///
    /// **This is the step that makes a mistake survivable.** Rings 0..15 have never been programmed, so
    /// their descriptors hold whatever the register file powered up with - and a ring enabled with a
    /// garbage descriptor is a bus master writing to a garbage physical address, on a board with no
    /// IOMMU. Zeroing their descriptors first means the worst case of a wrong enable is a write to
    /// physical 0, which the memory map already reserves, instead of a write into the kernel.
    ///
    /// It costs one bounded loop at bring-up and it is the difference between a bug that prints
    /// something and a bug that corrupts memory somewhere else entirely.
    fn quiesce_unused_descriptors(&self, block: usize, used: usize) {
        for d in used..TOTAL_DESC {
            self.wr(desc_word(block, d, DMA_DESC_ADDRESS_LO), 0);
            self.wr(desc_word(block, d, DMA_DESC_ADDRESS_HI), 0);
            self.wr(desc_word(block, d, DMA_DESC_LENGTH_STATUS), 0);
        }
    }

    /// Zero the whole DMA arena, one aligned 32-bit word at a time.
    ///
    /// **Not `Dma::zero()`, deliberately.** That is a `write_bytes`, and on this port the arena is
    /// mapped Device-nGnRnE (the spawn path uses `PCD`, which AArch64 reads as the Device attribute
    /// rather than as "uncached Normal"). Device memory does not permit the unaligned and
    /// cache-hint-bearing accesses a `memset` is free to emit, so the portable-looking call is the
    /// riskier one here. An explicit aligned loop cannot be anything other than what it says.
    ///
    /// Zeroing matters beyond tidiness: `receive` trusts the controller's length for how many bytes are
    /// MEANINGFUL, so a device reporting more than it wrote would hand `net-stack` whatever previously
    /// occupied the arena (the SEC-21 information-leak class).
    fn clear_arena(&self) {
        let words = self.a.len() / 4;
        for i in 0..words {
            self.a.write32(i * 4, 0);
        }
    }

    /// Build the receive ring: one arena buffer per descriptor, the ring bounds, the indices, and the
    /// ring position - then read the geometry back before anything is enabled.
    fn init_rx_ring(&self) -> bool {
        for i in 0..RX_RING_DESCS {
            let phys = self.a.phys_at(RX_BUF_BASE + i * RX_BUF_LENGTH);
            // The descriptor's address words. The controller reads these to find where to put a frame,
            // so they are PHYSICAL - our own view of that memory is the arena mapping, and handing over
            // a virtual address is a device writing to an address that means nothing to it.
            self.wr(desc_word(RDMA_OFFSET, i, DMA_DESC_ADDRESS_LO), phys as u32);
            self.wr(desc_word(RDMA_OFFSET, i, DMA_DESC_ADDRESS_HI), (phys >> 32) as u32);
            // Length/status starts clear: ownership is granted by the producer index, not by a bit here.
            self.wr(desc_word(RDMA_OFFSET, i, DMA_DESC_LENGTH_STATUS), 0);
        }
        self.quiesce_unused_descriptors(RDMA_OFFSET, RX_RING_DESCS);

        // Ring geometry. `DMA_RING_BUF_SIZE` packs the descriptor count in the upper half and the
        // buffer length in the lower - two different units in one register, which is the kind of field
        // that reads fine and behaves wrongly if the halves are swapped.
        let buf_size = ((RX_RING_DESCS as u32) << 16) | RX_BUF_LENGTH as u32;
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, DMA_RING_BUF_SIZE), buf_size);

        // Start and end are in WORDS, not descriptors and not bytes - `start_ptr * words_per_bd` in
        // Linux. The end is inclusive, hence the minus one.
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, DMA_START_ADDR), 0);
        self.wr(
            ring_reg(RDMA_OFFSET, RING_INDEX, DMA_END_ADDR),
            (RX_RING_DESCS * WORDS_PER_BD - 1) as u32,
        );
        // BOTH indices, and each is a different party's register: the producer is the hardware's, the
        // consumer is ours. Zeroing only one leaves the ring half-initialised.
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_PROD_INDEX), 0);
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_CONS_INDEX), 0);
        // How many descriptors the hardware fills before it counts a batch done. 1 = report every
        // frame, which is what a polled driver wants.
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, DMA_MBUF_DONE_THRESH), 1);

        // The ring POSITION pointers, in words, matching `DMA_START_ADDR`. These were missing entirely
        // once: the geometry said where the ring lives and the indices said it was empty, but nothing
        // told the engine where in it to start writing. An engine with no position never advances -
        // which is a ring that reports exactly what a filtered-out or disabled MAC reports, and is why
        // that survived three separate fixes upstream of it.
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_WRITE_PTR), 0);
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_WRITE_PTR_HI), 0);
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_READ_PTR), 0);
        self.wr(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_READ_PTR_HI), 0);

        // Flow-control thresholds. Linux programs these on every ring; a zero XOFF threshold is a
        // receiver permanently asking the far end to stop.
        self.wr(
            ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_XON_XOFF_THRESH),
            (DMA_FC_THRESH_LO << DMA_XOFF_THRESHOLD_SHIFT) | DMA_FC_THRESH_HI,
        );

        // Read the geometry back. A ring whose registers did not take is a ring the controller would
        // walk using whatever they do hold - and the descriptors above already point at real memory.
        let got = self.rd(ring_reg(RDMA_OFFSET, RING_INDEX, DMA_RING_BUF_SIZE));
        if got != buf_size {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet receive ring geometry did not take (wrote {:#x}, read {:#x}) - NOT enabling receive",
                buf_size, got));
            return false;
        }
        true
    }

    /// Build the transmit ring: same shape as the receive ring, and the same refusal to enable anything
    /// until the geometry has been read back.
    fn init_tx_ring(&self) -> bool {
        for i in 0..TX_RING_DESCS {
            let phys = self.a.phys_at(TX_BUF_BASE + i * RX_BUF_LENGTH);
            self.wr(desc_word(TDMA_OFFSET, i, DMA_DESC_ADDRESS_LO), phys as u32);
            self.wr(desc_word(TDMA_OFFSET, i, DMA_DESC_ADDRESS_HI), (phys >> 32) as u32);
            self.wr(desc_word(TDMA_OFFSET, i, DMA_DESC_LENGTH_STATUS), 0);
        }
        self.quiesce_unused_descriptors(TDMA_OFFSET, TX_RING_DESCS);

        let buf_size = ((TX_RING_DESCS as u32) << 16) | RX_BUF_LENGTH as u32;
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, DMA_RING_BUF_SIZE), buf_size);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, DMA_START_ADDR), 0);
        self.wr(
            ring_reg(TDMA_OFFSET, RING_INDEX, DMA_END_ADDR),
            (TX_RING_DESCS * WORDS_PER_BD - 1) as u32,
        );
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_PROD_INDEX), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_CONS_INDEX), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, DMA_MBUF_DONE_THRESH), 1);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_FLOW_PERIOD), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_READ_PTR), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_READ_PTR_HI), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_WRITE_PTR), 0);
        self.wr(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_WRITE_PTR_HI), 0);

        let got = self.rd(ring_reg(TDMA_OFFSET, RING_INDEX, DMA_RING_BUF_SIZE));
        if got != buf_size {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet transmit ring geometry did not take (wrote {:#x}, read {:#x})",
                buf_size, got));
            return false;
        }
        true
    }

    /// Turn a DMA engine on for ring `RING_INDEX`, and refuse to leave it on if the controller does not
    /// agree that it started.
    fn enable_dma(&self, block: usize, name: &str) -> bool {
        // Enable ONLY ring 16. `DMA_RING_CFG` is a bitmask of rings, so writing anything wider here is
        // what would start the rings just quiesced.
        let ring_bit = 1u32 << RING_INDEX;
        self.wr(dma_reg(block, DMA_RING_CFG), ring_bit);
        // `DMA_CTRL` carries the master enable in bit 0 and the per-ring enables from bit 1 up.
        self.wr(
            dma_reg(block, DMA_CTRL),
            DMA_EN | (ring_bit << DMA_RING_BUF_EN_SHIFT),
        );

        // Confirm the engine actually started. `DMA_STATUS` bit 0 reads SET while it is stopped, so a
        // controller that ignored the enable says so here rather than by silently moving nothing.
        if !self.wait_clear(dma_reg(block, DMA_STATUS), DMA_DISABLED, DMA_START_TIMEOUT_US) {
            // Back out rather than leave a half-enabled engine pointing at our buffers.
            self.wr(dma_reg(block, DMA_CTRL), 0);
            self.wr(dma_reg(block, DMA_RING_CFG), 0);
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet {} DMA would not start (status still says disabled) - backed out",
                name));
            return false;
        }
        true
    }

    /// Tell the MAC what speed the PHY settled on, and program the DMA burst size.
    ///
    /// Both were missing from the first receive enable, and either alone is enough to receive nothing:
    /// the speed because the MAC and PHY must clock together, and the burst size because it is part of
    /// the DMA init sequence Linux performs before starting the engine.
    pub(crate) fn apply_link_settings(&self) -> u32 {
        self.wr(dma_reg(RDMA_OFFSET, DMA_SCB_BURST_SIZE), DMA_MAX_BURST_LENGTH);
        self.wr(dma_reg(TDMA_OFFSET, DMA_SCB_BURST_SIZE), DMA_MAX_BURST_LENGTH);

        let mbps = self.negotiated_speed();
        if mbps == 0 {
            self.ctx.log("nic-driver: genet PHY has not settled on a speed - leaving the MAC at its default");
            return 0;
        }
        let cmd = self.rd(UMAC_CMD) & !(CMD_SPEED_MASK << CMD_SPEED_SHIFT);
        self.wr(UMAC_CMD, cmd | Self::cmd_speed(mbps));
        self.ctx.log_fmt(format_args!(
            "nic-driver: genet PHY negotiated {} Mbit - MAC speed set to match", mbps));
        mbps
    }

    // --- Bring-up -----------------------------------------------------------------------------

    /// Bring the controller all the way up, or say why not. `None` means the frame interface will be
    /// served with empty replies rather than driving hardware that is not in a known state.
    pub fn bring_up(&self) -> Option<[u8; 6]> {
        if self.m.len() < 0x10000 {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet register window is only {} bytes - the DMA blocks and the filter RAM are outside it",
                self.m.len()));
            return None;
        }
        if self.a.len() < ARENA_NEEDED {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet needs a {} byte DMA arena and was granted {} - not enabling the rings",
                ARENA_NEEDED, self.a.len()));
            return None;
        }

        // The revision picks the register layout: GENET v1..v5 move the DMA rings and rename fields, so
        // a driver written against v5 offsets and run on a v3 part reads plausible values from the wrong
        // places. All-ones and all-zeros are the two ways "nothing is there" presents, and neither is a
        // revision. (The kernel probes this address through an abort-catching read before granting us
        // the window at all, so by the time we get here a controller has already answered once.)
        let raw = self.rd(SYS_REV_CTRL);
        if raw == 0 || raw == 0xFFFF_FFFF {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet SYS_REV_CTRL reads {:#x} - no controller behind the granted window",
                raw));
            return None;
        }
        // The revision field encoding, from Linux's `bcmgenet_probe`: bits 27:24 hold the major, offset
        // by one from v4 onward (4 means v4 is reported as 5, 5 as 6), and bits 19:16 hold the minor.
        // The offset is not a detail to skip - reading the raw field gives a version number one higher
        // than the part actually is, and picking a register layout from that is how a driver ends up
        // addressing the wrong block on the right chip.
        let major = match (raw >> 24) & 0x0F {
            6 => 5,
            5 => 4,
            0 => 1,
            other => other,
        };
        let minor = (raw >> 16) & 0x0F;
        self.ctx.log_fmt(format_args!(
            "nic-driver: genet v{}.{} (rev {:#x}) - driving it from userspace", major, minor, raw));
        if major != 5 {
            self.ctx.log("nic-driver: genet WARNING expected v5 on a BCM2711 - the register map is written for v5");
        }

        // Drive the external gigabit PHY, not one of the internal modes. Wrong here and the MAC talks to
        // something that is not on this board.
        self.wr(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);

        // Switch on the RGMII block. Selecting the port mode above says WHICH interface the MAC drives;
        // this enables the interface itself. Without it the MAC is wired to a PHY it can negotiate with
        // over MDIO and receive nothing from.
        let mut oob = self.rd(EXT_RGMII_OOB_CTRL);
        oob &= !OOB_DISABLE;
        oob &= !ID_MODE_DIS; // rgmii-rxid: the PHY supplies the receive delay, so leave ID mode enabled
        oob |= RGMII_MODE_EN;
        self.wr(EXT_RGMII_OOB_CTRL, oob);
        if self.rd(EXT_RGMII_OOB_CTRL) & RGMII_MODE_EN == 0 {
            self.ctx.log("nic-driver: genet RGMII block will not enable - the MAC cannot reach the PHY");
            return None;
        }

        // Release the software reset first so the station registers read back, take the address, THEN
        // reset the command register. Reading before the release gets zeros from a MAC that is still
        // held down, which would look exactly like "the firmware left nothing here".
        self.release_sw_reset();
        let mac = self.station_address();

        if !self.umac_reset() {
            return None;
        }
        // Zero the statistics, so anything they report later was counted by us and not inherited from
        // whatever the firmware did with this MAC before we took it over.
        self.wr(UMAC_MIB_CTRL, MIB_RESET_RX | MIB_RESET_TX | MIB_RESET_RUNT);
        self.wr(UMAC_MIB_CTRL, 0);

        self.wr(UMAC_MAX_FRAME_LEN, MAX_FRAME);
        self.set_mac_address(mac);

        // Ignore pause frames in both directions: flow control is a policy the stack above has no way
        // to express, and honouring pause without a stack that can act on it stalls transmission for
        // reasons nothing can explain. TX and RX stay DISABLED until their rings are running - a
        // receiver enabled with no ring behind it fills a FIFO nobody drains.
        let cmd = self.rd(UMAC_CMD) & !(CMD_TX_EN | CMD_RX_EN | CMD_PROMISC | CMD_LCL_LOOP_EN);
        self.wr(UMAC_CMD, cmd | CMD_TX_PAUSE_IGNORE | CMD_RX_PAUSE_IGNORE);

        // The PHY id lives in MII registers 2 and 3. An id of 0 or all-ones is the signature of nothing
        // answering, which is exactly what a mis-clocked MDIO bus looks like.
        //
        // Spelled out rather than written with `?`, which is what the in-kernel version used: a bare
        // `?` here returns None with nothing said, and "genet did not come up" a frame later cannot
        // tell an MDIO bus that never went idle from a PHY that answered wrongly (§26.7). The two have
        // different causes and only one of them is about the PHY.
        let (Some(id_hi), Some(id_lo)) = (self.mdio(2, None), self.mdio(3, None)) else {
            self.ctx.log("nic-driver: genet MDIO did not answer the PHY id read - the bus never went idle, or the read failed");
            return None;
        };
        let phy_id = ((id_hi as u32) << 16) | id_lo as u32;
        if phy_id == 0 || phy_id == 0xFFFF_FFFF {
            self.ctx.log_fmt(format_args!(
                "nic-driver: genet MDIO answered but the PHY id is {:#x} - no PHY on the bus", phy_id));
            return None;
        }
        self.ctx.log_fmt(format_args!(
            "nic-driver: genet MAC configured, PHY id {:#x} ({})",
            phy_id,
            // BCM54213PE reports 0x600d84a2; the low nibbles are a revision, so compare the model bits.
            if phy_id & 0xFFFF_FFF0 == 0x600D_84A0 {
                "BCM54213PE, the Pi 4's gigabit PHY"
            } else {
                "unrecognised - the register map assumes a BCM54213PE"
            }));

        // Prove the DMA block bases before anything hands the controller a buffer. A wrong base does not
        // return a wrong value the way a bad MDIO offset does; it corrupts RAM, on a board with no
        // IOMMU. Nothing below runs unless both readbacks match.
        if !self.verify_dma_base(RDMA_OFFSET, "RDMA") || !self.verify_dma_base(TDMA_OFFSET, "TDMA") {
            return None;
        }

        // Set the PHY's RGMII clock delays before any traffic moves. Skipping this leaves whatever the
        // firmware happened to configure, and a clock skew that is wrong corrupts frames while leaving
        // link, speed and MDIO all perfectly healthy.
        self.config_phy_clock_delay();
        self.apply_link_settings();

        self.clear_arena();
        if !self.init_rx_ring() {
            return None;
        }
        if !self.enable_dma(RDMA_OFFSET, "receive") {
            return None;
        }

        // Nothing between the MAC and the DMA may drop a frame. Linux clears the filter block here too:
        // after the DMA is running, before the receiver is switched on.
        self.hfb_clear();

        // Only now let the MAC accept frames. RX_EN before the ring is running is a receiver with
        // nowhere to put what it takes.
        self.wr(UMAC_CMD, self.rd(UMAC_CMD) | CMD_RX_EN);
        if self.rd(UMAC_CMD) & CMD_RX_EN == 0 {
            self.wr(dma_reg(RDMA_OFFSET, DMA_CTRL), 0);
            self.wr(dma_reg(RDMA_OFFSET, DMA_RING_CFG), 0);
            self.ctx.log("nic-driver: genet RX_EN would not set - the MAC is not accepting frames, backed out");
            return None;
        }

        if !self.init_tx_ring() || !self.enable_dma(TDMA_OFFSET, "transmit") {
            return None;
        }
        self.wr(UMAC_CMD, self.rd(UMAC_CMD) | CMD_TX_EN);
        if self.rd(UMAC_CMD) & CMD_TX_EN == 0 {
            self.ctx.log("nic-driver: genet TX_EN would not set - the MAC will not transmit");
            return None;
        }

        // Narrow to our address plus broadcast before any traffic is admitted.
        self.set_rx_filter(mac);
        Some(mac)
    }

    // --- The data path ------------------------------------------------------------------------

    /// Take ONE frame into `dst`, returning the byte count. 0 when the ring is empty.
    ///
    /// Non-blocking by construction: a driver polling an idle network must get an immediate zero rather
    /// than holding its core waiting for traffic.
    fn receive(&self, dst: &mut [u8]) -> usize {
        let prod = self.rd(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_PROD_INDEX)) & 0xFFFF;
        let cons = self.rd(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_CONS_INDEX)) & 0xFFFF;
        if cons == prod {
            return 0;
        }

        let slot = (cons as usize) % RX_RING_DESCS;
        let status = self.rd(desc_word(RDMA_OFFSET, slot, DMA_DESC_LENGTH_STATUS));
        let len = ((status >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize;

        // Clamp to BOTH the caller's buffer and the buffer the hardware was given. The length comes
        // from a descriptor the CONTROLLER wrote, so it is device-supplied and not to be trusted with a
        // copy size.
        let n = len.min(dst.len()).min(RX_BUF_LENGTH);
        let base = RX_BUF_BASE + slot * RX_BUF_LENGTH;
        for (i, b) in dst.iter_mut().enumerate().take(n) {
            *b = self.a.read8(base + i);
        }

        // Returning the descriptor to the hardware is what keeps receive alive past the first ring's
        // worth. A driver that reads frames without advancing the consumer index receives exactly
        // RX_RING_DESCS of them and then stops forever.
        self.wr(
            ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_CONS_INDEX),
            cons.wrapping_add(1) & 0xFFFF,
        );
        n
    }

    /// Queue one frame for transmission. `false` if the frame does not fit a buffer or the engine did
    /// not take it.
    fn transmit(&self, frame: &[u8], tx_next: &mut u32) -> bool {
        if frame.len() > RX_BUF_LENGTH {
            return false;
        }
        let slot = (*tx_next as usize) % TX_RING_DESCS;
        let base = TX_BUF_BASE + slot * RX_BUF_LENGTH;
        for (i, b) in frame.iter().enumerate() {
            self.a.write8(base + i, *b);
        }

        let len_stat = ((frame.len() as u32) << 16)
            | DMA_SOP
            | DMA_EOP
            | DMA_TX_APPEND_CRC
            | (DMA_TX_QTAG_MASK << DMA_TX_QTAG_SHIFT);
        self.wr(desc_word(TDMA_OFFSET, slot, DMA_DESC_LENGTH_STATUS), len_stat);

        // Publishing the producer index IS the handover, so it goes last.
        *tx_next = tx_next.wrapping_add(1);
        self.wr(
            ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_PROD_INDEX),
            *tx_next & 0xFFFF,
        );
        true
    }

    /// Report what each half of the path has actually moved.
    ///
    /// "DHCP got no offer" and "ARP got no reply" are both consistent with several different faults and
    /// cannot tell them apart. These numbers can:
    ///
    ///   `tx_prod` climbing, `tx_cons` following   frames ARE reaching the wire; the failure is inbound
    ///   `tx_prod` climbing, `tx_cons` stuck       the DMA is not taking what we queue
    ///   `tx_prod` at 0                            nothing is calling transmit at all
    ///   `rx_pkt` climbing, `rx_prod` at 0         the MAC hears the wire but the ring gets nothing
    ///   `rx_prod` far ahead of `rx_cons`          frames ARRIVE and nothing is draining them
    ///
    /// The producer register's UPPER half is a DISCARD COUNT - frames the hardware had nowhere to put.
    /// Masking it off throws away the one number that distinguishes "nothing arrived" from "everything
    /// arrived and was dropped", so it is reported separately here.
    fn report_counters(&self) {
        let rx_prod = self.rd(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_PROD_INDEX));
        let rx_cons = self.rd(ring_reg(RDMA_OFFSET, RING_INDEX, RDMA_CONS_INDEX));
        let tx_prod = self.rd(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_PROD_INDEX));
        let tx_cons = self.rd(ring_reg(TDMA_OFFSET, RING_INDEX, TDMA_CONS_INDEX));
        self.ctx.log_fmt(format_args!(
            "nic-driver: genet counters - rx_pkt {:#x} rx_bcast {:#x} fcs_err {:#x} overflow {:#x} rx_discards {:#x} rx_prod {:#x} rx_cons {:#x} tx_pkt {:#x} tx_prod {:#x} tx_cons {:#x}",
            self.rd(UMAC_MIB_RX_PKT),
            self.rd(UMAC_MIB_RX_BCA),
            self.rd(UMAC_MIB_RX_FCS),
            self.rd(UMAC_MIB_RX_OVR),
            rx_prod >> 16,
            rx_prod & 0xFFFF,
            rx_cons & 0xFFFF,
            self.rd(UMAC_MIB_TX_PKT),
            tx_prod & 0xFFFF,
            tx_cons & 0xFFFF));
    }
}

// ---------------------------------------------------------------------------------------------
// The frame interface.
//
// Byte-for-byte the contract `kernel_net_main` serves, because `net-stack` must not be able to tell
// which backend is underneath it (Commandment X: the driver is mechanism, the stack is policy). A
// 1-byte payload of 3/4/5/6/7/8/9 is an opcode; anything else is a raw ethernet frame to transmit.
// ---------------------------------------------------------------------------------------------

/// The `nic-driver` entry point on a Pi 4 that hands GENET to userspace.
///
/// Degrades rather than hangs at every step (§26.7). No register window (no controller on the board),
/// no DMA arena, or a bring-up that refused: all three fall through to the shared empty-reply server,
/// so `net-stack` sees a NIC that reports itself down instead of a request that never comes back.
pub fn genet_main(ctx: ServiceContext) -> ! {
    let (Some(m), Some(a)) = (ctx.mmio(), ctx.dma_region()) else {
        ctx.log("nic-driver: no GENET register window or DMA arena granted - serving empty replies");
        crate::serve_status(&ctx, &[0u8; 8]);
    };

    let g = Genet::new(&ctx, m, a);
    if g.per_10ms == 0 {
        // Say it once, here, rather than letting every bounded wait quietly change meaning. A machine
        // in this state still works; its timeouts are just counted instead of measured.
        ctx.log("nic-driver: genet has no timer calibration - hardware waits fall back to an iteration ceiling, which is NOT a duration");
    }

    let Some(mac) = g.bring_up() else {
        ctx.log("nic-driver: genet did not come up - serving empty replies (net degrades, not hangs)");
        crate::serve_status(&ctx, &[0u8; 8]);
    };

    ctx.log_fmt(format_args!(
        "nic-driver: genet up  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  link {}  ({} rx / {} tx buffers of {} B in a {} KiB arena)",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        if g.link_is_up() { "UP" } else { "down (no cable?)" },
        RX_RING_DESCS, TX_RING_DESCS, RX_BUF_LENGTH, ARENA_NEEDED / 1024));
    ctx.log("nic-driver: serving frame interface");

    serve(&ctx, &g, mac)
}

fn serve(ctx: &ServiceContext, g: &Genet, mac: [u8; 6]) -> ! {
    let mut tx_next: u32 = 0;
    // Bounds the post-transmit counter report, so a diagnostic cannot become a console flood (§26.6).
    // It lives here rather than in `transmit` because it is serve-loop state, not driver state, and
    // because printing from the transmit path is what makes it useful: it is the thing that proved the
    // RGMII clock skew (frames well-formed, tx_pkt climbing, nothing ever answering).
    let mut tx_reports: u32 = 0;
    // Link state as of the last status request, for edge-triggered re-apply (see its use below).
    // SEEDED FROM THE CURRENT LINK, so a cable that was already present at bring-up is not treated as
    // a fresh transition - `bring_up` has just applied the settings for it.
    let mut link_was_up = g.link_is_up();
    let mut rxbuf = [0u8; FRAME_MAX];

    // Outside the loop deliberately: a once-only latch declared inside the loop it guards resets on
    // every iteration and reports every time, which is the flood it exists to prevent.
    let mut capless_logged = false;
    let mut reply_failed_logged = false;
    loop {
        let req = ctx.recv();
        // The reply cap is the ONLY authority to answer net-stack (§8.5).
        //
        // A request that carries none cannot be answered, and dropping it SILENTLY leaves no evidence
        // anywhere: net-stack waits out its deadline and reports the driver unresponsive, while the
        // driver's log shows a clean run. The sibling backend (`main.rs`) already logs this; GENET is
        // the backend that actually runs on the Pi 4 and was the one that forgot (Commandment III -
        // two implementations of one rule).
        //
        // Rate-limited to once, because the condition repeats per request and the report must not
        // become the flood it is reporting.
        let Some(reply_cap) = ctx.take_pending_cap() else {
            if !capless_logged {
                capless_logged = true;
                ctx.log("nic-driver: request had no reply cap - dropping (cannot answer without one)");
            }
            continue;
        };
        let p = req.payload_bytes();

        if p.len() == 1 && p[0] == 3 {
            // STATUS: [ok, mac(6), link] - net-stack reads the MAC at [1..7] and the link at [7]. The
            // link is read LIVE over MDIO, so a cable pulled after bring-up reports down.
            let mut out = [0u8; 8];
            out[0] = 1;
            out[1..7].copy_from_slice(&mac);
            // RE-APPLY the link settings when a cable arrives after bring-up.
            //
            // `apply_link_settings` (MAC speed + DMA burst) runs only during `bring_up`. Boot WITH a
            // cable and the PHY has negotiated by then, so the speed is programmed and the receiver
            // works. Boot WITHOUT one and it logs "PHY has not settled on a speed - leaving the MAC at
            // its default", and nothing ever ran it again - so when the cable appeared the MAC was
            // still unclocked and NOTHING was received. Measured, not guessed: net-stack's DHCP dance
            // reported "saw 0 frames" on every hot-plug attempt, against 4-5 frames per attempt on a
            // cable-at-boot run.
            //
            // Done HERE because this is the one place the link is already read live, on the status
            // request net-stack makes before it dances - so the settings are applied a moment before
            // the frames that need them, with no polling added anywhere.
            //
            // Edge-triggered: only on a down -> up TRANSITION. Re-running it on every status request
            // would rewrite MAC registers under live traffic for no reason.
            let up_now = g.link_is_up();
            if up_now && !link_was_up {
                ctx.log("nic-driver: genet link came up after bring-up - re-applying MAC speed and DMA burst");
                // CONSUME THE EDGE ONLY IF THE RE-APPLY ACTUALLY WORKED (audit A5-1, Commandments V
                // and IX). `apply_link_settings` returns 0 when the PHY has not settled - and it reads
                // a DIFFERENT register (the aux status) from `link_is_up`'s BMSR bit, so it can fail on
                // a link that genuinely is up, or on any failed MDIO read. Marking the edge consumed
                // regardless meant one unlucky read left the MAC unclocked FOREVER: nothing received,
                // and no second chance short of a physical replug.
                //
                // A recovery that fails must not be recorded as a recovery that happened. Leaving
                // `link_was_up` false keeps the edge pending, so the next status request tries again.
                if g.apply_link_settings() != 0 {
                    link_was_up = true;
                } else {
                    ctx.log("nic-driver: genet re-apply did not take (PHY not settled?) - leaving the link edge pending to retry");
                }
            } else {
                link_was_up = up_now;
            }
            out[7] = up_now as u8;
            if ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out)).is_err() && !reply_failed_logged {
            reply_failed_logged = true;
            ctx.log("nic-driver: a reply send FAILED - the requester will time out (queue full or peer dead)");
        }
        } else if p.len() == 1 && p[0] == 4 {
            // RX-only: one frame, no TX.
            let n = g.receive(&mut rxbuf);
            if ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])).is_err() && !reply_failed_logged {
            reply_failed_logged = true;
            ctx.log("nic-driver: a reply send FAILED - the requester will time out (queue full or peer dead)");
        }
        } else if p.len() == 1 && p[0] == 9 {
            // BATCH RX drain: [count:u8] then per frame [len:u16 LE][bytes]. Bounded three ways - by
            // BATCH_MAX, by the reply buffer, and by the ring emptying - so it always terminates.
            let mut out = [0u8; BATCH_MSG_MAX];
            let mut opos = 1usize;
            let mut count = 0u8;
            while count < BATCH_MAX {
                // Check a MAX-size frame would fit BEFORE taking one, so a frame is never pulled off
                // the ring only to be dropped for lack of room (userspace-audit A5-U2). net-stack
                // re-polls for whatever we stop short of.
                if opos + 2 + FRAME_MAX > out.len() {
                    break;
                }
                let mut rx = [0u8; FRAME_MAX];
                let n = g.receive(&mut rx);
                if n == 0 {
                    break;
                }
                out[opos..opos + 2].copy_from_slice(&(n as u16).to_le_bytes());
                opos += 2;
                out[opos..opos + n].copy_from_slice(&rx[..n]);
                opos += n;
                count += 1;
            }
            out[0] = count;
            if ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out[..opos])).is_err() && !reply_failed_logged {
            reply_failed_logged = true;
            ctx.log("nic-driver: a reply send FAILED - the requester will time out (queue full or peer dead)");
        }
        } else if p.len() == 1 && matches!(p[0], 5 | 6 | 7 | 8) {
            // UNSUPPORTED on this backend - answered `[0]`, not `[1]`. Ops 6/7/8 are the chaos
            // force-link override and op 5 is a Realtek/e1000-shaped register dump; acking any of them
            // with success would make `chaos link-flap` print that it had exercised link recovery
            // having exercised nothing. A test that cannot fail is worse than absent when it reads as
            // passing. The caller needs an ANSWER, and "not supported here" is one.
            if ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])).is_err() && !reply_failed_logged {
            reply_failed_logged = true;
            ctx.log("nic-driver: a reply send FAILED - the requester will time out (queue full or peer dead)");
        }
        } else {
            // TX FRAME (any multi-byte payload) + coupled RX: transmit, then hand back one frame.
            if !g.transmit(p, &mut tx_next) {
                // A failed transmit must not be dropped on the floor: the reply would still come back
                // normally, so net-stack would wait out its whole deadline for an answer to a frame
                // that never left the host - a send that did not happen, reported as one that did.
                ctx.log_fmt(format_args!(
                    "nic-driver: genet refused a {} byte frame (buffer is {}) - not sent",
                    p.len(), RX_BUF_LENGTH));
            } else if tx_reports < 4 {
                // The counters, on the first few transmits only. See `report_counters`: this is the
                // measurement that splits "we are not sending" from "nothing is answering".
                tx_reports += 1;
                g.report_counters();
            }
            let n = g.receive(&mut rxbuf);
            if ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])).is_err() && !reply_failed_logged {
            reply_failed_logged = true;
            ctx.log("nic-driver: a reply send FAILED - the requester will time out (queue full or peer dead)");
        }
        }
        ctx.remove_cap(reply_cap);
    }
}
