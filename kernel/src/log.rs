// SPDX-License-Identifier: GPL-2.0-only
//! Kernel ring buffer - §11.4.
//!
//! 16 KiB shared sink, mirrored to the serial console at all times so panics are always visible.
//!
//! NOTHING DRAINS IT. This said it was "drained by the `events` service on startup"; `drain_to_events`
//! below has zero callers anywhere in the tree. That is not an oversight to correct - §11.4's 2026-09-04
//! amendment makes it the design: `ctx.log()` is syscall 5 writing this ring and serial DIRECTLY, so
//! logging never depends on a service that can die. Do not wire this up.
//!
//! THE BOOT RECORD is the one part of this that IS read. Beside the ring sits a fixed copy of the first
//! `BOOT_RECORD_SIZE` bytes ever logged, which fills once and then freezes: the ring wraps within
//! seconds of a busy machine, and the boot is exactly what a reader most often wants back. It is read
//! by copy (InspectKernel query 27, INTROSPECT-gated), never drained, so reading it changes nothing and
//! the property above holds - no log line depends on a reader. The shell shows it as `events log boot`.
//!
//! Unsafe boundary: none. The ring buffer is protected by a SpinLock.

use core::fmt;
use core::fmt::Write;

use crate::smp::SpinLock;

const RING_SIZE: usize = 16 * 1024;

/// How much of the boot the record keeps. A T630 reaches its prompt in about 18 KiB of log; the rest
/// holds the driver bring-up that follows it. Past this the record is full and stays as it is - later
/// lines are in the ring and on serial, and the reader says so rather than pretending it has them.
pub const BOOT_RECORD_SIZE: usize = 32 * 1024;

/// The most one read copies out, so the copy held under the lock (interrupts masked) stays short.
pub const BOOT_READ_CHUNK: usize = 512;

struct RingBuffer {
    buf: [u8; RING_SIZE],
    head: usize,
    len: usize,
    boot: [u8; BOOT_RECORD_SIZE],
    boot_len: usize,
}

impl RingBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; RING_SIZE], head: 0, len: 0, boot: [0u8; BOOT_RECORD_SIZE], boot_len: 0 }
    }

    fn write_byte(&mut self, b: u8) {
        if self.boot_len < BOOT_RECORD_SIZE {
            self.boot[self.boot_len] = b;
            self.boot_len += 1;
        }
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
/// drain-to-events sink) and stages it for serial, flushing the staged bytes to COM1 in a
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
            crate::arch::imp::serial_write_bytes_lockfree(&self.stage[..self.n]);
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
    // RING is taken from BOTH task context and interrupt context (the timer ISR's control-channel
    // drains log lines like "control: KILL supervisor"), so its hold MUST mask interrupts - the
    // `smp::without_interrupts` contract. Without it, a task preempted mid-kprintln holds RING, and any
    // code that then takes RING with preemption suppressed deadlocks forever waiting on the unreschedulable
    // holder. That is exactly the supervisor respawn (Path C / Phase 6): it pins Core 0 (no context
    // switch) and its own kprintln spun on RING held by a preempted Core-0 task - the §22 Test 15 wedge
    // under QEMU. The serial flush is bounded (THRE poll drops on timeout, SERIAL_LOCK spin is capped),
    // so the interrupts-off window stays bounded.
    crate::smp::without_interrupts(|| {
        let mut ring = RING.lock();
        let mut sink = LogSink { ring: &mut ring, stage: [0u8; SERIAL_STAGE], n: 0 };
        let _ = sink.write_fmt(args);
        sink.flush();
    });
}

/// How many bytes the boot record holds so far (at most `BOOT_RECORD_SIZE`).
pub fn boot_record_len() -> usize {
    crate::smp::without_interrupts(|| RING.lock().boot_len)
}

/// Copy the boot record from `offset` into `out`, returning how many bytes were copied: 0 at or past
/// the end. A copy, never a drain - the record is the same after any number of reads. Masked for the
/// reason `write_fmt` states; the caller bounds `out` to `BOOT_READ_CHUNK` so the hold stays short.
pub fn boot_record_read(offset: usize, out: &mut [u8]) -> usize {
    crate::smp::without_interrupts(|| {
        let ring = RING.lock();
        if offset >= ring.boot_len {
            return 0;
        }
        let n = out.len().min(ring.boot_len - offset);
        out[..n].copy_from_slice(&ring.boot[offset..offset + n]);
        n
    })
}

/// Drain the ring buffer into a sink.
///
/// UNUSED - zero callers. Kept because the masking discipline below is the correct shape for any future
/// drainer, but see the module header: logging deliberately does not flow through a service (§11.4).
pub fn drain_to_events(send: impl FnMut(u8)) {
    // Masked, for the reason `write_fmt` above states: `RING` is taken from interrupt context too, and
    // a spinlock is not reentrant - so an unmasked hold lets the timer ISR spin on a lock its own core
    // already owns. The identical hazard in `arch/arm/irq.rs::HIRES` took 1754 chaos rounds to hit, which
    // is why the masking stays even though this function is currently never called.
    crate::smp::without_interrupts(|| RING.lock().drain(send));
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
