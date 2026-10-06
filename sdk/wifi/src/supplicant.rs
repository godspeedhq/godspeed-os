// SPDX-License-Identifier: GPL-2.0-only
//! The station's half of WPA2, run by the host on every radio: the four-way handshake (`Handshake`) and
//! the group-key rekey that follows it (`group_rekey`), with the keys an association keeps between the
//! two (`Keys`).
//!
//! The steps are OpenBSD net80211's, as the Broadcom driver first carried them (`services/wifi-driver`,
//! `join.rs` and `frames.rs`, verified on the Pi 4 2026-09-28 to 2026-10-02). They moved here unchanged so
//! the VisionFive 2's AIC8800 runs the SAME handshake: what differs between the radios is only how an
//! EAPOL frame is sent and how a key reaches the firmware, and that is the `KeyPath` each radio supplies.
//! The frame layer under this - recognising, signing, verifying and unwrapping key frames - is `eapol`.
//!
//! ## The handshake, quoted from net80211
//!
//! Message 1 arrives with `PAIRWISE | KEYACK` and the ANonce (`ieee80211_recv_4way_msg1`). The station
//! draws an SNonce, derives the PTK (`ieee80211_derive_ptk`), and answers with message 2: `PAIRWISE |
//! KEYMIC`, the same replay counter, the SNonce, and the RSN element it put in its association request as
//! key data, MIC'd with the KCK (`ieee80211_send_4way_msg2`). Message 3 arrives with `PAIRWISE | KEYACK |
//! KEYMIC | INSTALL | ENCRYPTED`: its ANonce must match, its MIC must verify under the KCK, and its key
//! data - AES-key-wrapped under the KEK - holds the group key as a KDE (`ieee80211_recv_4way_msg3`).
//! Message 4 is `PAIRWISE | KEYMIC | SECURE`, empty, MIC'd (`ieee80211_send_4way_msg4`). Then the
//! pairwise key (index 0, the access point's address) and the group key (its key id) are installed.
//!
//! ## How a wrong passphrase presents
//!
//! The access point never says so. It receives message 2, cannot verify a MIC made with the wrong PMK,
//! and repeats message 1, then deauthenticates with reason 15 (4-way handshake timeout). So "incorrect
//! passphrase" is decided here: message 1 arriving again after two answers. The deauthentication half is
//! the radio driver's, because only it knows how its firmware reports one.

use godspeed_sdk::ServiceContext;

use crate::crypto::{self, PMK_LEN};
use crate::eapol::{self, info};

/// An Ethernet header: the EAPOL frame follows it.
const ETHHDR: usize = 14;

/// How a key frame leaves this station and how a key reaches the firmware - the two things a radio
/// supplies to the handshake. Each is one bounded exchange with the chip; `false` means it did not happen,
/// and the caller has already said so in the log or will.
pub trait KeyPath {
    /// Send one ethernet frame carrying EAPOL (`eth` begins with the 14-byte header).
    fn send_eapol(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool;
    /// Install a CCMP key: the pairwise key at index 0 against `peer`, or a group key at its key id with
    /// `peer` `None`.
    fn install_key(&mut self, key_idx: u32, key: &[u8; 16], peer: Option<&[u8; 6]>, ctx: &ServiceContext) -> bool;
}

/// What a WPA2 join KEEPS for the life of the association, and nothing more: the confirmation and
/// encryption halves of the pairwise transient key, the last replay counter the access point used, and
/// the two addresses the answers carry. The temporal key itself lives in the firmware from the moment it
/// is installed and is not kept here. These exist for one reason - the access point rekeys the group key
/// on a timer and each rekey is a signed, wrapped frame that must be verified and answered
/// (`group_rekey`) - and they are zeroed the moment the association ends (`forget`).
pub struct Keys {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    /// The pairwise master key the association was made with, for a pairwise rekey: the access point
    /// may restart the four-way handshake at any time, and answering it derives a new PTK from this.
    pub pmk: [u8; PMK_LEN],
    /// The highest replay counter accepted; a frame at or below it is a replay (`ni_replaycnt`).
    pub replay: u64,
    /// Our address, for the frames we send.
    pub mac: [u8; 6],
}

/// Zero and drop the kept keys. Called on leave, radio off, a dropped link, and at the start of a join.
pub fn forget(keys: &mut Option<Keys>) {
    if let Some(k) = keys.as_mut() {
        k.kck.fill(0);
        k.kek.fill(0);
        k.pmk.fill(0);
        k.replay = 0;
    }
    *keys = None;
}

/// The station's nonce for one handshake. The hardware RNG where the kernel exposes one (`hw_random`: the
/// Pi 4's RNG200, the VisionFive's JH7110 TRNG); where it does not, or it fails, the cycle counter, the
/// access point's own nonce and our address hashed together, and the log SAYS SO, because a nonce from a
/// counter is a real weakening that must not pass unremarked.
fn snonce(ctx: &ServiceContext, who: &str, anonce: &[u8; 32], mac: &[u8; 6]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut from_hw = true;
    for i in 0..8 {
        match ctx.hw_random() {
            Some(r) => out[4 * i..4 * i + 4].copy_from_slice(&r.to_le_bytes()),
            None => {
                from_hw = false;
                break;
            }
        }
    }
    if from_hw {
        return out;
    }
    ctx.log_fmt(format_args!("{}: NO HARDWARE RNG on this board - the SNonce is hashed from the cycle counter, the AP's nonce and our address (weaker than the standard intends; recorded, not hidden)", who));
    let mut seed = [0u8; 8 + 8 + 32 + 6 + 1];
    seed[0..8].copy_from_slice(&ctx.read_tsc().to_le_bytes());
    seed[8..16].copy_from_slice(&ctx.epoch_secs_monotonic().to_le_bytes());
    seed[16..48].copy_from_slice(anonce);
    seed[48..54].copy_from_slice(mac);
    for half in 0..2u8 {
        seed[54] = half;
        let mut s = crypto::Sha1::new();
        s.update(&seed);
        let d = s.finish();
        let take = if half == 0 { 20 } else { 12 };
        out[half as usize * 20..half as usize * 20 + take].copy_from_slice(&d[..take]);
    }
    out
}

/// The station's side of the WPA2 four-way handshake, one message at a time - `ieee80211_recv_4way_msg1`
/// and `_msg3` from OpenBSD's net80211. A struct rather than a loop because two callers drive it: a join
/// feeds it the frames of a fresh association, and a pull feeds it the frames of a PAIRWISE REKEY, when
/// the access point restarts the handshake on a link that is already up. The steps, the checks and the
/// words are the same in both.
pub struct Handshake {
    /// The service running it; every line it logs opens with this name.
    who: &'static str,
    pmk: [u8; PMK_LEN],
    our_mac: [u8; 6],
    anonce: [u8; 32],
    have_anonce: bool,
    our_nonce: [u8; 32],
    ptk: Option<eapol::Ptk>,
    /// Message 2s sent. A THIRD message 1 with the same ANonce after two answers is the access point
    /// saying our key is not its key - the one pattern that means "incorrect passphrase".
    pub msg2_sent: u32,
}

/// What one key frame did to the handshake.
pub enum Step {
    /// Read, answered if it needed answering; keep reading.
    Continue,
    /// Message 3 verified and answered, both keys installed: the association has keys.
    Joined(Keys),
    /// The access point does not accept our answers: our key is not its key.
    PassphraseRefused,
    /// A step failed for a reason that is not the key; the log has it.
    Failed,
}

impl Handshake {
    pub fn new(pmk: [u8; PMK_LEN], our_mac: [u8; 6], who: &'static str) -> Self {
        Handshake { who, pmk, our_mac, anonce: [0; 32], have_anonce: false, our_nonce: [0; 32], ptk: None, msg2_sent: 0 }
    }

    /// One EAPOL-Key frame (a whole ethernet frame), from the access point.
    pub fn on_key_frame(&mut self, path: &mut dyn KeyPath, eth_frame: &[u8], ctx: &ServiceContext) -> Step {
        let key = match eapol::describe(eth_frame, ctx, self.who) {
            Some(k) => k,
            None => return Step::Continue,
        };
        let is_pairwise = key.info & info::PAIRWISE != 0;
        let is_ack = key.info & info::KEYACK != 0;
        let is_mic = key.info & info::KEYMIC != 0;
        let mut tx = [0u8; ETHHDR + 99 + 64];

        // ---- Message 1: the access point's nonce. Derive the PTK, answer with ours. ----
        if is_pairwise && is_ack && !is_mic {
            if self.msg2_sent >= 2 && self.have_anonce && key.nonce == self.anonce {
                ctx.log_fmt(format_args!("{}: the access point repeated message 1 after two answers - our key is not its key: INCORRECT PASSPHRASE", self.who));
                return Step::PassphraseRefused;
            }
            self.anonce = key.nonce;
            self.have_anonce = true;
            if self.msg2_sent == 0 {
                self.our_nonce = snonce(ctx, self.who, &self.anonce, &self.our_mac);
            }
            let derived = eapol::derive_ptk(&self.pmk, &key.from, &self.our_mac, &self.anonce, &self.our_nonce);
            let n = eapol::build_key_frame(
                &mut tx, &key.from, &self.our_mac,
                info::PAIRWISE | info::KEYMIC,
                key.replay, &self.our_nonce, &eapol::RSN_IE, Some(&derived.kck),
            );
            self.ptk = Some(derived);
            if n == 0 || !path.send_eapol(&tx[..n], ctx) {
                ctx.log_fmt(format_args!("{}: message 2 of the handshake could not be sent - not joined", self.who));
                return Step::Failed;
            }
            self.msg2_sent += 1;
            ctx.log_fmt(format_args!(
                "{}:   message 2 of 4 sent ({} bytes, replay {}) - our nonce and the RSN element, signed", self.who,
                n, key.replay
            ));
            return Step::Continue;
        }

        // ---- Message 3: verify, unwrap the group key, answer, install both keys. ----
        if is_pairwise && is_ack && is_mic {
            let p = match self.ptk.as_ref() {
                Some(p) => p,
                None => {
                    ctx.log_fmt(format_args!("{}:   message 3 before any message 1 - ignored", self.who));
                    return Step::Continue;
                }
            };
            if !self.have_anonce || key.nonce != self.anonce {
                ctx.log_fmt(format_args!("{}:   message 3's ANonce does not match message 1's - ignored (`ieee80211_recv_4way_msg3`)", self.who));
                return Step::Continue;
            }
            let eapol_body = &eth_frame[ETHHDR..];
            if !eapol::check_mic(eapol_body, &p.kck) {
                ctx.log_fmt(format_args!("{}: message 3's MIC does not verify under our KCK - the keys disagree; not joined", self.who));
                return Step::PassphraseRefused;
            }
            if key.info & info::ENCRYPTED == 0 {
                ctx.log_fmt(format_args!("{}: message 3's key data is not encrypted - refused (a group key in the clear is not one this driver installs)", self.who));
                return Step::Failed;
            }
            let wrapped = &eth_frame[key.key_data_at..key.key_data_at + key.key_data_len];
            let mut key_data = [0u8; 512];
            if wrapped.len() < 24 || wrapped.len() > key_data.len() + 8
                || !crypto::aes_key_unwrap(&p.kek, wrapped, &mut key_data)
            {
                ctx.log_fmt(format_args!(
                    "{}: message 3's {} bytes of key data did not unwrap under our KEK - not joined", self.who,
                    wrapped.len()
                ));
                return Step::Failed;
            }
            let plain = &key_data[..wrapped.len() - 8];
            let (kid, gtk_tx, gtk) = match eapol::find_gtk(plain) {
                Some(g) => g,
                None => {
                    ctx.log_fmt(format_args!("{}: message 3 carried no group key - not joined", self.who));
                    return Step::Failed;
                }
            };
            if gtk.len() != 16 {
                ctx.log_fmt(format_args!("{}: the group key is {} bytes, not the 16 of CCMP - not joined", self.who, gtk.len()));
                return Step::Failed;
            }
            let mut gtk16 = [0u8; 16];
            gtk16.copy_from_slice(gtk);
            let n = eapol::build_key_frame(
                &mut tx, &key.from, &self.our_mac,
                info::PAIRWISE | info::KEYMIC | info::SECURE,
                key.replay, &[0u8; 32], &[], Some(&p.kck),
            );
            if n == 0 || !path.send_eapol(&tx[..n], ctx) {
                ctx.log_fmt(format_args!("{}: message 4 of the handshake could not be sent - not joined", self.who));
                return Step::Failed;
            }
            ctx.log_fmt(format_args!(
                "{}:   message 3 verified (MIC, ANonce, {} bytes of key data unwrapped); message 4 sent", self.who,
                plain.len()
            ));
            let tk = p.tk;
            if !path.install_key(0, &tk, Some(&key.from), ctx) {
                ctx.log_fmt(format_args!("{}: the firmware refused the pairwise key - not joined", self.who));
                return Step::Failed;
            }
            if !path.install_key(kid as u32, &gtk16, None, ctx) {
                ctx.log_fmt(format_args!("{}: the firmware refused the group key - not joined", self.who));
                return Step::Failed;
            }
            ctx.log_fmt(format_args!(
                "{}: JOINED - handshake complete, pairwise key installed, group key {} installed{}", self.who,
                kid,
                if gtk_tx { " (tx)" } else { "" }
            ));
            return Step::Joined(Keys { kck: p.kck, kek: p.kek, pmk: self.pmk, replay: key.replay, mac: self.our_mac });
        }

        ctx.log_fmt(format_args!(
            "{}:   an EAPOL-Key frame this handshake does not expect (info {:#06x}) - ignored", self.who,
            key.info
        ));
        Step::Continue
    }
}

/// What an EAPOL-Key frame on a live link was.
pub enum Rekey {
    /// A group-key rekey, verified, installed and acknowledged.
    Answered,
    /// A group-key frame that could not be answered; the log says why.
    Refused,
    /// Message 1 of a new four-way handshake: the caller runs a `Handshake` from it.
    Pairwise,
    /// Not a key frame this station answers.
    NotAKey,
}

/// Answer one EAPOL-Key frame that arrived after the join - `ieee80211_eapol_key_input`'s dispatch, and
/// `ieee80211_recv_rsn_group_msg1` for the case that matters:
///
/// ```c
/// } else {                         /* Group Key Handshake */
///     if (!(info & EAPOL_KEY_KEYMIC)) goto done;
///     if (info & EAPOL_KEY_KEYACK) ieee80211_recv_rsn_group_msg1(ic, key, ni);
/// }
/// ...
/// if (BE_READ_8(key->replaycnt) <= ni->ni_replaycnt) return;         /* replay */
/// if (ieee80211_eapol_key_check_mic(key, ni->ni_ptk.kck) != 0) return;
/// if (!(info & EAPOL_KEY_ENCRYPTED) || ieee80211_eapol_key_decrypt(key, ni->ni_ptk.kek) != 0) return;
/// ... find the GTK KDE, kid = gtk[6] & 3, install with IEEE80211_KEY_GROUP ...
/// (void)ieee80211_send_group_msg2(ic, ni, NULL);   /* info = KEYMIC | SECURE, replay copied, no data */
/// ```
pub fn group_rekey(
    path: &mut dyn KeyPath, eth_frame: &[u8], keys: Option<&mut Keys>, who: &str, ctx: &ServiceContext,
) -> Rekey {
    let key = match eapol::describe(eth_frame, ctx, who) {
        Some(k) => k,
        None => return Rekey::NotAKey,
    };
    if key.info & info::PAIRWISE != 0 {
        return Rekey::Pairwise;
    }
    if key.info & info::KEYMIC == 0 || key.info & info::KEYACK == 0 {
        // A group frame that is not message 1 of the group handshake: nothing to answer.
        return Rekey::NotAKey;
    }
    let keys = match keys {
        Some(k) => k,
        None => {
            ctx.log_fmt(format_args!("{}: a group-key rekey arrived and this driver holds no keys for it (an open network, or a join that left none) - not answered", who));
            return Rekey::Refused;
        }
    };
    if key.replay <= keys.replay {
        ctx.log_fmt(format_args!(
            "{}: a group-key frame with replay counter {} at or below the last accepted {} - a replay, ignored", who,
            key.replay, keys.replay
        ));
        return Rekey::Refused;
    }
    let eapol_body = &eth_frame[ETHHDR..];
    if !eapol::check_mic(eapol_body, &keys.kck) {
        ctx.log_fmt(format_args!("{}: a group-key frame whose MIC does not verify under our KCK - ignored", who));
        return Rekey::Refused;
    }
    if key.info & info::ENCRYPTED == 0 {
        ctx.log_fmt(format_args!("{}: a group-key frame with its key data in the clear - refused", who));
        return Rekey::Refused;
    }
    let wrapped = &eth_frame[key.key_data_at..key.key_data_at + key.key_data_len];
    let mut key_data = [0u8; 512];
    if wrapped.len() < 24 || wrapped.len() > key_data.len() + 8
        || !crypto::aes_key_unwrap(&keys.kek, wrapped, &mut key_data)
    {
        ctx.log_fmt(format_args!(
            "{}: a group-key frame's {} bytes of key data did not unwrap under our KEK - refused", who,
            wrapped.len()
        ));
        return Rekey::Refused;
    }
    let plain = &key_data[..wrapped.len() - 8];
    let (kid, gtk_tx, gtk) = match eapol::find_gtk(plain) {
        Some(g) => g,
        None => {
            ctx.log_fmt(format_args!("{}: a group-key frame carried no group key - refused", who));
            return Rekey::Refused;
        }
    };
    if gtk.len() != 16 {
        ctx.log_fmt(format_args!("{}: the new group key is {} bytes, not the 16 of CCMP - refused", who, gtk.len()));
        return Rekey::Refused;
    }
    let mut gtk16 = [0u8; 16];
    gtk16.copy_from_slice(gtk);
    // Install FIRST, then acknowledge: an acknowledgement for a key the firmware refused would tell the
    // access point to start using a key we do not have.
    if !path.install_key(kid as u32, &gtk16, None, ctx) {
        ctx.log_fmt(format_args!("{}: the firmware refused the new group key - the rekey is not acknowledged", who));
        return Rekey::Refused;
    }
    // The acknowledgement is an ethernet header and a 99-byte key descriptor with no key data: 113 bytes.
    let mut tx = [0u8; 128];
    let n = eapol::build_key_frame(
        &mut tx, &key.from, &keys.mac,
        info::KEYMIC | info::SECURE,
        key.replay, &[0u8; 32], &[], Some(&keys.kck),
    );
    if n == 0 || !path.send_eapol(&tx[..n], ctx) {
        ctx.log_fmt(format_args!("{}: the group-key acknowledgement could not be sent - the access point will retry", who));
        return Rekey::Refused;
    }
    keys.replay = key.replay;
    ctx.log_fmt(format_args!(
        "{}: group key {} re-installed{} and acknowledged (replay {}) - the access point rekeyed", who,
        kid,
        if gtk_tx { " (tx)" } else { "" },
        key.replay
    ));
    Rekey::Answered
}
