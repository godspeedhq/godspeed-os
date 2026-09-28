// SPDX-License-Identifier: GPL-2.0-only
//! Link this service, and **embed the radio's firmware in its own binary**.
//!
//! ## Why the firmware is embedded rather than read from `fs`
//!
//! Because GodspeedOS is a live system, and this is how a live system does it. A Linux live ISO does not
//! fetch firmware from a disk either: it carries `/lib/firmware/brcm/brcmfmac43455-sdio.bin` inside the
//! squashfs or initramfs that was loaded into RAM at boot, and `request_firmware()` reads it from there.
//! The blob travels WITH the kernel image, not beside it on storage.
//!
//! Reading it through `fs` would have been strictly worse on this board, and not by a little:
//!
//!   * `block-driver` on the Pi 4 is built `storage_is_usb`, so the disk is behind the **`xhci` service**.
//!     The radio's firmware would then depend on the USB stack coming up AND a stick being plugged in -
//!     a dependency chain three services long for a file that is already in this repository.
//!   * it would need an `fs` send peer, which is new authority for a driver that has none (§3.1: granted
//!     deliberately or not at all). Embedding needs no capability whatsoever.
//!   * and it would fail on any machine booted without storage, which is every first boot.
//!
//! So the service carries what it uploads, exactly as the supervisor carries the service ELFs it spawns.
//! The cost is about 600 KiB in the boot image, which is what a live system pays for working out of the box.
//!
//! ## Why this is not a heap or an unbounded buffer (§26.6.1)
//!
//! An `include_bytes!` is read-only data in the mapped image. Nothing allocates, nothing grows, and the
//! upload streams it out in CMD53-sized blocks rather than copying it anywhere - which is the streaming
//! discipline that section asks for rather than an exception to it.

use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/wifi-driver -> services/ -> workspace root
    let workspace = Path::new(&manifest).parent().unwrap().parent().unwrap();

    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // The vendored blobs, whose licence and provenance `scripts/nonfree_check.py` enforces on every build.
    // Named here rather than reached with a relative `include_bytes!` path for the reason the supervisor
    // does the same for service ELFs: the path is computed once, in one place, and `rerun-if-changed`
    // actually fires when the file changes.
    let dir = workspace.join("nonfree").join("brcm43455");
    for (var, name, what) in [
        ("WIFI_FW_BIN", "brcmfmac43455-sdio.bin", "the firmware image the chip's processor runs"),
        ("WIFI_FW_NVRAM", "brcmfmac43455-sdio.txt", "the board-specific NVRAM calibration text"),
    ] {
        let path = dir.join(name);
        // REFUSED, not silently skipped. A missing blob would otherwise produce a driver that compiles,
        // boots, reports a radio and uploads nothing - which is the shape of failure this project spends
        // its enforcement layer refusing. `nonfree_check.py` guards the licence and the digest; this
        // guards that the file is actually here to be embedded.
        if !path.exists() {
            panic!(
                "wifi-driver: {} is missing ({}). It is {} and is vendored in this repository - see \
                 docs/licensing.md 5a. Without it this service would build and upload nothing.",
                name,
                path.display(),
                what
            );
        }
        println!("cargo:rustc-env={}={}", var, path.display());
        println!("cargo:rerun-if-changed={}", path.display());
        // THE LENGTH AS MEASURED ON DISK, passed through so the service can cross-check what it actually
        // embedded. This exists because `const` + `include_bytes!` silently discards the data when only
        // `.len()` is used: the build passed, every checker passed, and the binary was 135,312 bytes while
        // claiming to carry a 609 KB image. An arithmetic guard catches that; noticing does not.
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("wifi-driver: cannot read {}: {}", path.display(), e));
        println!("cargo:rustc-env={}_LEN={}", var, bytes.len());
        // AND A HASH OF THE CONTENT, which the service recomputes over what it embedded.
        //
        // This exists because a size assertion CANNOT catch the failure it was written for: `.len()` is a
        // compile-time constant whether or not the linker keeps the data, and the first attempt at a guard
        // passed while the bytes were absent from the ELF. Hashing them at boot is the only check that
        // works, and it works because it genuinely READS them - which is also what keeps the linker from
        // dropping a section nothing else touches.
        //
        // FNV-1a: four lines, identical here and in `no_std`, no dependency. Not cryptographic and not
        // claiming to be - `scripts/nonfree_check.py` owns the SHA-256 and catches substitution. This
        // catches a blob that did not reach the binary, or reached it wrong.
        let mut h: u32 = 0x811c_9dc5;
        for b in &bytes {
            h ^= *b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
        println!("cargo:rustc-env={}_FNV={}", var, h);
    }
}
