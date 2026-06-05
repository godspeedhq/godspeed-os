//! ServiceContext — entry-point type handed to every service's `service_main`.
//!
//! Provides safe, named access to the capabilities the service declared in its
//! contract. Capability names match the contract field names exactly.
//! Requesting a cap not in the contract returns `Err(CapNotHeld)`.

use crate::capability::{CapError, CapHandle};
use crate::ipc::{IpcError, Message};
use crate::syscall::raw_syscall;

// ---------------------------------------------------------------------------
// ServiceContextData page layout.
// MUST match `ServiceContextData` in `kernel/src/task/mod.rs`.
// ---------------------------------------------------------------------------

const SERVICE_CTX_ADDR:    u64   = 0x3ff000;
const SERVICE_CTX_MAGIC:   u32   = 0xD0_5D_EA_D5;
const MAX_SEND_PEERS:      usize = 4;
const PEER_NAME_BYTES:     usize = 24;

#[repr(C)]
struct SendPeerEntry {
    slot:     u32,
    name_len: u32,
    name:     [u8; PEER_NAME_BYTES],
}

/// Layout of the kernel-written page at SERVICE_CTX_ADDR.
#[repr(C)]
struct ServiceContextData {
    magic:              u32,
    log_write_slot:     u32,
    recv_slot:          u32,
    spawn_slot:         u32,
    send_peer_count:    u32,
    core_id:            u32,
    probe_mode:         u32,
    console_read_slot:  u32, // u32::MAX = not present
    xhci_mmio_va:       u64, // 0 = not mapped; else VA of the mapped xHCI BAR
    xhci_dma_va:        u64, // 0 = none; else VA of the driver's DMA arena
    xhci_dma_phys:      u64, // physical base of the DMA arena
    xhci_dma_len:       u64, // length of the DMA arena in bytes
    console_push_slot:  u32, // u32::MAX = none; else CONSOLE_PUSH cap slot
    send_peers:         [SendPeerEntry; MAX_SEND_PEERS],
}

// ---------------------------------------------------------------------------
// Dynamic send-cap cache — updated by `reacquire_cap` after EndpointDead.
// Safe: each service is a single-threaded process with its own BSS.
// ---------------------------------------------------------------------------

const CACHE_SIZE: usize = 8;

struct CacheEntry {
    slot:     u32,
    name_len: u8,
    name:     [u8; PEER_NAME_BYTES],
}

impl CacheEntry {
    const fn empty() -> Self {
        CacheEntry { slot: u32::MAX, name_len: 0, name: [0u8; PEER_NAME_BYTES] }
    }
}

// SAFETY: single-threaded service process; no concurrent access.
static mut SEND_CAP_CACHE: [CacheEntry; CACHE_SIZE] =
    [const { CacheEntry::empty() }; CACHE_SIZE];

// ---------------------------------------------------------------------------
// TaskStat — returned by ServiceContext::task_stat.
// ---------------------------------------------------------------------------

/// Snapshot of kernel task state for a single scheduler slot.
#[derive(Clone, Copy)]
pub struct TaskStat {
    /// True if the slot holds a live task.
    pub valid:       bool,
    /// Task state: 0=Ready, 1=Running, 2=BlockedOnRecv, 3=BlockedOnSend, 4=Dead.
    pub state:       u8,
    /// Core the task is pinned to.
    pub core:        u8,
    /// Bytes dynamically allocated so far.
    pub mem_used:    u64,
    /// Maximum bytes the task may allocate.
    pub mem_limit:   u64,
    /// Byte length of the name stored in `name`.
    pub name_len:    usize,
    /// Task name bytes (zero-padded to 32 bytes).
    pub name:        [u8; 32],
    /// Current endpoint generation (restart counter).
    pub generation:  u32,
    /// Current inbound IPC queue depth (0–16).
    pub queue_depth: u8,
    /// Timer ticks spent as the running task on its core (monotonic since boot).
    pub run_ticks:   u64,
}

impl TaskStat {
    /// Return the task name as a `&str`.
    pub fn name_str(&self) -> &str {
        let len = self.name_len.min(32);
        core::str::from_utf8(&self.name[..len]).unwrap_or("?")
    }

    /// Return a short human-readable state label.
    pub fn state_str(&self) -> &'static str {
        match self.state {
            0 => "Ready",
            1 => "Running",
            2 => "BlockRecv",
            3 => "BlockSend",
            4 => "Dead",
            _ => "?",
        }
    }
}

/// One held capability, as reported by [`ServiceContext::task_caps`].
#[derive(Clone, Copy, Default)]
pub struct CapInfo {
    /// Resource the cap targets. Stable kernel resources: 1=log_write, 2=spawn,
    /// 3=console_read, 4=console_push, 5=introspect; larger ids are IPC endpoints
    /// or other per-resource grants.
    pub resource_id: u64,
    /// Rights bitfield: READ=1, WRITE=2, SEND=4, RECV=8, GRANT=16, REVOKE=32.
    pub rights: u8,
}

// ---------------------------------------------------------------------------
// AllocError — returned by ServiceContext::alloc_mem.
// ---------------------------------------------------------------------------

/// Error from the AllocMem syscall (6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Allocation would exceed the task's memory limit (§10.3).
    Denied,
    /// Physical memory exhausted or other kernel-side failure.
    Failed,
}

// ---------------------------------------------------------------------------
// ServiceContext.
// ---------------------------------------------------------------------------

/// Passed by the kernel to `service_main`. Non-Copy; one per service instance.
pub struct ServiceContext {
    _private: (),
}

impl ServiceContext {
    #[inline]
    fn ctx() -> &'static ServiceContextData {
        // SAFETY: kernel maps a valid ServiceContextData at SERVICE_CTX_ADDR
        // before SYSRETQ into the service; page is read-only and lifetime-stable.
        unsafe { &*(SERVICE_CTX_ADDR as *const ServiceContextData) }
    }

    /// Look up a named capability from this service's cap table.
    pub fn capability(&self, name: &str) -> Result<CapHandle, CapError> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(CapError::CapNotHeld); }
        match name {
            "log_write" if data.log_write_slot != u32::MAX =>
                Ok(CapHandle(data.log_write_slot)),
            "spawn" if data.spawn_slot != u32::MAX =>
                Ok(CapHandle(data.spawn_slot)),
            "recv" if data.recv_slot != u32::MAX =>
                Ok(CapHandle(data.recv_slot)),
            _ => Err(CapError::CapNotHeld),
        }
    }

    /// Block until a message arrives on this service's primary recv endpoint.
    pub fn recv(&self) -> Message {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { loop {} }
        let slot = data.recv_slot;
        if slot == u32::MAX { loop {} }
        match crate::ipc::recv(CapHandle(slot)) {
            Ok(msg) => msg,
            Err(_)  => loop {},
        }
    }

    /// Block until a message arrives; returns the error instead of looping silently.
    pub fn recv_result(&self) -> Result<Message, crate::ipc::IpcError> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(crate::ipc::IpcError::EndpointDead); }
        let slot = data.recv_slot;
        if slot == u32::MAX { return Err(crate::ipc::IpcError::EndpointDead); }
        crate::ipc::recv(CapHandle(slot))
    }

    /// Send to a named peer declared in `ipc_send`. Blocking.
    pub fn send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        let slot = self.find_send_slot(peer).ok_or(IpcError::CapError(CapError::CapNotHeld))?;
        crate::ipc::send(CapHandle(slot), msg)
    }

    /// Non-blocking send; returns `QueueFull` immediately if the queue is full.
    pub fn try_send(&self, peer: &str, msg: &Message) -> Result<(), IpcError> {
        let slot = self.find_send_slot(peer).ok_or(IpcError::CapError(CapError::CapNotHeld))?;
        crate::ipc::try_send(CapHandle(slot), msg)
    }

    /// Acquire a fresh SEND cap to `peer` via the kernel name registry.
    ///
    /// Called after `try_send` returns `EndpointDead` (§14.2). Updates the
    /// per-service dynamic cap cache so subsequent `try_send` calls use the
    /// new slot without going to the kernel again.
    pub fn reacquire_cap(&self, peer: &str) -> Result<CapHandle, CapError> {
        let bytes = peer.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > PEER_NAME_BYTES { return Err(CapError::CapNotHeld); }

        // SAFETY: syscall(10) = AcquireSendCap; peer bytes are in user space.
        let ret = unsafe {
            raw_syscall(10, bytes.as_ptr() as u64, len as u64, 0)
        };
        if ret < 0 { return Err(CapError::CapNotHeld); }
        let new_slot = ret as u32;

        // Update dynamic cache.
        // SAFETY: single-threaded service; no concurrent cache writes.
        unsafe {
            for entry in SEND_CAP_CACHE.iter_mut() {
                if entry.slot == u32::MAX
                    || (entry.name_len as usize == len
                        && &entry.name[..len] == bytes)
                {
                    entry.slot     = new_slot;
                    entry.name_len = len as u8;
                    entry.name     = [0u8; PEER_NAME_BYTES];
                    entry.name[..len].copy_from_slice(bytes);
                    break;
                }
            }
        }

        Ok(CapHandle(new_slot))
    }

    /// Return the probe mode written by the kernel at spawn (0 for all production services).
    pub fn probe_mode(&self) -> u32 { Self::ctx().probe_mode }

    /// Return the recv cap handle for direct-handle use (e.g. wrong-right test probing).
    pub fn recv_handle(&self) -> Option<crate::capability::CapHandle> {
        let slot = Self::ctx().recv_slot;
        if slot == u32::MAX { None } else { Some(crate::capability::CapHandle(slot)) }
    }

    /// Return the cap handle for the Nth send-peer entry (0-indexed).
    ///
    /// Used by property-test probes (P9) to access multiple cap slots wired to
    /// the same endpoint, verifying all are invalidated on endpoint death (§7.5).
    pub fn send_peer_at(&self, idx: usize) -> Option<crate::capability::CapHandle> {
        let data  = Self::ctx();
        let count = (data.send_peer_count as usize).min(MAX_SEND_PEERS);
        if idx >= count { return None; }
        let slot = data.send_peers[idx].slot;
        if slot == u32::MAX { None } else { Some(crate::capability::CapHandle(slot)) }
    }

    /// Send to a specific cap handle directly, bypassing peer-name lookup.
    ///
    /// Used by the probe service to test kernel cap enforcement (§22 Tests 3B, 9B).
    pub fn try_send_by_handle(
        &self,
        handle: crate::capability::CapHandle,
        msg:    &crate::ipc::Message,
    ) -> Result<(), crate::ipc::IpcError> {
        crate::ipc::try_send(handle, msg)
    }

    /// Send a message to a named peer WITH an embedded capability grant.
    ///
    /// The send-peer cap (which must carry the `GRANT` right) is transferred to
    /// the receiver. On success the calling service loses that cap (§7.6).
    /// Returns `CapNotGrantable` if the cap lacks `GRANT` — the cap is kept.
    pub fn send_with_cap(&self, peer: &str, msg: &crate::ipc::Message) -> Result<(), crate::ipc::IpcError> {
        let slot = self.find_send_slot(peer)
            .ok_or(crate::ipc::IpcError::CapError(crate::capability::CapError::CapNotHeld))?;
        // syscall 11 = SendWithCap
        // arg0 = (grant_slot << 16) | endpoint_slot — same slot holds both SEND and GRANT.
        let packed  = ((slot as u64) << 16) | (slot as u64);
        let payload = msg.payload_bytes();
        // SAFETY: syscall(11) = SendWithCap; packed and payload are from user space.
        let ret = unsafe {
            raw_syscall(11, packed, payload.as_ptr() as u64, payload.len() as u64)
        };
        if ret == 0 { Ok(()) } else { Err(crate::ipc::i64_to_ipc_error(ret)) }
    }

    /// Take the next pending received capability, if any.
    ///
    /// After `recv()` delivers a message containing an embedded cap, the kernel
    /// installs the cap into this task's table and queues the slot index. Call
    /// this once per embedded cap to retrieve each one.
    pub fn take_pending_cap(&self) -> Option<CapHandle> {
        // SAFETY: syscall(12) = TakePendingCap; no args.
        let ret = unsafe { raw_syscall(12, 0, 0, 0) };
        if ret >= 0 { Some(CapHandle(ret as u32)) } else { None }
    }

    /// Acquire a fresh SEND|GRANT cap to `peer` via the kernel name registry.
    ///
    /// Used by property-test probes that need to transfer capabilities (P3).
    /// Returns `None` if the service is not registered or the cap table is full.
    pub fn acquire_send_grant_cap(&self, peer: &str) -> Option<CapHandle> {
        let bytes = peer.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > PEER_NAME_BYTES { return None; }
        // SAFETY: syscall(10) = AcquireSendCap; arg2=1 requests SEND|GRANT.
        let ret = unsafe { raw_syscall(10, bytes.as_ptr() as u64, len as u64, 1) };
        if ret < 0 { None } else { Some(CapHandle(ret as u32)) }
    }

    /// Acquire a fresh SEND cap to `peer` via the kernel name registry.
    ///
    /// Returns the new cap handle, or `None` if the name is not registered.
    pub fn acquire_send_cap(&self, peer: &str) -> Option<CapHandle> {
        let bytes = peer.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > PEER_NAME_BYTES { return None; }
        // SAFETY: syscall(10) = AcquireSendCap; arg2=0 = SEND only.
        let ret = unsafe { raw_syscall(10, bytes.as_ptr() as u64, len as u64, 0) };
        if ret < 0 { None } else { Some(CapHandle(ret as u32)) }
    }

    /// Query the current generation of the named endpoint.
    ///
    /// Returns the generation counter as a u64, or 0 if the name is not
    /// registered. Used by property tests P2 and P8 (§7.5, §14.2).
    pub fn inspect_endpoint_generation(&self, name: &str) -> u64 {
        let bytes = name.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > PEER_NAME_BYTES { return 0; }
        // SAFETY: syscall(13) = InspectKernel; query_id=2 = endpoint generation by name.
        let ret = unsafe {
            raw_syscall(13, 2, bytes.as_ptr() as u64, len as u64)
        };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Return the bytes dynamically allocated by this task so far.
    ///
    /// Wraps InspectKernel query 0. Used by property test P4 (§10.3).
    pub fn inspect_kernel_alloc_bytes(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=0 = task alloc bytes.
        let ret = unsafe { raw_syscall(13, 0, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Return the count of live endpoints in the kernel routing table.
    ///
    /// Wraps InspectKernel query 1. Used by property test P5 (§8.3).
    pub fn inspect_kernel_endpoint_count(&self) -> u32 {
        // SAFETY: syscall(13) = InspectKernel; query_id=1 = live endpoint count.
        let ret = unsafe { raw_syscall(13, 1, 0, 0) };
        if ret < 0 { 0 } else { ret as u32 }
    }

    /// Return the number of free physical frames.
    ///
    /// Wraps InspectKernel query 4.
    pub fn inspect_kernel_free_frames(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=4 = free frame count.
        let ret = unsafe { raw_syscall(13, 4, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Return the total usable physical frames at boot time.
    ///
    /// Wraps InspectKernel query 5.
    pub fn inspect_kernel_total_frames(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=5 = total frame count.
        let ret = unsafe { raw_syscall(13, 5, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Timer ticks the given core spent running a user task (not idle) since boot.
    ///
    /// Wraps InspectKernel query 6 (arg1 = core index).
    pub fn inspect_core_active_ticks(&self, core: u32) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=6, arg1=core.
        let ret = unsafe { raw_syscall(13, 6, core as u64, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Total timer ticks seen on the given core since boot.
    ///
    /// Wraps InspectKernel query 7 (arg1 = core index).
    pub fn inspect_core_total_ticks(&self, core: u32) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=7, arg1=core.
        let ret = unsafe { raw_syscall(13, 7, core as u64, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Number of CPU cores ready since boot.
    ///
    /// Wraps InspectKernel query 8.
    pub fn inspect_core_count(&self) -> u32 {
        // SAFETY: syscall(13) = InspectKernel; query_id=8.
        let ret = unsafe { raw_syscall(13, 8, 0, 0) };
        if ret <= 0 { 1 } else { ret as u32 }
    }

    /// Framebuffer console geometry as `(rows, cols)` text cells, or `(0, 0)` if
    /// there is no framebuffer. The console service uses this to lay out its
    /// terminal (pin the input line to the bottom row).
    ///
    /// Wraps InspectKernel query 9 (ambient — screen geometry is task-neutral).
    pub fn console_dims(&self) -> (u16, u16) {
        // SAFETY: syscall(13) = InspectKernel; query_id=9 = packed (rows<<16)|cols.
        let ret = unsafe { raw_syscall(13, 9, 0, 0) };
        if ret <= 0 {
            (0, 0)
        } else {
            let packed = ret as u64;
            (((packed >> 16) & 0xFFFF) as u16, (packed & 0xFFFF) as u16)
        }
    }

    /// Whether the input driver has reported setup complete (syscall 13, query 10).
    /// The deterministic end-of-boot signal: the shell watches it to auto-clear the
    /// boot screen the moment the keyboard subsystem is up. Ambient.
    pub fn input_ready(&self) -> bool {
        // SAFETY: syscall(13) = InspectKernel; query_id=10 = input-ready flag.
        unsafe { raw_syscall(13, 10, 0, 0) > 0 }
    }

    /// Report that input-subsystem setup is complete (syscall 27). Called by the
    /// USB keyboard driver (xHCI) in every terminal path once it has finished — the
    /// end-of-boot signal. Requires the CONSOLE_PUSH cap (the input driver only).
    pub fn signal_input_ready(&self) {
        let slot = Self::ctx().console_push_slot;
        if slot == u32::MAX { return; }
        // SAFETY: syscall(27) = SignalInputReady; slot is the kernel-written cap index.
        let _ = unsafe { raw_syscall(27, slot as u64, 0, 0) };
    }

    /// Read the hardware TSC (Time Stamp Counter) via the kernel.
    ///
    /// Returns RDTSC cycle count. Useful for measuring kernel operation latencies
    /// in benchmark probes (§22 Perf B1–B10). Not comparable across hosts.
    pub fn read_tsc(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=3 = read TSC.
        let ret = unsafe { raw_syscall(13, 3, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Query the kernel task stat for scheduler slot `slot` (syscall 16).
    ///
    /// Returns a best-effort snapshot. If `slot` is out of range or the task
    /// is dead, `valid` will be false.
    pub fn task_stat(&self, slot: u32) -> TaskStat {
        let mut buf = [0u8; 72];
        // SAFETY: syscall(16) = TaskStat; buf is a local array on the user stack.
        let ret = unsafe {
            raw_syscall(16, slot as u64, buf.as_mut_ptr() as u64, 72)
        };
        if ret != 0 {
            return TaskStat {
                valid: false, state: 0, core: 0,
                mem_used: 0, mem_limit: 0, name_len: 0, name: [0u8; 32],
                generation: 0, queue_depth: 0, run_ticks: 0,
            };
        }
        let valid       = buf[0] != 0;
        let state       = buf[1];
        let core        = buf[2];
        let name_len    = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let mem_used    = u64::from_le_bytes([buf[8],  buf[9],  buf[10], buf[11],
                                              buf[12], buf[13], buf[14], buf[15]]);
        let mem_limit   = u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19],
                                              buf[20], buf[21], buf[22], buf[23]]);
        let mut name = [0u8; 32];
        let copy_len = name_len.min(32);
        name[..copy_len].copy_from_slice(&buf[24..24 + copy_len]);
        let generation  = u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]);
        let queue_depth = buf[60];
        let run_ticks   = u64::from_le_bytes([buf[64], buf[65], buf[66], buf[67],
                                              buf[68], buf[69], buf[70], buf[71]]);
        TaskStat { valid, state, core, mem_used, mem_limit, name_len: copy_len, name,
                   generation, queue_depth, run_ticks }
    }

    /// List the capabilities held by the task in `slot`, into `out`. Returns the
    /// number of entries written (capped at `out.len()` and 64). Requires the
    /// INTROSPECT cap. Best-effort snapshot — see [`task_stat`](Self::task_stat).
    pub fn task_caps(&self, slot: u32, out: &mut [CapInfo]) -> usize {
        const ENTRY: usize = 16;
        const MAX: usize = 64;
        let want = out.len().min(MAX);
        if want == 0 { return 0; }
        let mut buf = [0u8; ENTRY * MAX];
        // SAFETY: syscall(28) = TaskCaps; buf is a local array on the user stack.
        let ret = unsafe {
            raw_syscall(28, slot as u64, buf.as_mut_ptr() as u64, (want * ENTRY) as u64)
        };
        if ret <= 0 { return 0; }
        let count = (ret as usize).min(want);
        for i in 0..count {
            let o = i * ENTRY;
            out[i].resource_id = u64::from_le_bytes([
                buf[o], buf[o + 1], buf[o + 2], buf[o + 3],
                buf[o + 4], buf[o + 5], buf[o + 6], buf[o + 7],
            ]);
            out[i].rights = buf[o + 8];
        }
        count
    }

    /// Send a message via an explicit cap handle (blocking).
    ///
    /// Used by benchmark probes that dynamically acquire send caps rather than
    /// using named peer slots, avoiding repeated name-lookup overhead.
    pub fn send_by_handle(&self, handle: CapHandle, msg: &Message) -> Result<(), IpcError> {
        crate::ipc::send(handle, msg)
    }

    /// Query the rights bitfield of the cap at `handle`.
    ///
    /// Returns the rights byte as a u64, or `None` if the slot is empty.
    /// Used by property test P3 to verify rights do not widen on transfer (§7.3).
    pub fn query_cap_rights(&self, handle: CapHandle) -> Option<u64> {
        // SAFETY: syscall(14) = QueryCapRights; slot is a cap table index.
        let ret = unsafe { raw_syscall(14, handle.0 as u64, 0, 0) };
        if ret < 0 { None } else { Some(ret as u64) }
    }

    /// Remove the cap at `handle` from this task's cap table.
    ///
    /// Idempotent: removing an already-empty slot is a no-op.
    pub fn remove_cap(&self, handle: CapHandle) {
        // SAFETY: syscall(15) = RemoveCap; slot is a cap table index.
        unsafe { raw_syscall(15, handle.0 as u64, 0, 0); }
    }

    /// Send a message to `endpoint` with cap `grant` embedded.
    ///
    /// Unlike `send_with_cap` (which looks up a peer by name), this takes
    /// explicit handles — used by P3 where the endpoint and grant slot are
    /// the same (self-referential cap transfer).  On success the grant cap is
    /// removed from the caller's table (§7.6).
    pub fn send_with_cap_by_handle(
        &self,
        endpoint: CapHandle,
        grant:    CapHandle,
        msg:      &crate::ipc::Message,
    ) -> Result<(), crate::ipc::IpcError> {
        let packed  = ((grant.0 as u64) << 16) | (endpoint.0 as u64);
        let payload = msg.payload_bytes();
        // SAFETY: syscall(11) = SendWithCap; packed and payload are from user space.
        let ret = unsafe {
            raw_syscall(11, packed, payload.as_ptr() as u64, payload.len() as u64)
        };
        if ret == 0 { Ok(()) } else { Err(crate::ipc::i64_to_ipc_error(ret)) }
    }

    /// Inject one byte into the console input ring (syscall 20). Only effective
    /// for an input-driver service holding a CONSOLE_PUSH cap (the USB keyboard
    /// driver, §12); the byte reaches the shell exactly like a serial keystroke.
    /// No-op for services without the cap.
    pub fn console_push(&self, byte: u8) {
        let slot = Self::ctx().console_push_slot;
        if slot == u32::MAX {
            return;
        }
        // SAFETY: syscall(20) = ConsolePush; slot is the kernel-written cap index.
        let _ = unsafe { raw_syscall(20, slot as u64, byte as u64, 0) };
    }

    /// Block until one byte is available on COM1 console input (syscall 17).
    ///
    /// Returns the byte value. Only usable by services that declared
    /// `has_console_read` in their kernel config (currently: shell only).
    pub fn console_read(&self) -> u8 {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { loop {} }
        let slot = data.console_read_slot;
        if slot == u32::MAX { loop {} }
        // SAFETY: syscall(17) = ConsoleRead; slot is kernel-written cap index.
        let ret = unsafe { raw_syscall(17, slot as u64, 0, 0) };
        if ret >= 0 { ret as u8 } else { 0 }
    }

    /// Non-blocking console read (syscall 24). Returns `Some(byte)` if a keystroke
    /// is waiting, `None` if the ring is empty. A foreground full-screen app polls
    /// this for `q`-to-quit between repaints instead of blocking in `console_read`.
    /// Requires the CONSOLE_READ cap (`has_console_read` in the kernel config).
    pub fn try_console_read(&self) -> Option<u8> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return None; }
        let slot = data.console_read_slot;
        if slot == u32::MAX { return None; }
        // SAFETY: syscall(24) = TryConsoleRead; slot is kernel-written cap index.
        // Returns 0..=255 (byte), 256 (empty), or negative (cap error).
        let ret = unsafe { raw_syscall(24, slot as u64, 0, 0) };
        if (0..=255).contains(&ret) { Some(ret as u8) } else { None }
    }

    /// Enable (`true`) or disable (`false`) console keystroke echo (syscall 25).
    /// A foreground full-screen app disables echo while it owns the screen — so
    /// its raw key polls do not smear its frame — and re-enables it on exit.
    /// Requires the CONSOLE_READ cap.
    pub fn console_echo(&self, on: bool) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.console_read_slot;
        if slot == u32::MAX { return; }
        // SAFETY: syscall(25) = ConsoleEcho; slot is kernel-written cap index.
        let _ = unsafe { raw_syscall(25, slot as u64, on as u64, 0) };
    }

    /// End boot-log mirroring to the framebuffer and clear the TV (syscall 26).
    /// The shell calls this once, on the first keystroke, so the user sees the
    /// boot sequence on the display and then gets a clean interactive console.
    /// Requires the CONSOLE_READ cap.
    pub fn console_boot_complete(&self) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.console_read_slot;
        if slot == u32::MAX { return; }
        // SAFETY: syscall(26) = ConsoleBootComplete; slot is kernel-written cap index.
        let _ = unsafe { raw_syscall(26, slot as u64, 0, 0) };
    }

    /// Return the core this service was spawned on.
    pub fn core_id(&self) -> u32 { Self::ctx().core_id }

    /// Safe MMIO handle for this service's xHCI controller, if one was granted
    /// (§12). The kernel mapped the controller's BAR into this driver's address
    /// space; the returned [`crate::Mmio`] reads/writes the uncached device
    /// registers directly. `None` for non-driver services.
    pub fn xhci_mmio(&self) -> Option<crate::mmio::Mmio> {
        let va = Self::ctx().xhci_mmio_va;
        if va == 0 {
            None
        } else {
            Some(crate::mmio::Mmio::new(va as *mut u8))
        }
    }

    /// Safe MMIO handle for this service's EHCI controller, if one was granted
    /// (§12). Reads the same kernel-mapped controller-BAR window as
    /// [`xhci_mmio`](Self::xhci_mmio) — a driver service holds exactly one
    /// controller, so the field is shared and unambiguous. `None` for non-drivers.
    pub fn ehci_mmio(&self) -> Option<crate::mmio::Mmio> {
        let va = Self::ctx().xhci_mmio_va;
        if va == 0 {
            None
        } else {
            Some(crate::mmio::Mmio::new(va as *mut u8))
        }
    }

    /// Safe handle to this service's DMA arena, if one was granted (§12). The
    /// kernel mapped a physically-contiguous region into this driver; the
    /// returned [`crate::Dma`] gives the CPU view (read/write) and the physical
    /// base to program into the controller. `None` for non-driver services.
    pub fn dma_region(&self) -> Option<crate::dma::Dma> {
        let d = Self::ctx();
        if d.xhci_dma_va == 0 {
            None
        } else {
            Some(crate::dma::Dma::new(
                d.xhci_dma_va as *mut u8,
                d.xhci_dma_phys,
                d.xhci_dma_len as usize,
            ))
        }
    }

    /// Allocate `size` bytes of read/write memory within this task's budget.
    ///
    /// Returns the virtual address of the mapping on success, or `AllocError`
    /// if the allocation would exceed the contract memory limit (AllocDenied)
    /// or physical memory is exhausted.
    pub fn alloc_mem(&self, size: usize) -> Result<u64, AllocError> {
        // SAFETY: syscall(6) = AllocMem; no user pointers passed.
        let ret = unsafe { raw_syscall(6, size as u64, 0, 0) };
        if ret >= 0 {
            Ok(ret as u64)
        } else if ret == -11 {
            Err(AllocError::Denied)
        } else {
            Err(AllocError::Failed)
        }
    }

    /// Trigger a kernel panic with `reason` as the message.
    ///
    /// Called by TCB services (init) when a required service fails to spawn
    /// (§6.2).  Does not return.
    pub fn abort(&self, reason: &str) -> ! {
        let bytes = reason.as_bytes();
        // SAFETY: syscall(9) = Abort; bytes is a valid slice in user space.
        unsafe { raw_syscall(9, bytes.as_ptr() as u64, bytes.len() as u64, 0) };
        // DIAGNOSTIC: ud2 fires if syscall returns (SYSCALL no-op on this hw).
        // "EXCEPTION: #UD" at RIP after syscall → SYSCALL fell through.
        // "KERNEL PANIC" → SYSCALL dispatched correctly; ud2 never reached.
        // SAFETY: noreturn; ud2 is intentional — catches SYSCALL fallthrough.
        unsafe { core::arch::asm!("ud2", options(noreturn)) }
    }

    /// Trigger a hardware reset via the kernel reboot syscall (18). Does not return.
    ///
    /// Flushes "rebooting..." to serial before the reset so the operator sees
    /// confirmation in PuTTY before the line goes silent.
    pub fn reboot(&self) -> ! {
        // SAFETY: syscall(18) = Reboot; no arguments.
        unsafe { raw_syscall(18, 0, 0, 0) };
        loop {} // unreachable
    }

    /// Advisory yield (§9.3).
    pub fn yield_cpu(&self) {
        // SAFETY: syscall(4) = Yield; always valid from ring-3.
        unsafe { raw_syscall(4, 0, 0, 0); }
    }

    /// Park this task forever: block with no waker. For idle services that have
    /// no further work (init, supervisor) — far better than `loop { yield_cpu() }`,
    /// which keeps the core busy and prevents it from halting (so it never runs
    /// cool). Nothing wakes a parked task in v1; the loop re-parks defensively.
    pub fn park(&self) -> ! {
        loop {
            // SAFETY: syscall(21) = Park; blocks this task indefinitely.
            unsafe { raw_syscall(21, 0, 0, 0); }
        }
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

    /// Write a string to the console WITHOUT a trailing newline (syscall 22,
    /// requires log_write cap). For inline output such as the shell prompt, where
    /// `log`'s newline would push the user's typed echo to the next line.
    pub fn print(&self, msg: &str) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.log_write_slot;
        if slot == u32::MAX { return; }

        let bytes = msg.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > 256 { return; }

        // SAFETY: syscall(22) = Print; bytes is a valid slice within user space.
        unsafe {
            raw_syscall(22, slot as u64, bytes.as_ptr() as u64, len as u64);
        }
    }

    /// Log a formatted message.
    pub fn log_fmt(&self, args: core::fmt::Arguments) {
        let mut buf    = [0u8; 256];
        let mut cursor = 0usize;
        let _ = core::fmt::write(
            &mut StackWriter { buf: &mut buf, pos: &mut cursor },
            args,
        );
        if cursor > 0 {
            self.log(core::str::from_utf8(&buf[..cursor]).unwrap_or("(fmt error)"));
        }
    }

    /// Write a string to the **interactive console** (serial + framebuffer),
    /// WITHOUT a trailing newline (syscall 23, requires log_write cap in Stage 1).
    /// This is the user-facing path: the shell prompt, command results, and
    /// `observe` frames. Unlike `log`/`print` (now serial-only), this also reaches
    /// the framebuffer/TV — the interactive surface (see docs/console-service.md).
    pub fn console_write(&self, msg: &str) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.log_write_slot;
        if slot == u32::MAX { return; }

        let bytes = msg.as_bytes();
        let len   = bytes.len();
        if len == 0 || len > 256 { return; }

        // SAFETY: syscall(23) = ConsoleWrite; bytes is a valid slice within user space.
        unsafe {
            raw_syscall(23, slot as u64, bytes.as_ptr() as u64, len as u64);
        }
    }

    /// Write a string to the interactive console followed by a newline.
    pub fn console_writeln(&self, msg: &str) {
        self.console_write(msg);
        self.console_write("\n");
    }

    /// Write a formatted message to the interactive console, followed by a newline.
    pub fn console_writeln_fmt(&self, args: core::fmt::Arguments) {
        let mut buf    = [0u8; 256];
        let mut cursor = 0usize;
        let _ = core::fmt::write(
            &mut StackWriter { buf: &mut buf, pos: &mut cursor },
            args,
        );
        if cursor > 0 {
            self.console_write(core::str::from_utf8(&buf[..cursor]).unwrap_or("(fmt error)"));
        }
        self.console_write("\n");
    }

    /// Write one console line. When `clear_eol` is true the line ends with
    /// `ESC[K` (erase to end of line) before the newline — so a full-screen app
    /// repainting in place (cursor homed each frame) overwrites a previous,
    /// longer line without leaving stale characters, and without a full-screen
    /// clear (no flicker). When false, behaves exactly like `console_writeln`.
    pub fn console_line(&self, clear_eol: bool, msg: &str) {
        self.console_write(msg);
        self.console_write(if clear_eol { "\x1b[K\n" } else { "\n" });
    }

    /// Formatted variant of [`console_line`].
    pub fn console_line_fmt(&self, clear_eol: bool, args: core::fmt::Arguments) {
        let mut buf    = [0u8; 256];
        let mut cursor = 0usize;
        let _ = core::fmt::write(
            &mut StackWriter { buf: &mut buf, pos: &mut cursor },
            args,
        );
        if cursor > 0 {
            self.console_write(core::str::from_utf8(&buf[..cursor]).unwrap_or("(fmt error)"));
        }
        self.console_write(if clear_eol { "\x1b[K\n" } else { "\n" });
    }

    /// Spawn a service by name on the kernel-selected core.
    pub fn spawn(&self, name: &str) -> Result<(), crate::Error> {
        self.spawn_on(name, 0xFFFF)
    }

    /// Spawn a service by name on `core` (0xFFFF = kernel round-robin).
    pub fn spawn_on(&self, name: &str, core: u32) -> Result<(), crate::Error> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC {
            return Err(crate::Error::InvalidArgument);
        }
        let slot = data.spawn_slot;
        if slot == u32::MAX {
            return Err(crate::Error::Cap(CapError::CapNotHeld));
        }
        let bytes = name.as_bytes();
        let packed = ((core as u64 & 0xFFFF) << 16) | (slot as u64 & 0xFFFF);
        // SAFETY: syscall(7) = Spawn; slot is from kernel-written page; bytes is valid.
        let ret = unsafe {
            raw_syscall(7, packed, bytes.as_ptr() as u64, bytes.len() as u64)
        };
        if ret == 0 { Ok(()) } else { Err(crate::Error::InvalidArgument) }
    }

    /// Spawn `producer` and delegate it a SEND cap to `sink`'s endpoint
    /// (`producer | sink`). `sink` must already be spawned. Requires the spawn
    /// capability — held only by the shell/supervisor.
    pub fn spawn_pipe(&self, producer: &str, sink: &str) -> Result<(), crate::Error> {
        self.spawn_pipe_on(producer, sink, 0xFFFF)
    }

    pub fn spawn_pipe_on(&self, producer: &str, sink: &str, core: u32) -> Result<(), crate::Error> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC {
            return Err(crate::Error::InvalidArgument);
        }
        let slot = data.spawn_slot;
        if slot == u32::MAX {
            return Err(crate::Error::Cap(CapError::CapNotHeld));
        }
        // Build "producer sink" in a fixed stack buffer (no_std, no alloc).
        let (pb, sb) = (producer.as_bytes(), sink.as_bytes());
        let mut buf = [0u8; 130];
        if pb.len() + 1 + sb.len() > buf.len() {
            return Err(crate::Error::InvalidArgument);
        }
        let mut n = 0;
        buf[n..n + pb.len()].copy_from_slice(pb); n += pb.len();
        buf[n] = b' '; n += 1;
        buf[n..n + sb.len()].copy_from_slice(sb); n += sb.len();
        let packed = ((core as u64 & 0xFFFF) << 16) | (slot as u64 & 0xFFFF);
        // SAFETY: syscall(19) = SpawnPipe; slot from kernel-written page; buf is valid.
        let ret = unsafe { raw_syscall(19, packed, buf.as_ptr() as u64, n as u64) };
        if ret == 0 { Ok(()) } else { Err(crate::Error::InvalidArgument) }
    }

    /// Kill a named service (supervisor only in production; unrestricted in Phase 5).
    pub fn kill(&self, name: &str) -> Result<(), crate::Error> {
        let bytes = name.as_bytes();
        // SAFETY: syscall(8) = Kill; bytes is a valid slice within user space.
        let ret = unsafe {
            raw_syscall(8, bytes.as_ptr() as u64, bytes.len() as u64, 0)
        };
        if ret == 0 { Ok(()) } else { Err(crate::Error::InvalidArgument) }
    }

    /// Kill then respawn a service with optional core override (§14.4).
    pub fn restart(&self, name: &str, core_override: Option<u32>) -> Result<(), crate::Error> {
        let _ = self.kill(name); // ignore error if service is already dead
        let core = core_override.unwrap_or(0xFFFF);
        self.spawn_on(name, core)
    }

    /// Drain the kernel ring buffer. Called by logger at startup (§11.4).
    ///
    /// Phase 5: reads the ring buffer via kprintln output (already mirrored to
    /// serial); full drain syscall deferred to Phase 6.
    pub fn drain_kernel_ring_buffer(&self) {
        // Ring buffer is already mirrored to serial at all times (§11.4).
        // Nothing additional needed until the logger has a dedicated drain syscall.
    }

    /// Receive a log message on this service's recv endpoint.
    pub fn recv_log_message(&self) -> Message {
        self.recv()
    }

    // ---------------------------------------------------------------------------
    // Private helpers.
    // ---------------------------------------------------------------------------

    /// Find the cap slot for a named send peer.
    ///
    /// Search order: dynamic cache (post-restart reacquisitions), then the
    /// kernel-written ServiceContextData send_peers array.
    fn find_send_slot(&self, peer: &str) -> Option<u32> {
        let bytes = peer.as_bytes();
        let len   = bytes.len();

        // 1. Dynamic cache (updated after EndpointDead + reacquire).
        // SAFETY: single-threaded service process.
        unsafe {
            for entry in SEND_CAP_CACHE.iter() {
                if entry.slot != u32::MAX
                    && entry.name_len as usize == len
                    && &entry.name[..len] == bytes
                {
                    return Some(entry.slot);
                }
            }
        }

        // 2. ServiceContextData send_peers (wired at spawn).
        let data  = Self::ctx();
        let count = (data.send_peer_count as usize).min(MAX_SEND_PEERS);
        for i in 0..count {
            let entry = &data.send_peers[i];
            if entry.slot == u32::MAX { continue; }
            let nlen = entry.name_len as usize;
            if nlen == len && &entry.name[..len] == bytes {
                return Some(entry.slot);
            }
        }

        None
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
        let n     = bytes.len().min(space);
        self.buf[*self.pos .. *self.pos + n].copy_from_slice(&bytes[..n]);
        *self.pos += n;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Placeholder types retained for compatibility.
// ---------------------------------------------------------------------------

pub struct ServiceDescriptor;
impl ServiceDescriptor {
    pub fn name(&self) -> &str { todo!() }
}

pub struct BootManifest;
impl BootManifest {
    pub fn services(&self) -> &[ServiceDescriptor] { todo!() }
}
