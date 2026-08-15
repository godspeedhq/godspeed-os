//! Find and configure a boot-protocol keyboard.
//!
//! Slice 2, first part (`docs/arm32-usb-userspace.md`): walk a device's configuration descriptor,
//! find the boot-HID interface and its interrupt IN endpoint, then configure the device and put it
//! in boot protocol. All of that is control transfers, which Slice 1 proved end to end - including
//! through a transaction translator, which this keyboard needs (it is the low-speed device on port 4).
//!
//! POLLING the endpoint is the next part and is where the remaining risk of this port lives: a
//! periodic split is microframe-scheduled, and that code moves from ring 0 with interrupts masked
//! into a preemptible task. Binding first means that when polling is attempted, everything under it
//! is known good.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

/// Descriptor types.
const DESC_CONFIG: u8 = 0x02;
const DESC_INTERFACE: u8 = 0x04;
const DESC_ENDPOINT: u8 = 0x05;

/// USB HID class, boot subclass, keyboard protocol.
const CLASS_HID: u8 = 0x03;
const SUBCLASS_BOOT: u8 = 0x01;
const PROTOCOL_KEYBOARD: u8 = 0x01;

/// Endpoint attribute bits: transfer type is the low two, 3 = interrupt.
const EP_TYPE_INTERRUPT: u8 = 0x03;

/// Where the keyboard's 8-byte boot report is DMA'd.
///
/// **Clear of the control-transfer scratch, which the first version was not.** `DATA_OFF` is 0x0040
/// and `DATA_LEN` is 256, so that scratch spans 0x0040..0x0140 - and this was 0x0100, inside it. The
/// 256-byte config-descriptor read landed directly on the report buffer, so the poll then read back
/// leftover descriptor bytes: non-zero, so they passed the "is there a keypress" filter, and garbage,
/// so they decoded to nothing. 88 reports in 15 seconds with nothing typed and nothing printed.
///
/// The previous comment on this line asserted it was clear of the scratch. It was not, and asserting
/// it is what stopped me checking - a comment that states a layout invariant without deriving it is
/// worth less than the arithmetic it replaced.
///
/// 0x0200 is past `DATA_OFF + DATA_LEN` (0x0140) with room to spare, and the assertion below now
/// derives the property instead of claiming it.
pub const REPORT_OFF: usize = 0x0200;
const _: () = assert!(REPORT_OFF >= chan::DATA_OFF + chan::DATA_LEN);

/// A bound boot keyboard.
pub struct Keyboard {
    /// Endpoint number (not the full address - the direction bit is stripped).
    pub ep: u8,
    /// Max packet size of the interrupt endpoint. A boot report is 8 bytes.
    pub mps: u16,
    /// Polling interval the device asked for, in frames.
    pub interval: u8,
}

/// Walk a configuration descriptor looking for a boot-protocol keyboard.
///
/// Returns the interface number and its interrupt IN endpoint. The walk is bounded by the buffer and
/// refuses a zero-length descriptor, because a device-supplied length of 0 would otherwise leave the
/// cursor fixed and spin forever - the classic parser hang, and device-supplied data is exactly where
/// it comes from.
fn find_boot_keyboard(buf: &[u8], total: usize) -> Option<(u8, Keyboard)> {
    let mut i = 0usize;
    let mut in_boot_kbd = false;
    let mut iface = 0u8;
    while i + 2 <= total {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > total {
            break; // malformed: end the walk rather than spinning on it
        }
        if ty == DESC_INTERFACE && len >= 9 {
            // A new interface ends the previous one's claim, so this is recomputed rather than latched.
            in_boot_kbd = buf[i + 5] == CLASS_HID
                && buf[i + 6] == SUBCLASS_BOOT
                && buf[i + 7] == PROTOCOL_KEYBOARD;
            iface = buf[i + 2];
        } else if ty == DESC_ENDPOINT && len >= 7 && in_boot_kbd {
            let addr = buf[i + 2];
            let attrs = buf[i + 3];
            // Bit 7 of bEndpointAddress is the direction: 1 = IN. A keyboard's report endpoint is IN,
            // and taking an OUT endpoint here would arm a channel that never delivers anything.
            if addr & 0x80 != 0 && attrs & 0x03 == EP_TYPE_INTERRUPT {
                return Some((
                    iface,
                    Keyboard {
                        ep: addr & 0x0F,
                        mps: u16::from_le_bytes([buf[i + 4], buf[i + 5]]),
                        interval: buf[i + 6],
                    },
                ));
            }
        }
        i += len;
    }
    None
}

/// Configure a device and, if it is a boot keyboard, put it in boot protocol.
///
/// `splt` is the split descriptor for this device (0 if it is directly attached), so the same code
/// serves a keyboard on a hub port and one plugged straight into the root port.
pub fn bind(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, splt: u32,
) -> Option<Keyboard> {
    // TWO ATTEMPTS, because one dropped control transfer used to cost the keyboard entirely.
    //
    // Measured on hardware across 23 dwc2 restarts: 7 bound the keyboard, and 3 more enumerated port 4
    // (`046d:c30a`, low speed, via split) and then bound NOTHING, with no failure logged anywhere. The
    // other 13 were killed mid-bring-up by the storm, which is honest. So of the restarts that actually
    // reached the keyboard, 30% lost it - and the operator sees that as "the keyboard works, then
    // doesn't, then does".
    //
    // A low-speed device reached through a hub's transaction translator is the flakiest path in this
    // driver, and the descriptor read is three control transfers deep. Retrying is what every other
    // device here effectively gets from its own init path; the keyboard had one shot.
    for attempt in 0..2u32 {
        match try_bind(ctx, mmio, dma, t, splt) {
            BindOutcome::Bound(k) => return Some(k),
            // Ordinary and common: three of the four ports on this board are not keyboards. Never
            // logged, and never retried - retrying "this is a disk" would just slow every enumeration.
            BindOutcome::NotAKeyboard => return None,
            BindOutcome::Failed(why) => {
                if attempt == 0 {
                    ctx.log_fmt(format_args!("dwc2-svc: keyboard bind failed ({}) - retrying once", why));
                } else {
                    // Loud on the way out. This path used to return None in silence, so a keyboard that
                    // failed to bind was indistinguishable from a port with no keyboard on it - which is
                    // why the intermittency went unexplained for as long as it did.
                    ctx.log_fmt(format_args!(
                        "dwc2-svc: KEYBOARD NOT BOUND after 2 attempts ({}) - this device looks like a \
                         keyboard but did not bind; input will be dead until the next enumeration", why));
                }
            }
        }
    }
    None
}

/// What one bind attempt concluded. The three outcomes need different handling and used to be
/// collapsed into `Option`: "not a keyboard" and "a keyboard that failed" both returned `None`, so the
/// second could not be reported without also reporting the first on every non-keyboard port.
enum BindOutcome {
    Bound(Keyboard),
    NotAKeyboard,
    Failed(&'static str),
}

/// True if the config descriptor declares ANY HID interface. It is what separates "this port has a
/// disk on it" from "this really is a keyboard and something went wrong", and therefore what makes the
/// failure reportable without drowning every enumeration in noise about the ports that are not.
fn has_hid_interface(buf: &[u8], total: usize) -> bool {
    let mut i = 0usize;
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            break;
        }
        if buf[i + 1] == DESC_INTERFACE && len >= 9 && buf[i + 5] == CLASS_HID {
            return true;
        }
        i += len;
    }
    false
}

fn try_bind(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, splt: u32,
) -> BindOutcome {
    // The 9-byte header first, for wTotalLength - the full descriptor's length is not knowable in
    // advance and asking for a fixed guess either truncates it or overruns the device's answer.
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control_split(ctx, mmio, dma, t, &get9, &mut head, true, 9, splt) {
        return BindOutcome::Failed("config descriptor header read");
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let cfg_val = head[5];

    // Clamp to the scratch buffer. A device is free to report a wTotalLength larger than this driver
    // has room for, and the programmed length is what the controller DMAs - so clamping the request
    // is what keeps the device out of memory past the buffer, not clamping the copy afterwards.
    let want = total.min(chan::DATA_LEN);
    let mut full = [0u8; chan::DATA_LEN];
    let getall = [
        0x80, 0x06, 0, DESC_CONFIG, 0, 0,
        (want & 0xFF) as u8, ((want >> 8) & 0xFF) as u8,
    ];
    if !chan::control_split(ctx, mmio, dma, t, &getall, &mut full, true, want, splt) {
        return BindOutcome::Failed("full config descriptor read");
    }

    let (iface, kbd) = match find_boot_keyboard(&full, want) {
        Some(x) => x,
        // A HID interface with no usable boot-keyboard endpoint is NOT the ordinary "this is a disk"
        // answer - the descriptor either arrived short or arrived wrong, and that is worth retrying
        // and worth saying. Without this split the two were the same silent `None`.
        None if has_hid_interface(&full, want) =>
            return BindOutcome::Failed("HID interface present but no boot-keyboard endpoint in it"),
        None => return BindOutcome::NotAKeyboard,
    };

    // SET_CONFIGURATION, for the same reason the hub needed it: class and interface requests are only
    // legal in the CONFIGURED state, and a device that skips this accepts them and answers nothing.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control_split(ctx, mmio, dma, t, &setcfg, &mut none, false, 0, splt) {
        return BindOutcome::Failed("SET_CONFIGURATION");
    }

    // SET_PROTOCOL(boot). bmRequestType 0x21 = host-to-device, CLASS, INTERFACE recipient - so wIndex
    // is the INTERFACE number, not a port or a device. Boot protocol is what makes the report a fixed
    // 8-byte layout instead of one that has to be decoded from a HID report descriptor.
    let setproto = [0x21, 0x0B, 0, 0, iface, 0, 0, 0];
    if !chan::control_split(ctx, mmio, dma, t, &setproto, &mut none, false, 0, splt) {
        // Not fatal: a keyboard that refuses SET_PROTOCOL is usually already in boot protocol, which
        // is the default. Report it and keep the binding rather than discarding a working keyboard.
        ctx.log("dwc2-svc: SET_PROTOCOL(boot) refused - continuing (boot protocol is the default)");
    }

    ctx.log_fmt(format_args!(
        "dwc2-svc: BOOT KEYBOARD bound - interface {} endpoint {} mps {} interval {}",
        iface, kbd.ep, kbd.mps, kbd.interval));
    BindOutcome::Bound(kbd)
}

/// Poll the keyboard once and push any keystrokes to the console.
///
/// Returns true if a report arrived. An idle keyboard NAKs, which is a positive answer of "nothing
/// happened" rather than a failure - so a false here is the normal case and must not be logged.
pub fn poll(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, splt: u32, kbd: &Keyboard,
    state: &mut KeyState,
) -> bool {
    let len = (kbd.mps as u32).min(8);
    let phys = dma.phys_at(REPORT_OFF) as u32;
    // Zero first, for the same reason every IN transfer here does: a short report would otherwise
    // leave the PREVIOUS keystroke in the buffer and it would be delivered a second time.
    for i in 0..8 {
        dma.write8(REPORT_OFF + i, 0);
    }

    let hcint = chan::periodic_split_in(
        ctx, mmio, t, chan::CH_KBD, state.pid, phys, len, kbd.ep as u32, splt);

    // Classify this poll BEFORE acting on it. Every return path below consumes `hcint` for a
    // decision and then discards it; counting first is what makes a silent endpoint explicable
    // afterwards instead of only observable as "the keyboard stopped".
    {
        use crate::regs::*;
        let known = HCINT_XFERCOMPL | HCINT_NAK | HCINT_CHHLTD | HCINT_STALL | HCINT_NYET
            | HCINT_XACTERR | HCINT_ACK;
        if hcint & HCINT_NYET == 0 {
            state.nyet_run = 0; // anything else is the TT answering, so it is not wedged
        }
        // ERRORS SINCE THE LAST REPORT, not consecutive errors.
        //
        // This used to reset on ANY non-error outcome, including a NAK - and a degraded endpoint
        // does not fail cleanly, it alternates. Measured after a storm: the translator was cleared,
        // the endpoint kept erroring in a mixed NAK/XACTERR state, and the consecutive counter kept
        // resetting, so a threshold worth 300 ms of polling took 5.2 SECONDS to trip. That gap is
        // the keyboard outage an operator feels.
        //
        // A NAK means "nothing changed", which is not evidence the endpoint is HEALTHY - only a
        // delivered report is. So the counter now clears on data and survives NAKs, and it is a RATE:
        // the window restarts if the errors are spread thinly enough not to matter, so ordinary
        // XACTERR noise on a low-speed split device still never triggers a port reset.
        if hcint & HCINT_XFERCOMPL != 0 && hcint & HCINT_NAK == 0 {
            state.xacterr_run = 0;      // a real report: the endpoint is transacting
            state.xacterr_since = 0;
        } else if hcint & HCINT_XACTERR != 0 {
            let now = ctx.read_tsc();
            // Restart the window if the last error is stale - errors that far apart are noise, not a
            // broken endpoint.
            if state.xacterr_since == 0
                || now.wrapping_sub(state.xacterr_since) > ctx.duration_cycles(2000)
            {
                state.xacterr_since = now;
                state.xacterr_run = 1;
            }
        }
        if hcint == 0 {
            state.n_silent = state.n_silent.wrapping_add(1);
        } else if hcint & HCINT_STALL != 0 {
            state.n_stall = state.n_stall.wrapping_add(1);
        } else if hcint & HCINT_XACTERR != 0 {
            state.n_xacterr = state.n_xacterr.wrapping_add(1);
            state.xacterr_run = state.xacterr_run.wrapping_add(1);
        } else if hcint & HCINT_NAK != 0 {
            state.n_nak = state.n_nak.wrapping_add(1);
        } else if hcint & HCINT_XFERCOMPL != 0 {
            state.n_data = state.n_data.wrapping_add(1);
        } else if hcint & HCINT_NYET != 0 {
            state.n_nyet = state.n_nyet.wrapping_add(1);
            state.nyet_run = state.nyet_run.wrapping_add(1);
        } else {
            state.n_other = state.n_other.wrapping_add(1);
            state.last_other = hcint;
        }
        let _ = known;
    }

    // A NAK is not a report, even when XFERCOMPL rides along with it.
    //
    // HCINT can latch BOTH, and testing only XFERCOMPL counted every idle poll as a keystroke: the
    // counter climbed at ~2/second with nothing typed, which made it useless for the one question it
    // exists to answer - are keystrokes reaching us, or reaching us and failing to decode?
    if hcint & crate::regs::HCINT_XFERCOMPL == 0 || hcint & crate::regs::HCINT_NAK != 0 {
        // AUTO-REPEAT IS DRIVEN FROM HERE, and this is the only place it can be driven from.
        //
        // A USB boot keyboard reports on CHANGE only: press "d" and it sends one report, then
        // NOTHING until something changes. So while a key is held, every poll lands on exactly this
        // NAK path - and the repeat timer was only ever advanced by `decode_keyboard`, which needs a
        // report. The timer therefore could not tick during the one condition it exists to serve, and
        // holding a key produced a single "d" forever.
        //
        // `KeyRepeat::poll` is documented "call once per poll iteration" and was called zero times per
        // poll iteration. The repeat machinery was complete, calibrated, and unreachable - which is
        // why this reads as a missing feature rather than a broken one.
        // REPEAT ONLY ON POSITIVE EVIDENCE, AND ONLY WHILE WE ARE STILL POLLING HEALTHILY.
        //
        // A NAK is the keyboard answering "nothing changed", which corroborates that the key is still
        // down. No completion at all (`hcint == 0`: a timed-out or failed transfer) is not evidence of
        // anything - and repeating on it meant repeating while OUT OF CONTACT with the device.
        //
        // That is how `date` became `daaaaaaaaaaaaaaaaaaaaaaat`. Measured on this board, a 125 us sleep
        // can return 2 SECONDS late, so this task gets descheduled for far longer than the 10 ms poll
        // period. A release report landing in that gap is discarded by the transaction translator and
        // NEVER RESENT - a boot keyboard reports only on change - so nothing would ever have arrived to
        // disarm the repeat.
        //
        // Hence the second condition: a repeat is only credible if the PREVIOUS successful poll was
        // recent. If we have been away longer than that, a release may have come and gone unseen, so
        // the hold is unproven and the right move is to stop rather than to assume. "Wait on truth,
        // not on time" - and the truth here is a fresh answer from the device.
        // A NAK IS NOT PROOF THAT THE KEY IS STILL DOWN, and treating it as proof is what typed
        // `daaaaaaaaaaaaaaaaaaaaaaat`. Measured: 391 characters from auto-repeat against 101 from
        // real reports, so the runaway was this code, not the device retransmitting.
        //
        // The device NAKs when nothing changed - and it also NAKs when the split transaction failed.
        // Those are indistinguishable here, and on this board the second happens constantly: a
        // requested 125 us sleep returns after 54 ms on average, so the microframe schedule this poll
        // depends on is missed most of the time. When that happens the RELEASE report is never
        // fetched, and a repeat anchored on NAK alone runs until the next key is pressed.
        //
        // So anchor it on something the device must actually do: TALK TO US. Any decoded report -
        // press or release - proves the poll path is working right now. While reports keep arriving,
        // a NAK genuinely means "nothing changed" and repeating is correct. Once they stop, we have
        // lost the ability to observe a release, so the hold is unprovable and repeating is guessing.
        //
        // The cost is honest and bounded: holding a key for longer than this window stops repeating.
        // That is a small annoyance. Spraying 391 characters is a bug.
        // A WEDGED TT NEVER RECOVERS ON ITS OWN, so stop asking and clear it.
        //
        // Ordinary NYET means "the TT is not done yet, ask again" and resolves within a few
        // microframes. A run of them means its buffer is stuck holding a transaction we never
        // collected, and no number of further complete-splits will change that - measured here as
        // 1497 consecutive polls, all NYET, across a keyboard that was dead the whole time.
        //
        // The threshold is generous on purpose: at a 10 ms poll period this is ~1 s of solid NYET,
        // far beyond any legitimate busy TT, so a healthy device is never disturbed. And the recovery
        // is bounded - clear it, reset the run, and if it wedges again the next second clears it
        // again, which is a loud repeating log line rather than a silently dead keyboard.
        // EIGHT, deliberately eager - because THIS rung is cheap and it is what the operator feels.
        //
        // The two remedies have very different costs and deserve opposite postures, which the first
        // attempt got wrong by treating them the same:
        //   - a TT clear is ONE control transfer to the hub, a few milliseconds, and it cannot harm a
        //     healthy device (it flushes a buffer holding something nobody wants). Firing it early is
        //     what turns a wedge into a hiccup the user barely notices - measured on hardware as "a
        //     few ms, noticeable but recovered".
        //   - a port RE-ENUMERATION costs 0.31 s and resets the device. Firing that early made 394
        //     recovery cycles in one idle session, each reset stranding the transaction that caused
        //     the next - a loop the driver drove itself.
        // So: eager here, conservative there, and the expensive rung is bounded separately.
        //
        // The log for this is rate-limited rather than the action: an operator needs to know it is
        // happening and how often, not to receive a line every time.
        const NYET_WEDGED: u32 = 8;
        if state.nyet_run >= NYET_WEDGED {
            state.nyet_run = 0;
            let hub_port = (splt & 0x7F) as u8;
            let hub_addr = ((splt >> 7) & 0x7F) as u8;
            let hub = Target { addr: hub_addr, mps: 64, low_speed: false };
            // Say it the first time and then every 50th. Silencing it would hide a device degrading;
            // printing all 388 of them buried every other line in the console, which is its own kind
            // of blindness.
            state.clears = state.clears.wrapping_add(1);
            if state.clears == 1 || state.clears % 50 == 0 {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: keyboard TT wedged ({} consecutive NYET) - cleared the hub's TT buffer [{} so far]",
                    NYET_WEDGED, state.clears));
            }
            let _ = crate::hub::clear_tt_buffer(
                ctx, mmio, dma, &hub, t.addr, kbd.ep, 3 /* interrupt */, true /* IN */, hub_port);
            state.cleared_at = ctx.read_tsc();
            // The next poll starts a fresh start-split; the toggle is read back from hardware, so
            // there is no software state to reset here.
        }

        let now = ctx.read_tsc();
        if hcint & crate::regs::HCINT_NAK != 0 {
            let proven = state.last_data != 0
                && now.wrapping_sub(state.last_data) < state.repeat_window;
            if proven {
                let n = &mut state.emitted_repeat;
                state.repeat.poll(now, |ch| { *n = n.wrapping_add(1); ctx.console_push(ch) });
            } else if state.repeat.armed() {
                state.repeat.cancel();
            }
            state.last_ok = now;
        }
        return false; // NAK (idle), NYET, or a rescheduled attempt - all ordinary
    }
    // An all-zero report is the keyboard saying every key is RELEASED. It is a real report and must
    // still be decoded (that is how a key-up is seen), but it is worth separating in the count from
    // reports that carry a keypress, so "keys are arriving" and "keys are being released" do not look
    // the same from the log.
    let any = (0..8).any(|i| dma.read8(REPORT_OFF + i) != 0);
    if !any {
        state.pid = chan::pid_from_hctsiz(mmio, chan::CH_KBD);
        state.last_data = ctx.read_tsc(); // a release report is proof the path works
        let rel = [0u8; 8];
        godspeed_sdk::hid::decode_keyboard(
            &rel, &mut state.last, &mut state.repeat, &mut state.caps,
            ctx.read_tsc(), |ch| ctx.console_push(ch), |_| {});
        // (a release report emits nothing; counted for symmetry only if it ever does)
        return false;
    }
    // READ THE TOGGLE BACK FROM THE HARDWARE. Do not flip it in software.
    //
    // The DWC2 advances HCTSIZ.PID [30:29] itself, and the kernel driver reads it back for exactly
    // this reason. Flipping it in software makes the two disagree the moment they ever differ, and a
    // toggle mismatch does not error - the device RETRANSMITS its last report, which is delivered
    // again as a fresh keystroke.
    //
    // Hardware said so: one Enter press arrived six times, redrawing the prompt six times, on 59
    // reports. The transfers were right and the bookkeeping was mine.
    state.pid = chan::pid_from_hctsiz(mmio, chan::CH_KBD);

    let mut rep = [0u8; 8];
    for i in 0..8 {
        rep[i] = dma.read8(REPORT_OFF + i);
    }
    let mut state_emitted = 0u32;
    state.last_data = ctx.read_tsc(); // a data report is proof the path works
    godspeed_sdk::hid::decode_keyboard(
        &rep,
        &mut state.last,
        &mut state.repeat,
        &mut state.caps,
        ctx.read_tsc(),
        |ch| { state_emitted += 1; ctx.console_push(ch) },
        |code| ctx.log_fmt(format_args!("dwc2-svc: unmapped HID key usage {:#04x}", code)),
    );
    state.emitted_report = state.emitted_report.wrapping_add(state_emitted);
    true
}

/// Decode state carried between polls: the rollover buffer, auto-repeat timing, caps lock, and the
/// endpoint's data toggle.
pub struct KeyState {
    pub last: [u8; 6],
    pub repeat: godspeed_sdk::hid::KeyRepeat,
    pub caps: bool,
    pub pid: u32,
    /// Timestamp of the last poll the device actually ANSWERED (data or NAK). Auto-repeat is only
    /// credible while these keep arriving; a long gap means a release may have been lost unseen.
    pub last_ok: u64,
    /// How stale that answer may be before a held key is treated as unproven. Derived from the
    /// board's own timer rate, never a constant - a cycle count is not a duration.
    pub stale_after: u64,
    /// Characters emitted by AUTO-REPEAT, and characters emitted by decoding a real report. Two
    /// mechanisms can produce a stream of one character - a repeat that will not stop, or the device
    /// retransmitting its last report because the data toggle disagrees - and they are indistinguishable
    /// in the output. They are not indistinguishable in a counter, so count them separately and let the
    /// log say which one is running away instead of reasoning about which one probably is.
    pub emitted_repeat: u32,
    pub emitted_report: u32,
    /// When the endpoint last DELIVERED a report (press or release). Auto-repeat rides on this, not
    /// on NAKs: a NAK cannot tell "nothing changed" apart from "the transfer failed", and only one of
    /// those means the key is still down.
    pub last_data: u64,
    /// How long a delivered report vouches for the poll path. Derived from this board's timer rate.
    pub repeat_window: u64,
    /// WHAT EVERY POLL ACTUALLY RETURNED, since the last report line.
    ///
    /// The keyboard goes silent for tens of seconds and then comes back. Across that window the
    /// service is provably healthy - the pass count per interval is unchanged, and it is still
    /// spending its time in this poll - so it is not descheduled and not starved: the endpoint stops
    /// answering while we keep asking. WHY it stops is not in any log, because the poll's outcome was
    /// never recorded, only acted on. These count it: one bucket per way a transfer can end.
    pub n_data: u32,
    pub n_nak: u32,
    pub n_nyet: u32,
    pub n_stall: u32,
    pub n_xacterr: u32,
    /// The transfer never halted at all - `periodic_split_in` timed out and returned 0. Distinct from
    /// every bucket above, because those are the controller REPORTING something and this is silence.
    pub n_silent: u32,
    /// Anything with bits set that none of the above explain, plus the raw value of the last one, so
    /// an unexpected combination names itself instead of being filed under "other".
    pub n_other: u32,
    pub last_other: u32,
    /// Consecutive polls that ended in NYET with no data in between. A wedged transaction translator
    /// answers NYET to every complete-split forever, so this is the counter that distinguishes "the
    /// TT is briefly busy", which is ordinary, from "the TT will never answer again", which is not.
    pub nyet_run: u32,
    /// Consecutive transaction errors with no data in between. XACTERR is CRC, timeout, bit-stuff or
    /// a data-toggle mismatch; one is ordinary noise, an unbroken run means the endpoint is in a
    /// state no further transfers will leave.
    pub xacterr_run: u32,
    /// When the current error window opened. Errors spread wider than the window are noise; errors
    /// packed inside it with no delivered report in between are a broken endpoint.
    pub xacterr_since: u64,
    /// When the TT buffer was last cleared. A clear that does not restore data tells us the endpoint
    /// itself is in trouble, and waiting out the full cold-start error budget after that is three
    /// seconds spent proving something already known.
    pub cleared_at: u64,
    /// How many TT clears this binding has needed. Rate-limits the log without muting the remedy.
    pub clears: u32,
}

impl KeyState {
    pub fn new(ctx: &ServiceContext) -> Self {
        Self {
            last: [0u8; 6],
            // Auto-repeat delays calibrated from THIS machine's timer rate, not assumed - the same
            // assumption cost the Wyse a keypress that repeated into `qqqqq`.
            repeat: godspeed_sdk::hid::KeyRepeat::new_calibrated(ctx.tsc_ticks_per_10ms()),
            caps: false,
            // An interrupt endpoint starts at DATA0 after configuration.
            pid: chan::PID_DATA0,
            last_ok: 0,
            emitted_repeat: 0,
            emitted_report: 0,
            last_data: 0,
            n_data: 0, n_nak: 0, n_nyet: 0, n_stall: 0, n_xacterr: 0, n_silent: 0,
            n_other: 0, last_other: 0, nyet_run: 0, xacterr_run: 0, xacterr_since: 0, cleared_at: 0, clears: 0,
            // ~1.5 s. Long enough that a deliberate hold keeps repeating through the initial 600 ms
            // delay and well beyond, short enough that a broken poll path stops within a couple of
            // characters instead of running to the next keypress.
            repeat_window: (ctx.tsc_ticks_per_10ms() * 150).max(1),
            // ~150 ms: comfortably more than the 10 ms poll period (so ordinary jitter and a busy
            // core do not cancel a legitimate hold) and far less than the 2 s deschedule that loses
            // a release report.
            stale_after: (ctx.tsc_ticks_per_10ms() * 15).max(1),
        }
    }
}
