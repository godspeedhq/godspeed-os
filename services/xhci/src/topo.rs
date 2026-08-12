// SPDX-License-Identifier: GPL-2.0-only
//! A shadow topology model: what the driver believes is attached, and where.
//!
//! **Step 1 of `docs/xhci-topology.md`, and deliberately behaviour-neutral.** Nothing here binds,
//! drops, announces or issues a single transfer. It is fed from the observations the driver ALREADY
//! makes - the root-port census and the hub port-status probes - and its only output is a log line
//! when a port changes state.
//!
//! ## Why observe before restructuring
//!
//! Seven fixes were made to this driver's hot-plug handling without anyone being able to see what it
//! believed about the world. Each was correct and each revealed another face of the same problem,
//! because the belief was spread across a dozen branches that inferred it from different signals and
//! disagreed with each other. The last boot produced zero lines from three separate paths - not a
//! wrong threshold, but code that never ran.
//!
//! So this lands first, changes nothing, and answers one question: does the model see what actually
//! happens? Including the two cases currently unobservable - booting with no stick attached, and a
//! replug after a failed probe.
//!
//! ## One evidence rule
//!
//! The driver currently has three, in three functions, that disagree: a definite "disconnected"
//! needs 2, a failed probe needs 20, and a failed block operation needs 1. Here there is one:
//!
//! * an answer of PRESENT moves evidence up,
//! * an answer of ABSENT moves it down,
//! * a FAILED probe moves it NOWHERE - a failed question is not an answer.
//!
//! That last line is the principle this port kept deriving and then contradicting with a constant.
//! Hysteresis makes a single bad read harmless without needing a special case for it.

use godspeed_sdk::ServiceContext;

/// How many ports the model tracks. Root ports plus the downstream ports of the hubs this board
/// has: comfortably above the Pi 4's 5 + 4, and a fixed array so the bound is readable here (§26.6).
const MAX_PORTS: usize = 32;

/// Evidence needed before a port changes state. Small on purpose: this is hysteresis against a
/// single bad read, not a timeout.
const THRESHOLD: i8 = 2;

#[derive(Clone, Copy, PartialEq)]
pub enum PortState {
    /// Never observed, or observed only through failed probes. Never acted upon.
    Unknown,
    Empty,
    Attached,
}

#[derive(Clone, Copy)]
struct Port {
    /// 0 = a root port; otherwise the slot of the hub this port belongs to.
    hub_slot: u32,
    port: u32,
    state: PortState,
    evidence: i8,
    used: bool,
}

pub struct Topo {
    ports: [Port; MAX_PORTS],
}

impl Topo {
    pub fn new() -> Self {
        Topo {
            ports: [Port { hub_slot: 0, port: 0, state: PortState::Unknown, evidence: 0, used: false };
                    MAX_PORTS],
        }
    }

    /// Record one observation. `present` is `None` for a FAILED probe, which is not evidence.
    ///
    /// Logs only on a state CHANGE, so a settled machine prints nothing - the property the previous
    /// diagnostic lacked, which was gated on a condition that was true when things were fine and
    /// became permanent spam.
    pub fn note(&mut self, ctx: &ServiceContext, hub_slot: u32, port: u32, present: Option<bool>) {
        let Some(present) = present else { return }; // a failed question is not an answer
        let mut idx = None;
        for (i, p) in self.ports.iter().enumerate() {
            if p.used && p.hub_slot == hub_slot && p.port == port {
                idx = Some(i);
                break;
            }
            if !p.used && idx.is_none() {
                idx = Some(i);
            }
        }
        // Table full: say so once rather than silently stop modelling (§26.7). MAX_PORTS is well
        // above this board, so reaching it means the assumption is wrong, not that the machine grew.
        let Some(i) = idx else {
            ctx.log("xhci: [topo] port table full - not modelling further ports");
            return;
        };
        let e = &mut self.ports[i];
        if !e.used {
            *e = Port { hub_slot, port, state: PortState::Unknown, evidence: 0, used: true };
        }
        e.evidence = if present {
            (e.evidence + 1).min(THRESHOLD)
        } else {
            (e.evidence - 1).max(-THRESHOLD)
        };
        let want = if e.evidence >= THRESHOLD {
            PortState::Attached
        } else if e.evidence <= -THRESHOLD {
            PortState::Empty
        } else {
            e.state
        };
        if want != e.state {
            let from = name(e.state);
            e.state = want;
            ctx.log_fmt(format_args!(
                "xhci: [topo] {}{} {} -> {}",
                if hub_slot == 0 { "root port " } else { "hub port " },
                port, from, name(want)));
        }
    }
}

fn name(s: PortState) -> &'static str {
    match s {
        PortState::Unknown => "unknown",
        PortState::Empty => "empty",
        PortState::Attached => "attached",
    }
}
