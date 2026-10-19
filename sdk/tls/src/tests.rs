// SPDX-License-Identifier: GPL-2.0-only
//! Every secret, key and record of RFC 8448 section 3, reproduced. Host tests: `cargo test -p godspeed-tls`.

use crate::ecdhe;
use crate::record::{content, Protector, HEADER_LEN, MAX_PLAINTEXT, TAG_LEN};
use crate::rfc8448 as v;
use crate::schedule::*;
use crate::TlsError;

/// The transcript after the given messages, in order.
fn transcript(messages: &[&[u8]]) -> [u8; HASH_LEN] {
    let mut t = Transcript::new();
    for m in messages {
        t.update(m);
    }
    t.current()
}

/// The server's handshake flight, split back into its four messages: EncryptedExtensions, Certificate,
/// CertificateVerify, Finished. The RFC prints them separately and as one record payload; check they agree.
fn server_flight() -> [&'static [u8]; 4] {
    [&v::ENCRYPTED_EXTENSIONS, &v::CERTIFICATE, &v::CERTIFICATE_VERIFY, &v::SERVER_FINISHED]
}

#[test]
fn x25519_public_keys_and_shared_secret() {
    assert_eq!(ecdhe::public_key(&v::CLIENT_X25519_PRIVATE), v::CLIENT_X25519_PUBLIC);
    assert_eq!(ecdhe::public_key(&v::SERVER_X25519_PRIVATE), v::SERVER_X25519_PUBLIC);
    assert_eq!(ecdhe::shared_secret(&v::CLIENT_X25519_PRIVATE, &v::SERVER_X25519_PUBLIC).unwrap(), v::ECDHE_SHARED);
    assert_eq!(ecdhe::shared_secret(&v::SERVER_X25519_PRIVATE, &v::CLIENT_X25519_PUBLIC).unwrap(), v::ECDHE_SHARED);
}

#[test]
fn x25519_refuses_a_low_order_point() {
    // The identity point, all zeros, makes every shared secret zero (RFC 7748 6.1; RFC 8446 7.4.2).
    assert_eq!(ecdhe::shared_secret(&v::CLIENT_X25519_PRIVATE, &[0u8; 32]), Err(TlsError::ZeroSharedSecret));
}

#[test]
fn early_handshake_and_master_secrets() {
    let early = early_secret();
    assert_eq!(early, v::EARLY_SECRET);
    // The salt of the handshake extract is Derive-Secret(early, "derived", H("")).
    let empty: [u8; HASH_LEN] = transcript(&[]);
    assert_eq!(derive_secret(&early, b"derived", &empty), v::HANDSHAKE_DERIVED);
    let hs = handshake_secret(&early, &v::ECDHE_SHARED);
    assert_eq!(hs, v::HANDSHAKE_SECRET);
    assert_eq!(derive_secret(&hs, b"derived", &empty), v::MASTER_DERIVED);
    assert_eq!(master_secret(&hs), v::MASTER_SECRET);
}

#[test]
fn transcript_hash_after_the_hellos() {
    assert_eq!(transcript(&[&v::CLIENT_HELLO, &v::SERVER_HELLO]), v::TRANSCRIPT_CH_SH);
}

#[test]
fn handshake_traffic_secrets_and_keys() {
    let hs = handshake_secret(&early_secret(), &v::ECDHE_SHARED);
    let th = transcript(&[&v::CLIENT_HELLO, &v::SERVER_HELLO]);
    let c_hs = derive_secret(&hs, b"c hs traffic", &th);
    let s_hs = derive_secret(&hs, b"s hs traffic", &th);
    assert_eq!(c_hs, v::CLIENT_HS_TRAFFIC);
    assert_eq!(s_hs, v::SERVER_HS_TRAFFIC);
    assert_eq!(traffic_keys(&s_hs), TrafficKeys { key: v::SERVER_HS_KEY, iv: v::SERVER_HS_IV });
    assert_eq!(traffic_keys(&c_hs), TrafficKeys { key: v::CLIENT_HS_KEY, iv: v::CLIENT_HS_IV });
}

#[test]
fn the_server_flight_is_the_four_messages() {
    let mut joined = [0u8; 657];
    let mut n = 0;
    for m in server_flight() {
        joined[n..n + m.len()].copy_from_slice(m);
        n += m.len();
    }
    assert_eq!(n, v::SERVER_HS_RECORD_PAYLOAD.len());
    assert_eq!(&joined[..n], &v::SERVER_HS_RECORD_PAYLOAD[..]);
}

#[test]
fn server_finished_verify_data() {
    let [ee, cert, cv, _] = server_flight();
    let th = transcript(&[&v::CLIENT_HELLO, &v::SERVER_HELLO, ee, cert, cv]);
    let expected = finished_verify_data(&v::SERVER_HS_TRAFFIC, &th);
    assert_eq!(expected, v::SERVER_FINISHED_VERIFY);
    // The Finished message is the 4-byte handshake header (type 20, length 32) and the verify_data.
    assert_eq!(&v::SERVER_FINISHED[..4], &[20, 0, 0, 32]);
    assert!(verify_data_matches(&expected, &v::SERVER_FINISHED[4..]));
    let mut wrong = v::SERVER_FINISHED;
    wrong[35] ^= 1;
    assert!(!verify_data_matches(&expected, &wrong[4..]));
    assert!(!verify_data_matches(&expected, &v::SERVER_FINISHED[4..35]));
}

#[test]
fn client_finished_verify_data() {
    let [ee, cert, cv, sfin] = server_flight();
    let th = transcript(&[&v::CLIENT_HELLO, &v::SERVER_HELLO, ee, cert, cv, sfin]);
    assert_eq!(finished_verify_data(&v::CLIENT_HS_TRAFFIC, &th), v::CLIENT_FINISHED_VERIFY);
    assert_eq!(&v::CLIENT_FINISHED[4..], &v::CLIENT_FINISHED_VERIFY[..]);
}

#[test]
fn application_traffic_secrets_and_keys() {
    let ms = master_secret(&handshake_secret(&early_secret(), &v::ECDHE_SHARED));
    let [ee, cert, cv, sfin] = server_flight();
    // The application secrets hash the transcript through the SERVER's Finished (RFC 8446 7.1).
    let th = transcript(&[&v::CLIENT_HELLO, &v::SERVER_HELLO, ee, cert, cv, sfin]);
    let c_ap = derive_secret(&ms, b"c ap traffic", &th);
    let s_ap = derive_secret(&ms, b"s ap traffic", &th);
    assert_eq!(c_ap, v::CLIENT_AP_TRAFFIC);
    assert_eq!(s_ap, v::SERVER_AP_TRAFFIC);
    assert_eq!(traffic_keys(&c_ap), TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV });
    assert_eq!(traffic_keys(&s_ap), TrafficKeys { key: v::SERVER_AP_KEY, iv: v::SERVER_AP_IV });
}

/// Seal `payload` as record number `seq` of a direction and check it is byte for byte the RFC's record; then
/// open the RFC's record and check the payload and content type come back.
fn check_record(keys: TrafficKeys, seq: u64, ctype: u8, payload: &[u8], record: &[u8]) {
    let mut tx = Protector::new(&keys);
    let mut rx = Protector::new(&keys);
    let mut out = [0u8; 1024];
    let mut buf = [0u8; 1024];
    // Advance both directions to `seq` with throwaway records, as the real connection would have.
    for _ in 0..seq {
        let n = tx.seal(content::HANDSHAKE, b"x", &mut out).unwrap();
        buf[..n].copy_from_slice(&out[..n]);
        rx.open(&mut buf[..n]).unwrap();
    }
    let n = tx.seal(ctype, payload, &mut out).unwrap();
    assert_eq!(&out[..n], record, "sealed record differs from the RFC's at sequence {seq}");
    buf[..record.len()].copy_from_slice(record);
    let (t, range) = rx.open(&mut buf[..record.len()]).unwrap();
    assert_eq!(t, ctype);
    assert_eq!(&buf[range], payload);
    assert_eq!(rx.sequence(), seq + 1);
}

#[test]
fn server_handshake_record() {
    let keys = TrafficKeys { key: v::SERVER_HS_KEY, iv: v::SERVER_HS_IV };
    check_record(keys, 0, content::HANDSHAKE, &v::SERVER_HS_RECORD_PAYLOAD, &v::SERVER_HS_RECORD);
}

#[test]
fn client_finished_record() {
    let keys = TrafficKeys { key: v::CLIENT_HS_KEY, iv: v::CLIENT_HS_IV };
    check_record(keys, 0, content::HANDSHAKE, &v::CLIENT_FINISHED, &v::CLIENT_FINISHED_RECORD);
}

#[test]
fn application_records_in_order() {
    let client = TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV };
    let server = TrafficKeys { key: v::SERVER_AP_KEY, iv: v::SERVER_AP_IV };
    // The server's first application-keyed record is its NewSessionTicket (sequence 0), then its data (1),
    // then its alert (2). The client sends data (0), then its alert (1).
    check_record(server, 0, content::HANDSHAKE, &v::SERVER_TICKET_PAYLOAD, &v::SERVER_TICKET_RECORD);
    check_record(server, 1, content::APPLICATION_DATA, &v::SERVER_APP_PAYLOAD, &v::SERVER_APP_RECORD);
    check_record(server, 2, content::ALERT, &v::SERVER_ALERT_PAYLOAD, &v::SERVER_ALERT_RECORD);
    check_record(client, 0, content::APPLICATION_DATA, &v::CLIENT_APP_PAYLOAD, &v::CLIENT_APP_RECORD);
    check_record(client, 1, content::ALERT, &v::CLIENT_ALERT_PAYLOAD, &v::CLIENT_ALERT_RECORD);
}

#[test]
fn a_tampered_record_is_refused_and_the_sequence_holds() {
    let keys = TrafficKeys { key: v::SERVER_HS_KEY, iv: v::SERVER_HS_IV };
    let mut buf = [0u8; 1024];
    let len = v::SERVER_HS_RECORD.len();
    // Flip one bit of the ciphertext, then of the tag, then of the header (which is the additional data).
    for at in [HEADER_LEN + 10, len - 1, 4] {
        let mut rx = Protector::new(&keys);
        buf[..len].copy_from_slice(&v::SERVER_HS_RECORD);
        buf[at] ^= 0x01;
        let r = rx.open(&mut buf[..len]);
        assert!(r == Err(TlsError::DecryptFailed) || r == Err(TlsError::BadRecordHeader), "byte {at}: {r:?}");
        assert_eq!(rx.sequence(), 0);
    }
}

#[test]
fn a_record_at_the_wrong_sequence_number_is_refused() {
    let keys = TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV };
    let mut rx = Protector::new(&keys);
    let mut buf = [0u8; 128];
    // The client's alert was sealed as record 1; read as record 0 the nonce is wrong.
    buf[..v::CLIENT_ALERT_RECORD.len()].copy_from_slice(&v::CLIENT_ALERT_RECORD);
    assert_eq!(rx.open(&mut buf[..v::CLIENT_ALERT_RECORD.len()]), Err(TlsError::DecryptFailed));
}

#[test]
fn malformed_headers_are_refused_before_any_decryption() {
    let keys = TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV };
    let mut rx = Protector::new(&keys);
    let good = v::CLIENT_APP_RECORD;
    let mut buf = good;
    buf[0] = content::HANDSHAKE; // a protected record always says application_data on the wire
    assert_eq!(rx.open(&mut buf), Err(TlsError::BadRecordHeader));
    let mut buf = good;
    buf[2] = 0x01; // legacy version must be 03 03
    assert_eq!(rx.open(&mut buf), Err(TlsError::BadRecordHeader));
    let mut buf = good;
    buf[4] = buf[4].wrapping_add(1); // the stated length must match the record given
    assert_eq!(rx.open(&mut buf), Err(TlsError::BadRecordHeader));
    let mut short = [0u8; HEADER_LEN + TAG_LEN];
    short[..5].copy_from_slice(&[23, 3, 3, 0, TAG_LEN as u8]);
    assert_eq!(rx.open(&mut short), Err(TlsError::BadRecordHeader));
    let mut huge = [0u8; HEADER_LEN + 1];
    huge[..5].copy_from_slice(&[23, 3, 3, 0x48, 0x01]); // 2^14 + 256 + 1
    assert_eq!(rx.open(&mut huge), Err(TlsError::RecordTooLarge));
}

#[test]
fn an_all_padding_record_has_no_content_type() {
    let keys = TrafficKeys { key: v::SERVER_AP_KEY, iv: v::SERVER_AP_IV };
    let mut tx = Protector::new(&keys);
    let mut rx = Protector::new(&keys);
    let mut out = [0u8; 64];
    // Seal "content type 0" with no content: the inner plaintext is a single zero byte, all padding.
    let n = tx.seal(0, &[], &mut out).unwrap();
    assert_eq!(rx.open(&mut out[..n]), Err(TlsError::NoContentType));
}

#[test]
fn sealing_respects_the_buffer_and_the_size_limit() {
    let keys = TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV };
    let mut tx = Protector::new(&keys);
    let mut small = [0u8; HEADER_LEN + 1 + TAG_LEN + 2];
    assert_eq!(tx.seal(content::APPLICATION_DATA, b"abc", &mut small), Err(TlsError::BufferTooSmall));
    assert_eq!(tx.sequence(), 0, "a refused seal must not use up a sequence number");
    static BIG: [u8; MAX_PLAINTEXT + 1] = [0; MAX_PLAINTEXT + 1];
    let mut out = [0u8; 8];
    assert_eq!(tx.seal(content::APPLICATION_DATA, &BIG, &mut out), Err(TlsError::RecordTooLarge));
}
