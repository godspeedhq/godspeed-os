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
//! `map_fixed_device` table stays as it is until the boot log says which window to name, because
//! that table's own comment records the cost of getting this wrong: a service handed a range whose first
//! register read is an external abort dies on that read, and the supervisor respawns it forever.
//!
//! **THE RESIDUAL RISK, stated rather than glossed.** An abort-safe read protects against an address
//! that decodes to nothing. It does not protect against an address that decodes to a DIFFERENT
//! peripheral, where a read could in principle have a side effect. The two offsets read here are the
//! documented SD-controller region of this SoC and the registers chosen are SDHCI's read-only ones, so
//! the exposure is small - but it is not zero, and it is the reason this reads two registers rather
//! than sweeping a range.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::uaccess;

/// In-repo: `mmu.rs` and `PL011_BASE` both derive from the BCM2711 low-peripheral base.
const PERIPHERAL_BASE: u64 = 0xFE00_0000;

/// Did the Arasan answer the census? **This is what gates the MMIO grant**, and it is the whole reason
/// the census runs before `map_fixed_device` is ever consulted: that table's comment records what
/// granting a window on a board without the device costs - the service's first register read is an
/// external abort, it dies on it, and the supervisor respawns it forever. A probe result is the only
/// honest gate, because QEMU's `raspi4b` emulates no Arasan at all.
static RADIO_CTRL: AtomicBool = AtomicBool::new(false);

/// The controller's base clock in Hz, from the VideoCore, or 0 if the firmware declined to say.
///
/// Zero is a REFUSAL rather than a default, and `sdhci.rs` is emphatic about why: every card clock
/// derives from this, the Arasan's own CAPS base-clock field is garbage on this family (Linux marks it
/// `missing_caps`), and a hardcoded guess runs the 400 kHz identification clock several times too fast
/// - so nothing ever answers, on hardware only, while emulation ignores clocks and passes.
static BASE_CLOCK: AtomicU32 = AtomicU32::new(0);

/// Whether a controller that could be the radio answered at boot. The spawn path's MMIO grant reads
/// this; nothing else should.
pub fn radio_present() -> bool { RADIO_CTRL.load(Ordering::Acquire) }

/// The Arasan's base clock in Hz (0 = the firmware said nothing, and the driver must refuse).
pub fn base_clock_hz() -> u32 { BASE_CLOCK.load(Ordering::Acquire) }

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

// BCM2711 GPIO, for the SD1 pin mux. Same block `gpio_init_uart` uses; the offsets below are the two
// registers GPIO34-39 live in.
const GPIO_BASE: u64 = PERIPHERAL_BASE + 0x20_0000;
/// Function select for GPIO30-39: ten 3-bit fields, so pin 34 is field 4 and pin 39 is field 9.
const GPFSEL3: u64 = 0x0C;
/// BCM2711 pull control, 2 bits per pin, REG2 covering GPIO32-47 - so pin 34 is bits [5:4] and pin 39
/// is bits [15:14]. **Not the BCM2835 mechanism**: the older SoC strobed GPPUD + GPPUDCLK, and porting
/// the Pi 2's pull code verbatim here does nothing at all (`GPIO_PUP_PDN_CNTRL_REG0` says the same).
const GPIO_PUP_PDN_CNTRL_REG2: u64 = 0xE4 + 2 * 4;

/// Mux GPIO34-39 to ALT3 - the Arasan's **SD1** interface, which is the only way the radio is reached.
///
/// This is BOARD-LEVEL pin muxing and so belongs here rather than in the driver, for the reason arm32's
/// `sd_route_to_emmc` already argues at length: a driver service is granted its own controller's
/// registers and nothing else (§12.3), and the GPIO block is not its to touch. Same shape as that
/// function, one SoC generation newer - different function-select register, and the BCM2711's direct
/// pull registers instead of the BCM2835 strobe.
///
/// **The read-back is logged BEFORE the write**, exactly as on arm32, because it is the one fact that
/// distinguishes "the radio was muxed away from us" from "it is ours and something else is wrong". On
/// the Pi 4 the firmware is expected to have done this already (it is what Linux finds), so reading
/// back ALT3 is the predicted case and a disagreement is the interesting one.
///
/// Pull-UPS on all six: SDIO idles high on CMD and DAT, and an undriven input beside a switching clock
/// frames noise into well-formed responses with no error flag - the failure `gpio_init_uart` records
/// for the UART's RX line, which cost real time on the Pi 2.
fn route_pins_to_arasan() {
    // SAFETY: the BCM2711 GPIO block, reached through the kernel's Device peripheral mapping (the
    // census runs after the jump to the high half, so `mmio()` is the translation - a raw physical
    // address here is unmapped). Read-modify-write touching only GPIO34-39's fields in each register.
    unsafe {
        let fsel = (super::mmio((GPIO_BASE + GPFSEL3) as usize)) as *mut u32;
        let before = fsel.read_volatile();
        super::put_str(b"sdio: GPIO34-39 fsel=");
        let mut all3 = true;
        for pin in 34..40u32 {
            let f = (before >> ((pin - 30) * 3)) & 7;
            super::put_str(&[b'0' + (f as u8 & 7)]);
            if f != 7 { all3 = false; }
        }
        super::put_str(if all3 {
            b" (ALT3 = Arasan SD1, the firmware already routed the radio to us)
" as &[u8]
        } else {
            b" (NOT all ALT3 - the radio was muxed away from the Arasan; routing it back)
"
        });

        let mut v = before;
        for pin in 34..40u32 {
            let sh = (pin - 30) * 3;
            v = (v & !(7 << sh)) | (7 << sh); // ALT3
        }
        fsel.write_volatile(v);

        let pud = (super::mmio((GPIO_BASE + GPIO_PUP_PDN_CNTRL_REG2) as usize)) as *mut u32;
        let mut p = pud.read_volatile();
        for pin in 34..40u32 {
            let sh = (pin - 32) * 2;
            // PULL-UP ON ALL SIX, which DIVERGES from the vendor's own pin config and is recorded rather
            // than silently matched. The Pi device tree's `sdio_pins` sets `brcm,pull = <0 2 2 2 2 2>`:
            // no pull on CLK (34), pull-up on CMD and DAT0-3. A pull-up on a line the controller DRIVES
            // is unlikely to matter, and changing it in the same boot as the readback below would muddy
            // that measurement. Known difference, not an oversight.
            //
            // Note the encodings are not the same either: `brcm,pull` uses BCM2835's 2 = up, while this
            // BCM2711 register uses 01 = up. Porting the numbers rather than the meaning would set
            // pull-DOWN on every line.
            p = (p & !(3 << sh)) | (1 << sh); // 01 = pull-up
        }
        pud.write_volatile(p);

        // READ BOTH BACK. Writing a register and not checking it took is the mistake that cost several
        // boots on the SDIO backplane window, and this is the same shape: the line above logs what the
        // mux WAS and nothing confirmed what it BECAME.
        //
        // It also fits the fault being chased better than anything else left. CLK and CMD are evidently
        // working - commands complete and responses arrive - and a card that accepted a read IS driving
        // DAT0. If pin 36 did not take ALT3, the controller is not connected to that line: it starts the
        // transfer, sees nothing, and sits in `DAT Line Active` forever, which is exactly what the
        // driver measured (active from the first poll to the last, no data, no error, no timeout).
        let fsel_after = fsel.read_volatile();
        let pull_after = pud.read_volatile();
        super::put_str(b"sdio: GPIO34-39 after: ");
        let mut all_alt3 = true;
        for pin in 34..40u32 {
            let f = (fsel_after >> ((pin - 30) * 3)) & 7;
            let pl = (pull_after >> ((pin - 32) * 2)) & 3;
            // `34=f3/p1` - the pin, its function, its pull. One group per pin so a single wrong pin is
            // visible rather than hidden in a digit string.
            super::put_dec(pin as u64);
            super::put_str(b"=f");
            super::put_str(&[b'0' + (f as u8 & 7)]);
            super::put_str(b"/p");
            super::put_str(&[b'0' + (pl as u8 & 3)]);
            super::put_str(b" ");
            if f != 7 {
                all_alt3 = false;
            }
        }
        super::put_str(if all_alt3 {
            b"- all six ALT3, so CLK/CMD/DAT0-3 are ALL connected to the Arasan\r\n" as &[u8]
        } else {
            b"- NOT all ALT3: a pin did not take, and if it is 36 then DAT0 is not wired to the \
              controller at all, which is why a data phase waits forever\r\n"
        });
    }
}

/// Ask the VideoCore to power the SD domain ON, and print what it says.
///
/// **Insurance with a printed answer, not an assumption.** On the Pi 2 this call is the difference
/// between a working card and the exact symptom this port would otherwise chase: the Arasan's
/// registers answer perfectly while no command ever completes. Whether device id 0 covers THIS
/// controller's domain on a BCM2711 is not something this author knows - which is why the firmware's
/// reply is logged rather than believed. Costs one mailbox call at boot either way.
fn power_on() {
    const TAG_SET_POWER_STATE: u32 = 0x0002_8001;
    let mut req = [0u32; 8];
    req[0] = 8 * 4;
    req[1] = 0;
    req[2] = TAG_SET_POWER_STATE;
    req[3] = 8;
    req[4] = 0;
    req[5] = 0;    // device id 0 = SD card
    req[6] = 0b11; // ON | WAIT (block until powered and stable)
    req[7] = 0;
    super::put_str(b"sdio: SET_POWER_STATE(SD, ON|WAIT) -> ");
    match super::mailbox::property_call(&mut req) {
        Some(()) => {
            // bit0 = on, bit1 = "no such device".
            let state = req[6];
            if state & 2 != 0 {
                super::put_str(b"the firmware says NO SUCH DEVICE
");
            } else if state & 1 != 0 {
                super::put_str(b"on
");
            } else {
                super::put_str(b"OFF - the firmware accepted the tag and left it off
");
            }
        }
        None => super::put_str(b"the firmware REJECTED the tag (QEMU stubs it)
"),
    }
}

/// Ask the VideoCore for the Arasan's base clock and record it for `emmc_base_clock_hz`.
///
/// Clock id 1 = EMMC, which is the ARASAN's clock on this family - arm32 asks for exactly this and it
/// is what makes the Pi 2's card work. The Pi 4's second controller (`emmc2`, the SD card) has its own
/// id 12 and is not what this driver touches.
fn read_base_clock() {
    const TAG_GET_CLOCK_RATE: u32 = 0x0003_0002;
    let mut req = [0u32; 8];
    req[0] = 8 * 4;
    req[1] = 0;
    req[2] = TAG_GET_CLOCK_RATE;
    req[3] = 8;
    req[4] = 4;
    req[5] = 1; // clock id 1 = EMMC (the Arasan)
    req[6] = 0;
    req[7] = 0;
    super::put_str(b"sdio: Arasan base clock ");
    match super::mailbox::property_call(&mut req) {
        Some(()) if req[6] != 0 => {
            BASE_CLOCK.store(req[6], Ordering::Release);
            super::put_dec(req[6] as u64);
            super::put_str(b" Hz
");
        }
        _ => {
            // Left at 0 on purpose. The driver refuses rather than guessing, because a divider computed
            // from a wrong base is a silent hardware-only failure (`sdhci.rs`, the same lesson).
            super::put_str(
                b"UNKNOWN - the firmware gave no rate, so the driver will REFUSE to set a card                   clock rather than guess one
",
            );
        }
    }
}

/// Census both candidates at boot and print what each says. Grants nothing.
///
/// Called from the pi4 boot path beside the GENET and PCIe probes, and gated to that board: the QEMU
/// `virt` variant has no Pi peripherals at all, so there is nothing to ask.
pub fn census() {
    super::put_str(b"sdio: BCM2711 SD host controller census - which one is the WiFi radio behind?\r\n");
    // The three board-level facts BEFORE the probe, in the order the hardware needs them: power the
    // domain, learn the clock, route the pins. Each prints what it got, so one boot log says which of
    // them is the problem when the radio does not answer - rather than leaving a silent CMD5 timeout
    // to be attributed by guesswork.
    power_on();
    read_base_clock();
    route_pins_to_arasan();
    let mut found = 0u32;
    let mut arasan = false;
    for c in CANDIDATES {
        if ask(c) {
            found += 1;
            // ONLY the Arasan gates the grant. `emmc2` answering is interesting (it is the SD card the
            // board booted from) and is not a licence to hand anybody a window.
            if c.off == 0x30_0000 { arasan = true; }
        }
    }
    RADIO_CTRL.store(arasan, Ordering::Release);
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
    // WHAT THIS BOOT WILL AND WILL NOT DO, said plainly, because the honest scope of the grant is
    // narrow and easy to overstate. The census no longer grants NOTHING - the Arasan answering is what
    // lets `map_fixed_device` grant that window by kind `WIFI_SDIO` (to `wifi-driver`), which is the change this line
    // used to deny. What is still unproven is the RADIO: a controller answering its version register
    // says a host controller is there, not that a CYW43455 is behind it. CMD5 is what says that, and
    // it happens in the service.
    if arasan {
        super::put_str(
            b"sdio: the Arasan answered, so `wifi-driver` will be granted 0xFE300000 at spawn. \
              That a RADIO is behind it is still unproven - CMD5 in the service is what says so\r\n",
        );
    } else {
        super::put_str(
            b"sdio: the Arasan did NOT answer, so `wifi-driver` is granted NOTHING and will report \
              no radio. A window on a board without the device is a service that dies on its first \
              register read, forever\r\n",
        );
    }
}
