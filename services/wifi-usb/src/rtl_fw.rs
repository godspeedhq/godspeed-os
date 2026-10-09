// SPDX-License-Identifier: GPL-2.0-only
//! The RTL8192CU family's firmware FILE: its header, and the plan for downloading it - which page, which
//! address, which bytes, in what order (`docs/wifi-usb.md`, phase R2).
//!
//! Pure, and named nothing outside `core`, so `scripts/host_test_check.py` compiles it alone and runs the
//! tests at the bottom on every build: a wrong offset here would otherwise cost a flash to find. The
//! layout is `struct rtl8xxxu_firmware_header` and the download is `rtl8xxxu_download_firmware` (Linux,
//! 26.14): the payload after a 32-byte header goes into the chip in 4 KiB PAGES, a page selected in
//! `MCU_FW_DL`'s third byte and written through the window at `0x1000`, each page in 128-byte control
//! transfers (`writeN_block_size` for this family).

/// The header in front of the code (`struct rtl8xxxu_firmware_header`). Not downloaded.
pub const HEADER_LEN: usize = 32;
/// One page of the download, selected in `MCU_FW_DL + 2` (`RTL_FW_PAGE_SIZE`).
pub const PAGE: usize = 4096;
/// One control transfer of it (`writeN_block_size` for the 8192C family).
pub const BLOCK: usize = 128;
/// Where every page is written (`REG_FW_START_ADDRESS`).
pub const WINDOW: u16 = 0x1000;
/// The page select is three bits (`& 0xF8`), so a payload past eight pages cannot be addressed.
pub const PAGES_MAX: usize = 8;

/// What the header says, for the log and the checks.
#[derive(Debug, PartialEq)]
pub struct Header {
    pub signature: u16,
    pub major: u16,
    pub minor: u8,
    /// `ramcodesize`: what the header claims the code after it is; checked against the file.
    pub code_len: usize,
}

/// The header, or why the file cannot be this family's firmware. The signature's low nibble is the cut
/// (`0x88C1` is an A-cut 8188C), so the family is the upper twelve bits: `0x88C0` and `0x92C0`, the two
/// `rtl8192cu` files Linux accepts (`rtl8xxxu_load_firmware` also accepts other families' signatures,
/// which do not belong to this chip).
pub fn header(fw: &[u8]) -> Result<Header, &'static str> {
    if fw.len() <= HEADER_LEN {
        return Err("shorter than its 32-byte header");
    }
    let le16 = |at: usize| u16::from_le_bytes([fw[at], fw[at + 1]]);
    let h = Header { signature: le16(0), major: le16(4), minor: fw[6], code_len: le16(12) as usize };
    if h.signature & 0xFFF0 != 0x88C0 && h.signature & 0xFFF0 != 0x92C0 {
        return Err("its signature is not an RTL8192CU-family firmware's (0x88Cx or 0x92Cx)");
    }
    if h.code_len != fw.len() - HEADER_LEN {
        return Err("its header's code size does not match the bytes after the header");
    }
    if fw.len() - HEADER_LEN > PAGE * PAGES_MAX {
        return Err("it is longer than the eight pages the page select can address");
    }
    Ok(h)
}

/// FNV-1a over `bytes`, as `build.rs` computes it over the file on disk: recomputed at start over what the
/// binary embedded, the one check that the blob reached it (a length is a constant either way).
pub fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// A decimal `u32` from a build-time environment string, at compile time.
pub const fn decimal(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v: u32 = 0;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

/// One control transfer of the download: select `page`, write `bytes` at `addr`.
#[derive(Debug, PartialEq)]
pub struct Block<'a> {
    pub page: u8,
    pub addr: u16,
    pub bytes: &'a [u8],
}

/// The download, in order: every page in turn, every page in `BLOCK`-byte writes from `WINDOW`, the last
/// page and its last block as short as the code is. `code` is the file after its header.
pub fn blocks(code: &[u8]) -> impl Iterator<Item = Block<'_>> {
    code.chunks(PAGE).enumerate().flat_map(|(page, p)| {
        p.chunks(BLOCK).enumerate().map(move |(i, b)| Block {
            page: page as u8,
            addr: WINDOW + (i * BLOCK) as u16,
            bytes: b,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file shaped like `rtl8192cufw_TMSC.bin` as the board's copy reads (`nonfree/rtl8192cu`): signature
    /// `0x88C1`, version 88.2, 16094 bytes of code.
    fn tmsc_shaped() -> [u8; 16126] {
        let mut f = [0u8; 16126];
        f[0..2].copy_from_slice(&0x88C1u16.to_le_bytes());
        f[2] = 2;
        f[3] = 5;
        f[4..6].copy_from_slice(&88u16.to_le_bytes());
        f[6] = 2;
        f[12..14].copy_from_slice(&16094u16.to_le_bytes());
        for (i, b) in f[HEADER_LEN..].iter_mut().enumerate() {
            *b = i as u8;
        }
        f
    }

    #[test]
    fn fnv1a_and_decimal_match_their_definitions() {
        // The published FNV-1a 32-bit vectors: the empty string is the offset basis, "a" is 0xe40c292c.
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
        assert_eq!(decimal("3792478515"), 3_792_478_515);
    }

    #[test]
    fn the_header_of_the_file_we_carry() {
        let f = tmsc_shaped();
        assert_eq!(header(&f), Ok(Header { signature: 0x88C1, major: 88, minor: 2, code_len: 16094 }));
    }

    #[test]
    fn a_header_that_is_not_this_family_or_not_this_file_is_refused() {
        let mut f = tmsc_shaped();
        f[0..2].copy_from_slice(&0x8723u16.to_le_bytes());
        assert!(header(&f).is_err());
        let mut g = tmsc_shaped();
        g[12..14].copy_from_slice(&16000u16.to_le_bytes());
        assert!(header(&g).is_err());
        assert!(header(&[0u8; 32]).is_err());
    }

    #[test]
    fn the_download_plan_for_16094_bytes() {
        let f = tmsc_shaped();
        let code = &f[HEADER_LEN..];
        let all: [Option<(u8, u16, usize, u8)>; 126] = {
            let mut a = [None; 126];
            for (i, b) in blocks(code).enumerate() {
                a[i] = Some((b.page, b.addr, b.bytes.len(), b.bytes[0]));
            }
            a
        };
        // Three full pages of 32 blocks, then 3806 bytes: 29 full blocks and one of 94.
        assert_eq!(blocks(code).count(), 3 * 32 + 30);
        assert_eq!(all[0], Some((0, 0x1000, 128, 0)));
        assert_eq!(all[31], Some((0, 0x1F80, 128, (31 * 128) as u8)));
        assert_eq!(all[32], Some((1, 0x1000, 128, 4096u32 as u8)));
        assert_eq!(all[125], Some((3, 0x1E80, 94, (3 * 4096 + 29 * 128) as u8)));
        // Every byte of the code, once, in order.
        let mut n = 0usize;
        for b in blocks(code) {
            for (i, x) in b.bytes.iter().enumerate() {
                assert_eq!(*x, (n + i) as u8);
            }
            n += b.bytes.len();
        }
        assert_eq!(n, 16094);
    }
}
