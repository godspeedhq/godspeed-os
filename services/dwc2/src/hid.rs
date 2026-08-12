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
    // The 9-byte header first, for wTotalLength - the full descriptor's length is not knowable in
    // advance and asking for a fixed guess either truncates it or overruns the device's answer.
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control_split(ctx, mmio, dma, t, &get9, &mut head, true, 9, splt) {
        ctx.log("dwc2-svc: config descriptor header read FAILED");
        return None;
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
        ctx.log("dwc2-svc: full config descriptor read FAILED");
        return None;
    }

    let (iface, kbd) = match find_boot_keyboard(&full, want) {
        Some(x) => x,
        None => return None, // not a keyboard: a normal outcome, not a failure to report
    };

    // SET_CONFIGURATION, for the same reason the hub needed it: class and interface requests are only
    // legal in the CONFIGURED state, and a device that skips this accepts them and answers nothing.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control_split(ctx, mmio, dma, t, &setcfg, &mut none, false, 0, splt) {
        ctx.log_fmt(format_args!("dwc2-svc: SET_CONFIGURATION {} FAILED", cfg_val));
        return None;
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
    Some(kbd)
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

    // A NAK is not a report, even when XFERCOMPL rides along with it.
    //
    // HCINT can latch BOTH, and testing only XFERCOMPL counted every idle poll as a keystroke: the
    // counter climbed at ~2/second with nothing typed, which made it useless for the one question it
    // exists to answer - are keystrokes reaching us, or reaching us and failing to decode?
    if hcint & crate::regs::HCINT_XFERCOMPL == 0 || hcint & crate::regs::HCINT_NAK != 0 {
        return false; // NAK (idle), NYET, or a rescheduled attempt - all ordinary
    }
    // An all-zero report is the keyboard saying every key is RELEASED. It is a real report and must
    // still be decoded (that is how a key-up is seen), but it is worth separating in the count from
    // reports that carry a keypress, so "keys are arriving" and "keys are being released" do not look
    // the same from the log.
    let any = (0..8).any(|i| dma.read8(REPORT_OFF + i) != 0);
    if !any {
        state.pid = chan::pid_from_hctsiz(mmio, chan::CH_KBD);
        let mut rel = [0u8; 8];
        godspeed_sdk::hid::decode_keyboard(
            &rel, &mut state.last, &mut state.repeat, &mut state.caps,
            ctx.read_tsc(), |ch| ctx.console_push(ch), |_| {});
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
    godspeed_sdk::hid::decode_keyboard(
        &rep,
        &mut state.last,
        &mut state.repeat,
        &mut state.caps,
        ctx.read_tsc(),
        |ch| ctx.console_push(ch),
        |code| ctx.log_fmt(format_args!("dwc2-svc: unmapped HID key usage {:#04x}", code)),
    );
    true
}

/// Decode state carried between polls: the rollover buffer, auto-repeat timing, caps lock, and the
/// endpoint's data toggle.
pub struct KeyState {
    pub last: [u8; 6],
    pub repeat: godspeed_sdk::hid::KeyRepeat,
    pub caps: bool,
    pub pid: u32,
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
        }
    }
}
