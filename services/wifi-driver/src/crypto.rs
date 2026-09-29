// SPDX-License-Identifier: GPL-2.0-only
//! The cryptography a WPA2-PSK station needs, and no more: SHA-1, HMAC-SHA1, and PBKDF2 over them.
//!
//! This exists because the firmware has no supplicant (`docs/wifi.md` §37), so the host must derive the keys.
//! The first of them is the pairwise master key: `PMK = PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)`
//! (IEEE 802.11-2020 §12.7.1.2). It is derived ONCE, when a passphrase arrives, and the passphrase is then
//! gone; the PMK is what the driver keeps and what every later step (the PTK, the handshake MIC) is built
//! from. Nothing here is clever, and none of it is exposed outside this crate.
//!
//! **Every primitive is checked against a published vector at boot** (`selftest`), because a wrong hash does
//! not fail - it produces a key the access point silently refuses, which would look exactly like a wrong
//! passphrase. The vectors are the standards' own: FIPS 180-1 (`abc`), RFC 2202 case 2, RFC 6070 case 1,
//! and IEEE 802.11-2020 Annex J.4.2 (`password` / `IEEE`).
//!
//! Bounded and heap-free (§26.6.1): fixed blocks, fixed digests, the caller's output slice.

use godspeed_sdk::ServiceContext;

/// SHA-1 digest size.
pub const SHA1_LEN: usize = 20;
/// SHA-1 / HMAC-SHA1 block size.
const BLOCK: usize = 64;
/// The pairwise master key, 256 bits.
pub const PMK_LEN: usize = 32;
/// `dot11RSNAConfigPSKPassPhrase` -> PSK iteration count (IEEE 802.11-2020 §12.7.1.2).
const PSK_ITERATIONS: u32 = 4096;

/// SHA-1, FIPS 180-1. One message, one digest.
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; BLOCK],
    buffered: usize,
    total: u64,
}

impl Sha1 {
    pub fn new() -> Self {
        Sha1 {
            h: [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0],
            buf: [0; BLOCK],
            buffered: 0,
            total: 0,
        }
    }

    fn compress(h: &mut [u32; 5], block: &[u8]) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buffered > 0 {
            let take = core::cmp::min(BLOCK - self.buffered, data.len());
            self.buf[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == BLOCK {
                Self::compress(&mut self.h, &self.buf);
                self.buffered = 0;
            }
        }
        while data.len() >= BLOCK {
            Self::compress(&mut self.h, &data[..BLOCK]);
            data = &data[BLOCK..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; SHA1_LEN] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        while self.buffered != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; SHA1_LEN];
        for (i, word) in self.h.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// HMAC-SHA1 (RFC 2104). A key longer than a block is hashed first, as the RFC says.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; SHA1_LEN] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = {
            let mut s = Sha1::new();
            s.update(key);
            s.finish()
        };
        k[..SHA1_LEN].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5C;
    }
    let inner = {
        let mut s = Sha1::new();
        s.update(&ipad);
        s.update(data);
        s.finish()
    };
    let mut s = Sha1::new();
    s.update(&opad);
    s.update(&inner);
    s.finish()
}

/// PBKDF2-HMAC-SHA1 (RFC 8018 §5.2) into `out`, whatever its length.
pub fn pbkdf2_sha1(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let mut block_index: u32 = 1;
    let mut at = 0;
    while at < out.len() {
        // U1 = PRF(P, S || INT(i)); Uj = PRF(P, Uj-1); T = U1 ^ ... ^ Uc.
        let mut salted = [0u8; 64];
        let n = core::cmp::min(salt.len(), salted.len() - 4);
        salted[..n].copy_from_slice(&salt[..n]);
        salted[n..n + 4].copy_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha1(password, &salted[..n + 4]);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha1(password, &u);
            for (ti, ui) in t.iter_mut().zip(u.iter()) {
                *ti ^= ui;
            }
        }
        let take = core::cmp::min(SHA1_LEN, out.len() - at);
        out[at..at + take].copy_from_slice(&t[..take]);
        at += take;
        block_index += 1;
    }
}

/// The pairwise master key from a passphrase and the network name it is for. IEEE 802.11-2020 §12.7.1.2:
/// `PSK = PBKDF2(PassPhrase, ssid, ssidLength, 4096, 256)`.
pub fn psk(passphrase: &[u8], ssid: &[u8]) -> [u8; PMK_LEN] {
    let mut pmk = [0u8; PMK_LEN];
    pbkdf2_sha1(passphrase, ssid, PSK_ITERATIONS, &mut pmk);
    pmk
}

/// Every primitive against its published vector. Logs one line per vector and returns whether all held.
/// Run at boot, before the radio is touched: a key derived by a wrong hash is refused by the access point
/// in a way that is indistinguishable from a wrong passphrase, so this is the only place the error is visible.
pub fn selftest(ctx: &ServiceContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, got: &[u8], want: &[u8]| {
        let same = got == want;
        if same {
            ctx.log_fmt(format_args!("wifi-driver:   {} - matches the published vector", name));
        } else {
            ok = false;
            ctx.log_fmt(format_args!(
                "wifi-driver:   {} - WRONG: got {:02x}{:02x}{:02x}{:02x}.., wanted {:02x}{:02x}{:02x}{:02x}..",
                name, got[0], got[1], got[2], got[3], want[0], want[1], want[2], want[3]
            ));
        }
    };

    ctx.log("wifi-driver: stage 0 - the key-derivation primitives against their published vectors");

    // FIPS 180-1 appendix A: SHA-1("abc").
    let sha = {
        let mut s = Sha1::new();
        s.update(b"abc");
        s.finish()
    };
    check(
        "SHA-1 (FIPS 180-1, `abc`)",
        &sha,
        &[
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c,
            0x9c, 0xd0, 0xd8, 0x9d,
        ],
    );

    // RFC 2202 test case 2: key "Jefe", data "what do ya want for nothing?".
    let mac = hmac_sha1(b"Jefe", b"what do ya want for nothing?");
    check(
        "HMAC-SHA1 (RFC 2202 case 2)",
        &mac,
        &[
            0xef, 0xfc, 0xdf, 0x6a, 0xe5, 0xeb, 0x2f, 0xa2, 0xd2, 0x74, 0x16, 0xd5, 0xf1, 0x84, 0xdf, 0x9c,
            0x25, 0x9a, 0x7c, 0x79,
        ],
    );

    // RFC 6070 test case 1: P "password", S "salt", c 1, dkLen 20.
    let mut dk = [0u8; 20];
    pbkdf2_sha1(b"password", b"salt", 1, &mut dk);
    check(
        "PBKDF2-SHA1 (RFC 6070 case 1)",
        &dk,
        &[
            0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf, 0x60, 0x12, 0x06,
            0x2f, 0xe0, 0x37, 0xa6,
        ],
    );

    // IEEE 802.11-2020 Annex J.4.2: passphrase "password", SSID "IEEE" -> the 256-bit PSK. This is the one
    // that matters: the exact computation a join will run.
    let pmk = psk(b"password", b"IEEE");
    check(
        "PSK (IEEE 802.11 Annex J.4.2, `password`/`IEEE`)",
        &pmk,
        &[
            0xf4, 0x2c, 0x6f, 0xc5, 0x2d, 0xf0, 0xeb, 0xef, 0x9e, 0xbb, 0x4b, 0x90, 0xb3, 0x8a, 0x5f, 0x90,
            0x2e, 0x83, 0xfe, 0x1b, 0x13, 0x5a, 0x70, 0xe2, 0x3a, 0xed, 0x76, 0x2e, 0x97, 0x10, 0xa1, 0x2e,
        ],
    );

    if ok {
        ctx.log("wifi-driver: stage 0 - every primitive matches its vector; a passphrase will derive the key the standard says");
    } else {
        ctx.log("wifi-driver: stage 0 - A PRIMITIVE IS WRONG. No passphrase will be accepted; joins are refused until this is fixed");
    }
    ok
}
