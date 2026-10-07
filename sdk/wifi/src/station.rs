// SPDX-License-Identifier: GPL-2.0-only
//! A `Station`: what the serve loop asks of a radio, whichever radio it is.
//!
//! The loop owns the policy every radio shares - the scan cache, the credential table and `/wifi.keys`,
//! auto-join and the rejoin after `radio on`, which key to join with, every reply's layout. A `Station`
//! owns the chip: how a scan is started and read, how a join is done, what "up" and "the link" mean to
//! this firmware, and how frames move. The Pi 4's Broadcom is the first; the VisionFive 2's AIC8800 is
//! next, and a soft-MAC radio (the Pi 2's Realtek) becomes one through a host-side MLME.
//!
//! Every method is a bounded exchange with the chip; none of them decides anything the loop does.

use godspeed_sdk::ServiceContext;

use crate::bss::Scan;
use crate::rxq::RxQueue;

/// What one turn of a sweep did.
pub enum ScanStep {
    /// A frame arrived and was accounted for (read, counted or skipped - all three are progress).
    Frame,
    /// Nothing arrived. The caller decides how many of these it will tolerate; this does not sleep.
    Empty,
    /// The firmware said the sweep is over, and why.
    Ended(&'static str),
}

/// How a join ended. Mirrors `crate::wire`'s connect statuses one for one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Joined,
    NotFound,
    PassphraseRefused,
    Failed,
    Timeout,
    /// Kept for the reply table; no path produces it now that the handshake is answered.
    HandshakeUnimplemented,
}

/// What the station joins WITH. The passphrase never reaches this module: it is turned into the pairwise
/// master key the moment it arrives (`crypto::psk`) and only the key is kept.
#[derive(Clone, Copy)]
pub enum Secret<'a> {
    /// No key at all - `bwfm_connect`'s final `else`: `wpa_auth` DISABLED, `wsec` NONE, no RSN element, and
    /// no handshake to wait for. The link coming up IS the join.
    Open,
    /// WPA2-PSK with this pairwise master key.
    Pmk(&'a [u8; crate::crypto::PMK_LEN]),
}

/// What the firmware says about the link RIGHT NOW - not what this driver remembers of its last join.
pub struct Link {
    /// The access point, or all zeros when the radio is not associated.
    pub bssid: [u8; 6],
    /// dBm. Meaningful only when associated.
    pub rssi: i32,
    /// The `chanspec` iovar: band in bits 15:14, channel in the low 8.
    pub chanspec: u16,
}

impl Link {
    pub fn associated(&self) -> bool {
        self.bssid.iter().any(|&b| b != 0)
    }
}

/// What one pull saw, so the serve loop can act on it without this module knowing the join state.
pub struct Pulled {
    /// Data frames queued for the stack.
    pub data: u32,
    /// Group-key rekeys answered: the new group key verified, installed and acknowledged.
    pub rekeyed: u32,
    /// Group-key frames that could not be answered - no keys held, a stale replay counter, a bad MIC, key
    /// data that would not unwrap, no group key inside, or a refused install. Each is logged where it
    /// happens; the count is for the caller.
    pub rekey_failed: u32,
    /// Pairwise rekeys answered: the access point restarted the four-way handshake on the live link, and
    /// it ran to message 4 with new keys installed.
    pub pairwise_rekeyed: u32,
    /// Pairwise rekeys that did not complete - the log names the step; the access point will drop the
    /// link and `wifi join` brings it back.
    pub pairwise_failed: u32,
    /// The link went down: `(event code, reason)`. A `LINK` event without the up bit, or a
    /// deauthentication or disassociation, in either direction.
    pub dropped_link: Option<(u32, u32)>,
}

/// A radio, as the serve loop drives it.
pub trait Station {
    /// Start a sweep. `false` when the firmware refused it.
    fn scan_start(&mut self, ctx: &ServiceContext) -> bool;
    /// Advance a running sweep by one turn, keeping what was heard in `scan`.
    fn scan_step(&mut self, scan: &mut Scan, ctx: &ServiceContext) -> ScanStep;
    /// Stop the sweep. `false` when the firmware did not take the abort.
    fn scan_abort(&mut self, ctx: &ServiceContext) -> bool;
    /// How many empty turns in a row end a sweep the firmware never said was over.
    fn scan_empty_bound(&self) -> u32;
    /// Join `ssid` with `secret`, the handshake included. The keys a WPA2 join keeps for the rekeys to
    /// come stay inside the station.
    fn join(&mut self, ssid: &[u8], secret: Secret, ctx: &ServiceContext) -> Outcome;
    /// Drop the keys of the last join, zeroed. Every end of an association calls it.
    fn forget_keys(&mut self);
    /// Leave the network.
    fn disassoc(&mut self, ctx: &ServiceContext) -> bool;
    /// The firmware's radio switch, off.
    fn radio_down(&mut self, ctx: &ServiceContext) -> bool;
    /// The firmware's radio switch, on again (the bring-up's own chain).
    fn radio_up(&mut self, ctx: &ServiceContext) -> bool;
    /// Is the firmware's radio up? `None` when it did not answer.
    fn is_up(&mut self, ctx: &ServiceContext) -> Option<bool>;
    /// The link as the firmware reports it now. `None` when it did not answer.
    fn link(&mut self, ctx: &ServiceContext) -> Option<Link>;
    /// The station's own address. `None` when it could not be read.
    fn mac(&mut self, ctx: &ServiceContext) -> Option<[u8; 6]>;
    /// Whether the chip will take a frame now.
    fn tx_ok(&self) -> bool;
    /// Send one ethernet frame.
    fn send(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool;
    /// Read what the chip has waiting into `rxq`, answering a rekey and watching the link on the way.
    fn pull(&mut self, rxq: &mut RxQueue, ctx: &ServiceContext) -> Pulled;
    /// The name of a firmware event code, for the log.
    fn event_name(&self, code: u32) -> &'static str;
    /// The driver's account of itself for `wifi debug`: write the reply for sub-command `sub` into `out`
    /// and return its length. `live` is whether the firmware may be asked anything now.
    fn debug(&mut self, sub: u8, live: bool, out: &mut [u8], ctx: &ServiceContext) -> usize;
    /// The station's facts for `wifi hardware <radio>` (`wire::OP_HARDWARE_DETAIL`), after the host's:
    /// the address, the firmware, whatever needs the chip up. `live` is `debug`'s: whether the chip may be
    /// asked anything now. A station that adds nothing leaves the host's facts as the whole answer.
    fn details(&mut self, _d: &mut crate::serve::Details, _live: bool, _ctx: &ServiceContext) {}
}
