// SPDX-License-Identifier: GPL-2.0-only
//! The request/reply vocabulary between the audio drivers (`audio-driver`, `pwm-audio`) and the shell's
//! `audio`.
//!
//! # Every request is TAGGED, and every answer is immediate
//!
//! A request is `[TAGGED, tag, op, args...]` and its answer `[TAGGED, tag, status, ...]`, so the asker
//! takes the answer carrying its own tag and passes over a late one. That is backlog/70's lesson from the
//! radio: a receive takes whatever is next, and an answer the shell had stopped waiting for was read as
//! the next request's. Here it is in the protocol from the first byte rather than added after.
//!
//! **No request waits for the sound.** `OP_TONE` starts a tone and answers; the shell follows it with
//! `OP_STATUS` and ends it early with `OP_STOP`. The driver serves requests while it plays (its wait is
//! `gs::driver::irq`'s, which hands a request back mid-tone), so every answer comes within one request's
//! work - which is what lets the shell's wait be short and never owe the driver an answer.
//!
//! Multi-byte fields are little-endian.

/// The first byte of every request and every answer: `[TAGGED, tag, ...]`. Outside every op.
pub const TAGGED: u8 = 0xA7;

// ---- Status: byte 0 of an answer, after the tag -------------------------------------------------------

/// Done; the op's fields follow.
pub const OK: u8 = 0;
/// Not an op this driver knows.
pub const UNKNOWN_OP: u8 = 1;
/// An argument out of range (a volume over 100, a tone outside `TONE_HZ_MIN..=TONE_HZ_MAX`).
pub const BAD_ARG: u8 = 2;
/// Audio is off (`audio off`, or `off hard`): nothing plays until `audio on`.
pub const AUDIO_OFF: u8 = 3;
/// Something is already playing.
pub const BUSY: u8 = 4;
/// The driver has no device it can use; byte 1 is a `NoDevice` reason.
pub const NO_DEVICE: u8 = 5;
/// Already in the state asked for: nothing was sent to the codec.
pub const ALREADY: u8 = 6;
/// A format this driver or this codec does not play; byte 1 is a `format` reason.
pub const FORMAT: u8 = 7;
/// `OP_PCM` or `OP_END` with no stream open.
pub const NOT_OPEN: u8 = 8;

/// Byte 1 of a `FORMAT` answer.
pub mod format {
    /// Only 16-bit samples are played.
    pub const BITS: u8 = 1;
    /// One or two channels.
    pub const CHANNELS: u8 = 2;
    /// A rate the codec does not offer; this driver plays 44100 and 48000 Hz where the codec has them.
    pub const RATE: u8 = 3;
}

/// Byte 1 of a `NO_DEVICE` answer: why there is nothing to drive.
pub mod no_device {
    /// No audio device (an HD Audio controller, or the Pis' PWM jack) was granted to this driver.
    pub const NO_CONTROLLER: u8 = 1;
    /// The controller did not come out of reset.
    pub const RESET_FAILED: u8 = 2;
    /// The link came up and no codec answered.
    pub const NO_CODEC: u8 = 3;
    /// A codec answered and offers no output path this driver can use.
    pub const NO_PATH: u8 = 4;
    /// The codec is not one playback has been verified on; the driver surveyed it and stopped
    /// (docs/audio.md, A6).
    pub const UNVERIFIED_CODEC: u8 = 5;
    /// No DMA arena, or one too small for the rings and the ring of sound.
    pub const NO_ARENA: u8 = 6;
    /// The command rings or the output path failed to come up; the driver's log says which.
    pub const BRINGUP_FAILED: u8 = 7;
}

/// The last byte of an answer to a change of state: what reading the codec back said.
pub const VERIFIED: u8 = 1;
/// The codec could not be asked (a verb went unanswered).
pub const UNVERIFIED: u8 = 2;
/// The codec was asked and disagrees.
pub const CONTRADICTED: u8 = 3;
/// The codec does not model what was changed, so it cannot confirm it - QEMU's `hda-output` reports no
/// power states at all. Not a failure: the change was made as far as the codec lets anything be made.
pub const UNSUPPORTED: u8 = 4;

/// `power` in a status answer and the `mode` of `OP_POWER`.
pub const POWER_OFF: u8 = 0;
pub const POWER_ON: u8 = 1;
/// The controller is held in reset - the closest HD Audio has to cutting the power. The Pis' PWM jack has
/// no deeper off: `pwm-audio` treats it as off and answers its verify byte `UNSUPPORTED`.
pub const POWER_HARD_OFF: u8 = 2;

// ---- Ops ----------------------------------------------------------------------------------------------

/// What audio is doing now. Answer `[OK, power, muted, volume, playing, hz u16, length_ms u32,
/// elapsed_ms u32, underruns u32, interrupts u8, output u8, silence_ms u32, system_sounds u8]` -
/// `STATUS_LEN` bytes. `system_sounds` is 1 when they are on (`OP_SYSTEM_SOUNDS`).
/// `playing` is a `PLAYING_*`; `hz` is the tone's, 0 for a stream. `interrupts` is 1 when the driver
/// refills on its interrupt, 0 when it polls. `underruns` counts since the driver started, and
/// `silence_ms` is the silence a stream that ran dry has had written in its place, for the stream
/// playing or last played. `output` is the output pin's default-device field, as in `OP_INFO`.
/// Or `[NO_DEVICE, reason]`.
pub const OP_STATUS: u8 = 1;
pub const STATUS_LEN: usize = 26;
pub const PLAYING_NOTHING: u8 = 0;
pub const PLAYING_TONE: u8 = 1;
pub const PLAYING_STREAM: u8 = 2;
/// A system sound (`OP_SOUND`).
pub const PLAYING_SOUND: u8 = 3;

/// The detail a fault needs. Answer `[OK, vendor u16, device u16, codec_addr, dac_node, pin_node,
/// pin_device, amp_steps, amp_step_now, rate u32, ring_bytes u32, interrupts u8, interrupts_seen u32,
/// version_major, version_minor, kind u8, pwm_range u16]` - `INFO_LEN` bytes. `kind` is a `KIND_*`; for
/// `KIND_PWM` the codec fields are zero and `pwm_range` is the PWM's steps per sample at this rate. `pin_device` is the pin's default-device field
/// (0 line out, 1 speaker, 2 headphone, ...); `amp_steps` 0 means the path has no amplifier to set.
/// Or `[NO_DEVICE, reason]`.
pub const OP_INFO: u8 = 2;
pub const INFO_LEN: usize = 29;
/// An Intel High Definition Audio controller and codec (x86).
pub const KIND_HDA: u8 = 0;
/// A jack driven by PWM and fed by the SoC's DMA engine (the Pis).
pub const KIND_PWM: u8 = 1;

/// `[3, volume]`, 0 to 100. Answer `[OK, volume, verify]`. 0 is silent and is NOT mute: the two stay
/// separate states (docs/audio.md, "Volume is 0 to 100").
pub const OP_VOLUME: u8 = 3;
pub const VOLUME_MAX: u8 = 100;

/// `[4, 1]` mute, `[4, 0]` unmute. Answer `[OK | ALREADY, volume, verify]`; `ALREADY` sends nothing.
pub const OP_MUTE: u8 = 4;

/// `[5, mode]` with a `POWER_*` mode. Answer `[OK | ALREADY, verify]`. Off and off hard stop anything
/// playing first; on re-applies the volume and the mute.
pub const OP_POWER: u8 = 5;

/// `[6, hz u16, ms u32]` - start a sine the driver generates itself. Answer `[OK]` the moment it starts,
/// or `AUDIO_OFF`, `BUSY`, `BAD_ARG`.
pub const OP_TONE: u8 = 6;
pub const TONE_HZ_MIN: u16 = 20;
pub const TONE_HZ_MAX: u16 = 20_000;
/// Ten minutes: a tone is a test, and a bound is a bound (CLAUDE.md 26.6).
pub const TONE_MS_MAX: u32 = 600_000;

/// Stop what is playing. Answer `[OK, was_playing, played_ms u32]`.
pub const OP_STOP: u8 = 7;

/// `[8, rate u32, channels u8, bits u8, frames u32]` - open a stream of samples the caller will send.
/// Answer `[OK, free_frames u32]`, or `FORMAT` with a reason, `AUDIO_OFF`, `BUSY`. Nothing plays yet:
/// the stream starts once half the ring is filled, or at `OP_END`, so a sender's first moments of
/// jitter are absorbed rather than heard. (`pwm-audio`'s engine never stops, so it gets the same cushion
/// by starting the stream half a ring of silence ahead of where the engine is.)
pub const OP_OPEN: u8 = 8;
/// `[9, samples...]` - whole frames of 16-bit little-endian samples, interleaved if stereo, at most
/// `PCM_MAX` bytes. Answer `[OK, accepted_frames u32, free_frames u32]` at once; frames past the free
/// space are NOT taken, and the caller sends them again. An empty `[9]` asks only how much is free.
/// A stream that runs dry has silence written in its place, counted as an underrun.
pub const OP_PCM: u8 = 9;
pub const PCM_MAX: usize = 3556;
/// `[10]` - nothing more is coming: the ring plays out and the stream stops. Answer `[OK]`.
pub const OP_END: u8 = 10;
/// The rates a stream may be opened at, where the codec offers them.
pub const STREAM_RATES: [u32; 2] = [44_100, 48_000];

/// The outputs the device has. Answer `[OK, count, (pin, device, selected, presence) x count]`: `pin` is
/// the output's node (what `OP_OUTPUT` names), `device` its default-device field as in `OP_INFO`,
/// `selected` 1 for the one playing, and `presence` a `PRESENCE_*`. At most `OUTPUTS_MAX` entries.
/// Or `[NO_DEVICE, reason]`.
pub const OP_OUTPUTS: u8 = 11;
/// The most outputs one answer lists. A codec offering more is listed up to here and the driver's log
/// says how many it left out.
pub const OUTPUTS_MAX: usize = 8;
/// The output cannot tell whether anything is plugged in (no presence detect).
pub const PRESENCE_UNKNOWN: u8 = 0;
/// Something is plugged into it.
pub const PRESENCE_PLUGGED: u8 = 1;
/// Nothing is plugged into it.
pub const PRESENCE_EMPTY: u8 = 2;

/// `[12, pin]` - play through the output whose node is `pin`. Answer `[OK | ALREADY, verify]`; `ALREADY`
/// sends nothing. `BAD_ARG` when `pin` is not one of `OP_OUTPUTS`'s; `BUSY` while something plays, since
/// changing the path under a running stream would cut it mid-sound; `AUDIO_OFF` when audio is off.
pub const OP_OUTPUT: u8 = 12;

/// `[14, on]` - the system sounds on (1) or off (0). Answer `[OK | ALREADY]`. Kept in `/audio.settings`;
/// on by default.
pub const OP_SYSTEM_SOUNDS: u8 = 14;

/// `[15, kind]` - play a system sound (`SOUND_*`). SENT WITHOUT A REPLY CAPABILITY and answered by
/// nobody: whoever asks never waits for a sound, so an absent or restarting driver means no sound, never a
/// delay. A driver plays it only when the system sounds are on, audio is on, nothing else is playing, and
/// `SOUND_GAP_MS` has passed since the last - which is what stops a burst of errors machine-gunning.
/// Mute and the volume apply as to anything else. Answered `[OK, played]` only if a reply capability came.
pub const OP_SOUND: u8 = 15;
/// A command was not found, or failed.
pub const SOUND_ERROR: u8 = 1;
/// An action was not allowed.
pub const SOUND_REFUSED: u8 = 2;
/// A background job finished.
pub const SOUND_DONE: u8 = 3;
/// A USB device arrived, or left.
pub const SOUND_PLUGGED: u8 = 4;
pub const SOUND_UNPLUGGED: u8 = 5;
/// At most one system sound in this many milliseconds; a request inside it is dropped, not queued.
pub const SOUND_GAP_MS: u32 = 500;

/// `[13, view, page]` - the driver's own account of itself (`audio debug`): one `DEBUG_*` view as
/// labelled lines of text, a page of at most `DEBUG_PAGE` bytes at a time. Answer `[OK, more, text...]`,
/// `more` 1 when another page follows; the asker fetches pages until it is 0 and joins them. A view this
/// device does not have is ONE line saying so, never an error - a PWM jack has no codec to dump. `BAD_ARG`
/// for a view this protocol does not name.
pub const OP_DEBUG: u8 = 13;
/// Text in one page. A message carries 4096 bytes; this leaves room for the tag and status around it.
pub const DEBUG_PAGE: usize = 3800;
/// Pages one view may take, so a reader looping on `more` is bounded too (CLAUDE.md 26.6).
pub const DEBUG_PAGES_MAX: u8 = 16;
pub const DEBUG_STATS: u8 = 0;
pub const DEBUG_CODEC: u8 = 1;
pub const DEBUG_STREAM: u8 = 2;
pub const DEBUG_TRACE: u8 = 3;
pub const DEBUG_REGISTERS: u8 = 4;
/// The views by the word `audio debug` takes, in `DEBUG_*` order.
pub const DEBUG_VIEWS: [&str; 5] = ["stats", "codec", "stream", "trace", "registers"];

/// One page of a debug view, written with `write!` as the whole view is rendered: bytes before the page
/// are counted and dropped, bytes after it set `more`. Rendering the whole view for every page keeps the
/// driver stateless between pages - nothing is held for an asker that may never come back for page 2.
pub struct Page<'a> {
    buf: &'a mut [u8],
    skip: usize,
    at: usize,
    len: usize,
    more: bool,
}

impl<'a> Page<'a> {
    /// Page `page` (from 0) of a view, into `buf`, whose length is the page size.
    pub fn new(buf: &'a mut [u8], page: u8) -> Self {
        let skip = page as usize * buf.len();
        Page { buf, skip, at: 0, len: 0, more: false }
    }

    /// The bytes written into the page, and whether the view went on past it.
    pub fn finish(self) -> (usize, bool) {
        (self.len, self.more)
    }
}

impl core::fmt::Write for Page<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if self.at >= self.skip {
                if self.len < self.buf.len() {
                    self.buf[self.len] = b;
                    self.len += 1;
                } else {
                    self.more = true;
                }
            }
            self.at += 1;
        }
        Ok(())
    }
}

/// What a pin's default-device field (bits 23:20 of its configuration default) names, as the driver's log
/// and the shell's `audio` both say it.
pub fn device_name(dev: u32) -> &'static str {
    match dev {
        0x0 => "line out",
        0x1 => "speaker",
        0x2 => "headphone",
        0x3 => "CD",
        0x4 => "S/PDIF out",
        0x5 => "digital out",
        0x8 => "line in",
        0xA => "microphone",
        _ => "other",
    }
}

/// The default-device field a name from [`device_name`] stands for, for reading a name back - from
/// `/audio.settings` or a typed `audio output headphone`. `None` for a name that table does not produce.
pub fn device_of(name: &str) -> Option<u32> {
    (0..16u32).find(|&d| device_name(d) == name && name != "other")
}

// ---- Byte helpers, so neither side hand-rolls them ----------------------------------------------------

pub fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
pub fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
/// 0 when `b` is too short, so a short answer reads as nothing rather than panicking a reader.
pub fn get_u16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
pub fn get_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
