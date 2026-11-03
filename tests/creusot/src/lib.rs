// SPDX-License-Identifier: GPL-2.0-only
//! The kernel's pure capability and queue functions, compiled as they ship, for Creusot to prove
//! (backlog/90, layer C). Each `#[path]` names the real file; the contracts are on the functions in it.
//! README.md says what is proved, and how to run it.

#![allow(dead_code)]
extern crate creusot_std;

pub mod capability;
pub mod ipc;
