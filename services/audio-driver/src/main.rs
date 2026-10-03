// SPDX-License-Identifier: GPL-2.0-only
// No `unsafe` in this crate, enforced by the compiler (CLAUDE.md 18.2): registers and DMA memory are
// reached through the SDK's safe `Mmio` and `Dma`. `deny` rather than `forbid` for one reason - the
// exported `service_main` symbol needs `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is
// itself covered by this lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed there.
#![deny(unsafe_code)]
#![no_std]
#![no_main]
//! `audio-driver` - an Intel High Definition Audio controller (docs/audio.md).
//!
//! **What it does, step by step** (each step's record is in docs/audio.md):
//!
//! - **A1** resets the controller, finds the codecs on its link, walks each one's widget graph and reports
//!   an output path - a converter (DAC) through any mixers and selectors to an output pin. It talks to the
//!   codec through the Immediate Command registers: plain MMIO, no DMA.
//! - **A2** moves codec commands onto the CORB and RIRB, the command rings in memory the spec requires
//!   (the Immediate Command registers are OPTIONAL, HDA 1.0a 3.4, and unknown on the T630's controller).
//! - **A3** configures the output path and plays a tone the driver generates itself: one output stream,
//!   a buffer descriptor list, a cyclic ring of sound in the DMA arena, refilled by polling the
//!   stream's position. Played once at start, as the step's self-test, until A4 gives it a request.
//!
//! **DMA is used only on the codec A2 and A3 were verified on - QEMU's (`1af4`).** On any other codec the
//! driver stops after A1's survey and says so: on the T630 the class lookup hands it the HDMI
//! controller, not the analog one, and the AMD controller needs its snoop bit set (docs/audio.md,
//! "Found while preparing"). Both are A6's. On this driver's death the kernel clears its device's bus
//! mastering, keyed on the device it was given (`kernel/src/task/scheduler.rs`).
//!
//! **Every wait is `gs::driver`'s** - this driver is the library's independent test (docs/driver-library.md,
//! "Wi-Fi discovers; audio tests"). A wait that has to be bent to fit is a finding about the library, and
//! goes in docs/audio.md.

use godspeed as gs;
use godspeed::driver::delay;
use godspeed::driver::irq::{Irq, Woke};
use godspeed::driver::wait::{self, Budget};
use godspeed_audio::wire;
use godspeed_sdk::mmio::Mmio;
use godspeed_sdk::{Dma, Message, ServiceContext};

// ---- Controller registers (HDA 1.0a section 3.3) ----------------------------------------------------
const GCAP: usize = 0x00; // u16: output/input/bidirectional stream counts, 64-bit support
const VMIN: usize = 0x02; // u8
const VMAJ: usize = 0x03; // u8
const GCTL: usize = 0x08; // u32: bit 0 CRST - 0 holds the controller and link in reset
const STATESTS: usize = 0x0E; // u16: bit n set = a codec answered at address n after reset
const INTCTL: usize = 0x20; // u32: 31 GIE, 30 CIE, and one SIE bit per stream descriptor (by index)
const CORBLBASE: usize = 0x40;
const CORBUBASE: usize = 0x44;
const CORBWP: usize = 0x48; // u16
const CORBRP: usize = 0x4A; // u16: bit 15 resets the read pointer
const CORBCTL: usize = 0x4C; // u8: bit 1 runs the CORB DMA engine
const CORBSIZE: usize = 0x4E; // u8: bits 1:0 the size chosen, 7:4 the sizes supported
const RIRBLBASE: usize = 0x50;
const RIRBUBASE: usize = 0x54;
const RIRBWP: usize = 0x58; // u16: bit 15 resets the write pointer
const RINTCNT: usize = 0x5A; // u16
const RIRBCTL: usize = 0x5C; // u8: bit 1 runs the RIRB DMA engine
const RIRBSTS: usize = 0x5D; // u8: write-1-to-clear
const RIRBSIZE: usize = 0x5E; // u8
const ICOI: usize = 0x60; // u32: Immediate Command Output - the verb to send
const ICII: usize = 0x64; // u32: Immediate Command Input - the response
const ICIS: usize = 0x68; // u16: Immediate Command Status
/// Stream descriptors: input streams first, then output, 0x20 bytes each from 0x80.
const SD_BASE: usize = 0x80;
const SD_STRIDE: usize = 0x20;
// Offsets within a stream descriptor.
const SD_CTL: usize = 0x00; // 24 bits: 0 SRST, 1 RUN, 23:20 stream tag; byte 3 is SDnSTS
const SD_LPIB: usize = 0x04; // u32: link position in the cyclic buffer, in bytes
const SD_CBL: usize = 0x08; // u32: cyclic buffer length, in bytes
const SD_LVI: usize = 0x0C; // u16: last valid index of the BDL
const SD_FMT: usize = 0x12; // u16
const SD_BDPL: usize = 0x18;
const SD_BDPU: usize = 0x1C;

const GCTL_CRST: u32 = 1 << 0;
const ICIS_ICB: u16 = 1 << 0; // busy: a verb is in flight
const ICIS_IRV: u16 = 1 << 1; // a response is waiting in ICII (write 1 to clear)
const RING_RUN: u8 = 1 << 1; // CORBCTL.CORBRUN and RIRBCTL.RIRBDMAEN are both bit 1
/// RIRBCTL.RINTCTL: raise the response status every RINTCNT responses. NOT optional in practice: the
/// controller stops taking commands from the CORB once RINTCNT responses are outstanding, until software
/// clears that status - and the status is only ever set with this bit on. A2 shipped without it and
/// QEMU answered one command and then nothing. Linux sets it (with RINTCNT 1). The CPU interrupt is a
/// separate enable (INTCTL), left off: the driver still polls.
const RIRB_RINTCTL: u8 = 1 << 0;
const CORBRP_RST: u16 = 1 << 15;
const RIRBWP_RST: u16 = 1 << 15;
const RING_SIZE_256: u8 = 0x02;
const RIRB_UNSOLICITED: u32 = 1 << 4; // in the response's extended word
const SD_SRST: u32 = 1 << 0;
const SD_RUN: u32 = 1 << 1;
const SD_STS_SHIFT: u32 = 24; // SDnSTS is byte 3 of the 32-bit word at SD_CTL
const SD_STS: usize = 0x03; // u8, write-1-to-clear: 2 BCIS (a period done), 3 FIFOE, 4 DESE
const SD_STS_ALL: u8 = 0x1C;
const SD_IOCE: u32 = 1 << 2; // interrupt when a BDL entry with IOC set has been played
const BDL_IOC: u32 = 1 << 0; // in a BDL entry's flags word: interrupt when this period is done
const INTCTL_GIE: u32 = 1 << 31;

// ---- Verbs and parameters (HDA 1.0a section 7.3) ----------------------------------------------------
const GET_PARAMETER: u32 = 0xF00;
const GET_CONNECTION_LIST: u32 = 0xF02;
const GET_POWER_STATE: u32 = 0xF05;
const GET_CONFIG_DEFAULT: u32 = 0xF1C;
const SET_CONNECT_SEL: u32 = 0x701;
const SET_POWER_STATE: u32 = 0x705;
const SET_CHANNEL_STREAMID: u32 = 0x706;
const SET_PIN_WIDGET_CONTROL: u32 = 0x707;
const SET_EAPD_BTLENABLE: u32 = 0x70C;
// Four-bit verbs, which carry a sixteen-bit payload.
const SET_CONVERTER_FORMAT: u32 = 0x2;
const SET_AMP_GAIN_MUTE: u32 = 0x3;
const GET_AMP_GAIN_MUTE: u32 = 0xB;

const PARAM_VENDOR_ID: u32 = 0x00;
const PARAM_SUB_NODE_COUNT: u32 = 0x04;
const PARAM_FUNCTION_GROUP_TYPE: u32 = 0x05;
const PARAM_AUDIO_WIDGET_CAP: u32 = 0x09;
const PARAM_PIN_CAP: u32 = 0x0C;
const PARAM_CONN_LIST_LEN: u32 = 0x0E;
const PARAM_OUT_AMP_CAP: u32 = 0x12;
const PARAM_SUPPORTED_POWER_STATES: u32 = 0x0F;

const FG_AUDIO: u32 = 0x01;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_POWER: u32 = 1 << 10;
const PINCAP_EAPD: u32 = 1 << 16;
const PIN_OUT_EN: u32 = 0x40;
const EAPD_ON: u32 = 0x02;
const AMP_OUT_LR_UNMUTED: u32 = 0xB000; // output amp, left and right, mute clear
const AMP_MUTE: u32 = 1 << 7; // in a gain/mute word, set or read
// GET_AMP_GAIN_MUTE's payload: which amplifier (bit 15 output) and which side (bit 13 left, clear right).
const AMP_GET_OUTPUT: u32 = 1 << 15;
const AMP_GET_LEFT: u32 = 1 << 13;
const AMP_GET_RIGHT: u32 = 0;
/// Power state D3, the lowest a codec widget offers short of losing power.
const D3: u32 = 3;

// Audio widget types: bits 23:20 of the Audio Widget Capabilities parameter.
const W_OUTPUT: u32 = 0x0;
const W_MIXER: u32 = 0x2;
const W_SELECTOR: u32 = 0x3;
const W_PIN: u32 = 0x4;

/// QEMU's `hda-output` codec - the one A2 and A3 were verified on. See the module documentation.
const VENDOR_QEMU: u32 = 0x1af4;

// ---- Timing ------------------------------------------------------------------------------------------
/// How long the controller has to show a change of GCTL.CRST. The spec gives no figure - "software
/// must read a 0", then "a 1" - and Linux allows 100 ms for each, polling.
const CRST_WAIT: Budget = Budget::ms(100);
/// How long the controller is held in reset. The spec's minimum is 100 us (2400 BCLKs, 5.5.1.2);
/// Linux holds 500-1000 us. A HOLD: nothing reports it has been long enough.
const RESET_HOLD: Budget = Budget::ms(1);
/// How long codecs get to announce themselves after the link leaves reset. The spec's minimum is
/// 521 us - 25 frames (4.3) - and a codec that is not there never sets its bit, so this is a HOLD and
/// not a wait: there is no condition that says "every codec that exists has answered". Linux holds
/// 1000-1200 us.
const CODEC_ENUM_HOLD: Budget = Budget::ms(2);
/// How long an immediate command may take, both to be accepted (ICB clear) and to be answered (IRV
/// set). A codec answers within a frame or two (21 us each); Linux allows 50 us. This allows 1 ms.
const VERB_WAIT: Budget = Budget::ms(1);
/// How long a ring DMA engine, or a command-ring pointer reset, has to show the change asked for.
/// Linux polls the CORB read-pointer reset for 1000 x 1 us; this allows 10 ms.
const RING_WAIT: Budget = Budget::ms(10);
/// How long a response may take to arrive in the RIRB. Linux allows a second.
const RESPONSE_WAIT: Budget = Budget::ms(1000);
/// How long a stream reset or a stream's RUN bit has to show the change asked for. The spec bounds RUN
/// clearing at 40 us; Linux polls SRST for 300 us.
const STREAM_WAIT: Budget = Budget::ms(10);
/// How long a widget may take to reach power state D0. The spec bounds leaving D3 at 10 ms (200 ms
/// from D3cold). Paced, because each look is a verb.
const POWER_WAIT: Budget = Budget::ms(200);
const POWER_PACE: Budget = Budget::ms(1);

/// Widgets this driver will look at in one function group. The ALC255 has fewer than 0x30; QEMU's
/// `hda-output` has two. A larger graph is reported and truncated rather than walked unbounded.
const MAX_WIDGETS: usize = 64;
/// How deep the output-path search goes from a pin towards a converter: pin, mixer or selector, maybe
/// another, then the DAC. Real codecs are two or three hops.
const MAX_PATH: usize = 4;

// ---- The DMA arena (offsets from its base; every structure 128-byte aligned, HDA 1.0a 3.3) ----------
//
// Hand-written, like the arena tables in `xhci`, `ehci`, `nic-driver`, `dwmac` and `genet`: that makes it
// the SIXTH copy of something already found repeated, and a library candidate (docs/audio.md, "The plan").
// Recorded here so the copy is a finding, not an oversight.
const CORB_OFF: usize = 0x0000; // 256 entries x 4 bytes
const RIRB_OFF: usize = 0x0400; // 256 entries x 8 bytes
const BDL_OFF: usize = 0x0C00; // BDL_ENTRIES x 16 bytes
const PCM_OFF: usize = 0x1000; // the cyclic buffer of sound
const PCM_LEN: usize = 64 * 1024;
const BDL_ENTRIES: usize = 4; // the ring as four 16 KiB periods
const ARENA_NEEDED: usize = PCM_OFF + PCM_LEN;

// ---- The sound -----------------------------------------------------------------------------------------
const RATE: u32 = 48_000;
/// SDnFMT and the converter format: 48 kHz base, x1, /1, 16 bits, 2 channels (HDA 1.0a 3.7.1).
const FMT_48K_16_STEREO: u32 = 0x0011;
const FRAME_BYTES: usize = 4; // two 16-bit channels
/// The stream tag the converter listens for. 1..=15; tag 0 is reserved. NOT the descriptor's index.
const STREAM_TAG: u32 = 1;
/// The volume at start, until `/audio.settings` keeps one across a restart (A4c).
const DEFAULT_VOLUME: u8 = 50;
/// With no interrupt routed, how often the playing stream's position is read and the ring refilled.
/// The ring holds about 341 ms, so this leaves over thirty refills of margin.
const REFILL_PACE: Budget = Budget::ms(10);
/// With the interrupt, the WATCHDOG: how long to wait for one before looking anyway. A period is 16 KiB,
/// about 85 ms at 48 kHz stereo, so a healthy stream interrupts well inside this and the watchdog never
/// fires; a lost interrupt costs one late refill, which the ring's 341 ms absorbs.
const REFILL_WATCHDOG: Budget = Budget::ms(150);
/// How long the idle loop waits before going round again. Nothing happens on its expiry; it bounds the
/// wait because `irq::Irq::wait` always has one, and an hour is no load at all.
const SERVE_WAIT: Budget = Budget::ms(3_600_000);

/// The controller, and how codec commands reach it: the Immediate Command registers until the rings are
/// set up, the CORB and RIRB after.
struct Hda<'a> {
    ctx: &'a ServiceContext,
    m: &'a Mmio,
    dma: Option<&'a Dma>,
    corb_wp: u16,
    rirb_rp: u16,
}

impl<'a> Hda<'a> {
    /// One codec command, by whichever path is up. `None` names the failure in the log.
    fn send(&mut self, word: u32) -> Option<u32> {
        match self.dma {
            Some(d) => self.send_ring(d, word),
            None => self.send_immediate(word),
        }
    }

    /// A twelve-bit verb with an eight-bit payload.
    fn verb(&mut self, cad: u32, nid: u32, cmd: u32, payload: u32) -> Option<u32> {
        self.send((cad & 0xF) << 28 | (nid & 0x7F) << 20 | (cmd & 0xFFF) << 8 | (payload & 0xFF))
    }

    /// A four-bit verb with a sixteen-bit payload.
    fn verb16(&mut self, cad: u32, nid: u32, cmd: u32, payload: u32) -> Option<u32> {
        self.send((cad & 0xF) << 28 | (nid & 0x7F) << 20 | (cmd & 0xF) << 16 | (payload & 0xFFFF))
    }

    fn param(&mut self, cad: u32, nid: u32, p: u32) -> Option<u32> {
        self.verb(cad, nid, GET_PARAMETER, p)
    }

    fn send_immediate(&mut self, word: u32) -> Option<u32> {
        let (ctx, m) = (self.ctx, self.m);
        if wait::until(ctx, VERB_WAIT, || m.read16(ICIS) & ICIS_ICB == 0).is_err() {
            ctx.log_fmt(format_args!(
                "audio-driver: the immediate command interface stayed busy (ICIS={:#06x}) - verb {:#010x} not sent",
                m.read16(ICIS), word));
            return None;
        }
        m.write16(ICIS, ICIS_IRV); // clear a stale response before asking for a new one
        m.write32(ICOI, word);
        m.write16(ICIS, ICIS_ICB);
        if wait::until(ctx, VERB_WAIT, || m.read16(ICIS) & ICIS_IRV != 0).is_err() {
            ctx.log_fmt(format_args!(
                "audio-driver: no answer to verb {:#010x} by immediate command (ICIS={:#06x})",
                word, m.read16(ICIS)));
            return None;
        }
        let r = m.read32(ICII);
        m.write16(ICIS, ICIS_IRV);
        Some(r)
    }

    /// Put the verb on the CORB after the last one written, and take its answer off the RIRB. An
    /// UNSOLICITED response (a codec reporting a jack, say) is passed over: it answers nobody's verb.
    fn send_ring(&mut self, d: &Dma, word: u32) -> Option<u32> {
        let (ctx, m) = (self.ctx, self.m);
        let wp = (self.corb_wp + 1) % 256;
        d.write32(CORB_OFF + wp as usize * 4, word);
        self.corb_wp = wp;
        m.write16(CORBWP, wp);
        let mut deadline = wait::Deadline::start(ctx, RESPONSE_WAIT);
        loop {
            if m.read16(RIRBWP) & 0xFF != self.rirb_rp {
                self.rirb_rp = (self.rirb_rp + 1) % 256;
                let at = RIRB_OFF + self.rirb_rp as usize * 8;
                let (resp, ext) = (d.read32(at), d.read32(at + 4));
                m.write8(RIRBSTS, 0x05); // response interrupt and overrun flags, write-1-to-clear
                if ext & RIRB_UNSOLICITED == 0 {
                    return Some(resp);
                }
                continue;
            }
            if deadline.expired() {
                ctx.log_fmt(format_args!(
                    "audio-driver: no answer to verb {:#010x} on the RIRB (CORBRP={:#06x} RIRBWP={:#06x})",
                    word, m.read16(CORBRP), m.read16(RIRBWP)));
                return None;
            }
            core::hint::spin_loop();
        }
    }
}

/// Reset the controller and the link, and return the codec presence mask (STATESTS).
fn reset(ctx: &ServiceContext, m: &Mmio) -> Option<u16> {
    m.write16(STATESTS, 0x7FFF); // write-1-to-clear: forget any codec seen before this reset
    m.write32(GCTL, m.read32(GCTL) & !GCTL_CRST);
    if wait::until(ctx, CRST_WAIT, || m.read32(GCTL) & GCTL_CRST == 0).is_err() {
        ctx.log("audio-driver: the controller never entered reset (GCTL.CRST stayed 1)");
        return None;
    }
    delay::hold(ctx, RESET_HOLD);
    m.write32(GCTL, m.read32(GCTL) | GCTL_CRST);
    if wait::until(ctx, CRST_WAIT, || m.read32(GCTL) & GCTL_CRST != 0).is_err() {
        ctx.log("audio-driver: the controller never left reset (GCTL.CRST stayed 0)");
        return None;
    }
    delay::hold(ctx, CODEC_ENUM_HOLD);
    Some(m.read16(STATESTS) & 0x7FFF)
}

/// A node's connection list, short form only (8-bit entries), up to `out.len()`. Long-form lists and
/// ranges are reported and not followed; no codec this driver meets uses them on an output path.
fn connections(h: &mut Hda, cad: u32, nid: u32, out: &mut [u32; 8]) -> usize {
    let Some(len) = h.param(cad, nid, PARAM_CONN_LIST_LEN) else { return 0 };
    if len & 0x80 != 0 {
        h.ctx.log_fmt(format_args!(
            "audio-driver: node {:#04x} has a long-form connection list - not followed in this step", nid));
        return 0;
    }
    let n = ((len & 0x7F) as usize).min(out.len());
    let mut i = 0;
    while i < n {
        // Each response carries four 8-bit entries, starting at the index asked for.
        let Some(r) = h.verb(cad, nid, GET_CONNECTION_LIST, i as u32) else { break };
        for k in 0..4 {
            if i + k < n {
                let e = (r >> (8 * k)) & 0xFF;
                // Bit 7 marks a RANGE (from the previous entry to this one). Not followed; said once.
                if e & 0x80 != 0 {
                    h.ctx.log_fmt(format_args!(
                        "audio-driver: node {:#04x} lists a connection range - not followed in this step", nid));
                }
                out[i + k] = e & 0x7F;
            }
        }
        i += 4;
    }
    n
}

/// Widget type of `nid`, from its capabilities.
fn widget_type(h: &mut Hda, cad: u32, nid: u32) -> Option<u32> {
    h.param(cad, nid, PARAM_AUDIO_WIDGET_CAP).map(|c| (c >> 20) & 0xF)
}

/// Depth-bounded search from `nid` towards an output converter, recording the path in `path`, pin
/// first. Returns the path length when a DAC is reached.
fn find_dac(h: &mut Hda, cad: u32, nid: u32, path: &mut [u32; MAX_PATH], depth: usize) -> Option<usize> {
    if depth >= MAX_PATH {
        return None;
    }
    path[depth] = nid;
    let t = widget_type(h, cad, nid)?;
    if t == W_OUTPUT {
        return Some(depth + 1);
    }
    if depth > 0 && t != W_MIXER && t != W_SELECTOR {
        return None; // only mixers and selectors pass sound towards a pin
    }
    let mut conns = [0u32; 8];
    let n = connections(h, cad, nid, &mut conns);
    for &c in &conns[..n] {
        if let Some(len) = find_dac(h, cad, c, path, depth + 1) {
            return Some(len);
        }
    }
    None
}

/// The pin's device as words - the shared table, so the log and the shell's `audio` say the same.
fn device_name(dev: u32) -> &'static str {
    wire::device_name(dev)
}

/// An output path the survey found: the codec, its audio function group, and the nodes pin-first.
#[derive(Clone, Copy)]
struct OutPath {
    cad: u32,
    vendor: u32,
    device: u32,
    afg: u32,
    nodes: [u32; MAX_PATH],
    len: usize,
}

/// Walk one codec: its audio function group, its widgets, and the output paths it offers. Returns the
/// first usable path.
fn survey_codec(h: &mut Hda, cad: u32) -> Option<OutPath> {
    let ctx = h.ctx;
    let vid = h.param(cad, 0, PARAM_VENDOR_ID)?;
    ctx.log_fmt(format_args!("audio-driver: codec {}: vendor {:04x} device {:04x}", cad, vid >> 16, vid & 0xFFFF));
    let fgs = h.param(cad, 0, PARAM_SUB_NODE_COUNT)?;
    let (fg_start, fg_count) = ((fgs >> 16) & 0xFF, fgs & 0xFF);
    let mut first: Option<OutPath> = None;
    for fg in fg_start..fg_start + fg_count {
        let Some(ty) = h.param(cad, fg, PARAM_FUNCTION_GROUP_TYPE) else { continue };
        if ty & 0xFF != FG_AUDIO {
            continue;
        }
        let Some(ws) = h.param(cad, fg, PARAM_SUB_NODE_COUNT) else { continue };
        let (w_start, mut w_count) = ((ws >> 16) & 0xFF, (ws & 0xFF) as usize);
        if w_count > MAX_WIDGETS {
            ctx.log_fmt(format_args!(
                "audio-driver: codec {} audio function group {:#04x} has {} widgets - looking at the first {}",
                cad, fg, w_count, MAX_WIDGETS));
            w_count = MAX_WIDGETS;
        }
        ctx.log_fmt(format_args!(
            "audio-driver: codec {} audio function group at node {:#04x}, widgets {:#04x}..{:#04x}",
            cad, fg, w_start, w_start + w_count as u32 - 1));
        for nid in w_start..w_start + w_count as u32 {
            let Some(t) = widget_type(h, cad, nid) else { continue };
            if t != W_PIN {
                continue;
            }
            let Some(cfg) = h.verb(cad, nid, GET_CONFIG_DEFAULT, 0) else { continue };
            let connectivity = cfg >> 30; // 01: nothing attached
            let dev = (cfg >> 20) & 0xF;
            if connectivity == 0b01 || !matches!(dev, 0x0 | 0x1 | 0x2) {
                continue;
            }
            let mut nodes = [0u32; MAX_PATH];
            match find_dac(h, cad, nid, &mut nodes, 0) {
                Some(len) => {
                    log_path(ctx, cad, &nodes[..len], dev, cfg);
                    if first.is_none() {
                        first = Some(OutPath { cad, vendor: vid >> 16, device: vid & 0xFFFF, afg: fg, nodes, len });
                    }
                }
                None => ctx.log_fmt(format_args!(
                    "audio-driver: codec {} pin {:#04x} ({}) has no path to a converter within {} hops",
                    cad, nid, device_name(dev), MAX_PATH)),
            }
        }
    }
    if first.is_none() {
        ctx.log_fmt(format_args!("audio-driver: codec {} offers no output path this step can use", cad));
    }
    first
}

/// The path converter-first, the way sound flows: "0x02 -> 0x03".
fn log_path(ctx: &ServiceContext, cad: u32, nodes: &[u32], dev: u32, cfg: u32) {
    let mut line = [0u8; 64];
    let mut at = 0;
    let hex = b"0123456789abcdef";
    for (i, n) in nodes.iter().rev().enumerate() {
        let sep: &[u8] = if i == 0 { b"" } else { b" -> " };
        let digits = [b'0', b'x', hex[(*n as usize >> 4) & 0xF], hex[*n as usize & 0xF]];
        for &b in sep.iter().chain(digits.iter()) {
            if at < line.len() {
                line[at] = b;
                at += 1;
            }
        }
    }
    ctx.log_fmt(format_args!(
        "audio-driver: codec {} output path: {} ({}, config {:#010x})",
        cad, core::str::from_utf8(&line[..at]).unwrap_or("?"), device_name(dev), cfg));
}

// ---- A2: the command rings --------------------------------------------------------------------------

/// Stop the command rings, point them at the arena, reset their pointers and start them. On success the
/// next codec command goes through them. A pointer reset the controller does not acknowledge is
/// reported and passed over, as Linux does ("CORB reset timeout"); a ring that will not stop or start
/// is a failure, and commands stay on the immediate path.
fn start_rings<'a>(h: &mut Hda<'a>, d: &'a Dma) -> bool {
    let (ctx, m) = (h.ctx, h.m);
    m.write8(CORBCTL, 0);
    m.write8(RIRBCTL, 0);
    if wait::until(ctx, RING_WAIT, || m.read8(CORBCTL) & RING_RUN == 0 && m.read8(RIRBCTL) & RING_RUN == 0).is_err() {
        ctx.log("audio-driver: the command rings would not stop - staying on immediate commands");
        return false;
    }
    if m.read8(CORBSIZE) & 0x40 == 0 || m.read8(RIRBSIZE) & 0x40 == 0 {
        ctx.log_fmt(format_args!(
            "audio-driver: the controller offers no 256-entry rings (CORBSIZE={:#04x} RIRBSIZE={:#04x}) - staying on immediate commands",
            m.read8(CORBSIZE), m.read8(RIRBSIZE)));
        return false;
    }
    let corb = d.phys_at(CORB_OFF);
    let rirb = d.phys_at(RIRB_OFF);
    m.write32(CORBLBASE, corb as u32);
    m.write32(CORBUBASE, (corb >> 32) as u32);
    m.write8(CORBSIZE, RING_SIZE_256);
    m.write16(CORBWP, 0);
    m.write16(CORBRP, CORBRP_RST);
    if wait::until(ctx, RING_WAIT, || m.read16(CORBRP) & CORBRP_RST != 0).is_err() {
        ctx.log("audio-driver: the CORB read pointer did not acknowledge its reset - carrying on, as Linux does");
    }
    m.write16(CORBRP, 0);
    if wait::until(ctx, RING_WAIT, || m.read16(CORBRP) & CORBRP_RST == 0).is_err() {
        ctx.log("audio-driver: the CORB read pointer stayed in reset - staying on immediate commands");
        return false;
    }
    m.write32(RIRBLBASE, rirb as u32);
    m.write32(RIRBUBASE, (rirb >> 32) as u32);
    m.write8(RIRBSIZE, RING_SIZE_256);
    m.write16(RIRBWP, RIRBWP_RST);
    m.write16(RINTCNT, 1);
    m.write8(RIRBSTS, 0x05);
    m.write8(RIRBCTL, RING_RUN | RIRB_RINTCTL);
    m.write8(CORBCTL, RING_RUN);
    if wait::until(ctx, RING_WAIT, || m.read8(CORBCTL) & RING_RUN != 0 && m.read8(RIRBCTL) & RING_RUN != 0).is_err() {
        ctx.log("audio-driver: the command rings would not start - staying on immediate commands");
        m.write8(CORBCTL, 0);
        m.write8(RIRBCTL, 0);
        return false;
    }
    h.corb_wp = 0;
    h.rirb_rp = 0;
    h.dma = Some(d);
    true
}

// ---- A3: the output path and a tone -------------------------------------------------------------------

/// Bring a widget to D0, and wait until it says it is there (PS-Act, bits 7:4 of its power state).
/// Paced: each look is a verb.
fn power_up(h: &mut Hda, cad: u32, nid: u32) -> bool {
    let ctx = h.ctx;
    let _ = h.verb(cad, nid, SET_POWER_STATE, 0);
    let reached = wait::until_paced(ctx, POWER_WAIT, POWER_PACE, || {
        matches!(h.verb(cad, nid, GET_POWER_STATE, 0), Some(ps) if (ps >> 4) & 0xF == 0)
    })
    .is_ok();
    if !reached {
        ctx.log_fmt(format_args!("audio-driver: codec {} node {:#04x} did not reach power state D0", cad, nid));
    }
    reached
}

/// Set up the path for playback: power, amplifiers unmuted at full gain, each node's selection of the
/// next, the pin's output enable and EAPD where it has one, and the converter's format and stream tag.
fn configure_path(h: &mut Hda, p: &OutPath) -> bool {
    let cad = p.cad;
    if !power_up(h, cad, p.afg) {
        return false;
    }
    for i in 0..p.len {
        let nid = p.nodes[i];
        let Some(caps) = h.param(cad, nid, PARAM_AUDIO_WIDGET_CAP) else { return false };
        if caps & WCAP_POWER != 0 && !power_up(h, cad, nid) {
            return false;
        }
        if caps & WCAP_OUT_AMP != 0 {
            // Full gain, unmuted. The steps come from the widget's own amp capabilities, or the
            // function group's defaults when the widget has none of its own.
            let amp = match h.param(cad, nid, PARAM_OUT_AMP_CAP) {
                Some(0) | None => h.param(cad, p.afg, PARAM_OUT_AMP_CAP).unwrap_or(0),
                Some(a) => a,
            };
            let steps = (amp >> 8) & 0x7F;
            let _ = h.verb16(cad, nid, SET_AMP_GAIN_MUTE, AMP_OUT_LR_UNMUTED | steps);
        }
        // A node with more than one input listens to the next node on the path.
        if i + 1 < p.len {
            let mut conns = [0u32; 8];
            let n = connections(h, cad, nid, &mut conns);
            if n > 1 {
                if let Some(idx) = conns[..n].iter().position(|&c| c == p.nodes[i + 1]) {
                    let _ = h.verb(cad, nid, SET_CONNECT_SEL, idx as u32);
                }
            }
        }
    }
    let pin = p.nodes[0];
    let _ = h.verb(cad, pin, SET_PIN_WIDGET_CONTROL, PIN_OUT_EN);
    if matches!(h.param(cad, pin, PARAM_PIN_CAP), Some(pc) if pc & PINCAP_EAPD != 0) {
        let _ = h.verb(cad, pin, SET_EAPD_BTLENABLE, EAPD_ON);
    }
    let dac = p.nodes[p.len - 1];
    h.verb16(cad, dac, SET_CONVERTER_FORMAT, FMT_48K_16_STEREO).is_some()
        && h.verb(cad, dac, SET_CHANNEL_STREAMID, STREAM_TAG << 4).is_some()
}

/// A sine wave from a phase accumulator, in fixed point - no floating point and no table. One full turn
/// of the phase is 2^32. The polynomial is sin's Taylor series to x^7 over a quarter turn, folded to
/// the other three: worst error about 1.6e-4, some 76 dB down, past what 16 bits can tell.
struct Sine {
    phase: u32,
    step: u32,
}

impl Sine {
    fn new(hz: u32) -> Self {
        Sine { phase: 0, step: (((hz as u64) << 32) / RATE as u64) as u32 }
    }

    /// The next sample, at half of full scale.
    fn next(&mut self) -> i16 {
        const ONE: i64 = 1 << 30;
        const HALF_PI: i64 = 1_686_629_713; // pi/2 in Q30
        let quadrant = self.phase >> 30;
        let frac = (self.phase & 0x3FFF_FFFF) as i64; // a quarter turn, in Q30
        let x = (frac * HALF_PI) >> 30;
        let x = if quadrant & 1 == 1 { HALF_PI - x } else { x };
        let x2 = (x * x) >> 30;
        let mut t = ONE - x2 / 42;
        t = ONE - ((x2 * t) >> 30) / 20;
        t = ONE - ((x2 * t) >> 30) / 6;
        let s = (x * t) >> 30; // sin(x), Q30
        let s = if quadrant >= 2 { -s } else { s };
        self.phase = self.phase.wrapping_add(self.step);
        ((s * 16_383) >> 30) as i16
    }
}

/// Fill `bytes` of the ring from absolute offset `from`, with tone while any remains and silence after.
fn fill(d: &Dma, from: usize, bytes: usize, tone: &mut Sine, tone_left: &mut usize) {
    let mut at = from;
    while at < from + bytes {
        let s = if *tone_left > 0 {
            *tone_left = tone_left.saturating_sub(FRAME_BYTES);
            tone.next()
        } else {
            0
        };
        let v = s as u16 as u32;
        d.write32(PCM_OFF + at % PCM_LEN, v | v << 16);
        at += FRAME_BYTES;
    }
}

// ---- A4: settings that survive a restart (`/audio.settings`) ------------------------------------------

/// Where the volume and the mute are kept (`utilities/57_audio.md`). Plain labelled lines, readable with
/// `read /audio.settings`. The driver owns the file: it reads it once when it comes up and writes it after
/// a change. `on` and `off` are deliberately NOT kept - audio comes up on at every boot.
const SETTINGS_PATH: &str = "/audio.settings";
/// The whole file is read in one piece into this many bytes. Two lines need about twenty; a file larger
/// than this is not one this driver wrote, and is ignored with a line rather than half-read.
const SETTINGS_MAX: usize = 256;
/// How long one `fs` exchange may take before it counts as unanswered, and how many times the load asks.
/// At boot `fs` may still be mounting; a few seconds of patience, then the defaults and a line - never a
/// wait the driver cannot get out of.
const SETTINGS_PATIENCE_SECS: i64 = 2;
const SETTINGS_TRIES: u32 = 3;
const SETTINGS_RETRY_PAUSE: Budget = Budget::ms(1000);

/// What the file holds.
#[derive(Clone, Copy)]
struct Settings {
    volume: u8,
    muted: bool,
}

/// Read the settings, or `None` for the defaults - with the reason said once in the log either way.
fn load_settings(ctx: &ServiceContext) -> Option<Settings> {
    use gs::Error;
    for attempt in 1..=SETTINGS_TRIES {
        let mut fs = gs::fs::Fs::new(ctx).patience_secs(SETTINGS_PATIENCE_SECS);
        let mut buf = [0u8; SETTINGS_MAX];
        match fs.read_into(SETTINGS_PATH, &mut buf) {
            Ok(n) => return parse_settings(ctx, &buf[..n]),
            Err(Error::NotFound) => {
                ctx.log("audio-driver: no /audio.settings yet - starting at the defaults; it is written at the first change");
                return None;
            }
            Err(Error::NoFilesystem) => {
                ctx.log("audio-driver: no filesystem on this machine's disk - settings are kept in memory only");
                return None;
            }
            Err(Error::BufferTooSmall) => {
                ctx.log_fmt(format_args!(
                    "audio-driver: /audio.settings is larger than {} bytes - not a file this driver wrote; ignored, starting at the defaults",
                    SETTINGS_MAX));
                return None;
            }
            Err(e) if attempt < SETTINGS_TRIES && e.retry_is_safe() => {
                delay::hold_parked(ctx, SETTINGS_RETRY_PAUSE);
            }
            Err(e) => {
                ctx.log_fmt(format_args!(
                    "audio-driver: could not read /audio.settings ({}) after {} attempt(s) - starting at the defaults",
                    e.as_str(), attempt));
                return None;
            }
        }
    }
    None
}

/// `volume N` and `muted yes|no`, one per line. A line this driver does not know is ignored and said once;
/// a value out of range keeps the default for that setting and is said too.
fn parse_settings(ctx: &ServiceContext, text: &[u8]) -> Option<Settings> {
    let mut s = Settings { volume: DEFAULT_VOLUME, muted: false };
    let mut ignored = 0u32;
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(' ').map_or((line, ""), |(k, v)| (k, v.trim()));
        match (key, value) {
            ("volume", v) => match v.parse::<u8>() {
                Ok(n) if n <= wire::VOLUME_MAX => s.volume = n,
                _ => ignored += 1,
            },
            ("muted", "yes") => s.muted = true,
            ("muted", "no") => s.muted = false,
            _ => ignored += 1,
        }
    }
    if ignored > 0 {
        ctx.log_fmt(format_args!("audio-driver: /audio.settings has {} line(s) this driver does not understand - ignored", ignored));
    }
    ctx.log_fmt(format_args!(
        "audio-driver: settings read from /audio.settings - volume {}, {}", s.volume, if s.muted { "muted" } else { "unmuted" }));
    Some(s)
}

/// A line of text into a fixed buffer, for the settings file. Truncation is impossible at this size; if
/// it ever happened the write would be refused rather than a cut file saved.
struct Line {
    buf: [u8; 64],
    len: usize,
    overflow: bool,
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, t: &str) -> core::fmt::Result {
        let b = t.as_bytes();
        if self.len + b.len() > self.buf.len() {
            self.overflow = true;
            return Err(core::fmt::Error);
        }
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        Ok(())
    }
}

/// Write the settings. A failure is returned for the caller to report; `OutcomeUnknown` means the file may
/// or may not hold them, and is never re-sent as though it had not happened.
fn save_settings(ctx: &ServiceContext, s: Settings) -> Result<(), gs::Error> {
    use core::fmt::Write;
    let mut l = Line { buf: [0; 64], len: 0, overflow: false };
    let _ = write!(l, "volume {}\nmuted {}\n", s.volume, if s.muted { "yes" } else { "no" });
    if l.overflow {
        return Err(gs::Error::InvalidInput);
    }
    gs::fs::Fs::new(ctx).patience_secs(SETTINGS_PATIENCE_SECS).write(SETTINGS_PATH, &l.buf[..l.len])
}

// ---- A4: the driver as a service -----------------------------------------------------------------------

/// A tone that is playing: what is left to generate, and how far the stream has read.
struct Tone {
    sine: Sine,
    hz: u32,
    ms: u32,
    /// Bytes of tone not yet written to the ring.
    left: usize,
    /// Bytes of tone in all, so "played out" is `played >= bytes`.
    bytes: usize,
    /// Bytes written to the ring and read by the stream, from the start of this tone.
    filled: usize,
    played: usize,
    /// The stream's position (LPIB) at the last look, to measure how far it moved since.
    last: usize,
    underruns: u32,
    /// Counter at the start, for elapsed time; and the ticks per millisecond it is measured in.
    started: u64,
    watchdog: u32,
    interrupts_at_start: u64,
}

/// Where the volume is set: the output amplifier on the path nearest the converter, and its range.
#[derive(Clone, Copy)]
struct Amp {
    node: u32,
    /// The amplifier's top step (its NumSteps field); 0 when the path has no amplifier to set.
    steps: u32,
}

/// The playing half of the driver: the codec path, what the operator asked for, and a tone if one plays.
struct Player<'a> {
    h: Hda<'a>,
    d: &'a Dma,
    path: OutPath,
    pin_device: u32,
    amp: Option<Amp>,
    /// The first output stream's descriptor.
    sd: usize,
    power: u8,
    volume: u8,
    muted: bool,
    tone: Option<Tone>,
    underruns_total: u32,
    /// A change to the volume or the mute not yet written to `/audio.settings`. Written when nothing is
    /// playing: an `fs` write blocks the serve loop, and a slow one mid-tone would starve the ring.
    settings_dirty: bool,
    /// The last write failed and said so; the next failure is not said again until one succeeds.
    settings_failing: bool,
}

/// Why there is nothing to play on, when there is not (`wire::no_device`).
enum Device<'a> {
    Ready(Player<'a>),
    Absent(u8),
}

/// Ticks per millisecond of the counter, or 0 when it is uncalibrated.
fn ticks_per_ms(ctx: &ServiceContext) -> u64 {
    ctx.tsc_ticks_per_10ms() / 10
}

fn ms_since(ctx: &ServiceContext, t0: u64) -> u32 {
    match ticks_per_ms(ctx) {
        0 => 0,
        per => (ctx.read_tsc().wrapping_sub(t0) / per).min(u32::MAX as u64) as u32,
    }
}

impl<'a> Player<'a> {
    fn cad(&self) -> u32 {
        self.path.cad
    }

    /// The amplifier gain for `volume`: linear in the codec's steps, which are themselves even steps
    /// of decibels (each a fixed fraction of a dB, the widget's StepSize), so equal steps of volume
    /// sound like equal changes - the perceptual scale the spec asks for.
    fn gain_for(&self, volume: u8, steps: u32) -> u32 {
        (volume as u32 * steps + 50) / 100
    }

    /// Set the amplifier to the volume and the mute, and read it back. Volume 0 sets the amplifier's
    /// own mute too - silent - while `muted` stays false: the two are separate states.
    fn apply_volume(&mut self) -> u8 {
        let Some(a) = self.amp else { return wire::UNVERIFIED };
        let gain = self.gain_for(self.volume, a.steps);
        let silent = self.muted || self.volume == 0;
        let word = AMP_OUT_LR_UNMUTED | if silent { AMP_MUTE } else { 0 } | gain;
        let cad = self.cad();
        if self.h.verb16(cad, a.node, SET_AMP_GAIN_MUTE, word).is_none() {
            return wire::UNVERIFIED;
        }
        // Read both sides back: GET_AMP_GAIN_MUTE answers the mute bit and the gain for one side.
        let want = if silent { AMP_MUTE } else { 0 } | gain;
        let mut verdict = wire::VERIFIED;
        for side in [AMP_GET_LEFT, AMP_GET_RIGHT] {
            match self.h.verb16(cad, a.node, GET_AMP_GAIN_MUTE, AMP_GET_OUTPUT | side) {
                Some(r) if r & 0xFF == want => {}
                Some(r) => {
                    self.h.ctx.log_fmt(format_args!(
                        "audio-driver: node {:#04x} amplifier reads {:#04x} after {:#04x} was set",
                        a.node, r & 0xFF, want));
                    return wire::CONTRADICTED;
                }
                None => verdict = wire::UNVERIFIED,
            }
        }
        verdict
    }

    /// Is the function group at D0, by its own report? The power verdict for `on` and `off`.
    fn group_power(&mut self) -> Option<u32> {
        let (cad, afg) = (self.cad(), self.path.afg);
        self.h.verb(cad, afg, GET_POWER_STATE, 0).map(|ps| (ps >> 4) & 0xF)
    }

    fn power_off(&mut self, hard: bool) -> u8 {
        self.stop_tone("audio off");
        if hard {
            let (ctx, m) = (self.h.ctx, self.h.m);
            m.write8(CORBCTL, 0);
            m.write8(RIRBCTL, 0);
            m.write32(GCTL, m.read32(GCTL) & !GCTL_CRST);
            let held = wait::until(ctx, CRST_WAIT, || m.read32(GCTL) & GCTL_CRST == 0).is_ok();
            self.power = wire::POWER_HARD_OFF;
            return if held { wire::VERIFIED } else { wire::CONTRADICTED };
        }
        let (cad, afg) = (self.cad(), self.path.afg);
        let _ = self.h.verb(cad, afg, SET_POWER_STATE, D3);
        self.power = wire::POWER_OFF;
        // A function group that reports no D3 among its supported states cannot say it reached one:
        // QEMU's codec reports none at all, and answers D0 whatever it is told. That is the codec not
        // modelling power, not a refusal, and the answer says so rather than "failed".
        if matches!(self.h.param(cad, afg, PARAM_SUPPORTED_POWER_STATES), Some(s) if s & (1 << D3) == 0) {
            return wire::UNSUPPORTED;
        }
        let reached = wait::until_paced(self.h.ctx, POWER_WAIT, POWER_PACE, || {
            matches!(self.h.verb(cad, afg, GET_POWER_STATE, 0), Some(ps) if (ps >> 4) & 0xF == D3)
        });
        if reached.is_ok() {
            return wire::VERIFIED;
        }
        // Say what the codec DID report: the whole power-state word (PS-Set in bits 3:0, PS-Act in 7:4)
        // and the power states the function group claims to support, so a refusal can be told apart
        // from a codec that does not model power states at all.
        let ps = self.h.verb(cad, afg, GET_POWER_STATE, 0);
        let supported = self.h.param(cad, afg, PARAM_SUPPORTED_POWER_STATES);
        self.h.ctx.log_fmt(format_args!(
            "audio-driver: D3 was set on function group {:#04x} and it reports power state {:?} (supported states {:?})",
            afg, ps, supported));
        if ps.is_some() { wire::CONTRADICTED } else { wire::UNVERIFIED }
    }

    /// Back to full power, then the path set up again and the volume and mute re-applied. From a hard
    /// off that is the whole bring-up: the controller out of reset and the command rings restarted.
    fn power_on(&mut self) -> u8 {
        if self.power == wire::POWER_HARD_OFF {
            let (ctx, m) = (self.h.ctx, self.h.m);
            if reset(ctx, m).is_none() {
                return wire::CONTRADICTED;
            }
            self.h.dma = None;
            if !start_rings(&mut self.h, self.d) {
                return wire::CONTRADICTED;
            }
        }
        if !configure_path(&mut self.h, &self.path) {
            return wire::CONTRADICTED;
        }
        self.power = wire::POWER_ON;
        let v = self.apply_volume();
        match self.group_power() {
            Some(0) => if v == wire::CONTRADICTED { v } else { wire::VERIFIED },
            Some(_) => wire::CONTRADICTED,
            None => wire::UNVERIFIED,
        }
    }

    /// Start a tone: the stream reset and set up over the ring, the ring filled, RUN, and the interrupt
    /// for this stream on. Returns at once; [`Player::service`] keeps the ring filled as it plays.
    fn start_tone(&mut self, irq: &Irq, hz: u32, ms: u32) -> bool {
        let (ctx, m, d, sd) = (self.h.ctx, self.h.m, self.d, self.sd);
        // Reset the stream: RUN off first, then SRST set and read back, then cleared and read back.
        m.write32(sd + SD_CTL, m.read32(sd + SD_CTL) & 0x00FF_FFFD);
        let _ = wait::until(ctx, STREAM_WAIT, || m.read32(sd + SD_CTL) & SD_RUN == 0);
        m.write32(sd + SD_CTL, (m.read32(sd + SD_CTL) & 0x00FF_FFFF) | SD_SRST);
        if wait::until(ctx, STREAM_WAIT, || m.read32(sd + SD_CTL) & SD_SRST != 0).is_err() {
            ctx.log("audio-driver: the output stream did not enter reset");
            return false;
        }
        m.write32(sd + SD_CTL, m.read32(sd + SD_CTL) & 0x00FF_FFFE);
        if wait::until(ctx, STREAM_WAIT, || m.read32(sd + SD_CTL) & SD_SRST == 0).is_err() {
            ctx.log("audio-driver: the output stream did not leave reset");
            return false;
        }
        // The BDL: the ring as BDL_ENTRIES equal periods, each asking for an interrupt when it is done.
        let period = PCM_LEN / BDL_ENTRIES;
        for i in 0..BDL_ENTRIES {
            let e = BDL_OFF + i * 16;
            d.write64(e, d.phys_at(PCM_OFF + i * period));
            d.write32(e + 8, period as u32);
            d.write32(e + 12, BDL_IOC);
        }
        let bytes = (RATE as usize * ms as usize / 1000) * FRAME_BYTES;
        let mut t = Tone {
            sine: Sine::new(hz), hz, ms, left: bytes, bytes, filled: PCM_LEN, played: 0, last: 0,
            underruns: 0, started: 0, watchdog: 0, interrupts_at_start: irq.seen(),
        };
        fill(d, 0, PCM_LEN, &mut t.sine, &mut t.left); // the whole ring before RUN; the BDL is read at RUN
        let bdl = d.phys_at(BDL_OFF);
        m.write32(sd + SD_BDPL, bdl as u32);
        m.write32(sd + SD_BDPU, (bdl >> 32) as u32);
        m.write32(sd + SD_CBL, PCM_LEN as u32);
        m.write16(sd + SD_LVI, (BDL_ENTRIES - 1) as u16);
        m.write16(sd + SD_FMT, FMT_48K_16_STEREO as u16);
        m.write32(sd + SD_CTL, (STREAM_TAG << 20) | (SD_STS_ALL as u32) << SD_STS_SHIFT); // the tag; clear status
        // The controller's interrupt for this stream only: GIE, and this descriptor's SIE bit. The codec
        // command interrupt (CIE) stays off - the rings are waited on by polling, and are quick.
        m.write32(INTCTL, INTCTL_GIE | 1 << (sd - SD_BASE) / SD_STRIDE);
        t.started = ctx.read_tsc();
        m.write32(sd + SD_CTL, (STREAM_TAG << 20) | SD_IOCE | SD_RUN);
        ctx.log_fmt(format_args!("audio-driver: playing {} Hz for {} ms", hz, ms));
        self.tone = Some(t);
        true
    }

    /// Keep a playing tone fed: read how far the stream has got, refill the ring behind it, and end the
    /// tone when it has played out - or when it has taken a second longer than it should, which means
    /// the stream is not playing at the rate it was set to.
    fn service(&mut self, irq: &Irq) {
        let (m, d, sd) = (self.h.m, self.d, self.sd);
        let Some(t) = self.tone.as_mut() else { return };
        let lpib = m.read32(sd + SD_LPIB) as usize % PCM_LEN;
        t.played += (lpib + PCM_LEN - t.last) % PCM_LEN;
        t.last = lpib;
        if t.played > t.filled {
            t.underruns += 1;
            t.filled = t.played;
        }
        let room = t.played + PCM_LEN - t.filled;
        fill(d, t.filled, room, &mut t.sine, &mut t.left);
        t.filled += room;
        let ctx = self.h.ctx;
        if t.played >= t.bytes {
            let (hz, ms, u, took, w, n) = (t.hz, t.ms, t.underruns, ms_since(ctx, t.started), t.watchdog,
                irq.seen() - t.interrupts_at_start);
            self.end_stream();
            ctx.log_fmt(format_args!(
                "audio-driver: played {} Hz for {} ms in {} ms by the clock, {} underrun(s); {} interrupt(s), {} watchdog wake(s)",
                hz, ms, took, u, n, w));
        } else if ms_since(ctx, t.started) > t.ms.saturating_add(1000) {
            let (played, bytes, ms) = (t.played, t.bytes, t.ms);
            self.end_stream();
            ctx.log_fmt(format_args!(
                "audio-driver: the stream read {} of {} bytes in {} ms - it is not playing at the rate it was set to; stopped",
                played, bytes, ms + 1000));
        }
    }

    /// RUN and the interrupt off, the status cleared, and the tone forgotten. Its underruns are kept.
    fn end_stream(&mut self) {
        let (ctx, m, sd) = (self.h.ctx, self.h.m, self.sd);
        m.write32(sd + SD_CTL, STREAM_TAG << 20);
        m.write32(INTCTL, 0);
        m.write8(sd + SD_STS, SD_STS_ALL);
        if wait::until(ctx, STREAM_WAIT, || m.read32(sd + SD_CTL) & SD_RUN == 0).is_err() {
            ctx.log("audio-driver: the output stream did not report stopping");
        }
        if let Some(t) = self.tone.take() {
            self.underruns_total = self.underruns_total.saturating_add(t.underruns);
        }
    }

    /// Stop a tone early. Returns whether one was playing and how many milliseconds of it had played.
    fn stop_tone(&mut self, why: &str) -> (bool, u32) {
        let Some(t) = self.tone.as_ref() else { return (false, 0) };
        let played_ms = (t.played / FRAME_BYTES * 1000 / RATE as usize) as u32;
        let hz = t.hz;
        self.end_stream();
        self.h.ctx.log_fmt(format_args!("audio-driver: {} Hz stopped after {} ms ({})", hz, played_ms, why));
        (true, played_ms)
    }

    fn status(&self, irq: &Irq, out: &mut [u8]) -> usize {
        out[0] = wire::OK;
        out[1] = self.power;
        out[2] = self.muted as u8;
        out[3] = self.volume;
        let (playing, hz, len, elapsed, under) = match self.tone.as_ref() {
            Some(t) => (1, t.hz, t.ms, ms_since(self.h.ctx, t.started), t.underruns),
            None => (0, 0, 0, 0, 0),
        };
        out[4] = playing;
        wire::put_u16(out, 5, hz as u16);
        wire::put_u32(out, 7, len);
        wire::put_u32(out, 11, elapsed);
        wire::put_u32(out, 15, self.underruns_total.saturating_add(under));
        out[19] = irq.routed() as u8;
        out[20] = self.pin_device as u8;
        wire::STATUS_LEN
    }

    fn info(&mut self, irq: &Irq, out: &mut [u8]) -> usize {
        let m = self.h.m;
        out[0] = wire::OK;
        wire::put_u16(out, 1, self.path.vendor as u16);
        wire::put_u16(out, 3, self.path.device as u16);
        out[5] = self.path.cad as u8;
        out[6] = self.path.nodes[self.path.len - 1] as u8;
        out[7] = self.path.nodes[0] as u8;
        out[8] = self.pin_device as u8;
        let (steps, now) = match self.amp {
            Some(a) => (a.steps, self.gain_for(self.volume, a.steps)),
            None => (0, 0),
        };
        out[9] = steps as u8;
        out[10] = now as u8;
        wire::put_u32(out, 11, RATE);
        wire::put_u32(out, 15, PCM_LEN as u32);
        out[19] = irq.routed() as u8;
        wire::put_u32(out, 20, irq.seen().min(u32::MAX as u64) as u32);
        out[24] = m.read8(VMAJ);
        out[25] = m.read8(VMIN);
        wire::INFO_LEN
    }

    /// One request, answered into `out`. Returns the answer's length.
    fn answer(&mut self, irq: &Irq, op: u8, args: &[u8], out: &mut [u8]) -> usize {
        let on = self.power == wire::POWER_ON;
        match op {
            wire::OP_STATUS => self.status(irq, out),
            wire::OP_INFO => self.info(irq, out),
            wire::OP_VOLUME => {
                let Some(&v) = args.first() else { return bad(out) };
                if v > wire::VOLUME_MAX {
                    return bad(out);
                }
                self.volume = v;
                // Off, the codec cannot be asked: the volume is kept and applied by `audio on`.
                out[0] = wire::OK;
                out[1] = v;
                out[2] = if on { self.apply_volume() } else { wire::UNVERIFIED };
                // Kept on disk unless the codec CONTRADICTED it - a setting it refused is not written.
                self.settings_dirty |= out[2] != wire::CONTRADICTED;
                3
            }
            wire::OP_MUTE => {
                let want = args.first().copied().unwrap_or(1) != 0;
                if want == self.muted {
                    out[0] = wire::ALREADY;
                    out[1] = self.volume;
                    out[2] = wire::VERIFIED;
                    return 3;
                }
                self.muted = want;
                out[0] = wire::OK;
                out[1] = self.volume;
                out[2] = if on { self.apply_volume() } else { wire::UNVERIFIED };
                self.settings_dirty |= out[2] != wire::CONTRADICTED;
                3
            }
            wire::OP_POWER => {
                let mode = args.first().copied().unwrap_or(0xFF);
                if !matches!(mode, wire::POWER_OFF | wire::POWER_ON | wire::POWER_HARD_OFF) {
                    return bad(out);
                }
                if mode == self.power {
                    out[0] = wire::ALREADY;
                    out[1] = wire::VERIFIED;
                    return 2;
                }
                // From a hard off, `off` is a lighter off than the one in force: bring the controller up
                // first, so the state reported is the state the codec is in.
                if mode == wire::POWER_OFF && self.power == wire::POWER_HARD_OFF {
                    let _ = self.power_on();
                }
                out[0] = wire::OK;
                out[1] = match mode {
                    wire::POWER_ON => self.power_on(),
                    m => self.power_off(m == wire::POWER_HARD_OFF),
                };
                2
            }
            wire::OP_TONE => {
                let hz = wire::get_u16(args, 0);
                let ms = wire::get_u32(args, 2);
                if args.len() < 6 || !(wire::TONE_HZ_MIN..=wire::TONE_HZ_MAX).contains(&hz) || ms == 0
                    || ms > wire::TONE_MS_MAX {
                    return bad(out);
                }
                out[0] = if !on {
                    wire::AUDIO_OFF
                } else if self.tone.is_some() {
                    wire::BUSY
                } else if self.start_tone(irq, hz as u32, ms) {
                    wire::OK
                } else {
                    wire::NO_DEVICE
                };
                if out[0] == wire::NO_DEVICE {
                    out[1] = wire::no_device::BRINGUP_FAILED;
                    return 2;
                }
                1
            }
            wire::OP_STOP => {
                let (was, ms) = self.stop_tone("asked to stop");
                out[0] = wire::OK;
                out[1] = was as u8;
                wire::put_u32(out, 2, ms);
                6
            }
            _ => {
                out[0] = wire::UNKNOWN_OP;
                1
            }
        }
    }
}

fn bad(out: &mut [u8]) -> usize {
    out[0] = wire::BAD_ARG;
    1
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.trace_as("audio-driver");
    let mmio = ctx.mmio();
    let dma = ctx.dma_region();
    let irq = Irq::granted(&ctx);
    let dev = match (mmio.as_ref(), dma.as_ref()) {
        (None, _) => {
            ctx.log("audio-driver: no HD Audio controller was granted - nothing to drive on this machine");
            Device::Absent(wire::no_device::NO_CONTROLLER)
        }
        (Some(m), d) => bring_up(&ctx, m, d),
    };
    serve(&ctx, &irq, dev)
}

/// Reset the controller, survey the codecs, start the command rings and set up the output path. What
/// comes back is either a player, ready and idle, or the reason there is none.
fn bring_up<'a>(ctx: &'a ServiceContext, m: &'a Mmio, dma: Option<&'a Dma>) -> Device<'a> {
    let gcap = m.read16(GCAP);
    ctx.log_fmt(format_args!(
        "audio-driver: HD Audio {}.{}, {} output / {} input / {} bidirectional stream(s), window {} bytes",
        m.read8(VMAJ), m.read8(VMIN), (gcap >> 12) & 0xF, (gcap >> 8) & 0xF, (gcap >> 3) & 0x1F, m.len()));
    if !wait::calibrated(ctx) {
        ctx.log("audio-driver: no counter calibration - every wait below is a count of looks, not a time");
    }
    let codecs = match reset(ctx, m) {
        None => return Device::Absent(wire::no_device::RESET_FAILED),
        Some(0) => {
            ctx.log("audio-driver: the link came out of reset and no codec answered");
            return Device::Absent(wire::no_device::NO_CODEC);
        }
        Some(c) => c,
    };
    ctx.log_fmt(format_args!("audio-driver: codec(s) answered at mask {:#06x}", codecs));
    let mut h = Hda { ctx, m, dma: None, corb_wp: 0, rirb_rp: 0 };
    let mut path: Option<OutPath> = None;
    for cad in 0..15u32 {
        if codecs & (1 << cad) != 0 {
            let p = survey_codec(&mut h, cad);
            if path.is_none() {
                path = p;
            }
        }
    }
    let Some(path) = path else { return Device::Absent(wire::no_device::NO_PATH) };

    // From here on, DMA - on the codec it was verified on, and nowhere else (see the module docs).
    if path.vendor != VENDOR_QEMU {
        ctx.log_fmt(format_args!(
            "audio-driver: codec vendor {:04x} is not the one playback was verified on - stopping after the survey (docs/audio.md, A6)",
            path.vendor));
        return Device::Absent(wire::no_device::UNVERIFIED_CODEC);
    }
    let Some(d) = dma else {
        ctx.log("audio-driver: no DMA arena was granted - the survey is all this driver can do");
        return Device::Absent(wire::no_device::NO_ARENA);
    };
    if d.len() < ARENA_NEEDED {
        ctx.log_fmt(format_args!(
            "audio-driver: the DMA arena is {} bytes and this driver needs {} - stopping after the survey",
            d.len(), ARENA_NEEDED));
        return Device::Absent(wire::no_device::NO_ARENA);
    }
    d.zero();
    if !start_rings(&mut h, d) {
        return Device::Absent(wire::no_device::BRINGUP_FAILED);
    }
    // The rings proved by asking again what the survey already asked: same codec, same answer.
    match h.param(path.cad, 0, PARAM_VENDOR_ID) {
        Some(v) if v >> 16 == path.vendor => ctx.log_fmt(format_args!(
            "audio-driver: codec commands now go through the CORB and RIRB (vendor {:04x} read back through them)",
            v >> 16)),
        other => {
            ctx.log_fmt(format_args!(
                "audio-driver: the rings answered {:?} where the survey read vendor {:04x} - stopping",
                other, path.vendor));
            return Device::Absent(wire::no_device::BRINGUP_FAILED);
        }
    }
    if !configure_path(&mut h, &path) {
        ctx.log("audio-driver: the output path could not be configured");
        return Device::Absent(wire::no_device::BRINGUP_FAILED);
    }
    let amp = volume_amp(&mut h, &path);
    let pin_device = h.verb(path.cad, path.nodes[0], GET_CONFIG_DEFAULT, 0).map_or(0, |c| (c >> 20) & 0xF);
    let iss = ((m.read16(GCAP) >> 8) & 0xF) as usize;
    let mut p = Player {
        h, d, path, pin_device, amp,
        sd: SD_BASE + iss * SD_STRIDE, // the first output stream follows the input streams
        power: wire::POWER_ON, volume: DEFAULT_VOLUME, muted: false, tone: None, underruns_total: 0,
        settings_dirty: false, settings_failing: false,
    };
    if let Some(s) = load_settings(ctx) {
        p.volume = s.volume;
        p.muted = s.muted;
    }
    let v = p.apply_volume();
    match amp {
        Some(a) => ctx.log_fmt(format_args!(
            "audio-driver: volume {}{} on node {:#04x} ({} steps) - {}",
            p.volume, if p.muted { ", muted," } else { "" }, a.node, a.steps, verdict_word(v))),
        None => ctx.log("audio-driver: the output path has no amplifier - volume cannot be set on this codec"),
    }
    Device::Ready(p)
}

/// The amplifier the volume goes on: the output amplifier nearest the converter, since that is the one
/// every path to a pin shares. `None` when no node on the path has one.
fn volume_amp(h: &mut Hda, p: &OutPath) -> Option<Amp> {
    for i in (0..p.len).rev() {
        let nid = p.nodes[i];
        let caps = h.param(p.cad, nid, PARAM_AUDIO_WIDGET_CAP)?;
        if caps & WCAP_OUT_AMP == 0 {
            continue;
        }
        let amp = match h.param(p.cad, nid, PARAM_OUT_AMP_CAP) {
            Some(0) | None => h.param(p.cad, p.afg, PARAM_OUT_AMP_CAP).unwrap_or(0),
            Some(a) => a,
        };
        let steps = (amp >> 8) & 0x7F;
        if steps > 0 {
            return Some(Amp { node: nid, steps });
        }
    }
    None
}

fn verdict_word(v: u8) -> &'static str {
    match v {
        wire::VERIFIED => "verified",
        wire::UNSUPPORTED => "unconfirmed - the codec does not report it",
        wire::CONTRADICTED => "CONTRADICTED by the codec",
        _ => "unverified",
    }
}

/// Serve, and play. One wait for everything (`gs::driver::irq`): the stream's interrupt refills the ring,
/// a request is answered - including mid-tone, which is what lets every answer be immediate - and a
/// timeout while playing is the watchdog. Bounded per wake; nothing is held between requests.
fn serve(ctx: &ServiceContext, irq: &Irq, mut dev: Device) -> ! {
    match &dev {
        Device::Ready(_) => ctx.log(if irq.routed() {
            "audio-driver: ready - serving requests; playback refills on the stream's interrupt"
        } else {
            "audio-driver: ready - serving requests; NO interrupt was routed, so playback refills by polling"
        }),
        Device::Absent(r) => ctx.log_fmt(format_args!(
            "audio-driver: serving with no device to play on (reason {}) - every request is answered with that", r)),
    }
    let mut out = [0u8; 64];
    loop {
        let playing = matches!(&dev, Device::Ready(p) if p.tone.is_some());
        let within = match (playing, irq.routed()) {
            (false, _) => SERVE_WAIT,
            (true, true) => REFILL_WATCHDOG,
            (true, false) => REFILL_PACE,
        };
        let woke = irq.wait(ctx, within);
        if let Device::Ready(p) = &mut dev {
            let sts = p.sd + SD_STS;
            match &woke {
                // Clear the stream's status before the next interrupt can be raised. On a timeout too: an
                // interrupt that was lost leaves its status set, and a set status raises no new edge.
                Woke::Interrupt => {
                    p.h.m.write8(sts, SD_STS_ALL);
                    irq.rearm(ctx);
                }
                Woke::Timeout if playing => {
                    p.h.m.write8(sts, SD_STS_ALL);
                    if irq.routed() {
                        if let Some(t) = p.tone.as_mut() {
                            t.watchdog += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        if let Woke::Request(req) = woke {
            let n = match &mut dev {
                _ if req.payload_bytes().first() != Some(&wire::TAGGED) => {
                    out[2] = wire::UNKNOWN_OP; // untagged: answered, so a caller is never left waiting
                    3
                }
                Device::Absent(r) => {
                    out[2] = wire::NO_DEVICE;
                    out[3] = *r;
                    4
                }
                Device::Ready(p) => {
                    let b = req.payload_bytes();
                    let (op, args) = (b.get(2).copied().unwrap_or(0), b.get(3..).unwrap_or(&[]));
                    2 + p.answer(irq, op, args, &mut out[2..])
                }
            };
            out[0] = wire::TAGGED;
            out[1] = req.payload_bytes().get(1).copied().unwrap_or(0);
            if let Some(cap) = gs::ipc::take_sent_cap(ctx) {
                // `reply` gives the one-shot reply capability back either way (CLAUDE.md 8.5).
                let _ = gs::ipc::reply(ctx, cap, &Message::from_bytes(&out[..n]));
            }
        }
        if let Device::Ready(p) = &mut dev {
            p.service(irq);
            if p.settings_dirty && p.tone.is_none() {
                p.settings_dirty = false;
                match save_settings(ctx, Settings { volume: p.volume, muted: p.muted }) {
                    Ok(()) => p.settings_failing = false,
                    Err(e) if !p.settings_failing => {
                        p.settings_failing = true;
                        ctx.log_fmt(format_args!(
                            "audio-driver: could not write /audio.settings ({}) - the setting holds until this driver restarts{}",
                            e.as_str(),
                            if e == gs::Error::OutcomeUnknown { "; the file may or may not have it" } else { "" }));
                    }
                    Err(_) => {}
                }
            }
        }
    }
}
