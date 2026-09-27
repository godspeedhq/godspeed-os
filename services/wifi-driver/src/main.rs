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

mod host;
mod sdio;

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
                Some(id) if id.is_expected_radio() => ctx.log_fmt(format_args!(
                    "wifi-driver: the radio is CONFIRMED ON THE BUS - manufacturer {:#06x} (Broadcom), \
                     device {:#06x} (CYW43455). Asked of the part, not read from a device tree",
                    id.manf, id.device
                )),
                Some(id) => ctx.log_fmt(format_args!(
                    "wifi-driver: an SDIO part answered but it is NOT the expected radio - \
                     manufacturer {:#06x}, device {:#06x} (expected {:#06x}/{:#06x}). That is a \
                     finding, not a failure: something is on this bus and it is not what this board \
                     is documented to carry",
                    id.manf,
                    id.device,
                    sdio::Manfid::BROADCOM,
                    sdio::Manfid::CYW43455
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
    if card.funcs >= 1 {
        if !sdio::enable_function(&h, 1, &ctx) {
            ctx.log(
                "wifi-driver: function 1 (the backplane) is not open, so no firmware could be written \
                 through it. Identification succeeded, so the card is there and reachable for reads",
            );
        }
    }

    // ---- The honest end of phase 1 step 1. --------------------------------------------------------
    ctx.log(
        "wifi-driver: phase 1 step 1 complete. NO firmware is uploaded and no 802.11 exists yet - the \
         chip runs no MAC until a host uploads one into it - so every request is answered `unavailable`",
    );
    serve(&ctx)
}
