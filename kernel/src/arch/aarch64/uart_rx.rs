// SPDX-License-Identifier: GPL-2.0-only
//! Serial input: draining the PL011 receive FIFO into a ring the console reader pops from.
//!
//! Nothing on this port has read input before. Output has been one-way since milestone 1, which is why
//! there is still no prompt - a shell that cannot be typed at is not a shell.
//!
//! ## The lesson inherited from the 32-bit port, which is not optional
//!
//! **The PL011 reports receive errors in the SAME read as the data**, in `DR` bits 11:8 (framing,
//! parity, break, overrun). Masking them off - the obvious thing to write - silently promotes line
//! noise to input.
//!
//! That is not a theoretical concern. On the Pi 2, a GPIO HAT sitting on the UART pins held GPIO15 (RX)
//! low, which the PL011 reports as a continuous **break** condition. Each one enqueued a spurious
//! `0x00`, so the ring filled with nulls forever, and every reader spun: a full-screen editor blocked
//! on `ConsoleRead` woke on each null, discarded it as unprintable, and repainted - 966 full-screen
//! repaints while the document changed twice. Dropping a flagged byte costs nothing on a healthy line
//! and turns a flood back into silence on a broken one.
//!
//! ## Why a stuck line gets switched off rather than tolerated
//!
//! A faulty RX line does not merely deliver junk, it **starves the machine's other input**: the ring is
//! shared with the keyboard, so a producer filling it faster than anything drains it means real
//! keystrokes are dropped for want of space. That is how a Pi 2 with a HAT became unquittable - the
//! editor repainted on phantom bytes and Ctrl+Q never got in.
//!
//! So once the line is provably not a console, we stop listening. The threshold is a **duration** of
//! unbroken overflow, not a byte count: a count is not a measure of how long a condition has persisted,
//! and picking one is how you get a threshold that either fires on a fast typist or never fires at all.
//! Serial *output* is untouched (different pin; this only clears `RXE`), and it latches until reboot -
//! silently resuming a line just declared faulty would be the silent fallback §26.7 forbids.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Receive ring. Fixed size, in `.bss` - a bounded, visible footprint (§26.6.1).
const RX_BUF_SIZE: usize = 256;
static mut RX_BUF: [u8; RX_BUF_SIZE] = [0; RX_BUF_SIZE];
static RX_HEAD: AtomicU32 = AtomicU32::new(0);
static RX_TAIL: AtomicU32 = AtomicU32::new(0);

/// Diagnostics, so a misbehaving line is answerable from the log rather than from guesswork (§26.4).
static RX_LINE_ERRORS: AtomicU32 = AtomicU32::new(0);
static RX_OVERRUN: AtomicU32 = AtomicU32::new(0);

/// When the current unbroken run of ring-full overflow started, in counter ticks; 0 = not overflowing.
static RX_OVERFLOW_SINCE: AtomicU32 = AtomicU32::new(0);
/// Set once the receiver has been switched off. Latches until reboot.
static RX_SHUT_OFF: AtomicBool = AtomicBool::new(false);

/// How long the ring may be continuously full before the line is declared not-a-console. Two seconds
/// of solid overflow is far beyond any human typing or any paste; it means something is holding the
/// line. A **duration**, deliberately - see the module header.
const RX_OVERFLOW_FAULT_SECS: u64 = 2;

/// `DR` bits 11:8: framing, parity, break, overrun. Read with the data, not separately.
const PL011_DR_ERR: u32 = 0xF00;

/// Drain everything currently in the receive FIFO into the ring.
///
/// Called from the timer tick and directly by a blocked console reader - the latter because a timer ISR
/// starved under load would otherwise leave a byte stranded in the FIFO with a reader asleep waiting
/// for it.
pub fn drain() {
    if RX_SHUT_OFF.load(Ordering::Relaxed) {
        return; // the line is stuck and we are no longer listening to it
    }
    // **Reentrancy guard, and it is not optional.** This is reachable from three places: the timer
    // tick, a console reader blocked in a syscall, and any task that polls. It reads and advances the
    // ring's tail and then calls `wake_by_slot`, which touches scheduler state. A task interrupted
    // half-way through and re-entered from the tick would advance the tail twice from the same stale
    // value, and would re-enter the wake path while the scheduler is mid-update.
    //
    // Masking is the right tool rather than a lock: the body is short, bounded (at most one FIFO's
    // worth), and runs on the core that owns the ring, so there is nothing to contend with once
    // interrupts are out of the way.
    crate::smp::without_interrupts(drain_locked)
}

/// The body of [`drain`], with interrupts already masked.
fn drain_locked() {

    let mut woke = false;
    let mut stuck: Option<(u8, u32)> = None;

    // SAFETY: PL011 flag and data registers, reached through the kernel's peripheral mapping. Volatile
    // reads with no side effect beyond removing a byte from the FIFO, which is the intent.
    unsafe {
        // Bounded: a wedged UART that never clears RXFE must not spin the timer ISR forever
        // (invariant 12). The FIFO is 32 entries, so this cap is generous and still finite.
        for _ in 0..64 {
            let fr = (super::mmio(super::PL011_FR as usize) as *const u32).read_volatile();
            if fr & super::PL011_FR_RXFE != 0 {
                break; // receive FIFO empty
            }
            let dr = (super::mmio(super::PL011_DR as usize) as *const u32).read_volatile();

            if dr & PL011_DR_ERR != 0 {
                // Clear the sticky error status (any write clears) and DISCARD the byte. See the
                // module header for what keeping it costs.
                (super::mmio(super::PL011_ECR as usize) as *mut u32).write_volatile(0);
                RX_LINE_ERRORS.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let b = (dr & 0xFF) as u8;
            let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
            let head = RX_HEAD.load(Ordering::Acquire) as usize;
            let next = (tail + 1) % RX_BUF_SIZE;

            if next == head {
                // Ring full. Drop the byte but COUNT it, and time how long this has been going on.
                let over = RX_OVERRUN.fetch_add(1, Ordering::Relaxed) + 1;
                let now = (super::read_cycle_counter() / super::timer::frequency().max(1)) as u32;
                let since = RX_OVERFLOW_SINCE.load(Ordering::Relaxed);
                if since == 0 {
                    RX_OVERFLOW_SINCE.store(now | 1, Ordering::Relaxed); // never 0 ("not overflowing")
                } else if (now.wrapping_sub(since)) as u64 > RX_OVERFLOW_FAULT_SECS
                    && !RX_SHUT_OFF.swap(true, Ordering::AcqRel)
                {
                    let cr_addr = super::mmio(super::PL011_CR as usize) as *mut u32;
                    let cr = cr_addr.read_volatile();
                    cr_addr.write_volatile(cr & !(1 << 9)); // clear RXE - stop receiving
                    stuck = Some((b, over));
                }
                continue; // keep draining the FIFO either way
            }

            RX_BUF[tail] = b;
            RX_TAIL.store(next as u32, Ordering::Release);
            RX_OVERFLOW_SINCE.store(0, Ordering::Relaxed); // a byte landed: not overflowing
            woke = true;
        }
    }

    if let Some((b, over)) = stuck {
        crate::kprintln!(
            "aarch64: serial RX SHUT OFF - the line overflowed the ring continuously for {}s \
             ({} bytes dropped, last {:#04x}, {} line errors). Output is unaffected; reboot to re-enable.",
            RX_OVERFLOW_FAULT_SECS, over, b, RX_LINE_ERRORS.load(Ordering::Relaxed)
        );
    }

    if woke {
        wake_console_reader();
    }
}

/// Keystrokes discarded because the ring was full. Counted, never silent - see [`push`].
static KBD_DROPPED: AtomicU32 = AtomicU32::new(0);

/// Push one byte into the console input ring from a producer that is **not** the UART - a keyboard.
///
/// The ring is deliberately shared with serial rather than duplicated: the console has one input
/// stream, and a reader blocked on it should not have to know which device a byte came from. That is
/// also why the UART side shuts a stuck line off (see the module header) - a producer that floods this
/// ring starves the other one.
///
/// A full ring means a KEYSTROKE is being lost, which is the one drop here that a user notices and
/// cannot explain. It is counted and reported the first time, rather than letting a keyboard that
/// appears dead give no account of itself (invariant 12).
pub fn push(b: u8) {
    let tail = RX_TAIL.load(Ordering::Relaxed) as usize;
    let head = RX_HEAD.load(Ordering::Acquire) as usize;
    let next = (tail + 1) % RX_BUF_SIZE;
    if next != head {
        // SAFETY: `tail` is in bounds by construction and this is the only writer of that slot until
        // the Release store below publishes it.
        unsafe { RX_BUF[tail] = b };
        RX_TAIL.store(next as u32, Ordering::Release);
        wake_console_reader();
    } else if KBD_DROPPED.fetch_add(1, Ordering::Relaxed) == 0 {
        crate::kprintln!(
            "aarch64: console input ring FULL - a keystroke was dropped (nothing is draining it)"
        );
    }
}

/// Take one byte, if there is one.
pub fn pop() -> Option<u8> {
    let head = RX_HEAD.load(Ordering::Relaxed) as usize;
    if head == RX_TAIL.load(Ordering::Acquire) as usize {
        return None;
    }
    // SAFETY: `head != tail`, so this slot holds a byte the producer published with a Release store.
    let b = unsafe { RX_BUF[head] };
    RX_HEAD.store(((head + 1) % RX_BUF_SIZE) as u32, Ordering::Release);
    Some(b)
}

/// Wake a task blocked in `ConsoleRead`, if one is waiting.
///
/// The waiter registers its slot *before* it checks the ring, so a byte arriving in that window still
/// wakes it - the lost-wakeup race the neutral side is careful about, and which this must not reopen.
fn wake_console_reader() {
    let slot = super::CONSOLE_READ_WAITER.load(Ordering::Acquire);
    if slot != u32::MAX {
        crate::task::scheduler::wake_by_slot(slot as usize, 0);
    }
}

/// Report the receive path's state. Called once after init so "is serial input alive?" is answerable
/// from the boot log rather than by typing and hoping.
pub fn report() {
    crate::kprintln!(
        "aarch64: serial RX ready - {} byte ring, PL011 error bits discarded (DR[11:8]), \
         line shut off after {}s of unbroken overflow",
        RX_BUF_SIZE, RX_OVERFLOW_FAULT_SECS
    );
}
