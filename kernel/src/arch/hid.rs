// SPDX-License-Identifier: GPL-2.0-only
//! USB HID boot-protocol keyboard decoding, shared by every in-kernel USB driver.
//!
//! **Why this exists here and not in the SDK.** On x86 the USB drivers are userspace *services* and
//! share `sdk/rust/src/hid.rs`. Neither ARM port routes device IRQs to userspace yet, so their USB
//! drivers run **in the kernel** (`arch/arm/CLAUDE.md`, "Drivers on ARM") - and the kernel cannot link
//! the SDK (it is a separate, differently-licensed crate; the kernel depends on no service code). So
//! the kernel side implements the *same behaviour contract*. When device IRQs reach userspace and the
//! drivers become services, they drop this module and use the SDK's, like x86. Keep the two in step:
//! this is the same decode a serial terminal produces, so the shell's ONE input parser handles USB and
//! serial alike.
//!
//! **Why it sits in `arch/` rather than in one arch's directory.** It started under `arch/arm/`, where
//! it was the only in-kernel USB driver. The Pi 4's xHCI is the second, and copying 234 lines of key
//! tables and repeat timing to sit beside it is exactly the mistake `crate::fbcon` was created to undo:
//! two consoles drifted apart, each having fixed terminal bugs the other still had. One copy, two
//! callers. There is nothing CPU-specific in here to make it arch-specific in the first place.
//!
//! Pure logic - no MMIO, no I/O, and **no clock**. Time comes in as a parameter, because the two ports
//! read different counters (a 1 MHz system timer on the Pi 2, the generic timer on the Pi 4) and a
//! module that reached for one of them by name would only work on one machine. The driver reads the
//! fixed 8-byte boot report and hands it here; the side effect (pushing a byte to the console ring)
//! stays in the driver, passed in as a closure.

/// HID usage of the Caps Lock key. It is a host-tracked LATCH: the modifier byte never reports it, so
/// the host toggles a `caps` flag on each fresh press.
pub const KEY_CAPS_LOCK: u8 = 0x39;
/// HID usage of Delete (forward-delete) - the `Del` of Ctrl+Alt+Del.
pub const KEY_DELETE: u8 = 0x4C;
/// Console-stream signal for the Ctrl+Alt+Del secure-attention chord. `0x80` is outside ASCII, so no
/// typed key can produce it. The driver *signals*; the shell (which holds REBOOT) decides (§6.4 SEC-2).
pub const CTRL_ALT_DEL_SIGNAL: u8 = 0x80;

/// Decode a HID boot-keyboard usage code to ASCII (US layout). `caps` is the host's Caps Lock latch;
/// Caps Lock XORs Shift but **only for letters** - it never affects digits or symbols.
pub fn hid_to_ascii(key: u8, mods: u8, caps: bool) -> Option<u8> {
    let shift = mods & 0x22 != 0; // left or right Shift
    let ctrl = mods & 0x11 != 0;  // left or right Ctrl
    match key {
        0x04..=0x1D => {
            // Ctrl+letter -> the C0 control code (Ctrl+A = 0x01 ... Ctrl+Z = 0x1A), exactly what a
            // serial terminal sends, so ^C/^S/^Q and an app's shortcuts work from a USB keyboard.
            if ctrl { return Some(key - 0x03); }
            let base = b'a' + (key - 0x04);
            // Upper-case iff exactly one of Shift / Caps is active (Shift inverts Caps).
            Some(if shift ^ caps { base - 32 } else { base })
        }
        0x1E..=0x26 => Some(if shift {
            [b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'('][(key - 0x1E) as usize]
        } else {
            b'1' + (key - 0x1E)
        }),
        0x27 => Some(if shift { b')' } else { b'0' }),
        0x28 => Some(b'\r'), // Enter (the console/line editor acts on CR)
        0x29 => Some(0x1B),  // Escape - bare ESC (the shell disambiguates it from a sequence)
        0x2A => Some(0x08),  // Backspace
        0x2B => Some(b'\t'), // Tab
        0x2C => Some(b' '),  // Space
        0x2D => Some(if shift { b'_' } else { b'-' }),
        0x2E => Some(if shift { b'+' } else { b'=' }),
        0x2F => Some(if shift { b'{' } else { b'[' }),
        0x30 => Some(if shift { b'}' } else { b']' }),
        0x31 => Some(if shift { b'|' } else { b'\\' }),
        0x32 => Some(if shift { b'~' } else { b'#' }), // Non-US # / ~ (ISO extra key)
        0x33 => Some(if shift { b':' } else { b';' }),
        0x34 => Some(if shift { b'"' } else { b'\'' }),
        0x35 => Some(if shift { b'~' } else { b'`' }),
        0x36 => Some(if shift { b'<' } else { b',' }),
        0x37 => Some(if shift { b'>' } else { b'.' }),
        0x38 => Some(if shift { b'?' } else { b'/' }),
        0x64 => Some(if shift { b'|' } else { b'\\' }), // Non-US \ / | (the 0x31 twin)
        // Numeric keypad (a SEPARATE number pad sends these, 0x54-0x63) - the keys that produced
        // nothing at all before. We do not track the NumLock LED, so map them NumLock-ON
        // unconditionally: digits, the arithmetic operators, and keypad Enter. That is what a shell
        // wants from a numpad; the NumLock-off navigation meaning is deliberately not modelled.
        0x54 => Some(b'/'),
        0x55 => Some(b'*'),
        0x56 => Some(b'-'),
        0x57 => Some(b'+'),
        0x58 => Some(b'\r'), // Keypad Enter
        0x59 => Some(b'1'),
        0x5A => Some(b'2'),
        0x5B => Some(b'3'),
        0x5C => Some(b'4'),
        0x5D => Some(b'5'),
        0x5E => Some(b'6'),
        0x5F => Some(b'7'),
        0x60 => Some(b'8'),
        0x61 => Some(b'9'),
        0x62 => Some(b'0'),
        0x63 => Some(b'.'),
        _ => None,
    }
}

/// Emit the byte(s) one keycode produces, returning true if it produced output (i.e. it is a
/// printable / cursor key worth auto-repeating). Cursor and navigation keys emit the same ANSI escape
/// sequences a serial terminal sends, so the shell's single input parser handles USB and serial alike.
fn emit_key(k: u8, mods: u8, caps: bool, emit: &mut impl FnMut(u8)) -> bool {
    fn csi(body: &[u8], emit: &mut impl FnMut(u8)) -> bool {
        emit(0x1B);
        emit(b'[');
        for &b in body { emit(b); }
        true
    }
    match k {
        0x52 => csi(b"A", emit),  // Up
        0x51 => csi(b"B", emit),  // Down
        0x4F => csi(b"C", emit),  // Right
        0x50 => csi(b"D", emit),  // Left
        0x4A => csi(b"H", emit),  // Home
        0x4D => csi(b"F", emit),  // End
        0x49 => csi(b"2~", emit), // Insert
        0x4C => csi(b"3~", emit), // Delete (forward)
        0x4B => csi(b"5~", emit), // PageUp
        0x4E => csi(b"6~", emit), // PageDown
        // F1-F12: the standard xterm sequences (F1-F4 = SS3 `ESC O P/Q/R/S`, F5-F12 = `ESC[<n>~`).
        0x3A => { emit(0x1B); emit(b'O'); emit(b'P'); true }
        0x3B => { emit(0x1B); emit(b'O'); emit(b'Q'); true }
        0x3C => { emit(0x1B); emit(b'O'); emit(b'R'); true }
        0x3D => { emit(0x1B); emit(b'O'); emit(b'S'); true }
        0x3E => csi(b"15~", emit),
        0x3F => csi(b"17~", emit),
        0x40 => csi(b"18~", emit),
        0x41 => csi(b"19~", emit),
        0x42 => csi(b"20~", emit),
        0x43 => csi(b"21~", emit),
        0x44 => csi(b"23~", emit),
        0x45 => csi(b"24~", emit),
        _ => match hid_to_ascii(k, mods, caps) {
            Some(ch) => { emit(ch); true }
            None => false,
        },
    }
}

/// Typematic auto-repeat: a USB boot keyboard reports only on CHANGE - a held key sends one down
/// report and then nothing until release - so the host must synthesise repeat itself.
///
/// Time is in **microseconds**, supplied by the caller. On the Pi 2 that is the BCM2835 1 MHz System
/// Timer, which needs no calibration (it is exactly 1 MHz by construction); on the Pi 4 it is derived
/// from the generic timer, whose frequency is read from `CNTFRQ_EL0`. Either way the delays are true
/// wall-clock and cannot drift the way the calibrated-TSC path did on the Wyse.
pub struct KeyRepeat {
    key: u8,       // HID usage being repeated (0 = nothing armed)
    mods: u8,      // modifiers captured at press (so Shift+key repeats the shifted form)
    caps: bool,    // Caps latch captured at press (so a held letter repeats in the right case)
    next_at: u32,  // systimer microsecond value at which the next repeat is due
}

/// Delay before the first repeat. Must require a CLEARLY DELIBERATE hold: at 400 ms ordinary typing
/// tripped it on real hardware (`appeardddddddddddd`, `multiplere`), because a brief pause mid-word is
/// easily that long. 600 ms is the value x86 settled on for the same complaint - fast enough to feel
/// responsive when you mean it, slow enough that normal typing never triggers it.
const REPEAT_INITIAL_US: u32 = 600_000; // 600 ms
/// Delay between repeats once repeating (~20 characters/second).
const REPEAT_INTERVAL_US: u32 = 50_000; // 50 ms

/// Wrap-safe "has `now` reached `when`?" - the 1 MHz counter's low word wraps every ~71 minutes, so
/// compare the signed difference rather than the raw values.
fn due(now: u32, when: u32) -> bool {
    (now.wrapping_sub(when) as i32) >= 0
}

impl KeyRepeat {
    pub const fn new() -> Self {
        KeyRepeat { key: 0, mods: 0, caps: false, next_at: 0 }
    }

    fn arm(&mut self, key: u8, mods: u8, caps: bool, now: u32) {
        self.key = key;
        self.mods = mods;
        self.caps = caps;
        self.next_at = now.wrapping_add(REPEAT_INITIAL_US);
    }

    /// Stop repeating. Called on release, and by the driver when it can no longer see the keyboard
    /// (an unreachable poll leaves our view of which keys are down stale - better silent than spewing).
    pub fn disarm(&mut self) {
        self.key = 0;
    }

    /// Emit a repeat of the held key if one is due. The driver calls this EVERY poll (every timer
    /// tick), including ticks where the keyboard sent no report - a held key sends nothing, so the
    /// repeat must come from here.
    pub fn poll(&mut self, now: u32, mut emit: impl FnMut(u8)) {
        if self.key == 0 { return; }
        if !due(now, self.next_at) { return; }
        emit_key(self.key, self.mods, self.caps, &mut emit);
        self.next_at = now.wrapping_add(REPEAT_INTERVAL_US);
    }
}

/// True if a keyboard boot report is the Ctrl+Alt+Del chord. Apply ONLY to keyboard reports.
pub fn is_ctrl_alt_del(report: &[u8; 8]) -> bool {
    if report[1] != 0 { return false; }
    let mods = report[0];
    (mods & 0x11 != 0) && (mods & 0x44 != 0) && report[2..8].contains(&KEY_DELETE)
}

/// Decode a keyboard boot report (modifiers in byte 0, up to six keycodes in bytes 2..8) with N-key
/// edge detection: `emit` is called for every key down now but not in `last`, so rolling onto a new
/// key before releasing the previous one drops nothing and a held key fires exactly once. `last` is
/// updated for the next call. `rep` is armed on a fresh printable press and disarmed on release.
pub fn decode_keyboard(
    report: &[u8; 8],
    last: &mut [u8; 6],
    rep: &mut KeyRepeat,
    caps: &mut bool,
    now: u32,
    mut emit: impl FnMut(u8),
) {
    // Byte 1 is reserved and always 0 in a real report; an all-0xff report is the signature of a
    // failed/stale read (device gone mid-transaction). Decoding it would spew 0xff "keystrokes" AND
    // poison `last` so later real keys diff wrong. Drop it untouched.
    if report[1] != 0 { return; }
    let mods = report[0];
    let cur = [report[2], report[3], report[4], report[5], report[6], report[7]];
    for &k in cur.iter() {
        if k == 0 || k == 0x01 { continue; } // 0 = empty slot, 0x01 = rollover error
        if !last.contains(&k) {
            // Caps Lock is a latch, not a character: flip the host state, emit nothing, no repeat.
            if k == KEY_CAPS_LOCK { *caps = !*caps; continue; }
            if emit_key(k, mods, *caps, &mut emit) {
                // The newest printable/cursor key held becomes the repeat key - except the one-shot
                // keys: Escape (repeating it makes the shell re-disambiguate a bare ESC every tick),
                // F1-F12 (actions, not characters - holding F1 must not re-open help forever), and
                // Enter / keypad Enter (commit keys - repeating spams prompts and re-fires a
                // confirmation's answer).
                if k != 0x29 && k != 0x28 && k != 0x58 && !(0x3A..=0x45).contains(&k) {
                    rep.arm(k, mods, *caps, now);
                }
            }
        }
    }
    // Stop repeating once the armed key is no longer held.
    if rep.key != 0 && !cur.contains(&rep.key) {
        rep.disarm();
    }
    *last = cur;
}
