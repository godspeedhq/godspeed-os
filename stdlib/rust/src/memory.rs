// SPDX-License-Identifier: Apache-2.0
//! A bounded block of this program's own memory, for a working set too large for its stack.
//!
//! [`Region`] is the SDK's, re-exported so an ordinary program never names `godspeed_sdk`: the same type,
//! not a wrapper. Ask for one ONCE and reuse it - there is no free, because the kernel reclaims a program's
//! memory only when it ends (CLAUDE.md 10.5), so a region asked for per operation is one leaked per
//! operation until the memory limit refuses. Most programs never need one: change the representation so
//! the working set is small first (CLAUDE.md 26.6.1). The first user is the supervisor, holding a package's
//! image while it checks and starts it.

pub use godspeed_sdk::memory::Region;
