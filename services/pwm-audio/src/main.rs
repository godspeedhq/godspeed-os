// SPDX-License-Identifier: GPL-2.0-only
//! `pwm-audio`: the Raspberry Pi's 3.5 mm jack, driven by PWM and fed by the SoC's DMA engine
//! (`docs/audio.md`, "The Pis").
//!
//! The Pi has no audio codec. Its jack is two PWM channels through an RC filter: each sample is a duty
//! cycle, written as a 32-bit word into the PWM's FIFO, and the DMA engine moves the words there as the
//! PWM asks for them. Learned from Circle (`lib/sound/pwmsoundbasedevice.cpp`, `dmasoundbuffers.cpp`)
//! and the BCM2835 / BCM2711 datasheets; the values are the silicon's, the design is ours (26.14).
//!
//! **What the kernel did before this service ran**: routed the jack's two pins to the PWM and started
//! the PWM clock - both in SHARED blocks, so they are part of the grant rather than this driver's to
//! touch (CLAUDE.md 12.3, as amended for audio). This service is granted an 8 KiB window - the PWM page
//! at +0 and the DMA engine's page at +0x1000 - and a DMA arena.
//!
//! **The DMA engine never stops while audio is on.** It loops over a ring of control blocks, one per
//! period, and when nothing plays the ring holds the PWM's mid-scale - silence - so starting and stopping
//! a sound never clicks. Playing is writing samples into the ring ahead of where the engine is reading,
//! exactly as `audio-driver` writes ahead of an HD Audio stream. **No interrupt**: the DMA engine's lines
//! are shared between channels, and routing one would hand over the others', so the driver reads the
//! engine's position every 10 ms through `gs::driver::irq`'s no-interrupt path. The ring is about 370 ms,
//! which is the margin a polled refill needs.
//!
//! **Speaks the same protocol as `audio-driver`** (`sdk/audio`), so the shell's `audio` works the same on
//! every board. Volume is applied to the samples: the PWM has no amplifier.
//!
//! **Which board** comes from the supervisor's spawn row (`probe_mode`: 2 for the Pi 2, 4 for the Pi 4),
//! declared once in its per-board build facts - not guessed here from the instruction set.
#![no_std]
#![no_main]
#![deny(unsafe_code)]

use godspeed as gs;
use godspeed::driver::delay;
use godspeed::driver::irq::{Irq, Woke};
use godspeed::driver::wait::{self, Budget};
use godspeed_audio::settings::{self, Settings};
use godspeed_audio::sine::Sine;
use godspeed_audio::sounds::{self, Chime};
use godspeed_audio::wire;
use godspeed_sdk::mmio::Mmio;
use godspeed_sdk::{Dma, Message, ServiceContext};

// ---- The window: the PWM page at +0, the DMA engine's page at +0x1000 ------------------------------
const WINDOW: usize = 0x2000;
const PWM_CTL: usize = 0x00;
const PWM_STA: usize = 0x04;
const PWM_DMAC: usize = 0x08;
const PWM_RNG1: usize = 0x10;
const PWM_RNG2: usize = 0x20;
/// Both channels on, both fed from the FIFO, mark-space mode, FIFO cleared (PWEN1|USEF1|CLRF1|MSEN1|
/// PWEN2|USEF2|MSEN2). The FIFO feeds the two channels alternately.
const PWM_CTL_RUN: u32 = 0x0000_A1E1;
const PWM_CTL_ENABLED: u32 = (1 << 0) | (1 << 8);
/// DMA enabled, PANIC and DREQ thresholds 7 (the datasheet's defaults, and Circle's).
const PWM_DMAC_RUN: u32 = 0x8000_0707;

const DMA_PAGE: usize = 0x1000;
const DMA_CH_STRIDE: usize = 0x100;
const DMA_CS: usize = 0x00;
const DMA_CONBLK_AD: usize = 0x04;
const DMA_SOURCE_AD: usize = 0x0C;
const DMA_ENABLE: usize = 0xFF0;
const DMA_CS_RESET: u32 = 1 << 31;
const DMA_CS_ACTIVE: u32 = 1 << 0;
const DMA_CS_ERROR: u32 = 1 << 8;
/// Start: wait for outstanding writes, panic priority 15, priority 1, ACTIVE.
const DMA_CS_START: u32 = 0x10F1_0001;
/// Transfer information for every control block, less the DREQ: 128-bit source reads, source
/// increments, the destination paced by the PWM's DREQ, wait for write responses (Circle's 0x349 less
/// INTEN, since no interrupt is routed).
const DMA_TI_BASE: u32 = 0x0000_0348;
/// RAM as the DMA engine sees it: the uncached alias, on both boards. The Pi 4's engine reaches only the
/// first 1 GiB this way, which is why the arena must sit below it.
const DMA_BUS_RAM: u32 = 0xC000_0000;
const DMA_REACH: u64 = 0x4000_0000;

// ---- The arena: control blocks, then the ring ----------------------------------------------------------
const PERIODS: usize = 16;
/// One period: 2048 words, 1024 stereo frames, about 23 ms at 44.1 kHz - Circle's chunk.
const PERIOD_BYTES: usize = 8192;
const RING_OFF: usize = 0x1000;
const RING_BYTES: usize = PERIODS * PERIOD_BYTES;
/// A frame is two 32-bit words: RIGHT then LEFT (both Pis wire channel 1 to the right).
const FRAME: usize = 8;
const RING_FRAMES: usize = RING_BYTES / FRAME;
const ARENA_NEEDED: usize = RING_OFF + RING_BYTES;
/// How far ahead of the engine's position the driver keeps written: it reads ahead in bursts.
const GUARD: usize = 256;

// ---- Timing ----------------------------------------------------------------------------------------
/// How long a channel reset has to clear.
const DMA_RESET_WAIT: Budget = Budget::ms(10);
/// The pacing check: one period, about 23 ms at 44.1 kHz when the PWM's requests pace the engine. A
/// period finished in under `PACED_MIN_US` was not paced at all - QEMU's DMA model runs a transfer to its
/// end inside the write that starts it, and a LOOPING chain there never ends, which froze the whole
/// emulated machine (2026-10-03). One that has not finished by `PACED_WAIT` was never asked for data:
/// the PWM's clock is not running.
const PACED_MIN_US: u64 = 10_000;
const PACED_WAIT: Budget = Budget::ms(200);
const DMA_CS_END: u32 = 1 << 1;
/// The PWM settles after its range and control change (Circle waits 2 ms after each).
const PWM_SETTLE: Budget = Budget::ms(2);
/// How often the ring is refilled while something plays: the ring holds about 370 ms.
const REFILL_PACE: Budget = Budget::ms(10);
/// When nothing plays there is nothing to do; the wait is bounded because every `irq::wait` is.
const SERVE_WAIT: Budget = Budget::ms(3_600_000);
/// A stream's sender has this long between sends before the stream is ended for it.
const FEED_TIMEOUT_MS: u32 = 5000;

const RATE_DEFAULT: u32 = 44_100;
const DEFAULT_VOLUME: u8 = 50;

/// The jack, as `audio outputs` lists it: one output, named by the same default-device field an HD Audio
/// pin carries (2, headphone). It has no node; 0 stands for it in `OP_OUTPUT`.
const JACK_PIN: u8 = 0;
const JACK_DEVICE: u8 = 2;

/// What differs between the two boards. Everything else is the same driver.
#[derive(Clone, Copy)]
struct Board {
    name: &'static str,
    /// The jack's PWM block within the PWM page: PWM0 at +0 on the Pi 2, PWM1 at +0x800 on the Pi 4.
    pwm: usize,
    /// The PWM FIFO as the DMA engine addresses it.
    fifo_bus: u32,
    /// The DMA request line the PWM drives, and the channel this driver takes (free for the ARM in the
    /// firmware's channel mask on each board: 0x7f35 on the Pi 2, 0x07f5 on the Pi 4).
    dreq: u32,
    channel: usize,
    /// The PWM clock the kernel started: PLLD / 2 on the Pi 2, PLLD / 6 on the Pi 4.
    clock_hz: u32,
}

const PI2: Board = Board { name: "Pi 2", pwm: 0x000, fifo_bus: 0x7E20_C018, dreq: 5, channel: 11, clock_hz: 250_000_000 };
const PI4: Board = Board { name: "Pi 4", pwm: 0x800, fifo_bus: 0x7E20_C818, dreq: 1, channel: 7, clock_hz: 125_000_000 };

/// What is playing: a tone, or a stream a sender feeds.
struct Play {
    sine: Sine,
    hz: u32,
    ms: u32,
    /// Frames of tone not yet written (tones only).
    left: usize,
    /// Frames in all, for a tone; for a stream, what the sender said it would send.
    frames: usize,
    filled: usize,
    played: usize,
    /// The ring position at the last look.
    last: usize,
    /// Where this sound began, and (for a tone, once its last frame is written) where it ends - both in
    /// the same running frame count as `played`.
    begin: usize,
    end_at: usize,
    underruns: u32,
    silence: usize,
    started: u64,
    feed: Option<Feed>,
    /// A system sound (`OP_SOUND`), written whole before the engine reached it.
    sound: bool,
}

struct Feed {
    channels: u8,
    ended: bool,
    last_feed: u64,
}

struct Pwm<'a> {
    ctx: &'a ServiceContext,
    m: &'a Mmio,
    d: &'a Dma,
    b: Board,
    rate: u32,
    range: u32,
    power: u8,
    volume: u8,
    muted: bool,
    play: Option<Play>,
    underruns_total: u32,
    last_silence_ms: u32,
    settings_dirty: bool,
    settings_failing: bool,
    /// The pacing check at the last start (`start`): how long one period of silence took, and how long a
    /// PWM at the set rate should take - the measured rate `audio debug stats` shows. `None` before one.
    paced: Option<(u64, u64)>,
    /// The system sounds (`audio system sounds on|off`), and when the last one started.
    system_sounds: bool,
    last_sound: Option<u64>,
}

enum Device<'a> {
    Ready(Pwm<'a>),
    Absent(u8),
}

fn ms_since(ctx: &ServiceContext, t0: u64) -> u32 {
    match wait::ticks_per_10ms(ctx) / 10 {
        0 => 0,
        per => (wait::ticks(ctx).wrapping_sub(t0) / per).min(u32::MAX as u64) as u32,
    }
}

impl<'a> Pwm<'a> {
    fn ch(&self) -> usize {
        DMA_PAGE + self.b.channel * DMA_CH_STRIDE
    }

    fn bus(&self, off: usize) -> u32 {
        DMA_BUS_RAM | (self.d.phys_at(off) as u32 & 0x3FFF_FFFF)
    }

    fn cb(&self, i: usize) -> usize {
        i * 32
    }

    /// The PWM word for a sample: the volume applied (a square law, so equal steps sound like equal
    /// changes), then mapped onto the PWM's range with silence at mid-scale.
    fn word(&self, s: i16) -> u32 {
        let gain = if self.muted { 0 } else { (self.volume as i64 * self.volume as i64 * 65_536) / 10_000 };
        let v = ((s as i64 * gain) >> 16) + 32_768;
        ((v.clamp(0, 65_535) as u64 * (self.range as u64 - 1)) >> 16) as u32
    }

    fn silence_word(&self) -> u32 {
        (self.range - 1) / 2
    }

    /// Write one frame at ring position `at` (frames, any count - wrapped here).
    fn put(&self, at: usize, left: i16, right: i16) {
        let off = RING_OFF + (at % RING_FRAMES) * FRAME;
        self.d.write32(off, self.word(right));
        self.d.write32(off + 4, self.word(left));
    }

    fn put_silence(&self, from: usize, frames: usize) {
        let w = self.silence_word();
        for i in 0..frames {
            let off = RING_OFF + ((from + i) % RING_FRAMES) * FRAME;
            self.d.write32(off, w);
            self.d.write32(off + 4, w);
        }
    }

    /// Where the engine is reading in the ring, in frames, from the channel's source address.
    fn position(&self) -> usize {
        let src = self.m.read32(self.ch() + DMA_SOURCE_AD);
        let base = self.bus(RING_OFF);
        (src.wrapping_sub(base) as usize % RING_BYTES) / FRAME
    }

    /// The PWM at `rate`: range = clock / rate, both channels from the FIFO, then the DMA request on.
    fn set_rate(&mut self, rate: u32) {
        let m = self.m;
        let pwm = self.b.pwm;
        self.rate = rate;
        self.range = (self.b.clock_hz + rate / 2) / rate;
        m.write32(pwm + PWM_RNG1, self.range);
        m.write32(pwm + PWM_RNG2, self.range);
    }

    /// Bring the jack up: PWM off, the DMA channel reset and enabled, the PWM set to `rate`, the ring
    /// silent, the control blocks chained into a loop, and the engine started. Returns whether the PWM and
    /// the channel say they are running.
    fn start(&mut self, rate: u32) -> bool {
        let (ctx, m, d, pwm, ch) = (self.ctx, self.m, self.d, self.b.pwm, self.ch());
        m.write32(pwm + PWM_DMAC, 0);
        m.write32(pwm + PWM_CTL, 0);
        m.write32(DMA_PAGE + DMA_ENABLE, m.read32(DMA_PAGE + DMA_ENABLE) | 1 << self.b.channel);
        m.write32(ch + DMA_CS, DMA_CS_RESET);
        if wait::until(ctx, DMA_RESET_WAIT, || m.read32(ch + DMA_CS) & DMA_CS_RESET == 0).is_err() {
            ctx.log_fmt(format_args!("pwm-audio: DMA channel {} did not leave reset", self.b.channel));
            return false;
        }
        self.set_rate(rate);
        m.write32(pwm + PWM_CTL, PWM_CTL_RUN);
        delay::hold(ctx, PWM_SETTLE);
        // THE PWM MUST BE THERE BEFORE THE DMA ENGINE IS POINTED AT IT. A block that ignores writes and
        // reads zero - QEMU's `raspi2b` models the PWM as exactly that - would leave the engine looping
        // a ring into nothing; in QEMU, whose DMA runs a transfer to its end inside the write that
        // starts it, that loop froze the whole machine (2026-10-03). So the PWM's enable bits are read
        // back first, and the engine is started only when they hold.
        let ctl = m.read32(pwm + PWM_CTL);
        if ctl & PWM_CTL_ENABLED != PWM_CTL_ENABLED {
            ctx.log_fmt(format_args!(
                "pwm-audio: the PWM did not take its enable (CTL reads {:#010x} after {:#010x} was written) - not starting the DMA engine",
                ctl, PWM_CTL_RUN));
            m.write32(pwm + PWM_CTL, 0);
            return false;
        }
        self.put_silence(0, RING_FRAMES);
        let ti = DMA_TI_BASE | self.b.dreq << 16;

        // PACING CHECK, before anything loops: one period of silence through ONE control block that ends.
        // A real PWM asks for its words at the sample rate, so this takes a period's time; that proves the
        // clock, the pins' PWM, the DMA request line and the engine together, and measures the rate.
        let cb0 = self.cb(0);
        d.write32(cb0, ti);
        d.write32(cb0 + 4, self.bus(RING_OFF));
        d.write32(cb0 + 8, self.b.fifo_bus);
        d.write32(cb0 + 12, PERIOD_BYTES as u32);
        d.write32(cb0 + 16, 0);
        d.write32(cb0 + 20, 0); // NEXTCONBK 0: this chain ENDS
        m.write32(pwm + PWM_DMAC, PWM_DMAC_RUN);
        m.write32(ch + DMA_CONBLK_AD, self.bus(cb0));
        m.write32(ch + DMA_CS, DMA_CS_START);
        let took = wait::until(ctx, PACED_WAIT, || m.read32(ch + DMA_CS) & DMA_CS_ACTIVE == 0);
        m.write32(ch + DMA_CS, DMA_CS_END); // write-1-to-clear
        let expected_us = (PERIOD_BYTES / FRAME) as u64 * 1_000_000 / rate as u64;
        match took {
            Err(_) => {
                ctx.log_fmt(format_args!(
                    "pwm-audio: one period was not taken within {} ms - the PWM never asked for data (is its clock running?); not starting the ring",
                    PACED_WAIT.as_us() / 1000));
                m.write32(ch + DMA_CS, DMA_CS_RESET);
                m.write32(pwm + PWM_DMAC, 0);
                m.write32(pwm + PWM_CTL, 0);
                return false;
            }
            Ok(us) if us < PACED_MIN_US && wait::calibrated(ctx) => {
                ctx.log_fmt(format_args!(
                    "pwm-audio: one period went in {} us where a paced PWM takes {} us - the DMA engine is not paced by the PWM's requests (QEMU's model is not), so no ring is looped into it",
                    us, expected_us));
                m.write32(pwm + PWM_DMAC, 0);
                m.write32(pwm + PWM_CTL, 0);
                return false;
            }
            Ok(us) => {
                self.paced = Some((us, expected_us));
                ctx.log_fmt(format_args!(
                    "pwm-audio: paced - one period of {} frames took {} us (expected {} us at {} Hz)",
                    PERIOD_BYTES / FRAME, us, expected_us, rate));
            }
        }

        for i in 0..PERIODS {
            let cb = self.cb(i);
            d.write32(cb, ti);
            d.write32(cb + 4, self.bus(RING_OFF + i * PERIOD_BYTES));
            d.write32(cb + 8, self.b.fifo_bus);
            d.write32(cb + 12, PERIOD_BYTES as u32);
            d.write32(cb + 16, 0);
            d.write32(cb + 20, self.bus(self.cb((i + 1) % PERIODS)));
            d.write32(cb + 24, 0);
            d.write32(cb + 28, 0);
        }
        m.write32(pwm + PWM_DMAC, PWM_DMAC_RUN);
        m.write32(ch + DMA_CONBLK_AD, self.bus(self.cb(0)));
        m.write32(ch + DMA_CS, DMA_CS_START);
        let cs = m.read32(ch + DMA_CS);
        if cs & DMA_CS_ERROR != 0 || cs & DMA_CS_ACTIVE == 0 {
            ctx.log_fmt(format_args!("pwm-audio: DMA channel {} is not running cleanly after start (CS {:#010x})", self.b.channel, cs));
        }
        m.read32(pwm + PWM_CTL) & PWM_CTL_ENABLED == PWM_CTL_ENABLED
    }

    /// Stop the engine and the PWM. Returns whether the PWM reads back as off.
    fn stop_all(&mut self) -> bool {
        let (m, pwm, ch) = (self.m, self.b.pwm, self.ch());
        m.write32(ch + DMA_CS, 0); // ACTIVE clear: the channel pauses
        m.write32(pwm + PWM_DMAC, 0);
        m.write32(pwm + PWM_CTL, 0);
        m.read32(pwm + PWM_CTL) & PWM_CTL_ENABLED == 0
    }

    fn free_frames(&self) -> u32 {
        match self.play.as_ref() {
            Some(p) => ((p.played + RING_FRAMES - GUARD).saturating_sub(p.filled)) as u32,
            None => 0,
        }
    }

    /// Begin playing at `rate`: where the engine is reading now. Writing starts a guard ahead of it.
    fn begin(&mut self, rate: u32) -> usize {
        if rate != self.rate {
            self.set_rate(rate);
            self.put_silence(0, RING_FRAMES);
        }
        self.position()
    }

    fn start_tone(&mut self, hz: u32, ms: u32) {
        let at = self.begin(RATE_DEFAULT);
        let frames = RATE_DEFAULT as usize * ms as usize / 1000;
        self.play = Some(Play {
            sine: Sine::new(hz, RATE_DEFAULT), hz, ms, left: frames, frames, filled: at + GUARD, played: at,
            last: at, begin: at + GUARD, end_at: usize::MAX, underruns: 0, silence: 0,
            started: wait::ticks(self.ctx), feed: None, sound: false,
        });
        self.ctx.log_fmt(format_args!("pwm-audio: playing {} Hz for {} ms", hz, ms));
        self.service();
    }

    /// A system sound (`wire::OP_SOUND`), written whole a guard ahead of the engine - every one is far
    /// shorter than the ring - then played out by `service` like a tone whose sine has run out. Played only
    /// when the sounds are on, audio is on, nothing else plays and `SOUND_GAP_MS` has passed.
    fn start_sound(&mut self, kind: u8) -> bool {
        let ctx = self.ctx;
        let frames = sounds::frames(kind, RATE_DEFAULT);
        let gap_ok = self.last_sound.map_or(true, |t| ms_since(ctx, t) >= wire::SOUND_GAP_MS);
        if !self.system_sounds || self.power != wire::POWER_ON || self.play.is_some() || !gap_ok
            || frames == 0 || frames + 2 * GUARD > RING_FRAMES {
            return false;
        }
        let Some(mut chime) = Chime::new(kind, RATE_DEFAULT) else { return false };
        let at = self.begin(RATE_DEFAULT);
        let start = at + GUARD;
        for i in 0..frames {
            let v = chime.next().unwrap_or(0);
            self.put(start + i, v, v);
        }
        self.play = Some(Play {
            sine: Sine::new(1, RATE_DEFAULT), hz: 0, ms: (frames as u64 * 1000 / RATE_DEFAULT as u64) as u32,
            left: 0, frames, filled: start + frames, played: at, last: at, begin: start, end_at: start + frames,
            underruns: 0, silence: 0, started: wait::ticks(ctx), feed: None, sound: true,
        });
        self.last_sound = Some(wait::ticks(ctx));
        true
    }

    fn open_stream(&mut self, rate: u32, channels: u8, bits: u8, frames: u32, out: &mut [u8]) -> usize {
        if bits != 16 || (channels != 1 && channels != 2) || (rate != 44_100 && rate != 48_000) {
            out[0] = wire::FORMAT;
            out[1] = if bits != 16 { wire::format::BITS } else if channels != 1 && channels != 2 {
                wire::format::CHANNELS } else { wire::format::RATE };
            return 2;
        }
        let at = self.begin(rate);
        let ctx = self.ctx;
        self.play = Some(Play {
            sine: Sine::new(1, rate), hz: 0, ms: (frames as u64 * 1000 / rate as u64) as u32, left: 0,
            // HALF A RING AHEAD: the engine never stops, so a stream that began a guard ahead of it
            // would run dry on the sender's first slow read. This is the cushion `audio-driver` gets by
            // not starting its stream until half the ring is written - about 185 ms before a sound is
            // heard, the price of not stuttering at its start.
            frames: frames as usize, filled: at + RING_FRAMES / 2, played: at, last: at, begin: at + RING_FRAMES / 2,
            end_at: usize::MAX, underruns: 0, silence: 0,
            started: wait::ticks(ctx), feed: Some(Feed { channels, ended: false, last_feed: wait::ticks(ctx) }),
            sound: false,
        });
        ctx.log_fmt(format_args!("pwm-audio: stream opened - {} Hz, {} channel(s), {} frames", rate, channels, frames));
        out[0] = wire::OK;
        wire::put_u32(out, 1, self.free_frames());
        5
    }

    fn feed_pcm(&mut self, data: &[u8], out: &mut [u8]) -> usize {
        let free = self.free_frames() as usize;
        let now = wait::ticks(self.ctx);
        let (channels, filled) = match self.play.as_ref() {
            Some(Play { feed: Some(f), filled, .. }) => (f.channels as usize, *filled),
            _ => { out[0] = wire::NOT_OPEN; return 1; }
        };
        let per = 2 * channels;
        let n = (data.len() / per).min(free);
        for i in 0..n {
            let l = i16::from_le_bytes([data[i * per], data[i * per + 1]]);
            let r = if channels == 2 { i16::from_le_bytes([data[i * per + 2], data[i * per + 3]]) } else { l };
            self.put(filled + i, l, r);
        }
        if let Some(p) = self.play.as_mut() {
            p.filled += n;
            if let Some(f) = p.feed.as_mut() {
                f.last_feed = now;
            }
        }
        out[0] = wire::OK;
        wire::put_u32(out, 1, n as u32);
        wire::put_u32(out, 5, self.free_frames());
        9
    }

    fn end_feed(&mut self, out: &mut [u8]) -> usize {
        match self.play.as_mut() {
            Some(Play { feed: Some(f), .. }) => f.ended = true,
            _ => { out[0] = wire::NOT_OPEN; return 1; }
        }
        out[0] = wire::OK;
        1
    }

    /// Keep what plays fed, as `audio-driver` does: a tone refilled with sine, a stream padded with
    /// silence where its sender fell behind (counted), and either one ended with the ring silenced -
    /// the engine keeps reading it, so silence is how a sound stops.
    fn service(&mut self) {
        let ctx = self.ctx;
        let pos = self.position();
        let Some(mut p) = self.play.take() else { return };
        p.played += (pos + RING_FRAMES - p.last) % RING_FRAMES;
        p.last = pos;
        let mut done = false;
        if let Some(f) = p.feed.as_mut() {
            if !f.ended && ms_since(ctx, f.last_feed) > FEED_TIMEOUT_MS {
                ctx.log("pwm-audio: the stream's sender stopped sending - playing out what it sent");
                f.ended = true;
            }
            if f.ended {
                let free = (p.played + RING_FRAMES).saturating_sub(p.filled);
                self.put_silence(p.filled, free);
                done = p.played >= p.filled;
            } else if p.filled < p.played + GUARD {
                let pad = p.played + GUARD - p.filled;
                self.put_silence(p.filled, pad);
                p.filled += pad;
                p.silence += pad;
                p.underruns += 1;
            }
        } else {
            if p.played > p.filled {
                p.underruns += 1;
                p.filled = p.played;
            }
            let room = (p.played + RING_FRAMES - GUARD).saturating_sub(p.filled);
            for i in 0..room {
                if p.left > 0 {
                    p.left -= 1;
                    let s = p.sine.next();
                    self.put(p.filled + i, s, s);
                    if p.left == 0 {
                        p.end_at = p.filled + i + 1;
                    }
                } else {
                    let w = self.silence_word();
                    let off = RING_OFF + ((p.filled + i) % RING_FRAMES) * FRAME;
                    self.d.write32(off, w);
                    self.d.write32(off + 4, w);
                }
            }
            p.filled += room;
            // A tone has played out when the engine has read past its last frame.
            done = p.played >= p.end_at;
        }
        if done {
            let took = ms_since(ctx, p.started);
            let sil = (p.silence as u64 * 1000 / self.rate as u64) as u32;
            match p.feed {
                Some(_) => ctx.log_fmt(format_args!(
                    "pwm-audio: played a stream of {} frames at {} Hz in {} ms by the clock, {} underrun(s), {} ms of silence",
                    p.frames, self.rate, took, p.underruns, sil)),
                // A system sound ends without a line: it is a few hundred milliseconds of feedback, and a
                // line for each would fill the log with every typing mistake.
                None if p.sound => {}
                None => ctx.log_fmt(format_args!(
                    "pwm-audio: played {} Hz for {} ms in {} ms by the clock, {} underrun(s)", p.hz, p.ms, took, p.underruns)),
            }
            self.put_silence(0, RING_FRAMES);
            self.underruns_total = self.underruns_total.saturating_add(p.underruns);
            if p.feed.is_some() {
                self.last_silence_ms = sil;
            }
            return;
        }
        self.play = Some(p);
    }

    /// Stop what plays now: the ring silenced at once. Returns whether anything played and for how long.
    fn stop_play(&mut self, why: &str) -> (bool, u32) {
        let Some(p) = self.play.take() else { return (false, 0) };
        let pos = self.position();
        let played = p.played + (pos + RING_FRAMES - p.last) % RING_FRAMES;
        // Heard, by the engine's position: from where the sound began to where it is now.
        let ms = (played.saturating_sub(p.begin) as u64 * 1000 / self.rate as u64).min(p.ms as u64) as u32;
        self.put_silence(0, RING_FRAMES);
        self.underruns_total = self.underruns_total.saturating_add(p.underruns);
        self.ctx.log_fmt(format_args!("pwm-audio: stopped after {} ms ({})", ms, why));
        (true, ms)
    }

    /// `audio debug` (`wire::OP_DEBUG`): the jack's account of itself, one page of one view. There is no
    /// codec and no command trace on a PWM jack, and those views say so in a line rather than failing.
    fn debug(&self, view: u8, page: u8, out: &mut [u8]) -> usize {
        use core::fmt::Write;
        let text_len = wire::DEBUG_PAGE.min(out.len().saturating_sub(2));
        let (len, more) = {
            let mut w = wire::Page::new(&mut out[2..2 + text_len], page);
            let (m, pwm, ch) = (self.m, self.b.pwm, self.ch());
            match view {
                wire::DEBUG_STATS => {
                    let _ = writeln!(w, "device       the 3.5 mm jack on the {}: PWM fed by DMA channel {}", self.b.name, self.b.channel);
                    let _ = writeln!(w, "clock        {} Hz; range {} steps per sample at {} Hz", self.b.clock_hz, self.range, self.rate);
                    match self.paced {
                        Some((us, want)) => {
                            let pm = if us == 0 { 0 } else { want * 1000 / us };
                            let _ = writeln!(w, "pacing       one period took {} us, expected {} us - the PWM asks at {}.{}% of the set rate",
                                us, want, pm / 10, pm % 10);
                        }
                        None => { let _ = writeln!(w, "pacing       not measured yet"); }
                    }
                    let _ = writeln!(w, "underruns    {} since the driver started", self.underruns_total);
                    match self.play.as_ref() {
                        Some(p) => {
                            let _ = writeln!(w, "playing      {}, {} frame(s) written ahead of the engine",
                                if p.feed.is_some() { "a stream" } else { "a tone" }, p.filled.saturating_sub(p.played));
                        }
                        None => { let _ = writeln!(w, "playing      nothing"); }
                    }
                    if !wait::calibrated(self.ctx) {
                        let _ = writeln!(w, "clock        UNCALIBRATED - the pacing above is a count of looks, not microseconds");
                    }
                }
                wire::DEBUG_CODEC => { let _ = writeln!(w, "no codec: the jack is driven by PWM through the board's filter, not by an HD Audio codec"); }
                wire::DEBUG_TRACE => { let _ = writeln!(w, "no command trace: a PWM jack takes no codec commands"); }
                wire::DEBUG_STREAM => {
                    let cs = m.read32(ch + DMA_CS);
                    let _ = writeln!(w, "DMA CS       {:#010x}: active {}, error {}", cs, cs & DMA_CS_ACTIVE != 0, cs & DMA_CS_ERROR != 0);
                    let _ = writeln!(w, "control blk  {:#010x}", m.read32(ch + DMA_CONBLK_AD));
                    let _ = writeln!(w, "source       {:#010x} - frame {} of the {}-frame ring", m.read32(ch + DMA_SOURCE_AD), self.position(), RING_FRAMES);
                    let _ = writeln!(w, "ring         {} periods of {} bytes", PERIODS, PERIOD_BYTES);
                }
                _ => {
                    let _ = writeln!(w, "PWM CTL      {:#010x}", m.read32(pwm + PWM_CTL));
                    let _ = writeln!(w, "PWM STA      {:#010x}", m.read32(pwm + PWM_STA));
                    let _ = writeln!(w, "PWM DMAC     {:#010x}", m.read32(pwm + PWM_DMAC));
                    let _ = writeln!(w, "PWM RNG1/2   {} / {}", m.read32(pwm + PWM_RNG1), m.read32(pwm + PWM_RNG2));
                    let _ = writeln!(w, "DMA CS       {:#010x}", m.read32(ch + DMA_CS));
                    let _ = writeln!(w, "DMA ENABLE   {:#010x} (channel {})", m.read32(DMA_PAGE + DMA_ENABLE), self.b.channel);
                }
            }
            w.finish()
        };
        out[0] = wire::OK;
        out[1] = more as u8;
        2 + len
    }

    fn status(&self, out: &mut [u8]) -> usize {
        out[0] = wire::OK;
        out[1] = self.power;
        out[2] = self.muted as u8;
        out[3] = self.volume;
        let (playing, hz, len, elapsed, under, sil) = match self.play.as_ref() {
            Some(p) if p.feed.is_some() => (wire::PLAYING_STREAM, 0, p.ms, ms_since(self.ctx, p.started).min(p.ms),
                p.underruns, (p.silence as u64 * 1000 / self.rate as u64) as u32),
            Some(p) if p.sound => (wire::PLAYING_SOUND, 0, p.ms, ms_since(self.ctx, p.started).min(p.ms), p.underruns, 0),
            Some(p) => (wire::PLAYING_TONE, p.hz, p.ms, ms_since(self.ctx, p.started).min(p.ms), p.underruns, 0),
            None => (wire::PLAYING_NOTHING, 0, 0, 0, 0, self.last_silence_ms),
        };
        out[4] = playing;
        wire::put_u16(out, 5, hz as u16);
        wire::put_u32(out, 7, len);
        wire::put_u32(out, 11, elapsed);
        wire::put_u32(out, 15, self.underruns_total.saturating_add(under));
        out[19] = 0; // no interrupt: the ring is polled
        out[20] = JACK_DEVICE;
        wire::put_u32(out, 21, sil);
        out[25] = self.system_sounds as u8;
        wire::STATUS_LEN
    }

    fn info(&self, out: &mut [u8]) -> usize {
        for b in out[..wire::INFO_LEN].iter_mut() {
            *b = 0;
        }
        out[0] = wire::OK;
        out[8] = 2;
        wire::put_u32(out, 11, self.rate);
        // In 16-bit stereo bytes, as the protocol counts a ring.
        wire::put_u32(out, 15, (RING_FRAMES * 4) as u32);
        out[26] = wire::KIND_PWM;
        wire::put_u16(out, 27, self.range as u16);
        wire::INFO_LEN
    }

    fn answer(&mut self, op: u8, args: &[u8], out: &mut [u8]) -> usize {
        let on = self.power == wire::POWER_ON;
        match op {
            wire::OP_STATUS => self.status(out),
            wire::OP_INFO => self.info(out),
            wire::OP_VOLUME => {
                let Some(&v) = args.first() else { return bad(out) };
                if v > wire::VOLUME_MAX {
                    return bad(out);
                }
                self.volume = v;
                self.settings_dirty = true;
                out[0] = wire::OK;
                out[1] = v;
                // Applied to the samples by this driver, so there is nothing to read back that it does
                // not already know: what it writes from the next refill on IS the volume.
                out[2] = wire::VERIFIED;
                3
            }
            wire::OP_MUTE => {
                let want = args.first().copied().unwrap_or(1) != 0;
                out[1] = self.volume;
                out[2] = wire::VERIFIED;
                if want == self.muted {
                    out[0] = wire::ALREADY;
                    return 3;
                }
                self.muted = want;
                self.settings_dirty = true;
                out[0] = wire::OK;
                3
            }
            wire::OP_POWER => {
                let mode = args.first().copied().unwrap_or(0xFF);
                if !matches!(mode, wire::POWER_OFF | wire::POWER_ON | wire::POWER_HARD_OFF) {
                    return bad(out);
                }
                if mode == self.power || (mode != wire::POWER_ON && self.power != wire::POWER_ON) {
                    out[0] = wire::ALREADY;
                    out[1] = wire::VERIFIED;
                    return 2;
                }
                out[0] = wire::OK;
                out[1] = if mode == wire::POWER_ON {
                    let rate = self.rate;
                    if self.start(rate) { self.power = mode; wire::VERIFIED } else { wire::CONTRADICTED }
                } else {
                    self.stop_play("audio off");
                    let off = self.stop_all();
                    self.power = mode;
                    match (off, mode) {
                        (false, _) => wire::CONTRADICTED,
                        // The clock is the kernel's, and stays running: there is no deeper off this
                        // driver can reach, so a hard off is the same off, and says it cannot confirm more.
                        (true, wire::POWER_HARD_OFF) => wire::UNSUPPORTED,
                        (true, _) => wire::VERIFIED,
                    }
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
                } else if self.play.is_some() {
                    wire::BUSY
                } else {
                    self.start_tone(hz as u32, ms);
                    wire::OK
                };
                1
            }
            wire::OP_OPEN => {
                if !on {
                    out[0] = wire::AUDIO_OFF;
                    return 1;
                }
                if self.play.is_some() {
                    out[0] = wire::BUSY;
                    return 1;
                }
                if args.len() < 10 {
                    return bad(out);
                }
                self.open_stream(wire::get_u32(args, 0), args[4], args[5], wire::get_u32(args, 6), out)
            }
            // One output, the jack: listed, always selected, and with no way to tell whether anything is
            // plugged in - the jack has no sense line the SoC can read.
            wire::OP_SYSTEM_SOUNDS => {
                let want = args.first().copied().unwrap_or(1) != 0;
                out[0] = if want == self.system_sounds { wire::ALREADY } else { wire::OK };
                if want != self.system_sounds {
                    self.system_sounds = want;
                    self.settings_dirty = true;
                }
                1
            }
            wire::OP_SOUND => match args.first() {
                Some(&kind) if sounds::shape(kind).is_some() => {
                    out[0] = wire::OK;
                    out[1] = self.start_sound(kind) as u8;
                    2
                }
                _ => {
                    out[0] = wire::BAD_ARG;
                    1
                }
            },
            wire::OP_DEBUG => {
                let (view, page) = (args.first().copied().unwrap_or(0xFF), args.get(1).copied().unwrap_or(0));
                if view as usize >= wire::DEBUG_VIEWS.len() || page >= wire::DEBUG_PAGES_MAX {
                    out[0] = wire::BAD_ARG;
                    return 1;
                }
                self.debug(view, page, out)
            }
            wire::OP_OUTPUTS => {
                out[0] = wire::OK;
                out[1] = 1;
                out[2] = JACK_PIN;
                out[3] = JACK_DEVICE;
                out[4] = 1;
                out[5] = wire::PRESENCE_UNKNOWN;
                6
            }
            wire::OP_OUTPUT => match args.first() {
                Some(&JACK_PIN) => {
                    out[0] = wire::ALREADY;
                    out[1] = wire::VERIFIED;
                    2
                }
                _ => {
                    out[0] = wire::BAD_ARG;
                    1
                }
            },
            wire::OP_PCM => self.feed_pcm(args, out),
            wire::OP_END => self.end_feed(out),
            wire::OP_STOP => {
                let (was, ms) = self.stop_play("asked to stop");
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
    gs::trace::as_name(&ctx, "pwm-audio");
    let mmio = ctx.mmio();
    let dma = ctx.dma_region();
    let irq = Irq::granted(&ctx);
    let dev = bring_up(&ctx, mmio.as_ref(), dma.as_ref());
    serve(&ctx, &irq, dev)
}

fn bring_up<'a>(ctx: &'a ServiceContext, m: Option<&'a Mmio>, d: Option<&'a Dma>) -> Device<'a> {
    let b = match ctx.probe_mode() {
        2 => PI2,
        4 => PI4,
        other => {
            ctx.log_fmt(format_args!("pwm-audio: board {} is not one this driver knows (2 = Pi 2, 4 = Pi 4) - nothing to drive", other));
            return Device::Absent(wire::no_device::NO_CONTROLLER);
        }
    };
    let Some(m) = m.filter(|m| m.len() >= WINDOW) else {
        ctx.log("pwm-audio: no PWM and DMA window was granted - this board has no jack the kernel knows");
        return Device::Absent(wire::no_device::NO_CONTROLLER);
    };
    let Some(d) = d.filter(|d| d.len() >= ARENA_NEEDED) else {
        ctx.log_fmt(format_args!("pwm-audio: no DMA arena of {} bytes was granted - nothing to play from", ARENA_NEEDED));
        return Device::Absent(wire::no_device::NO_ARENA);
    };
    // The engine reaches the first 1 GiB of RAM through its uncached alias, and no more.
    let top = d.phys_at(0) + d.len() as u64;
    if top > DMA_REACH {
        ctx.log_fmt(format_args!(
            "pwm-audio: the DMA arena ends at {:#x}, past the 1 GiB the DMA engine can reach - refusing rather than playing someone else's memory",
            top));
        return Device::Absent(wire::no_device::NO_ARENA);
    }
    d.zero();
    let mut p = Pwm {
        ctx, m, d, b, rate: RATE_DEFAULT, range: 0, power: wire::POWER_ON, volume: DEFAULT_VOLUME, muted: false,
        play: None, underruns_total: 0, last_silence_ms: 0, settings_dirty: false, settings_failing: false,
        paced: None, system_sounds: true, last_sound: None,
    };
    if let Some(s) = settings::load(ctx, &mut gs::fs::Fs::new(ctx).patience_secs(settings::PATIENCE_SECS), "pwm-audio", DEFAULT_VOLUME) {
        p.volume = s.volume;
        p.muted = s.muted;
        p.system_sounds = s.system_sounds;
    }
    if !p.start(RATE_DEFAULT) {
        ctx.log_fmt(format_args!("pwm-audio: the jack did not come up (PWM CTL {:#010x}, STA {:#010x}) - nothing will play",
            m.read32(b.pwm + PWM_CTL), m.read32(b.pwm + PWM_STA)));
        return Device::Absent(wire::no_device::BRINGUP_FAILED);
    }
    ctx.log_fmt(format_args!(
        "pwm-audio: {} jack up - PWM at {} Hz, range {} (about {} bits), DMA channel {} looping a {} ms ring; volume {}{}",
        b.name, p.rate, p.range, 31 - p.range.leading_zeros(), b.channel,
        RING_FRAMES as u64 * 1000 / p.rate as u64, p.volume, if p.muted { ", muted" } else { "" }));
    Device::Ready(p)
}

fn serve(ctx: &ServiceContext, irq: &Irq, mut dev: Device) -> ! {
    match &dev {
        Device::Ready(_) => ctx.log("pwm-audio: ready - serving requests; the ring is refilled by polling"),
        Device::Absent(r) => ctx.log_fmt(format_args!(
            "pwm-audio: serving with no device to play on (reason {}) - every request is answered with that", r)),
    }
    let mut out = [0u8; 4096];
    loop {
        let playing = matches!(&dev, Device::Ready(p) if p.play.is_some());
        let within = if playing { REFILL_PACE } else { SERVE_WAIT };
        if let Woke::Request(req) = irq.wait(ctx, within) {
            let b = req.payload_bytes();
            let n = match &mut dev {
                _ if b.first() != Some(&wire::TAGGED) => {
                    out[2] = wire::UNKNOWN_OP;
                    3
                }
                Device::Absent(r) => {
                    out[2] = wire::NO_DEVICE;
                    out[3] = *r;
                    4
                }
                Device::Ready(p) => {
                    let (op, args) = (b.get(2).copied().unwrap_or(0), b.get(3..).unwrap_or(&[]));
                    2 + p.answer(op, args, &mut out[2..])
                }
            };
            out[0] = wire::TAGGED;
            out[1] = b.get(1).copied().unwrap_or(0);
            if let Some(cap) = gs::ipc::take_sent_cap(ctx) {
                // `reply` gives the one-shot reply capability back either way (CLAUDE.md 8.5).
                let _ = gs::ipc::reply(ctx, cap, &Message::from_bytes(&out[..n]));
            }
        }
        if let Device::Ready(p) = &mut dev {
            p.service();
            if p.settings_dirty && p.play.is_none() {
                p.settings_dirty = false;
                match settings::save(&mut gs::fs::Fs::new(ctx).patience_secs(settings::PATIENCE_SECS), Settings { volume: p.volume, muted: p.muted, output: None, system_sounds: p.system_sounds }) {
                    Ok(()) => p.settings_failing = false,
                    Err(e) if !p.settings_failing => {
                        p.settings_failing = true;
                        ctx.log_fmt(format_args!(
                            "pwm-audio: could not write /audio.settings ({}) - the setting holds until this driver restarts", e.as_str()));
                    }
                    Err(_) => {}
                }
            }
        }
    }
}
