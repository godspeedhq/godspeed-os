// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! USB HID boot-protocol decoding, shared by the USB host drivers (`xhci`, `ehci`).
//!
//! Pure logic — no syscalls, no I/O. Each driver reads the fixed 8-byte boot
//! report from its controller's DMA and hands it here; the side effects (pushing
//! a character to the console, logging a mouse event) stay in the driver, passed
//! in as closures. This is the controller-agnostic reuse §26.2 anticipated once
//! both drivers existed: the report format is identical whether the bytes arrived
//! over xHCI transfer-event rings or EHCI split qTDs.

/// Decode a HID boot-keyboard usage code to ASCII (US layout, common keys).
pub fn hid_to_ascii(key: u8, mods: u8) -> Option<u8> {
    let shift = mods & 0x22 != 0; // left or right Shift
    match key {
        0x04..=0x1D => {
            let base = b'a' + (key - 0x04);
            Some(if shift { base - 32 } else { base })
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

/// Codes in the printable-key ranges (letters, digits, punctuation, keypad) — but NOT the
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
/// byte-for-byte identical to its first press. Arrow keys → ANSI escape sequences
/// (ESC [ A/B/C/D), exactly what a serial terminal sends, so the shell's one input
/// parser handles USB and serial alike (e.g. the up-arrow history walk).
fn emit_key(k: u8, mods: u8, emit: &mut impl FnMut(u8)) -> bool {
    match k {
        0x52 => { emit(0x1B); emit(b'['); emit(b'A'); true } // Up
        0x51 => { emit(0x1B); emit(b'['); emit(b'B'); true } // Down
        0x4F => { emit(0x1B); emit(b'['); emit(b'C'); true } // Right
        0x50 => { emit(0x1B); emit(b'['); emit(b'D'); true } // Left
        _ => match hid_to_ascii(k, mods) {
            Some(ch) => { emit(ch); true }
            None => false,
        },
    }
}

/// Typematic auto-repeat timing, in `ServiceContext::monotonic_ticks` units (one tick
/// ≈ the kernel preemption period: ~50 ms on the T630 periodic timer, 10 ms under
/// TSC-Deadline). USB HID boot keyboards report only on *change* — a held key sends
/// one down report and then nothing until release — so the host must synthesise
/// repeat itself. These are deliberately coarse; the goal is "hold backspace and it
/// keeps deleting," not a configurable rate.
pub const REPEAT_INITIAL_TICKS: u64 = 5; // delay before the first repeat (~250 ms HW)
pub const REPEAT_INTERVAL_TICKS: u64 = 1; // then one repeat per tick (~20/s HW)

/// Tracks the currently-held key so a driver can synthesise typematic auto-repeat
/// from a monotonic tick. `decode_keyboard` arms it on a fresh printable press and
/// disarms it when that key is released; the driver calls [`KeyRepeat::poll`] every
/// loop iteration with the current tick to emit repeats. One per keyboard device.
pub struct KeyRepeat {
    key: u8,      // HID usage of the key being repeated (0 = none armed)
    mods: u8,     // modifier byte captured at press (so Shift+key repeats the shifted form)
    next_at: u64, // monotonic tick at which the next repeat is due
}

impl KeyRepeat {
    pub const fn new() -> Self {
        KeyRepeat { key: 0, mods: 0, next_at: 0 }
    }

    fn arm(&mut self, key: u8, mods: u8, now: u64) {
        self.key = key;
        self.mods = mods;
        self.next_at = now.wrapping_add(REPEAT_INITIAL_TICKS);
    }

    fn disarm(&mut self) {
        self.key = 0;
    }

    /// Emit a repeat of the held key if one is due at tick `now`. Call once per poll
    /// iteration; it is a no-op until the initial delay elapses, then fires at most
    /// once per `REPEAT_INTERVAL_TICKS`.
    pub fn poll(&mut self, now: u64, mut emit: impl FnMut(u8)) {
        if self.key == 0 || now < self.next_at {
            return;
        }
        emit_key(self.key, self.mods, &mut emit);
        self.next_at = now.wrapping_add(REPEAT_INTERVAL_TICKS);
    }
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
/// (loud, not silently dropped — §3.12) so its HID usage code can be logged and added.
pub fn decode_keyboard(
    report: &[u8; 8],
    last: &mut [u8; 6],
    rep: &mut KeyRepeat,
    now: u64,
    mut emit: impl FnMut(u8),
    mut on_unmapped: impl FnMut(u8),
) {
    let mods = report[0];
    let cur = [report[2], report[3], report[4], report[5], report[6], report[7]];
    for &k in cur.iter() {
        if k == 0 || k == 0x01 { continue; } // 0 = empty slot, 0x01 = rollover error
        if !last.contains(&k) {
            if emit_key(k, mods, &mut emit) {
                // Newest printable/cursor key held becomes the repeat key.
                rep.arm(k, mods, now);
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
    /// — a mouse emits far too many move reports to surface each one.
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
