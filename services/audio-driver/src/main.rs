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
//!   a buffer descriptor list, a cyclic ring of sound in the DMA arena. As first built it was refilled by
//!   polling and played once at start as a self-test; both are gone - the ring is refilled on the stream's
//!   interrupt (with a watchdog for a lost one), and a tone plays only when a request asks for it (A4).
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
use godspeed_audio::settings::{self, Settings};
use godspeed_audio::sine::Sine;
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
/// QEMU answered one command and then nothing. Linux sets it (with RINTCNT 1). The CPU interrupt for it
/// is a separate enable (INTCTL.CIE), left off: the driver polls the RIRB for responses. (Only the
/// output stream's interrupt is enabled, in `run_stream`.)
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
const GET_PIN_WIDGET_CONTROL: u32 = 0xF07;
const GET_PIN_SENSE: u32 = 0xF09;
const SET_PIN_SENSE: u32 = 0x709; // "execute": a pin that needs a trigger measures when this is sent
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
/// Supported PCM sizes and rates: bit 5 44.1 kHz, bit 6 48 kHz, bit 17 16-bit samples.
const PARAM_PCM: u32 = 0x0A;
const PCM_RATE_44K1: u32 = 1 << 5;
const PCM_RATE_48K: u32 = 1 << 6;
const PCM_BITS_16: u32 = 1 << 17;

const FG_AUDIO: u32 = 0x01;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_POWER: u32 = 1 << 10;
const PINCAP_EAPD: u32 = 1 << 16;
const PINCAP_TRIGGER: u32 = 1 << 1; // presence is measured only when asked (SET_PIN_SENSE)
const PINCAP_PRESENCE: u32 = 1 << 2; // the pin can tell whether something is plugged in
const SENSE_PRESENT: u32 = 1 << 31;
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
/// The same at the 44.1 kHz base (bit 14).
const FMT_44K1_16_STEREO: u32 = 0x4011;
/// A stream's sender has this long between sends before the stream is ended for it: the shell that
/// opened it may have been killed mid-file, and a stream nobody feeds must not hold the device.
const FEED_TIMEOUT_MS: u32 = 5000;
/// How far ahead of the stream's position the driver keeps written: the DMA engine reads ahead of
/// LPIB in bursts, so the bytes just past it are not safe to change, and a feed that falls behind this
/// has silence written instead.
const GUARD: usize = 1024;
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
    /// The last `TRACE_LEN` verbs and their answers, for `audio debug trace` - exactly what was said to
    /// the codec, as `wifi debug trace` is for frames. A fixed ring: the oldest is overwritten.
    trace: [Traced; TRACE_LEN],
    /// Verbs sent since the driver started, and how many of them had no answer.
    verbs: u64,
    verbs_failed: u64,
}

/// One verb in the trace: the word sent and the answer, or `None` when none came.
#[derive(Clone, Copy)]
struct Traced {
    verb: u32,
    answer: Option<u32>,
}

/// Verbs `audio debug trace` keeps.
const TRACE_LEN: usize = 64;

impl<'a> Hda<'a> {
    fn new(ctx: &'a ServiceContext, m: &'a Mmio) -> Self {
        Hda { ctx, m, dma: None, corb_wp: 0, rirb_rp: 0, trace: [Traced { verb: 0, answer: None }; TRACE_LEN], verbs: 0, verbs_failed: 0 }
    }

    /// One codec command, by whichever path is up. `None` names the failure in the log.
    fn send(&mut self, word: u32) -> Option<u32> {
        let answer = match self.dma {
            Some(d) => self.send_ring(d, word),
            None => self.send_immediate(word),
        };
        self.trace[(self.verbs % TRACE_LEN as u64) as usize] = Traced { verb: word, answer };
        self.verbs += 1;
        if answer.is_none() {
            self.verbs_failed += 1;
        }
        answer
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
    /// The pin's default-device field (`wire::device_name`): line out, speaker, headphone.
    dev: u32,
}

/// Every output path one codec offers, in the order the survey found them; the first is the one played
/// through until `audio output` chooses another.
struct Outputs {
    paths: [Option<OutPath>; wire::OUTPUTS_MAX],
    n: usize,
}

impl Outputs {
    fn get(&self, i: usize) -> Option<&OutPath> {
        self.paths.get(i).and_then(|p| p.as_ref())
    }

    /// The output whose pin is `pin`.
    fn by_pin(&self, pin: u32) -> Option<OutPath> {
        (0..self.n).filter_map(|i| self.get(i).copied()).find(|p| p.nodes[0] == pin)
    }
}

/// Walk one codec: its audio function group, its widgets, and the output paths it offers, every one of
/// them into `outs` (up to `wire::OUTPUTS_MAX`, the rest said in the log). Returns how many it found.
fn survey_codec(h: &mut Hda, cad: u32, outs: &mut Outputs) -> usize {
    outs.n = 0;
    let mut left_out = 0u32;
    survey_paths(h, cad, outs, &mut left_out);
    if left_out > 0 {
        h.ctx.log_fmt(format_args!(
            "audio-driver: codec {} offers {} more output(s) than the {} listed - not offered",
            cad, left_out, wire::OUTPUTS_MAX));
    }
    if outs.n == 0 {
        h.ctx.log_fmt(format_args!("audio-driver: codec {} offers no output path this step can use", cad));
    }
    outs.n
}

/// The survey proper: every pin that can play, and its path to a converter, recorded in `outs`.
fn survey_paths(h: &mut Hda, cad: u32, outs: &mut Outputs, left_out: &mut u32) -> Option<()> {
    let ctx = h.ctx;
    let vid = h.param(cad, 0, PARAM_VENDOR_ID)?;
    ctx.log_fmt(format_args!("audio-driver: codec {}: vendor {:04x} device {:04x}", cad, vid >> 16, vid & 0xFFFF));
    let fgs = h.param(cad, 0, PARAM_SUB_NODE_COUNT)?;
    let (fg_start, fg_count) = ((fgs >> 16) & 0xFF, fgs & 0xFF);
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
                    let p = OutPath { cad, vendor: vid >> 16, device: vid & 0xFFFF, afg: fg, nodes, len, dev };
                    match outs.paths.get_mut(outs.n) {
                        Some(slot) => {
                            *slot = Some(p);
                            outs.n += 1;
                        }
                        None => *left_out += 1,
                    }
                }
                None => ctx.log_fmt(format_args!(
                    "audio-driver: codec {} pin {:#04x} ({}) has no path to a converter within {} hops",
                    cad, nid, device_name(dev), MAX_PATH)),
            }
        }
    }
    Some(())
}

/// Whether something is plugged into `pin`, by the pin's own sense - `PRESENCE_UNKNOWN` for a pin that
/// cannot tell, or a codec that did not answer.
fn presence(h: &mut Hda, cad: u32, pin: u32) -> u8 {
    let Some(caps) = h.param(cad, pin, PARAM_PIN_CAP) else { return wire::PRESENCE_UNKNOWN };
    if caps & PINCAP_PRESENCE == 0 {
        return wire::PRESENCE_UNKNOWN;
    }
    if caps & PINCAP_TRIGGER != 0 {
        let _ = h.verb(cad, pin, SET_PIN_SENSE, 0);
    }
    match h.verb(cad, pin, GET_PIN_SENSE, 0) {
        Some(s) if s & SENSE_PRESENT != 0 => wire::PRESENCE_PLUGGED,
        Some(_) => wire::PRESENCE_EMPTY,
        None => wire::PRESENCE_UNKNOWN,
    }
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
    /// The sample rate the stream runs at: 48000 for a tone, the file's for a stream.
    rate: u32,
    /// `Some` when the samples come from a sender (`OP_OPEN`) rather than the sine.
    feed: Option<Feed>,
    /// Bytes of silence written because the sender fell behind.
    silence: usize,
}

/// A stream fed by a sender, through `OP_PCM`.
struct Feed {
    channels: u8,
    /// `OP_END` arrived: nothing more is coming, play out what is in the ring.
    ended: bool,
    /// RUN is set. A stream starts once half the ring is written, or at the end.
    running: bool,
    /// Counter at the last `OP_PCM`, for `FEED_TIMEOUT_MS`.
    last_feed: u64,
    frames: u32,
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
    /// The output playing now - one of `outputs`.
    path: OutPath,
    /// Every output the codec offers, for `audio outputs` and `audio output`.
    outputs: Outputs,
    /// The output chosen with `audio output`, kept in `/audio.settings` as its device field; `None` until
    /// one is chosen, so a machine that never chose keeps playing through whatever the survey found first.
    output_chosen: Option<u8>,
    pin_device: u32,
    amp: Option<Amp>,
    /// The first output stream's descriptor.
    sd: usize,
    power: u8,
    volume: u8,
    muted: bool,
    tone: Option<Tone>,
    underruns_total: u32,
    /// The silence the last stream had written in its place, for `status` after it ends.
    last_silence_ms: u32,
    /// The last sound to end, for `audio debug stats`: what it was, how much it played and how long that
    /// took by the clock - the measured play rate, which proves the DMA engine and the link clock run at
    /// the speed they were set to.
    last: Option<LastPlay>,
    /// A change to the volume or the mute not yet written to `/audio.settings`. Written when nothing is
    /// playing: an `fs` write blocks the serve loop, and a slow one mid-tone would starve the ring.
    settings_dirty: bool,
    /// The last write failed and said so; the next failure is not said again until one succeeds.
    settings_failing: bool,
}

/// How the last sound went (`audio debug stats`).
#[derive(Clone, Copy)]
struct LastPlay {
    /// The tone's frequency, or 0 for a stream.
    hz: u32,
    rate: u32,
    played_ms: u32,
    took_ms: u32,
    underruns: u32,
    watchdog: u32,
}

/// Why there is nothing to play on, when there is not (`wire::no_device`).
enum Device<'a> {
    Ready(Player<'a>),
    Absent(u8),
    /// The codec was surveyed and the driver stopped short of playing - on the T630 today, a codec
    /// playback has not been verified on (A6). The controller is kept, so `audio debug` can still show
    /// the codec, the verbs and the registers: the dump that finds a real codec's path is needed most
    /// exactly here.
    Surveyed(u8, Hda<'a>, OutPath),
}

/// Ticks per millisecond of the counter, or 0 when it is uncalibrated.
fn ticks_per_ms(ctx: &ServiceContext) -> u64 {
    wait::ticks_per_10ms(ctx) / 10
}

fn ms_since(ctx: &ServiceContext, t0: u64) -> u32 {
    match ticks_per_ms(ctx) {
        0 => 0,
        per => (wait::ticks(ctx).wrapping_sub(t0) / per).min(u32::MAX as u64) as u32,
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

    /// Reset the output stream and set it up over the ring at `rate`, the converter with it - everything
    /// short of RUN. The BDL: the ring as BDL_ENTRIES equal periods, each asking for an interrupt.
    fn prepare_stream(&mut self, rate: u32) -> bool {
        let (ctx, m, d, sd) = (self.h.ctx, self.h.m, self.d, self.sd);
        // RUN off first, then SRST set and read back, then cleared and read back.
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
        let period = PCM_LEN / BDL_ENTRIES;
        for i in 0..BDL_ENTRIES {
            let e = BDL_OFF + i * 16;
            d.write64(e, d.phys_at(PCM_OFF + i * period));
            d.write32(e + 8, period as u32);
            d.write32(e + 12, BDL_IOC);
        }
        let fmt = if rate == 44_100 { FMT_44K1_16_STEREO } else { FMT_48K_16_STEREO };
        // The converter must agree with the stream, or it plays at the wrong speed.
        let (cad, dac) = (self.cad(), self.path.nodes[self.path.len - 1]);
        if self.h.verb16(cad, dac, SET_CONVERTER_FORMAT, fmt).is_none() {
            return false;
        }
        let bdl = d.phys_at(BDL_OFF);
        m.write32(sd + SD_BDPL, bdl as u32);
        m.write32(sd + SD_BDPU, (bdl >> 32) as u32);
        m.write32(sd + SD_CBL, PCM_LEN as u32);
        m.write16(sd + SD_LVI, (BDL_ENTRIES - 1) as u16);
        m.write16(sd + SD_FMT, fmt as u16);
        m.write32(sd + SD_CTL, (STREAM_TAG << 20) | (SD_STS_ALL as u32) << SD_STS_SHIFT); // the tag; clear status
        true
    }

    /// RUN, with the controller's interrupt for this stream only: GIE and this descriptor's SIE bit.
    /// The codec command interrupt (CIE) stays off - the rings are waited on by polling, and are quick.
    fn run_stream(&mut self) {
        let (ctx, m, sd) = (self.h.ctx, self.h.m, self.sd);
        m.write32(INTCTL, INTCTL_GIE | 1 << (sd - SD_BASE) / SD_STRIDE);
        if let Some(t) = self.tone.as_mut() {
            t.started = wait::ticks(ctx);
            if let Some(f) = t.feed.as_mut() {
                f.running = true;
            }
        }
        m.write32(sd + SD_CTL, (STREAM_TAG << 20) | SD_IOCE | SD_RUN);
    }

    /// Start a tone: the stream set up over the ring, the ring filled with sine, RUN. Returns at once;
    /// [`Player::service`] keeps the ring filled as it plays.
    fn start_tone(&mut self, irq: &Irq, hz: u32, ms: u32) -> bool {
        if !self.prepare_stream(RATE) {
            return false;
        }
        let bytes = (RATE as usize * ms as usize / 1000) * FRAME_BYTES;
        let mut t = Tone {
            sine: Sine::new(hz, RATE), hz, ms, left: bytes, bytes, filled: PCM_LEN, played: 0, last: 0,
            underruns: 0, started: 0, watchdog: 0, interrupts_at_start: irq.seen(), rate: RATE, feed: None,
            silence: 0,
        };
        fill(self.d, 0, PCM_LEN, &mut t.sine, &mut t.left); // the whole ring before RUN; the BDL is read at RUN
        self.tone = Some(t);
        self.run_stream();
        self.h.ctx.log_fmt(format_args!("audio-driver: playing {} Hz for {} ms", hz, ms));
        true
    }

    /// The rates and sizes the converter says it takes (its own PCM parameter, or the function group's
    /// default when it has none).
    fn pcm_caps(&mut self) -> u32 {
        let (cad, dac, afg) = (self.cad(), self.path.nodes[self.path.len - 1], self.path.afg);
        match self.h.param(cad, dac, PARAM_PCM) {
            Some(0) | None => self.h.param(cad, afg, PARAM_PCM).unwrap_or(0),
            Some(c) => c,
        }
    }

    /// Open a stream the caller will feed with `OP_PCM`. The ring starts silent and nothing plays yet.
    fn open_stream(&mut self, irq: &Irq, rate: u32, channels: u8, bits: u8, frames: u32, out: &mut [u8]) -> usize {
        if bits != 16 {
            out[0] = wire::FORMAT;
            out[1] = wire::format::BITS;
            return 2;
        }
        if channels != 1 && channels != 2 {
            out[0] = wire::FORMAT;
            out[1] = wire::format::CHANNELS;
            return 2;
        }
        let caps = self.pcm_caps();
        let rate_ok = match rate {
            48_000 => caps & PCM_RATE_48K != 0,
            44_100 => caps & PCM_RATE_44K1 != 0,
            _ => false,
        };
        if !rate_ok || caps & PCM_BITS_16 == 0 {
            self.h.ctx.log_fmt(format_args!(
                "audio-driver: a {} Hz stream was refused - the converter offers PCM {:#010x}", rate, caps));
            out[0] = wire::FORMAT;
            out[1] = wire::format::RATE;
            return 2;
        }
        if !self.prepare_stream(rate) {
            out[0] = wire::NO_DEVICE;
            out[1] = wire::no_device::BRINGUP_FAILED;
            return 2;
        }
        let mut quiet = Sine::new(1, RATE);
        let mut none = 0usize;
        fill(self.d, 0, PCM_LEN, &mut quiet, &mut none); // silence, until the sender writes over it
        let ctx = self.h.ctx;
        self.tone = Some(Tone {
            sine: quiet, hz: 0, ms: (frames as u64 * 1000 / rate as u64) as u32, left: 0,
            bytes: frames as usize * FRAME_BYTES, filled: 0, played: 0, last: 0, underruns: 0, started: 0,
            watchdog: 0, interrupts_at_start: irq.seen(), rate, silence: 0,
            feed: Some(Feed { channels, ended: false, running: false, last_feed: wait::ticks(ctx), frames }),
        });
        ctx.log_fmt(format_args!("audio-driver: stream opened - {} Hz, {} channel(s), {} frames", rate, channels, frames));
        out[0] = wire::OK;
        wire::put_u32(out, 1, self.free_frames());
        5
    }

    /// Room for frames in the ring: everything the stream has already read, less a guard behind it.
    fn free_frames(&self) -> u32 {
        let Some(t) = self.tone.as_ref() else { return 0 };
        let running = t.feed.as_ref().is_some_and(|f| f.running);
        let limit = if running { t.played + PCM_LEN - GUARD } else { PCM_LEN };
        (limit.saturating_sub(t.filled) / FRAME_BYTES) as u32
    }

    /// Take whole frames from a sender into the ring; as many as fit. Mono is written to both sides.
    fn feed_pcm(&mut self, data: &[u8], out: &mut [u8]) -> usize {
        let d = self.d;
        let free = self.free_frames() as usize;
        let ctx = self.h.ctx;
        let Some(t) = self.tone.as_mut() else { out[0] = wire::NOT_OPEN; return 1 };
        let Some(f) = t.feed.as_mut() else { out[0] = wire::NOT_OPEN; return 1 };
        let per = 2 * f.channels as usize;
        let n = (data.len() / per).min(free);
        for i in 0..n {
            let l = u16::from_le_bytes([data[i * per], data[i * per + 1]]) as u32;
            let r = if f.channels == 2 { u16::from_le_bytes([data[i * per + 2], data[i * per + 3]]) as u32 } else { l };
            d.write32(PCM_OFF + (t.filled + i * FRAME_BYTES) % PCM_LEN, l | r << 16);
        }
        t.filled += n * FRAME_BYTES;
        f.last_feed = wait::ticks(ctx);
        let start = !f.running && t.filled >= PCM_LEN / 2;
        if start {
            self.run_stream();
        }
        out[0] = wire::OK;
        wire::put_u32(out, 1, n as u32);
        wire::put_u32(out, 5, self.free_frames());
        9
    }

    /// Nothing more is coming: play out what is in the ring, then stop.
    fn end_feed(&mut self, out: &mut [u8]) -> usize {
        let Some(t) = self.tone.as_mut() else { out[0] = wire::NOT_OPEN; return 1 };
        let Some(f) = t.feed.as_mut() else { out[0] = wire::NOT_OPEN; return 1 };
        f.ended = true;
        let start = !f.running && t.filled > 0;
        let empty = t.filled == 0;
        if start {
            self.run_stream();
        } else if empty {
            self.end_stream();
        }
        out[0] = wire::OK;
        1
    }

    /// Keep what is playing fed. A tone: refill the ring behind the stream's position with sine, and end
    /// it when it has played out - or a second late, which means the stream is not playing at the rate it
    /// was set to. A stream: when it has run dry, write silence ahead of the position and count it; when
    /// the sender has ended, silence the free ring so nothing stale plays, and stop once all is played.
    fn service(&mut self, irq: &Irq) {
        let (m, d, sd, ctx) = (self.h.m, self.d, self.sd, self.h.ctx);
        let Some(t) = self.tone.as_mut() else { return };
        if let Some(f) = t.feed.as_mut() {
            if !f.running {
                if ms_since(ctx, f.last_feed) > FEED_TIMEOUT_MS {
                    ctx.log("audio-driver: a stream was opened and never fed - closed");
                    self.end_stream();
                }
                return;
            }
        }
        let lpib = m.read32(sd + SD_LPIB) as usize % PCM_LEN;
        t.played += (lpib + PCM_LEN - t.last) % PCM_LEN;
        t.last = lpib;
        if let Some(f) = t.feed.as_mut() {
            if !f.ended && ms_since(ctx, f.last_feed) > FEED_TIMEOUT_MS {
                ctx.log("audio-driver: the stream's sender stopped sending - playing out what it sent");
                f.ended = true;
            }
            let ended = f.ended;
            let mut quiet = Sine::new(1, RATE);
            let mut none = 0usize;
            if ended {
                let free = (t.played + PCM_LEN).saturating_sub(t.filled);
                fill(d, t.filled, free, &mut quiet, &mut none);
                if t.played >= t.filled {
                    let (frames, rate, u, sil, took, n) = (f.frames, t.rate, t.underruns,
                        (t.silence / FRAME_BYTES * 1000 / t.rate as usize) as u32, ms_since(ctx, t.started),
                        irq.seen() - t.interrupts_at_start);
                    self.end_stream();
                    ctx.log_fmt(format_args!(
                        "audio-driver: played a stream of {} frames at {} Hz in {} ms by the clock, {} underrun(s), {} ms of silence; {} interrupt(s)",
                        frames, rate, took, u, sil, n));
                }
            } else if t.filled < t.played + GUARD {
                let pad = t.played + GUARD - t.filled;
                fill(d, t.filled, pad, &mut quiet, &mut none);
                t.filled += pad;
                t.silence += pad;
                t.underruns += 1;
            }
            return;
        }
        if t.played > t.filled {
            t.underruns += 1;
            t.filled = t.played;
        }
        let room = t.played + PCM_LEN - t.filled;
        fill(d, t.filled, room, &mut t.sine, &mut t.left);
        t.filled += room;
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

    /// RUN and the interrupt off, the status cleared, and the tone or stream forgotten. Its underruns,
    /// and a stream's silence, are kept for `status`.
    fn end_stream(&mut self) {
        let (ctx, m, sd) = (self.h.ctx, self.h.m, self.sd);
        m.write32(sd + SD_CTL, STREAM_TAG << 20);
        m.write32(INTCTL, 0);
        m.write8(sd + SD_STS, SD_STS_ALL);
        if wait::until(ctx, STREAM_WAIT, || m.read32(sd + SD_CTL) & SD_RUN == 0).is_err() {
            ctx.log("audio-driver: the output stream did not report stopping");
        }
        if let Some(t) = self.tone.take() {
            if t.started != 0 {
                self.last = Some(LastPlay {
                    hz: if t.feed.is_some() { 0 } else { t.hz }, rate: t.rate,
                    played_ms: (t.played / FRAME_BYTES * 1000 / t.rate as usize) as u32,
                    took_ms: ms_since(ctx, t.started), underruns: t.underruns, watchdog: t.watchdog,
                });
            }
            self.underruns_total = self.underruns_total.saturating_add(t.underruns);
            if t.feed.is_some() {
                self.last_silence_ms = (t.silence / FRAME_BYTES * 1000 / t.rate as usize) as u32;
            }
        }
    }

    /// Stop what is playing early. Returns whether anything was, and how many milliseconds had played.
    fn stop_tone(&mut self, why: &str) -> (bool, u32) {
        let Some(t) = self.tone.as_ref() else { return (false, 0) };
        let played_ms = (t.played / FRAME_BYTES * 1000 / t.rate as usize) as u32;
        let what = if t.feed.is_some() { 0 } else { t.hz };
        self.end_stream();
        match what {
            0 => self.h.ctx.log_fmt(format_args!("audio-driver: stream stopped after {} ms ({})", played_ms, why)),
            hz => self.h.ctx.log_fmt(format_args!("audio-driver: {} Hz stopped after {} ms ({})", hz, played_ms, why)),
        }
        (true, played_ms)
    }

    fn status(&self, irq: &Irq, out: &mut [u8]) -> usize {
        out[0] = wire::OK;
        out[1] = self.power;
        out[2] = self.muted as u8;
        out[3] = self.volume;
        let (playing, hz, len, elapsed, under, sil) = match self.tone.as_ref() {
            Some(t) if t.feed.is_some() => (wire::PLAYING_STREAM, 0, t.ms,
                (t.played / FRAME_BYTES * 1000 / t.rate as usize) as u32, t.underruns,
                (t.silence / FRAME_BYTES * 1000 / t.rate as usize) as u32),
            Some(t) => (wire::PLAYING_TONE, t.hz, t.ms, ms_since(self.h.ctx, t.started), t.underruns, 0),
            None => (wire::PLAYING_NOTHING, 0, 0, 0, 0, self.last_silence_ms),
        };
        out[4] = playing;
        wire::put_u16(out, 5, hz as u16);
        wire::put_u32(out, 7, len);
        wire::put_u32(out, 11, elapsed);
        wire::put_u32(out, 15, self.underruns_total.saturating_add(under));
        out[19] = irq.routed() as u8;
        out[20] = self.pin_device as u8;
        wire::put_u32(out, 21, sil);
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
        out[26] = wire::KIND_HDA;
        wire::put_u16(out, 27, 0);
        wire::INFO_LEN
    }

    /// `[OK, count, (pin, device, selected, presence) x count]` - every output, the one playing marked,
    /// and whether something is plugged into each where the pin can tell.
    fn outputs_answer(&mut self, out: &mut [u8]) -> usize {
        out[0] = wire::OK;
        let (cad, now) = (self.cad(), self.path.nodes[0]);
        let mut at = 2;
        let mut count = 0u8;
        for i in 0..self.outputs.n {
            let Some(p) = self.outputs.get(i).copied() else { continue };
            if at + 4 > out.len() {
                break;
            }
            let pin = p.nodes[0];
            out[at] = pin as u8;
            out[at + 1] = p.dev as u8;
            out[at + 2] = (pin == now) as u8;
            out[at + 3] = presence(&mut self.h, cad, pin);
            at += 4;
            count += 1;
        }
        out[1] = count;
        at
    }

    /// Play through the output whose pin is `pin`: the old pin's output and EAPD switched off, the new
    /// path configured as at bring-up, the volume re-applied on the new path's amplifier. The verdict is
    /// the new pin's own report that its output is enabled - and the volume's, where that disagrees.
    fn select_output(&mut self, p: OutPath) -> u8 {
        let (cad, old) = (self.cad(), self.path.nodes[0]);
        if old != p.nodes[0] {
            let _ = self.h.verb(cad, old, SET_PIN_WIDGET_CONTROL, 0);
            if matches!(self.h.param(cad, old, PARAM_PIN_CAP), Some(pc) if pc & PINCAP_EAPD != 0) {
                let _ = self.h.verb(cad, old, SET_EAPD_BTLENABLE, 0);
            }
        }
        if !configure_path(&mut self.h, &p) {
            self.h.ctx.log_fmt(format_args!(
                "audio-driver: the path to pin {:#04x} ({}) could not be configured", p.nodes[0], device_name(p.dev)));
            return wire::CONTRADICTED;
        }
        self.path = p;
        self.pin_device = p.dev;
        self.amp = volume_amp(&mut self.h, &p);
        let v = self.apply_volume();
        let enabled = self.h.verb(cad, p.nodes[0], GET_PIN_WIDGET_CONTROL, 0);
        self.h.ctx.log_fmt(format_args!("audio-driver: output now pin {:#04x} ({})", p.nodes[0], device_name(p.dev)));
        match enabled {
            Some(c) if c & PIN_OUT_EN == 0 => wire::CONTRADICTED,
            None => wire::UNVERIFIED,
            Some(_) if v == wire::CONTRADICTED => wire::CONTRADICTED,
            Some(_) => wire::VERIFIED,
        }
    }

    /// `audio debug`: one view as text, one page of it (`wire::OP_DEBUG`). The whole view is rendered for
    /// every page and the page cut from it, so nothing is held between an asker's pages.
    fn debug(&mut self, irq: &Irq, view: u8, page: u8, out: &mut [u8]) -> usize {
        let now = self.path.nodes[0];
        debug_page(out, page, |w| match view {
            wire::DEBUG_STATS => self.debug_stats(irq, w),
            wire::DEBUG_CODEC => debug_codec(&mut self.h, &self.path, now, w),
            wire::DEBUG_STREAM => self.debug_stream(w),
            wire::DEBUG_TRACE => debug_trace(&self.h, w),
            _ => debug_registers(self.h.m, w),
        })
    }

    fn debug_stats(&mut self, irq: &Irq, w: &mut wire::Page) {
        use core::fmt::Write;
        let _ = writeln!(w, "verbs        {} sent, {} unanswered", self.h.verbs, self.h.verbs_failed);
        let _ = writeln!(w, "commands     {}", if self.h.dma.is_some() { "through the CORB and RIRB" } else { "by immediate command" });
        let _ = writeln!(w, "interrupts   {} taken; refill {}", irq.seen(),
            if irq.routed() { "on the stream's interrupt" } else { "by polling - none was routed" });
        let _ = writeln!(w, "underruns    {} since the driver started", self.underruns_total);
        match self.tone.as_ref() {
            Some(t) => {
                let ahead = t.filled.saturating_sub(t.played);
                let _ = writeln!(w, "playing      {}, ring {} of {} bytes ahead of the stream ({} ms)",
                    if t.feed.is_some() { "a stream" } else { "a tone" }, ahead, PCM_LEN,
                    ahead as u64 * 1000 / (t.rate as u64 * FRAME_BYTES as u64));
            }
            None => {
                let _ = writeln!(w, "playing      nothing");
            }
        }
        match self.last {
            Some(l) => {
                // Per mille of real time: 1000 is exact, below it the stream ran slow by the clock.
                let rate = if l.took_ms == 0 { 0 } else { l.played_ms as u64 * 1000 / l.took_ms as u64 };
                let what = if l.hz == 0 { "a stream at" } else { "a tone of" };
                let _ = writeln!(w, "last sound   {} {} Hz: {} ms of sound in {} ms by the clock - played at {}.{}% of real time",
                    what, if l.hz == 0 { l.rate } else { l.hz }, l.played_ms, l.took_ms, rate / 10, rate % 10);
                let _ = writeln!(w, "             {} underrun(s), {} watchdog wake(s)", l.underruns, l.watchdog);
            }
            None => {
                let _ = writeln!(w, "last sound   none since the driver started");
            }
        }
        if !wait::calibrated(self.h.ctx) {
            let _ = writeln!(w, "clock        UNCALIBRATED - every time above is a count of looks, not milliseconds");
        }
        if self.settings_failing {
            let _ = writeln!(w, "settings     the last write of /audio.settings failed");
        }
    }

}

/// `[OK, more, text]` for one page of a debug view: `render` writes the whole view, and the page is cut
/// from it (`wire::Page`).
fn debug_page(out: &mut [u8], page: u8, render: impl FnOnce(&mut wire::Page)) -> usize {
    let text_len = wire::DEBUG_PAGE.min(out.len().saturating_sub(2));
    let (len, more) = {
        let mut w = wire::Page::new(&mut out[2..2 + text_len], page);
        render(&mut w);
        w.finish()
    };
    out[0] = wire::OK;
    out[1] = more as u8;
    2 + len
}

/// The whole widget graph of the codec: every node, its type, capabilities, connections and (for a pin)
/// its configuration - what finds a real codec's output path, not an assumption. `now` is the pin playing.
fn debug_codec(h: &mut Hda, path: &OutPath, now: u32, w: &mut wire::Page) {
    use core::fmt::Write;
    let (cad, afg) = (path.cad, path.afg);
    let _ = writeln!(w, "codec {} - vendor {:04x} device {:04x}, audio function group {:#04x}", cad,
        path.vendor, path.device, afg);
    let Some(ws) = h.param(cad, afg, PARAM_SUB_NODE_COUNT) else {
        let _ = writeln!(w, "the function group did not answer how many widgets it has");
        return;
    };
    let (start, count) = ((ws >> 16) & 0xFF, ws & 0xFF);
    let power = match h.verb(cad, afg, GET_POWER_STATE, 0) {
        Some(ps) => match (ps >> 4) & 0xF { 0 => "D0", 1 => "D1", 2 => "D2", 3 => "D3", _ => "D3cold" },
        None => "no answer",
    };
    let _ = writeln!(w, "widgets      {:#04x}..{:#04x} ({}); function group power {}", start, start + count.max(1) - 1, count, power);
    for nid in start..start + count {
        let Some(caps) = h.param(cad, nid, PARAM_AUDIO_WIDGET_CAP) else {
            let _ = writeln!(w, "node {:#04x}  no answer", nid);
            continue;
        };
        let t = (caps >> 20) & 0xF;
        let name = match t {
            0x0 => "output", 0x1 => "input", 0x2 => "mixer", 0x3 => "selector", 0x4 => "pin",
            0x5 => "power", 0x6 => "volume knob", 0x7 => "beep", 0xF => "vendor", _ => "?",
        };
        let _ = write!(w, "node {:#04x}  {:<11} caps {:#010x}", nid, name, caps);
        if caps & WCAP_OUT_AMP != 0 {
            let amp = h.param(cad, nid, PARAM_OUT_AMP_CAP).unwrap_or(0);
            let _ = write!(w, "  amp-out {:#010x} ({} steps)", amp, (amp >> 8) & 0x7F);
        }
        if t == W_PIN {
            let cfg = h.verb(cad, nid, GET_CONFIG_DEFAULT, 0).unwrap_or(0);
            let pc = h.param(cad, nid, PARAM_PIN_CAP).unwrap_or(0);
            let ctl = h.verb(cad, nid, GET_PIN_WIDGET_CONTROL, 0).unwrap_or(0);
            let _ = write!(w, "  config {:#010x} ({}{})  pincap {:#010x}  control {:#04x}{}", cfg,
                device_name((cfg >> 20) & 0xF), if cfg >> 30 == 0b01 { ", nothing attached" } else { "" }, pc, ctl,
                if nid == now { "  <- playing" } else { "" });
        }
        let mut conns = [0u32; 8];
        let n = connections(h, cad, nid, &mut conns);
        if n > 0 {
            let _ = write!(w, "  from");
            for c in &conns[..n] {
                let _ = write!(w, " {:#04x}", c);
            }
            if n > 1 {
                if let Some(sel) = h.verb(cad, nid, 0xF01, 0) {
                    let _ = write!(w, " (selected {})", sel & 0xFF);
                }
            }
        }
        let _ = writeln!(w);
    }
}

impl<'a> Player<'a> {
    /// The output stream's registers and its buffer descriptors.
    fn debug_stream(&mut self, w: &mut wire::Page) {
        use core::fmt::Write;
        let (m, d, sd) = (self.h.m, self.d, self.sd);
        let ctl = m.read32(sd + SD_CTL);
        let fmt = m.read16(sd + SD_FMT);
        let _ = writeln!(w, "descriptor   {:#06x} (the first output stream)", sd);
        let _ = writeln!(w, "control      {:#08x}: run {}, reset {}, stream tag {}, interrupt on completion {}",
            ctl & 0x00FF_FFFF, ctl & SD_RUN != 0, ctl & SD_SRST != 0, (ctl >> 20) & 0xF, ctl & SD_IOCE != 0);
        let sts = m.read8(sd + SD_STS);
        let _ = writeln!(w, "status       {:#04x}: period done {}, FIFO error {}, descriptor error {}",
            sts, sts & 0x04 != 0, sts & 0x08 != 0, sts & 0x10 != 0);
        let _ = writeln!(w, "position     {} of {} bytes (LPIB of CBL), last valid index {}",
            m.read32(sd + SD_LPIB), m.read32(sd + SD_CBL), m.read16(sd + SD_LVI));
        let _ = writeln!(w, "format       {:#06x}: {} Hz base, {} bits, {} channel(s)", fmt,
            if fmt & 0x4000 != 0 { 44_100 } else { 48_000 },
            match (fmt >> 4) & 0x7 { 0 => 8, 1 => 16, 2 => 20, 3 => 24, 4 => 32, _ => 0 }, (fmt & 0xF) + 1);
        let _ = writeln!(w, "BDL at       {:#010x}{:08x}", m.read32(sd + SD_BDPU), m.read32(sd + SD_BDPL));
        for i in 0..BDL_ENTRIES {
            let e = BDL_OFF + i * 16;
            let _ = writeln!(w, "  entry {}    address {:#012x}, {} bytes, flags {:#x}", i, d.read64(e), d.read32(e + 8), d.read32(e + 12));
        }
        match self.tone.as_ref() {
            Some(t) => {
                let _ = writeln!(w, "ring         {} bytes written, {} played, at {} Hz", t.filled, t.played, t.rate);
            }
            None => {
                let _ = writeln!(w, "ring         idle - nothing playing");
            }
        }
    }
}

/// The last verbs sent to the codec and their answers, oldest first.
fn debug_trace(h: &Hda, w: &mut wire::Page) {
    use core::fmt::Write;
    let n = h.verbs.min(TRACE_LEN as u64);
    let _ = writeln!(w, "the last {} of {} verb(s), oldest first: codec, node, verb, payload -> answer", n, h.verbs);
    for k in 0..n {
        let i = ((h.verbs - n + k) % TRACE_LEN as u64) as usize;
        let t = h.trace[i];
        let v = t.verb;
        // Two verb shapes share the word: a 12-bit verb (always 0x7.. or 0xF..) with an 8-bit payload, and
        // a 4-bit verb (format, amplifier) with a 16-bit payload. Split each the way it was sent.
        if matches!((v >> 16) & 0xF, 0x7 | 0xF) {
            let _ = write!(w, "{:#010x}  codec {} node {:#04x} verb {:#05x} payload {:#04x} -> ",
                v, v >> 28, (v >> 20) & 0x7F, (v >> 8) & 0xFFF, v & 0xFF);
        } else {
            let _ = write!(w, "{:#010x}  codec {} node {:#04x} verb {:#03x} payload {:#06x} -> ",
                v, v >> 28, (v >> 20) & 0x7F, (v >> 16) & 0xF, v & 0xFFFF);
        }
        match t.answer {
            Some(a) => { let _ = writeln!(w, "{:#010x}", a); }
            None => { let _ = writeln!(w, "NO ANSWER"); }
        }
    }
}

/// The controller's global registers: the first look when nothing works.
fn debug_registers(m: &Mmio, w: &mut wire::Page) {
    use core::fmt::Write;
    let gcap = m.read16(GCAP);
    let _ = writeln!(w, "GCAP      {:#06x}: {} output, {} input, {} bidirectional stream(s), 64-bit {}",
        gcap, (gcap >> 12) & 0xF, (gcap >> 8) & 0xF, (gcap >> 3) & 0x1F, gcap & 1 != 0);
    let _ = writeln!(w, "VMAJ.VMIN {}.{}", m.read8(VMAJ), m.read8(VMIN));
    let _ = writeln!(w, "GCTL      {:#010x} (bit 0: out of reset)", m.read32(GCTL));
    let _ = writeln!(w, "STATESTS  {:#06x} (a bit per codec that answered)", m.read16(STATESTS));
    let _ = writeln!(w, "INTCTL    {:#010x}", m.read32(INTCTL));
    let _ = writeln!(w, "INTSTS    {:#010x}", m.read32(0x24));
    let _ = writeln!(w, "WALCLK    {:#010x}", m.read32(0x30));
    let _ = writeln!(w, "CORB      WP {:#06x} RP {:#06x} CTL {:#04x} SIZE {:#04x}",
        m.read16(CORBWP), m.read16(CORBRP), m.read8(CORBCTL), m.read8(CORBSIZE));
    let _ = writeln!(w, "RIRB      WP {:#06x} CTL {:#04x} STS {:#04x} SIZE {:#04x} RINTCNT {}",
        m.read16(RIRBWP), m.read8(RIRBCTL), m.read8(RIRBSTS), m.read8(RIRBSIZE), m.read16(RINTCNT));
    let _ = writeln!(w, "window    {} bytes", m.len());
}

impl<'a> Player<'a> {
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
            wire::OP_OPEN => {
                if !on {
                    out[0] = wire::AUDIO_OFF;
                    return 1;
                }
                if self.tone.is_some() {
                    out[0] = wire::BUSY;
                    return 1;
                }
                if args.len() < 10 {
                    return bad(out);
                }
                let (rate, ch, bits, frames) = (wire::get_u32(args, 0), args[4], args[5], wire::get_u32(args, 6));
                self.open_stream(irq, rate, ch, bits, frames, out)
            }
            wire::OP_DEBUG => {
                let (view, page) = (args.first().copied().unwrap_or(0xFF), args.get(1).copied().unwrap_or(0));
                if view as usize >= wire::DEBUG_VIEWS.len() || page >= wire::DEBUG_PAGES_MAX {
                    return bad(out);
                }
                self.debug(irq, view, page, out)
            }
            wire::OP_OUTPUTS => self.outputs_answer(out),
            wire::OP_OUTPUT => {
                let Some(&pin) = args.first() else { return bad(out) };
                let Some(p) = self.outputs.by_pin(pin as u32) else { return bad(out) };
                if p.nodes[0] == self.path.nodes[0] {
                    out[0] = wire::ALREADY;
                    out[1] = wire::VERIFIED;
                    return 2;
                }
                if !on {
                    out[0] = wire::AUDIO_OFF;
                    return 1;
                }
                if self.tone.is_some() {
                    out[0] = wire::BUSY;
                    return 1;
                }
                out[0] = wire::OK;
                out[1] = self.select_output(p);
                if out[1] != wire::CONTRADICTED {
                    self.output_chosen = Some(p.dev as u8);
                    self.settings_dirty = true;
                }
                2
            }
            wire::OP_PCM => self.feed_pcm(args, out),
            wire::OP_END => self.end_feed(out),
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
    gs::trace::as_name(&ctx, "audio-driver");
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
    let mut h = Hda::new(ctx, m);
    // Every codec is surveyed, so the log names each; the outputs offered are the first codec's that has
    // any - one codec plays at a time.
    let mut outputs = Outputs { paths: [None; wire::OUTPUTS_MAX], n: 0 };
    let mut other = Outputs { paths: [None; wire::OUTPUTS_MAX], n: 0 };
    for cad in 0..15u32 {
        if codecs & (1 << cad) != 0 {
            let into = if outputs.n == 0 { &mut outputs } else { &mut other };
            survey_codec(&mut h, cad, into);
        }
    }
    let Some(path) = outputs.get(0).copied() else { return Device::Absent(wire::no_device::NO_PATH) };

    // From here on, DMA - on the codec it was verified on, and nowhere else (see the module docs).
    if path.vendor != VENDOR_QEMU {
        ctx.log_fmt(format_args!(
            "audio-driver: codec vendor {:04x} is not the one playback was verified on - stopping after the survey (docs/audio.md, A6)",
            path.vendor));
        return Device::Surveyed(wire::no_device::UNVERIFIED_CODEC, h, path);
    }
    let Some(d) = dma else {
        ctx.log("audio-driver: no DMA arena was granted - the survey is all this driver can do");
        return Device::Surveyed(wire::no_device::NO_ARENA, h, path);
    };
    if d.len() < ARENA_NEEDED {
        ctx.log_fmt(format_args!(
            "audio-driver: the DMA arena is {} bytes and this driver needs {} - stopping after the survey",
            d.len(), ARENA_NEEDED));
        return Device::Surveyed(wire::no_device::NO_ARENA, h, path);
    }
    d.zero();
    if !start_rings(&mut h, d) {
        return Device::Surveyed(wire::no_device::BRINGUP_FAILED, h, path);
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
            return Device::Surveyed(wire::no_device::BRINGUP_FAILED, h, path);
        }
    }
    if !configure_path(&mut h, &path) {
        ctx.log("audio-driver: the output path could not be configured");
        return Device::Surveyed(wire::no_device::BRINGUP_FAILED, h, path);
    }
    let amp = volume_amp(&mut h, &path);
    let pin_device = h.verb(path.cad, path.nodes[0], GET_CONFIG_DEFAULT, 0).map_or(0, |c| (c >> 20) & 0xF);
    let iss = ((m.read16(GCAP) >> 8) & 0xF) as usize;
    let mut p = Player {
        h, d, path, outputs, output_chosen: None, pin_device, amp,
        sd: SD_BASE + iss * SD_STRIDE, // the first output stream follows the input streams
        power: wire::POWER_ON, volume: DEFAULT_VOLUME, muted: false, tone: None, underruns_total: 0,
        last_silence_ms: 0, last: None,
        settings_dirty: false, settings_failing: false,
    };
    if let Some(s) = settings::load(ctx, &mut gs::fs::Fs::new(ctx).patience_secs(settings::PATIENCE_SECS), "audio-driver", DEFAULT_VOLUME) {
        p.volume = s.volume;
        p.muted = s.muted;
        // The output chosen last time, if this codec still has one of that kind; said either way.
        if let Some(dev) = s.output {
            p.output_chosen = Some(dev);
            let want = (0..p.outputs.n).filter_map(|i| p.outputs.get(i).copied()).find(|o| o.dev == dev as u32);
            match want {
                Some(o) if o.nodes[0] != p.path.nodes[0] => {
                    let v = p.select_output(o);
                    ctx.log_fmt(format_args!("audio-driver: output {} restored from /audio.settings - {}",
                        device_name(o.dev), verdict_word(v)));
                }
                Some(_) => {}
                None => ctx.log_fmt(format_args!(
                    "audio-driver: /audio.settings asks for the {} output and this codec has none - playing through {}",
                    device_name(dev as u32), device_name(p.path.dev))),
            }
        }
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
        Device::Surveyed(r, ..) => ctx.log_fmt(format_args!(
            "audio-driver: serving with no device to play on (reason {}) - every request is answered with that, and `audio debug` still shows the codec, the verbs and the registers", r)),
    }
    let mut out = [0u8; 4096];
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
                Device::Surveyed(r, h, path) => {
                    let b = req.payload_bytes();
                    let (op, view, page) = (b.get(2).copied().unwrap_or(0), b.get(3).copied().unwrap_or(0xFF), b.get(4).copied().unwrap_or(0));
                    if op == wire::OP_DEBUG && matches!(view, wire::DEBUG_CODEC | wire::DEBUG_TRACE | wire::DEBUG_REGISTERS)
                        && page < wire::DEBUG_PAGES_MAX {
                        2 + debug_page(&mut out[2..], page, |w| match view {
                            wire::DEBUG_CODEC => debug_codec(h, path, 0xFF, w),
                            wire::DEBUG_TRACE => debug_trace(h, w),
                            _ => debug_registers(h.m, w),
                        })
                    } else {
                        out[2] = wire::NO_DEVICE;
                        out[3] = *r;
                        4
                    }
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
                match settings::save(&mut gs::fs::Fs::new(ctx).patience_secs(settings::PATIENCE_SECS), Settings { volume: p.volume, muted: p.muted, output: p.output_chosen }) {
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
