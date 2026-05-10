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

// ---------------------------------------------------------------------------
// ServiceContextData page layout.
// MUST match `ServiceContextData` in `kernel/src/task/mod.rs`.
// ---------------------------------------------------------------------------

const SERVICE_CTX_ADDR: u64 = 0x3ff000;
const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;

/// Layout of the kernel-written page at SERVICE_CTX_ADDR.
#[repr(C)]
struct ServiceContextData {
    magic:          u32,
    log_write_slot: u32,  // u32::MAX = not held
    recv_slot:      u32,  // u32::MAX = not held
    _pad:           u32,
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
    /// Name format matches the contract key: `"log_write"`, `"ipc_send.pong"`, etc.
    pub fn capability(&self, name: &str) -> Result<CapHandle, CapError> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(CapError::CapNotHeld); }
        match name {
            "log_write" if data.log_write_slot != u32::MAX =>
                Ok(CapHandle(data.log_write_slot)),
            _ => Err(CapError::CapNotHeld),
        }
    }

    /// Block until a message arrives on this service's receive endpoint.
    pub fn recv(&self) -> Message {
        todo!("Phase 4: call ipc::recv on this service's primary receive endpoint")
    }

    /// Send to a named peer declared in `ipc_send`.
    pub fn send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("Phase 4: look up peer cap, call ipc::send")
    }

    /// Non-blocking send; returns QueueFull immediately.
    pub fn try_send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("Phase 4: look up peer cap, call ipc::try_send")
    }

    /// Advisory yield (§9.3).
    pub fn yield_cpu(&self) {
        // SAFETY: syscall instruction is always valid from ring-3.
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

        // SAFETY: syscall instruction is valid from ring-3; bytes is a valid slice.
        unsafe {
            raw_syscall(
                5,
                slot as u64,
                bytes.as_ptr() as u64,
                len as u64,
            );
        }
    }

    /// Log a formatted message.
    pub fn log_fmt(&self, args: core::fmt::Arguments) {
        // Stack-allocate a small buffer for formatting.
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
        todo!("Phase 4: syscall to drain kernel ring buffer")
    }

    /// Spawn a service by name (init only — §11.1).
    pub fn spawn(&self, _name: &str) -> Result<(), crate::Error> {
        // Phase 4: syscall Spawn with name ptr+len; returns Ok on success.
        // Phase 3 stub returns Ok so init's spawn calls succeed harmlessly.
        Ok(())
    }

    /// Spawn a service from a full descriptor (supervisor only — §14.1).
    pub fn spawn_service(&self, service: &ServiceDescriptor) -> Result<(), crate::Error> {
        todo!("Phase 4: syscall Spawn from binary + contract")
    }

    /// Restart a service with optional core override (supervisor only — §14.4).
    pub fn restart(&self, name: &str, core_override: Option<u32>) -> Result<(), crate::Error> {
        todo!("Phase 4: syscall Kill then Spawn per §9.2 placement rules")
    }

    /// Receive a service death notification (supervisor only).
    pub fn recv_death_notification(&self) -> Option<&str> {
        todo!("Phase 4: block on the supervisor's death-notification endpoint")
    }

    /// Read the boot manifest (supervisor only — §11.1).
    pub fn read_boot_manifest(&self) -> BootManifest {
        todo!("Phase 4: read the manifest embedded in the kernel image")
    }

    pub fn recv_log_message(&self) -> Message {
        self.recv()
    }
}

// ---------------------------------------------------------------------------
// Raw syscall wrapper.
// ---------------------------------------------------------------------------

/// Issue a three-argument syscall.
///
/// Calling convention (matches `syscall_entry.rs` stub):
///   rax = syscall number
///   rdi = arg0, rsi = arg1, rdx = arg2
/// Return value in rax.
///
/// SYSCALL clobbers rcx (saves user RIP) and r11 (saves user RFLAGS);
/// SYSRETQ restores them from the kernel-pushed values.
///
/// # Safety
/// The caller is responsible for passing valid arguments for the given syscall.
#[inline]
unsafe fn raw_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    // SAFETY: SYSCALL transitions to ring-0 and back; valid from ring-3 at any time.
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,   // clobbered: SYSCALL stores user RIP here
            out("r11") _,   // clobbered: SYSCALL stores user RFLAGS here
            options(nostack),
        );
    }
    ret
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
// Placeholder types (Phase 4).
// ---------------------------------------------------------------------------

pub struct ServiceDescriptor;

impl ServiceDescriptor {
    pub fn name(&self) -> &str { todo!() }
}

pub struct BootManifest;

impl BootManifest {
    pub fn services(&self) -> &[ServiceDescriptor] { todo!() }
}
