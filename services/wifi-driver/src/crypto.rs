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

/// The 802.11 PRF (IEEE 802.11-2020 §12.7.1.2, `ieee80211_prf`): `HMAC-SHA1(key, label || context || i)`
/// for i = 0, 1, ... until `out` is full. `label` is passed WITH its terminating NUL, as OpenBSD passes it
/// (`"Pairwise key expansion", 23 /* PRF uses \0 */`) - the NUL is part of the input, not a C artefact.
pub fn prf_sha1(key: &[u8], label: &[u8], context: &[u8], out: &mut [u8]) {
    let mut msg = [0u8; 160];
    let n = label.len() + context.len() + 1;
    if n > msg.len() {
        // The PTK context is 76 bytes and the label 23; anything larger is a caller error, and a PRF that
        // silently truncated its input would derive a key that fails everywhere downstream.
        out.fill(0);
        return;
    }
    msg[..label.len()].copy_from_slice(label);
    msg[label.len()..label.len() + context.len()].copy_from_slice(context);
    let mut at = 0;
    let mut count = 0u8;
    while at < out.len() {
        msg[n - 1] = count;
        let d = hmac_sha1(key, &msg[..n]);
        let take = core::cmp::min(SHA1_LEN, out.len() - at);
        out[at..at + take].copy_from_slice(&d[..take]);
        at += take;
        count = count.wrapping_add(1);
    }
}

/// AES-128, FIPS 197. Both directions: the key-data unwrap in message 3 of the handshake is the INVERSE
/// cipher (RFC 3394 §2.2.2), which is why a stack that only ever encrypts cannot finish a WPA2 join.
///
/// The S-box is computed, not typed: 256 bytes copied by hand are 256 chances to be wrong in a way that the
/// self-test would catch but nobody could read. Each byte is the multiplicative inverse in GF(2^8) followed
/// by the affine transform, exactly as §5.1.1 defines it.
pub struct Aes128 {
    round_keys: [[u8; 16]; 11],
    sbox: [u8; 256],
    inv_sbox: [u8; 256],
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        // S-box: inverse in GF(2^8) (0 maps to 0), then the affine transform b ^ rotl(b,1..4) ^ 0x63.
        let mut sbox = [0u8; 256];
        let mut inv_sbox = [0u8; 256];
        for x in 0..256usize {
            let inv = if x == 0 {
                0u8
            } else {
                // x^254 is the inverse in GF(2^8).
                let mut r = 1u8;
                let mut base = x as u8;
                let mut e = 254u32;
                while e > 0 {
                    if e & 1 != 0 {
                        r = gf_mul(r, base);
                    }
                    base = gf_mul(base, base);
                    e >>= 1;
                }
                r
            };
            let s = inv ^ inv.rotate_left(1) ^ inv.rotate_left(2) ^ inv.rotate_left(3) ^ inv.rotate_left(4) ^ 0x63;
            sbox[x] = s;
            inv_sbox[s as usize] = x as u8;
        }
        // Key expansion (§5.2): 44 words, the first 4 the key itself.
        let mut w = [[0u8; 4]; 44];
        for i in 0..4 {
            w[i].copy_from_slice(&key[4 * i..4 * i + 4]);
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                t = [sbox[t[1] as usize] ^ rcon, sbox[t[2] as usize], sbox[t[3] as usize], sbox[t[0] as usize]];
                rcon = gf_mul(rcon, 2);
            }
            for k in 0..4 {
                w[i][k] = w[i - 4][k] ^ t[k];
            }
        }
        let mut round_keys = [[0u8; 16]; 11];
        for r in 0..11 {
            for c in 0..4 {
                round_keys[r][4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        Aes128 { round_keys, sbox, inv_sbox }
    }

    fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
        for i in 0..16 {
            state[i] ^= rk[i];
        }
    }

    /// State is column-major: byte `4*c + r` is row r, column c (§3.4).
    fn shift_rows(s: &mut [u8; 16]) {
        let t = *s;
        for c in 0..4 {
            for r in 0..4 {
                s[4 * c + r] = t[4 * ((c + r) % 4) + r];
            }
        }
    }

    fn inv_shift_rows(s: &mut [u8; 16]) {
        let t = *s;
        for c in 0..4 {
            for r in 0..4 {
                s[4 * ((c + r) % 4) + r] = t[4 * c + r];
            }
        }
    }

    fn mix_columns(s: &mut [u8; 16]) {
        for c in 0..4 {
            let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
            s[4 * c] = gf_mul(a[0], 2) ^ gf_mul(a[1], 3) ^ a[2] ^ a[3];
            s[4 * c + 1] = a[0] ^ gf_mul(a[1], 2) ^ gf_mul(a[2], 3) ^ a[3];
            s[4 * c + 2] = a[0] ^ a[1] ^ gf_mul(a[2], 2) ^ gf_mul(a[3], 3);
            s[4 * c + 3] = gf_mul(a[0], 3) ^ a[1] ^ a[2] ^ gf_mul(a[3], 2);
        }
    }

    fn inv_mix_columns(s: &mut [u8; 16]) {
        for c in 0..4 {
            let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
            s[4 * c] = gf_mul(a[0], 14) ^ gf_mul(a[1], 11) ^ gf_mul(a[2], 13) ^ gf_mul(a[3], 9);
            s[4 * c + 1] = gf_mul(a[0], 9) ^ gf_mul(a[1], 14) ^ gf_mul(a[2], 11) ^ gf_mul(a[3], 13);
            s[4 * c + 2] = gf_mul(a[0], 13) ^ gf_mul(a[1], 9) ^ gf_mul(a[2], 14) ^ gf_mul(a[3], 11);
            s[4 * c + 3] = gf_mul(a[0], 11) ^ gf_mul(a[1], 13) ^ gf_mul(a[2], 9) ^ gf_mul(a[3], 14);
        }
    }

    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        Self::add_round_key(block, &self.round_keys[0]);
        for round in 1..10 {
            for b in block.iter_mut() {
                *b = self.sbox[*b as usize];
            }
            Self::shift_rows(block);
            Self::mix_columns(block);
            Self::add_round_key(block, &self.round_keys[round]);
        }
        for b in block.iter_mut() {
            *b = self.sbox[*b as usize];
        }
        Self::shift_rows(block);
        Self::add_round_key(block, &self.round_keys[10]);
    }

    pub fn decrypt_block(&self, block: &mut [u8; 16]) {
        Self::add_round_key(block, &self.round_keys[10]);
        for round in (1..10).rev() {
            Self::inv_shift_rows(block);
            for b in block.iter_mut() {
                *b = self.inv_sbox[*b as usize];
            }
            Self::add_round_key(block, &self.round_keys[round]);
            Self::inv_mix_columns(block);
        }
        Self::inv_shift_rows(block);
        for b in block.iter_mut() {
            *b = self.inv_sbox[*b as usize];
        }
        Self::add_round_key(block, &self.round_keys[0]);
    }
}

/// AES Key Unwrap (RFC 3394 §2.2.2) with a 128-bit KEK - how message 3's key data is opened. `wrapped` is
/// 8 more bytes than the plaintext; `out` receives the plaintext. False if the integrity check value does not
/// come out as `A6A6A6A6A6A6A6A6`, which is the RFC's only verdict on a wrong key or a damaged input.
pub fn aes_key_unwrap(kek: &[u8; 16], wrapped: &[u8], out: &mut [u8]) -> bool {
    if wrapped.len() < 24 || wrapped.len() % 8 != 0 || out.len() < wrapped.len() - 8 {
        return false;
    }
    let n = wrapped.len() / 8 - 1;
    let aes = Aes128::new(kek);
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    let r = &mut out[..n * 8];
    r.copy_from_slice(&wrapped[8..]);
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let t = (n * j + i) as u64;
            let mut b = [0u8; 16];
            let tb = t.to_be_bytes();
            for k in 0..8 {
                b[k] = a[k] ^ tb[k];
            }
            b[8..].copy_from_slice(&r[(i - 1) * 8..i * 8]);
            aes.decrypt_block(&mut b);
            a.copy_from_slice(&b[..8]);
            r[(i - 1) * 8..i * 8].copy_from_slice(&b[8..]);
        }
    }
    a == [0xA6; 8]
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

    // FIPS 197 appendix C.1: AES-128, key 000102..0f, plaintext 00112233..ff.
    let aes = Aes128::new(&[
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    ]);
    let mut block = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    ];
    aes.encrypt_block(&mut block);
    check(
        "AES-128 encrypt (FIPS 197 C.1)",
        &block,
        &[
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a,
        ],
    );
    aes.decrypt_block(&mut block);
    check(
        "AES-128 decrypt (FIPS 197 C.1, back to the plaintext)",
        &block,
        &[
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ],
    );

    // RFC 3394 section 4.1: 128-bit key data wrapped with a 128-bit KEK.
    let kek = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    ];
    let wrapped = [
        0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82,
        0x9D, 0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5,
    ];
    let mut unwrapped = [0u8; 16];
    let unwrap_ok = aes_key_unwrap(&kek, &wrapped, &mut unwrapped);
    check(
        "AES key unwrap (RFC 3394 4.1)",
        &unwrapped,
        &[
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ],
    );
    if !unwrap_ok {
        ok = false;
        ctx.log("wifi-driver:   AES key unwrap - WRONG: the integrity value did not come out as A6A6A6A6A6A6A6A6");
    }

    if ok {
        ctx.log("wifi-driver: stage 0 - every primitive matches its vector; a passphrase will derive the key the standard says");
    } else {
        ctx.log("wifi-driver: stage 0 - A PRIMITIVE IS WRONG. No passphrase will be accepted; joins are refused until this is fixed");
    }
    ok
}
