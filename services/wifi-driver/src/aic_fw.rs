// SPDX-License-Identifier: GPL-2.0-only
//! The AIC8800D80's firmware, carried in this service's own binary on the VisionFive (`build.rs` embeds it
//! only in the riscv64 build), for the reason `firmware.rs` gives for the CYW43455's: a live system carries
//! its firmware with its image, and reading it through `fs` would make the radio wait on the USB stack.
//!
//! Five files from `nonfree/aic8800d80/`, whose licence and provenance are recorded there: the patch table
//! (where the patches go and what the chip's registers are set to), the ADID and ROM patches, the extension
//! patch the table names, and `fmacfw`, the full-MAC firmware. `verify` reads every byte against the hash
//! the build measured on disk - which is also what keeps the linker from dropping them (`firmware.rs`).

use crate::firmware::{fnv1a, konst_u32};

pub static TABLE: &[u8] = include_bytes!(env!("AIC_FW_TABLE"));
pub static ADID: &[u8] = include_bytes!(env!("AIC_FW_ADID"));
pub static PATCH: &[u8] = include_bytes!(env!("AIC_FW_PATCH"));
pub static EXT0: &[u8] = include_bytes!(env!("AIC_FW_EXT0"));
pub static FMAC: &[u8] = include_bytes!(env!("AIC_FW_FMAC"));

const FNV: [u32; 5] = [
    konst_u32(env!("AIC_FW_TABLE_FNV")),
    konst_u32(env!("AIC_FW_ADID_FNV")),
    konst_u32(env!("AIC_FW_PATCH_FNV")),
    konst_u32(env!("AIC_FW_EXT0_FNV")),
    konst_u32(env!("AIC_FW_FMAC_FNV")),
];

/// Check every embedded file against the build's hash. Returns false, naming the file, if any differs.
pub fn verify(ctx: &godspeed_sdk::ServiceContext) -> bool {
    let files: [(&str, &[u8]); 5] =
        [("patch table", TABLE), ("ADID", ADID), ("patch", PATCH), ("ext0", EXT0), ("fmacfw", FMAC)];
    let mut ok = true;
    for (i, (name, bytes)) in files.iter().enumerate() {
        let h = fnv1a(bytes);
        if h != FNV[i] {
            ctx.log_fmt(format_args!(
                "wifi-driver: the embedded AIC {} does NOT match the build ({} bytes, fnv {:#010x}, want {:#010x}) - nothing is uploaded",
                name, bytes.len(), h, FNV[i]));
            ok = false;
        }
    }
    if ok {
        ctx.log_fmt(format_args!(
            "wifi-driver: the embedded AIC firmware VERIFIES - table {} B, ADID {} B, patch {} B, ext0 {} B, fmacfw {} B",
            TABLE.len(), ADID.len(), PATCH.len(), EXT0.len(), FMAC.len()));
    }
    ok
}
