// SPDX-License-Identifier: GPL-2.0-only
//! The TLS 1.3 key schedule (RFC 8446 7.1), for SHA-256 and no pre-shared key.
//!
//! ```text
//!  0 (32 zero bytes) -> HKDF-Extract = early secret
//!                         |
//!                    Derive-Secret(., "derived", "")
//!                         |
//!  X25519 shared ----> HKDF-Extract = handshake secret --> "c hs traffic", "s hs traffic"
//!                         |
//!                    Derive-Secret(., "derived", "")
//!                         |
//!  0 (32 zero bytes) -> HKDF-Extract = master secret --> "c ap traffic", "s ap traffic"
//! ```
//!
//! Each traffic secret gives a record key and IV (7.3) and, for the handshake, a finished key (4.4.4).
//! Every function is a pure computation over fixed-size arrays; nothing here allocates or keeps state.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// SHA-256's output, and so the size of every secret here.
pub const HASH_LEN: usize = 32;
/// AES-128-GCM's key size.
pub const KEY_LEN: usize = 16;
/// The per-record nonce base (RFC 8446 5.3).
pub const IV_LEN: usize = 12;

/// A traffic secret's record protection: the AEAD key and the nonce base.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrafficKeys {
    pub key: [u8; KEY_LEN],
    pub iv: [u8; IV_LEN],
}

/// The running transcript hash (RFC 8446 4.4.1): every handshake message, in order, as sent.
#[derive(Clone)]
pub struct Transcript {
    hash: Sha256,
}

impl Transcript {
    pub fn new() -> Self {
        Self { hash: Sha256::new() }
    }

    /// Add one handshake message, header included.
    pub fn update(&mut self, message: &[u8]) {
        self.hash.update(message);
    }

    /// The hash of everything so far. The transcript carries on: later messages are still added.
    pub fn current(&self) -> [u8; HASH_LEN] {
        self.hash.clone().finalize().into()
    }
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

/// HKDF-Expand-Label (RFC 8446 7.1). `label` is given without its "tls13 " prefix, as the RFC writes it.
///
/// Labels and contexts here are fixed and short, so the 255-byte limits of the encoding are asserted rather
/// than returned as errors: a caller that broke them would be a bug in this crate, not in the peer.
pub fn expand_label(secret: &[u8; HASH_LEN], label: &[u8], context: &[u8], out: &mut [u8]) {
    const PREFIX: &[u8] = b"tls13 ";
    assert!(PREFIX.len() + label.len() <= 255 && context.len() <= 255 && out.len() <= 255 * HASH_LEN);
    // struct { uint16 length; opaque label<7..255>; opaque context<0..255>; } HkdfLabel
    let mut info = [0u8; 2 + 1 + 255 + 1 + 255];
    let mut n = 0;
    info[n..n + 2].copy_from_slice(&(out.len() as u16).to_be_bytes());
    n += 2;
    info[n] = (PREFIX.len() + label.len()) as u8;
    n += 1;
    info[n..n + PREFIX.len()].copy_from_slice(PREFIX);
    n += PREFIX.len();
    info[n..n + label.len()].copy_from_slice(label);
    n += label.len();
    info[n] = context.len() as u8;
    n += 1;
    info[n..n + context.len()].copy_from_slice(context);
    n += context.len();
    let hk = Hkdf::<Sha256>::from_prk(secret).expect("a 32-byte secret is a valid SHA-256 PRK");
    hk.expand(&info[..n], out).expect("the length is checked above");
}

/// Derive-Secret (RFC 8446 7.1): a secret from a secret, a label and a transcript hash.
pub fn derive_secret(secret: &[u8; HASH_LEN], label: &[u8], transcript_hash: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let mut out = [0u8; HASH_LEN];
    expand_label(secret, label, transcript_hash, &mut out);
    out
}

/// The hash of nothing, the context of the two "derived" steps.
fn empty_hash() -> [u8; HASH_LEN] {
    Sha256::digest([]).into()
}

fn extract(salt: &[u8; HASH_LEN], ikm: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let (prk, _) = Hkdf::<Sha256>::extract(Some(salt), ikm);
    prk.into()
}

/// The early secret with no pre-shared key: HKDF-Extract over zeros.
pub fn early_secret() -> [u8; HASH_LEN] {
    extract(&[0u8; HASH_LEN], &[0u8; HASH_LEN])
}

/// The handshake secret, from the early secret and the X25519 shared secret.
pub fn handshake_secret(early: &[u8; HASH_LEN], ecdhe_shared: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let salt = derive_secret(early, b"derived", &empty_hash());
    extract(&salt, ecdhe_shared)
}

/// The master secret, from the handshake secret.
pub fn master_secret(handshake: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let salt = derive_secret(handshake, b"derived", &empty_hash());
    extract(&salt, &[0u8; HASH_LEN])
}

/// The record key and IV for a traffic secret (RFC 8446 7.3).
pub fn traffic_keys(traffic_secret: &[u8; HASH_LEN]) -> TrafficKeys {
    let mut keys = TrafficKeys { key: [0; KEY_LEN], iv: [0; IV_LEN] };
    expand_label(traffic_secret, b"key", &[], &mut keys.key);
    expand_label(traffic_secret, b"iv", &[], &mut keys.iv);
    keys
}

/// The Finished message's verify_data (RFC 8446 4.4.4): an HMAC of the transcript under the finished key.
pub fn finished_verify_data(traffic_secret: &[u8; HASH_LEN], transcript_hash: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let mut finished_key = [0u8; HASH_LEN];
    expand_label(traffic_secret, b"finished", &[], &mut finished_key);
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&finished_key).expect("HMAC takes a key of any length");
    mac.update(transcript_hash);
    mac.finalize().into_bytes().into()
}

/// Whether a received verify_data is the one expected, compared in constant time so the comparison does not
/// tell an attacker how many leading bytes were right.
pub fn verify_data_matches(expected: &[u8; HASH_LEN], received: &[u8]) -> bool {
    if received.len() != HASH_LEN {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..HASH_LEN {
        diff |= expected[i] ^ received[i];
    }
    diff == 0
}
