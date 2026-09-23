// SPDX-License-Identifier: GPL-2.0-only
//! The self-describing content pattern that makes a TORN FILE detectable.
//!
//! **Why this is in the SDK and not in whichever crate happens to write it.** This is a contract
//! between a WRITER and a CHECKER, and its failure mode if the two ever disagree is the worst one
//! available: `churn verify` reports "NONE torn" because it no longer recognises a tear. A safety
//! check that passes because it stopped working is worse than no check, because somebody trusts it.
//!
//! The two halves used to be one expression written twice in `services/shell` - tolerable while both
//! copies sat a few hundred lines apart in one file, where anybody editing one saw the other.
//! Backgrounding `churn` puts the writer in `services/copier` and leaves the checker in the shell,
//! in different crates that deliberately do not share headers. So the pattern moves here, where
//! there is exactly one of it and both callers name the same function.
//!
//! **What the pattern is.** Every byte encodes the generation that wrote it:
//!
//! ```text
//! byte[k] = (gen + k) mod 251
//! ```
//!
//! A file written wholly in one generation satisfies that for every `k`. A file holding a MIX of two
//! generations breaks it at exactly the byte where the tear happened, so the checker learns `gen`
//! from byte 0 and then walks the rest.
//!
//! **251 is the largest prime under 256, and the primality is doing work.** A prime stride does not
//! align with the 508-byte block payload, so a tear landing on a block boundary still lands
//! mid-pattern and stays visible instead of reading as a continuation.
//!
//! This is the half that structural checking cannot reach: `drives check` validates the tree, the
//! bitmap and the CRCs, and none of that can tell whether a file holds the first half of one write
//! and the second half of another. The permitted-outcome table says a whole-file write must be
//! old-complete or new-complete and never a mix; this is what makes that claim testable.

/// The byte a file written in generation `gen` must hold at offset `k`.
///
/// Writer and checker MUST both come through here. Inlining the arithmetic at a call site is how
/// the two drift.
#[inline]
pub fn byte_at(gen: u8, k: usize) -> u8 {
    gen.wrapping_add((k % 251) as u8) % 251
}

/// The generation for the `i`th write of a churn run.
#[inline]
pub fn generation(i: u64) -> u8 {
    (i % 251) as u8
}

/// Fill `buf` with one generation's content, end to end.
#[inline]
pub fn fill(buf: &mut [u8], gen: u8) {
    for (k, b) in buf.iter_mut().enumerate() {
        *b = byte_at(gen, k);
    }
}

/// The first offset in `data` that does NOT match the generation recorded in its byte 0, if any.
///
/// `None` means every byte agrees: the file holds one generation end to end. `Some(k)` is the exact
/// byte where two writes meet, which is what makes a tear report actionable rather than a suspicion.
/// An empty slice has nothing to disagree with and is reported as intact - a file with no bytes is a
/// separate condition and the caller counts it separately.
#[inline]
pub fn first_divergence(data: &[u8]) -> Option<usize> {
    let gen = match data.first() {
        Some(&g) => g,
        None => return None,
    };
    data.iter().enumerate().find(|(k, &b)| b != byte_at(gen, *k)).map(|(k, _)| k)
}
