// SPDX-License-Identifier: Apache-2.0
//! A bounded block of this task's own memory, as a byte slice.
//!
//! `AllocMem` (syscall 6) maps fresh pages into the calling task and returns their address, and an
//! address is all it returns: turning it into something a service can read and write needs `unsafe`,
//! which no service may hold (CLAUDE.md 18.2). This module is that one step, audited once here, so that a
//! service which needs a working set too large for its stack - the supervisor holding an image it read from
//! disk before starting it - can have one without a heap and without `unsafe` of its own.
//!
//! **It is not an allocator.** There is no free: the kernel reclaims a task's memory only when the task
//! dies (CLAUDE.md 10.5), so a region is asked for once and reused, and its size is the bound, stated where
//! it is asked for (26.6.1). A caller that asks for one per operation leaks one per operation, until its
//! contract's memory limit refuses the next (`AllocError::Denied`), which is loud rather than silent.

use crate::service_context::{AllocError, ServiceContext};

/// The page size `AllocMem` maps in.
pub const PAGE: usize = 4096;

/// A run of pages `AllocMem` mapped for this task, readable and writable, owned by this value.
///
/// Neither `Clone` nor `Copy`: there is one owner, and a mutable slice can be borrowed from it only through
/// `&mut self`, so no two live slices of it can alias.
pub struct Region {
    va: u64,
    len: usize,
}

impl Region {
    /// Ask the kernel for at least `bytes` of fresh memory, rounded up to whole pages.
    ///
    /// `Denied` when it would pass this task's memory limit, `Failed` when the machine has no frames left
    /// or `bytes` is zero or too large to round.
    pub fn alloc(ctx: &ServiceContext, bytes: usize) -> Result<Region, AllocError> {
        if bytes == 0 { return Err(AllocError::Failed); }
        let len = bytes.checked_add(PAGE - 1).ok_or(AllocError::Failed)? & !(PAGE - 1);
        let va = ctx.alloc_mem(len)?;
        Ok(Region { va, len })
    }

    /// Its length in bytes: a whole number of pages, at least what was asked for.
    pub fn len(&self) -> usize { self.len }

    /// Always false: a region of no bytes cannot be made.
    pub fn is_empty(&self) -> bool { self.len == 0 }

    /// The region, to read.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `va .. va + len` is exactly the mapping a successful `AllocMem` made for this task (the
        // constructor is the only way to build a Region): mapped, readable, page-aligned, and initialised,
        // because the kernel hands out zeroed frames. The kernel unmaps a task's memory only when the task
        // dies, so the mapping outlives `self`. The borrow is shared, and a mutable one can only be taken
        // through `&mut self`, so nothing writes while this slice lives.
        unsafe { core::slice::from_raw_parts(self.va as *const u8, self.len) }
    }

    /// The region, to write.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`, and the mapping is writable. `&mut self` on a value that is neither Clone nor
        // Copy means this is the only live slice of the region.
        unsafe { core::slice::from_raw_parts_mut(self.va as *mut u8, self.len) }
    }

    /// The region as 32-bit words, to write: for a device FIFO that is read a word at a time (a WiFi chip's
    /// SDIO read buffer, larger than any stack can hold). The same memory as [`bytes`](Region::bytes), in
    /// the machine's own byte order.
    pub fn words_mut(&mut self) -> &mut [u32] {
        // SAFETY: as `bytes_mut`. The base is page-aligned, so it is aligned for u32, and the length is a whole
        // number of pages, so `len / 4` words lie exactly inside the mapping. Every bit pattern is a valid u32.
        // `&mut self` makes this the only live view of the region.
        unsafe { core::slice::from_raw_parts_mut(self.va as *mut u32, self.len / 4) }
    }
}
