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
        0x33 => Some(if shift { b':' } else { b';' }),
        0x34 => Some(if shift { b'"' } else { b'\'' }), // apostrophe / quote
        0x35 => Some(if shift { b'~' } else { b'`' }),  // grave / tilde
        0x36 => Some(if shift { b'<' } else { b',' }),
        0x37 => Some(if shift { b'>' } else { b'.' }),
        0x38 => Some(if shift { b'?' } else { b'/' }),
        _ => None,
    }
}

/// Decode a keyboard boot report (modifiers in byte 0, up to six keycodes in
/// bytes 2..8) with N-key edge detection: `emit(ascii)` is called for every key
/// that is down now but was not in `last`, so rolling onto a new key before
/// releasing the previous one drops nothing and a held key fires exactly once.
/// `last` is updated to this report's keycodes for the next call.
pub fn decode_keyboard(report: &[u8; 8], last: &mut [u8; 6], mut emit: impl FnMut(u8)) {
    let mods = report[0];
    let cur = [report[2], report[3], report[4], report[5], report[6], report[7]];
    for &k in cur.iter() {
        if k == 0 || k == 0x01 { continue; } // 0 = empty slot, 0x01 = rollover error
        if !last.contains(&k) {
            // Arrow keys → ANSI escape sequences (ESC [ A/B/C/D), exactly what a serial
            // terminal sends, so the shell's one input parser handles both paths (e.g. the
            // up-arrow history walk). Other keys decode to ASCII.
            match k {
                0x52 => { emit(0x1B); emit(b'['); emit(b'A'); } // Up
                0x51 => { emit(0x1B); emit(b'['); emit(b'B'); } // Down
                0x4F => { emit(0x1B); emit(b'['); emit(b'C'); } // Right
                0x50 => { emit(0x1B); emit(b'['); emit(b'D'); } // Left
                _ => if let Some(ch) = hid_to_ascii(k, mods) { emit(ch); }
            }
        }
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
