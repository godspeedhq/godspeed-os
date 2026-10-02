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
mod ctrl;
mod frames;
mod scan;
mod firmware;
mod erom;
mod host;
mod join;
mod sdio;
mod upload;

use godspeed_sdk::{Message, ServiceContext};
// The chip-independent half, shared with every radio driver (`sdk/wifi`). Imported at the root so the
// modules here keep writing `crate::crypto` and friends.
use godspeed_wifi::{crypto, eapol, keyfile};
use godspeed_wifi::sdio::SdioHost;

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
/// Serve with NO radio: every request is answered "radio down", loudly and at once, so a shell that asks
/// gets a fact rather than a timeout. This is where the driver goes when any stage before the radio came
/// up has failed - identification, upload, bus - and it is the rule above the rules (Commandment VIII): a
/// dependency that cannot do the thing must RETURN with a loud unavailable, never hang.
/// How long WL_REG_ON is held low. Fifty milliseconds was tried first and was MARGINAL: of two power cycles on
/// hardware (2026-10-01) one gave a cold chip and one gave a chip whose SDIO side had reset but whose
/// firmware then trapped at start with the section-45 signature - reset, not power-cycled. The pin is
/// driven by the VideoCore over I2C at its own pace, and the chip's internal supplies take time to
/// drain; half a second removes the margin and costs an operator's `powercycle` nothing it notices.
/// Two seconds (2026-10-01). 50 ms, 500 ms, 2 s and 75 s all produced warm starts, so the hold-off is
/// not the variable; this only has to exceed the chip's own discharge.
const POWER_OFF_MS: u64 = 2_000;
/// How long after WL_REG_ON goes high before the SDIO side is asked anything.
///
/// 300 ms. Five seconds was tried (2026-10-01) to give the chip the time on that boot gives it, and changed
/// nothing - docs/wifi.md 52 - so the reasoning below is the experiment's, kept as its record.
/// At boot the VideoCore raises WL_ON seconds before this driver's first command, and boot always comes
/// up cold; after a cycle the driver started 300 ms after, and came up warm almost every time. The OFF time
/// was varied from 50 ms to 75 s and never mattered; the ON time never was varied. Linux's four pre-download
/// steps changed nothing, and the chip's registers read identically cold and warm (docs/wifi.md 52), which
/// leaves the chip's own power-on initialisation, still running when the driver halts it, as the suspect.
const POWER_ON_SETTLE_MS: u64 = 300;

/// Cut the radio's power and restore it, through the kernel's `DevicePower` (docs/wifi.md 47). The
/// two waits are the DEVICE'S - WL_REG_ON low long enough for the CYW43455 to lose its state, then the
/// chip's own power-on before its SDIO side answers - and live here and not in the kernel because they
/// are facts about this chip, not about power (26.10). `false` means the kernel has no control over
/// this device's power on this machine - the caller then does what it can without.
pub(crate) fn power_cycle_device(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    power_cycle_device_ms(ctx, h, POWER_OFF_MS)
}

/// The three settings Linux's brcmfmac writes after making the cores passive and before downloading the
/// firmware (`brcmf_sdio_probe_attach`, read 2026-10-01). Each is read back and said; none is fatal.
fn linux_pre_download(h: &dyn SdioHost, w: &mut backplane::Window, cores: Option<&erom::Cores>, ctx: &ServiceContext) {
    // KSO, keep-SDIO-on (`brcmf_sdio_kso_init`): SDIO device core rev >= 12 only; F1 SLEEPCSR bit 0.
    const SLEEPCSR: u32 = 0x1_001F;
    const KSO_EN: u8 = 0x01;
    let sdio_rev = cores.and_then(|c| c.sdiod).map(|c| c.rev).unwrap_or(0);
    if sdio_rev >= 12 {
        match sdio::read_reg(h, 1, SLEEPCSR) {
            Some(v) if v & KSO_EN != 0 => ctx.log_fmt(format_args!("wifi-driver: KSO already set (SLEEPCSR {:#04x})", v)),
            Some(v) => {
                let ok = sdio::write_reg(h, 1, SLEEPCSR, v | KSO_EN).is_some();
                let after = sdio::read_reg(h, 1, SLEEPCSR);
                ctx.log_fmt(format_args!("wifi-driver: KSO set (SLEEPCSR {:#04x} -> {:?}, write {})", v, after, if ok { "ok" } else { "refused" }));
            }
            None => ctx.log("wifi-driver: KSO - SLEEPCSR did not answer"),
        }
    } else {
        ctx.log_fmt(format_args!("wifi-driver: KSO skipped - SDIO core rev {} is below 12", sdio_rev));
    }
    // CCCR_BRCM_CARDCTRL (F0 0xF1) |= WLANRESET: "so an SDIO card reset does a WLAN backplane reset".
    const CARDCTRL: u32 = 0xF1;
    const WLANRESET: u8 = 0x02;
    match sdio::read_reg(h, 0, CARDCTRL) {
        Some(v) => {
            let ok = sdio::write_reg(h, 0, CARDCTRL, v | WLANRESET).is_some();
            ctx.log_fmt(format_args!("wifi-driver: CARDCTRL {:#04x} -> {:?} (WLANRESET, write {})",
                v, sdio::read_reg(h, 0, CARDCTRL), if ok { "ok" } else { "refused" }));
        }
        None => ctx.log("wifi-driver: CARDCTRL did not answer"),
    }
    // PMU pmucontrol |= RES_RELOAD << RES_SHIFT: "so a backplane reset does PMU state reload". No separate
    // PMU core in this chip's table, so pmucontrol is chipcommon's, at 0x18000600.
    const PMUCONTROL: u32 = 0x1800_0600;
    const RES_RELOAD: u32 = 0x2 << 13;
    match w.read32(h, PMUCONTROL, ctx) {
        Some(v) => {
            let ok = w.write32(h, PMUCONTROL, v | RES_RELOAD, ctx).is_some();
            ctx.log_fmt(format_args!("wifi-driver: PMU control {:#010x} -> {:?} (RES_RELOAD, write {})",
                v, w.read32(h, PMUCONTROL, ctx), if ok { "ok" } else { "refused" }));
        }
        None => ctx.log("wifi-driver: PMU control did not answer"),
    }
}

/// How long the Arm clock lease asks for. The bring-up through the upload and the first scan takes about
/// 6 s with the cores fast; the margin is for a slow card, and `power` caps any lease at 30 s anyway.
const CLOCK_LEASE_SECS: u8 = 20;
/// How long to wait for `power` to answer. It answers from memory, so this is a dead-or-alive bound.
const POWER_SECS: i64 = 2;

/// Ask `power` to hold the Arm clock at its maximum while this instance loads the chip (`docs/power.md`).
///
/// WHY: the CYW43455's firmware traps at start when it is uploaded with the cores at their minimum clock -
/// every load after the Pi firmware's first minute did, every load inside it did not (`docs/wifi.md` 55).
/// The mechanism is not known; the dependence is measured, so it is asked for rather than assumed.
///
/// OPTIONAL by design: a `power` that is absent, dead or refusing costs one line and the load goes ahead at
/// whatever the clock is - the same as before this existed. `gs::call::request_within` does the one retry
/// that is safe - reacquire by name and resend when the send itself failed (a `power` respawned since this
/// driver was wired, Commandment IX) - and never resends after a deadline.
///
/// A deadline is `OutcomeUnknown`, which here means a lease MAY be open with no id to release it by. That
/// is said rather than hidden, and it is bounded: the lease expires on its own within 30 s.
fn clock_lease(ctx: &ServiceContext) -> Option<u8> {
    let msg = Message::from_bytes(&[1, CLOCK_LEASE_SECS]);
    let r = match godspeed::call::request_within(ctx, "power", &msg, POWER_SECS) {
        Ok(r) => r,
        Err(godspeed::Error::OutcomeUnknown) => {
            ctx.log("wifi-driver: `power` did not answer the clock lease in time - a lease may be open with no id to release it by, and expires on its own; loading at whatever the Arm clock is");
            return None;
        }
        Err(e) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: no clock lease ({}) - loading at whatever the Arm clock is (docs/power.md)", e.as_str()));
            return None;
        }
    };
    let p = r.payload_bytes();
    match p.first().copied() {
        Some(0) if p.len() >= 6 => {
            let hz = u32::from_le_bytes([p[2], p[3], p[4], p[5]]);
            if hz > 0 {
                ctx.log_fmt(format_args!("wifi-driver: Arm clock held at {} MHz for the load (lease {})", hz / 1_000_000, p[1]));
            } else {
                ctx.log_fmt(format_args!("wifi-driver: Arm clock lease {} joined one already open", p[1]));
            }
            Some(p[1])
        }
        Some(2) => {
            ctx.log("wifi-driver: this machine gives the OS no control over its clock - loading at whatever it is");
            None
        }
        other => {
            ctx.log_fmt(format_args!("wifi-driver: `power` refused the clock lease ({:?}) - loading at whatever the Arm clock is", other));
            None
        }
    }
}

/// Hand the lease back. A failure here is said and otherwise harmless: the lease expires on its own.
fn clock_release(ctx: &ServiceContext, lease: Option<u8>) {
    let Some(id) = lease else { return };
    let msg = Message::from_bytes(&[2, id]);
    if let Err(e) = godspeed::call::request_within(ctx, "power", &msg, POWER_SECS) {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not hand clock lease {} back to `power` ({}) - it expires on its own", id, e.as_str()));
    }
}

/// After a cut: is the chip really unpowered? It waits for the rail to fall, brings the host back long
/// enough for one CMD52, and parks it again. `true` means the chip still answers - the cut did NOT take.
fn chip_still_answers(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    const RAIL_FALL_MS: u64 = 50;
    ctx.sleep_ms(RAIL_FALL_MS);
    let answers = h.reset(ctx) && sdio::initialised_card_answers(h);
    h.park(ctx);
    answers
}

/// The verdict on a hard off, said in the log and returned for reply byte 3.
fn verify_hard_off(ctx: &ServiceContext, h: &dyn SdioHost) -> u8 {
    if chip_still_answers(ctx, h) {
        ctx.log("wifi-driver: `wifi radio off hard` - the pin was driven low but the chip STILL ANSWERS on its bus: its power did not go off");
        scan::reply::OFF_CONTRADICTED
    } else {
        ctx.log("wifi-driver: `wifi radio off hard` - verified: the chip no longer answers on its bus");
        scan::reply::OFF_VERIFIED
    }
}

/// Restore the chip's power the way a board power-on would: with the host PARKED (`host::Host::park`),
/// nothing driving the SDIO lines at the rising edge of WL_REG_ON, where the chip samples its boot straps.
/// The park is repeated here although the cut already parked it, so the edge is quiet whatever touched the
/// host while the power was off. The host is left parked: its next user brings it back from reset after
/// this delay, which is Linux's order (power first, the init clock after). `false` means the kernel refused.
/// (docs/wifi.md 48: the park did not by itself make the start cold.)
pub(crate) fn power_on_device(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    h.park(ctx);
    if !ctx.device_power(true) {
        return false;
    }
    ctx.sleep_ms(POWER_ON_SETTLE_MS);
    true
}

/// `power_cycle_device` with the hold-off chosen by the caller (`wifi radio powercycle` asks for a fixed
/// 2 s): the shell decides how long, this decides how.
pub(crate) fn power_cycle_device_ms(ctx: &ServiceContext, h: &dyn SdioHost, off_ms: u64) -> bool {
    if !ctx.device_power(false) {
        return false;
    }
    // THE HOST GOES QUIET FOR THE WHOLE OFF WINDOW (docs/wifi.md 48). Every earlier cycle left the card
    // clock toggling into the unpowered chip from here to the power-on. Parked after the cut is accepted,
    // not before: a refused cut then leaves the host and any live session untouched, and the gap between
    // the pin going low and the park is the mailbox's return - well under a millisecond of a long quiet.
    // Tried as the warm-start fix and it was not one (one cold start in three loads, docs/wifi.md 48); kept,
    // because a quiet host across the edge is still the reference's order.
    h.park(ctx);
    ctx.sleep_ms(off_ms);
    if !power_on_device(ctx, h) {
        ctx.log("wifi-driver: the radio's power was cut and could NOT be restored - the kernel refused the second request");
        return false;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the radio's power was held off for {} ms and given {} ms to come up",
        off_ms, POWER_ON_SETTLE_MS));
    true
}

/// The hold-off a `[OP_RADIO, RADIO_POWERCYCLE, units]` request asks for: `units` of 100 ms, 0 meaning the
/// driver's default. Bounded above so a stray byte cannot hold the radio off for half a minute.
fn requested_off_ms(payload: &[u8]) -> u64 {
    match payload.get(2).copied().unwrap_or(0) {
        0 => POWER_OFF_MS,
        u => (u as u64 * 100).min(20_000),
    }
}

fn serve_unavailable(ctx: &ServiceContext, h: Option<&dyn SdioHost>) -> ! {
    // `wifi radio off hard` from here leaves the chip powered down, and from then on this loop answers the
    // way `serve_radio`'s powered-off arms do, so the shell sees one shape for one state whichever loop
    // holds it. `h` is `None` only where no SDIO window was granted, and there no power op can be made.
    let mut powered_off = false;
    let mut out = [0u8; 30 + join::MAX_SSID];
    loop {
        let req = ctx.recv();
        // No reply cap means there is nothing to answer on, and dropping is all that is left.
        let Some(reply) = ctx.take_pending_cap() else { continue };
        // The reply cap is RECLAIMED after use (26.6): see the serve loop for what not doing so cost.
        // One byte at least, not an empty message: the kernel refuses a zero-length send, so an "empty
        // reply" is no reply at all and the caller waits out its deadline.
        let (tag, p) = untag(req.payload_bytes());
        // THIS LOOP IS REACHED when the radio never got as far as its firmware - no SDIO window, the host
        // failed, no card on the bus even after the power was asserted, a backplane that would not open. A
        // firmware that trapped at start does NOT come here: it goes to `serve_radio` with DOWN_TRAPPED.
        //
        // THE POWER OPS ARE SERVED FOR REAL, because they are the way out of this state and the chip's
        // power is still this driver's to command. `off hard` was answered RADIO_DOWN here until
        // 2026-10-02, and the shell read that as a driver that could not act and advised `kill
        // wifi-driver` - wrong advice for a cut this loop could have made (docs/wifi.md 49). Reply shapes
        // match `serve_radio`'s; everything else stays "radio down".
        let op = p.first().copied().unwrap_or(0);
        let mode = p.get(1).copied().unwrap_or(1);
        out[..5].fill(0);
        let n = if op == scan::reply::OP_STATUS && powered_off {
            // The powered-down status `serve_radio` gives: all zero, the trailing power byte included.
            out.fill(0);
            out[0] = scan::reply::OK;
            out.len()
        } else if op == scan::reply::OP_RADIO && mode == scan::reply::RADIO_POWERCYCLE {
            // The hold-off is the caller's (the shell asks for a fixed 2 s).
            let cycled = match h {
                Some(h) => power_cycle_device_ms(ctx, h, requested_off_ms(p)),
                None => false,
            };
            ctx.log(if cycled {
                "wifi-driver: `wifi radio powercycle` on a radio that is down - the chip's power was cut and restored; this instance expects to be restarted onto the cold chip"
            } else {
                "wifi-driver: `wifi radio powercycle` on a radio that is down, and this machine has no control over the radio's power - nothing changed"
            });
            if cycled {
                powered_off = false;
            }
            out[0] = if cycled { scan::reply::OK } else { scan::reply::NO_POWER_CONTROL };
            out[2] = cycled as u8;
            5
        } else if op == scan::reply::OP_RADIO && powered_off {
            if mode == 0 || mode == scan::reply::RADIO_HARD_OFF {
                // Already as asked.
                out[0] = scan::reply::OK;
            } else if h.is_some_and(|h| power_on_device(ctx, h)) {
                powered_off = false;
                ctx.log("wifi-driver: `wifi radio on` on a powered-down chip - the power is restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                out[0] = scan::reply::OK;
                out[2] = 1;
                out[3] = scan::reply::COLD_START;
            } else {
                ctx.log("wifi-driver: `wifi radio on` on a powered-down chip, and the kernel refused to restore the power");
                out[0] = scan::reply::NO_POWER_CONTROL;
            }
            5
        } else if op == scan::reply::OP_RADIO && mode == scan::reply::RADIO_HARD_OFF {
            match h {
                Some(h) if ctx.device_power(false) => {
                    h.park(ctx);
                    powered_off = true;
                    ctx.log("wifi-driver: `wifi radio off hard` on a radio that is down - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = scan::reply::OK;
                    out[2] = 1;
                    out[3] = verify_hard_off(ctx, h);
                }
                _ => {
                    ctx.log("wifi-driver: `wifi radio off hard` asked, and this machine has no control over the radio's power - the radio stays as it was");
                    out[0] = scan::reply::NO_POWER_CONTROL;
                }
            }
            5
        } else if powered_off {
            out[0] = scan::reply::RADIO_POWERED_OFF;
            1
        } else {
            out[0] = scan::reply::RADIO_DOWN;
            out[1] = scan::reply::DOWN_NO_RADIO;
            2
        };
        let _ = ctx.try_send_by_handle(reply, &tagged_reply(tag, &out[..n]));
        ctx.remove_cap(reply);
    }
}

/// A request's correlation tag, if it carries one (`scan::reply::TAGGED`), and the request without it.
fn untag(raw: &[u8]) -> (Option<u8>, &[u8]) {
    match raw {
        [scan::reply::TAGGED, tag, rest @ ..] => (Some(*tag), rest),
        _ => (None, raw),
    }
}

/// The reply to send: `body` as it is for an untagged request, `[TAGGED, tag, body...]` for a tagged one.
fn tagged_reply(tag: Option<u8>, body: &[u8]) -> Message {
    match tag {
        None => Message::from_bytes(body),
        Some(t) => {
            let mut m = Message::from_bytes(&[scan::reply::TAGGED, t]);
            let k = body.len().min(m.payload.len() - 2);
            m.payload[2..2 + k].copy_from_slice(&body[..k]);
            m.payload_len = 2 + k;
            m
        }
    }
}

/// Serve the shell and `nic-driver`: sweeps as a state the loop advances, the cache, joins, keys, status,
/// debug - and the frame interface (`frames.rs`), over which the radio carries the stack's traffic when
/// the cable is out. Without a radio, every request is answered "radio down" - the same loud fact
/// `serve_unavailable` gives, so a shell can never tell the two apart by waiting.
///
/// The loop never sits inside a sweep: `scan::step` advances it one frame at a time and `try_recv`
/// answers requests between frames, which is what lets `q` stop a sweep (`utilities/56_wifi.md` rule 11)
/// and `wifi list` answer during one. It DOES sit inside a join (`join::join`, a few seconds), and
/// `nic-driver`'s bound on it is short for exactly that reason.
fn serve_radio(
    ctx: &ServiceContext,
    h: &dyn SdioHost,
    w: &mut backplane::Window,
    mut radio: Option<ctrl::Session>,
    down_reason: u8,
) -> ! {
    // Bounded: 2 status bytes plus 32 records of 44 is 1410 for a list, and 2 plus 64 names of 33 is 2114 for
    // `stored` - both inside this fixed buffer, inside a 4 KiB message.
    let mut out = [0u8; 2560];

    // THE SWEEP IS A STATE, NOT A CALL. `wifi scan` starts one and returns; the loop below advances it one
    // frame per turn and answers requests between frames, so `wifi list`, `wifi status` and an abort are
    // heard while the radio sweeps (rule 11), and a sweep left running by `b` finishes on its own. A
    // finished sweep becomes THE CACHE - the one list `wifi list` ever prints - and only a finished one
    // does: an abort or a sweep that fell silent past the poll bound is discarded, never half-served.
    struct Sweep {
        scan: scan::Scan,
        empty: u32,
    }
    struct Cache {
        scan: scan::Scan,
        at_secs: i64,
    }
    let mut sweep: Option<Sweep> = None;
    let mut cache: Option<Cache> = None;
    // The last sweep died by the poll bound rather than the firmware's word. Pollers are told, not handed
    // whatever older cache exists as if it were the sweep they asked for.
    let mut sweep_failed = false;
    // `bring_up` ran the UP chain, so the radio starts on; `wifi radio off` is the only thing that turns
    // it off, and `wifi radio on` re-runs the same chain.
    let mut radio_on = true;
    // `wifi radio off hard` cut the chip's power and this instance is still here to say so. While it is
    // set, status and the radio op are served and everything else is answered RADIO_POWERED_OFF; `on`
    // restores the power and asks to be restarted, because a cold chip needs the boot's own path.
    let mut powered_off = false;
    // The network the firmware last reported JOINED, cleared by a disconnect or a power-off. Only an open
    // network can reach that state until the host supplicant exists - see `join.rs`.
    let mut joined: Option<([u8; join::MAX_SSID], u8)> = None;
    // When the join was reported, and with what security - for `status`'s `joined N s ago` and `security`.
    let mut joined_at_secs: i64 = 0;
    let mut joined_security: u8 = scan::sec::OPEN;
    // THE FRAME PATH (`frames.rs`): what `nic-driver` asks once the cable is out. The queue is bounded
    // and on this stack; the address is asked of the chip once; the rekey count is for the log.
    let mut rxq = frames::RxQueue::new();
    let mut link_mac: Option<[u8; 6]> = None;
    // Pairwise rekeys that did not complete, for the log.
    let mut rekey_seen: u32 = 0;
    // The network last JOINED this boot - name, length, security - so `radio on` after `radio off` can
    // go back to it without being asked (`utilities/56_wifi.md` 2). Set on every successful join, cleared
    // by `wifi leave` (an explicit leave means "not this one") and by `forget` of that name.
    let mut last_joined: Option<([u8; join::MAX_SSID], u8, u8)> = None;
    /// The reply status for a join outcome - one table, used by `wifi join` and by the rejoin.
    fn reply_of(outcome: join::Outcome) -> u8 {
        match outcome {
            join::Outcome::Joined => scan::reply::JOINED,
            join::Outcome::NotFound => scan::reply::NOT_FOUND,
            join::Outcome::PassphraseRefused => scan::reply::PASSPHRASE_REFUSED,
            join::Outcome::Failed => scan::reply::JOIN_FAILED,
            join::Outcome::Timeout => scan::reply::JOIN_TIMEOUT,
            join::Outcome::HandshakeUnimplemented => scan::reply::HANDSHAKE_UNIMPLEMENTED,
        }
    }
    // THE KEY FILE (`keyfile.rs`, `/wifi.keys`). Loaded once the radio is up - retried a bounded number
    // of times while `fs` is still coming up, then given up with a line - and the most recent entry is
    // joined without being asked. Written after every change to the table.
    let mut keyfile_settled = false;
    let mut keyfile_tries: u32 = 0;
    const KEYFILE_TRIES: u32 = 15;
    let mut auto_join: Option<([u8; join::MAX_SSID], u8, u8)> = None;
    /// Join a network this driver holds a key for - the rejoin after `radio on`, and the auto-join at boot
    /// from `/wifi.keys`. `None` when there is no key for a WPA2 name (nothing was attempted); otherwise
    /// the join's outcome, with the driver's memory of the link updated either way.
    fn join_known(
        h: &dyn SdioHost,
        w: &mut backplane::Window,
        session: &mut ctrl::Session,
        name: &[u8; join::MAX_SSID],
        len: u8,
        sec: u8,
        stored: &mut [Option<Stored>],
        keys: &mut Option<join::Keys>,
        joined: &mut Option<([u8; join::MAX_SSID], u8)>,
        joined_at_secs: &mut i64,
        joined_security: &mut u8,
        rxq: &mut frames::RxQueue,
        ctx: &ServiceContext,
    ) -> Option<join::Outcome> {
        let ssid = &name[..len as usize];
        let mut pmk_buf = [0u8; crypto::PMK_LEN];
        let secret = if sec == scan::sec::OPEN {
            Some(join::Secret::Open)
        } else {
            match slot_of(stored, ssid).and_then(|i| stored[i].as_ref()) {
                Some(st) => {
                    pmk_buf = st.pmk;
                    Some(join::Secret::Pmk(&pmk_buf))
                }
                None => None,
            }
        };
        let secret = secret?;
        let outcome = join::join(h, w, session, ssid, secret, keys, ctx);
        if outcome == join::Outcome::Joined {
            *joined = Some((*name, len));
            *joined_at_secs = ctx.epoch_secs_monotonic();
            *joined_security = sec;
            rxq.clear();
            if let Some(st) = slot_of(stored, ssid).and_then(|i| stored[i].as_mut()) {
                st.used_at = ctx.epoch_secs_monotonic();
            }
        } else {
            *joined = None;
            let _ = ctrl::disassoc(h, w, session, ctx);
        }
        pmk_buf.fill(0);
        Some(outcome)
    }
    /// Write the table to `/wifi.keys`, most recently used first. Called after any change to it.
    fn save_keys(ctx: &ServiceContext, stored: &[Option<Stored>]) {
        let mut entries = [keyfile::Entry::EMPTY; keyfile::MAX_SAVED];
        let mut n = 0usize;
        // Selection by recency into a bounded array: the table is 64 slots and this is 48 picks of it.
        let mut taken = [false; CREDENTIAL_SLOTS];
        while n < keyfile::MAX_SAVED {
            let mut best: Option<usize> = None;
            for (i, s) in stored.iter().enumerate() {
                if taken[i] {
                    continue;
                }
                if let Some(st) = s {
                    if best.map_or(true, |b| stored[b].as_ref().map_or(true, |bs| st.used_at > bs.used_at)) {
                        best = Some(i);
                    }
                }
            }
            let Some(i) = best else { break };
            taken[i] = true;
            if let Some(st) = stored[i].as_ref() {
                entries[n] = keyfile::Entry { ssid: st.ssid, len: st.len, sec: scan::sec::WPA2, pmk: st.pmk };
                n += 1;
            }
        }
        if keyfile::save(ctx, &entries[..n]) {
            ctx.log_fmt(format_args!("wifi-driver: /wifi.keys written - {} network(s)", n));
        }
        for e in entries.iter_mut() {
            e.pmk.fill(0);
        }
    }
    // The keys a WPA2 join keeps for the rekeys to come (`join::Keys`); `None` on an open network and
    // after any end of the association.
    let mut keys: Option<join::Keys> = None;
    /// What a pull saw, applied to the driver's memory of the join - here, so the frame module need not
    /// know what a join is.
    fn note_pull(
        p: &frames::Pulled,
        joined: &mut Option<([u8; join::MAX_SSID], u8)>,
        keys: &mut Option<join::Keys>,
        rekey_seen: &mut u32,
        ctx: &ServiceContext,
    ) {
        if let Some((event, reason)) = p.dropped_link {
            if joined.is_some() {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the access point dropped the link (event {} - {}, reason {}) - not joined; `wifi join` returns",
                    event, scan::code::name(event), reason
                ));
            }
            *joined = None;
            join::forget(keys);
        }
        if p.pairwise_failed > 0 {
            // The pull answered a restarted four-way handshake and it did not complete; the step is in
            // the log above. The access point decides what happens to the link next, and if it drops it
            // the pull sees that too.
            *rekey_seen = rekey_seen.wrapping_add(p.pairwise_failed);
            ctx.log("wifi-driver: a pairwise rekey did not complete - if the access point drops the link, `wifi join` brings it back");
        }
    }

    // THE CREDENTIAL SLOTS (`utilities/56_wifi.md` 6): a network name and the pairwise master key derived
    // from its passphrase, sixty-four of them. The passphrase itself is gone the moment the key exists. When
    // every slot is held, the one JOINED LONGEST AGO is replaced. They live here and nowhere else - not on
    // disk, so they need no `fs` and never write a network name to the card - and die with this instance.
    // About 70 bytes each, some 4.5 KiB in all, against a 16 MiB limit: the count is a BOUND (26.6), chosen
    // so that nobody reaches it, not a fit to the memory - a table that grew to fill what is available is
    // the elastic growth 26.6.1 says to resist.
    struct Stored {
        ssid: [u8; join::MAX_SSID],
        len: u8,
        pmk: [u8; crypto::PMK_LEN],
        /// When this key was last used to join (or was stored), monotonic seconds - the replacement order.
        used_at: i64,
    }
    const CREDENTIAL_SLOTS: usize = 64;
    let mut stored: [Option<Stored>; CREDENTIAL_SLOTS] = core::array::from_fn(|_| None);
    /// The slot holding this name, if any.
    fn slot_of(stored: &[Option<Stored>], ssid: &[u8]) -> Option<usize> {
        stored.iter().position(|s| {
            s.as_ref().map(|st| st.len as usize == ssid.len() && &st.ssid[..ssid.len()] == ssid).unwrap_or(false)
        })
    }
    /// The NOTE byte of one record: is a key held for this name, and is it the network joined. What a person
    /// picking from the list most needs to know, and what only this loop knows.
    fn note_for<'a>(
        stored: &'a [Option<Stored>],
        joined: &'a Option<([u8; join::MAX_SSID], u8)>,
    ) -> impl Fn(&scan::Network) -> u8 + 'a {
        move |n: &scan::Network| {
            let name = &n.ssid[..n.ssid_len as usize];
            let mut note = 0;
            if slot_of(stored, name).is_some() {
                note |= scan::reply::NOTE_SAVED;
            }
            if let Some((j, jl)) = joined {
                if *jl as usize == name.len() && &j[..name.len()] == name {
                    note |= scan::reply::NOTE_JOINED;
                }
            }
            note
        }
    }
    /// Where a new key goes: the slot already holding this name, else a free one, else the one used longest ago.
    fn slot_for(stored: &[Option<Stored>], ssid: &[u8]) -> usize {
        if let Some(i) = slot_of(stored, ssid) {
            return i;
        }
        if let Some(i) = stored.iter().position(|s| s.is_none()) {
            return i;
        }
        let mut oldest = 0;
        for (i, s) in stored.iter().enumerate() {
            if let (Some(a), Some(b)) = (s.as_ref(), stored[oldest].as_ref()) {
                if a.used_at < b.used_at {
                    oldest = i;
                }
            }
        }
        oldest
    }

    // The primitives that turn a passphrase into a key, checked against their published vectors. A wrong hash
    // would be refused by every access point in a way indistinguishable from a wrong passphrase, so if this
    // fails, passphrases are refused HERE, with the reason, rather than there, without one.
    let crypto_ok = crypto::selftest(ctx);
    let mut frame = [0u8; ctrl::FRAME];
    // Requests that could not be answered, and answers that could not be delivered - both loud.
    let mut capless: u32 = 0;
    let mut reply_failed: u32 = 0;

    loop {
        if !keyfile_settled && radio.is_some() {
            let mut entries = [keyfile::Entry::EMPTY; keyfile::MAX_SAVED];
            match keyfile::load(ctx, &mut entries) {
                keyfile::Load::Loaded(n) => {
                    keyfile_settled = true;
                    let now = ctx.epoch_secs_monotonic();
                    for (i, e) in entries.iter().take(n).enumerate() {
                        let ssid = &e.ssid[..e.len as usize];
                        let slot = slot_for(&stored, ssid);
                        // Most recent first in the file, so the first keeps the highest `used_at`.
                        stored[slot] = Some(Stored { ssid: e.ssid, len: e.len, pmk: e.pmk, used_at: now - i as i64 });
                    }
                    ctx.log_fmt(format_args!("wifi-driver: /wifi.keys loaded - {} network(s) known", n));
                    if n > 0 {
                        auto_join = Some((entries[0].ssid, entries[0].len, entries[0].sec));
                    }
                }
                keyfile::Load::NoFile => {
                    keyfile_settled = true;
                    ctx.log("wifi-driver: no /wifi.keys - nothing to rejoin; the first join writes it");
                }
                keyfile::Load::Unreachable => {
                    keyfile_tries += 1;
                    if keyfile_tries >= KEYFILE_TRIES {
                        keyfile_settled = true;
                        ctx.log_fmt(format_args!(
                            "wifi-driver: fs did not answer for /wifi.keys in {} tries - running on the table in memory alone this boot",
                            keyfile_tries
                        ));
                    }
                }
            }
            for e in entries.iter_mut() {
                e.pmk.fill(0);
            }
        }
        if let (Some((name, len, sec)), Some(session)) = (auto_join.take(), radio.as_mut()) {
            if radio_on && sweep.is_none() && joined.is_none() {
                ctx.log("wifi-driver: joining the network last joined, from /wifi.keys");
                match join_known(h, w, session, &name, len, sec, &mut stored, &mut keys, &mut joined,
                                 &mut joined_at_secs, &mut joined_security, &mut rxq, ctx) {
                    Some(join::Outcome::Joined) => {
                        last_joined = Some((name, len, sec));
                        save_keys(ctx, &stored);
                    }
                    Some(_) => ctx.log("wifi-driver: the network last joined did not take us back - not joined; `wifi join` when it is in range"),
                    None => {}
                }
            }
        }
        // ---- 1. One frame of the running sweep, if there is one. ----
        if let (Some(s), Some(session)) = (sweep.as_mut(), radio.as_mut()) {
            match scan::step(h, w, session, &mut s.scan, &mut frame, ctx) {
                scan::Step::Frame => {}
                scan::Step::Empty => {
                    s.empty += 1;
                    ctx.sleep_ms(1);
                    if s.empty >= scan::MAX_EMPTY_POLLS {
                        ctx.log_fmt(format_args!(
                            "wifi-driver: the sweep fell silent for {} empty polls without the firmware saying it \
                             was over - discarded ({} heard); the last complete scan stands",
                            s.empty,
                            s.scan.count()
                        ));
                        sweep = None;
                        sweep_failed = true;
                    }
                }
                scan::Step::Ended(why) => {
                    let done = sweep.take().unwrap_or(Sweep { scan: scan::Scan::new(), empty: 0 });
                    ctx.log_fmt(format_args!(
                        "wifi-driver: sweep complete - {} network(s), ended by {}",
                        done.scan.count(),
                        why
                    ));
                    cache = Some(Cache { scan: done.scan, at_secs: ctx.epoch_secs_monotonic() });
                    sweep_failed = false;
                }
            }
        }

        // ---- 2. A request. Blocking when idle - there is nothing else to do - and a look when sweeping. ----
        let req = if sweep.is_some() {
            match ctx.try_recv() {
                Some(m) => m,
                None => continue,
            }
        } else if !keyfile_settled {
            // `fs` may still be mounting when the radio comes up: wait for a request, but not forever, so
            // the load above gets its next try.
            match ctx.recv_timeout(ctx.duration_cycles(2_000)) {
                Some(m) => m,
                None => continue,
            }
        } else {
            ctx.recv()
        };
        let reply = match ctx.take_pending_cap() {
            Some(r) => r,
            None => {
                // No cap to answer on. Once, this was the WHOLE failure of the first frame-path boot:
                // reply caps were never reclaimed (below), the 64-slot table filled after some fifty
                // requests, the kernel could install no more, and every request after that landed here
                // and was dropped without a word - `net-stack` saw a radio that "stopped responding
                // after a while", `observe` saw this driver idle with an empty queue. Counted and said.
                capless = capless.saturating_add(1);
                if capless == 1 || capless % 64 == 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: a request arrived with no reply cap - dropped (x{}); if this repeats,                          the cap table is full and every answer is being lost",
                        capless
                    ));
                }
                continue;
            }
        };
        let (tag, payload) = untag(req.payload_bytes());
        let op = payload.first().copied().unwrap_or(0);
        // How long this request takes to serve, so a slow one is named from THIS side too: the shell and
        // `nic-driver` both bound their waits, and a driver that quietly took three seconds over a sweep
        // start (boot 2026-09-30 14:51) left neither of them able to say where the time went.
        let served_t0 = ctx.read_tsc();
        let n = match (op, radio.as_mut()) {
            // ---- Powered down (`wifi radio off hard`): these four arms come first, so nothing below talks
            // to a chip that has no power. ----
            (scan::reply::OP_STATUS, _) if powered_off => {
                // The status shape the shell knows, all zero: radio off, not associated, no cache claimed,
                // and the trailing power byte at 0 - the one fact that tells this off from the soft one.
                out[..30 + join::MAX_SSID].fill(0);
                out[0] = scan::reply::OK;
                30 + join::MAX_SSID
            }
            (scan::reply::OP_RADIO, _) if powered_off => {
                let mode = payload.get(1).copied().unwrap_or(1);
                out[1] = 0;
                out[3] = 0;
                out[4] = 0;
                if mode == 0 || mode == scan::reply::RADIO_HARD_OFF {
                    // Already as asked.
                    out[0] = scan::reply::OK;
                    out[2] = 0;
                } else if power_on_device(ctx, h) {
                    powered_off = false;
                    ctx.log("wifi-driver: `wifi radio on` on a powered-down chip - the power is restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                    out[0] = scan::reply::OK;
                    out[2] = 1;
                    out[3] = scan::reply::COLD_START;
                } else {
                    ctx.log("wifi-driver: `wifi radio on` on a powered-down chip, and the kernel refused to restore the power");
                    out[0] = scan::reply::NO_POWER_CONTROL;
                    out[2] = 0;
                }
                5
            }
            (_, Some(_)) if powered_off => {
                out[0] = scan::reply::RADIO_POWERED_OFF;
                1
            }
            (scan::reply::OP_RADIO, Some(session)) if payload.get(1).copied() == Some(scan::reply::RADIO_HARD_OFF) => {
                // `wifi radio off hard`: leave the network politely while the firmware can still send,
                // then cut the chip's power and stay that way. The keys held in memory go with the
                // association; `/wifi.keys` stays, and the cold start after `on` rejoins from it.
                let was_joined = joined.is_some();
                if let Some(s) = sweep.take() {
                    let _ = scan::abort(h, w, session, ctx);
                    ctx.log_fmt(format_args!(
                        "wifi-driver: hard off mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()));
                }
                if was_joined {
                    let _ = ctrl::disassoc(h, w, session, ctx);
                }
                joined = None;
                join::forget(&mut keys);
                out[1] = was_joined as u8;
                out[3] = 0;
                out[4] = 0;
                if ctx.device_power(false) {
                    // Quiet for as long as the power stays off - and while it is off nothing in this loop
                    // touches the bus after the one-CMD52 check below that the cut took: auto-join needs `radio_on`, the sweep was stopped above, and every
                    // request is answered from the powered-off arms (docs/wifi.md 48).
                    h.park(ctx);
                    powered_off = true;
                    radio_on = false;
                    ctx.log("wifi-driver: `wifi radio off hard` - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = scan::reply::OK;
                    out[2] = 1;
                    out[3] = verify_hard_off(ctx, h);
                } else {
                    ctx.log("wifi-driver: `wifi radio off hard` asked, and this machine has no control over the radio's power - the radio stays as it was");
                    out[0] = scan::reply::NO_POWER_CONTROL;
                    out[2] = 0;
                }
                5
            }
            // THE RADIO IS DOWN (no session) AND THE CHIP'S POWER IS STILL THIS DRIVER'S TO COMMAND. The
            // power ops are served here, because they are the way out of exactly this state: a firmware
            // that trapped at start leaves the radio down, and a `powercycle` or `off hard` typed then
            // used to be answered RADIO_DOWN - which the shell read, wrongly, as "no control over the
            // radio's power" (boot 2026-10-01 09:48). Everything else below stays "radio down".
            (scan::reply::OP_RADIO, None) if payload.get(1).copied() == Some(scan::reply::RADIO_POWERCYCLE) => {
                let cycled = power_cycle_device_ms(ctx, h, requested_off_ms(payload));
                ctx.log(if cycled {
                    "wifi-driver: `wifi radio powercycle` on a radio that is down - the chip's power was cut and restored; this instance expects to be restarted onto the cold chip"
                } else {
                    "wifi-driver: `wifi radio powercycle` on a radio that is down, and this machine has no control over the radio's power - nothing changed"
                });
                out[0] = if cycled { scan::reply::OK } else { scan::reply::NO_POWER_CONTROL };
                out[1] = 0;
                out[2] = cycled as u8;
                out[3] = 0;
                out[4] = 0;
                5
            }
            (scan::reply::OP_RADIO, None) if payload.get(1).copied() == Some(scan::reply::RADIO_HARD_OFF) => {
                if ctx.device_power(false) {
                    h.park(ctx);
                    powered_off = true;
                    radio_on = false;
                    ctx.log("wifi-driver: `wifi radio off hard` on a radio that is down - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = scan::reply::OK;
                    out[2] = 1;
                    out[3] = verify_hard_off(ctx, h);
                } else {
                    out[0] = scan::reply::NO_POWER_CONTROL;
                    out[2] = 0;
                    out[3] = 0;
                }
                out[1] = 0;
                out[4] = 0;
                5
            }
            (_, None) => {
                // Byte 1 says WHY (scan::reply::DOWN_*), so `wifi status` can tell a firmware that
                // trapped from a bring-up that stopped, instead of guessing.
                out[0] = scan::reply::RADIO_DOWN;
                out[1] = down_reason;
                2
            }
            (scan::reply::OP_LIST, Some(_)) => match (&sweep, &cache) {
                (Some(s), _) => {
                    out[0] = scan::reply::SCANNING;
                    out[1] = s.scan.count() as u8;
                    2
                }
                (None, Some(c)) => {
                    let note = note_for(&stored, &joined);
                    scan::write_reply(&c.scan, &note, &mut out)
                }
                (None, None) => {
                    out[0] = scan::reply::NO_SCAN_YET;
                    1
                }
            },
            // A powered-off radio cannot sweep or join; the cache and the status are still served.
            (scan::reply::OP_SCAN_START, Some(_)) | (scan::reply::OP_CONNECT, Some(_)) if !radio_on => {
                out[0] = scan::reply::RADIO_OFF;
                1
            }
            (scan::reply::OP_DISCONNECT, Some(session)) => {
                if let Some(s) = sweep.take() {
                    let _ = scan::abort(h, w, session, ctx);
                    ctx.log_fmt(format_args!(
                        "wifi-driver: a disconnect was asked for mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()
                    ));
                }
                let left = joined;
                // Sent whether or not this driver believes it is associated: the firmware's state is the truth,
                // and a stale belief here must not stop the operator leaving a network.
                let _ = ctrl::disassoc(h, w, session, ctx);
                joined = None;
                last_joined = None;
                join::forget(&mut keys);
                rxq.clear();
                // `[OK, was_joined, len, name[32]]` - the name of what was left, so the shell can say it.
                out[0] = scan::reply::OK;
                out[1] = left.is_some() as u8;
                out[2] = 0;
                out[3..3 + join::MAX_SSID].fill(0);
                if let Some((name, len)) = left {
                    out[2] = len;
                    out[3..3 + join::MAX_SSID].copy_from_slice(&name);
                }
                3 + join::MAX_SSID
            }
            // `wifi radio powercycle`: the CHIP's power, not the firmware's radio switch. The operator's form of
            // the recovery this driver does for itself when it finds a firmware it cannot adopt
            // (docs/wifi.md 47). The power is cut and restored here, because this service holds
            // DEVICE_POWER; what follows is the SHELL's, because it holds restart authority: it kills this
            // instance, and the respawn finds a card that does not answer the CCCR - the boot's own path.
            // The reply goes out before the kill arrives, so the operator is told the cycle happened (or
            // that this machine cannot do it) rather than left with a prompt that went quiet.
            (scan::reply::OP_RADIO, Some(_)) if payload.get(1).copied() == Some(scan::reply::RADIO_POWERCYCLE) => {
                let was_joined = joined.is_some();
                let cycled = power_cycle_device_ms(ctx, h, requested_off_ms(payload));
                if cycled {
                    ctx.log("wifi-driver: `wifi radio powercycle` - the chip's power was cut and restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                    joined = None;
                    last_joined = None;
                    join::forget(&mut keys);
                    radio_on = false;
                } else {
                    ctx.log("wifi-driver: `wifi radio powercycle` asked, and this machine has no control over the radio's power - nothing changed");
                }
                out[0] = if cycled { scan::reply::OK } else { scan::reply::NO_POWER_CONTROL };
                out[1] = was_joined as u8;
                out[2] = cycled as u8;
                out[3] = 0;
                out[4] = 0;
                5
            }
            (scan::reply::OP_RADIO, Some(session)) => {
                // Reply: `[status, was_joined, changed]`. `changed` is 0 when the radio was already in the
                // state asked for - and then NOTHING is sent to the firmware, because a DOWN to a radio that
                // is down is not a no-op on every firmware and an UP chain re-run resets a live interface.
                let on = payload.get(1).copied().unwrap_or(1) != 0;
                let was_joined = joined.is_some();
                out[2] = (on != radio_on) as u8;
                // EVERY byte the reply carries is written on EVERY path. `out` is one buffer reused for
                // every request, and the "already on" branch left byte 3 - the rejoin status - holding
                // whatever the previous reply put there. The shell read it as a rejoin outcome it had no
                // words for. Boot 2026-09-30 15:17: `radio already on` followed by "a reply this shell
                // does not understand".
                out[3] = 0;
                out[4] = 0;
                if on {
                    if !radio_on {
                        if !ctrl::interface_up(h, w, session, ctx) {
                            ctx.log("wifi-driver: the radio would not come back up - it stays off");
                            out[0] = scan::reply::JOIN_FAILED;
                            out[1] = 0;
                            // The status byte names a refused command; the shell says so in its own words.
                        } else {
                            radio_on = true;
                            out[0] = scan::reply::OK;
                            out[1] = 0;
                            // BACK ON THE NETWORK IT WAS ON. Reply bytes 3.. carry the rejoin: `[status,
                            // ssid_len, ssid[32]]`, status 0 when there was nothing to rejoin. The key
                            // is the held one - the passphrase is never asked for here - and an open
                            // network is rejoined open. A `forget` of the name leaves nothing to rejoin
                            // with, and the reply says so by attempting nothing.
                            if let Some((name, len, sec)) = last_joined {
                                ctx.log("wifi-driver: radio back on - rejoining the network last joined");
                                if let Some(outcome) = join_known(h, w, session, &name, len, sec, &mut stored, &mut keys,
                                                                  &mut joined, &mut joined_at_secs, &mut joined_security,
                                                                  &mut rxq, ctx) {
                                    out[3] = reply_of(outcome);
                                    out[4] = len;
                                    out[5..5 + join::MAX_SSID].copy_from_slice(&name);
                                    if outcome == join::Outcome::Joined {
                                        save_keys(ctx, &stored);
                                    }
                                }
                            }
                        }
                    } else {
                        out[0] = scan::reply::OK;
                        out[1] = 0;
                    }
                } else if !radio_on {
                    // Already off: nothing to stop, nothing to send.
                    out[0] = scan::reply::OK;
                    out[1] = 0;
                } else {
                    // `off` disconnects first (`utilities/56_wifi.md` 2), then takes the interface down.
                    if let Some(s) = sweep.take() {
                        let _ = scan::abort(h, w, session, ctx);
                        ctx.log_fmt(format_args!(
                            "wifi-driver: radio off mid-sweep - the sweep is stopped ({} heard, not kept)",
                            s.scan.count()
                        ));
                    }
                    if was_joined {
                        let _ = ctrl::disassoc(h, w, session, ctx);
                    }
                    joined = None;
                    join::forget(&mut keys);
                    if ctrl::radio_down(h, w, session, ctx) {
                        radio_on = false;
                        out[0] = scan::reply::OK;
                        // VERIFIED, as `on` is: ask the firmware whether it is still up.
                        out[3] = match ctrl::is_up(h, w, session, ctx) {
                            Some(false) => scan::reply::OFF_VERIFIED,
                            Some(true) => {
                                ctx.log("wifi-driver: `wifi radio off` - DOWN was accepted but the firmware still says it is up");
                                scan::reply::OFF_CONTRADICTED
                            }
                            None => {
                                ctx.log("wifi-driver: `wifi radio off` - the firmware did not answer whether it is up, so the off is unverified");
                                scan::reply::OFF_UNVERIFIED
                            }
                        };
                    } else {
                        ctx.log("wifi-driver: the firmware refused DOWN - the radio stays on");
                        out[0] = scan::reply::JOIN_FAILED;
                    }
                    out[1] = was_joined as u8;
                }
                5 + join::MAX_SSID
            }
            (scan::reply::OP_SCAN_START, Some(session)) => match &sweep {
                Some(s) => {
                    // Already sweeping: the caller attaches to it rather than starting a second - the radio
                    // has one sweep in it at a time.
                    out[0] = scan::reply::SCANNING;
                    out[1] = s.scan.count() as u8;
                    2
                }
                None => {
                    if scan::start(h, w, session, ctx) {
                        sweep = Some(Sweep { scan: scan::Scan::new(), empty: 0 });
                        sweep_failed = false;
                        out[0] = scan::reply::OK;
                        out[1] = 0;
                        2
                    } else {
                        out[0] = scan::reply::SCAN_FAILED;
                        1
                    }
                }
            },
            (scan::reply::OP_SCAN_POLL, Some(_)) => {
                let from = payload.get(1).copied().unwrap_or(0) as usize;
                match (&sweep, &cache) {
                    (Some(s), _) => {
                        let note = note_for(&stored, &joined);
                        scan::write_records(&s.scan, from, scan::reply::SCANNING, &note, &mut out)
                    }
                    (None, _) if sweep_failed => {
                        out[0] = scan::reply::SCAN_FAILED;
                        1
                    }
                    (None, Some(c)) => {
                        let note = note_for(&stored, &joined);
                        scan::write_records(&c.scan, from, scan::reply::SCAN_DONE, &note, &mut out)
                    }
                    (None, None) => {
                        out[0] = scan::reply::NO_SCAN_YET;
                        1
                    }
                }
            }
            (scan::reply::OP_SCAN_ABORT, Some(session)) => {
                let heard = match sweep.take() {
                    Some(s) => {
                        if !scan::abort(h, w, session, ctx) {
                            ctx.log("wifi-driver: the firmware did not take the abort - the sweep's events will be drained and discarded as they arrive");
                        }
                        ctx.log_fmt(format_args!(
                            "wifi-driver: sweep stopped by request - {} heard, not kept",
                            s.scan.count()
                        ));
                        s.scan.count()
                    }
                    None => 0,
                };
                out[0] = scan::reply::OK;
                out[1] = heard as u8;
                2
            }
            (scan::reply::OP_STATUS, Some(session)) => {
                // Reply: `[OK, sweeping, heard, has_cache, cache_count, age u32, radio_on, associated,
                //          bssid[6], rssi i32, chanspec u16, security, joined_secs u32, ssid_len, ssid[32]]`.
                out[0] = scan::reply::OK;
                out[1] = sweep.is_some() as u8;
                out[2] = sweep.as_ref().map(|s| s.scan.count()).unwrap_or(0) as u8;
                out[3] = cache.is_some() as u8;
                out[4] = cache.as_ref().map(|c| c.scan.count()).unwrap_or(0) as u8;
                let age = cache
                    .as_ref()
                    .map(|c| (ctx.epoch_secs_monotonic() - c.at_secs).max(0) as u32)
                    .unwrap_or(u32::MAX);
                out[5..9].copy_from_slice(&age.to_le_bytes());
                out[9] = radio_on as u8;

                // THE LINK IS READ, NOT REMEMBERED - but not while a sweep runs: a control exchange reads
                // frames off the bus and skips the ones that are not its reply, which mid-sweep would be the
                // scan's own results. During a sweep the status is the driver's memory, and says the sweep is
                // running, which is the fact that matters then.
                let link = if radio_on && sweep.is_none() { ctrl::link_now(h, w, session, ctx) } else { None };
                let (assoc, bssid, rssi, chanspec) = match &link {
                    Some(l) if l.associated() => (true, l.bssid, l.rssi, l.chanspec),
                    Some(_) => {
                        if joined.is_some() {
                            ctx.log("wifi-driver: the firmware reports no association - the remembered join is dropped");
                            joined = None;
                            join::forget(&mut keys);
                        }
                        (false, [0u8; 6], 0, 0)
                    }
                    None => (joined.is_some(), [0u8; 6], 0, 0),
                };
                out[10] = assoc as u8;
                out[11..17].copy_from_slice(&bssid);
                out[17..21].copy_from_slice(&rssi.to_le_bytes());
                out[21..23].copy_from_slice(&chanspec.to_le_bytes());
                out[23] = joined_security;
                let since = if assoc { (ctx.epoch_secs_monotonic() - joined_at_secs).max(0) as u32 } else { 0 };
                out[24..28].copy_from_slice(&since.to_le_bytes());
                match &joined {
                    Some((ssid, len)) => {
                        out[28] = *len;
                        out[29..29 + join::MAX_SSID].copy_from_slice(ssid);
                    }
                    None => {
                        out[28] = 0;
                        out[29..29 + join::MAX_SSID].fill(0);
                    }
                }
                // Trailing power byte: 1 here, because this arm is only reached with the chip powered. The
                // powered-down status arm above writes 0. The shell reads it to say which off this is.
                out[29 + join::MAX_SSID] = 1;
                30 + join::MAX_SSID
            }
            (scan::reply::OP_CONNECT, Some(session)) => {
                // A join and a sweep cannot share the radio. The sweep goes, and says so; the cache stays.
                if let Some(s) = sweep.take() {
                    let _ = scan::abort(h, w, session, ctx);
                    ctx.log_fmt(format_args!(
                        "wifi-driver: a join was asked for mid-sweep - the sweep is stopped ({} heard, not kept)",
                        s.scan.count()
                    ));
                }
                // `[op, ssid_len, ssid[32], pass_len, pass[64]]`. Lengths are checked against the fixed
                // fields, and the passphrase bytes are used from the request buffer and never copied
                // anywhere that outlives this arm.
                const SSID_LEN_AT: usize = 1;
                const SSID_AT: usize = 2;
                const PASS_LEN_AT: usize = 2 + join::MAX_SSID;
                const PASS_AT: usize = PASS_LEN_AT + 1;
                const TOTAL: usize = PASS_AT + join::MAX_PASSPHRASE;
                if payload.len() < TOTAL {
                    out[0] = scan::reply::JOIN_FAILED;
                    1
                } else {
                    let ssid_len = core::cmp::min(payload[SSID_LEN_AT] as usize, join::MAX_SSID);
                    let pass_len = core::cmp::min(payload[PASS_LEN_AT] as usize, join::MAX_PASSPHRASE);
                    let ssid = &payload[SSID_AT..SSID_AT + ssid_len];
                    let pass = &payload[PASS_AT..PASS_AT + pass_len];

                    // WHAT TO JOIN WITH, decided in this order and never guessed:
                    //  1. a passphrase in the request: derive the key, keep it in the slot (replacing whatever
                    //     was there), and join with it. The passphrase bytes live in `req`, which the next
                    //     `recv` overwrites; nothing copies them.
                    //  2. no passphrase, and the slot holds this name: join with the stored key.
                    //  3. no passphrase, and the last sweep heard this name as OPEN: join open.
                    //  4. otherwise: NEEDS_PASSPHRASE - the shell asks and sends again.
                    let name_of = |ssid: &[u8]| {
                        let mut name = [0u8; join::MAX_SSID];
                        name[..ssid.len()].copy_from_slice(ssid);
                        name
                    };
                    // ALREADY ON IT? Asked of the firmware, not remembered: `joined` names the network and
                    // `GET_BSSID` says whether the link is still up. A join of the network we are on sends
                    // nothing and says so; a stale memory of one is cleared and the join proceeds.
                    let on_this = joined
                        .as_ref()
                        .map(|(j, jl)| *jl as usize == ssid_len && &j[..ssid_len] == ssid)
                        .unwrap_or(false);
                    let already = on_this
                        && matches!(ctrl::link_now(h, w, session, ctx), Some(l) if l.associated());
                    if on_this && !already {
                        ctx.log("wifi-driver: the remembered join is not on the air any more - joining afresh");
                        joined = None;
                    }
                    let now = ctx.epoch_secs_monotonic();
                    // A KEY IS KEPT ONLY ONCE IT HAS JOINED. A passphrase typed now is derived into `pmk_buf`
                    // and enters the table after `JOINED`, not on arrival. The first hardware run of the
                    // handshake kept it on arrival, so a join that failed left behind a key nothing had
                    // proved, and the next `wifi join` used it without asking - the operator had to `forget`
                    // between every attempt. A key already in the table has joined before; it is used from
                    // there, and dropped only when the network refuses it as incorrect (its passphrase changed).
                    let mut pmk_buf = [0u8; crypto::PMK_LEN];
                    let mut fresh = false;
                    let mut use_slot: Option<usize> = None;
                    if !already && pass_len > 0 && crypto_ok {
                        pmk_buf = crypto::psk(pass, ssid);
                        fresh = true;
                        ctx.log("wifi-driver: pairwise master key derived from the passphrase - kept once it has joined");
                    } else if pass_len == 0 {
                        if let Some(i) = slot_of(&stored, ssid) {
                            ctx.log_fmt(format_args!("wifi-driver: joining with the key in credential slot {}", i));
                            // A match rather than an unwrap: a service never halts (Commandment V).
                            if let Some(st) = stored[i].as_mut() {
                                pmk_buf = st.pmk;
                                st.used_at = now;
                            }
                            use_slot = Some(i);
                        }
                    }
                    let secret = if pass_len > 0 {
                        if fresh {
                            Some(join::Secret::Pmk(&pmk_buf))
                        } else {
                            ctx.log("wifi-driver: a passphrase arrived and the key derivation failed its self-test at boot - refused");
                            None
                        }
                    } else if use_slot.is_some() {
                        Some(join::Secret::Pmk(&pmk_buf))
                    } else if cache
                        .as_ref()
                        .and_then(|c| c.scan.find(ssid))
                        .map(|n| n.security == scan::sec::OPEN)
                        .unwrap_or(false)
                    {
                        ctx.log("wifi-driver: the last sweep heard this network as open - joining without a key");
                        Some(join::Secret::Open)
                    } else {
                        None
                    };

                    if already {
                        out[0] = scan::reply::ALREADY_JOINED;
                        1
                    } else {
                    match secret {
                        None if pass_len > 0 => {
                            out[0] = scan::reply::JOIN_FAILED;
                            1
                        }
                        None => {
                            out[0] = scan::reply::NEEDS_PASSPHRASE;
                            1
                        }
                        Some(secret) => {
                            let outcome = join::join(h, w, session, ssid, secret, &mut keys, ctx);
                            if outcome == join::Outcome::Joined {
                                joined = Some((name_of(ssid), ssid_len as u8));
                                joined_at_secs = ctx.epoch_secs_monotonic();
                                rxq.clear();
                                last_joined = Some((name_of(ssid), ssid_len as u8, if matches!(secret, join::Secret::Open) { scan::sec::OPEN } else { scan::sec::WPA2 }));
                                joined_security = if matches!(secret, join::Secret::Open) {
                                    scan::sec::OPEN
                                } else {
                                    scan::sec::WPA2
                                };
                                if fresh {
                                    let i = slot_for(&stored, ssid);
                                    let replaced = stored[i].is_some() && slot_of(&stored, ssid) != Some(i);
                                    stored[i] = Some(Stored { ssid: name_of(ssid), len: ssid_len as u8, pmk: pmk_buf, used_at: now });
                                    ctx.log_fmt(format_args!(
                                        "wifi-driver: joined - the key is kept in credential slot {}{}",
                                        i,
                                        if replaced { " (replacing the one used longest ago)" } else { "" }
                                    ));
                                }
                                if !matches!(secret, join::Secret::Open) {
                                    save_keys(ctx, &stored);
                                }
                            } else {
                                joined = None;
                                if fresh {
                                    ctx.log("wifi-driver: the key from that passphrase is NOT kept - it did not join, so the next `wifi join` asks again");
                                } else if let Some(i) = use_slot {
                                    if matches!(outcome, join::Outcome::PassphraseRefused) {
                                        if let Some(st) = stored[i].as_mut() {
                                            st.pmk.fill(0);
                                            st.ssid.fill(0);
                                        }
                                        stored[i] = None;
                                        ctx.log_fmt(format_args!(
                                            "wifi-driver: the key in credential slot {} was refused as incorrect - dropped; the next `wifi join` asks again",
                                            i
                                        ));
                                    }
                                }
                                // A join that failed AFTER association leaves the firmware on the network with
                                // no keys - and `link_now` would then report it as joined, which the first
                                // hardware run of the handshake showed as `wifi status` saying `joined 5 min
                                // ago` to a `(hidden)` network after `wsec_key` was refused. The firmware's
                                // state is made to match this driver's answer. Harmless when there was no
                                // association to leave, exactly as `leave` is.
                                let _ = ctrl::disassoc(h, w, session, ctx);
                            }
                            out[0] = reply_of(outcome);
                            // The working copy of the key does not outlive the join it was for.
                            pmk_buf.fill(0);
                            1
                        }
                    }
                    }
                }
            }
            (scan::reply::OP_DEBUG, Some(session)) => {
                let sub = payload.get(1).copied().unwrap_or(scan::reply::dbg::STATS);
                out[0] = scan::reply::OK;
                match sub {
                    scan::reply::dbg::TRACE => {
                        let held = session.trace.held();
                        out[1] = held as u8;
                        let mut at = 2;
                        for i in 0..held {
                            let e = session.trace.entry(i);
                            out[at..at + 4].copy_from_slice(&e.ms.to_le_bytes());
                            out[at + 4] = e.kind;
                            out[at + 5] = e.chanflag;
                            out[at + 6..at + 8].copy_from_slice(&e.id.to_le_bytes());
                            out[at + 8..at + 12].copy_from_slice(&e.what.to_le_bytes());
                            out[at + 12..at + 16].copy_from_slice(&e.status.to_le_bytes());
                            // Entry stride is 18: len takes the last two bytes.
                            out[at + 16..at + 18].copy_from_slice(&e.len.to_le_bytes());
                            at += 18;
                        }
                        at
                    }
                    scan::reply::dbg::FIRMWARE => {
                        // Asked of the firmware now, not remembered from boot - but not mid-sweep, for the
                        // reason `OP_STATUS` gives.
                        let mut at = 1;
                        let mut ver = [0u8; 128];
                        let mut cap = [0u8; 512];
                        let mut mac = [0u8; 6];
                        if sweep.is_none() && radio_on {
                            let _ = ctrl::query_iovar(h, w, session, "ver", &mut ver, ctx);
                            let _ = ctrl::query_iovar(h, w, session, "cap", &mut cap, ctx);
                            let _ = ctrl::query_iovar(h, w, session, "cur_etheraddr", &mut mac, ctx);
                        }
                        let vlen = ver.iter().position(|&b| b == 0).unwrap_or(ver.len());
                        let clen = cap.iter().position(|&b| b == 0).unwrap_or(cap.len());
                        out[at] = vlen as u8;
                        at += 1;
                        out[at..at + 128].copy_from_slice(&ver);
                        at += 128;
                        out[at..at + 2].copy_from_slice(&(clen as u16).to_le_bytes());
                        at += 2;
                        out[at..at + 512].copy_from_slice(&cap);
                        at += 512;
                        out[at..at + 6].copy_from_slice(&mac);
                        at + 6
                    }
                    _ => {
                        let st = &session.stats;
                        let words: [u32; 20] = [
                            st.ctrl_sent, st.ctrl_accepted, st.ctrl_refused, st.ctrl_unanswered,
                            st.rx_ctrl, st.rx_event, st.rx_data, st.rx_glom, st.rx_header_only, st.rx_other,
                            st.tx_bytes, st.rx_bytes, st.rx_skipped_in_ctrl_wait,
                            st.events[0], st.events[1], st.events[2], st.events[3], st.events[4],
                            st.events[5], st.events[6],
                        ];
                        let mut at = 1;
                        for w32 in words.iter() {
                            out[at..at + 4].copy_from_slice(&w32.to_le_bytes());
                            at += 4;
                        }
                        // The last three event buckets, then the last-seen facts.
                        for w32 in [st.events[7], st.events[8], st.events[9]].iter() {
                            out[at..at + 4].copy_from_slice(&w32.to_le_bytes());
                            at += 4;
                        }
                        out[at..at + 4].copy_from_slice(&st.last_event_code.to_le_bytes());
                        out[at + 4..at + 8].copy_from_slice(&st.last_event_status.to_le_bytes());
                        out[at + 8..at + 12].copy_from_slice(&st.last_refused_cmd.to_le_bytes());
                        out[at + 12..at + 16].copy_from_slice(&st.last_refused_status.to_le_bytes());
                        at += 16;
                        // And the driver's clock, so the shell can say how long the session has run.
                        out[at..at + 4].copy_from_slice(&session.now_ms(ctx).to_le_bytes());
                        at += 4;
                        out[at..at + 4].copy_from_slice(&session.trace.total().to_le_bytes());
                        at += 4;
                        out[at..at + 4].copy_from_slice(&st.rx_glom_sub.to_le_bytes());
                        at + 4
                    }
                }
            }
            (scan::reply::OP_STORED, Some(_)) => {
                // `[OK, count, (len, ssid[32]) * count]` - names only, in slot order.
                out[0] = scan::reply::OK;
                let mut count = 0u8;
                let mut at = 2;
                for st in stored.iter().flatten() {
                    out[at] = st.len;
                    out[at + 1..at + 1 + join::MAX_SSID].copy_from_slice(&st.ssid);
                    at += 1 + join::MAX_SSID;
                    count += 1;
                }
                out[1] = count;
                at
            }
            (scan::reply::OP_FORGET, Some(_)) => {
                // `[10, len, ssid[32]]`.
                let len = core::cmp::min(payload.get(1).copied().unwrap_or(0) as usize, join::MAX_SSID);
                let dropped = match payload.get(2..2 + len).and_then(|ssid| slot_of(&stored, ssid)) {
                    Some(i) => {
                        // Zeroed, not just forgotten: the key must not linger in memory nothing points at.
                        if let Some(st) = stored[i].as_mut() {
                            st.pmk.fill(0);
                            st.ssid.fill(0);
                            st.len = 0;
                        }
                        stored[i] = None;
                        true
                    }
                    None => false,
                };
                if dropped {
                    save_keys(ctx, &stored);
                }
                out[0] = scan::reply::OK;
                out[1] = dropped as u8;
                2
            }
            // ---- THE FRAME INTERFACE (`frames.rs`), served to `nic-driver` alongside the `wifi` ops. The
            // op numbers start at 0x10 for the reason `dwc2`'s do: they share an endpoint with another
            // protocol. Every reply is tagged with its op, because the caller bounds its wait and a late
            // answer must not be read as the next one. ----
            (frames::OP_NET_INFO, Some(session)) => {
                // `[op, ok, mac(6), link]`. The address is the chip's, asked once; the link is this
                // driver's memory of the join, which every pull keeps honest. Not asked mid-sweep: a
                // control exchange would eat the sweep's frames, and a sweep is a moment of no link.
                out[0] = frames::OP_NET_INFO;
                if link_mac.is_none() && sweep.is_none() {
                    let mut mac = [0u8; 6];
                    if matches!(ctrl::query_iovar(h, w, session, "cur_etheraddr", &mut mac, ctx), Some(n) if n >= 6) {
                        link_mac = Some(mac);
                    }
                }
                match link_mac {
                    Some(mac) => {
                        out[1] = 1;
                        out[2..8].copy_from_slice(&mac);
                    }
                    None => {
                        out[1] = 0;
                        out[2..8].fill(0);
                    }
                }
                out[8] = (radio_on && joined.is_some() && sweep.is_none()) as u8;
                9
            }
            (frames::OP_NET_TX, Some(session)) => {
                // `[op, sent]`. Refused, not queued, when there is no link to send on: the stack retries
                // on its own pace and a refusal is a fact it can act on.
                out[0] = frames::OP_NET_TX;
                let eth = &payload[1..];
                let mut sent = false;
                if radio_on && joined.is_some() && sweep.is_none() && eth.len() >= scan::ev::ETHHDR {
                    if !session.tx_ok() {
                        // Credit comes back on received frames; a stack that only sends runs dry.
                        let p = frames::pull(h, w, session, &mut rxq, &mut frame, keys.as_mut(), ctx);
                        note_pull(&p, &mut joined, &mut keys, &mut rekey_seen, ctx);
                    }
                    if joined.is_some() {
                        sent = ctrl::send_data(h, w, session, eth, ctx);
                    }
                }
                out[1] = sent as u8;
                2
            }
            (frames::OP_NET_RX, Some(session)) => {
                // `[op, len_lo, len_hi, frame...]`, oldest first; a length of 0 is "nothing waiting".
                // The chip is read only when the queue is empty and the radio has a link to read.
                out[0] = frames::OP_NET_RX;
                if rxq.is_empty() && radio_on && joined.is_some() && sweep.is_none() {
                    let p = frames::pull(h, w, session, &mut rxq, &mut frame, keys.as_mut(), ctx);
                    note_pull(&p, &mut joined, &mut keys, &mut rekey_seen, ctx);
                }
                let n = rxq.pop(&mut out[3..]);
                out[1..3].copy_from_slice(&(n as u16).to_le_bytes());
                3 + n
            }
            _ => {
                out[0] = scan::reply::UNKNOWN_OP;
                1
            }
        };
        // `try_send`, never `send`: the shell may have given up on this reply (`q`, or its own deadline),
        // and a blocking send toward a peer that is not receiving is the mutual-blocking anti-pattern §8.9
        // names. A failed reply usually means nobody was waiting - but it can also mean the caller's
        // queue is FULL while it waits (`nic-driver` blocked on this very answer with sixteen stale
        // requests behind it), and that one is worth seeing, so it is counted and reported sparingly.
        let served_ms = ctx.read_tsc().wrapping_sub(served_t0) / ctx.duration_cycles(1).max(1);
        if served_ms >= 500 {
            ctx.log_fmt(format_args!("wifi-driver: op {:#04x} took {} ms to serve", op, served_ms));
        }
        if ctx.try_send_by_handle(reply, &tagged_reply(tag, &out[..n])).is_err() {
            reply_failed = reply_failed.saturating_add(1);
            if reply_failed == 1 || reply_failed % 64 == 0 {
                ctx.log_fmt(format_args!(
                    "wifi-driver: an answer (op {:#04x}) could not be delivered (x{}) - the caller gave up, or its queue is full",
                    op, reply_failed
                ));
            }
        }
        // RECLAIM THE REPLY CAP. A one-shot cap the caller derived for this answer; used or not, it is
        // this task's slot until released, and a task holds 64. This line was missing, and the driver
        // went deaf after its first fifty requests - see the `capless` arm above.
        ctx.remove_cap(reply);
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
            serve_unavailable(&ctx, None);
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 1 - granted {} byte(s) of SDIO host registers",
        mmio.len()
    ));
    // THE CLOCK, BEFORE ANYTHING TOUCHES THE CHIP: a lease from `power` holds the Arm cores fast for the
    // bring-up, and every exit below hands it back (docs/power.md, docs/wifi.md 55).
    let lease = clock_lease(&ctx);

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
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
    }

    // ---- Stage 3: what is on the bus. ------------------------------------------------------------
    let mut card = match sdio::identify(&h, &ctx) {
        Some(c) => c,
        None => {
            // NO ANSWER MAY MEAN NO POWER. An instance that died with the chip's power cut - killed in the
            // middle of a power cycle, or while `wifi radio off hard` held it down - leaves WL_ON low, and
            // its respawn finds an empty bus. Boot 2026-10-01 13:59: three respawns in a row, each "no SDIO
            // card answered", and `wifi radio on` could not bring it back. Where this service holds the
            // power, it is restored the boot's way - host parked across the edge, `power_on_device` - and
            // the bus is asked once more. A pin that is already high makes no edge, so a powered chip that
            // truly does not answer costs one more identification and the settle, then is reported as
            // before. Stated consequence: a driver that dies during `off hard` comes back with the radio
            // powered; the alternative was a radio no command short of a power cycle could reach.
            let retried = if power_on_device(&ctx, &h) {
                ctx.log("wifi-driver: no SDIO card answered - the chip's power is now asserted (a dead instance may have left it cut), and the bus is asked again");
                if h.reset(&ctx) { sdio::identify(&h, &ctx) } else { None }
            } else {
                None
            };
            match retried {
                Some(c) => c,
                None => {
                    ctx.log(
                        "wifi-driver: no SDIO card answered on this bus. The controller is ours and came up, \
                         so the remaining suspects are the ones the kernel reports at boot: the SD power \
                         domain and the GPIO34-39 mux",
                    );
                    { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
                }
            }
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
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
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
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
    }
    let mut window = backplane::Window::new();
    // The radio's session, if boot brings it up. The serving loop scans on it when the shell asks; `None`
    // means every such request is answered "radio down" - loudly, and without pretending (§26.7).
    let mut radio: Option<ctrl::Session> = None;
    // Set when the firmware is seen to trap at start; it decides what `radio down` tells the shell.
    let mut trapped = false;
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

            // ---- ADOPT: the firmware an earlier instance loaded is running; attach to it. -------------
            // `identify` found the card initialised with function 2 up, which only a running firmware
            // holds up. Everything that would disturb that firmware is skipped - no halt, no core reset,
            // no upload (stages 9-11) - and the bus is brought up on it as it stands. Any transfer the
            // dead instance left in flight is aborted per function first (IO_ABORT, not RES). The
            // firmware is then asked the same first question the boot asks; if it answers, this instance
            // carries on with it, joins from `/wifi.keys` as a fresh one would, and the kill cost
            // seconds. If it does not answer there is nothing left to try on this chip short of cutting
            // its power (`backlog/69`), and the radio says so rather than restarting a firmware the ROM
            // will not boot.
            if card.warm {
                ctx.log("wifi-driver: ADOPT - stages 9 to 11 are skipped: no halt, no reset, no upload. The running firmware is asked to answer");
                sdio::abort(&h, 1, &ctx);
                sdio::abort(&h, 2, &ctx);
                match cores.as_ref().and_then(|c| c.sdiod.as_ref()) {
                    Some(sdiod) => {
                        if bus::bring_up(&h, &mut window, sdiod.base, &ctx) && ctrl::report_mac(&h, &mut window, &ctx) {
                            ctx.log("wifi-driver: ADOPTED - the firmware the earlier instance loaded answers; this instance carries on with it");
                            radio = scan::run(&h, &mut window, true, &ctx);
                        } else if power_cycle_device(&ctx, &h) {
                            // THE FIRMWARE IS DEAD AND THE CHIP IS NOW COLD. Everything the warm path did
                            // on the SDIO side has to be done again on the fresh card - identification,
                            // the operating clock, block sizes, function 1, the backplane - and the
                            // backplane window object forgets what it thought it had set. The core table
                            // from the EROM walk is the same silicon and stands. Then `card.warm` is
                            // cleared and the boot's own path, stage 9 on, takes over.
                            ctx.log("wifi-driver: the running firmware did not answer - the radio was power-cycled, and this instance starts it from cold");
                            // The host was PARKED across the cut and has no clock; `identify_once` sets
                            // none. Back from reset first, at the identification clock - power, then the
                            // init clock (docs/wifi.md 48).
                            let fresh_card = if h.reset(&ctx) {
                                sdio::identify_once(&h, &ctx)
                            } else {
                                ctx.log("wifi-driver: the host did not come back from its park after the power cycle");
                                None
                            };
                            match fresh_card {
                                Some(fresh) => {
                                    card = fresh;
                                    if !h.set_operating_clock(OPERATING_HZ, &ctx) {
                                        ctx.log("wifi-driver: the operating clock would not stabilise after the power cycle - continuing at the identification clock");
                                    }
                                    sdio::set_block_size(&h, 1, 64, &ctx);
                                    sdio::set_block_size(&h, 2, 512, &ctx);
                                    if !sdio::enable_function(&h, 1, &ctx) || !backplane::wake(&h, &ctx) {
                                        ctx.log("wifi-driver: the power-cycled chip did not open its backplane - the radio stays down");
                                        card.warm = true; // keep stage 9 off a chip that did not come back
                                    } else {
                                        window = backplane::Window::new();
                                    }
                                }
                                None => {
                                    ctx.log("wifi-driver: the power-cycled chip did not identify - the radio stays down");
                                }
                            }
                        } else {
                            ctx.log("wifi-driver: the running firmware did not answer, this machine has no control over the radio's power, and a firmware restarted on a warm chip traps in its ROM (docs/wifi.md 45) - the radio stays down: this machine cannot restart a firmware that stopped without cutting the chip's power");
                        }
                    }
                    None => ctx.log(
                        "wifi-driver: the EROM described no SDIO device core, so the running firmware cannot be reached - the radio stays down until a reboot",
                    ),
                }
            }

            // ---- Stage 9: how much RAM, and where the firmware goes. ---------------------------------
            // The upload needs an address and a size. The CR4 reports its TCM as a set of BANKS through
            // its own registers - reached by the core's BASE, not its wrapper, which is why a wrapper of
            // 0 does not block this - and the firmware's start address is a per-part constant the
            // reference keeps in a table rather than a formula. NOT on an adopted firmware: the read
            // below halts the core it would size.
            if !card.warm {
                ctx.log("wifi-driver: stage 9 - asking the ARM core how much TCM it has");
            }
            match if card.warm { None } else { cores.as_ref().and_then(|c| c.arm) } {
                Some(arm) => match {
                    // THE CORE IS RESET AND RELEASED WITH ITS CPU HALTED BEFORE IT IS ASKED. A chip a
                    // dead instance left running its firmware answers `ARMCR4_CAP` with zero - 50
                    // respawns under `chaos max-carnage`, 50 times "ZERO memory banks", radio down for
                    // the life of each - and so does a core HELD in reset, which the first attempt at
                    // this (a plain `aicore::disable`) found out on a fresh boot. The register reads
                    // with the core clocked, out of reset and halted: the state brcmfmac's
                    // `brcmf_chip_recognition` puts the chip in before it sizes the RAM ("assure chip
                    // is passive for core register access" - for a CR4, a reset-core with CPUHALT,
                    // not a disable). `aicore::reset(halt = true)` is that sequence and stage 11
                    // already performs it for the upload; here it happens first, where the read needs
                    // it. A fresh chip happens to answer unhalted; one running firmware does not, and
                    // asking in the reference's state serves both (26.14).
                    if let Some(wrap) = arm.wrapper() {
                        if !aicore::reset(&h, &mut window, wrap, true, &ctx) {
                            ctx.log(
                                "wifi-driver: the ARM core could not be reset and halted before its \
                                 memory is sized - the read below may say zero",
                            );
                        }
                    }
                    armcr4::probe(&h, &mut window, arm.base, id.id, &ctx)
                } {
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
                                // THE 802.11 CORE IS RESET BEFORE THE FIRMWARE GOES IN. This is the
                                // other half of the reference's passive step for a CR4 chip
                                // (`brcmf_chip_cr4_set_passive`: disable the ARM, then reset the D11
                                // core with PHYRESET|PHYCLOCKEN going in and PHYCLOCKEN coming out). A
                                // fresh chip has never run anything, so the firmware finds the D11 as
                                // the reset left it; a RESPAWN finds it as the dead instance's firmware
                                // left it, mid-whatever, and the new firmware came alive over that
                                // state and never brought function 2 ready (26.14). The log says which
                                // state the core was found in, so a boot can tell whether it mattered.
                                if let Some(dw) = cores.as_ref().and_then(|c| c.wlan).and_then(|d| d.wrapper()) {
                                    ctx.log("wifi-driver: resetting the 802.11 core before the upload");
                                    if !aicore::reset_bits(
                                        &h, &mut window, dw,
                                        aicore::D11_PHYRESET | aicore::D11_PHYCLOCKEN,
                                        aicore::D11_PHYCLOCKEN,
                                        aicore::D11_PHYCLOCKEN,
                                        &ctx,
                                    ) {
                                        ctx.log("wifi-driver: the 802.11 core did not come out of its reset cleanly - continuing, the firmware may not bring its functions up");
                                    }
                                }
                                // LINUX'S PRE-DOWNLOAD STEPS (`brcmf_sdio_probe_attach`, after the cores are
                                // passive): KSO, CARDCTRL WLANRESET and PMU RES_RELOAD. This driver never did
                                // them; the chip comes up warm after most power cuts here, and Linux's recovery
                                // rests on the same WL_ON cut plus these (docs/wifi.md 51).
                                linux_pre_download(&h, &mut window, cores.as_ref(), &ctx);
                                ctx.log("wifi-driver: stage 11 - uploading the firmware");
                                if upload::run(&h, &mut window, wrap, &ram, &mut trapped, &ctx) {
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
                                            if bus::bring_up(&h, &mut window, sdiod.base, &ctx) {
                                                // ---- Stage 13: ask the firmware something. ---------
                                                // ONLY WITH A BUS THAT REPORTED ITSELF READY. A control
                                                // frame sent into a data function that never came up
                                                // would time out for a reason that has nothing to do
                                                // with the protocol being built here.
                                                if ctrl::report_mac(&h, &mut window, &ctx) {
                                                    // ---- Stage 14: scan. -----------------
                                                    // ONLY ONCE THE CONTROL CHANNEL HAS
                                                    // ANSWERED. A scan is a set plus a
                                                    // stream of events, so running it
                                                    // against a channel that has never
                                                    // replied would confuse "the scan is
                                                    // wrong" with "nothing works yet".
                                                    radio = scan::run(&h, &mut window, false, &ctx);
                                                }
                                            }
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
                None => if !card.warm {
                    ctx.log("wifi-driver: no ARM core was found, so there is nothing to ask about TCM")
                },
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
        // FOURTH TIME. This line has now been wrong in four different ways as the code moved past it, the
        // last being "no control channel" printed directly after a list of ten networks the control channel
        // fetched. It no longer describes the radio's state at all - the stages above do that, each on its
        // own line, and they are read rather than asserted. What it says is the one thing still true: the
        // SHELL has no way to ask this driver for any of it yet.
        // FIFTH TIME, and the last: it now reports the one fact the serving loop is about to act on.
        if radio.is_some() {
            "wifi-driver: the stages above are the radio's state, each reported as it was read. The \
             radio is up and the shell may ask it to sweep: `wifi scan`"
        } else {
            "wifi-driver: the stages above are the radio's state, each reported as it was read. The \
             radio did NOT come up, so every `wifi` request will be answered `radio down` rather than left waiting"
        },
    );
    let down_reason = if trapped { scan::reply::DOWN_TRAPPED } else { scan::reply::DOWN_BRINGUP };
    clock_release(&ctx, lease);
    serve_radio(&ctx, &h, &mut window, radio, down_reason)
}
