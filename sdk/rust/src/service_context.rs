// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! ServiceContext — entry-point type handed to every service's `service_main`.
//!
//! Provides safe, named access to the capabilities the service declared in its
//! contract. Capability names match the contract field names exactly.
//! Requesting a cap not in the contract returns `Err(CapNotHeld)`.

use crate::capability::{CapError, CapHandle};
use crate::ipc::{IpcError, Message};
use crate::syscall::raw_syscall;

/// Wall-clock date/time read from the hardware RTC, fully decoded (binary,
/// 24-hour). See [`ServiceContext::datetime`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Datetime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Datetime {
    /// Days since the epoch (1970-01-01), proleptic Gregorian and leap-year aware
    /// (Howard Hinnant's `days_from_civil`). The basis for both `weekday` and
    /// `epoch_secs`.
    fn days_since_epoch(&self) -> i64 {
        let mut y = self.year as i64;
        let m = self.month as i64;
        let d = self.day as i64;
        y -= (m <= 2) as i64;
        let era = (if y >= 0 { y } else { y - 399 }) / 400;
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    }

    /// Day of week, 0 = Sunday .. 6 = Saturday (computed, not the RTC's
    /// often-unreliable weekday register).
    pub fn weekday(&self) -> u8 {
        (self.days_since_epoch() + 4).rem_euclid(7) as u8
    }

    /// Seconds since the epoch (1970-01-01). Assumes the RTC reads UTC; if the
    /// hardware clock is set to local time the value is offset by the timezone
    /// (v1 has no timezone database).
    pub fn epoch_secs(&self) -> i64 {
        self.days_since_epoch() * 86_400
            + self.hour as i64 * 3_600
            + self.minute as i64 * 60
            + self.second as i64
    }
}

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
    self_grant_slot:    u32, // u32::MAX = none; else SEND|GRANT cap to own endpoint (H11)
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

// ---------------------------------------------------------------------------
// Registry wire protocol (H11). Shared by the `registry` service and the
// `register`/`registry_lookup` client helpers above.
//
//   Register request : payload [REGISTER, name_len, name…] + embedded SEND|GRANT
//                       cap to the registrant's endpoint.
//   Lookup request   : payload [LOOKUP, name_len, name…]   + embedded SEND cap to
//                       the client's reply endpoint.
//   Lookup reply     : payload [FOUND]      + embedded SEND cap to the named service,
//                    or payload [NOT_FOUND]  (no cap).
// ---------------------------------------------------------------------------

/// Registry op: announce `name → my endpoint`.
pub const REGISTRY_OP_REGISTER: u8 = 0;
/// Registry op: resolve `name`, reply with a SEND cap.
pub const REGISTRY_OP_LOOKUP:   u8 = 1;
/// Lookup reply: name found; a cap is embedded.
pub const REGISTRY_FOUND:       u8 = 0;
/// Lookup reply: name not registered; no cap.
pub const REGISTRY_NOT_FOUND:   u8 = 1;
/// Max registry name length (matches the kernel name registry).
pub const REGISTRY_NAME_MAX:    usize = 32;

/// Point the dynamic send-cap cache entry for `name` at `new_slot`, so the next
/// `find_send_slot(name)` resolves to the freshly-acquired cap. Shared by the
/// registry reacquire path; mirrors the inline update in `reacquire_cap`.
fn cache_send_slot(name: &str, new_slot: u32) {
    let bytes = name.as_bytes();
    let len   = bytes.len().min(PEER_NAME_BYTES);
    // SAFETY: single-threaded service process; no concurrent cache writers.
    // addr_of_mut! avoids materialising a &mut to the `static mut` directly
    // (silences the static_mut_refs lint).
    unsafe {
        for entry in (*core::ptr::addr_of_mut!(SEND_CAP_CACHE)).iter_mut() {
            if entry.slot == u32::MAX
                || (entry.name_len as usize == len && &entry.name[..len] == &bytes[..len])
            {
                entry.slot     = new_slot;
                entry.name_len = len as u8;
                entry.name     = [0u8; PEER_NAME_BYTES];
                entry.name[..len].copy_from_slice(&bytes[..len]);
                break;
            }
        }
    }
}

/// Encode a register/lookup request payload: `[op, name_len, name…]`.
fn registry_request(op: u8, name: &str) -> Message {
    let nb  = name.as_bytes();
    let len = nb.len().min(REGISTRY_NAME_MAX);
    let mut buf = [0u8; 2 + REGISTRY_NAME_MAX];
    buf[0] = op;
    buf[1] = len as u8;
    buf[2..2 + len].copy_from_slice(&nb[..len]);
    Message::from_bytes(&buf[..2 + len])
}

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

    /// Non-blocking receive on this service's primary recv endpoint: `Some(msg)` if a
    /// message was waiting, `None` if the queue is empty. A busy-polling driver uses this
    /// to drain interrupt events (§12) each loop iteration without blocking.
    pub fn try_recv(&self) -> Option<Message> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return None; }
        let slot = data.recv_slot;
        if slot == u32::MAX { return None; }
        crate::ipc::try_recv(CapHandle(slot)).ok().flatten()
    }

    /// Block on this service's recv endpoint until a message arrives or `timeout_cycles`
    /// (TSC cycles) elapse: `Some(msg)` = message, `None` = timed out. `timeout_cycles == 0`
    /// blocks forever. A driver uses this to idle on its hardware interrupt while still
    /// waking on a timer for auto-repeat (§12 timed-wait).
    pub fn recv_timeout(&self, timeout_cycles: u64) -> Option<Message> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return None; }
        let slot = data.recv_slot;
        if slot == u32::MAX { return None; }
        crate::ipc::recv_timeout(CapHandle(slot), timeout_cycles).ok().flatten()
    }

    /// Re-open the kernel's IOAPIC gate for a level-triggered IRQ `vector` after this driver
    /// has cleared its device's interrupt source (§12). The kernel masks a level INTx while the
    /// driver handles it (so it can't storm); call this to let it fire again. Only the driver
    /// registered for `vector` (via its `hw_interrupt` route) may unmask it. No-op for MSI.
    pub fn irq_unmask(&self, vector: u8) {
        // SAFETY: syscall(36) = IrqUnmask; gated kernel-side by the route registration.
        let _ = unsafe { raw_syscall(36, vector as u64, 0, 0) };
    }

    /// Block this task for roughly `cycles` TSC cycles, then return (syscall 37). A real sleep:
    /// the core can halt while parked, so a poll/wait loop does not busy-`yield` (which pegs the
    /// core at ~100% and makes every task on it read as fully busy in `observe`). Like `yield`,
    /// needs no capability. Granularity is one scheduler quantum (~10 ms). Use for UI repaint
    /// pacing and "wait for child" loops — not for precise timing.
    pub fn sleep(&self, cycles: u64) {
        // SAFETY: syscall(37) = Sleep; sleeping your own task is unprivileged (like yield).
        let _ = unsafe { raw_syscall(37, cycles, 0, 0) };
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
        // SAFETY: single-threaded service; no concurrent cache writes. addr_of_mut!
        // avoids a direct &mut to the static (static_mut_refs lint).
        unsafe {
            for entry in (*core::ptr::addr_of_mut!(SEND_CAP_CACHE)).iter_mut() {
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

    /// Derive a duplicate of a capability this service holds **with the GRANT right**
    /// into a fresh slot (syscall 29 = `DeriveCap`). The copy carries the same
    /// resource, generation, and (non-widened) rights.
    ///
    /// Used by the `registry` to serve many `lookup`s from the one endpoint cap it
    /// holds per name: it derives a copy per client and grants that copy away (via
    /// `send_with_cap_by_handle`) while keeping the original. Returns `None` if the
    /// cap lacks GRANT, is stale, or the cap table is full.
    pub fn derive_cap(&self, held: CapHandle) -> Option<CapHandle> {
        // SAFETY: syscall(29) = DeriveCap; `held.0` is a slot index into this task's
        // own cap table. The kernel validates GRANT + generation before duplicating.
        let ret = unsafe { raw_syscall(29, held.0 as u64, 0, 0) };
        if ret < 0 { None } else { Some(CapHandle(ret as u32)) }
    }

    /// Return the probe mode written by the kernel at spawn (0 for all production services).
    pub fn probe_mode(&self) -> u32 { Self::ctx().probe_mode }

    /// Return the recv cap handle for direct-handle use (e.g. wrong-right test probing).
    pub fn recv_handle(&self) -> Option<crate::capability::CapHandle> {
        let slot = Self::ctx().recv_slot;
        if slot == u32::MAX { None } else { Some(crate::capability::CapHandle(slot)) }
    }

    /// Announce this service to the `registry` under `name` (H11). Derives a
    /// `SEND|GRANT` copy of our self-endpoint cap and grants it to the registry, which
    /// records `name → cap` and later hands SEND copies to clients that look the name
    /// up. Idempotent — call again to re-register (e.g. after a registry restart).
    /// Requires a `registry` send-peer and a self endpoint. Returns `true` on send.
    pub fn register(&self, name: &str) -> bool {
        let reg = match self.find_send_slot("registry") {
            Some(s) => CapHandle(s),
            None    => return false,
        };
        let self_grant = match self.self_grant_handle() {
            Some(h) => h,
            None    => return false,
        };
        let to_grant = match self.derive_cap(self_grant) {
            Some(h) => h,
            None    => return false,
        };
        let msg = registry_request(REGISTRY_OP_REGISTER, name);
        self.send_with_cap_by_handle(reg, to_grant, &msg).is_ok()
    }

    /// Look `name` up via the registry; return a fresh SEND cap to it, or `None` if
    /// the name is not registered (H11). **Blocks** on this service's own endpoint
    /// for the registry's reply, so the service must have an endpoint and not expect
    /// other traffic to race the reply. Replaces the kernel `reacquire_cap` path once
    /// services are cut over (Phase 5).
    pub fn registry_lookup(&self, name: &str) -> Option<CapHandle> {
        let self_grant = self.self_grant_handle()?;
        // A SEND|GRANT copy of our own endpoint cap — the registry replies here.
        let reply_cap = self.derive_cap(self_grant)?;
        let req = registry_request(REGISTRY_OP_LOOKUP, name);

        // Send the lookup to the registry. Our cached `registry` cap can be stale if the
        // registry itself has restarted (H11) — and the registry is the one name a client
        // cannot resolve *through* the registry (you can't look the namer up in the namer).
        // So on a failed send, reacquire a fresh `registry` SEND cap from the **kernel name
        // table** (syscall 10 — the bootstrap exception) and retry once. The send only fails
        // here on a dead endpoint cap, which leaves `reply_cap` untouched (the kernel
        // validates the endpoint cap before the embedded grant), so reusing it is leak-free.
        // NOTE (stopgap): this is the one place a client falls back to the kernel name path;
        // all other reacquisition still goes through the userspace registry. The planned
        // "move naming to the supervisor" work removes this exception — see docs.
        let mut reg = CapHandle(self.find_send_slot("registry")?);
        if self.send_with_cap_by_handle(reg, reply_cap, &req).is_err() {
            reg = self.reacquire_cap("registry").ok()?;
            self.send_with_cap_by_handle(reg, reply_cap, &req).ok()?;
        }

        // The registry always replies (found or not-found).
        let reply = crate::ipc::recv(self.recv_handle()?).ok()?;
        if reply.payload_bytes().first() == Some(&REGISTRY_FOUND) {
            self.take_pending_cap()
        } else {
            None
        }
    }

    /// Send a request to a named `peer` and block for its reply (synchronous
    /// request/response). Embeds a per-request reply cap — a `SEND|GRANT` copy of
    /// this service's own endpoint cap — so the server can reply via
    /// `take_pending_cap()` + `send_by_handle()` (the registry pattern, §8). The
    /// caller must own an endpoint and not have other traffic racing the reply.
    /// `None` if the peer is unknown, the cap cannot be derived, or the send fails.
    pub fn request_with_reply(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
    ) -> Option<crate::ipc::Message> {
        let target = CapHandle(self.find_send_slot(peer)?);
        let self_grant = self.self_grant_handle()?;
        let reply_cap = self.derive_cap(self_grant)?;
        self.send_with_cap_by_handle(target, reply_cap, msg).ok()?;
        crate::ipc::recv(self.recv_handle()?).ok()
    }

    /// Reacquire a fresh SEND cap to `peer` via the **registry service** (H11) and
    /// point the named-peer cache at it, so subsequent `try_send(peer)` / `send(peer)`
    /// use the new cap. This is the registry-service replacement for `reacquire_cap`
    /// (the kernel syscall-10 path). Returns `false` if the registry cannot currently
    /// resolve the name (e.g. the named service has not yet re-registered after its
    /// own restart) — the caller should retry on a later tick.
    pub fn reacquire_via_registry(&self, peer: &str) -> bool {
        match self.registry_lookup(peer) {
            Some(h) => { cache_send_slot(peer, h.0); true }
            None    => false,
        }
    }

    /// Handle to this service's `SEND|GRANT` cap to its **own** endpoint, minted at
    /// spawn (H11). The service announces itself to the registry by deriving a copy
    /// (`derive_cap`) and granting it across — keeping this original so it can
    /// re-register after a registry restart. `None` if the service has no endpoint.
    pub fn self_grant_handle(&self) -> Option<crate::capability::CapHandle> {
        let slot = Self::ctx().self_grant_slot;
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

    /// The wall-clock datetime captured by the kernel at **boot** (InspectKernel query 12, ungated).
    /// Same packed layout as `datetime`. Pairs with `datetime` to compute uptime as a wall-clock
    /// delta — portable across timer modes (a tick counter's rate is not: periodic-mode QEMU ticks
    /// at ~10 Hz, TSC-deadline HW at 100 Hz). Returns the epoch (all-zero fields) if not captured.
    pub fn boot_datetime(&self) -> Datetime {
        // SAFETY: syscall(13) = InspectKernel; query_id=12 = packed boot datetime.
        let p = unsafe { raw_syscall(13, 12, 0, 0) } as u64;
        Self::unpack_datetime(p)
    }

    /// System uptime in **seconds** = now − boot, both from the hardware RTC. Never negative
    /// (saturates at 0). The `uptime` shell command renders this. Wall-clock based, so it is
    /// correct regardless of the APIC timer mode (unlike a raw tick counter).
    pub fn uptime_secs(&self) -> i64 {
        (self.datetime().epoch_secs() - self.boot_datetime().epoch_secs()).max(0)
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

    /// Read the hardware real-time clock (wall-clock date/time) via the kernel.
    ///
    /// Ambient — the time of day is task-neutral hardware info, like the TSC.
    /// Wraps InspectKernel query 11; the kernel returns the fields packed into a
    /// `u64` (see `kernel/src/arch/x86_64/rtc.rs`), which this unpacks.
    pub fn datetime(&self) -> Datetime {
        // SAFETY: syscall(13) = InspectKernel; query_id=11 = packed RTC datetime.
        let p = unsafe { raw_syscall(13, 11, 0, 0) } as u64;
        Self::unpack_datetime(p)
    }

    /// Decode the packed RTC `u64` (the layout shared by query 11 / 12) into a `Datetime`.
    fn unpack_datetime(p: u64) -> Datetime {
        Datetime {
            second: (p & 0x3F) as u8,
            minute: ((p >> 6) & 0x3F) as u8,
            hour: ((p >> 12) & 0x1F) as u8,
            day: ((p >> 17) & 0x1F) as u8,
            month: ((p >> 22) & 0x0F) as u8,
            year: ((p >> 26) & 0xFFF) as u16,
        }
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

    /// Mint a **delegated resource** (§7.10, P2 file-as-capability): the kernel allocates a
    /// fresh resource owned by this service and a cap for it carrying `rights` (use the
    /// `RIGHT_*` bits), returning `(resource_id, cap)`. Requires the `RESOURCE_MINT` authority
    /// (held by `fs`). The `resource_id` is what the kernel badges into a later
    /// `resource_invoke`, so this service knows which resource a client is acting on (e.g. `fs`
    /// maps it → file). `None` if the authority is missing, the band is full, or the cap table
    /// is full. Syscall 30 = `ResourceMint`.
    pub fn resource_mint(&self, rights: u8) -> Option<(u64, CapHandle)> {
        let mut id: u64 = 0;
        // SAFETY: syscall(30) = ResourceMint; arg1 points at our own `id` for the kernel to
        // fill via write_user_bytes (validated kernel-side). Return is the cap slot or <0.
        let ret = unsafe { raw_syscall(30, rights as u64, &mut id as *mut u64 as u64, 0) };
        if ret < 0 { None } else { Some((id, CapHandle(ret as u32))) }
    }

    /// Use a delegated resource cap (§7.10) — the "send" of file-as-capability. The kernel
    /// validates the cap holds `right` (a read needs `RIGHT_READ`, a write needs `RIGHT_WRITE`;
    /// a cap lacking it fails `CapInsufficientRights` — the non-escalation check), then routes
    /// `msg` to the owning service badged with the resource id + the validated right, embedding
    /// `reply` (a SEND|GRANT cap) so the owner can reply. `Ok(())` on delivery. Syscall 31.
    pub fn resource_invoke(&self, file: CapHandle, right: u8, reply: CapHandle, msg: &Message)
        -> Result<(), IpcError> {
        let packed = ((right as u64) << 32) | ((reply.0 as u64) << 16) | (file.0 as u64);
        let payload = msg.payload_bytes();
        // SAFETY: syscall(31) = ResourceInvoke; packed + payload are user values the kernel
        // validates (cap slots, rights, generation, and the message bounds) before acting.
        let ret = unsafe {
            raw_syscall(31, packed, payload.as_ptr() as u64, payload.len() as u64)
        };
        if ret == 0 { Ok(()) } else { Err(crate::ipc::i64_to_ipc_error(ret)) }
    }

    /// Read (and clear) the delegated-resource badge of the message just `recv`'d (§7.10). A
    /// service that owns delegated resources (e.g. `fs`) calls this right after `recv`: `Some((
    /// resource_id, right))` means the message was a **kernel-validated** invocation of a real
    /// cap on `resource_id` with `right` already checked (the owner enforces op ≤ `right`); `None`
    /// means an ordinary message (no badge — handle it on the name-addressed path). The badge
    /// cannot be forged over a plain `send`, so its presence is trustworthy. Syscall 33.
    pub fn last_recv_badge(&self) -> Option<(u64, u8)> {
        // SAFETY: syscall(33) = LastRecvBadge; reads+clears this task's stored badge.
        let packed = unsafe { raw_syscall(33, 0, 0, 0) } as u64;
        if packed == 0 {
            None
        } else {
            Some((packed & 0xFFFF_FFFF, ((packed >> 32) & 0xFF) as u8))
        }
    }

    /// Revoke a delegated resource this service owns (§7.10): bumps its generation so every
    /// outstanding cap to it goes stale (clients see `CapRevoked`/`EndpointDead` on next use).
    /// Owner-gated by the kernel (ownership is the check). `true` on success. Syscall 32.
    pub fn resource_revoke(&self, resource_id: u64) -> bool {
        // SAFETY: syscall(32) = ResourceRevoke; the kernel checks this task owns the resource.
        unsafe { raw_syscall(32, resource_id, 0, 0) == 0 }
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

    /// Safe MMIO handle to this service's device register window, if one was
    /// granted (§12) — the neutrally-named accessor for non-USB drivers (e.g. the
    /// AHCI `block-driver`, which maps its HBA ABAR here). Same kernel-mapped
    /// window as [`xhci_mmio`](Self::xhci_mmio). `None` for non-driver services.
    pub fn mmio(&self) -> Option<crate::mmio::Mmio> {
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

    /// Write a formatted message to the interactive console, with **no** trailing
    /// newline (e.g. a pager status line the cursor should park on).
    pub fn console_write_fmt(&self, args: core::fmt::Arguments) {
        let mut buf    = [0u8; 256];
        let mut cursor = 0usize;
        let _ = core::fmt::write(
            &mut StackWriter { buf: &mut buf, pos: &mut cursor },
            args,
        );
        if cursor > 0 {
            self.console_write(core::str::from_utf8(&buf[..cursor]).unwrap_or("(fmt error)"));
        }
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

    /// Spawn `name` on `core` (0xFFFF = round-robin) and receive a `SEND|GRANT` cap to its recv
    /// endpoint. This is the Phase-0 seam for moving naming out of the kernel
    /// (`docs/naming-design.md`): a spawner (the supervisor) collects a cap to every service it
    /// starts — a userspace `name → cap` map — instead of the kernel resolving names. Requires the
    /// SPAWN cap. `None` if the cap is not held, the spawn failed, or the service has no recv
    /// endpoint to hand back. The old name-wiring path is unchanged; this is purely additive.
    pub fn spawn_returning_endpoint(&self, name: &str, core: u32) -> Option<CapHandle> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return None; }
        let slot = data.spawn_slot;
        if slot == u32::MAX { return None; }
        let bytes  = name.as_bytes();
        let packed = ((core as u64 & 0xFFFF) << 16) | (slot as u64 & 0xFFFF);
        // SAFETY: syscall(38) = SpawnReturningEndpoint; slot from the kernel-written page; bytes valid.
        let ret = unsafe { raw_syscall(38, packed, bytes.as_ptr() as u64, bytes.len() as u64) };
        if ret < 0 { None } else { Some(CapHandle(ret as u32)) }
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
