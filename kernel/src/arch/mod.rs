// SPDX-License-Identifier: GPL-2.0-only
// Arch layer is the unsafe boundary (§18.1).
// All unsafe code in the kernel lives here (or in memory/, capability/, smp/).
//
// `imp` is THE seam: it aliases the implementation module for the current target arch. The rest of the
// kernel reaches the hardware only through `crate::arch::imp::...`, never a named arch like
// `crate::arch::x86_64::...` (aarch64 Phase 0 - docs/aarch64.md). Because `pub use x86_64 as imp` makes
// `arch::imp` a literal alias of `arch::x86_64` on this target, routing the neutral layers through it is
// behavior-identical (the compiler resolves the same module) - so adding `arch/aarch64/` that exposes
// the same surface becomes a drop-in, with no arch-neutral call site to touch.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64 as imp;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64 as imp;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64 as imp;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64 as imp;

#[cfg(target_arch = "s390x")]
pub mod s390x;
#[cfg(target_arch = "s390x")]
pub use s390x as imp;
