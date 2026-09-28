// SPDX-License-Identifier: GPL-2.0-only
//! The radio's firmware, carried in this service's own binary.
//!
//! **Why it is here and not on a disk.** GodspeedOS is a live system today - it boots from a card and has
//! no installed filesystem - and this is exactly how a live system supplies firmware. A Linux live ISO does
//! not read it from storage either: the blob sits inside the squashfs or initramfs that was loaded into RAM
//! at boot, and `request_firmware()` finds it there. It travels with the kernel image.
//!
//! On this board the alternative would have been markedly worse. `block-driver` is built `storage_is_usb`,
//! so the disk is behind the **`xhci` service**: reading firmware through `fs` would make the radio depend
//! on the USB stack coming up and on a stick being present, for a file already in this repository. It would
//! also need an `fs` send peer - new authority for a driver that currently has none - where embedding needs
//! no capability at all.
//!
//! **What the two blobs are.** `brcmfmac43455-sdio.bin` is the program the chip's own processor runs; the
//! part has no ROM firmware for its MAC, so until this is uploaded there is no 802.11 inside to talk to.
//! `brcmfmac43455-sdio.txt` is board-specific NVRAM calibration data, which brcmfmac writes to the END of
//! the chip's RAM rather than to the start. Both are vendored under `nonfree/brcm43455/` with their licence
//! and a SHA-256 that `scripts/nonfree_check.py` verifies on every build (`docs/licensing.md` 5a).
//!
//! The CLM blob in that directory is deliberately NOT embedded yet: it is delivered to the running firmware
//! through an iovar rather than written into RAM, and that path does not exist. Embedding it now would be
//! 2.6 KiB of image for nothing (§26.2).

/// The firmware image the chip's processor runs. Written to the start of its RAM.
///
/// **`static`, NOT `const`, and the difference is whether the bytes exist at all.** A `const` is inlined
/// at each use site rather than stored, so when the only uses were `.len()` the compiler kept the two
/// lengths and DISCARDED 609 KB of firmware - leaving a service binary of 135,312 bytes that reported
/// carrying an image it did not have. The build passed, every checker passed, and the failure would have
/// appeared part-way through an upload as a transfer of whatever happened to be at that address.
///
/// `static` is necessary and STILL NOT SUFFICIENT, which is the part worth knowing: measured on this
/// board, changing `const` to `static` left the ELF byte-for-byte the same size and a run from the
/// firmware still absent from it. Dead data is dropped at link time whatever its declaration. What keeps
/// the bytes is `verify` below genuinely READING them - and that is a real check rather than a trick to
/// defeat the linker, since the same pass proves they are the blob that was vendored.
pub static IMAGE: &[u8] = include_bytes!(env!("WIFI_FW_BIN"));

/// Board-specific NVRAM calibration text. Written to the END of the chip's RAM. `static` for the reason
/// above.
pub static NVRAM: &[u8] = include_bytes!(env!("WIFI_FW_NVRAM"));

/// FNV-1a over a byte slice. Four lines, and the same four `build.rs` runs over the file on disk.
///
/// **This is the check that actually works**, and the reason it does is that it READS every byte. A size
/// assertion cannot catch an embed that was dropped - `.len()` is a compile-time constant whether or not
/// the linker keeps the data, and the first guard written here passed while the bytes were absent from the
/// ELF. Hashing them proves they are present AND that they are the ones that were vendored.
///
/// It is also why the bytes survive linking at all: dead data is dropped, and this makes them not dead.
/// That is a real use rather than a trick - the same pass that retains them is the one that verifies them.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in bytes {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// What `build.rs` computed over the same files on disk. Compared at boot by `verify`.
///
/// Not cryptographic and not claiming to be: `scripts/nonfree_check.py` owns the SHA-256 and catches
/// substitution in the repository. This pair catches the gap nothing covered before - whether the BOOTED
/// IMAGE carries what the repository holds.
const IMAGE_FNV: u32 = konst_u32(env!("WIFI_FW_BIN_FNV"));
const NVRAM_FNV: u32 = konst_u32(env!("WIFI_FW_NVRAM_FNV"));

/// Parse a decimal `u32` at compile time, since `env!` yields a string and `no_std` has no `parse` in
/// const context.
const fn konst_u32(s: &str) -> u32 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v: u32 = 0;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u32;
        i += 1;
    }
    v
}

/// Check that the embedded blobs are the ones that were vendored. Returns false and says so if not.
pub fn verify(ctx: &godspeed_sdk::ServiceContext) -> bool {
    let img = fnv1a(IMAGE);
    let nvr = fnv1a(NVRAM);
    if img == IMAGE_FNV && nvr == NVRAM_FNV {
        ctx.log_fmt(format_args!(
            "wifi-driver: the embedded blobs VERIFY - image {} bytes fnv {:#010x}, NVRAM {} bytes fnv \
             {:#010x}, both matching what the build measured on disk",
            IMAGE.len(),
            img,
            NVRAM.len(),
            nvr
        ));
        return true;
    }
    // LOUD, because the alternative is uploading whatever happened to be at that address. This is exactly
    // the failure the first guard was meant to catch and could not.
    ctx.log_fmt(format_args!(
        "wifi-driver: the embedded firmware does NOT match what the build measured - image fnv {:#010x} \
         want {:#010x}, NVRAM fnv {:#010x} want {:#010x}. The binary is not carrying the vendored blob, \
         so nothing is uploaded",
        img, IMAGE_FNV, nvr, NVRAM_FNV
    ));
    false
}

/// Report what is carried, and whether it fits the memory the chip reported.
///
/// `ram_size` and `ram_base` come from the CR4 itself (`armcr4::probe`), so this is the chip's own answer
/// checked against the image this build actually holds - not against a number from a document.
pub fn report(ram_size: u32, ram_base: u32, ctx: &godspeed_sdk::ServiceContext) {
    // VERIFY BEFORE REPORTING A SIZE. Reporting "609309 bytes of image" while holding none of it is what
    // this whole mechanism exists to prevent, so the check runs first and its result is part of the report.
    if !verify(ctx) {
        return;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: firmware is EMBEDDED in this service - {} bytes of image, {} bytes of NVRAM. No \
         disk, no `fs` peer, no USB stack: a live system carries what it uploads",
        IMAGE.len(),
        NVRAM.len()
    ));
    // THE FIT IS CHECKED AGAINST WHAT THE CHIP SAID, and against both ends of the RAM, because the two
    // blobs go to opposite ends of it: the image up from `ram_base`, the NVRAM down from the top. A
    // total that fits does not by itself mean they do not collide, so both are stated.
    let total = IMAGE.len() as u64 + NVRAM.len() as u64;
    if total <= ram_size as u64 {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} bytes total into {} KiB of TCM at {:#08x} - fits, with {} KiB between the \
             image and the NVRAM at the top",
            total,
            ram_size / 1024,
            ram_base,
            (ram_size as u64 - total) / 1024
        ));
    } else {
        ctx.log_fmt(format_args!(
            "wifi-driver: {} bytes does NOT fit in {} KiB of TCM. Either the bank walk is wrong or this \
             is the wrong firmware for this part - and it is better to know now than part-way through a \
             600 KB transfer",
            total,
            ram_size / 1024
        ));
    }
}
