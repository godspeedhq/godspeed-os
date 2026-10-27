// SPDX-License-Identifier: GPL-2.0-only
//! The kernel's capability files, as they ship. A real directory rather than an inline module so the
//! `#[path]`s resolve on Linux, which walks `..` through directories that must exist (Windows
//! resolves them lexically, so an inline module compiled there and failed under `cargo kani`).

#[path = "../../../../kernel/src/capability/cap.rs"]
pub mod cap;
#[path = "../../../../kernel/src/capability/generation.rs"]
pub mod generation;
#[path = "../../../../kernel/src/capability/rights.rs"]
pub mod rights;
#[path = "../../../../kernel/src/capability/table.rs"]
pub mod table;
// `table.rs` asks `delegated::is_delegated` which kind a reused id is (CapTable::get), so the band
// module comes with it, as the kernel builds it.
#[path = "../../../../kernel/src/capability/delegated.rs"]
pub mod delegated;
