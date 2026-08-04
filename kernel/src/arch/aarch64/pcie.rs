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
const MSI_INTR2_MASK_SET: u64 = 0x4500 + 0x10;
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

/// Encode an inbound-window size the way `RC_BAR2_CONFIG_LO` wants it: `log2(size) - 15`, for sizes
/// from 64 KiB up.
fn encode_ibar_size(size: u64) -> u32 {
    if size < 64 * 1024 {
        return 0;
    }
    let lg = 63 - size.next_power_of_two().leading_zeros() as u64;
    (lg - 15) as u32 + 1
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
    let scb = if enc >= 1 { enc - 1 } else { 0 };
    set_bits(MISC_CTRL, 0x1F << MISC_CTRL_SCB0_SIZE_SHIFT, scb << MISC_CTRL_SCB0_SIZE_SHIFT);

    // 5. The other two inbound windows stay shut - nothing here uses them, and an open window is
    //    authority granted to a bus master for no reason.
    set_bits(RC_BAR1_CONFIG_LO, 0x1F, 0);
    set_bits(RC_BAR3_CONFIG_LO, 0x1F, 0);

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
    //    the class to decide whether to look for a bus behind it.
    wr(RC_CFG_PRIV1_ID_VAL3, 0x0604_00);

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
    let base_mb = cpu_addr >> 20;
    let limit_mb = (cpu_addr + size - 1) >> 20;
    wr(
        MEM_WIN0_BASE_LIMIT,
        (((base_mb & 0xFFF) as u32) << 20) | (((limit_mb & 0xFFF) as u32) << 4),
    );
    wr(MEM_WIN0_BASE_HI, (base_mb >> 12) as u32 & 0xFF);
    wr(MEM_WIN0_LIMIT_HI, (limit_mb >> 12) as u32 & 0xFF);
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
