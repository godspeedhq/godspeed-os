// SPDX-License-Identifier: Apache-2.0
//! Waiting for a device's INTERRUPT - and for requests, on the same endpoint, without losing one.
//!
//! # Why this exists
//!
//! A driver that takes interrupts idles in one place: a timed receive on its endpoint. The kernel
//! delivers the device's interrupt there as a message (CLAUDE.md 12.2), and the driver's clients send
//! their requests there too, because a task owns exactly one endpoint. So every wake is one of three
//! things - the interrupt, a request, or the deadline - and the driver must tell them apart.
//!
//! `xhci`, `ehci` and `dwc2` each wrote that out by hand, and the copies record what it cost:
//!
//! - **A request taken and dropped.** The receive CONSUMES; it does not peek. `dwc2` took a message,
//!   found no disk, and let it go - and `block-driver`, blocked in request/reply, hung before printing a
//!   line, with `fs` behind it. The same `let _ =` once cost the keyboard its interrupts.
//! - **Any message counted as an interrupt.** `xhci` set its "waking on interrupts" flag on every wake,
//!   and with a disk attached disk requests set it, so a log line that read as proof of MSI proved only
//!   that something had arrived.
//! - **A zero deadline.** `recv_timeout(0)` blocks FOREVER, so a computed budget that rounds to zero
//!   turns a watchdog into a hang.
//!
//! This is the one wait, written once, from those three.
//!
//! # The shape
//!
//! ```ignore
//! use godspeed::driver::irq::{Irq, Woke};
//! use godspeed::driver::wait::Budget;
//!
//! let irq = Irq::granted(ctx);              // None-vector is honest: no interrupt was routed
//! loop {
//!     match irq.wait(ctx, Budget::ms(150)) {
//!         Woke::Interrupt => { /* read and clear the device's status; then */ irq.rearm(ctx); }
//!         Woke::Request(msg) => serve(ctx, &msg), // SERVED, never dropped
//!         Woke::Timeout => { /* the watchdog: look anyway, an interrupt can be lost */ }
//!     }
//!     do_the_work();
//! }
//! ```
//!
//! **With no interrupt the same loop is a paced poll that still serves requests**, so a driver keeps
//! one loop rather than an interrupt loop and a polling loop that drift apart. [`Irq::routed`] says
//! which case it is in, for a driver that should say so (invariant 12: a driver that wanted an
//! interrupt and polls must not do it silently).
//!
//! # What it will not do for you
//!
//! **It does not read the device.** What raised the interrupt, and how to clear it, are the device's;
//! a driver that does not clear its source before [`Irq::rearm`] gets the interrupt again at once.
//!
//! **It does not log**, for the reason `wait` does not: only the driver knows what a timeout means.
//!
//! **It cannot tell a one-byte request equal to the vector from an interrupt.** The kernel's
//! notification is a one-byte payload naming the vector, and that is the only mark it carries - the
//! convention all three drivers already relied on. A protocol whose requests are longer than one byte,
//! as every protocol in the tree is, never meets it.

#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

#[cfg(not(test))]
use super::wait::Budget;
#[cfg(not(test))]
use crate::ipc::Message;

/// Is a message with this payload the kernel's notification of interrupt `vector`?
pub(crate) fn is_interrupt(payload: &[u8], vector: Option<u8>) -> bool {
    match vector {
        Some(v) => payload == [v],
        None => false,
    }
}

/// Counter ticks for a receive deadline of `us` microseconds, NEVER zero: zero blocks forever. On an
/// uncalibrated machine (`per_10ms == 0`) that is 1, which the kernel floors to one scheduler quantum -
/// the same answer `ServiceContext::duration_cycles` gives.
pub(crate) fn deadline_ticks(per_10ms: u64, us: u64) -> u64 {
    if per_10ms == 0 { 1 } else { super::wait::ticks_for(per_10ms, us) }
}

/// What ended a wait.
#[cfg(not(test))]
pub enum Woke {
    /// The device's interrupt. Clear its source, then [`Irq::rearm`].
    Interrupt,
    /// A request from a client. It was taken off the endpoint and is now the caller's to SERVE - its
    /// reply capability, if it carries one, is waiting to be taken (`gs::ipc::take_sent_cap`).
    Request(Message),
    /// The deadline passed with nothing arriving.
    Timeout,
}

/// The interrupt this driver was granted, if any, and how many it has seen.
#[cfg(not(test))]
pub struct Irq {
    vector: Option<u8>,
    seen: core::cell::Cell<u64>,
}

#[cfg(not(test))]
impl Irq {
    /// The interrupt the kernel routed to this driver at spawn. A driver asks for one in its spawn row
    /// (`hwclass::pci_irq`) and never names a vector: routing a vector is authority the kernel keeps.
    pub fn granted(ctx: &ServiceContext) -> Self {
        Irq { vector: ctx.irq_vector(), seen: core::cell::Cell::new(0) }
    }

    /// Was an interrupt routed at all? `false` means every [`Irq::wait`] ends on a request or its deadline.
    pub fn routed(&self) -> bool {
        self.vector.is_some()
    }

    /// The vector the kernel routed, for a driver that reports it or tells its notice apart itself.
    ///
    /// Reading it is not routing it: the vector is the kernel's to choose (see [`Irq::granted`]), and a
    /// driver that knows the number still cannot ask for another one.
    pub fn vector(&self) -> Option<u8> {
        self.vector
    }

    /// Interrupts seen by [`Irq::wait`] so far - evidence, for a driver to report.
    pub fn seen(&self) -> u64 {
        self.seen.get()
    }

    /// Wait for the interrupt, a request or `within`, whichever is first.
    ///
    /// `#[inline(always)]` because it returns a 4 KiB message by value; as its own frame that would be
    /// 4 KiB of stack on every caller (the SDK's `recv_timeout` says the same).
    #[inline(always)]
    pub fn wait(&self, ctx: &ServiceContext, within: Budget) -> Woke {
        let ticks = deadline_ticks(ctx.tsc_ticks_per_10ms(), within.as_us());
        match ctx.recv_timeout(ticks) {
            None => Woke::Timeout,
            Some(m) if is_interrupt(m.payload_bytes(), self.vector) => {
                self.seen.set(self.seen.get().saturating_add(1));
                Woke::Interrupt
            }
            Some(m) => Woke::Request(m),
        }
    }

    /// Let the interrupt fire again, after the driver has cleared its source. A level-triggered line is
    /// held masked by the kernel while it is being handled, so it cannot storm; this re-opens it. For an
    /// MSI, which is an edge and is never masked, it is a no-op - so a driver calls it either way and does
    /// not have to know which it was given.
    pub fn rearm(&self, ctx: &ServiceContext) {
        if let Some(v) = self.vector {
            ctx.irq_unmask(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_vector_alone_is_an_interrupt() {
        assert!(is_interrupt(&[0x41], Some(0x41)));
        assert!(!is_interrupt(&[0x42], Some(0x41))); // another vector's notice is not ours
        assert!(!is_interrupt(&[0x41, 0x00], Some(0x41))); // a longer message is a request
        assert!(!is_interrupt(&[], Some(0x41)));
        assert!(!is_interrupt(&[0x41], None)); // no interrupt routed: nothing is one
    }

    #[test]
    fn a_deadline_is_never_zero() {
        assert_eq!(deadline_ticks(0, 150_000), 1); // uncalibrated: one quantum, not "forever"
        assert_eq!(deadline_ticks(540_000, 0), 1); // a zero budget still ends
        assert_eq!(deadline_ticks(540_000, 10_000), 540_000);
    }
}
