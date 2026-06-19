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

static RING: SpinLock<RingBuffer> = SpinLock::new(RingBuffer::new());

/// Bytes a single log message stages for serial before flushing atomically. Covers any
/// kernel log line and the 256-byte service-log cap (+`\n`) in one flush; a longer
/// message flushes in chunks of this size (still far better than per-byte).
const SERIAL_STAGE: usize = 512;

/// `fmt::Write` sink for one log message: appends every byte to the ring buffer (the
/// drain-to-logger sink) and stages it for serial, flushing the staged bytes to COM1 in a
/// **single `SERIAL_LOCK` hold** so a concurrent console write (the shell prompt, `observe`)
/// cannot split the message mid-character. Previously the serial mirror was per-byte, taking
/// and releasing the lock for each byte, which let console output interleave into the gaps
/// and garble the boot log.
struct LogSink<'a> {
    ring: &'a mut RingBuffer,
    stage: [u8; SERIAL_STAGE],
    n: usize,
}

impl LogSink<'_> {
    fn flush(&mut self) {
        if self.n > 0 {
            crate::arch::x86_64::serial_write_bytes_lockfree(&self.stage[..self.n]);
            self.n = 0;
        }
    }
}

impl fmt::Write for LogSink<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.ring.write_byte(b);
            if self.n == SERIAL_STAGE {
                self.flush();
            }
            self.stage[self.n] = b;
            self.n += 1;
        }
        Ok(())
    }
}

pub fn write_fmt(args: fmt::Arguments) {
    let mut ring = RING.lock();
    let mut sink = LogSink { ring: &mut ring, stage: [0u8; SERIAL_STAGE], n: 0 };
    let _ = sink.write_fmt(args);
    sink.flush();
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
