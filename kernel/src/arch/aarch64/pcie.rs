// SPDX-License-Identifier: GPL-2.0-only
//! The BCM2711 PCIe root complex, and finding the VL805 xHCI controller behind it.
//!
//! ## Why the Pi 4 needs this at all, when the Pi 2 did not
//!
//! On the Pi 2 the USB host controller is a DWC2 sitting directly on the SoC's peripheral bus: a fixed
//! MMIO address, no discovery. On the Pi 4 the four USB-A ports are behind a **VIA VL805**, an
//! off-SoC xHCI controller on the far side of a **PCIe Gen2 x1 link**. Nothing about it is at a known
//! address. Before a single USB register can be read, the root complex has to be brought out of reset,
//! its link trained, an address window opened onto the bus, config space reached through it, and the
//! device's BAR assigned somewhere inside that window.
//!
//! That is what this file does, and it stops there. It knows nothing about USB.
//!
//! ## The window lives above 4 GiB, and that is a mapping problem before it is a driver problem
//!
//! The conventional CPU-side window - the one the Pi's own device tree uses, so the one every other OS
//! agrees is free - is at `0x6_0000_0000`. That is outside the kernel's identity/direct map, which
//! covers the low 4 GiB. `mmu` maps it as one sparse Device GiB rather than by widening the map; the
//! reasoning is there, next to the tables.
//!
//! ## Reference
//!
//! Written from u-boot's `drivers/pci/pcie_brcmstb.c` and Linux's `drivers/pci/controller/pcie-brcmstb.c`
//! as executable datasheets, per the driver-porting doctrine in `arch/CLAUDE.md`: the register-init
//! sequence and its quirks are the reusable knowledge; none of the Linux/u-boot integration is. The
//! order below is theirs and is not arbitrary - the SerDes must leave IDDQ before the link can train,
//! and PERST# must be released last or the endpoint never sees a clean reset edge.
//!
//! ## Every wait here is bounded
//!
//! A device that never answers must not hang the boot (invariant 12). Link training, config reads and
//! the reset settling all count out and report, and a failure leaves the machine on serial with the USB
//! ports dead rather than wedged before the shell exists.

use super::{put_dec, put_hex, put_str};

/// The PCIe root-complex register block (BCM2711, low-peripheral mode).
const PCIE_BASE: u64 = 0xFD50_0000;

// --- Root-complex registers (offsets from PCIE_BASE) ------------------------------------------
const RC_CFG_VENDOR_SPECIFIC_REG1: u64 = 0x0188;
const RC_CFG_PRIV1_ID_VAL3: u64 = 0x043c;
const MISC_CTRL: u64 = 0x4008;
const MEM_WIN0_LO: u64 = 0x400c;
const MEM_WIN0_HI: u64 = 0x4010;
const RC_BAR1_CONFIG_LO: u64 = 0x402c;
const RC_BAR2_CONFIG_LO: u64 = 0x4034;
const RC_BAR2_CONFIG_HI: u64 = 0x4038;
const RC_BAR3_CONFIG_LO: u64 = 0x403c;
const PCIE_STATUS: u64 = 0x4068;
const MEM_WIN0_BASE_LIMIT: u64 = 0x4070;
const MEM_WIN0_BASE_HI: u64 = 0x4080;
const MEM_WIN0_LIMIT_HI: u64 = 0x4084;
const HARD_DEBUG: u64 = 0x4204;
const UBUS_BAR2_CONFIG_REMAP: u64 = 0x40b4;
/// The MSI_INTR2 interrupt block. One SPI serves ALL 32 MSIs on this controller, so the handler has
/// to read STATUS to find out which fired and CLR to acknowledge it. A bit left set re-raises the SPI
/// immediately - an unacknowledged level interrupt is a storm, not a missed event.
const MSI_INTR2_STATUS:   u64 = 0x4500;
const MSI_INTR2_CLR:      u64 = 0x4500 + 0x08;
const MSI_INTR2_MASK_SET: u64 = 0x4500 + 0x10;
const MSI_INTR2_MASK_CLR: u64 = 0x4500 + 0x14;

/// Where an endpoint writes to raise an MSI, and what the controller matches on.
const MSI_BAR_CONFIG_LO:  u64 = 0x4044;
const MSI_BAR_CONFIG_HI:  u64 = 0x4048;
const MSI_DATA_CONFIG:    u64 = 0x404c;

/// The MSI target address. It is deliberately an address the ENDPOINT can reach through the inbound
/// window but that decodes to the controller rather than to RAM: an MSI is a posted write the
/// controller intercepts, not a write anything stores. Linux uses this same constant for a
/// controller whose inbound window sits below 4 GB, which is ours.
const MSI_TARGET_ADDR: u64 = 0x0000_0000_FFFF_FFFC;
/// Match-enable, bit 0 of the LO register.
const MSI_BAR_CONFIG_LO_MATCH_EN: u32 = 1;
/// The data pattern the controller matches for a 32-MSI configuration. The low 16 bits are the base
/// the endpoint must send; the MSI index is OR'd into them.
const MSI_DATA_CONFIG_VAL_32: u32 = 0xffe0_6540;

/// PCI capability IDs and the MSI capability's register layout.
const PCI_CAP_ID_MSI: u32 = 0x05;
const PCI_STATUS_CAP_LIST: u32 = 1 << 20;
const EXT_CFG_DATA: u64 = 0x8000;
const EXT_CFG_INDEX: u64 = 0x9000;
const RGR1_SW_INIT_1: u64 = 0x9210;

// --- Field masks ------------------------------------------------------------------------------
const SW_INIT_1_PERST: u32 = 1 << 0;
const SW_INIT_1_INIT_GENERIC: u32 = 1 << 1;
const HARD_DEBUG_SERDES_IDDQ: u32 = 1 << 27;
const MISC_CTRL_SCB_ACCESS_EN: u32 = 1 << 12;
const MISC_CTRL_CFG_READ_UR_MODE: u32 = 1 << 13;
const MISC_CTRL_MAX_BURST_MASK: u32 = 0x3 << 20;
const MISC_CTRL_SCB0_SIZE_SHIFT: u32 = 27;
const STATUS_PHYLINKUP: u32 = 1 << 4;
const STATUS_DL_ACTIVE: u32 = 1 << 5;

/// Where on the PCI bus the window appears. Endpoint BARs are assigned inside this.
const PCI_WIN_BUS_ADDR: u64 = 0xF800_0000;

/// A device found on the bus.
#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    /// CPU physical address the device's BAR0 was assigned. 0 if none.
    pub bar0: u64,
    pub bar0_len: u32,
}

/// The VIA VL805 (and its VL806 sibling), which is what a Pi 4's USB-A ports hang off.
const VIA_VENDOR: u16 = 0x1106;

#[inline]
fn reg(off: u64) -> *mut u32 {
    // The root complex is inside the peripheral window, so `mmio` translates it correctly on both
    // sides of the jump to the high half.
    super::mmio(PCIE_BASE as usize + off as usize) as *mut u32
}

fn rd(off: u64) -> u32 {
    // SAFETY: a volatile read of a root-complex register, in the kernel's Device mapping.
    unsafe { reg(off).read_volatile() }
}

fn wr(off: u64, v: u32) {
    // SAFETY: a volatile write of a root-complex register, in the kernel's Device mapping.
    unsafe { reg(off).write_volatile(v) }
}

/// Busy-wait. The generic timer is already running and its frequency is read from the register, so
/// this is a real duration rather than a spin count - which matters, because every delay below comes
/// from a datasheet in microseconds.
fn delay_us(us: u64) {
    let hz = super::timer::frequency().max(1);
    let ticks = (hz * us) / 1_000_000;
    let start = super::read_cycle_counter();
    while super::read_cycle_counter().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

fn set_bits(off: u64, clear: u32, set: u32) {
    let v = (rd(off) & !clear) | set;
    wr(off, v);
}

/// Assert or release the bridge's internal reset.
fn bridge_reset(assert: bool) {
    let v = rd(RGR1_SW_INIT_1);
    wr(RGR1_SW_INIT_1, if assert { v | SW_INIT_1_INIT_GENERIC } else { v & !SW_INIT_1_INIT_GENERIC });
}

/// Assert or release PERST# to the endpoint.
///
/// **On the BCM2711 this is `RGR1_SW_INIT_1` bit 0, not the `PCIE_CTRL.PERSTB` bit.** Both registers
/// exist and both look plausible; the Broadcom family uses different ones per chip, and the 2711 takes
/// the "generic" path (`brcm_pcie_perst_set_generic` in Linux), which is this register, with 1 meaning
/// ASSERT. Writing the other one leaves the endpoint held in reset with every register read looking
/// healthy - the first bring-up on real hardware did exactly that and reported `status 0x80`: the port
/// bit set, and both link bits clear.
fn perst(assert: bool) {
    let v = rd(RGR1_SW_INIT_1);
    wr(RGR1_SW_INIT_1, if assert { v | SW_INIT_1_PERST } else { v & !SW_INIT_1_PERST });
}

fn link_up() -> bool {
    let s = rd(PCIE_STATUS);
    (s & STATUS_DL_ACTIVE) != 0 && (s & STATUS_PHYLINKUP) != 0
}

/// Encode an inbound-window size the way `RC_BAR2_CONFIG_LO` wants it.
///
/// `log2(size) - 15` for 64 KiB..32 GiB, and 0 (disabled) for anything outside that. The `+ 1` this
/// carried was simply wrong - it opened a window twice the size of RAM. That does not affect the
/// outbound reads being chased here, because this register sizes what a bus master may reach INBOUND,
/// but it would have shown up later as the xHCI controller being able to DMA past the end of memory,
/// which is a security property and not a performance one.
fn encode_ibar_size(size: u64) -> u32 {
    let lg = 63 - size.next_power_of_two().leading_zeros() as u64;
    if !(16..=35).contains(&lg) {
        return 0; // outside what the field can express: leave the window shut
    }
    (lg - 15) as u32
}

// --- Config space -----------------------------------------------------------------------------

/// Read a 32-bit config-space register.
///
/// Bus 0 is the root complex itself and answers from its own register block; anything beyond it goes
/// through the index/data pair. Getting that split wrong reads the RC's own registers and reports the
/// bridge as though it were the endpoint - which looks like a successful enumeration of the wrong
/// device rather than like a failure.
fn cfg_read(bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    if bus == 0 {
        if dev != 0 || func != 0 {
            return 0xFFFF_FFFF; // only one device on the root bus: the RC bridge itself
        }
        return rd(off as u64 & 0xFFF);
    }
    let idx = ((bus as u32) << 20) | ((dev as u32) << 15) | ((func as u32) << 12);
    let _g = CFG_LOCK.lock();
    wr(EXT_CFG_INDEX, idx);
    rd(EXT_CFG_DATA + (off as u64 & 0xFFF))
}

fn cfg_write(bus: u8, dev: u8, func: u8, off: u16, val: u32) {
    if bus == 0 {
        if dev == 0 && func == 0 {
            wr(off as u64 & 0xFFF, val);
        }
        return;
    }
    let idx = ((bus as u32) << 20) | ((dev as u32) << 15) | ((func as u32) << 12);
    let _g = CFG_LOCK.lock();
    wr(EXT_CFG_INDEX, idx);
    wr(EXT_CFG_DATA + (off as u64 & 0xFFF), val);
}

// --- Bring-up ---------------------------------------------------------------------------------

/// Bring the root complex up and return the first VIA xHCI controller found behind it.
///
/// `ram_bytes` sizes the inbound window: it is how much of RAM a bus master (the xHCI controller doing
/// DMA) is allowed to reach. Sizing it from the actual memory map rather than a constant is what keeps
/// a 2 GiB board from opening a window over memory it does not have.
///
/// Returns `None` - loudly, with the stage that failed - rather than panicking. A machine with no USB
/// is a machine you can still use over serial; a panicking one is not.
pub fn init(ram_bytes: u64) -> Option<Device> {
    // Is there a root complex here at all? On a real Pi 4 there is; under QEMU's `raspi4b` there is
    // not, and the difference is NOT a translation fault - the mapping is valid - but an external abort
    // from the interconnect, which at EL1 halts the machine. Probing first turns "this emulator has no
    // PCIe" from a dead boot into one printed line. See `uaccess::probe_read32`.
    //
    // SAFETY: 4-byte aligned, inside the peripheral Device mapping the kernel built.
    let present = unsafe { super::uaccess::probe_read32(reg(RGR1_SW_INIT_1) as u64) };
    if present.is_none() {
        put_str(b"pcie: no root complex at 0xFD500000 (this machine has none) - no USB this boot\r\n");
        return None;
    }

    put_str(b"pcie: bringing up the BCM2711 root complex\r\n");

    // 1. Both resets asserted, then settle. Some firmware leaves PERST# released; starting from a known
    //    state is cheaper than reasoning about which firmware ran.
    bridge_reset(true);
    perst(true);
    delay_us(100);

    // 2. Bridge out of reset, SerDes out of IDDQ, then let the SerDes stabilise. The link cannot train
    //    while the SerDes is powered down, and the failure looks identical to "no card present".
    bridge_reset(false);
    set_bits(HARD_DEBUG, HARD_DEBUG_SERDES_IDDQ, 0);
    delay_us(100);

    // 3. Controller settings: burst size 128 bytes, config reads to a missing device return
    //    all-ones (unsupported-request mode) rather than aborting, and SCB access on.
    set_bits(
        MISC_CTRL,
        MISC_CTRL_MAX_BURST_MASK,
        MISC_CTRL_SCB_ACCESS_EN | MISC_CTRL_CFG_READ_UR_MODE,
    );

    // 4. Inbound window: what a bus master can reach in system RAM, based at 0.
    let size = ram_bytes.next_power_of_two().max(64 * 1024);
    let enc = encode_ibar_size(size);
    wr(RC_BAR2_CONFIG_LO, enc & 0x1F);
    wr(RC_BAR2_CONFIG_HI, 0);
    // SCB0_SIZE uses the SAME encoding as the inbound window, not one less than it. The `enc - 1` this
    // carried was a guess that happened to compile; the reference computes `ilog2(size) - 15` for both.
    // 0x1F is the "no memory controller" value, which is what a size we cannot express should mean.
    let scb = if enc == 0 { 0x1F } else { enc };
    set_bits(MISC_CTRL, 0x1F << MISC_CTRL_SCB0_SIZE_SHIFT, scb << MISC_CTRL_SCB0_SIZE_SHIFT);

    // 5. The other two inbound windows stay shut - nothing here uses them, and an open window is
    //    authority granted to a bus master for no reason.
    set_bits(RC_BAR1_CONFIG_LO, 0x1F, 0);
    set_bits(RC_BAR3_CONFIG_LO, 0x1F, 0);

    // 5b. The inbound window's endian mode and its UBUS access enable. Both are unconditional in the
    //     reference, on every chip it supports, and neither has an obvious symptom when missing - which
    //     is precisely why they are copied rather than reasoned about.
    set_bits(RC_CFG_VENDOR_SPECIFIC_REG1, 0xC, 0x0); // BAR2 endian mode: little
    set_bits(UBUS_BAR2_CONFIG_REMAP, 0, 1); // access enable

    // 6. Mask every MSI. Interrupts are not routed yet - the event ring is polled - and an unmasked
    //    source with no handler is a live interrupt storm waiting for the first packet.
    wr(MSI_INTR2_MASK_SET, 0xFFFF_FFFF);

    // 7. Release PERST# and wait for the link. 100 ms is the figure both references use.
    perst(false);
    let mut ms = 0;
    while ms < 100 && !link_up() {
        delay_us(1000);
        ms += 1;
    }
    if !link_up() {
        put_str(b"pcie: link did NOT train within 100ms (status ");
        put_hex(rd(PCIE_STATUS) as u64);
        put_str(b") - no USB on this boot\r\n");
        return None;
    }
    put_str(b"pcie: link up after ");
    put_dec(ms);
    put_str(b"ms\r\n");

    // 8. Present the RC as a PCI-to-PCI bridge. Enumeration software (including the code below) reads
    //    the class to decide whether to look for a bus behind it. The class code is a 24-bit FIELD in
    //    this register, so it is a masked write - a whole-register write zeroes the byte above it.
    set_bits(RC_CFG_PRIV1_ID_VAL3, 0x00FF_FFFF, 0x0006_0400);

    // 9. Outbound window: CPU addresses that reach the bus.
    let (cpu_base, cpu_size) = super::mmu::pcie_window();
    set_outbound_window(cpu_base, PCI_WIN_BUS_ADDR, cpu_size);

    // 10. Route config cycles to bus 1: primary 0, secondary 1, subordinate 1.
    cfg_write(0, 0, 0, 0x18, 0x0001_0100);

    enumerate(cpu_base, cpu_size)
}

fn set_outbound_window(cpu_addr: u64, bus_addr: u64, size: u64) {
    wr(MEM_WIN0_LO, bus_addr as u32);
    wr(MEM_WIN0_HI, (bus_addr >> 32) as u32);

    // Base and limit are in MiB units: the low register carries bits 31:20 of each, and the two HI
    // registers carry what is left over above 4 GiB. Splitting them is why a window at 0x6_0000_0000
    // needs three writes and not one.
    //
    // **LIMIT occupies bits 31:20 and BASE occupies bits 15:4 - not the other way round.** The name of
    // the register reads "BASE_LIMIT", the fields appear in that order in every description of it, and
    // the obvious guess is that base is the high half. It is not:
    //
    //     PCIE_MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT_LIMIT_MASK  0xfff00000
    //     PCIE_MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT_BASE_MASK   0xfff0
    //
    // Getting it backwards writes a base ABOVE the limit, which is how PCI spells "this window is
    // closed" - so the root complex accepts every register write, reads them all back exactly as
    // written, reports the link up, and forwards nothing. The symptom is a poison value from the far
    // side, indistinguishable from a device that is not answering. Four hardware iterations were spent
    // on that, and none of them could have found it by reasoning: the readback was `0x3f0`, which looks
    // like a plausible small number either way round.
    //
    // Read-modify-write, as the reference does, so bits outside these two fields are preserved.
    let base_mb = cpu_addr >> 20;
    let limit_mb = (cpu_addr + size - 1) >> 20;
    set_bits(
        MEM_WIN0_BASE_LIMIT,
        0xFFF0_FFF0,
        (((limit_mb & 0xFFF) as u32) << 20) | (((base_mb & 0xFFF) as u32) << 4),
    );
    // The upper bits of each are shifted by the WIDTH of the base field (12), not by 20.
    set_bits(MEM_WIN0_BASE_HI, 0xFF, (base_mb >> 12) as u32 & 0xFF);
    set_bits(MEM_WIN0_LIMIT_HI, 0xFF, (limit_mb >> 12) as u32 & 0xFF);
}

/// Read the routing state back and print it.
///
/// Called after everything is configured, because at this point reasoning has been wrong twice: the
/// bridge window was genuinely missing, and the controller had genuinely lost its firmware, and
/// neither fix made the reads land. What has NOT been done is look at what the registers actually hold
/// afterwards - and a firmware call that resets and re-initialises the endpoint is exactly the kind of
/// thing that can put the root complex back the way it found it.
///
/// Every value here was written by this file. Any of them reading back differently says which write did
/// not stick, and that is a different bug from every one considered so far.
fn report_routing() {
    put_str(b"pcie: win0 lo/hi ");
    put_hex(rd(MEM_WIN0_LO) as u64);
    put_str(b"/");
    put_hex(rd(MEM_WIN0_HI) as u64);
    put_str(b" base-limit ");
    put_hex(rd(MEM_WIN0_BASE_LIMIT) as u64);
    put_str(b" hi ");
    put_hex(rd(MEM_WIN0_BASE_HI) as u64);
    put_str(b"/");
    put_hex(rd(MEM_WIN0_LIMIT_HI) as u64);
    put_str(b"\r\n");
    put_str(b"pcie: bridge buses ");
    put_hex(cfg_read(0, 0, 0, 0x18) as u64);
    put_str(b" memwin ");
    put_hex(cfg_read(0, 0, 0, 0x20) as u64);
    put_str(b" cmd ");
    put_hex((cfg_read(0, 0, 0, 0x04) & 0xFFFF) as u64);
    put_str(b" class ");
    put_hex((cfg_read(0, 0, 0, 0x08) >> 8) as u64);
    put_str(b" status ");
    put_hex(rd(PCIE_STATUS) as u64);
    put_str(b"\r\n");
}

/// Serializes config access. INDEX/DATA is ONE global register pair - an access is "latch the
/// selector, then read the data" - so two of them interleaved read each other's device. The kernel's
/// own walk is single-threaded at boot, but `cfg_read_gated` below lets a SERVICE issue config reads
/// on another core at any time, so the pair is now genuinely shared and must be serialized. This is
/// the aarch64 twin of x86's `PCI_CONFIG_LOCK`, which carries the same reasoning.
static CFG_LOCK: crate::smp::SpinLock<()> = crate::smp::SpinLock::new(());

/// The highest bus number the bridge is programmed to forward (see the `0x18` write in `init`:
/// primary 0, secondary 1, subordinate 1).
const SUBORDINATE_BUS: u32 = 1;

/// One complete config read for a capability holder: select and fetch, indivisibly.
///
/// ATOMIC BY CONSTRUCTION, and that is the point. Exposing "write the selector" and "read the data"
/// as two separate operations would let a caller be descheduled between them - or two callers
/// interleave - and each would then read whichever device the OTHER selected. That is a wrong answer
/// with nothing anywhere to say so, which is worse than a refused one (invariant 12). One operation
/// under one lock removes the window entirely, and the caller cannot leave the selector anywhere.
///
/// REFUSES A BUS THE BRIDGE DOES NOT FORWARD, and this is not a nicety. A config read past the
/// subordinate bus is an unsupported request on this root complex: it raises an SError and stalls the
/// interconnect rather than reading back all-ones the way a PC's host bridge does. A service must not
/// be able to halt the machine with an argument, so admissibility is checked HERE - the kernel
/// programmed the bus range and is the only thing that knows it. What the kernel does NOT do is
/// interpret the selector any further: `sel` is the caller's encoding, not a device it vouches for.
///
/// Bus 0 is the bridge's own config header, which answers from the root-complex register block rather
/// than through the window. Masked to that header (`0..0xFFF`), so this cannot reach the control
/// registers further up the block - EXT_CFG_INDEX at 0x9000 or the reset control at 0x9210 - which is
/// exactly why the kernel mediates instead of granting an MMIO page.
pub(super) fn cfg_read_gated(sel: u32, off: u16) -> Option<u32> {
    let bus = (sel >> 20) & 0xFF;
    if bus > SUBORDINATE_BUS {
        return None;
    }
    let o = (off as u64) & 0xFFF;
    if bus == 0 {
        // Bus 0 carries exactly one device: the root complex bridge itself. Its header answers from
        // the register block directly, which has no device/function selector to apply - so without
        // this check every slot on bus 0 would read back the BRIDGE and a caller would report 32 of
        // them. Absent is the truth for every slot but the first.
        if sel & 0x000F_F000 != 0 {
            return Some(0xFFFF_FFFF);
        }
        let _g = CFG_LOCK.lock();
        return Some(rd(o));
    }
    let _g = CFG_LOCK.lock();
    wr(EXT_CFG_INDEX, sel);
    Some(rd(EXT_CFG_DATA + o))
}

/// Walk bus 1 looking for a VIA xHCI controller, and give it a BAR inside the window.
fn enumerate(cpu_base: u64, cpu_size: u64) -> Option<Device> {
    // A Gen2 x1 link to a single soldered-down controller: one device, function 0. Walking all 32
    // slots would be enumeration theatre on a board whose topology is fixed and known.
    for dev in 0..2u8 {
        let id = cfg_read(1, dev, 0, 0x00);
        if id == 0xFFFF_FFFF || id == 0 {
            continue;
        }
        let vendor = (id & 0xFFFF) as u16;
        let device = (id >> 16) as u16;
        let class = cfg_read(1, dev, 0, 0x08) >> 8; // class/subclass/prog-if

        put_str(b"pcie: bus1 dev");
        put_dec(dev as u64);
        put_str(b" vendor ");
        put_hex(vendor as u64);
        put_str(b" device ");
        put_hex(device as u64);
        put_str(b" class ");
        put_hex(class as u64);
        put_str(b"\r\n");

        // RECORD IT, whatever it is, BEFORE the per-class filter below (step D1). The order is the
        // whole point: a table built from what the BUS reported - not from what this kernel happens
        // to care about - is one that can serve a driver for a device the kernel has no name for.
        //
        // BARs are read raw here. On this board the firmware does NOT assign them; the code below
        // assigns BAR0 for the controller it drives, and re-records afterwards with the assigned
        // value. A device nobody drives therefore shows 0, which is the truth about it rather than a
        // guess - and `hw_mmio_of` grants nothing for a 0 BAR, so a driver gets no mapping and says
        // so instead of being pointed somewhere plausible (§26.7).
        {
            let mut bars = [0u64; 6];
            for (i, b) in bars.iter_mut().enumerate() {
                let raw = cfg_read(1, dev, 0, 0x10 + (i as u16) * 4);
                if raw & 0x1 == 0 { *b = (raw & !0xF) as u64; }   // memory BAR; I/O BARs are not usable here
            }
            let irq_line = (cfg_read(1, dev, 0, 0x3C) & 0xFF) as u8;
            let bdf = ((1u32) << 8) | ((dev as u32) << 3);        // bus 1, func 0 - same encoding as x86's make_bdf
            crate::arch::aarch64::pci::record_device(bdf, class, bars, irq_line, vendor, device);
        }

        // Class 0x0C0330 is "serial bus / USB / xHCI". Checking the CLASS rather than the device id is
        // what makes this work on a board that ships a VL806 instead of a VL805.
        if vendor != VIA_VENDOR || class != 0x0C_0330 {
            continue;
        }

        // **Ask the firmware to reload the controller's firmware, before touching its BAR.**
        //
        // The VL805's firmware lives in an SPI EEPROM and is loaded into it by the VideoCore at
        // power-on. The PERST# assertion this bring-up performs wipes it, leaving a device whose config
        // space answers perfectly (that is the PCIe core) and whose memory BAR answers not at all (that
        // is the firmware). See `mailbox::notify_xhci_reset` for why that is worth a paragraph.
        let dev_addr = (1u32 << 20) | ((dev as u32) << 15);
        if super::mailbox::notify_xhci_reset(dev_addr) {
            put_str(b"pcie: firmware notified of the xHCI reset - reloading controller firmware\r\n");
        } else {
            // Not fatal, and not silently ignored: on a board whose firmware predates the tag this is
            // expected, and on one where it should have worked it is the single most likely reason the
            // BAR reads back as poison a few lines from here.
            put_str(b"pcie: firmware REFUSED the xHCI reset notify - the controller may have no firmware\r\n");
        }
        // The reload is not instant, and there is nothing to poll: the tag returns as soon as the
        // request is accepted, not when the controller is ready. Linux waits in the same shape.
        delay_us(200_000);

        // **Re-apply the root complex's own routing after the firmware has been in here.**
        //
        // The notify above does not just hand the controller a blob: the firmware resets and
        // re-initialises the endpoint, and it reaches it through this same root complex. Anything it
        // does to the RC on the way - retraining the link, resetting the bridge, reprogramming the
        // outbound window it wants for its own access - lands on registers this file configured
        // several steps earlier and has not looked at since.
        //
        // Re-applying is cheap and idempotent. Assuming they survived is how a correct sequence
        // produces a dead device, with the log showing every step succeeding.
        set_bits(RC_CFG_PRIV1_ID_VAL3, 0x00FF_FFFF, 0x0006_0400);
        set_outbound_window(cpu_base, PCI_WIN_BUS_ADDR, cpu_size);
        cfg_write(0, 0, 0, 0x18, 0x0001_0100);

        // Size BAR0 the standard way: write all-ones, read back the writable bits.
        cfg_write(1, dev, 0, 0x10, 0xFFFF_FFFF);
        let probe = cfg_read(1, dev, 0, 0x10);
        if probe == 0 || probe == 0xFFFF_FFFF {
            put_str(b"pcie: xHCI BAR0 did not size - skipping\r\n");
            continue;
        }
        let len = (!(probe & !0xF)).wrapping_add(1);
        if (len as u64) > cpu_size {
            put_str(b"pcie: xHCI BAR0 is larger than the outbound window - skipping\r\n");
            continue;
        }

        // Assign it at the base of the window. One device, so there is nothing to pack around it.
        cfg_write(1, dev, 0, 0x10, PCI_WIN_BUS_ADDR as u32);
        cfg_write(1, dev, 0, 0x14, 0);

        // **Open the bridge's memory window, or nothing downstream is reachable.**
        //
        // The root complex is a PCI-to-PCI bridge, and a bridge forwards a memory transaction to its
        // secondary bus only if the address falls inside its Memory Base/Limit window. Assigning the
        // endpoint a BAR without opening that window gives a device that is correctly configured and
        // completely unreachable: every read comes back as the root complex's poison value rather than
        // as an error, so the driver above sees plausible-looking register contents made of `0xDEAD`
        // and computes offsets from them. The first bring-up read `caplen = 0xad` out of `0xDEADDEAD`
        // and took an alignment fault on the operational base it derived - a fault three layers away
        // from the missing register write.
        //
        // Base and limit are in 1 MiB units in bits 31:20 of a 16-bit field, hence the `>> 16` with the
        // low nibble left clear.
        let win_end = PCI_WIN_BUS_ADDR + cpu_size - 1;
        let base_f = ((PCI_WIN_BUS_ADDR >> 16) & 0xFFF0) as u32;
        let limit_f = ((win_end >> 16) & 0xFFF0) as u32;
        cfg_write(0, 0, 0, 0x20, (limit_f << 16) | base_f);
        // Disable the prefetchable window explicitly: base above limit is how PCI spells "closed", and
        // leaving whatever the firmware left there is a second window we did not choose.
        cfg_write(0, 0, 0, 0x24, 0x0000_FFF0);

        // Memory space + bus master, on the BRIDGE and on the endpoint. Bus master is what lets the
        // controller DMA at all; without it the controller initialises, accepts commands, and silently
        // never reads a single ring entry. The command word is masked to 16 bits because the upper half
        // of that dword is the status register, whose bits are write-1-to-clear - writing a read-back
        // value straight back acknowledges errors nothing has looked at.
        let bcmd = cfg_read(0, 0, 0, 0x04) & 0xFFFF;
        cfg_write(0, 0, 0, 0x04, bcmd | 0x6);
        let cmd = cfg_read(1, dev, 0, 0x04) & 0xFFFF;
        cfg_write(1, dev, 0, 0x04, cmd | 0x6);

        // Read the BAR back rather than assume the write took. A BAR that did not stick is the same
        // silence as a bridge window that was never opened, and the two want different fixes.
        // MSI, before the BAR is reported and long before any driver touches the controller. The
        // endpoint must know where to post an interrupt before it is ever asked to generate one.
        //
        // Unmasked immediately, and that is safe BECAUSE the handler acknowledges unconditionally:
        // `msi_take_pending` clears whatever is set whether or not a service is listening, so an MSI
        // arriving before the `xhci` service exists is absorbed rather than left to re-raise a
        // level-triggered SPI forever. Waiting for the service to spawn would be the more obvious
        // order and the wrong one - it leaves a window where the endpoint can post and nothing acks.
        if enable_msi(1, dev, 0) {
            unmask_msi();
            put_str(b"pcie: xHCI MSI enabled - the driver waits on interrupts
");
        }

        let bar_back = cfg_read(1, dev, 0, 0x10) & !0xF;
        put_str(b"pcie: xHCI BAR0 assigned bus ");
        put_hex(bar_back as u64);
        put_str(b" -> CPU ");
        put_hex(cpu_base);
        put_str(b" (");
        put_dec(len as u64);
        put_str(b" bytes), bridge window ");
        put_hex(PCI_WIN_BUS_ADDR);
        put_str(b"..");
        put_hex(win_end);
        put_str(b", command ");
        put_hex((cfg_read(1, dev, 0, 0x04) & 0xFFFF) as u64);
        put_str(b"\r\n");
        if bar_back as u64 != PCI_WIN_BUS_ADDR {
            put_str(b"pcie: BAR0 did not take the address it was given - not usable\r\n");
            continue;
        }

        // Everything that decides whether a read reaches the device, read back from the hardware.
        report_routing();

        // And then the question itself, asked so the ANSWER distinguishes the remaining possibilities
        // rather than merely restating the symptom.
        //
        // A poison value on its own is ambiguous: it is what the root complex returns when it forwards
        // a read and nobody completes it, AND what comes back when nothing decodes the address at all.
        // Those want opposite fixes. The bridge's **secondary status** register settles it, because a
        // bridge that forwarded a transaction and received no completer records a Master Abort. So the
        // bit is cleared, the read is made, and the bit is read back.
        //
        // Secondary status is the upper half of the dword at config offset 0x1C, and its error bits are
        // write-1-to-clear.
        let sec = cfg_read(0, 0, 0, 0x1C);
        cfg_write(0, 0, 0, 0x1C, sec | 0xFFFF_0000);

        // SAFETY: 4-byte aligned, inside the Device mapping `mmu` built for this window.
        let first = unsafe { super::uaccess::probe_read32(super::mmu::PCIE_WIN_VA) };
        let sec_after = (cfg_read(0, 0, 0, 0x1C) >> 16) & 0xFFFF;
        const SEC_MASTER_ABORT: u32 = 1 << 13;

        put_str(b"pcie: first dword through the window: ");
        match first {
            None => put_str(b"ABORTED - the CPU address is not decoded to PCIe at all\r\n"),
            Some(v) => {
                put_hex(v as u64);
                if v == 0xDEAD_DEAD || v == 0xFFFF_FFFF {
                    if sec_after & SEC_MASTER_ABORT != 0 {
                        put_str(b" - the bridge FORWARDED it and no device completed it: the endpoint \
                                  is not decoding its BAR\r\n");
                    } else {
                        put_str(b" - the bridge did NOT forward it (no master abort recorded): the \
                                  outbound window or the bridge window is not routing\r\n");
                    }
                } else {
                    put_str(b" - the read arrived\r\n");
                }
            }
        }
        put_str(b"pcie: bridge secondary status ");
        put_hex(sec_after as u64);
        put_str(b", device status ");
        put_hex(((cfg_read(1, dev, 0, 0x04) >> 16) & 0xFFFF) as u64);
        put_str(b", device bar0 ");
        put_hex(cfg_read(1, dev, 0, 0x10) as u64);
        put_str(b", device cmd ");
        put_hex((cfg_read(1, dev, 0, 0x04) & 0xFFFF) as u64);
        put_str(b"\r\n");

        // Re-record with the BAR this code just ASSIGNED. The first record above captured the raw
        // config space, where BAR0 reads 0 because the firmware on this board assigns nothing - so
        // without this the table would hold a truthful-but-useless 0 for the one device we drive.
        //
        // `cpu_base` (not the bus address) because that is what a driver needs to map, and it is what
        // the x86 table stores for every device: one meaning for the field on both ports.
        {
            let bdf = ((1u32) << 8) | ((dev as u32) << 3);
            let mut bars = [0u64; 6];
            bars[0] = cpu_base;
            let irq_line = (cfg_read(1, dev, 0, 0x3C) & 0xFF) as u8;
            crate::arch::aarch64::pci::record_device(bdf, 0x0C_0330, bars, irq_line, vendor, device);
        }

        // CROSS-CHECK, printed rather than asserted. The generic table and the `Device` returned here
        // are two independent views of one truth, and they must agree before anything is switched over
        // to the table. This is not ceremony: the identical check on x86 caught the AHCI BAR at once -
        // the table recorded BAR0 while AHCI's registers are in BAR5, and the log said so on the first
        // boot rather than in a driver that mapped the wrong window.
        //
        // It PRINTS because this port cannot be tested in QEMU (no VL805 emulation), so the first real
        // evidence arrives from a Pi 4 boot log. A mismatch here is the thing to look for.
        if let Some(d) = crate::arch::aarch64::pci::find_by_class(0x0C_0330) {
            let agree = d.bar[0] == cpu_base && d.vendor == vendor && d.device == device;
            crate::kprintln!(
                "pcie: device table - {} device(s); xHCI class 0x0c0330 {} (BAR0 {:#x} vs assigned {:#x}, BDF {:#06x})",
                crate::arch::aarch64::pci::DEVICE_COUNT.load(core::sync::atomic::Ordering::Relaxed),
                if agree { "AGREES" } else { "DISAGREES - do not trust the table" },
                d.bar[0], cpu_base, d.bdf);
        } else {
            crate::kprintln!("pcie: device table has NO 0x0c0330 entry though one was just driven - table is wrong");
        }

        return Some(Device {
            bus: 1,
            dev,
            func: 0,
            vendor,
            device,
            bar0: cpu_base,
            bar0_len: len,
        });
    }

    put_str(b"pcie: no xHCI controller found on bus 1 - no USB on this boot\r\n");
    None
}

/// Turn the controller's MSI receiver on, and program `bdf`'s MSI capability to use index 0.
///
/// Returns `true` if the endpoint had an MSI capability and it was programmed.
///
/// ## Why this is the fix and the polling was not
///
/// Until now `MSI_INTR2_MASK_SET` masked every MSI at init, with the comment "interrupts are not
/// routed yet - the event ring is polled". So the `xhci` service could only discover an event by
/// waking on a timer and looking, which is a floor on input latency no amount of tuning removes:
/// the wait is not waiting FOR anything, it is waiting BEFORE looking.
///
/// ## The order matters
///
/// The controller's receiver is set up FIRST and the endpoint is enabled LAST. An endpoint allowed
/// to post an MSI before the controller knows the target address writes to an address nothing
/// claims; on this SoC that is an unclaimed posted write, not a fault, so it would be lost silently
/// and the symptom would be "interrupts never arrive" with everything looking configured.
///
/// The MSI stays MASKED here regardless. Unmasking is `unmask_msi`, called only once a handler is
/// installed - an unmasked source with nothing to acknowledge it is a level interrupt that re-fires
/// forever, which is worse than the polling it replaces.
pub fn enable_msi(bus: u8, dev: u8, func: u8) -> bool {
    // 1. Controller side: where MSIs land, and what data pattern identifies them.
    wr(MSI_BAR_CONFIG_LO, (MSI_TARGET_ADDR as u32) | MSI_BAR_CONFIG_LO_MATCH_EN);
    wr(MSI_BAR_CONFIG_HI, (MSI_TARGET_ADDR >> 32) as u32);
    wr(MSI_DATA_CONFIG, MSI_DATA_CONFIG_VAL_32);
    // Acknowledge anything stale before a handler can see it, and keep everything masked.
    wr(MSI_INTR2_CLR, 0xFFFF_FFFF);
    wr(MSI_INTR2_MASK_SET, 0xFFFF_FFFF);

    // 2. Endpoint side: find its MSI capability by walking the capability list.
    if cfg_read(bus, dev, func, 0x04) & PCI_STATUS_CAP_LIST == 0 {
        put_str(b"pcie: endpoint reports no capability list - cannot use MSI\r\n");
        return false;
    }
    let mut off = (cfg_read(bus, dev, func, 0x34) & 0xFC) as u16;
    // Bounded walk: a malformed or looping Next pointer must not spin the boot (§26.6).
    for _ in 0..48 {
        if off < 0x40 {
            break;
        }
        let hdr = cfg_read(bus, dev, func, off);
        if hdr & 0xFF == PCI_CAP_ID_MSI {
            // Message Control is the upper half of the capability's first dword. Bit 7 = 64-bit
            // address capable, which decides where the Data register sits - getting that wrong
            // writes the vector into the address's high half and the interrupt never arrives.
            let mc = (hdr >> 16) & 0xFFFF;
            let is64 = mc & (1 << 7) != 0;
            cfg_write(bus, dev, func, off + 0x04, MSI_TARGET_ADDR as u32);
            let data_off = if is64 {
                cfg_write(bus, dev, func, off + 0x08, (MSI_TARGET_ADDR >> 32) as u32);
                off + 0x0C
            } else {
                off + 0x08
            };
            // Index 0. The controller matches the low 16 bits of its data pattern, and the MSI
            // number is OR'd in - so index 0 is the pattern itself.
            cfg_write(bus, dev, func, data_off, MSI_DATA_CONFIG_VAL_32 & 0xFFFF);
            // Enable, and request exactly ONE vector (Multiple Message Enable = 0). Asking for more
            // than the handler demultiplexes is how a driver receives interrupts it never handles.
            let mc_new = (mc & !(0x7 << 4)) | 1;
            cfg_write(bus, dev, func, off, (hdr & 0xFFFF) | (mc_new << 16));
            put_str(b"pcie: endpoint MSI capability programmed (index 0, masked until a handler is installed)\r\n");
            return true;
        }
        let next = (hdr >> 8) & 0xFF;
        if next == 0 {
            break;
        }
        off = next as u16;
    }
    put_str(b"pcie: endpoint has no MSI capability - falling back to the polled path\r\n");
    false
}

/// Unmask MSI index 0. Called only after a handler exists to acknowledge it.
pub fn unmask_msi() {
    wr(MSI_INTR2_CLR, 0xFFFF_FFFF);
    wr(MSI_INTR2_MASK_CLR, 1);
    // And ENABLE THE SPI AT THE GIC. Unmasking at the controller only decides whether it drives its
    // output; the distributor decides whether that output reaches a core, and by default it does not.
    //
    // This was the missing line. Everything else - target address, data pattern, the endpoint's own
    // MSI capability, the demultiplexing handler - was correct, and not one interrupt was ever
    // delivered because the SPI sat masked in the distributor. Only the timer PPI had ever been
    // enabled here, so this port had never needed the SPI path and nothing revealed the gap.
    super::gic::enable(super::exceptions::PCIE_MSI_SPI);
    // Record the route as EDGE-triggered. An MSI is a posted write, not a held line, so there is
    // nothing to mask while the driver works - and masking it would be worse than pointless: the
    // xhci service does not call `irq_unmask`, so USB interrupts would stop after the first one and
    // the driver would fall back to polling with nothing saying why. Registering it states that
    // explicitly rather than leaving it to an absent table entry.
    super::ioapic::register_route(
        super::exceptions::XHCI_MSI_VECTOR, super::exceptions::PCIE_MSI_SPI, false);
}

/// Read and ACKNOWLEDGE the pending MSI set. Returns the bits that were pending.
///
/// Clearing is not optional and not deferrable: the SPI this serves is level-triggered, so a bit
/// left set re-raises it the instant the handler returns. That is an interrupt storm, and it would
/// present as a core that never makes progress - which the liveness watchdog turns into a panic.
pub fn msi_take_pending() -> u32 {
    let status = rd(MSI_INTR2_STATUS);
    if status != 0 {
        wr(MSI_INTR2_CLR, status);
    }
    status
}
