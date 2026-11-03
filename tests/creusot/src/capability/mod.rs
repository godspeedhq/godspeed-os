// SPDX-License-Identifier: GPL-2.0-only
//! The kernel's capability files, as they ship. `cap.rs` comes because the queue's message carries caps.

#[path = "../../../../kernel/src/capability/cap.rs"]
pub mod cap;
#[path = "../../../../kernel/src/capability/generation.rs"]
pub mod generation;
#[path = "../../../../kernel/src/capability/rights.rs"]
pub mod rights;
