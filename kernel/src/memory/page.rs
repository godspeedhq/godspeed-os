// SPDX-License-Identifier: GPL-2.0-only
//! Virtual page type - §10.
//!
//! A `Page` is a page-aligned virtual address within a specific task's
//! address space. The kernel never dereferences a user virtual address
//! directly; it uses `Page` as a typed index into page tables only.

use crate::arch::x86_64::page_tables::VirtAddr;

pub const PAGE_SIZE: usize = 4096;

/// A page-aligned virtual address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Page(VirtAddr);

impl Page {
    /// # Safety
    /// `addr` must be page-aligned.
    pub unsafe fn from_virt(addr: VirtAddr) -> Self {
        debug_assert!(addr.0 % PAGE_SIZE as u64 == 0);
        Self(addr)
    }

    pub fn virt_addr(self) -> VirtAddr {
        self.0
    }

    pub fn page_number(self) -> u64 {
        self.0.0 / PAGE_SIZE as u64
    }
}
