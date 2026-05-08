//! ServiceContext — entry-point type handed to every service's `service_main`.
//!
//! Provides safe, named access to the capabilities the service declared in its
//! contract. Capability names match the contract field names exactly.
//! Requesting a cap not in the contract returns `Err(CapNotHeld)`.

use crate::capability::{CapError, CapHandle};
use crate::ipc::{IpcError, Message};

/// Passed by the kernel to `service_main`. Non-Copy; one per service instance.
pub struct ServiceContext {
    // The kernel populates this at spawn time from the contract.
    // Service code only calls methods; it never touches raw cap slots.
    _private: (),
}

impl ServiceContext {
    /// Look up a named capability from this service's cap table.
    ///
    /// Name format matches the contract key: `"log_write"`, `"ipc_send.pong"`, etc.
    pub fn capability(&self, name: &str) -> Result<CapHandle, CapError> {
        todo!("syscall: look up named cap slot from kernel's contract metadata for this task")
    }

    /// Block until a message arrives on this service's receive endpoint.
    pub fn recv(&self) -> Message {
        todo!("call ipc::recv on this service's primary receive endpoint")
    }

    /// Send to a named peer declared in `ipc_send`.
    pub fn send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("look up peer cap, call ipc::send")
    }

    /// Non-blocking send; returns QueueFull immediately.
    pub fn try_send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        todo!("look up peer cap, call ipc::try_send")
    }

    /// Advisory yield (§9.3).
    pub fn yield_cpu(&self) {
        todo!("syscall SyscallNumber::Yield")
    }

    /// Log a static string via the logger service.
    pub fn log(&self, msg: &str) {
        todo!("send to log_write endpoint")
    }

    /// Log a formatted message.
    pub fn log_fmt(&self, args: core::fmt::Arguments) {
        todo!("format into stack buffer, call self.log")
    }

    // --- TCB-only helpers (only init and supervisor call these) ---

    /// Drain the kernel ring buffer. Called once by logger at startup (§11.4).
    pub fn drain_kernel_ring_buffer(&self) {
        todo!("syscall: read all bytes from the kernel ring buffer into the log sink")
    }

    /// Spawn a service by name (init only — §11.1).
    pub fn spawn(&self, name: &str) -> Result<(), crate::Error> {
        todo!("syscall: spawn service from the boot manifest by name")
    }

    /// Spawn a service from a full descriptor (supervisor only — §14.1).
    pub fn spawn_service(&self, service: &ServiceDescriptor) -> Result<(), crate::Error> {
        todo!("syscall: spawn from binary + contract, apply placement rules")
    }

    /// Restart a service with optional core override (supervisor only — §14.4).
    pub fn restart(&self, name: &str, core_override: Option<u32>) -> Result<(), crate::Error> {
        todo!("syscall: kill then re-spawn per §9.2 placement rules")
    }

    /// Receive a service death notification (supervisor only).
    pub fn recv_death_notification(&self) -> Option<&str> {
        todo!("block on the supervisor's death-notification endpoint")
    }

    /// Read the boot manifest (supervisor only — §11.1).
    pub fn read_boot_manifest(&self) -> BootManifest {
        todo!("read the manifest embedded in the kernel image or from block device")
    }

    pub fn recv_log_message(&self) -> Message {
        self.recv()
    }
}

pub struct ServiceDescriptor {
    // Parsed service.toml
}

impl ServiceDescriptor {
    pub fn name(&self) -> &str { todo!() }
}

pub struct BootManifest;

impl BootManifest {
    pub fn services(&self) -> &[ServiceDescriptor] { todo!() }
}
