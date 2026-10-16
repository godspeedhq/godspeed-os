// SPDX-License-Identifier: GPL-2.0-only
//! The server's identity: its certificate chain checked to a pinned root, and its CertificateVerify
//! signature checked with the leaf certificate's key.
//!
//! # What is checked, and by whom
//!
//! [rustls-webpki](https://github.com/rustls/webpki) builds and checks the path: every signature in the
//! chain, every validity period against the caller's clock, basic constraints, key usage for a TLS
//! server, name constraints, and the server name against the leaf's subjectAltName. It is built without
//! `alloc` and without its own cryptography; the ECDSA it asks for is [`ECDSA_ALGORITHMS`] below,
//! RustCrypto's `p256` and `p384`.
//!
//! The roots, the server name and the time are the caller's (`docs/developer-services-design.md` 10.8).
//! Nothing here holds a default root: a verifier with no roots trusts nothing.
//!
//! # ECDSA only
//!
//! P-256 and P-384, each with SHA-256 or SHA-384. That is every signature in the registry's chain
//! (`docs/developer-services-design.md` 10.3); an RSA certificate anywhere on the path is refused,
//! including the RSA cross-signed root a server may also send, which is simply not used.
//!
//! # No revocation
//!
//! Neither CRLs nor OCSP are checked: both need a second network fetch and a heap to hold the list, and
//! the certificates this client meets are short-lived (the registry's leaf is valid for 90 days).
//! Recorded rather than implied (CLAUDE.md 26.7).

use core::time::Duration;

use rustls_pki_types::{
    alg_id, AlgorithmIdentifier, CertificateDer, Der, InvalidSignature, ServerName, SignatureVerificationAlgorithm,
    TrustAnchor, UnixTime,
};
use webpki::{EndEntityCert, Error as PkiError, KeyUsage};

use crate::client::{CertificateChain, ServerVerifier, ECDSA_SECP256R1_SHA256, ECDSA_SECP384R1_SHA384};
use crate::{CertProblem, TlsError};

/// The most trusted roots one verifier takes.
pub const MAX_ROOTS: usize = 8;
/// The most certificates a server's chain may hold, leaf included. Public chains are 2 to 4.
pub const MAX_CHAIN: usize = 8;
/// The largest leaf certificate kept between the Certificate and CertificateVerify messages. The
/// registry's is 955 bytes.
pub const MAX_LEAF: usize = 4096;

#[derive(Clone, Copy, Debug)]
enum Curve {
    P256,
    P384,
}

#[derive(Clone, Copy, Debug)]
enum Hash {
    Sha256,
    Sha384,
}

/// One ECDSA signature algorithm, as webpki asks for it.
#[derive(Debug)]
struct Ecdsa {
    curve: Curve,
    hash: Hash,
}

impl SignatureVerificationAlgorithm for Ecdsa {
    fn verify_signature(&self, public_key: &[u8], message: &[u8], signature: &[u8]) -> Result<(), InvalidSignature> {
        use sha2::Digest;
        use ecdsa::signature::hazmat::PrehashVerifier;
        let mut digest = [0u8; 48];
        let digest = match self.hash {
            Hash::Sha256 => {
                digest[..32].copy_from_slice(&sha2::Sha256::digest(message));
                &digest[..32]
            }
            Hash::Sha384 => {
                digest.copy_from_slice(&sha2::Sha384::digest(message));
                &digest[..]
            }
        };
        // A hash longer than the curve's order is truncated to it, as ECDSA defines (FIPS 186-5 6.4.1).
        match self.curve {
            Curve::P256 => {
                let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key).map_err(|_| InvalidSignature)?;
                let sig = p256::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                key.verify_prehash(digest, &sig).map_err(|_| InvalidSignature)
            }
            Curve::P384 => {
                let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(public_key).map_err(|_| InvalidSignature)?;
                let sig = p384::ecdsa::Signature::from_der(signature).map_err(|_| InvalidSignature)?;
                key.verify_prehash(digest, &sig).map_err(|_| InvalidSignature)
            }
        }
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        match self.curve {
            Curve::P256 => alg_id::ECDSA_P256,
            Curve::P384 => alg_id::ECDSA_P384,
        }
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        match self.hash {
            Hash::Sha256 => alg_id::ECDSA_SHA256,
            Hash::Sha384 => alg_id::ECDSA_SHA384,
        }
    }
}

static P256_SHA256: Ecdsa = Ecdsa { curve: Curve::P256, hash: Hash::Sha256 };
static P256_SHA384: Ecdsa = Ecdsa { curve: Curve::P256, hash: Hash::Sha384 };
static P384_SHA256: Ecdsa = Ecdsa { curve: Curve::P384, hash: Hash::Sha256 };
static P384_SHA384: Ecdsa = Ecdsa { curve: Curve::P384, hash: Hash::Sha384 };

/// Every signature a certificate on the path may carry.
pub static ECDSA_ALGORITHMS: &[&dyn SignatureVerificationAlgorithm] = &[&P256_SHA256, &P256_SHA384, &P384_SHA256, &P384_SHA384];

const EMPTY_ANCHOR: TrustAnchor<'static> =
    TrustAnchor { subject: Der::from_slice(&[]), subject_public_key_info: Der::from_slice(&[]), name_constraints: None };

/// A [`ServerVerifier`] that accepts a server only when its chain reaches one of `roots` at time `now`,
/// names `server_name`, and its CertificateVerify signature is the leaf's.
pub struct WebPkiVerifier<'r> {
    roots: &'r [&'r [u8]],
    server_name: &'r str,
    now: UnixTime,
    leaf: [u8; MAX_LEAF],
    leaf_len: usize,
    chain_ok: bool,
}

impl<'r> WebPkiVerifier<'r> {
    /// `roots` are DER certificates, `now_secs` the current time in seconds since 1970. A clock that is
    /// wrong makes valid certificates look expired or not yet valid, and that is reported as such: it is
    /// never a reason to skip the check (`docs/developer-services-design.md` 10.4).
    pub fn new(roots: &'r [&'r [u8]], server_name: &'r str, now_secs: u64) -> Self {
        Self {
            roots,
            server_name,
            now: UnixTime::since_unix_epoch(Duration::from_secs(now_secs)),
            leaf: [0; MAX_LEAF],
            leaf_len: 0,
            chain_ok: false,
        }
    }

    fn check_chain(&mut self, chain: CertificateChain<'_>) -> Result<(), CertProblem> {
        if self.roots.is_empty() || self.roots.len() > MAX_ROOTS {
            return Err(CertProblem::NoRoots);
        }
        let mut root_ders = [const { CertificateDer::from_slice(&[]) }; MAX_ROOTS];
        for (d, r) in root_ders.iter_mut().zip(self.roots) {
            *d = CertificateDer::from_slice(r);
        }
        let mut anchors = [const { EMPTY_ANCHOR }; MAX_ROOTS];
        for (a, der) in anchors.iter_mut().zip(&root_ders[..self.roots.len()]) {
            // A root that does not parse is the caller's mistake, and says so rather than being skipped.
            *a = webpki::anchor_from_trusted_cert(der).map_err(|_| CertProblem::BadRoot)?;
        }
        let n_anchors = self.roots.len();

        let mut certs = chain.certificates();
        let leaf = certs.next().ok_or(CertProblem::Malformed)?;
        let mut intermediates = [const { CertificateDer::from_slice(&[]) }; MAX_CHAIN];
        let mut n = 0;
        for c in certs {
            if n == MAX_CHAIN - 1 {
                return Err(CertProblem::ChainTooLong);
            }
            intermediates[n] = CertificateDer::from_slice(c);
            n += 1;
        }
        if leaf.len() > MAX_LEAF {
            return Err(CertProblem::Malformed);
        }
        let leaf_der = CertificateDer::from_slice(leaf);
        let ee = EndEntityCert::try_from(&leaf_der).map_err(problem)?;
        ee.verify_for_usage(
            ECDSA_ALGORITHMS,
            &anchors[..n_anchors],
            &intermediates[..n],
            self.now,
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(problem)?;
        let name = ServerName::try_from(self.server_name).map_err(|_| CertProblem::NameMismatch)?;
        ee.verify_is_valid_for_subject_name(&name).map_err(problem)?;
        self.leaf[..leaf.len()].copy_from_slice(leaf);
        self.leaf_len = leaf.len();
        Ok(())
    }
}

/// webpki's reason, as one this crate reports. The ones a person can act on keep their meaning: a wrong
/// clock, a server name that does not match, a root that is not trusted.
fn problem(e: PkiError) -> CertProblem {
    match e {
        PkiError::CertExpired { .. } => CertProblem::Expired,
        PkiError::CertNotValidYet { .. } => CertProblem::NotYetValid,
        PkiError::UnknownIssuer => CertProblem::UnknownIssuer,
        PkiError::CertNotValidForName(_) => CertProblem::NameMismatch,
        PkiError::InvalidSignatureForPublicKey => CertProblem::BadChainSignature,
        PkiError::UnsupportedSignatureAlgorithmContext(_)
        | PkiError::UnsupportedSignatureAlgorithmForPublicKeyContext(_) => CertProblem::UnsupportedAlgorithm,
        PkiError::CaUsedAsEndEntity | PkiError::EndEntityUsedAsCa => CertProblem::WrongRole,
        PkiError::RequiredEkuNotFoundContext(_) => CertProblem::WrongRole,
        _ => CertProblem::Malformed,
    }
}

impl ServerVerifier for WebPkiVerifier<'_> {
    fn certificate_chain(&mut self, chain: CertificateChain<'_>) -> Result<(), TlsError> {
        self.chain_ok = false;
        self.check_chain(chain).map_err(TlsError::CertificateRejected)?;
        self.chain_ok = true;
        Ok(())
    }

    fn handshake_signature(&mut self, scheme: u16, message: &[u8], signature: &[u8]) -> Result<(), TlsError> {
        if !self.chain_ok {
            return Err(TlsError::UnexpectedMessage);
        }
        // In TLS 1.3 the scheme names the curve too (RFC 8446 4.2.3), so a P-384 key under 0x0403 fails
        // webpki's key-type check below rather than being verified as something it is not.
        let alg: &dyn SignatureVerificationAlgorithm = match scheme {
            ECDSA_SECP256R1_SHA256 => &P256_SHA256,
            ECDSA_SECP384R1_SHA384 => &P384_SHA384,
            _ => return Err(TlsError::BadSignature),
        };
        let der = CertificateDer::from_slice(&self.leaf[..self.leaf_len]);
        let ee = EndEntityCert::try_from(&der).map_err(|_| TlsError::BadSignature)?;
        ee.verify_signature(alg, message, signature).map_err(|_| TlsError::BadSignature)
    }
}
