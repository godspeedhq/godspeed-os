//! ServiceContext — entry-point type handed to every service's `service_main`.
//!
//! Provides safe, named access to the capabilities the service declared in its
//! contract. Capability names match the contract field names exactly.
//! Requesting a cap not in the contract returns `Err(CapNotHeld)`.
//!
//! The kernel writes a `ServiceContextData` struct to the fixed page at
//! `SERVICE_CTX_ADDR` (0x3ff000) in the service's address space before
//! launching it.  `ServiceContext` methods read capability slot assignments
//! from that page and issue the appropriate syscall instructions.

use crate::capability::{CapError, CapHandle};
use crate::ipc::{IpcError, Message};
use crate::syscall::raw_syscall;

// ---------------------------------------------------------------------------
// ServiceContextData page layout.
// MUST match `ServiceContextData` in `kernel/src/task/mod.rs`.
// ---------------------------------------------------------------------------

const SERVICE_CTX_ADDR:  u64 = 0x3ff000;
const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;

/// Layout of the kernel-written page at SERVICE_CTX_ADDR.
#[repr(C)]
struct ServiceContextData {
    magic:          u32,
    log_write_slot: u32,  // u32::MAX = not held
    recv_slot:      u32,  // u32::MAX = not held
    spawn_slot:     u32,  // u32::MAX = not held
}

// ---------------------------------------------------------------------------
// ServiceContext.
// ---------------------------------------------------------------------------

/// Passed by the kernel to `service_main`. Non-Copy; one per service instance.
pub struct ServiceContext {
    _private: (),
}

impl ServiceContext {
    /// Read the kernel-written context data page.
    ///
    /// # Safety
    /// Called only from service code. The kernel maps and writes this page
    /// before launching; it remains valid for the service's lifetime.
    #[inline]
    fn ctx() -> &'static ServiceContextData {
        // SAFETY: kernel guarantees a valid ServiceContextData at SERVICE_CTX_ADDR
        // before SYSRETQ into the service.  The page is read-only and mapped for
        // the service's entire lifetime.
        unsafe { &*(SERVICE_CTX_ADDR as *const ServiceContextData) }
    }

    /// Look up a named capability from this service's cap table.
    ///
    /// Name format matches the contract key: `"log_write"`, `"spawn"`, etc.
    pub fn capability(&self, name: &str) -> Result<CapHandle, CapError> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(CapError::CapNotHeld); }
        match name {
            "log_write" if data.log_write_slot != u32::MAX =>
                Ok(CapHandle(data.log_write_slot)),
            "spawn" if data.spawn_slot != u32::MAX =>
                Ok(CapHandle(data.spawn_slot)),
            _ => Err(CapError::CapNotHeld),
        }
    }

    /// Block until a message arrives on this service's primary receive endpoint.
    pub fn recv(&self) -> Message {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { loop {} }
        let slot = data.recv_slot;
        if slot == u32::MAX { loop {} } // no recv endpoint configured
        match crate::ipc::recv(CapHandle(slot)) {
            Ok(msg) => msg,
            Err(_) => loop {},
        }
    }

    /// Send to a named peer declared in `ipc_send`.
    pub fn send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("Phase 5: look up peer cap by name, call ipc::send — peer={}", peer)
    }

    /// Non-blocking send; returns `QueueFull` immediately.
    pub fn try_send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("Phase 5: look up peer cap by name, call ipc::try_send — peer={}", peer)
    }

    /// Advisory yield (§9.3).
    pub fn yield_cpu(&self) {
        // SAFETY: syscall(4) = Yield; always valid from ring-3.
        unsafe { raw_syscall(4, 0, 0, 0); }
    }

    /// Log a string via the kernel ring buffer (syscall 5, requires log_write cap).
    pub fn log(&self, msg: &str) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.log_write_slot;
        if slot == u32::MAX { return; }

        let bytes = msg.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > 256 { return; }

        // SAFETY: syscall(5) = Log; bytes is a valid slice within user space.
        unsafe {
            raw_syscall(5, slot as u64, bytes.as_ptr() as u64, len as u64);
        }
    }

    /// Log a formatted message.
    pub fn log_fmt(&self, args: core::fmt::Arguments) {
        let mut buf = [0u8; 256];
        let mut cursor = 0usize;
        let _ = core::fmt::write(
            &mut StackWriter { buf: &mut buf, pos: &mut cursor },
            args,
        );
        if cursor > 0 {
            self.log(core::str::from_utf8(&buf[..cursor]).unwrap_or("(fmt error)"));
        }
    }

    /// Drain the kernel ring buffer. Called once by logger at startup (§11.4).
    pub fn drain_kernel_ring_buffer(&self) {
        todo!("Phase 5: syscall to drain kernel ring buffer")
    }

    /// Spawn a service by name via the Spawn syscall (syscall 7).
    ///
    /// Requires this service to hold a spawn capability.
    /// The kernel looks up the service ELF by name and spawns on core 0.
    pub fn spawn(&self, name: &str) -> Result<(), crate::Error> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC {
            return Err(crate::Error::InvalidArgument);
        }
        let slot = data.spawn_slot;
        if slot == u32::MAX {
            return Err(crate::Error::Cap(CapError::CapNotHeld));
        }
        let bytes = name.as_bytes();
        // SAFETY: syscall(7) = Spawn; slot is from kernel-written page; bytes is valid.
        let ret = unsafe {
            raw_syscall(7, slot as u64, bytes.as_ptr() as u64, bytes.len() as u64)
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(crate::Error::InvalidArgument)
        }
    }

    /// Spawn a service from a full descriptor (supervisor only — §14.1).
    pub fn spawn_service(&self, service: &ServiceDescriptor) -> Result<(), crate::Error> {
        todo!("Phase 5: syscall Spawn from binary + contract")
    }

    /// Restart a service with optional core override (supervisor only — §14.4).
    pub fn restart(&self, name: &str, core_override: Option<u32>) -> Result<(), crate::Error> {
        todo!("Phase 5: syscall Kill then Spawn per §9.2 placement rules")
    }

    /// Receive a service death notification (supervisor only).
    pub fn recv_death_notification(&self) -> Option<&str> {
        todo!("Phase 5: block on the supervisor's death-notification endpoint")
    }

    /// Read the boot manifest (supervisor only — §11.1).
    pub fn read_boot_manifest(&self) -> BootManifest {
        todo!("Phase 5: read the manifest embedded in the kernel image")
    }

    pub fn recv_log_message(&self) -> Message {
        self.recv()
    }
}

// ---------------------------------------------------------------------------
// Stack-based fmt::Write helper for log_fmt.
// ---------------------------------------------------------------------------

struct StackWriter<'a> {
    buf: &'a mut [u8],
    pos: &'a mut usize,
}

impl<'a> core::fmt::Write for StackWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let space = self.buf.len().saturating_sub(*self.pos);
        let n = bytes.len().min(space);
        self.buf[*self.pos .. *self.pos + n].copy_from_slice(&bytes[..n]);
        *self.pos += n;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Placeholder types (Phase 5).
// ---------------------------------------------------------------------------

pub struct ServiceDescriptor;

impl ServiceDescriptor {
    pub fn name(&self) -> &str { todo!() }
}

pub struct BootManifest;

impl BootManifest {
    pub fn services(&self) -> &[ServiceDescriptor] { todo!() }
}
