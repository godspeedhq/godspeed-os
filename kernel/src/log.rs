// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Kernel ring buffer — §11.4.
//!
//! 16 KiB shared sink. Written to before the logger service exists;
//! drained by the logger service on startup. Also mirrors to the serial
//! console at all times so panics are always visible.
//!
//! Unsafe boundary: none. The ring buffer is protected by a SpinLock.

use core::fmt;
use core::fmt::Write;

use crate::smp::SpinLock;

const RING_SIZE: usize = 16 * 1024;

struct RingBuffer {
    buf: [u8; RING_SIZE],
    head: usize,
    len: usize,
}

impl RingBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; RING_SIZE], head: 0, len: 0 }
    }

    fn write_byte(&mut self, b: u8) {
        let tail = (self.head + self.len) % RING_SIZE;
        if self.len == RING_SIZE {
            // Overwrite oldest byte, advance head.
            self.buf[self.head] = b;
            self.head = (self.head + 1) % RING_SIZE;
        } else {
            self.buf[tail] = b;
            self.len += 1;
        }
    }

    /// Drain all bytes into `f`, emptying the buffer.
    pub fn drain(&mut self, mut f: impl FnMut(u8)) {
        while self.len > 0 {
            f(self.buf[self.head]);
            self.head = (self.head + 1) % RING_SIZE;
            self.len -= 1;
        }
    }
}

impl fmt::Write for RingBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.write_byte(b);
            crate::arch::x86_64::serial_write_byte(b);
        }
        Ok(())
    }
}

static RING: SpinLock<RingBuffer> = SpinLock::new(RingBuffer::new());

pub fn write_fmt(args: fmt::Arguments) {
    let _ = RING.lock().write_fmt(args);
}

/// Drain the ring buffer into the logger service endpoint once it is ready.
pub fn drain_to_logger(send: impl FnMut(u8)) {
    RING.lock().drain(send);
}

#[macro_export]
macro_rules! kprintln {
    ($($arg:tt)*) => {
        $crate::log::write_fmt(format_args!("{}\n", format_args!($($arg)*)))
    };
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::log::write_fmt(format_args!($($arg)*))
    };
}
