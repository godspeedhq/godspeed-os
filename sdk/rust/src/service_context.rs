// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! ServiceContext - entry-point type handed to every service's `service_main`.
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

#[cfg(test)]
mod datetime_tests {
    use super::Datetime;

    fn dt(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Datetime {
        Datetime { year, month, day, hour, minute, second }
    }

    fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }
    fn days_in_month(y: i64, m: i64) -> i64 {
        match m {
            1 => 31, 2 => if is_leap(y) { 29 } else { 28 }, 3 => 31, 4 => 30, 5 => 31, 6 => 30,
            7 => 31, 8 => 31, 9 => 30, 10 => 31, 11 => 30, 12 => 31, _ => 0,
        }
    }

    /// A deliberately naive, obviously-correct reference: count days from 1970 by iterating years +
    /// months. It cannot share a bug with Hinnant's closed form - which is what makes the cross-check valid.
    fn reference_epoch(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        let mut days: i64 = 0;
        for yy in 1970..y { days += if is_leap(yy) { 366 } else { 365 }; }
        for mm in 1..mo  { days += days_in_month(y, mm); }
        days += d - 1;
        days * 86_400 + h * 3_600 + mi * 60 + s
    }

    #[test]
    fn reference_matches_known_unix_anchors() {
        // Validate the REFERENCE itself against well-known Unix epochs first, so the cross-check is trustworthy.
        assert_eq!(reference_epoch(1970, 1, 1, 0, 0, 0), 0);
        assert_eq!(reference_epoch(2000, 1, 1, 0, 0, 0), 946_684_800);
        assert_eq!(reference_epoch(2038, 1, 19, 3, 14, 8), 2_147_483_648); // Y2038 (2^31)
        assert_eq!(reference_epoch(2000, 2, 29, 0, 0, 0), 951_782_400);    // leap day
    }

    #[test]
    fn epoch_secs_matches_known_unix_anchors() {
        assert_eq!(dt(1970, 1, 1, 0, 0, 0).epoch_secs(), 0);
        assert_eq!(dt(2000, 1, 1, 0, 0, 0).epoch_secs(), 946_684_800);
        assert_eq!(dt(2038, 1, 19, 3, 14, 8).epoch_secs(), 2_147_483_648);
    }

    #[test]
    fn epoch_secs_matches_reference_over_a_multi_century_sweep() {
        // Cross-check Hinnant (the SDK's epoch_secs - the twin every SERVICE uses) vs the naive reference for
        // every month of 1971..=2100: div-4 leaps, the 2100 century non-leap, the 2000 leap-400. This is the
        // drift guard - if a future edit to the SDK's days_since_epoch diverges from the kernel's (pinned in
        // kernel/src/clock.rs), this catches it.
        for y in 1971..=2100i64 {
            for mo in 1..=12i64 {
                let last = days_in_month(y, mo);
                for &d in &[1i64, 15, 28, last] {
                    for &(h, mi, s) in &[(0i64, 0i64, 0i64), (23, 59, 59), (12, 30, 15)] {
                        assert_eq!(
                            dt(y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8).epoch_secs(),
                            reference_epoch(y, mo, d, h, mi, s),
                            "SDK epoch_secs mismatch at {}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, mi, s);
                    }
                }
            }
        }
    }

    #[test]
    fn weekday_matches_known_anchors() {
        // weekday() shares days_since_epoch with epoch_secs. 0=Sun..6=Sat. Known: 1970-01-01 Thursday (4),
        // 2000-01-01 Saturday (6), 2026-06-06 Saturday (6) - the T630 `date` HW example.
        assert_eq!(dt(1970, 1, 1, 0, 0, 0).weekday(), 4);
        assert_eq!(dt(2000, 1, 1, 0, 0, 0).weekday(), 6);
        assert_eq!(dt(2026, 6, 6, 0, 0, 0).weekday(), 6);
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
// Dynamic send-cap cache - updated by `reacquire_cap` after EndpointDead.
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
// TaskStat - returned by ServiceContext::task_stat.
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
    /// Number of times this service has been restarted (0 on a fresh boot / first spawn, +1 per
    /// respawn). Saturating u64 - effectively unbounded.
    pub restart_count: u64,
    /// Current inbound IPC queue depth (0-16).
    pub queue_depth: u8,
    /// Timer ticks spent as the running task on its core (monotonic since boot).
    pub run_ticks:   u64,
    /// Seconds since this service last (re)started - resets on restart. Per-service uptime.
    pub uptime_secs: u64,
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
// AllocError - returned by ServiceContext::alloc_mem.
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

/// Point the dynamic send-cap cache entry for `name` at `new_slot`, so the next
/// `find_send_slot(name)` resolves to the freshly-acquired cap. Mirrors the inline
/// update in `reacquire_cap`.
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
    /// pacing and "wait for child" loops - not for precise timing.
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

        // Update the dynamic cache. Reuse this peer's EXISTING entry if it has one (and reclaim its
        // now-stale cap), otherwise take a free slot. Reclaiming the old cap is essential: without it
        // every restart-reacquire orphans the previous cap, and a storm (e.g. `chaos max-carnage`)
        // fills the cap table until `derive_cap`/`acquire_send_cap` start returning None - which shows
        // up as "storage unavailable" (fs) and "never registered" (pipe filters). Searching the peer's
        // entry first also prevents creating a duplicate entry when a free slot precedes it.
        // SAFETY: single-threaded service; no concurrent cache writes. addr_of_mut! avoids a direct
        // &mut to the static (static_mut_refs lint).
        let mut stale: Option<u32> = None;
        let mut placed = false;
        unsafe {
            let cache = &mut *core::ptr::addr_of_mut!(SEND_CAP_CACHE);
            for entry in cache.iter_mut() {
                if entry.name_len as usize == len && &entry.name[..len] == bytes {
                    if entry.slot != u32::MAX && entry.slot != new_slot { stale = Some(entry.slot); }
                    entry.slot = new_slot;
                    placed = true;
                    break;
                }
            }
            if !placed {
                for entry in cache.iter_mut() {
                    if entry.slot == u32::MAX {
                        entry.slot     = new_slot;
                        entry.name_len = len as u8;
                        entry.name     = [0u8; PEER_NAME_BYTES];
                        entry.name[..len].copy_from_slice(bytes);
                        break;
                    }
                }
            }
        }
        if let Some(old) = stale { self.remove_cap(CapHandle(old)); }

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

    /// Send a request to a named `peer` and block for its reply (synchronous
    /// request/response). Embeds a per-request reply cap - a `SEND|GRANT` copy of
    /// this service's own endpoint cap - so the server can reply via
    /// `take_pending_cap()` + `send_by_handle()` (the request/reply pattern, §8). The
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
        if self.send_with_cap_by_handle(target, reply_cap, msg).is_err() {
            // Send failed (dead endpoint): the embedded reply cap was NOT transferred (the kernel
            // validates the endpoint cap before the grant), so reclaim it here. Without this, a storm
            // of failed sends leaks reply caps until the table fills and every request returns None.
            self.remove_cap(reply_cap);
            return None;
        }
        crate::ipc::recv(self.recv_handle()?).ok()
    }

    /// Like `request_with_reply`, but the wait for the reply is **bounded** by `max_secs` of
    /// **wall-clock** time (the RTC). Returns `None` on timeout - so a peer that dies *after*
    /// receiving the request but *before* replying cannot block the caller forever (the blocking
    /// `recv` in `request_with_reply` would hang). Use it when the peer may be unstable - e.g.
    /// writing a report to `fs` right after a chaos storm hammered `fs` + its `block-driver`.
    ///
    /// Uses the RTC (not a TSC-cycle deadline) deliberately: a cycle bound is not portable - under
    /// QEMU's TCG the guest TSC races ahead and expires the deadline before the reply arrives, while
    /// the RTC is real wall-clock on both TCG and hardware. Polls `try_recv`, yielding cooperatively.
    pub fn request_with_reply_deadline(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        max_secs: i64,
    ) -> Option<crate::ipc::Message> {
        let target = CapHandle(self.find_send_slot(peer)?);
        let self_grant = self.self_grant_handle()?;
        let reply_cap = self.derive_cap(self_grant)?;
        if self.send_with_cap_by_handle(target, reply_cap, msg).is_err() {
            self.remove_cap(reply_cap);   // send failed: reclaim the untransferred reply cap (no leak)
            return None;
        }
        let t0 = self.datetime().epoch_secs();
        loop {
            if let Some(r) = self.try_recv() { return Some(r); }
            if self.datetime().epoch_secs() - t0 >= max_secs {
                self.remove_cap(reply_cap);   // reply never consumed - reclaim its slot
                return None;
            }
            self.yield_cpu();
        }
    }

    /// Reacquire a fresh SEND cap to `peer` and point the named-peer cache at it, so subsequent
    /// `try_send(peer)` / `send(peer)` use the new cap. Returns `false` if `peer` cannot currently
    /// be resolved (e.g. it has not finished respawning) - the caller should retry on a later tick.
    ///
    /// A thin shim over `reacquire_cap` (syscall 10): name resolution is the **kernel name
    /// directory**, not a service. The directory is populated synchronously at each service's spawn,
    /// so there is no round-trip and no bootstrap chicken-and-egg (the directory lives in the kernel,
    /// always reachable). `reacquire_cap` also updates the send-cap cache.
    pub fn reacquire_by_name(&self, peer: &str) -> bool {
        self.reacquire_cap(peer).is_ok()
    }

    /// Handle to this service's `SEND|GRANT` cap to its **own** endpoint, minted at
    /// spawn. A service hands a copy of its endpoint to a peer by deriving one
    /// (`derive_cap`) and granting it across - keeping this original so it can derive
    /// again later. `None` if the service has no endpoint.
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
    /// Returns `CapNotGrantable` if the cap lacks `GRANT` - the cap is kept.
    pub fn send_with_cap(&self, peer: &str, msg: &crate::ipc::Message) -> Result<(), crate::ipc::IpcError> {
        let slot = self.find_send_slot(peer)
            .ok_or(crate::ipc::IpcError::CapError(crate::capability::CapError::CapNotHeld))?;
        // syscall 11 = SendWithCap
        // arg0 = (grant_slot << 16) | endpoint_slot - same slot holds both SEND and GRANT.
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
    /// delta - portable across timer modes (a tick counter's rate is not: periodic-mode QEMU ticks
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
    /// Wraps InspectKernel query 9 (ambient - screen geometry is task-neutral).
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
    /// USB keyboard driver (xHCI) in every terminal path once it has finished - the
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
    /// in benchmark probes (§22 Perf B1-B10). Not comparable across hosts.
    pub fn read_tsc(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=3 = read TSC.
        let ret = unsafe { raw_syscall(13, 3, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Read the hardware real-time clock (wall-clock date/time) via the kernel.
    ///
    /// Ambient - the time of day is task-neutral hardware info, like the TSC.
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
        let mut buf = [0u8; 80];
        // SAFETY: syscall(16) = TaskStat; buf is a local array on the user stack.
        let ret = unsafe {
            raw_syscall(16, slot as u64, buf.as_mut_ptr() as u64, 80)
        };
        if ret != 0 {
            return TaskStat {
                valid: false, state: 0, core: 0,
                mem_used: 0, mem_limit: 0, name_len: 0, name: [0u8; 32],
                restart_count: 0, queue_depth: 0, run_ticks: 0, uptime_secs: 0,
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
        let restart_count = u64::from_le_bytes([buf[56], buf[57], buf[58], buf[59],
                                                buf[60], buf[61], buf[62], buf[63]]);
        let queue_depth = buf[3];
        let run_ticks   = u64::from_le_bytes([buf[64], buf[65], buf[66], buf[67],
                                              buf[68], buf[69], buf[70], buf[71]]);
        let uptime_secs = u64::from_le_bytes([buf[72], buf[73], buf[74], buf[75],
                                              buf[76], buf[77], buf[78], buf[79]]);
        TaskStat { valid, state, core, mem_used, mem_limit, name_len: copy_len, name,
                   restart_count, queue_depth, run_ticks, uptime_secs }
    }

    /// List the capabilities held by the task in `slot`, into `out`. Returns the
    /// number of entries written (capped at `out.len()` and 64). Requires the
    /// INTROSPECT cap. Best-effort snapshot - see [`task_stat`](Self::task_stat).
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
    /// explicit handles - used by P3 where the endpoint and grant slot are
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

    /// Use a delegated resource cap (§7.10) - the "send" of file-as-capability. The kernel
    /// validates the cap holds `right` (a read needs `RIGHT_READ`, a write needs `RIGHT_WRITE`;
    /// a cap lacking it fails `CapInsufficientRights` - the non-escalation check), then routes
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
    /// means an ordinary message (no badge - handle it on the name-addressed path). The badge
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
    /// A foreground full-screen app disables echo while it owns the screen - so
    /// its raw key polls do not smear its frame - and re-enables it on exit.
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

    /// Whether THIS task currently owns (or shares, when unclaimed) console input - i.e. its console
    /// reads return bytes. False when another task holds the foreground (syscall 40, e.g. `chaos`): a
    /// backgrounded task should then stay quiet (not draw, not read) and redraw its prompt only when
    /// this returns true again. InspectKernel query 13 (ungated, caller-specific).
    pub fn is_console_foreground(&self) -> bool {
        // SAFETY: syscall(13) = InspectKernel; query 13 = is-foreground for the caller.
        unsafe { raw_syscall(13, 13, 0, 0) != 0 }
    }

    /// Claim exclusive console input (syscall 40, op = 1): after this, only THIS task's
    /// `try_console_read` returns bytes; every other task reads empty. The `chaos` service
    /// claims it for the duration of a run so a resurrected shell cannot swallow its
    /// `q`-to-quit. Pair with `release_console_foreground` on exit, after ensuring a live
    /// shell exists to hand the keyboard back to. Requires the CONSOLE_READ cap.
    pub fn claim_console_foreground(&self) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.console_read_slot;
        if slot == u32::MAX { return; }
        // SAFETY: syscall(40) = ConsoleForeground; op 1 = claim; slot is kernel-written cap index.
        let _ = unsafe { raw_syscall(40, slot as u64, 1, 0) };
    }

    /// Release exclusive console input (syscall 40, op = 0) back to the unclaimed state, so
    /// any CONSOLE_READ holder (the shell) reads normally again. Idempotent.
    pub fn release_console_foreground(&self) {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return; }
        let slot = data.console_read_slot;
        if slot == u32::MAX { return; }
        // SAFETY: syscall(40) = ConsoleForeground; op 0 = release; slot is kernel-written cap index.
        let _ = unsafe { raw_syscall(40, slot as u64, 0, 0) };
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
    /// [`xhci_mmio`](Self::xhci_mmio) - a driver service holds exactly one
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
    /// granted (§12) - the neutrally-named accessor for non-USB drivers (e.g. the
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

    // `abort()` (the kernel `Abort`/9 syscall) was removed: it let any task panic the kernel, an
    // ungated §3.1 hole, and its only caller (`init`) is gone (Phase 5). A service that hits a fatal
    // error now simply dies (and is restarted by the supervisor) rather than aborting the kernel.

    /// Trigger a hardware reset via the kernel reboot syscall (18). Does not return.
    ///
    /// Flushes "rebooting..." to serial before the reset so the operator sees
    /// confirmation in PuTTY before the line goes silent.
    pub fn reboot(&self) -> ! {
        // SAFETY: syscall(18) = Reboot; no arguments.
        unsafe { raw_syscall(18, 0, 0, 0) };
        loop {} // unreachable
    }

    /// Attempt a reboot but RETURN the syscall result instead of assuming it never comes back.
    ///
    /// A successful reset does not return; a denial returns a negative error code (CapNotHeld = -2
    /// when the caller lacks the REBOOT capability, §3.1). For tests/probes that must *observe* the
    /// denial without resetting the machine - ordinary rebooters use `reboot()`.
    pub fn try_reboot(&self) -> i64 {
        // SAFETY: syscall(18) = Reboot; no arguments. On success it never returns; on denial it
        // returns the error code, which we hand back to the caller.
        unsafe { raw_syscall(18, 0, 0, 0) }
    }

    /// Advisory yield (§9.3).
    pub fn yield_cpu(&self) {
        // SAFETY: syscall(4) = Yield; always valid from ring-3.
        unsafe { raw_syscall(4, 0, 0, 0); }
    }

    /// Park this task forever: block with no waker. For idle services that have
    /// no further work (init, supervisor) - far better than `loop { yield_cpu() }`,
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
    /// the framebuffer/TV - the interactive surface (see docs/console-service.md).
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
    /// `ESC[K` (erase to end of line) before the newline - so a full-screen app
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
    /// starts - a userspace `name → cap` map - instead of the kernel resolving names. Requires the
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

    /// Spawn `name` on `core` (0xFFFF = round-robin), wiring its send-peers from caller-supplied
    /// `(label, cap)` pairs **instead of the kernel name table** (Phase 0b, `docs/naming-design.md`).
    /// Each cap must be one this task holds with GRANT; the kernel copies it into the child under
    /// `label`, so the child's `ctx.capability(label)` resolves to it. Returns the new service's
    /// endpoint cap (`Ok(Some)`), `Ok(None)` if it spawned but has no recv endpoint (a producer like
    /// `greet`), or `Err(())` if the spawn failed. Requires the SPAWN cap. This is how the supervisor
    /// wires a dependent from its name→cap map without the kernel resolving names.
    pub fn spawn_with_caps(&self, name: &str, core: u32, installs: &[(&str, CapHandle)])
        -> Result<Option<CapHandle>, ()>
    {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(()); }
        let slot = data.spawn_slot;
        if slot == u32::MAX { return Err(()); }
        let nb = name.as_bytes();
        if nb.is_empty() || nb.len() > 64 || installs.len() > 4 { return Err(()); }

        // Build [name_len, name, count, {label_len, label, slot_lo, slot_hi}…] in a stack buffer.
        let mut buf = [0u8; 256];
        let mut n = 0usize;
        buf[n] = nb.len() as u8; n += 1;
        buf[n..n + nb.len()].copy_from_slice(nb); n += nb.len();
        buf[n] = installs.len() as u8; n += 1;
        for (label, cap) in installs {
            let lb = label.as_bytes();
            if lb.is_empty() || lb.len() > 24 || n + 1 + lb.len() + 2 > buf.len() { return Err(()); }
            buf[n] = lb.len() as u8; n += 1;
            buf[n..n + lb.len()].copy_from_slice(lb); n += lb.len();
            buf[n] = (cap.0 & 0xFF) as u8; n += 1;
            buf[n] = ((cap.0 >> 8) & 0xFF) as u8; n += 1;
        }
        let packed = ((core as u64 & 0xFFFF) << 16) | (slot as u64 & 0xFFFF);
        // SAFETY: syscall(39) = SpawnWithCaps; slot from the kernel-written page; buf valid for n bytes.
        let ret = unsafe { raw_syscall(39, packed, buf.as_ptr() as u64, n as u64) };
        match ret {
            -2 => Ok(None),                              // spawned OK, no recv endpoint
            r if r >= 0 => Ok(Some(CapHandle(r as u32))),
            _  => Err(()),                               // spawn failed
        }
    }

    /// Spawn `producer` and delegate it a SEND cap to `sink`'s endpoint
    /// (`producer | sink`). `sink` must already be spawned. Requires the spawn
    /// capability - held only by the shell/supervisor.
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
