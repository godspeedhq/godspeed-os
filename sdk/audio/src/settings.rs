// SPDX-License-Identifier: GPL-2.0-only
//! `/audio.settings`: the volume, the mute and the output, kept across a restart (`utilities/57_audio.md`), written
//! once for every audio driver - the HD Audio driver on x86 and the PWM jack driver on the Pis keep the
//! same file in the same words.

use godspeed as gs;
use godspeed::driver::delay;
use godspeed::driver::wait::Budget;
use godspeed_sdk::ServiceContext;

/// Where the volume and the mute are kept (`utilities/57_audio.md`). Plain labelled lines, readable with
/// `read /audio.settings`. The driver owns the file: it reads it once when it comes up and writes it after
/// a change. `on` and `off` are deliberately NOT kept - audio comes up on at every boot.
pub const SETTINGS_PATH: &str = "/audio.settings";
/// The whole file is read in one piece into this many bytes. Two lines need about twenty; a file larger
/// than this is not one this driver wrote, and is ignored with a line rather than half-read.
const SETTINGS_MAX: usize = 256;
/// How long one `fs` exchange may take before it counts as unanswered, and how many times the load asks.
/// At boot `fs` may still be mounting; a few seconds of patience, then the defaults and a line - never a
/// wait the driver cannot get out of.
pub const PATIENCE_SECS: i64 = 2;
const SETTINGS_TRIES: u32 = 3;
const SETTINGS_RETRY_PAUSE: Budget = Budget::ms(1000);

/// What the file holds.
#[derive(Clone, Copy)]
pub struct Settings {
    pub volume: u8,
    pub muted: bool,
    /// The output chosen with `audio output`, as its default-device field (`wire::device_of`); `None`
    /// when none was ever chosen, and the driver plays through the first output it found.
    pub output: Option<u8>,
}

/// Read the settings, or `None` for the defaults - with the reason said once in the log either way.
///
/// `fs` is the DRIVER's handle - `gs::fs::Fs::new(ctx).patience_secs(PATIENCE_SECS)` - because the driver
/// owns its connection to `fs`, and the library that reads the file is only borrowing it. That handle is
/// also what reacquires `fs` by name when it restarts (Commandment IX).
pub fn load(ctx: &ServiceContext, fs: &mut gs::fs::Fs, who: &str, default_volume: u8) -> Option<Settings> {
    use gs::Error;
    for attempt in 1..=SETTINGS_TRIES {
        let mut buf = [0u8; SETTINGS_MAX];
        match fs.read_into(SETTINGS_PATH, &mut buf) {
            Ok(n) => return parse(ctx, who, &buf[..n], default_volume),
            Err(Error::NotFound) => {
                ctx.log_fmt(format_args!("{}: no /audio.settings yet - starting at the defaults; it is written at the first change", who));
                return None;
            }
            Err(Error::NoFilesystem) => {
                ctx.log_fmt(format_args!("{}: no filesystem on this machine's disk - settings are kept in memory only", who));
                return None;
            }
            Err(Error::BufferTooSmall) => {
                ctx.log_fmt(format_args!(
                    "{}: /audio.settings is larger than {} bytes - not a file this driver wrote; ignored, starting at the defaults",
                    who, SETTINGS_MAX));
                return None;
            }
            Err(e) if attempt < SETTINGS_TRIES && e.retry_is_safe() => {
                delay::hold_parked(ctx, SETTINGS_RETRY_PAUSE);
            }
            Err(e) => {
                ctx.log_fmt(format_args!(
                    "{}: could not read /audio.settings ({}) after {} attempt(s) - starting at the defaults",
                    who, e.as_str(), attempt));
                return None;
            }
        }
    }
    None
}

/// `volume N`, `muted yes|no` and `output <name>`, one per line. A line this driver does not know is ignored and said once;
/// a value out of range keeps the default for that setting and is said too.
fn parse(ctx: &ServiceContext, who: &str, text: &[u8], default_volume: u8) -> Option<Settings> {
    let mut s = Settings { volume: default_volume, muted: false, output: None };
    let mut ignored = 0u32;
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(' ').map_or((line, ""), |(k, v)| (k, v.trim()));
        match (key, value) {
            ("volume", v) => match v.parse::<u8>() {
                Ok(n) if n <= crate::wire::VOLUME_MAX => s.volume = n,
                _ => ignored += 1,
            },
            ("muted", "yes") => s.muted = true,
            ("muted", "no") => s.muted = false,
            ("output", name) => match crate::wire::device_of(name) {
                Some(d) => s.output = Some(d as u8),
                None => ignored += 1,
            },
            _ => ignored += 1,
        }
    }
    if ignored > 0 {
        ctx.log_fmt(format_args!("{}: /audio.settings has {} line(s) this driver does not understand - ignored", who, ignored));
    }
    ctx.log_fmt(format_args!(
        "{}: settings read from /audio.settings - volume {}, {}{}{}", who, s.volume,
        if s.muted { "muted" } else { "unmuted" }, if s.output.is_some() { ", output " } else { "" },
        s.output.map_or("", |d| crate::wire::device_name(d as u32))));
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

/// Write the settings through the driver's `fs` handle (see [`load`]). A failure is returned for the caller
/// to report; `OutcomeUnknown` means the file may or may not hold them, and is never re-sent as though it
/// had not happened.
pub fn save(fs: &mut gs::fs::Fs, s: Settings) -> Result<(), gs::Error> {
    use core::fmt::Write;
    let mut l = Line { buf: [0; 64], len: 0, overflow: false };
    let _ = write!(l, "volume {}\nmuted {}\n", s.volume, if s.muted { "yes" } else { "no" });
    if let Some(d) = s.output {
        let _ = write!(l, "output {}\n", crate::wire::device_name(d as u32));
    }
    if l.overflow {
        return Err(gs::Error::InvalidInput);
    }
    fs.write(SETTINGS_PATH, &l.buf[..l.len])
}

