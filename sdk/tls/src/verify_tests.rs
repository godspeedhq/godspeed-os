// SPDX-License-Identifier: GPL-2.0-only
//! The verifier, against the registry's real chain and a test PKI made with openssl.

use crate::certs as c;
use crate::client::{CertificateChain, Client, Event, ServerVerifier};
use crate::record::{self, content, LEGACY_VERSION};
use crate::rfc8448 as v;
use crate::{CertProblem, TlsError, WebPkiVerifier};

const REGISTRY: &str = "godspeed-registry.bankole-422.workers.dev";

/// A Certificate message's certificate_list for these DER certificates, each with no extensions.
fn list(certs: &[&[u8]], out: &mut [u8]) -> usize {
    let mut n = 0;
    for der in certs {
        let len = der.len();
        out[n..n + 3].copy_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        out[n + 3..n + 3 + len].copy_from_slice(der);
        out[n + 3 + len..n + 5 + len].copy_from_slice(&[0, 0]);
        n += 5 + len;
    }
    n
}

fn check(roots: &[&[u8]], name: &str, now: u64, certs: &[&[u8]]) -> Result<(), TlsError> {
    let mut buf = [0u8; 8192];
    let n = list(certs, &mut buf);
    let mut verifier = WebPkiVerifier::new(roots, name, now);
    verifier.certificate_chain(CertificateChain::parse(&buf[..n]).unwrap())
}

fn rejected(p: CertProblem) -> Result<(), TlsError> {
    Err(TlsError::CertificateRejected(p))
}

const X2: &[&[u8]] = &[&c::ISRG_ROOT_X2];

#[test]
fn the_registry_chain_reaches_isrg_root_x2() {
    // Exactly as the server sends it: leaf, YE2, Root YE, and X2 cross-signed by the RSA X1.
    let sent: &[&[u8]] = &[&c::REGISTRY_LEAF, &c::REGISTRY_YE2, &c::REGISTRY_ROOT_YE, &c::REGISTRY_X2_BY_X1];
    assert_eq!(check(X2, REGISTRY, c::NOW, sent), Ok(()));
    // The RSA cross-sign is not needed, and the wildcard covers the bare name too.
    let ecdsa_only: &[&[u8]] = &[&c::REGISTRY_LEAF, &c::REGISTRY_YE2, &c::REGISTRY_ROOT_YE];
    assert_eq!(check(X2, "bankole-422.workers.dev", c::NOW, ecdsa_only), Ok(()));
}

#[test]
fn the_registry_chain_is_refused_for_every_reason_it_should_be() {
    let chain: &[&[u8]] = &[&c::REGISTRY_LEAF, &c::REGISTRY_YE2, &c::REGISTRY_ROOT_YE];
    assert_eq!(check(X2, "example.com", c::NOW, chain), rejected(CertProblem::NameMismatch));
    // A wildcard covers one label only.
    assert_eq!(check(X2, "a.godspeed-registry.bankole-422.workers.dev", c::NOW, chain), rejected(CertProblem::NameMismatch));
    // 2027-02-01: the leaf has expired. 2026-10-01: it was issued on 2026-10-16.
    assert_eq!(check(X2, REGISTRY, 1801440000, chain), rejected(CertProblem::Expired));
    assert_eq!(check(X2, REGISTRY, 1790812800, chain), rejected(CertProblem::NotYetValid));
    // Another root, or a missing intermediate: the path does not arrive.
    assert_eq!(check(&[&c::TEST_ROOT], REGISTRY, c::NOW, chain), rejected(CertProblem::UnknownIssuer));
    assert_eq!(check(X2, REGISTRY, c::NOW, &[&c::REGISTRY_LEAF, &c::REGISTRY_ROOT_YE]), rejected(CertProblem::UnknownIssuer));
    // One bit of the leaf's signature.
    let mut leaf = c::REGISTRY_LEAF;
    let last = leaf.len() - 1;
    leaf[last] ^= 1;
    assert_eq!(check(X2, REGISTRY, c::NOW, &[&leaf, &c::REGISTRY_YE2, &c::REGISTRY_ROOT_YE]), rejected(CertProblem::BadChainSignature));
}

#[test]
fn a_verifier_set_up_wrongly_says_so() {
    let chain: &[&[u8]] = &[&c::REGISTRY_LEAF, &c::REGISTRY_YE2, &c::REGISTRY_ROOT_YE];
    assert_eq!(check(&[], REGISTRY, c::NOW, chain), rejected(CertProblem::NoRoots));
    assert_eq!(check(&[b"not a certificate"], REGISTRY, c::NOW, chain), rejected(CertProblem::BadRoot));
}

#[test]
fn a_ca_cannot_be_the_server() {
    assert_eq!(check(&[&c::TEST_ROOT], "registry.test", c::NOW, &[&c::TEST_ROOT]), rejected(CertProblem::WrongRole));
}

#[test]
fn the_handshake_signature_is_the_leafs() {
    let roots: &[&[u8]] = &[&c::TEST_ROOT];
    let mut buf = [0u8; 1024];
    let n = list(&[&c::TEST_LEAF], &mut buf);
    let mut verifier = WebPkiVerifier::new(roots, "registry.test", c::NOW);
    // Before any chain, no signature is accepted.
    assert_eq!(verifier.handshake_signature(0x0403, &c::TEST_MESSAGE, &c::TEST_SIGNATURE), Err(TlsError::UnexpectedMessage));
    verifier.certificate_chain(CertificateChain::parse(&buf[..n]).unwrap()).unwrap();
    assert_eq!(verifier.handshake_signature(0x0403, &c::TEST_MESSAGE, &c::TEST_SIGNATURE), Ok(()));
    let mut other = c::TEST_MESSAGE;
    other[0] ^= 1;
    assert_eq!(verifier.handshake_signature(0x0403, &other, &c::TEST_SIGNATURE), Err(TlsError::BadSignature));
    let mut sig = c::TEST_SIGNATURE;
    let last = sig.len() - 1;
    sig[last] ^= 1;
    assert_eq!(verifier.handshake_signature(0x0403, &c::TEST_MESSAGE, &sig), Err(TlsError::BadSignature));
    // The scheme names the curve: a P-256 key under the P-384 scheme fails, and RSA-PSS is not offered.
    assert_eq!(verifier.handshake_signature(0x0503, &c::TEST_MESSAGE, &c::TEST_SIGNATURE), Err(TlsError::BadSignature));
    assert_eq!(verifier.handshake_signature(0x0804, &c::TEST_MESSAGE, &c::TEST_SIGNATURE), Err(TlsError::BadSignature));
}

#[test]
fn a_refused_chain_leaves_no_key_to_sign_with() {
    let roots: &[&[u8]] = &[&c::TEST_ROOT];
    let mut buf = [0u8; 1024];
    let n = list(&[&c::TEST_LEAF], &mut buf);
    let mut verifier = WebPkiVerifier::new(roots, "wrong.test", c::NOW);
    assert!(verifier.certificate_chain(CertificateChain::parse(&buf[..n]).unwrap()).is_err());
    assert_eq!(verifier.handshake_signature(0x0403, &c::TEST_MESSAGE, &c::TEST_SIGNATURE), Err(TlsError::UnexpectedMessage));
}

#[test]
fn the_rfc8448_server_is_refused_for_its_rsa_certificate() {
    // RFC 8448's server certificate is RSA. Even trusted as a root, it cannot pass: this client is ECDSA only.
    // It is valid 2016-07-30 to 2026-07-30, so the clock is set to 2020 to reach the algorithm check.
    let rsa_cert = &v::CERTIFICATE[11..11 + 0x1b0];
    let roots: &[&[u8]] = &[rsa_cert];
    let mut buf = [0u8; 4096];
    let mut client = Client::after_hello(&v::CLIENT_HELLO, v::CLIENT_X25519_PRIVATE, WebPkiVerifier::new(roots, "server", 1577836800), &mut buf).unwrap();
    let mut rec = [0u8; 1024];
    let n = record::write_plaintext(content::HANDSHAKE, LEGACY_VERSION, &v::SERVER_HELLO, &mut rec).unwrap();
    let mut out = [0u8; 256];
    assert_eq!(client.read_record(&mut rec[..n], &mut out), Ok((Event::Continue, 0)));
    let mut flight = v::SERVER_HS_RECORD;
    let r = client.read_record(&mut flight, &mut out);
    assert!(
        r == Err(TlsError::CertificateRejected(CertProblem::UnsupportedAlgorithm)),
        "{r:?}"
    );
}
