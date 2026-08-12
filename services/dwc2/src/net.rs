//! USB ethernet (SMSC LAN9514) - find its bulk endpoints and configure it.
//!
//! Slice 4, first part (`docs/arm32-usb-userspace.md`). Same shape as the keyboard and the disk: the
//! descriptor walk is control transfers, which are proven, so "did we find it" fails separately from
//! "can we talk to it".
//!
//! The NIC is `0424:ec00` on port 1 - HIGH speed and directly attached, so no splits. It reports
//! device class `0xff` (vendor-specific), which is why the endpoint walk does NOT filter by class:
//! there is no standard class to match, only bulk endpoints to find.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

const DESC_CONFIG: u8 = 0x02;
const DESC_ENDPOINT: u8 = 0x05;
const EP_TYPE_BULK: u8 = 0x02;

/// A bound USB ethernet device.
pub struct Nic {
    pub ep_in: u8,
    pub ep_out: u8,
    pub mps: u16,
    /// The station MAC, read from the device's serial-number STRING descriptor.
    ///
    /// Not a register read: the LAN9514 publishes its MAC as twelve ASCII hex characters in a UTF-16
    /// string descriptor, which is why the parse below steps FOUR bytes per output byte - two UTF-16
    /// code units, each two bytes, per pair of hex digits.
    pub mac: [u8; 6],
    /// Endpoint data toggles, per DIRECTION, for the device's lifetime - the level USB defines them
    /// at, learned three times over on the disk path.
    pub pid_in: u32,
    pub pid_out: u32,
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

/// Read the station MAC from string descriptor `idx`, as twelve ASCII hex characters.
fn read_mac(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, idx: u8) -> [u8; 6] {
    let mut mac = [0u8; 6];
    if idx == 0 {
        return mac;
    }
    // Length first: a string descriptor's length is its first byte, and asking for a fixed guess
    // either truncates it or overruns the device's answer - the same rule the config descriptor needs.
    let mut sd = [0u8; 40];
    let head = [0x80, 0x06, idx, 0x03, 0x09, 0x04, 2, 0]; // wIndex = langid 0x0409 (en-US)
    if !chan::control(ctx, mmio, dma, t, &head, &mut sd, true, 2) {
        return mac;
    }
    let len = (sd[0] as usize).min(sd.len());
    if len < 26 {
        return mac; // too short to hold twelve hex characters as UTF-16
    }
    let full = [0x80, 0x06, idx, 0x03, 0x09, 0x04, len as u8, 0];
    if !chan::control(ctx, mmio, dma, t, &full, &mut sd, true, len) {
        return mac;
    }
    for b in 0..6 {
        mac[b] = (hex_val(sd[2 + b * 4]) << 4) | hex_val(sd[2 + b * 4 + 2]);
    }
    mac
}

/// Find the bulk IN/OUT pair. No class filter: this device is vendor-specific.
fn find_bulk(buf: &[u8], total: usize) -> Option<(u8, u8, u16)> {
    let mut i = 0usize;
    let (mut ep_in, mut ep_out, mut mps) = (0u8, 0u8, 64u16);
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            break; // malformed descriptor ends the walk rather than freezing the cursor
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7 && buf[i + 3] & 0x03 == EP_TYPE_BULK {
            let raw = u16::from_le_bytes([buf[i + 4], buf[i + 5]]);
            mps = match raw & 0x07FF {
                0 => 64,
                v => v,
            };
            if buf[i + 2] & 0x80 != 0 {
                ep_in = buf[i + 2] & 0x0F;
            } else {
                ep_out = buf[i + 2] & 0x0F;
            }
        }
        i += len;
    }
    if ep_in != 0 && ep_out != 0 {
        Some((ep_in, ep_out, mps))
    } else {
        None
    }
}

/// Configure the NIC and return its bulk endpoints.
pub fn bind(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> Option<Nic> {
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control(ctx, mmio, dma, t, &get9, &mut head, true, 9) {
        return None;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let cfg_val = head[5];

    let want = total.min(chan::DATA_LEN);
    let mut full = [0u8; chan::DATA_LEN];
    let getall = [
        0x80, 0x06, 0, DESC_CONFIG, 0, 0,
        (want & 0xFF) as u8, ((want >> 8) & 0xFF) as u8,
    ];
    if !chan::control(ctx, mmio, dma, t, &getall, &mut full, true, want) {
        return None;
    }

    let (ep_in, ep_out, mps) = find_bulk(&full, want)?;

    // RETRY SET_CONFIGURATION, with a settle between attempts.
    //
    // This is not defensive coding - it is a documented property of this device, and the kernel
    // driver's comment records it from hardware: the LAN9514 ACCEPTS the request (the SETUP is ACKed)
    // and then XactErrs the zero-length status stage for tens of milliseconds while it brings up its
    // internal ethernet state. SET_ADDRESS, the same shape of no-data control-OUT, succeeded moments
    // earlier - so the device CAN do the transfer, it just needs to settle after accepting the config.
    //
    // It also NAKs that status stage for longer than an ordinary control budget allows, which is the
    // other half of the same story. This driver's control path treats a NAK as a failed stage, so the
    // retry here is what covers both: eight attempts, each a fresh whole request, exactly as usbcore
    // retries control transfers. HW-blind and tuned by observation in the kernel driver; carried
    // across rather than re-derived, because it passes in QEMU either way.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    let mut set_ok = false;
    for attempt in 0..8u32 {
        if chan::control(ctx, mmio, dma, t, &setcfg, &mut none, false, 0) {
            set_ok = true;
            if attempt > 0 {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: NIC SET_CONFIGURATION took {} attempts (the LAN9514 settles slowly)",
                    attempt + 1));
            }
            break;
        }
        ctx.sleep(ctx.duration_cycles(50));
    }
    if !set_ok {
        ctx.log("dwc2-svc: NIC SET_CONFIGURATION FAILED after 8 attempts");
        return None;
    }

    // iSerialNumber is byte 16 of the device descriptor, and on this part it carries the MAC.
    let mut dev = [0u8; 18];
    let getdev = [0x80, 0x06, 0, 0x01, 0, 0, 18, 0];
    let mac = if chan::control(ctx, mmio, dma, t, &getdev, &mut dev, true, 18) {
        read_mac(ctx, mmio, dma, t, dev[16])
    } else {
        [0u8; 6]
    };

    ctx.log_fmt(format_args!(
        "dwc2-svc: USB ETHERNET bound - bulk IN {} OUT {} mps {} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        ep_in, ep_out, mps, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));
    Some(Nic { ep_in, ep_out, mps, mac, pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0 })
}

/// Where frames live in the DMA arena, clear of everything else.
///
/// DERIVED, not asserted in prose. The first values here were `0x1000`/`0x2000` and the const
/// assertion below rejected them at BUILD time: `msc`'s data buffer spans `0x400..0x1400`, so the TX
/// frame would have been laid inside it. That is the identical overlap that cost a boot in slice 2 -
/// where the comment claimed the buffer was clear of the scratch and nobody checked the arithmetic.
/// The assertion is why this one cost a compile instead.
pub const TX_OFF: usize = 0x2000;
pub const RX_OFF: usize = 0x3000;
pub const FRAME_MAX: usize = 1514;
/// One IN transfer can carry SEVERAL frames, so the receive burst is larger than one frame.
pub const RX_BURST: usize = 2048;
const _: () = assert!(TX_OFF >= crate::msc::DATA_OFF + crate::msc::DATA_MAX);
const _: () = assert!(RX_OFF >= TX_OFF + FRAME_MAX + 8);

/// Send one ethernet frame.
///
/// **The LAN9514 is NOT plain CDC-ECM.** It wants an 8-byte TX command in front of the frame, and a
/// driver that sends the raw frame instead is not rejected - the device silently drops it, which on a
/// NIC is the worst failure shape there is.
pub fn tx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic, frame: &[u8],
) -> bool {
    let n = frame.len().min(FRAME_MAX);
    // TX_CMD_A = len | FIRST_SEG | LAST_SEG, TX_CMD_B = len. Both little-endian.
    let a = (n as u32) | 0x0000_2000 | 0x0000_1000;
    for (i, b) in a.to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + i, *b);
    }
    for (i, b) in (n as u32).to_le_bytes().iter().enumerate() {
        dma.write8(TX_OFF + 4 + i, *b);
    }
    for i in 0..n {
        dma.write8(TX_OFF + 8 + i, frame[i]);
    }
    // No ZLP needed: the smsc95xx carries an explicit length in its TX command, so it does not rely on
    // a short packet to find the frame boundary the way CDC-ECM does.
    bulk(ctx, mmio, t, nic.mps, false, nic.ep_out, dma.phys_at(TX_OFF) as u32,
         (n + 8) as u32, 2_000, &mut nic.pid_out).is_some()
}

/// Receive one burst and hand each complete frame to `deliver`.
///
/// Returns the number of frames delivered. Zero is the ordinary answer on a quiet network and is not
/// a failure.
pub fn rx(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic,
    mut deliver: impl FnMut(&[u8]),
) -> u32 {
    let got = match bulk(ctx, mmio, t, nic.mps, true, nic.ep_in, dma.phys_at(RX_OFF) as u32,
                         RX_BURST as u32, 50, &mut nic.pid_in) {
        Some(n) => n as usize,
        None => return 0, // NAK on a quiet network is the normal case, not an error
    };

    // A burst is [4-byte RX status][frame incl FCS][DWORD pad], REPEATED - one transfer can carry
    // several frames, so this is a parse rather than a copy.
    let mut pos = 0usize;
    let mut frames = 0u32;
    let mut buf = [0u8; FRAME_MAX];
    while pos + 4 <= got {
        let status = u32::from_le_bytes([
            dma.read8(RX_OFF + pos), dma.read8(RX_OFF + pos + 1),
            dma.read8(RX_OFF + pos + 2), dma.read8(RX_OFF + pos + 3),
        ]);
        pos += 4;
        let flen = ((status >> 16) & 0x3FFF) as usize; // length INCLUDING the 4-byte FCS

        // The floor is a full ethernet header PLUS the FCS, not merely 4. A device-supplied `flen == 4`
        // would deliver a ZERO-length frame, and zero is the "nothing received" sentinel everywhere
        // above - so one malformed length would look like an empty network rather than a bad frame.
        if flen < 4 + 14 || flen > FRAME_MAX + 4 || pos + flen > got {
            break; // invalid, or a frame split across the burst boundary - give up on this burst
        }
        let n = flen - 4; // strip the FCS; the stack does not want it
        for i in 0..n.min(FRAME_MAX) {
            buf[i] = dma.read8(RX_OFF + pos + i);
        }
        deliver(&buf[..n.min(FRAME_MAX)]);
        frames += 1;
        pos += flen;
        pos += (4 - (flen % 4)) % 4; // each frame is followed by DWORD padding
    }
    frames
}

/// One bulk transfer for the NIC. Shares the disk's shape but its OWN channel, so a frame in flight
/// cannot leave state on the channel a block transfer inherits - the rule the kernel driver learned
/// by corrupting block transfers with an abandoned interrupt split.
#[allow(clippy::too_many_arguments)]
fn bulk(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target, mps: u16,
    dir_in: bool, ep: u8, buf_phys: u32, len: u32, budget_ms: u64, pid: &mut u32,
) -> Option<u32> {
    let bt = Target { addr: t.addr, mps, low_speed: false };
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(budget_ms));
    loop {
        chan::program(mmio, &bt, CH_NET, dir_in, *pid, len, buf_phys, ep as u32, 2, 0);
        match chan::wait_halt(ctx, mmio, CH_NET, 50) {
            Some(hcint) if hcint & crate::regs::HCINT_XFERCOMPL != 0 => {
                let left = mmio.read32(chan::hctsiz_at(CH_NET)) & 0x7_FFFF;
                *pid = chan::pid_from_hctsiz(mmio, CH_NET);
                return Some(len.saturating_sub(left));
            }
            Some(hcint) if hcint & crate::regs::HCINT_STALL != 0 => return None,
            _ => {}
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            return None;
        }
    }
}

/// The NIC's own channel. Separate from CH_BULK (disk/control) and CH_KBD for the same reason those
/// are separate from each other: an abandoned transfer must not leave its state on a channel another
/// stream inherits.
pub const CH_NET: u32 = 2;

// --- The frame IPC protocol -------------------------------------------------------------------
//
// **These opcodes must not collide with the BLOCK ones.** `dwc2` serves `block-driver` and
// `nic-driver` on the SAME endpoint, and a shared opcode space with two independent protocols in it
// is a bug waiting for the first person to add an op to either. Block uses 1..5; net starts at 0x10.
pub const OP_NET_INFO: u8 = 0x10;
pub const OP_NET_TX: u8 = 0x11;
pub const OP_NET_RX: u8 = 0x12;

/// Serve one frame request. Returns false if the message was not one.
pub fn serve(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, nic: &mut Nic,
    msg: &godspeed_sdk::Message, reply: godspeed_sdk::CapHandle,
) -> bool {
    let p = msg.payload_bytes();
    if p.is_empty() {
        return false;
    }
    let mut out = [0u8; FRAME_MAX + 4];
    let n = match p[0] {
        OP_NET_INFO => {
            // [ok, mac(6), link]. The link bit is reported as UP: this driver does not yet read the
            // PHY, and saying so here rather than inventing a "down" keeps net-stack from concluding
            // the cable is out. Reading it properly is the remaining piece of this slice.
            out[0] = 1;
            out[1..7].copy_from_slice(&nic.mac);
            out[7] = 1;
            8
        }
        OP_NET_TX if p.len() > 1 => {
            out[0] = u8::from(tx(ctx, mmio, dma, t, nic, &p[1..]));
            1
        }
        OP_NET_RX => {
            // [n_lo, n_hi, frame...]. Zero length is "nothing received", which on a quiet network is
            // the ordinary answer and not a failure.
            let mut got = 0usize;
            rx(ctx, mmio, dma, t, nic, |f| {
                // ONE frame per reply: the protocol has no framing of its own, so a second frame in
                // the same message would be indistinguishable from the first one's payload. The rest
                // of the burst is dropped rather than mis-delivered, and the client polls again.
                if got == 0 {
                    let take = f.len().min(FRAME_MAX);
                    out[2..2 + take].copy_from_slice(&f[..take]);
                    got = take;
                }
            });
            out[0] = (got & 0xFF) as u8;
            out[1] = ((got >> 8) & 0xFF) as u8;
            got + 2
        }
        _ => return false, // not a net op - the block server gets a look at it
    };
    let _ = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n]));
    ctx.remove_cap(reply);
    true
}
