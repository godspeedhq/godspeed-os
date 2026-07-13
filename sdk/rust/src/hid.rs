// SPDX-License-Identifier: Apache-2.0
//! USB HID boot-protocol decoding, shared by the USB host drivers (`xhci`, `ehci`).
//!
//! Pure logic - no syscalls, no I/O. Each driver reads the fixed 8-byte boot
//! report from its controller's DMA and hands it here; the side effects (pushing
//! a character to the console, logging a mouse event) stay in the driver, passed
//! in as closures. This is the controller-agnostic reuse §26.2 anticipated once
//! both drivers existed: the report format is identical whether the bytes arrived
//! over xHCI transfer-event rings or EHCI split qTDs.

/// Decode a HID boot-keyboard usage code to ASCII (US layout, common keys). `caps` is the host's
/// Caps Lock toggle state (the HID modifier byte does NOT carry it - Caps Lock is a host-tracked
/// latch, see `decode_keyboard`). Caps Lock XORs Shift, but **only for letters** - it never affects
/// digits or symbols (that's the difference from Shift).
pub fn hid_to_ascii(key: u8, mods: u8, caps: bool) -> Option<u8> {
    let shift = mods & 0x22 != 0; // left or right Shift
    let ctrl  = mods & 0x11 != 0; // left or right Ctrl
    match key {
        0x04..=0x1D => {
            // Ctrl+letter → the C0 control code (Ctrl+A=0x01 … Ctrl+Z=0x1A), exactly what a
            // serial terminal sends. Without this a USB keyboard can't produce ^S/^Q/^C, so
            // app shortcuts (the editor's save/quit, the shell's Ctrl-C) are unreachable on
            // hardware - they only worked over the serial console, which synthesises these
            // bytes itself. Ctrl takes precedence over Shift/Caps. (key 0x04='a' → 0x01.)
            if ctrl { return Some(key - 0x03); }
            let base = b'a' + (key - 0x04);
            // Uppercase iff exactly one of Shift / Caps Lock is active (Caps Lock toggles letters,
            // and Shift inverts Caps Lock - so SHIFT+letter is lowercase while Caps is on).
            Some(if shift ^ caps { base - 32 } else { base })
        }
        0x1E..=0x26 => {
            if shift {
                Some([b'!', b'@', b'#', b'$', b'%', b'^', b'&', b'*', b'('][(key - 0x1E) as usize])
            } else {
                Some(b'1' + (key - 0x1E))
            }
        }
        0x27 => Some(if shift { b')' } else { b'0' }),
        0x28 => Some(b'\n'), // Enter
        0x29 => Some(0x1B),  // Escape - bare ESC (the shell disambiguates it from a sequence)
        0x2A => Some(0x08),  // Backspace
        0x2B => Some(b'\t'), // Tab
        0x2C => Some(b' '),  // Space
        0x2D => Some(if shift { b'_' } else { b'-' }),
        0x2E => Some(if shift { b'+' } else { b'=' }),
        0x2F => Some(if shift { b'{' } else { b'[' }),
        0x30 => Some(if shift { b'}' } else { b']' }),
        0x31 => Some(if shift { b'|' } else { b'\\' }),
        0x32 => Some(if shift { b'~' } else { b'#' }), // Non-US # and ~ (ISO-layout extra key)
        0x33 => Some(if shift { b':' } else { b';' }),
        0x34 => Some(if shift { b'"' } else { b'\'' }), // apostrophe / quote
        0x35 => Some(if shift { b'~' } else { b'`' }),  // grave / tilde
        0x36 => Some(if shift { b'<' } else { b',' }),
        0x37 => Some(if shift { b'>' } else { b'.' }),
        0x38 => Some(if shift { b'?' } else { b'/' }),
        0x64 => Some(if shift { b'|' } else { b'\\' }), // Non-US \ and | (the 0x31 twin)
        // Numeric keypad (a separate number pad sends these, 0x54-0x63). We don't track
        // the NumLock LED state, so map them NumLock-ON unconditionally: digits + the
        // arithmetic operators + keypad-Enter. This is what a shell wants from a numpad
        // (typing numbers); the navigation interpretation (NumLock off → Home/arrows/etc.)
        // is deliberately not modelled. Shift does not change keypad output here.
        0x54 => Some(b'/'),  // Keypad /
        0x55 => Some(b'*'),  // Keypad *
        0x56 => Some(b'-'),  // Keypad -
        0x57 => Some(b'+'),  // Keypad +
        0x58 => Some(b'\n'), // Keypad Enter
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
        0x63 => Some(b'.'),  // Keypad .
        _ => None,
    }
}

/// Codes in the printable-key ranges (letters, digits, punctuation, keypad) - but NOT the
/// control keys (Enter/Esc/Backspace/Tab/Space at 0x28-0x2C) or modifiers/F-keys. Used to
/// decide whether an unmapped key is worth reporting (a missing punctuation mapping) vs
/// silent noise (a function/modifier key with no character). Keys in these ranges are all
/// mapped by `hid_to_ascii`, so reaching `on_unmapped` here means a gap to fill.
fn is_typable_code(k: u8) -> bool {
    matches!(k, 0x04..=0x27 | 0x2D..=0x38 | 0x54..=0x63 | 0x64)
}

/// Emit the byte(s) a single keycode produces under `mods`, returning `true` if it
/// produced any output (i.e. it is a printable / cursor key worth auto-repeating).
/// Shared by the first-press edge path and the auto-repeat path so a repeated key is
/// byte-for-byte identical to its first press. The cursor and navigation-cluster keys
/// emit the same ANSI escape sequences a serial terminal sends, so the shell's one input
/// parser (`handle_csi` / the pager) handles USB and serial alike - this is what makes a
/// standard extended keyboard's Home/End/Delete/PageUp/PageDown work on real hardware
/// (without it the physical keys produce nothing).
fn emit_key(k: u8, mods: u8, caps: bool, emit: &mut impl FnMut(u8)) -> bool {
    // ESC [ <body...> for a cursor/navigation key. Returns true (it produced output).
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
        0x4C => csi(b"3~", emit), // Delete (forward delete)
        0x4B => csi(b"5~", emit), // PageUp
        0x4E => csi(b"6~", emit), // PageDown
        // Function keys F1-F12: the standard xterm sequences (F1-F4 are SS3 `ESC O P/Q/R/S`,
        // F5-F12 are `ESC[<n>~`). The shell acts on F1 (help); the rest are recognised and
        // consumed by its escape parser, so the physical keys are no longer dead and never
        // smear stray characters onto the line.
        0x3A => { emit(0x1B); emit(b'O'); emit(b'P'); true } // F1
        0x3B => { emit(0x1B); emit(b'O'); emit(b'Q'); true } // F2
        0x3C => { emit(0x1B); emit(b'O'); emit(b'R'); true } // F3
        0x3D => { emit(0x1B); emit(b'O'); emit(b'S'); true } // F4
        0x3E => csi(b"15~", emit), // F5
        0x3F => csi(b"17~", emit), // F6
        0x40 => csi(b"18~", emit), // F7
        0x41 => csi(b"19~", emit), // F8
        0x42 => csi(b"20~", emit), // F9
        0x43 => csi(b"21~", emit), // F10
        0x44 => csi(b"23~", emit), // F11
        0x45 => csi(b"24~", emit), // F12
        _ => match hid_to_ascii(k, mods, caps) {
            Some(ch) => { emit(ch); true }
            None => false,
        },
    }
}

/// Tracks the currently-held key so a driver can synthesise typematic auto-repeat.
/// USB HID boot keyboards report only on *change* - a held key sends one down report
/// and then nothing until release - so the host must synthesise repeat itself.
///
/// `now`, `initial`, and `interval` are in **whatever monotonic unit the driver feeds
/// in** - the drivers use `ServiceContext::read_tsc()` cycles (hardware-proven to
/// advance on real machines, unlike the coarse kernel tick), so `initial`/`interval`
/// are cycle counts (e.g. ~300 ms / ~50 ms worth at the CPU's frequency). The unit is
/// the driver's choice; this struct only compares and adds. `decode_keyboard` arms it
/// on a fresh printable press and disarms on release; the driver calls
/// [`KeyRepeat::poll`] every loop iteration with the current `now`. One per keyboard.
pub struct KeyRepeat {
    key: u8,       // HID usage of the key being repeated (0 = none armed)
    mods: u8,      // modifier byte captured at press (so Shift+key repeats the shifted form)
    caps: bool,    // Caps Lock state captured at press (so a held letter repeats in the right case)
    next_at: u64,  // `now` value at which the next repeat is due
    initial: u64,  // delay (in the driver's `now` unit) before the first repeat
    interval: u64, // delay between subsequent repeats
}

impl KeyRepeat {
    /// `initial`/`interval` are in the same unit the driver passes as `now` (TSC cycles).
    pub const fn new(initial: u64, interval: u64) -> Self {
        KeyRepeat { key: 0, mods: 0, caps: false, next_at: 0, initial, interval }
    }

    /// Construct a repeat CALIBRATED to this machine's real TSC rate, so the feel is ~300 ms
    /// initial / ~50 ms interval on ANY CPU - not just a ~2 GHz one. `ticks_per_10ms` is
    /// `ServiceContext::tsc_ticks_per_10ms()` (TSC cycles in 10 ms, PIT-calibrated by the kernel).
    /// It is 0 only when the TSC was not calibrated (QEMU, which has no USB HID keyboard anyway);
    /// there we fall back to ~2 GHz cycle counts. This removes the hidden "assume 2 GHz" that made a
    /// single keypress repeat into `qqqqq` on a differently-clocked part (the Goldmont+ Wyse).
    pub fn new_calibrated(ticks_per_10ms: u64) -> Self {
        if ticks_per_10ms == 0 {
            // Uncalibrated (QEMU): assume ~2 GHz. 600M cycles ~= 300 ms, 100M ~= 50 ms.
            KeyRepeat::new(600_000_000, 100_000_000)
        } else {
            // 300 ms = 30 * (cycles in 10 ms); 50 ms = 5 * (cycles in 10 ms).
            KeyRepeat::new(ticks_per_10ms.saturating_mul(30), ticks_per_10ms.saturating_mul(5))
        }
    }

    fn arm(&mut self, key: u8, mods: u8, caps: bool, now: u64) {
        self.key = key;
        self.mods = mods;
        self.caps = caps;
        self.next_at = now.wrapping_add(self.initial);
    }

    fn disarm(&mut self) {
        self.key = 0;
    }

    /// Is a key currently held (repeat armed)? A driver uses this to decide how long to
    /// block: a short timeout while a key is down (to wake and emit repeats), a long one
    /// otherwise (the keyboard is silent, so just idle until the next interrupt).
    pub fn armed(&self) -> bool {
        self.key != 0
    }

    /// Emit a repeat of the held key if one is due at `now`. Call once per poll
    /// iteration; a no-op until `initial` elapses, then fires at most once per `interval`.
    pub fn poll(&mut self, now: u64, mut emit: impl FnMut(u8)) {
        if self.key == 0 || now < self.next_at {
            return;
        }
        emit_key(self.key, self.mods, self.caps, &mut emit);
        self.next_at = now.wrapping_add(self.interval);
    }
}

/// Is an 8-byte HID boot report real device data, or the all-`0xff` signature of a failed/stale
/// DMA read (a device that vanished mid-transaction, or a buffer the controller never wrote)?
/// Returns `false` only for an all-`0xff` report. This is the **universal** garbage check - safe
/// for both keyboards and mice (a real mouse won't send all-`0xff`; a keyboard never does) -
/// which a driver uses to count a wedged endpoint toward disconnect and re-enumerate. (The
/// keyboard decoder additionally rejects any report whose reserved byte 1 ≠ 0, a stricter check
/// that only makes sense for keyboards.)
pub fn report_is_valid(report: &[u8; 8]) -> bool {
    *report != [0xFF; 8]
}

/// HID usage of the Delete (forward-delete) key - the `Del` in Ctrl+Alt+Del.
pub const KEY_DELETE: u8 = 0x4C;
/// HID usage of the Caps Lock key. It is a host-tracked LATCH: the modifier byte never reports it,
/// so the host toggles a `caps` flag on each fresh press (see `decode_keyboard`).
pub const KEY_CAPS_LOCK: u8 = 0x39;

/// True if a **keyboard** boot report is the Ctrl+Alt+Del chord: either Ctrl (left 0x01 / right
/// 0x10) **and** either Alt (left 0x04 / right 0x40) held, with the Delete key down. This is the
/// secure-attention reboot combo - a driver checks it each poll for a keyboard device and, when
/// true, issues the reboot syscall. Because reboot does not return, no edge-tracking is needed
/// (the first detection reboots). Apply this ONLY to keyboard reports: a mouse boot report's
/// button byte (byte 0) can alias the Ctrl/Alt modifier bits, so it must never be tested here.
pub fn is_ctrl_alt_del(report: &[u8; 8]) -> bool {
    if report[1] != 0 { return false; }                  // reserved byte ≠ 0 → invalid/stale report
    let mods = report[0];
    let ctrl = mods & 0x11 != 0;
    let alt  = mods & 0x44 != 0;
    ctrl && alt && report[2..8].contains(&KEY_DELETE)
}

/// Decode a keyboard boot report (modifiers in byte 0, up to six keycodes in
/// bytes 2..8) with N-key edge detection: `emit(ascii)` is called for every key
/// that is down now but was not in `last`, so rolling onto a new key before
/// releasing the previous one drops nothing and a held key fires exactly once.
/// `last` is updated to this report's keycodes for the next call.
///
/// `rep`/`now` drive typematic auto-repeat: the newest printable key still held is
/// armed (at tick `now`) so the driver's [`KeyRepeat::poll`] re-emits it while held;
/// releasing it disarms repeat. A key we don't map is reported via `on_unmapped`
/// (loud, not silently dropped - §3.12) so its HID usage code can be logged and added.
/// `caps` is the host's Caps Lock latch (toggled on each fresh Caps Lock press); the driver owns it
/// per keyboard and passes it in, so the state persists across reports. It cases letters via
/// `hid_to_ascii` (Caps Lock XORs Shift, letters only).
pub fn decode_keyboard(
    report: &[u8; 8],
    last: &mut [u8; 6],
    rep: &mut KeyRepeat,
    caps: &mut bool,
    now: u64,
    mut emit: impl FnMut(u8),
    mut on_unmapped: impl FnMut(u8),
) {
    // Reject an invalid report before decoding it. Byte 1 of a USB HID boot-keyboard report is
    // reserved and is always 0; an all-`0xff` report (byte 1 == 0xff) is the signature of a
    // failed/stale DMA read - what the buffer returns when the device has gone (e.g. mid-unplug)
    // or the endpoint's buffer wasn't refreshed. Decoding it would spew 0xff "keystrokes" to the
    // console AND corrupt `last` (poisoning edge-detection so later real keys diff wrong and
    // never register). Drop it untouched: don't emit, don't update `last`, don't disarm repeat -
    // so the next genuine report decodes cleanly.
    if report[1] != 0 {
        return;
    }
    let mods = report[0];
    let cur = [report[2], report[3], report[4], report[5], report[6], report[7]];
    for &k in cur.iter() {
        if k == 0 || k == 0x01 { continue; } // 0 = empty slot, 0x01 = rollover error
        if !last.contains(&k) {
            // Caps Lock is a latch, not a character: a fresh press flips the host's `caps` state
            // (which then cases letters) and emits nothing. It does NOT arm auto-repeat.
            if k == KEY_CAPS_LOCK { *caps = !*caps; continue; }
            if emit_key(k, mods, *caps, &mut emit) {
                // Newest printable/cursor key held becomes the repeat key - except the
                // one-shot control keys: Escape (0x29), whose repeat would make the shell
                // re-disambiguate a bare ESC every tick, and the function keys F1-F12
                // (0x3A-0x45), which are actions, not characters (holding F1 should not
                // re-open help over and over).
                if k != 0x29 && !(0x3A..=0x45).contains(&k) {
                    rep.arm(k, mods, *caps, now);
                }
            } else if is_typable_code(k) {
                // Modifiers/Caps/Esc (0x29, 0x39, 0xE0-E7) are not printable; only the
                // typable ranges we'd expect to map are surfaced as "unmapped" noise.
                on_unmapped(k);
            }
        }
    }
    // Stop repeating once the armed key is no longer held.
    if rep.key != 0 && !cur.contains(&rep.key) {
        rep.disarm();
    }
    *last = cur;
}

/// Left / right / middle button masks in a mouse boot report's byte 0.
pub const MOUSE_LEFT: u8 = 0x01;
pub const MOUSE_RIGHT: u8 = 0x02;
pub const MOUSE_MIDDLE: u8 = 0x04;

/// Name a mouse button mask, for logging.
pub fn button_name(mask: u8) -> &'static str {
    match mask {
        MOUSE_LEFT => "LEFT",
        MOUSE_RIGHT => "RIGHT",
        MOUSE_MIDDLE => "MIDDLE",
        _ => "?",
    }
}

/// Tracks mouse boot reports across calls: edge-detects button transitions and
/// accumulates relative motion. There is no on-screen cursor in a text console
/// (that belongs to a future display server), so the driver surfaces events by
/// logging; this struct decides *what* is worth surfacing and hands it back via
/// closures.
pub struct MouseTracker {
    buttons: u8,
    ax: i32,
    ay: i32,
}

impl MouseTracker {
    pub const fn new() -> Self {
        MouseTracker { buttons: 0, ax: 0, ay: 0 }
    }

    /// Feed one boot report (byte 0 = buttons, byte 1 = dx, byte 2 = dy as signed
    /// deltas). Calls `on_button(mask, down)` for each of left/right/middle that
    /// changed, and `on_move(dx, dy)` once accumulated motion crosses a threshold
    /// - a mouse emits far too many move reports to surface each one.
    pub fn feed(
        &mut self, report: &[u8; 8],
        mut on_button: impl FnMut(u8, bool), mut on_move: impl FnMut(i32, i32),
    ) {
        let b = report[0] & 0x07;
        let dx = report[1] as i8 as i32;
        let dy = report[2] as i8 as i32;
        let changed = b ^ self.buttons;
        for &mask in &[MOUSE_LEFT, MOUSE_RIGHT, MOUSE_MIDDLE] {
            if changed & mask != 0 {
                on_button(mask, b & mask != 0);
            }
        }
        self.buttons = b;
        self.ax += dx;
        self.ay += dy;
        if self.ax.abs() + self.ay.abs() >= 60 {
            on_move(self.ax, self.ay);
            self.ax = 0;
            self.ay = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    // Decode a single-keycode report and collect the bytes it emits.
    fn emit_for(code: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut last = [0u8; 6];
        let mut rep = KeyRepeat::new(0, 0);
        let mut caps = false;
        let report = [0, 0, code, 0, 0, 0, 0, 0];
        decode_keyboard(&report, &mut last, &mut rep, &mut caps, 0, |b| out.push(b), |_| {});
        out
    }

    #[test]
    fn nav_cluster_emits_terminal_escape_sequences() {
        // The navigation cluster a standard extended keyboard sends - each must map to
        // the exact escape sequence the shell's CSI parser / pager understands.
        assert_eq!(emit_for(0x4A), b"\x1b[H");  // Home
        assert_eq!(emit_for(0x4D), b"\x1b[F");  // End
        assert_eq!(emit_for(0x49), b"\x1b[2~"); // Insert
        assert_eq!(emit_for(0x4C), b"\x1b[3~"); // Delete
        assert_eq!(emit_for(0x4B), b"\x1b[5~"); // PageUp
        assert_eq!(emit_for(0x4E), b"\x1b[6~"); // PageDown
    }

    #[test]
    fn function_keys_emit_xterm_sequences() {
        // F1-F4 are SS3 (ESC O P/Q/R/S); F5-F12 are ESC[<n>~. F1 is the one the shell acts
        // on (help); the rest are recognised + consumed (no stray characters).
        assert_eq!(emit_for(0x3A), b"\x1bOP");   // F1
        assert_eq!(emit_for(0x3D), b"\x1bOS");   // F4
        assert_eq!(emit_for(0x3E), b"\x1b[15~"); // F5
        assert_eq!(emit_for(0x45), b"\x1b[24~"); // F12
    }

    #[test]
    fn arrows_still_emit_their_sequences() {
        assert_eq!(emit_for(0x52), b"\x1b[A"); // Up
        assert_eq!(emit_for(0x51), b"\x1b[B"); // Down
        assert_eq!(emit_for(0x4F), b"\x1b[C"); // Right
        assert_eq!(emit_for(0x50), b"\x1b[D"); // Left
    }

    #[test]
    fn ordinary_and_keypad_keys_unaffected() {
        assert_eq!(emit_for(0x04), b"a");      // letter
        assert_eq!(emit_for(0x59), b"1");      // keypad 1
        assert_eq!(emit_for(0x2A), &[0x08]);   // backspace
        assert_eq!(emit_for(0x28), b"\n");     // enter
    }

    #[test]
    fn ctrl_letter_emits_control_codes() {
        // Ctrl+letter must produce the C0 control byte a terminal sends, so USB-keyboard
        // users can reach app shortcuts (editor ^S/^Q, shell ^C). Left Ctrl = 0x01.
        assert_eq!(hid_to_ascii(0x16, 0x01, false), Some(0x13)); // Ctrl-S (save)
        assert_eq!(hid_to_ascii(0x14, 0x01, false), Some(0x11)); // Ctrl-Q (quit)
        assert_eq!(hid_to_ascii(0x06, 0x01, false), Some(0x03)); // Ctrl-C
        assert_eq!(hid_to_ascii(0x04, 0x10, false), Some(0x01)); // Ctrl-A via RIGHT Ctrl (0x10)
        assert_eq!(hid_to_ascii(0x1D, 0x01, false), Some(0x1A)); // Ctrl-Z
        // Ctrl takes precedence over Shift/Caps, and a plain letter is unchanged.
        assert_eq!(hid_to_ascii(0x16, 0x01 | 0x02, true), Some(0x13)); // Ctrl+Shift+S, Caps on → still ^S
        assert_eq!(hid_to_ascii(0x16, 0x00, false), Some(b's'));       // no Ctrl → 's'
    }

    #[test]
    fn caps_lock_cases_letters_only() {
        // Caps Lock XORs Shift, but ONLY for letters. (key 0x16 = 's', 0x1E = '1'/'!')
        assert_eq!(hid_to_ascii(0x16, 0x00, true),  Some(b'S')); // Caps on → uppercase
        assert_eq!(hid_to_ascii(0x16, 0x22, true),  Some(b's')); // Caps + Shift → lowercase (XOR)
        assert_eq!(hid_to_ascii(0x16, 0x22, false), Some(b'S')); // Shift only → uppercase
        assert_eq!(hid_to_ascii(0x16, 0x00, false), Some(b's')); // neither → lowercase
        // Digits/symbols ignore Caps Lock - only Shift changes them.
        assert_eq!(hid_to_ascii(0x1E, 0x00, true),  Some(b'1')); // Caps on, a digit → still '1'
        assert_eq!(hid_to_ascii(0x1E, 0x22, true),  Some(b'!')); // Shift → '!' (Caps irrelevant)
    }

    #[test]
    fn caps_lock_key_toggles_the_latch() {
        // The Caps Lock keycode (0x39) flips the host latch and emits nothing; letters then case.
        // Each helper decodes one fresh press (last reset to empty so the press re-triggers).
        fn decode_one(code: u8, caps: &mut bool) -> Vec<u8> {
            let mut out = Vec::new();
            let mut last = [0u8; 6];
            let mut rep = KeyRepeat::new(0, 0);
            decode_keyboard(&[0, 0, code, 0, 0, 0, 0, 0], &mut last, &mut rep, caps, 0,
                            |b| out.push(b), |_| {});
            out
        }
        let mut caps = false;
        assert_eq!(decode_one(0x16, &mut caps), b"s");  // off → 's'  (0x16 = 's')
        assert_eq!(decode_one(0x39, &mut caps), b"");   // Caps press emits nothing
        assert!(caps);
        assert_eq!(decode_one(0x16, &mut caps), b"S");  // on → 'S'
        assert_eq!(decode_one(0x39, &mut caps), b"");
        assert!(!caps);
        assert_eq!(decode_one(0x16, &mut caps), b"s");  // off again → 's'
    }

    #[test]
    fn ctrl_alt_del_chord_detected() {
        // The reboot chord: a Ctrl bit + an Alt bit + the Delete keycode (0x4C) in a key slot.
        assert!(is_ctrl_alt_del(&[0x01 | 0x04, 0, 0x4C, 0, 0, 0, 0, 0]));  // left Ctrl + left Alt + Del
        assert!(is_ctrl_alt_del(&[0x10 | 0x44, 0, 0, 0x4C, 0, 0, 0, 0]));  // right Ctrl + Alts + Del in slot 2
        // Missing any leg of the chord → not detected.
        assert!(!is_ctrl_alt_del(&[0x01, 0, 0x4C, 0, 0, 0, 0, 0]));        // Ctrl+Del, no Alt
        assert!(!is_ctrl_alt_del(&[0x04, 0, 0x4C, 0, 0, 0, 0, 0]));        // Alt+Del, no Ctrl
        assert!(!is_ctrl_alt_del(&[0x05, 0, 0, 0, 0, 0, 0, 0]));           // Ctrl+Alt, no Del
        // A stale/invalid report (reserved byte ≠ 0) is rejected even if it otherwise matches.
        assert!(!is_ctrl_alt_del(&[0x05, 0xFF, 0x4C, 0, 0, 0, 0, 0]));
    }
}
