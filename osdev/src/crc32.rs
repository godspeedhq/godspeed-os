//! CRC32 (IEEE 802.3, reflected, poly `0xEDB88320`) - host-side copy of the GSFS0004
//! checksum used by `services/fs/src/crc32.rs`. Byte-identical to the on-disk writer so a
//! host-baked image (`osdev mkfs` / `script-disk`) checksums exactly as `fs` would. The
//! algorithm is the universal standard one, so the two copies cannot drift in meaning.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// Standard CRC32 of `data` (init `0xFFFFFFFF`, final XOR `0xFFFFFFFF`).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let mut i = 0;
    while i < data.len() {
        crc = TABLE[((crc ^ data[i] as u32) & 0xFF) as usize] ^ (crc >> 8);
        i += 1;
    }
    crc ^ 0xFFFF_FFFF
}
