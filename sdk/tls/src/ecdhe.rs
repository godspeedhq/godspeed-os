// SPDX-License-Identifier: GPL-2.0-only
//! The X25519 key exchange (RFC 7748, as RFC 8446 4.2.8 uses it).
//!
//! The private key is 32 random bytes the caller supplies (from InspectKernel query 19), used for one
//! handshake and then forgotten. The shared secret becomes the handshake's input keying material.

use x25519_dalek::{PublicKey, StaticSecret};

use crate::TlsError;

/// An X25519 key or shared secret: 32 bytes.
pub const X25519_LEN: usize = 32;

/// The public key to send in the ClientHello's `key_share`, for this private key.
pub fn public_key(private: &[u8; X25519_LEN]) -> [u8; X25519_LEN] {
    let secret = StaticSecret::from(*private);
    PublicKey::from(&secret).to_bytes()
}

/// The shared secret, from our private key and the server's public key.
///
/// Refuses an all-zero result: that is what a low-order point gives, and RFC 8446 7.4.2 requires the
/// handshake to abort rather than derive keys an attacker can predict.
pub fn shared_secret(private: &[u8; X25519_LEN], peer_public: &[u8; X25519_LEN]) -> Result<[u8; X25519_LEN], TlsError> {
    let secret = StaticSecret::from(*private);
    let shared = secret.diffie_hellman(&PublicKey::from(*peer_public));
    if !shared.was_contributory() {
        return Err(TlsError::ZeroSharedSecret);
    }
    Ok(shared.to_bytes())
}
