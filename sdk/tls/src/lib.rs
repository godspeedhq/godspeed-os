// SPDX-License-Identifier: GPL-2.0-only
//! A TLS 1.3 client for GodspeedOS, written here from RFC 8446.
//!
//! # Why this exists
//!
//! The native `godspeed` package manager reads the registry over HTTPS, and the operator ruled out plain
//! HTTP (`docs/developer-services-design.md` 10.3). No TLS library fits this tree as it stands: the one
//! that fits the no-heap rule is Apache-2.0 only, which GPL-2.0-only cannot include, and the others need
//! a heap or are C. So the protocol is written here, the way Linux's own TLS is a userspace library: the
//! kernel learns nothing about it.
//!
//! # What is ours and what is not
//!
//! - **Ours:** the TLS 1.3 key schedule ([`schedule`]), the record layer ([`record`]), and (in later
//!   slices) the handshake messages and the client state machine.
//! - **Not ours:** the primitives. SHA-256, HMAC, HKDF and AES-128-GCM are RustCrypto's; X25519 is dalek's.
//!   Constant-time field arithmetic is exactly the code that is easy to get subtly wrong, so it is taken
//!   from libraries built and reviewed for it.
//!
//! # Scope, deliberately narrow
//!
//! A client, never a server. TLS 1.3 only. One cipher suite, TLS_AES_128_GCM_SHA256, which RFC 8446 makes
//! mandatory for every server, and one key exchange, X25519. Certificates are checked by ECDSA only (the
//! registry's chain is ECDSA end to end, `docs/developer-services-design.md` 10.3). Nothing is taken from
//! a configuration that lives in this crate: the server name, the root certificates and the current time
//! are the caller's (10.8).
//!
//! # Bounded and heap-free
//!
//! Every buffer is a slice the caller owns (CLAUDE.md 26.6.1). A TLS record is at most 16 KiB of
//! plaintext, so a caller needs about 16 to 20 KiB per connection.
//!
//! # Tested against the RFC's own traces
//!
//! RFC 8448 section 3 is a complete TLS 1.3 handshake with this key exchange and this cipher suite, every
//! secret and every encrypted record printed byte for byte. [`rfc8448`] holds those values, generated from
//! the RFC text by a script, and the tests reproduce every one of them.

#![no_std]
#![deny(unsafe_code)]

pub mod ecdhe;
pub mod record;
pub mod schedule;

#[cfg(test)]
mod rfc8448;
#[cfg(test)]
mod tests;

/// Every way this crate refuses. Each is final for the connection it happened on: TLS has no "try again"
/// at this layer, and a caller that carries on after one is carrying on with a broken connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsError {
    /// A record header that is not TLS 1.3's: the wrong content type for a protected record, or the wrong
    /// legacy version.
    BadRecordHeader,
    /// A record longer than RFC 8446 5.2 allows.
    RecordTooLarge,
    /// The record's authentication tag did not verify: it was altered, or it was not sent with these keys
    /// in this order. RFC 8446 5.2 makes this a `bad_record_mac` alert, never something to skip past.
    DecryptFailed,
    /// A decrypted record that is all padding, with no content type (RFC 8446 5.4).
    NoContentType,
    /// The caller's buffer cannot hold the result.
    BufferTooSmall,
    /// 2^64 - 1 records were sent or received on one key. RFC 8446 5.3 forbids wrapping the counter; the
    /// connection must end (this client does not rekey).
    SequenceExhausted,
    /// The X25519 shared secret came out all zero: the peer sent a low-order point. RFC 8446 7.4.2 says to
    /// abort.
    ZeroSharedSecret,
}
