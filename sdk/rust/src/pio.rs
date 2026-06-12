//! Capability-mediated I/O port access for userspace driver services
//! (§12, docs/persistence.md §5).
//!
//! Part of the SDK's audited hardware/ABI layer (§18.1): the only `unsafe` here
//! calls `raw_syscall`, exactly like `ipc.rs` and `mmio.rs`. Unlike MMIO — which
//! a driver reads directly from a kernel-mapped window — ring-3 services cannot
//! execute `in`/`out`, so each port access is a kernel syscall (`PortRead` 30 /
//! `PortWrite` 31). The kernel validates every access against this task's
//! `hw_pio` grant; the safe [`Pio`] wrapper hides the syscalls so driver code
//! never writes `unsafe`.

use crate::syscall::raw_syscall;

const SYS_PORT_READ:  u64 = 30;
const SYS_PORT_WRITE: u64 = 31;

/// Access to the I/O ports granted to this driver by its `hw_pio` grant. Every
/// read/write is validated by the kernel against the grant; an out-of-range or
/// ungranted port yields `None` (read) / `false` (write). Zero-sized: the
/// authority lives in the kernel-side grant, not in this handle.
#[derive(Clone, Copy)]
pub struct Pio;

impl Pio {
    /// Construct the port-I/O accessor. Crate-internal: handed to services via
    /// [`crate::ServiceContext::pio`].
    pub(crate) fn new() -> Self {
        Self
    }

    /// Read a byte from `port`. `None` if the kernel denied the access.
    #[inline]
    pub fn read8(&self, port: u16) -> Option<u8> {
        // SAFETY: raw_syscall(PortRead); all args are scalars, no pointers. A
        // negative return is the kernel's "denied / bad args" path.
        let r = unsafe { raw_syscall(SYS_PORT_READ, port as u64, 1, 0) };
        if r < 0 { None } else { Some(r as u8) }
    }

    /// Read a 16-bit word from `port` (e.g. the ATA data register 0x170/0x1F0).
    #[inline]
    pub fn read16(&self, port: u16) -> Option<u16> {
        // SAFETY: raw_syscall(PortRead) with width 2; scalar args only.
        let r = unsafe { raw_syscall(SYS_PORT_READ, port as u64, 2, 0) };
        if r < 0 { None } else { Some(r as u16) }
    }

    /// Write a byte to `port`. `false` if the kernel denied the access.
    #[inline]
    pub fn write8(&self, port: u16, val: u8) -> bool {
        // SAFETY: raw_syscall(PortWrite) with width 1; scalar args only.
        unsafe { raw_syscall(SYS_PORT_WRITE, port as u64, 1, val as u64) == 0 }
    }

    /// Write a 16-bit word to `port`.
    #[inline]
    pub fn write16(&self, port: u16, val: u16) -> bool {
        // SAFETY: raw_syscall(PortWrite) with width 2; scalar args only.
        unsafe { raw_syscall(SYS_PORT_WRITE, port as u64, 2, val as u64) == 0 }
    }
}
