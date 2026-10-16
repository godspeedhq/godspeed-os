// SPDX-License-Identifier: GPL-2.0-only
//! The TLS 1.3 record layer (RFC 8446 5), protected with AES-128-GCM.
//!
//! A protected record on the wire is
//!
//! ```text
//!   23 03 03 <length: 2>  <ciphertext of (content || content type || zero padding)> <tag: 16>
//! ```
//!
//! The 5-byte header is the AEAD's additional data, so it cannot be changed without the tag failing, and
//! the nonce is the IV with the record's sequence number XORed into its last eight bytes (5.3). Each
//! direction has its own key, IV and counter, so one [`Protector`] is one direction of one connection.

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce, Tag};

use crate::schedule::{TrafficKeys, IV_LEN};
use crate::TlsError;

/// A record's content type (RFC 8446 5.1).
pub mod content {
    pub const CHANGE_CIPHER_SPEC: u8 = 20;
    pub const ALERT: u8 = 21;
    pub const HANDSHAKE: u8 = 22;
    pub const APPLICATION_DATA: u8 = 23;
}

/// The record header's size.
pub const HEADER_LEN: usize = 5;
/// AES-GCM's tag size.
pub const TAG_LEN: usize = 16;
/// The largest plaintext one record carries (RFC 8446 5.1).
pub const MAX_PLAINTEXT: usize = 1 << 14;
/// The largest protected record body: plaintext, content type, padding and tag (RFC 8446 5.2).
pub const MAX_CIPHERTEXT: usize = MAX_PLAINTEXT + 256;
/// The legacy version every TLS 1.3 record carries.
const LEGACY_VERSION: [u8; 2] = [0x03, 0x03];

/// How many bytes follow a protected record's 5-byte header, judged from the header alone.
///
/// A reader calls this as soon as it has the header, so a record that claims too much is refused before
/// the rest of it is read, and nothing waits for bytes that would be refused anyway.
pub fn protected_body_len(header: &[u8]) -> Result<usize, TlsError> {
    if header.len() < HEADER_LEN {
        return Err(TlsError::BadRecordHeader);
    }
    if header[0] != content::APPLICATION_DATA || header[1..3] != LEGACY_VERSION {
        return Err(TlsError::BadRecordHeader);
    }
    let body_len = u16::from_be_bytes([header[3], header[4]]) as usize;
    if body_len > MAX_CIPHERTEXT {
        return Err(TlsError::RecordTooLarge);
    }
    if body_len < 1 + TAG_LEN {
        return Err(TlsError::BadRecordHeader);
    }
    Ok(body_len)
}

/// One direction of protected records: its key, its nonce base and its sequence number.
pub struct Protector {
    aead: Aes128Gcm,
    iv: [u8; IV_LEN],
    seq: u64,
}

impl Protector {
    pub fn new(keys: &TrafficKeys) -> Self {
        Self { aead: Aes128Gcm::new(&keys.key.into()), iv: keys.iv, seq: 0 }
    }

    /// How many records this direction has protected or opened.
    pub fn sequence(&self) -> u64 {
        self.seq
    }

    fn nonce(&self) -> [u8; IV_LEN] {
        let mut n = self.iv;
        for (b, s) in n[IV_LEN - 8..].iter_mut().zip(self.seq.to_be_bytes()) {
            *b ^= s;
        }
        n
    }

    fn advance(&mut self) -> Result<(), TlsError> {
        self.seq = self.seq.checked_add(1).ok_or(TlsError::SequenceExhausted)?;
        Ok(())
    }

    /// Protect `plaintext` of `content_type` into `out` as one complete record, and return its length.
    /// No padding is added: padding hides a message's length, and this client's messages are not secret
    /// in length.
    pub fn seal(&mut self, content_type: u8, plaintext: &[u8], out: &mut [u8]) -> Result<usize, TlsError> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err(TlsError::RecordTooLarge);
        }
        let inner = plaintext.len() + 1;
        let total = HEADER_LEN + inner + TAG_LEN;
        if out.len() < total {
            return Err(TlsError::BufferTooSmall);
        }
        if self.seq == u64::MAX {
            return Err(TlsError::SequenceExhausted);
        }
        let body_len = (inner + TAG_LEN) as u16;
        out[0] = content::APPLICATION_DATA;
        out[1..3].copy_from_slice(&LEGACY_VERSION);
        out[3..5].copy_from_slice(&body_len.to_be_bytes());
        out[HEADER_LEN..HEADER_LEN + plaintext.len()].copy_from_slice(plaintext);
        out[HEADER_LEN + plaintext.len()] = content_type;
        let (header, body) = out.split_at_mut(HEADER_LEN);
        let nonce = self.nonce();
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), header, &mut body[..inner])
            .map_err(|_| TlsError::BufferTooSmall)?;
        body[inner..inner + TAG_LEN].copy_from_slice(&tag);
        self.advance()?;
        Ok(total)
    }

    /// Open one complete protected record in place. Returns its true content type and where its content
    /// lies within `record`. A record that does not verify is refused, and the sequence number does not
    /// move, because the connection is over (RFC 8446 5.2).
    pub fn open(&mut self, record: &mut [u8]) -> Result<(u8, core::ops::Range<usize>), TlsError> {
        let body_len = protected_body_len(record)?;
        if record.len() != HEADER_LEN + body_len {
            return Err(TlsError::BadRecordHeader);
        }
        let inner = body_len - TAG_LEN;
        let (header, body) = record.split_at_mut(HEADER_LEN);
        let (ciphertext, tag) = body.split_at_mut(inner);
        let nonce = self.nonce();
        self.aead
            .decrypt_in_place_detached(Nonce::from_slice(&nonce), header, ciphertext, Tag::from_slice(tag))
            .map_err(|_| TlsError::DecryptFailed)?;
        // The content type is the last non-zero byte; everything after it is padding (5.4).
        let Some(last) = ciphertext.iter().rposition(|&b| b != 0) else {
            return Err(TlsError::NoContentType);
        };
        let content_type = ciphertext[last];
        self.advance()?;
        Ok((content_type, HEADER_LEN..HEADER_LEN + last))
    }
}
