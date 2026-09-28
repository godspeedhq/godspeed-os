// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK's audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a grep cannot
// - unsafe produced by a macro, or spelled across lines. `deny` rather than `forbid` for exactly one
// reason: the exported `service_main` symbol needs `#[allow(unsafe_code)]`, because a `#[no_mangle]`
// declaration is itself covered by this lint (a colliding symbol is a soundness hole). `forbid` cannot
// be relaxed even there.
#![deny(unsafe_code)]
//! `wifi-driver` - the Raspberry Pi 4's onboard radio, as a userspace service.
//!
//! **Phase 1 step 1 of `docs/wifi.md`: reach the radio and name it.** This service owns the Arasan SD
//! host controller at `0xFE30_0000`, which on this board is not a card slot - it is the SDIO bus the
//! CYW43455 WiFi part sits on. The kernel grants that one page of registers at spawn, by name, and
//! only where its boot census saw the controller answer. Nothing here does networking yet: it brings
//! the controller up, identifies what is on the bus, reads the card's own CIS, and says whether the
//! manufacturer and device codes are the ones this board is documented to carry.
//!
//! That is a deliberately small step, and it is the one that answers a question no amount of reading
//! settles. The device tree says the radio is on this controller; the kernel census says a controller
//! is there; **only CMD5 and the CIS say a radio is.** If they say it is not, one boot log names which
//! of the three board-level preconditions failed - the power domain, the pin mux, or the bus itself -
//! because the kernel prints all three before this service starts.
//!
//! ## What this service deliberately does NOT do yet
//!
//! No firmware upload, so no 802.11 of any kind. The CYW43455 carries its own processor with no ROM
//! firmware for the MAC: until a host uploads `nonfree/brcm43455/brcmfmac43455-sdio.bin` into it there
//! is nothing inside to talk to (`docs/wifi.md` section 8, `docs/licensing.md` section 5a). It also
//! serves no frames: `net-stack` reaches the link through `nic-driver`, and this service is not in
//! that path yet. So every request it receives is ANSWERED with "unavailable" rather than queued or
//! dropped - a missing capability must return loudly, never hang (the rule above the rules).
//!
//! ## Reference
//!
//! Written from the SDIO specification's identification sequence and the behaviour of Linux's
//! `drivers/mmc/core/sdio.c` and `drivers/mmc/host/sdhci-iproc.c`, read as executable datasheets
//! (§26.14): what the silicon needs written, in what order, and what it does when you get it wrong. No
//! code is taken from either. The register-level lessons about this specific controller come from
//! `services/block-driver/src/sdhci.rs`, which drives the same Arasan block on the Pi 2.

#![no_std]
#![no_main]

mod aicore;
mod armcr4;
mod backplane;
mod bus;
mod firmware;
mod erom;
mod host;
mod sdio;
mod upload;

use godspeed_sdk::{Message, ServiceContext};

/// Once identification is over, this is the clock to run at.
///
/// 25 MHz is SDIO default speed - the mode any card must support without a high-speed negotiation this
/// driver does not perform. Raising it is a later phase's business, and asking for a mode we have not
/// enabled is how a working bus becomes an intermittent one.
const OPERATING_HZ: u32 = 25_000_000;

/// Serve forever, answering every request with one byte that means "not available".
///
/// **Answering matters more than what is answered.** A registered service that recv's and never
/// replies leaves its caller waiting out a deadline for a request already decided against, and a
/// service that never recv's at all sits at 16/16 on its queue forever - the flood-endpoint disease.
/// `recv` BLOCKS, so the core still reaches its idle path between messages and this costs nothing
/// while nobody is calling.
fn serve(ctx: &ServiceContext) -> ! {
    loop {
        let _req = ctx.recv();
        // No reply cap means there is nothing to answer on, and dropping is all that is left.
        if let Some(reply) = ctx.take_pending_cap() {
            // One byte, not an empty message: the kernel refuses a zero-length send, so an "empty
            // reply" is no reply at all and the caller waits out its deadline.
            let _ = ctx.try_send_by_handle(reply, &Message::from_bytes(&[1u8]));
        }
    }
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column and
    // every metric published lands under a blank owner.
    ctx.trace_as("wifi-driver");

    // ---- Stage 1: the register window. -------------------------------------------------------------
    // Numbered because this IS a sequence and each stage can only be reached through the one before
    // it, which is what makes the last line printed the diagnosis.
    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => {
            // Not a fault, and it must not read as one. The kernel grants this window only where its
            // census saw the Arasan answer, so on any other board - including QEMU's `raspi4b`, which
            // emulates no Arasan at all - arriving here is the correct outcome.
            ctx.log(
                "wifi-driver: no SDIO register window was granted, so there is no radio to drive on \
                 this machine. The kernel grants it only where its boot census saw the controller \
                 answer - look for the `sdio:` lines above",
            );
            serve(&ctx);
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 1 - granted {} byte(s) of SDIO host registers",
        mmio.len()
    ));

    // ---- Stage 2: the host controller. -----------------------------------------------------------
    // The base clock comes from the platform, not from the controller: the Arasan reports it wrongly
    // in CAPABILITIES on this family (Linux carries `.missing_caps = true` for exactly this part), and
    // a divider from a wrong base runs the identification clock at the wrong speed so that nothing
    // answers - silently, and on hardware only. 0 means the platform declined to say, and the host
    // layer refuses rather than guessing.
    let base = ctx.emmc_base_clock_hz();
    let h = host::Host::new(&mmio, base);
    // Print the version register the KERNEL identified this controller by. If the two numbers
    // disagree, the grant is pointed somewhere other than where the census looked, and this is the one
    // line where both are visible.
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 2 - SLOTISR_VER={:#010x} (the kernel's census read this same register)",
        h.version_reg()
    ));
    if !h.reset(&ctx) {
        ctx.log("wifi-driver: the host controller did not come up, so nothing further was attempted");
        serve(&ctx);
    }

    // ---- Stage 3: what is on the bus. ------------------------------------------------------------
    let card = match sdio::identify(&h, &ctx) {
        Some(c) => c,
        None => {
            ctx.log(
                "wifi-driver: no SDIO card answered on this bus. The controller is ours and came up, \
                 so the remaining suspects are the ones the kernel reports at boot: the SD power \
                 domain and the GPIO34-39 mux",
            );
            serve(&ctx);
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 3 - an SDIO card with {} function(s) at RCA {:#06x}, I/O OCR {:#08x}{}",
        card.funcs,
        card.rca,
        card.ocr,
        if card.memory { ", and memory too (a combo card)" } else { "" }
    ));

    // The identification clock has done its job, so leave it. A failure here is reported and NOT
    // fatal: everything below rides CMD52, which works at 400 kHz perfectly well, just slowly. Saying
    // so rather than returning is the difference between a degraded stage and a lost one.
    if !h.set_operating_clock(OPERATING_HZ, &ctx) {
        ctx.log(
            "wifi-driver: the operating clock would not stabilise - continuing at the identification \
             clock, which is slow but correct",
        );
    }

    // ---- Stage 4: the card's own common registers. ------------------------------------------------
    sdio::report_cccr(&h, &ctx);

    // ---- Stage 5: the CIS, which is the actual proof. ---------------------------------------------
    // A controller answering says a HOST CONTROLLER is there. A card answering CMD5 says an I/O card
    // is there. Only the CIS says WHICH part, and that is the claim `docs/wifi.md` section 4 rests on.
    match sdio::cis_pointer(&h, &ctx) {
        Some(ptr) => {
            ctx.log_fmt(format_args!("wifi-driver: stage 5 - walking the CIS from {:#07x}", ptr));
            match sdio::walk_cis(&h, ptr, &ctx) {
                // REPORTED, NOT JUDGED. The manufacturer is a meaningful check; the device code is the
                // SDIO id, which is NOT the field that identifies the part for anything this driver does
                // - see the note in `sdio::Manfid`. The verdict is stage 7's, from the chip id.
                Some(id) if id.is_broadcom() => ctx.log_fmt(format_args!(
                    "wifi-driver: a BROADCOM part is on the bus - manufacturer {:#06x}, SDIO device \
                     code {:#06x}. Which part it is comes from the chip id below, not from this code",
                    id.manf, id.device
                )),
                Some(id) => ctx.log_fmt(format_args!(
                    "wifi-driver: the part on this bus is NOT Broadcom - manufacturer {:#06x} (expected \
                     {:#06x}), SDIO device code {:#06x}. That is a finding, not a failure",
                    id.manf,
                    sdio::Manfid::BROADCOM,
                    id.device
                )),
                None => ctx.log(
                    "wifi-driver: the CIS walk found no MANFID tuple, so the part on the bus is \
                     unidentified. It answered CMD5 and CMD52, so this is the tuple chain rather than \
                     the device",
                ),
            }
        }
        None => ctx.log("wifi-driver: no CIS pointer, so the part on the bus cannot be identified"),
    }

    // ---- Stage 6: enable the backplane function, which is the first WRITE. ------------------------
    // Everything above is a read. A bus that answers reads and drops writes looks perfectly healthy
    // until here, so this is worth doing even though nothing yet uses the function: it is the path a
    // firmware image is later written through, and the readback of IO_READY is the proof it is open.
    //
    // Function 1 specifically, because that is the backplane on this part - what `brcmfmac` enables
    // first, before any firmware exists inside the chip to answer. Not fatal: identification already
    // succeeded, and saying which half failed is worth more than stopping.
    // THE BLOCK SIZES FIRST, which is the order `brcmf_sdiod_probe` uses: function 1 to 64 and function
    // 2 to 512, both BEFORE function 1 is enabled. This driver set neither, ever - and it is the one step
    // the reference performs in the stretch the fault has been narrowed to (the window is verified, the
    // card accepts the command, and then sends nothing).
    //
    // Function 2 is set even though nothing uses it yet, because that is what the reference does here and
    // the firmware upload will need it. Neither is fatal: they are reported and the sequence continues, so
    // a refusal here does not hide whatever the read does next.
    ctx.log("wifi-driver: stage 6 - block sizes, then opening function 1, the backplane");
    if card.funcs >= 1 {
        sdio::set_block_size(&h, 1, 64, &ctx);
    }
    if card.funcs >= 2 {
        sdio::set_block_size(&h, 2, 512, &ctx);
    }
    let backplane_open = card.funcs >= 1 && sdio::enable_function(&h, 1, &ctx);
    if !backplane_open {
        ctx.log(
            "wifi-driver: function 1 (the backplane) is not open, so no firmware could be written \
             through it. Identification succeeded, so the card is there and reachable for reads",
        );
        serve(&ctx);
    }

    // ---- Stage 7: ask the SILICON what it is. -----------------------------------------------------
    // The CIS device code and this board's documented part disagree, and that disagreement decides
    // which firmware blob phase 2 must upload. The CIS cannot settle it - it IS the disputed reading -
    // so this asks the chip's own identity register, reached through the backplane that stage 6 opened.
    //
    // Not a detour: the same register carries the chip TYPE, which is what says how a later phase walks
    // the core list to find where the chip's RAM is. The firmware upload needs this read anyway.
    ctx.log("wifi-driver: stage 7 - waking the backplane to read the chip's own identity");
    if !backplane::wake(&h, &ctx) {
        ctx.log(
            "wifi-driver: the backplane is not answering, so the chip cannot be asked what it is. \
             Everything through stage 6 stands: the card is on the bus, identified, and function 1 \
             reported ready",
        );
        serve(&ctx);
    }
    let mut window = backplane::Window::new();
    // The proper read first - one CMD53, one 32-bit fetch by the bridge. If it fails, fall back to four
    // CMD52 byte reads, which is NOT how this should be done and is the command known to work on this
    // bus: whichever answers tells us something we do not have yet. See `chip_id_via_cmd52`.
    let found = backplane::chip_id(&h, &mut window, &ctx)
        .or_else(|| backplane::chip_id_via_cmd52(&h, &mut window, &ctx));
    match found {
        Some(id) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: CHIP SAYS id {:#06x} ({}) rev {} package {} type {} [raw {:#010x}]",
                id.id,
                id.describe(),
                id.rev,
                id.package,
                id.chip_type,
                id.raw
            ));
            // THE COMPARISON IS THE POINT, so it is made here rather than left to a reader with two
            // numbers in different bases. The CIS device code and the silicon's chip id are DIFFERENT
            // fields - Broadcom does not oblige them to match, and 0xA9BF/0x4345 for the 43455 is the
            // worked example - so agreement and disagreement both mean something specific.
            // WHICH FIRMWARE, selected from the chip id and revision exactly as brcmfmac's table does
            // (the revision field there is a BITMASK - see `ChipId::firmware`). This is the answer the
            // whole of phase 1 existed to get, because it is what phase 2 uploads.
            match id.firmware() {
                Some(fw) => ctx.log_fmt(format_args!(
                    "wifi-driver: this part wants firmware `{}` - so `nonfree/{}/` is the blob to \
                     upload, chosen from the chip id and revision rather than from the board's \
                     documentation",
                    fw,
                    if fw.contains("43455") { "brcm43455" } else { "<not vendored>" }
                )),
                None => ctx.log(
                    "wifi-driver: no firmware is mapped for this chip id and revision, so phase 2 has \
                     nothing to upload. A finding rather than a failure - report the id and revision",
                ),
            }

            // ---- Stage 8: enumerate the chip's internal cores. --------------------------------------
            // The firmware goes into the chip's RAM and nothing yet knows where that is. The chip
            // publishes a table - the EROM - naming every core on its internal bus with an ID, a
            // revision and a register base, and finding the ARM core in it is what makes an address to
            // write to. So this comes before the upload rather than beside it, and it is verifiable on
            // its own: it prints a table that is either a plausible CYW43455 or it is not.
            ctx.log("wifi-driver: stage 8 - walking the EROM to find the ARM core and the RAM");
            let cores = erom::scan(&h, &mut window, &ctx);
            match &cores {
                Some(cores) => {
                    // CHECK THE WRAPPER RULE BEFORE TRUSTING IT. The scan collected what the EROM
                    // published; this asks whether `base + WRAPPER_OFFSET` reproduces those, which is the
                    // only evidence from THIS die that the derivation is right.
                    cores.check_wrappers(&ctx);
                    cores.report(&ctx);
                }
                None => ctx.log(
                    "wifi-driver: the core table could not be walked, so phase 2 has no address to                      write firmware to. Everything through stage 7 stands - the chip is identified and                      its backplane reads",
                ),
            }

            // ---- Stage 9: how much RAM, and where the firmware goes. ---------------------------------
            // The upload needs an address and a size. The CR4 reports its TCM as a set of BANKS through
            // its own registers - reached by the core's BASE, not its wrapper, which is why a wrapper of
            // 0 does not block this - and the firmware's start address is a per-part constant the
            // reference keeps in a table rather than a formula.
            ctx.log("wifi-driver: stage 9 - asking the ARM core how much TCM it has");
            match cores.as_ref().and_then(|c| c.arm) {
                Some(arm) => match armcr4::probe(&h, &mut window, arm.base, id.id, &ctx) {
                    Some(ram) => {
                        ram.report(&ctx);
                        // ---- Stage 10: what this build actually carries. ----------------------------
                        // Checked against the size the CHIP just reported rather than against a number
                        // from a document, and stated before any transfer starts: finding out mid-upload
                        // that 600 KB does not fit is the wrong time.
                        ctx.log("wifi-driver: stage 10 - the firmware this build carries");
                        firmware::report(ram.size, ram.base, &ctx);

                        // ---- Stage 11: halt, write, release. --------------------------------------
                        // GUARDED, not attempted. The upload needs the ARM's WRAPPER as well as the RAM
                        // it just sized, and `wrapper()` is `None` only for a core with no register base
                        // at all. Without it there is no address to halt the core through, and writing
                        // into the memory of a RUNNING core is worse than not trying - which is also why
                        // `upload::run` halts first and refuses to continue unless the halt confirms.
                        match arm.wrapper() {
                            Some(wrap) => {
                                ctx.log("wifi-driver: stage 11 - uploading the firmware");
                                if upload::run(&h, &mut window, wrap, &ram, &ctx) {
                                    ctx.log(
                                        "wifi-driver: PHASE 2 COMPLETE - firmware and NVRAM are in the \
                                         chip and its processor is running them",
                                    );

                                    // ---- Stage 12: bring the bus up for frames. -----------------------
                                    // ONLY REACHED WITH A CONFIRMED-RUNNING FIRMWARE, because `upload::run`
                                    // ends by asking the chip rather than by asserting. Enabling a data
                                    // function against a dead firmware would produce a bus that looks
                                    // ready and answers nothing, which is the failure mode this driver
                                    // keeps refusing to build.
                                    match cores.as_ref().and_then(|c| c.sdiod.as_ref()) {
                                        Some(sdiod) => {
                                            let _ = bus::bring_up(
                                                &h, &mut window, sdiod.base, &ctx,
                                            );
                                        }
                                        None => ctx.log(
                                            "wifi-driver: the EROM described no SDIO device core, so \
                                             there is no mailbox to announce the protocol version to - \
                                             the bus cannot be brought up for frames",
                                        ),
                                    }
                                } else {
                                    ctx.log(
                                        "wifi-driver: the upload did not complete. Everything through \
                                         stage 10 stands, and the last line above says which step \
                                         stopped it",
                                    );
                                }
                            }
                            None => ctx.log(
                                "wifi-driver: the ARM core has no register base, so no wrapper can be \
                                 derived and it cannot be halted - the upload is not attempted",
                            ),
                        }
                    }
                    None => ctx.log("wifi-driver: the ARM core memory could not be sized, so the upload has no destination yet"),
                },
                None => ctx.log("wifi-driver: no ARM core was found, so there is nothing to ask about TCM"),
            }
        }
        None => ctx.log(
            "wifi-driver: neither CMD53 nor the CMD52 fallback could read the chip's identity, so the \
             firmware question is still open. The backplane woke and its clock is granted, so this is \
             the register access rather than the chip",
        ),
    }

    // ---- The honest end of phase 1 step 1. --------------------------------------------------------
    ctx.log(
        // SAY WHAT IS TRUE AT THE POINT THIS PRINTS. This line used to assert "NO firmware is uploaded
        // and no 802.11 exists yet" - and it printed immediately AFTER "PHASE 2 COMPLETE", so the log
        // contradicted itself by one line. Third time in this effort that the code moved on and the
        // sentence did not, which is why the sentence no longer claims a phase it cannot see.
        "wifi-driver: the radio is on the bus, identified by BOTH its CIS and its own silicon, its \
         backplane is open, and the stages above say how far the firmware got. There is no control \
         channel to it yet, so every request is still answered `unavailable` - a loaded chip is not a \
         usable radio, and this line does not pretend otherwise",
    );
    serve(&ctx)
}
