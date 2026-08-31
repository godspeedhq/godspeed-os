// SPDX-License-Identifier: Apache-2.0
//! ServiceContext - entry-point type handed to every service's `service_main`.
//!
//! Provides safe, named access to the capabilities the service declared in its
//! contract. Capability names match the contract field names exactly.
//! Requesting a cap not in the contract returns `Err(CapNotHeld)`.

use crate::capability::{CapError, CapHandle};
use crate::ipc::{IpcError, Message};
use crate::syscall::raw_syscall;

/// Outcome of an abortable request/reply ([`ServiceContext::request_with_reply_abortable`]): the reply
/// arrived, the user pressed `q`/`Q`/ESC to abort the wait, or the deadline expired with no reply.
pub enum ReqOutcome {
    Reply(Message),
    Aborted,
    Timeout,
}

/// Outcome of [`ServiceContext::request_with_reply_deadline_outcome`], which distinguishes the two
/// ways a deadline request fails - so a caller can self-heal a *restarted* peer without penalising a
/// *silent* one. `SendFailed`: the send never left (the peer's cap is stale or its name is currently
/// unresolvable - it was killed and respawned, generation bumped), so reacquiring it by name and
/// retrying will reach the fresh instance. `Timeout`: the peer received the request but did not reply
/// within the deadline (a genuinely silent/absent host) - retrying only doubles the wait.
/// `request_with_reply_deadline` collapses both to `None`; use this when the difference matters.
pub enum DeadlineOutcome {
    Reply(Message),
    SendFailed,
    /// The peer is ALIVE and its queue is FULL. A different failure entirely, and it was folded into
    /// `SendFailed` until x86 showed the cost: `net-stack` answered a full queue by reacquiring a
    /// capability that was never stale, retried once and gave up - "5 of 6 REQUESTs never left the
    /// host", DHCP backing off for 34 seconds on a link that was up the whole time.
    ///
    /// Congestion is transient by definition. The answer is to pace and retry, not to go looking for
    /// a peer that never went anywhere.
    QueueFull,
    Timeout,
}

/// Where a wall-clock reading came from. Reported alongside the time so a displayed timestamp says what
/// it is standing on: a local hardware clock, a network correction, or nothing (§26.4 - a fallback chain
/// is mechanism while its choice is visible, and magic when it is not).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// No clock: no hardware RTC and nothing has set the time. `date` says so rather than inventing one.
    Unset,
    /// A local hardware real-time clock reading a plausible date.
    Rtc,
    /// Set from the network (SNTP) this boot.
    Ntp,
    /// Started from the persisted floor (`/clock.last`) - no RTC on this board and no network sync yet.
    ///
    /// A real reading and a distinct one: it is a LOWER BOUND carried over from the last boot, advancing
    /// correctly since, but it cannot know how long the machine was powered off. Reporting it as `Rtc`
    /// would claim hardware this board does not have, and as `Unset` would deny a time it is displaying.
    Floor,
}

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
    /// Decode Unix epoch seconds into calendar fields (Howard Hinnant's `civil_from_days`) - the inverse
    /// of [`Datetime::epoch_secs`]. Used to render a stored epoch, such as the clock floor, as a date.
    pub fn from_epoch_secs(epoch: i64) -> Datetime {
        let days = epoch.div_euclid(86_400);
        let rem = epoch.rem_euclid(86_400);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        Datetime {
            year:  (y + (m <= 2) as i64) as u16,
            month: m as u8,
            day:   d as u8,
            hour:   (rem / 3_600) as u8,
            minute: ((rem % 3_600) / 60) as u8,
            second: (rem % 60) as u8,
        }
    }

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

    #[test]
    fn from_epoch_secs_round_trips_over_the_sweep() {
        // The INVERSE (civil_from_days) must undo epoch_secs exactly, over the same multi-century sweep -
        // the drift guard its forward twin already has. Without this, the only thing pinning the decode
        // used to render a stored epoch (the clock floor) would be that it looked right once.
        for y in 1971..=2100i64 {
            for mo in 1..=12i64 {
                let last = days_in_month(y, mo);
                for &d in &[1i64, 15, 28, last] {
                    for &(h, mi, s) in &[(0i64, 0i64, 0i64), (23, 59, 59), (12, 30, 15)] {
                        let a = dt(y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8);
                        assert!(Datetime::from_epoch_secs(a.epoch_secs()) == a,
                            "from_epoch_secs round-trip mismatch at {}-{:02}-{:02} {:02}:{:02}:{:02}",
                            y, mo, d, h, mi, s);
                    }
                }
            }
        }
        assert!(Datetime::from_epoch_secs(0) == dt(1970, 1, 1, 0, 0, 0));
    }
}

// ---------------------------------------------------------------------------
// ServiceContextData page layout.
// MUST match `ServiceContextData` in `kernel/src/task/mod.rs`.
// ---------------------------------------------------------------------------

const SERVICE_CTX_ADDR:    u64   = 0x3ff000;
/// What a spawner tells the kernel about a task it wants started from an image IT supplies
/// (`SpawnImage`, syscall 52). Layout must match the kernel's `SpawnRequest` exactly.
///
/// Shaped for the end state, not for what the kernel honours today: the hardware and privilege
/// fields are here because step D needs them, and a request that sets one is REFUSED loudly rather
/// than silently ignored (`docs/service-ownership.md`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnRequest {
    pub version:      u32,
    pub flags:        u32,
    pub image_ptr:    u64,
    pub image_len:    u64,
    pub name_ptr:     u64,
    pub name_len:     u32,
    pub core:         u32,
    pub memory_limit: u64,
    pub privileges:   u32,
    pub hw_flags:     u32,
    pub mmio_base:    u64,
    pub mmio_len:     u64,
    pub dma_pages:    u32,
    pub bdf:          u32,
    pub irq_count:    u32,
    pub irqs:         [u8; 4],
    pub peers_ptr:      u64,
    pub peers_len:      u32,
    pub _pad:           u32,
    /// Caller-provided caps to install into the child: `[label_len][label][slot_lo][slot_hi]`
    /// repeated. The same encoding `SpawnWithCaps` uses - one wire format, not two.
    pub installs_ptr:   u64,
    pub installs_count: u32,
    pub _pad2:          u32,
    /// Mode selector. Named for probes, its first user, but general: `observe` reads it to choose
    /// one-shot / live / foreground, and takes the LIVE LOOP at 0.
    pub probe_mode:     u32,
    pub _pad3:          u32,
}

/// 2 since `installs` was added. A spawner built against a different kernel is refused loudly.
pub const SPAWN_REQUEST_VERSION:  u32 = 3;
pub const SPAWN_FLAG_REQ_RECV:    u32 = 1 << 0;
pub const SPAWN_FLAG_REQ_CONSOLE: u32 = 1 << 1;
/// `core` is a STRICT placement, not a preference.
///
/// The two are different rules (9.2) and the kernel has always had both: an explicitly-requested core
/// (a restart's `--core N`) is REJECTED with `PlacementInvalid` when that core is not ready, because
/// silently placing the service elsewhere would defeat the point of asking. A catalogue row's
/// PREFERRED core falls back to round-robin instead, so a machine with fewer cores still boots.
///
/// Without this bit a moved service's table preference read as an override, and every service naming
/// a core the machine does not have failed to spawn rather than degrading - `logger` and `xhci` (both
/// core 2) simply vanished under `-smp 2`, which 11.3 requires to keep working.
pub const SPAWN_FLAG_CORE_STRICT: u32 = 1 << 2;
/// Mint the child's name-wired peer caps WITH `GRANT`, so it can re-delegate them.
///
/// AUTHORITY, not a setting: handing out a re-delegatable capability is itself a grant (§7.4), which
/// is why the kernel used to keep this keyed by name for the one probe that needs it (§22 Test 5A).
/// As a request bit it is checked the same way every privilege is - the spawner may ask for it only
/// because it could transfer such a cap itself.
pub const SPAWN_FLAG_PEERS_GRANT: u32 = 1 << 3;

/// Bits for `SpawnRequest::privileges`. A spawner may only request what it HOLDS ITSELF - the kernel
/// checks, and refuses otherwise - so this passes authority on, it never mints it (3.1, 7.3).
pub mod privbits {
    pub const SPAWN:           u32 = 1 << 0;
    pub const CONSOLE_PUSH:    u32 = 1 << 1;
    pub const INTROSPECT:      u32 = 1 << 2;
    pub const SERVICE_CONTROL: u32 = 1 << 3;
    pub const FIRE_IRQ:        u32 = 1 << 4;
    pub const REBOOT:          u32 = 1 << 5;
    pub const ACQUIRE_ANY:     u32 = 1 << 6;
    pub const RESOURCE_MINT:   u32 = 1 << 7;
    /// ARM-only in practice; the bit is arch-neutral.
    pub const GPIO:            u32 = 1 << 8;
    /// SET_CLOCK with READ (raise the clock FLOOR), not WRITE (step the clock).
    pub const SET_CLOCK_FLOOR: u32 = 1 << 9;
    /// SET_CLOCK with WRITE (set the wall clock). Distinct from SET_CLOCK_FLOOR, the READ right.
    pub const SET_CLOCK:       u32 = 1 << 10;
    /// NET_DEVICE: move ethernet frames through the in-kernel network device (aarch64's GENET).
    pub const NET_DEVICE:      u32 = 1 << 11;
}

/// Device classes a spawner can name in `SpawnRequest::hw_flags`. The kernel resolves the class to
/// what its own bus scan found - a CLASS rather than an address, because the kernel keeps a permanent
/// physical DMA reservation per device that a respawned driver must get back.
pub mod hwclass {
    pub const NONE:        u32 = 0;
    pub const AHCI:        u32 = 1;
    pub const NIC:         u32 = 2;
    pub const XHCI:        u32 = 3;
    pub const EHCI:        u32 = 4;
    pub const DWC2:        u32 = 5;
    pub const FRAMEBUFFER: u32 = 6;
    /// Not a device: the software-raised test interrupt (§22 IR1). A class, so that the probe which
    /// receives it names a CLASS like any driver and the kernel states the vector.
    pub const TEST_IRQ:    u32 = 7;
}

impl SpawnRequest {
    /// A request with everything the kernel does not yet honour left at zero.
    pub fn new(image: &[u8], name: &str) -> Self {
        Self {
            version: SPAWN_REQUEST_VERSION, flags: 0,
            image_ptr: image.as_ptr() as u64, image_len: image.len() as u64,
            name_ptr: name.as_ptr() as u64, name_len: name.len() as u32,
            core: u32::MAX, memory_limit: 0,
            privileges: 0, hw_flags: 0, mmio_base: 0, mmio_len: 0,
            dma_pages: 0, bdf: 0, irq_count: 0, irqs: [0; 4],
            peers_ptr: 0, peers_len: 0, _pad: 0,
            installs_ptr: 0, installs_count: 0, _pad2: 0,
            probe_mode: 0, _pad3: 0,
        }
    }

    /// Encode `(label, cap)` pairs into `buf` and point this request at them.
    ///
    /// The caller must hold each cap WITH GRANT: the kernel checks it, and that check is what makes
    /// installing them non-escalating (7.3) - a caller that can GRANT a cap could transfer it anyway.
    pub fn set_installs(&mut self, buf: &mut [u8], pairs: &[(&str, CapHandle)]) -> bool {
        let mut n = 0usize;
        for (label, cap) in pairs {
            let lb = label.as_bytes();
            if lb.is_empty() || lb.len() > 32 { return false; }
            if n + 1 + lb.len() + 2 > buf.len() { return false; }
            buf[n] = lb.len() as u8; n += 1;
            buf[n..n + lb.len()].copy_from_slice(lb); n += lb.len();
            buf[n] = (cap.0 & 0xFF) as u8; buf[n + 1] = ((cap.0 >> 8) & 0xFF) as u8; n += 2;
        }
        self.installs_ptr   = if n > 0 { buf.as_ptr() as u64 } else { 0 };
        self.installs_count = pairs.len() as u32;
        true
    }
}

/// The supervisor's command channel wire format.
///
/// ONE definition. This lived in three services at once (`supervisor`, `control`, `shell`) - three
/// copies of a protocol, which is the second-truth problem this project rejects everywhere else
/// (26.4, Commandment III). A drifted copy would not fail loudly; it would send a byte the supervisor
/// reads as a different opcode.
///
/// The supervisor's endpoint receives two kinds of message, told apart by the first byte:
///
/// ```text
///   death notice   "pong"                        <- kernel-generated; a name, [a-z0-9-]
///   command        0x01 op <core u32 LE> name    <- 0x01 cannot begin a name
///                                    name may be followed by NUL-separated PEER names
/// ```
///
/// Choosing an impossible first byte means the kernel's death-notification format does not change at
/// all: the two are unambiguous without the kernel learning anything about commands.
pub mod supcmd {
    /// First byte of a command. Not a legal first byte of a service name.
    pub const MARKER:  u8 = 0x01;
    /// Kill if alive, then spawn.
    pub const RESTART: u8 = b'R';
    /// Spawn a service that is not running.
    pub const SPAWN:   u8 = b'S';

    /// Reply status, one byte, so a caller can log the truth rather than assume success.
    pub const OK:      u8 = 0;
    pub const FAILED:  u8 = 1;
    pub const UNKNOWN: u8 = 2;

    /// Longest command payload: opcode + core + a name and its peers.
    pub const MAX: usize = 128;

    /// Build `MARKER op <core> name[\0peer]*` into `buf`, returning its length.
    pub fn encode(buf: &mut [u8; MAX], op: u8, core: u32, name: &str, peers: &[&str]) -> Option<usize> {
        if name.is_empty() { return None; }
        buf[0] = MARKER;
        buf[1] = op;
        buf[2..6].copy_from_slice(&core.to_le_bytes());
        let mut n = 6usize;
        let mut put = |b: &[u8], buf: &mut [u8; MAX], n: &mut usize| -> bool {
            if *n + b.len() > MAX { return false; }
            buf[*n..*n + b.len()].copy_from_slice(b);
            *n += b.len();
            true
        };
        if !put(name.as_bytes(), buf, &mut n) { return None; }
        for p in peers {
            if !put(&[0u8], buf, &mut n) || !put(p.as_bytes(), buf, &mut n) { return None; }
        }
        Some(n)
    }
}


/// Probe parameters ride in the upper 32 bits of `Spawn`'s `arg0`, which were unused:
/// `[55..48] flags  [47..32] mode  [31..16] core  [15..0] spawn cap slot`.
pub const SPAWN_FLAG_HAS_RECV:  u64 = 1 << 48;
pub const SPAWN_FLAG_SMALL_MEM: u64 = 1 << 49;
pub const SPAWN_FLAG_IS_PROBE:  u64 = 1 << 50;

/// Upper bound on a spawn name payload (`name` + NUL-separated peers). Matches the kernel's limit.
pub const SPAWN_PAYLOAD_MAX: usize = 128;

const SERVICE_CTX_MAGIC:   u32   = 0xD0_5D_EA_D5;
/// MUST match `kernel::task::MAX_SEND_PEERS` - this indexes the kernel-written context page.
const MAX_SEND_PEERS:      usize = 6;
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
    xhci_mmio_len:      u64, // length of the mapped MMIO register window in bytes (SEC-4)
    xhci_dma_va:        u64, // 0 = none; else VA of the driver's DMA arena
    xhci_dma_phys:      u64, // physical base of the DMA arena
    xhci_dma_len:       u64, // length of the DMA arena in bytes
    console_push_slot:  u32, // u32::MAX = none; else CONSOLE_PUSH cap slot
    self_grant_slot:    u32, // u32::MAX = none; else SEND|GRANT cap to own endpoint (H11)
    // --- Framebuffer grant (the `console` service only) ---
    // The kernel maps the display's framebuffer into this service's address space Normal NON-cacheable
    // + USER, as a driver's MMIO BAR is mapped, and describes it here. Deliberately PIXEL geometry only:
    // no rows, no columns, no cell size. Character geometry belongs to the terminal, and the terminal is
    // the service (`docs/console-service.md` §9.7).
    fb_va:              u64, // 0 = no framebuffer grant; else VA of the mapped framebuffer
    fb_len:             u64, // length of the mapping in bytes (pitch * height)
    fb_pitch:           u32, // bytes per scanline
    fb_width:           u32, // visible width in pixels
    fb_height:          u32, // visible height in pixels
    fb_bpp:             u32, // bytes per pixel
    fb_shifts:          u32, // r_shift | g_shift << 8 | b_shift << 16
    send_peers:         [SendPeerEntry; MAX_SEND_PEERS],
    /// A SECOND endpoint, for REPLIES only. `u32::MAX` = none.
    ///
    /// A service that serves clients on the endpoint it also awaits replies on cannot drain that
    /// endpoint while it is blocked for a reply. Sixteen client requests arrive, the queue is full,
    /// and the reply it is waiting for is DROPPED by a peer that (correctly) uses `try_send` rather
    /// than deadlocking. The wait then runs to its full deadline - 30 s per block operation on x86,
    /// which is what made `write append` take 73 seconds.
    ///
    /// Correlation tags cannot reach this: a tag identifies a reply that ARRIVED, and this one never
    /// did. `docs/net-tags-design.md` rejected a second endpoint for lacking a `CreateEndpoint`
    /// syscall - true, and not needed: the first endpoint is minted at spawn and so is this one.
    reply_recv_slot:    u32,
    /// SEND|GRANT cap to `reply_recv_slot`'s endpoint, for handing out as a reply cap. `u32::MAX` = none.
    reply_grant_slot:   u32,
}

// The kernel writes this struct and the SDK reads it, from two crates, with no shared definition -
// they are kept in step BY HAND. There was no check on that, and adding a field to one and not the
// other silently misaligns every field after it: a service would read its neighbour's slot numbers.
//
// Pinned by SIZE in both crates. It does not prove field ORDER, but it catches the mistake that
// actually happens - an append on one side only - and it fails at compile time in the crate that
// drifted rather than at boot in a service that reads garbage.
const SERVICE_CONTEXT_DATA_SIZE: usize = 320;   // 256 + 2 x SendPeerEntry(32) after MAX_SEND_PEERS 4 -> 6
const _: () = assert!(
    core::mem::size_of::<ServiceContextData>() == SERVICE_CONTEXT_DATA_SIZE,
    "ServiceContextData changed size: update BOTH kernel/src/task/mod.rs and      sdk/rust/src/service_context.rs, then update SERVICE_CONTEXT_DATA_SIZE in both"
);


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

/// These wait helpers POLL (`try_recv` + `yield_cpu`); they do not block. That is deliberate, and it is
/// a REVERSAL - they were made to block earlier on this branch, and the change was wrong twice over.
///
/// Why it was tried: a blocked task lets the core reach the scheduler's idle path, which saves power and
/// (on ARM) lets idle-hook work run while a command waits. Both are real benefits.
///
/// Why it is reverted:
/// 1. **It broke x86 networking.** net-stack and nic-driver both sit on core 1, and every exchange
///    between them goes through `request_with_reply_deadline`. With blocking waits, net-stack degraded
///    to "no NIC MAC yet (driver absent/not ready)" and DHCP/ARP never completed; restoring the poll
///    made DHCP, ARP and a sustained 14 ms ping work immediately. Proven by an A/B on real hardware -
///    identical build, this one difference.
/// 2. **It bought nothing that survived.** It was introduced to make USB hot-plug get noticed during a
///    `ping`. That symptom turned out to be the chaos harness pacing itself with a fixed yield count
///    (3000 yields x a full quantum = 30 s on the Pi), not these helpers. The hot-plug fixes that
///    actually worked were the hub change-latch and the catch-up sweep, neither of which needs this.
///
/// It also exposed a genuine kernel bug on the way through - the BSP halting onto a consumed one-shot
/// TSC deadline - which is FIXED and stays fixed independently of this revert.
///
/// The lesson worth keeping: same-core request/reply through a blocking waiter is not exercised by the
/// test suite (identity and fs-restart are all cross-core), so a change here cannot be validated by
/// those suites. If blocking is attempted again, it needs a same-core request/reply test first.

/// Passed by the kernel to `service_main`. Non-Copy; one per service instance.
pub struct ServiceContext {
    _private: (),
}

/// The USB-disk syscalls' "device NAKed, re-ask" status.
///
/// Outside the capability-error range (-2..-7) ON PURPOSE, and this must stay in step with the kernel's
/// `USB_DISK_BUSY` (`kernel/src/syscall/dispatch.rs`). It was originally `-2`, which is `CapNotHeld`, so
/// a driver missing its `USB_DISK` capability was indistinguishable from a busy device and got retried
/// thousands of times before being reported as a device that "stayed busy" - a cap failure wearing an
/// I/O failure's name.
pub const USB_DISK_BUSY: i64 = -20;

/// No USB disk is attached - the opposite instruction to [`USB_DISK_BUSY`], and it must stay in step with
/// the kernel's `USB_DISK_ABSENT`. BUSY means "come back, I am working"; ABSENT means "there is nothing
/// here, and asking again cannot change that - only a hot-plug can". A caller that retries on this waits
/// out its entire budget against an empty socket and then reports the device as busy, which is a false
/// statement about hardware sitting on the operator's desk.
pub const USB_DISK_ABSENT: i64 = -21;

/// Can this LBA survive the syscall ABI intact?
///
/// The ARM ABI gives each syscall argument exactly ONE 32-bit register (`arch/arm/CLAUDE.md`, hazard
/// A-U1), so a wider value is silently narrowed on the way in. That matters here more than anywhere:
/// the kernel guards `lba >= MSC_SECTORS` and then builds a READ(10)/WRITE(10) CDB, but it guards the
/// value it RECEIVED. An LBA of 0x1_0000_0000 arrives as 0, passes the range check, and overwrites the
/// GSFS superblock while the device, `block-driver` and `fs` all report success - a range check
/// defeated one level above itself, and silent corruption of exactly the block the check exists to
/// protect. On x86 the same request is rejected loudly, so the two ports would disagree about what is
/// a valid write.
///
/// Rejecting it in the wrapper is the fix the ABI hazard prescribes (clamp before the syscall, or split
/// into a register pair). Nothing legitimate is refused: capacity comes from SCSI READ CAPACITY(10),
/// whose last-LBA field is 32 bits wide, so an LBA above `u32::MAX` is a caller bug - and it is
/// reported as a failure rather than aliased onto a real block (Invariant 12, failures are loud).
#[inline]
fn lba_fits_syscall_abi(lba: u64) -> bool { lba <= u32::MAX as u64 }

impl ServiceContext {
    /// A handle for the PANIC HANDLER, which has no `ctx` to borrow.
    ///
    /// This type is a zero-sized token - every method reads the kernel-written context block rather
    /// than any field - so making one costs nothing and grants nothing that was not already granted.
    /// It is `pub(crate)` because the panic path is the only caller that legitimately lacks a context:
    /// a service is handed one and must thread it, and an escape hatch for anyone else would let a
    /// caller pretend to authority it was never given.
    pub(crate) fn for_panic() -> Self {
        ServiceContext { _private: () }
    }

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
    ///
    /// **Every failure here now PANICS rather than spinning.** All three exits used to be `loop {}`:
    /// a silent, logless, non-yielding tight spin that pegged the core. That is the worst possible
    /// response to `EndpointDead`, which is not corruption but an ordinary runtime truth (§8.6) - the
    /// service was told its endpoint died and answered by burning a core forever, telling nobody.
    ///
    /// Panicking is now the RIGHT answer because the panic handler was fixed in the same pass: it
    /// logs and faults, so the kernel kills the task, bumps its endpoint generation, wakes any peer
    /// blocked in `call` with `ReplyDead`, and the supervisor restarts it. Loud, and recovered.
    ///
    /// A service that wants to HANDLE the error instead of dying should call `recv_result`.
    pub fn recv(&self) -> Message {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC {
            panic!("recv: corrupt ServiceContext (bad magic) - the kernel handed us an unusable context");
        }
        let slot = data.recv_slot;
        if slot == u32::MAX {
            panic!("recv: no receive endpoint - this service has no ipc_receive in its contract");
        }
        match crate::ipc::recv(CapHandle(slot)) {
            Ok(msg) => msg,
            Err(e)  => panic!("recv failed: {:?} - endpoint is gone; dying so the supervisor restarts us", e),
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
    /// `#[inline(always)]` for the same reason as `await_slice`: it returns a 4 KiB `Message` by
    /// value, so as a separate frame it costs 4 KiB of stack on every caller.
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

    /// Transmit one raw ethernet frame via the in-kernel USB-net device (the ARM DWC2 CDC-ECM bridge).
    /// Gated by the NET_DEVICE cap (the ARM `nic-driver` holds it). Returns true if it was sent. Both
    /// args are pointer + length, so they fit the 32-bit ABI without truncation. Inert on non-ARM.
    #[must_use = "the frame is NOT on the wire if this is false"]
    pub fn net_frame_tx(&self, frame: &[u8]) -> bool {
        // SAFETY: syscall(42) = NetFrameTx; the kernel range-checks (ptr, len) before copying.
        unsafe { raw_syscall(42, frame.as_ptr() as u64, frame.len() as u64, 0) >= 0 }
    }

    /// Receive one raw ethernet frame into `dst` (a single bulk-IN poll). Returns the frame length, or 0
    /// if none is available. Gated by the NET_DEVICE cap.
    pub fn net_frame_rx(&self, dst: &mut [u8]) -> usize {
        // SAFETY: syscall(43) = NetFrameRx; the kernel range-checks (ptr, len) before writing.
        let r = unsafe { raw_syscall(43, dst.as_mut_ptr() as u64, dst.len() as u64, 0) };
        if r > 0 { r as usize } else { 0 }
    }

    /// Query the USB-net device: writes `[mac(6), link(1)]` (7 bytes) into `out`. Returns true if a net
    /// device is up. Gated by the NET_DEVICE cap.
    pub fn net_info(&self, out: &mut [u8; 7]) -> bool {
        // SAFETY: syscall(44) = NetInfo; the kernel writes exactly 7 range-checked bytes.
        unsafe { raw_syscall(44, out.as_mut_ptr() as u64, 0, 0) == 1 }
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

    /// Acquire a fresh SEND cap to `peer` via the kernel name directory.
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
    /// Used to hand out many copies of a held endpoint cap: derive a copy per
    /// recipient and grant that copy away (via `send_with_cap_by_handle`) while
    /// keeping the original. Returns `None` if the cap lacks GRANT, is stale, or the
    /// cap table is full.
    pub fn derive_cap(&self, held: CapHandle) -> Option<CapHandle> {
        // SAFETY: syscall(29) = DeriveCap; `held.0` is a slot index into this task's
        // own cap table. The kernel validates GRANT + generation before duplicating.
        let ret = unsafe { raw_syscall(29, held.0 as u64, 0, 0) };
        if ret < 0 { None } else { Some(CapHandle(ret as u32)) }
    }

    /// Return the probe mode written by the kernel at spawn (0 for all production services).
    pub fn probe_mode(&self) -> u32 { Self::ctx().probe_mode }

    /// Return the recv cap handle for direct-handle use (e.g. wrong-right test probing).
    /// The REPLY mailbox: `(recv, grant)` for the endpoint that exists only to receive replies, or
    /// `None` on a task that has none (then the caller uses the shared endpoint, as before).
    ///
    /// Awaiting a reply on the endpoint you also SERVE means you cannot drain client traffic while
    /// you wait; the queue fills and your own reply is dropped. Replies come here instead, where
    /// nothing else is ever sent.
    fn reply_mailbox(&self) -> Option<(crate::capability::CapHandle, crate::capability::CapHandle)> {
        let d = Self::ctx();
        if d.reply_recv_slot == u32::MAX || d.reply_grant_slot == u32::MAX { return None; }
        Some((crate::capability::CapHandle(d.reply_recv_slot),
              crate::capability::CapHandle(d.reply_grant_slot)))
    }

    pub fn recv_handle(&self) -> Option<crate::capability::CapHandle> {
        let slot = Self::ctx().recv_slot;
        if slot == u32::MAX { None } else { Some(crate::capability::CapHandle(slot)) }
    }

    /// Send a request to a named `peer` and block for its reply (synchronous
    /// request/response). Embeds a per-request reply cap - a `SEND|GRANT` copy of
    /// this service's own endpoint cap - so the server can reply via
    /// `take_pending_cap()` + `send_by_handle()` (the request/reply pattern, §8). The
    /// caller must own an endpoint and not have other traffic racing the reply.
    /// `None` if the peer is unknown, the cap cannot be derived, or the call fails.
    ///
    /// **Waits on truth, not on time (Commandment VIII).** The wait for the reply is a synchronous
    /// kernel CALL (syscall 41): if `peer` dies *after* receiving the request but *before* replying,
    /// the kernel wakes this caller with `ReplyDead` (the reply-side twin of `EndpointDead`, §8.6)
    /// instead of hanging it forever - no timer, no fixed yield count. On any failure (send failed,
    /// or `ReplyDead`) this returns `None`, so a caller like `fs`'s `block_rpc` reacquires the peer
    /// by name and retries exactly as it does for a failed send.
    /// Emit one trace event, or do nothing at all.
    ///
    /// The whole cost when this service is not tracing is the `enabled()` load and a not-taken
    /// branch. When it IS tracing, the send is `try_send` and its result is DISCARDED on purpose: an
    /// observer must never be able to slow, block, or fail the thing it observes. A full logger queue
    /// loses one event, which the ring counts and `trace status` reports - a visible loss, not a
    /// silent one (invariant 12).
    ///
    /// Recursion is cut at the source: `logger` itself holds no `logger` send cap, so the sink cannot trace
    /// its own sends, and no event can beget another.
    #[inline]
    /// Declare this service's own name for the trace ring, once, at startup.
    ///
    /// A service cannot ask what it is called - identity is not ambient - so a traced service says.
    /// Costs nothing in trust: the whole event is already the emitter's testimony (see `crate::trace`).
    /// A service that never calls this still traces; its events read `?` in the caller column, which
    /// is the honest answer rather than a guess.
    pub fn trace_as(&self, name: &str) { crate::trace::set_caller(name); }

    /// Declare that requests to `peer` carry their opcode at byte `at` (default 0).
    ///
    /// For protocols that prepend a correlation tag - `shell -> fs` and `fs -> block-driver` both do -
    /// byte 0 is a request id, so a trace that recorded it was showing noise in a column labelled
    /// "op". The SDK is generic and cannot know; the service that owns the protocol can. See
    /// `crate::trace::set_op_offset`.
    pub fn trace_op_at(&self, peer: &str, at: u8) { crate::trace::set_op_offset(peer, at); }

    fn trace_emit(&self, peer: &str, op: u8, kind: u8) {
        // Arm lazily, ONCE. A service is handed a context with no init hook to run in, so there is
        // nowhere else to resolve from. A service whose contract does not grant `ipc_send =
        // ["logger"]` records `u32::MAX` here and never looks again: one relaxed load per call for the
        // rest of its life, which is the cost the untraced majority pays.
        if !crate::trace::resolved() {
            crate::trace::set_sink_slot(self.find_send_slot(crate::trace::SINK_NAME).unwrap_or(u32::MAX));
        }
        let slot = crate::trace::sink_slot();
        if slot == u32::MAX {
            return;
        }
        // Do NOT record a call to the SINK ITSELF. The `trace` utility reads the ring by asking
        // `logger` for it, so tracing that call would fill the ring with the reader's own questions -
        // each dump adding two more events, pushing out the traffic the reader came to see. An
        // instrument that mostly measures itself is not measuring anything. This hides nothing: every
        // OTHER peer is still recorded, and `trace status` still counts every event accepted.
        if peer == crate::trace::SINK_NAME {
            return;
        }
        // NO CLOCK READ HERE. `epoch_secs_monotonic` is a full CMOS RTC read: `wait_update_clear`
        // spins until the update-in-progress flag clears (up to ~1 ms), then seven port-I/O reads,
        // repeated until two agree. Two of those per request/reply turned `ls`, `move`, `find` and tab
        // completion into TIMEOUTS, and cost the shell so much time inside the kernel that it stopped
        // draining the console and lost Enter keystrokes. Measured, not guessed: `osdev test files`
        // was 222/0 before, 213/9 with the clock read, 222/0 again without it.
        //
        // The SINK stamps the event instead. `logger` has to wake to receive it either way, so the
        // cost lands on the service whose job this is rather than on every service that talks to
        // anyone - and the caller already used `try_send` and did not wait. An observer must not be
        // able to slow the thing it observes, and a millisecond of port I/O per IPC is exactly that.
        let ev = crate::trace::encode(peer, op, kind);
        if self.try_send_by_handle(CapHandle(slot), &crate::ipc::Message::from_bytes(&ev)).is_err() {
            // THE SINK RESTARTED - REACQUIRE IT. The slot is resolved once and cached, so a `logger`
            // respawn left every emitter holding a stale generation FOREVER: tracing silently stopped,
            // `trace ipc` could not reach the ring, and each emission logged a kernel gen-mismatch.
            // Seen on hardware after a chaos storm restarted `logger` forty times - cap generation 985
            // against a live record of 1025.
            //
            // This is the ordinary reacquire-by-name recovery every client owes a restartable peer
            // (14.3); the trace path just never did it. Only on FAILURE, so the healthy path is
            // unchanged, and if the sink is genuinely down the next emission tries again.
            if self.reacquire_by_name(crate::trace::SINK_NAME) {
                if let Some(fresh) = self.find_send_slot(crate::trace::SINK_NAME) {
                    crate::trace::set_sink_slot(fresh);
                }
            }
        }
    }

    // ---------------------------------------------------------------------------
    // The request/reply family: each wraps its `_inner` implementation with one
    // trace event on the way in and one on the way out.
    // ---------------------------------------------------------------------------
    //
    // THESE ARE WRAPPERS AND NOT EDITS TO EACH BODY, deliberately. There are eight of these, each an
    // independent implementation with several early returns inside a wait loop; instrumenting the
    // bodies means finding every exit in eight functions and getting all of them right, forever. A
    // wrapper cannot miss one - the value returned IS the outcome.
    //
    // Instrumenting them ALL is also deliberate. The first cut traced only `request_with_reply`, and
    // on hardware that produced an EMPTY ring while the shell was busily talking to `fs` - because the
    // shell uses `_abortable` and `_deadline`, not the plain one. Partial instrumentation is worse
    // than none: `trace ipc` then shows SOME traffic and silently omits the rest, with nothing on
    // screen saying which. That is a silent gap in the instrument, which is the exact failure this
    // ring exists to prevent (26.4, invariant 12).
    //
    // `op` is byte 0 of OUR OWN message. A service is entitled to interpret its own protocol; the
    // kernel is not, and that asymmetry is the whole reason this lives here rather than at the
    // routing point (4.4, 26.10).

    // `#[inline]` on every wrapper below is LOAD-BEARING, not decoration. `Message` is 4 KiB by
    // value, so an un-inlined wrapper adds a whole extra payload move to each request - and the shell
    // paths that do many round trips (tab completion listing a directory, `move`, `find` descending)
    // went from passing to timing out on it. Measured, not assumed: `osdev test files` was 222/0
    // before the wrappers, 213/9 after, and 213/9 again with tracing DISABLED - so the cost was the
    // restructuring, never the emission.
    #[inline]
    fn trace_out(&self, peer: &str, op: u8, kind: u8) { self.trace_emit(peer, op, kind); }

    /// The op byte of an outgoing request. NO EVENT IS EMITTED HERE.
    ///
    /// One event per exchange, recording how it ENDED, not two recording that it started and how it
    /// ended. Two doubled the traffic through the sink's single 16-deep endpoint, and the sink drains
    /// on its own core's schedule - so the flood crowded out the READER's own request and `trace
    /// status` reported a service that was alive and busy as "unavailable". Every exchange still
    /// produces exactly one event carrying its fate (REPLY / TIMEOUT / PEER_LOST / QUEUE_FULL /
    /// ABORTED), so nothing is lost but the duplicate.
    ///
    /// A request still IN FLIGHT is therefore not in the ring - and that is the one question the ring
    /// was never the right instrument for: `trace blocked` reads it from the kernel, live, which is
    /// what a hang needs (`utilities/46_trace.md` mechanism A).
    #[inline]
    fn trace_in(&self, peer: &str, msg: &crate::ipc::Message) -> u8 {
        // The opcode's byte is the PEER'S protocol's business, not a fixed 0 - see `trace_op_at`.
        let at = crate::trace::op_offset(peer);
        msg.payload_bytes().get(at).copied().unwrap_or(0)
    }

    /// Synchronous request/reply on the caller own endpoint. Blocks until the reply arrives, or until
    /// the kernel wakes it with `ReplyDead` because the replier died (8.6) - it never hangs.
    #[inline]
    pub fn request_with_reply(&self, peer: &str, msg: &crate::ipc::Message)
        -> Option<crate::ipc::Message>
    {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_inner(peer, msg);
        // No deadline on this variant, so `None` is always a lost peer, never a timeout.
        self.trace_out(peer, op, if out.is_some() { crate::trace::KIND_REPLY }
                                 else { crate::trace::KIND_PEER_LOST });
        out
    }

    /// `request_with_reply_call`, saying WHY it failed instead of collapsing everything to `None`.
    #[inline]
    pub fn request_with_reply_call_err(&self, peer: &str, msg: &crate::ipc::Message, max_secs: i64)
        -> Result<Option<crate::ipc::Message>, crate::ipc::IpcError>
    {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_call_err_inner(peer, msg, max_secs);
        self.trace_out(peer, op, match &out {
            Ok(Some(_)) => crate::trace::KIND_REPLY,
            Ok(None)    => crate::trace::KIND_TIMEOUT,
            Err(_)      => crate::trace::KIND_PEER_LOST,
        });
        out
    }

    /// Bounded request/reply that decodes straight into the caller buffer.
    ///
    /// This variant collapses "the deadline passed" and "the send never left" into one `None`, so the
    /// ring records both as TIMEOUT. Where that difference matters the caller should be using
    /// `_call_err` or `_deadline_outcome`, which keep it - and so does the ring.
    #[inline]
    pub fn request_with_reply_deadline_into(
        &self, peer: &str, req: &[u8], buf: &mut [u8], max_secs: i64,
    ) -> Option<usize> {
        let op = req.get(crate::trace::op_offset(peer)).copied().unwrap_or(0);
        let out = self.request_with_reply_deadline_into_inner(peer, req, buf, max_secs);
        self.trace_out(peer, op, if out.is_some() { crate::trace::KIND_REPLY }
                                 else { crate::trace::KIND_TIMEOUT });
        out
    }

    /// Bounded request/reply. `None` is "no answer within the deadline" (see `_into` on the collapse).
    #[inline]
    pub fn request_with_reply_deadline(
        &self, peer: &str, msg: &crate::ipc::Message, max_secs: i64,
    ) -> Option<crate::ipc::Message> {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_deadline_inner(peer, msg, max_secs);
        self.trace_out(peer, op, if out.is_some() { crate::trace::KIND_REPLY }
                                 else { crate::trace::KIND_TIMEOUT });
        out
    }

    /// `request_with_reply_deadline` with a millisecond budget.
    #[inline]
    pub fn request_with_reply_ms(
        &self, peer: &str, msg: &crate::ipc::Message, max_ms: u64,
    ) -> Option<crate::ipc::Message> {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_ms_inner(peer, msg, max_ms);
        self.trace_out(peer, op, if out.is_some() { crate::trace::KIND_REPLY }
                                 else { crate::trace::KIND_TIMEOUT });
        out
    }

    /// Bounded request/reply that distinguishes every way it can fail.
    #[inline]
    pub fn request_with_reply_deadline_outcome(
        &self, peer: &str, msg: &crate::ipc::Message, max_secs: i64,
    ) -> DeadlineOutcome {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_deadline_outcome_inner(peer, msg, max_secs);
        self.trace_out(peer, op, match &out {
            DeadlineOutcome::Reply(_)   => crate::trace::KIND_REPLY,
            DeadlineOutcome::SendFailed => crate::trace::KIND_PEER_LOST,
            DeadlineOutcome::QueueFull  => crate::trace::KIND_QUEUE_FULL,
            DeadlineOutcome::Timeout    => crate::trace::KIND_TIMEOUT,
        });
        out
    }

    /// Bounded request/reply the user can abandon with `q`.
    #[inline]
    pub fn request_with_reply_abortable(
        &self, peer: &str, msg: &crate::ipc::Message, max_secs: i64,
    ) -> ReqOutcome {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_abortable_inner(peer, msg, max_secs);
        self.trace_out(peer, op, match &out {
            ReqOutcome::Reply(_) => crate::trace::KIND_REPLY,
            ReqOutcome::Aborted  => crate::trace::KIND_ABORTED,
            ReqOutcome::Timeout  => crate::trace::KIND_TIMEOUT,
        });
        out
    }

    /// `request_with_reply_abortable`, plus a callback when the wait starts to linger.
    #[inline]
    pub fn request_with_reply_qhint(
        &self, peer: &str, msg: &crate::ipc::Message, hint_after_secs: i64, max_secs: i64,
        on_linger: impl FnOnce(),
    ) -> ReqOutcome {
        let op = self.trace_in(peer, msg);
        let out = self.request_with_reply_qhint_inner(peer, msg, hint_after_secs, max_secs, on_linger);
        self.trace_out(peer, op, match &out {
            ReqOutcome::Reply(_) => crate::trace::KIND_REPLY,
            ReqOutcome::Aborted  => crate::trace::KIND_ABORTED,
            ReqOutcome::Timeout  => crate::trace::KIND_TIMEOUT,
        });
        out
    }



    fn request_with_reply_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
    ) -> Option<crate::ipc::Message> {
        let target = CapHandle(self.find_send_slot(peer)?);
        let self_grant = self.self_grant_handle()?;
        let reply_cap = self.derive_cap(self_grant)?;
        let recv = self.recv_handle()?;
        match crate::ipc::call(target, reply_cap, recv, msg) {
            Ok(reply) => Some(reply),
            Err(_) => {
                // Send failed (dead endpoint) or the peer died before replying (ReplyDead): the
                // embedded reply cap may not have been transferred, so reclaim it (remove_cap is
                // idempotent if the kernel already moved it out on a successful send). Without this, a
                // storm of failed calls would leak reply caps until the table fills and every request
                // returns None.
                self.remove_cap(reply_cap);
                None
            }
        }
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
    /// **Costs one message-sized stack frame, not two.**
    ///
    /// This used to be a thin wrapper over `request_with_reply_deadline_outcome`, and that made it
    /// unusable on a tight stack: `DeadlineOutcome` CARRIES a `Message`, so the enum is a second
    /// 4 KiB temporary on top of the `Option<Message>` returned. `fs` already sits near its 256 KiB
    /// user stack, and switching its block RPC to this wrapper produced an instant data abort at
    /// startup and an 826-deep restart loop on real hardware - the machine unusable, from a change
    /// whose whole purpose was to make a hang impossible.
    ///
    /// So the wait is written out here instead of borrowed. It is the same loop, minus the enum: a
    /// caller that does not need to distinguish "the send never left" from "the peer was silent"
    /// should not pay a message-sized frame for the distinction. Callers that DO need it still have
    /// `..._outcome`, and should keep an eye on their own stack.
    #[inline(never)]
    /// A mark of how deep the stack currently is, in bytes below the first mark taken.
    ///
    /// THE INSTRUMENT THAT WAS MISSING. Five attempts were made to fit correlation tags into `fs`
    /// without ever measuring the frame they had to fit in; each removed one 4 KiB item, the overflow
    /// returned at a different pc, and QEMU - which has no disk, so `fs` never mounts and never makes
    /// the deep calls - reported every build clean. A change to stack usage that cannot be measured
    /// can only be guessed at, and five guesses is the evidence.
    ///
    /// Safe Rust and no kernel involvement: the address of a local IS the stack pointer, near enough.
    /// Relative to a mark taken at service start, so it needs no knowledge of where the stack lives.
    pub fn stack_mark(&self) -> usize {
        let probe = 0u8;
        &probe as *const u8 as usize
    }

    /// Bytes of stack used at this point, without needing a reference mark.
    ///
    /// A mark taken at service start only works if the report is taken at the DEEPEST point, and the
    /// first attempt took it after `mount` had returned - so it measured a popped frame and printed
    /// zero. Absolute is harder to misuse: it can be called from anywhere and still means the same
    /// thing.
    ///
    /// The stack is one region, 256 KiB, and top-aligned, so rounding the current pointer UP to the
    /// next 256 KiB boundary lands on its top and the difference is what has been used. That derives
    /// the top from the alignment the kernel already guarantees rather than restating the address here,
    /// which would be a second copy of a layout decision (Commandment III).
    pub fn stack_used(&self) -> usize {
        const REGION: usize = 256 * 1024;
        let sp = self.stack_mark();
        let top = (sp + (REGION - 1)) & !(REGION - 1);
        top - sp
    }

    /// Send `req` to `peer` and receive the reply into `buf`, returning its length.
    ///
    /// The by-value path costs 4 KiB for the request `Message`, 4 KiB for the reply `Message`, and
    /// 4 KiB again at every wrapper that forwards it. This costs none of that: the caller owns both
    /// buffers, and only a length is returned.
    ///
    /// Same deadline discipline as `request_with_reply_deadline` - block on the endpoint in slices,
    /// give up when the caller's time is spent, and reclaim the reply cap either way (§8.5).
    /// Send `req` to `peer` and receive the reply into `buf`, bounded by `max_secs`.
    ///
    /// **This used to be send + `recv_timeout_into`, and that was wrong in a way that cost days.**
    /// `recv_timeout_into` takes whatever is next on the endpoint, and a service that SERVES clients on
    /// the same endpoint it awaits replies on would receive an unrelated client request, fail to match
    /// it, and drop it - losing that request outright while its own reply arrived later as an orphan
    /// that desynced the next exchange. `fs` does exactly that, which is why its clients hung and its
    /// block protocol "lost step" on both boards.
    ///
    /// It now uses `CallDeadline`, which dequeues the REPLY specifically (the kernel matches it to the
    /// reply cap) and leaves everything else queued. The correct primitive, bounded.
    /// A bounded request/reply through the KERNEL's `CallDeadline`, returning the reply as a message.
    ///
    /// The same primitive `request_with_reply_deadline_into` already uses, in the shape the other
    /// request helpers have, so a caller can move to it without restructuring its buffers.
    ///
    /// Use this rather than `request_with_reply_deadline*` when the caller SERVES clients on the same
    /// endpoint it awaits replies on. Those helpers wait with a plain `recv`, which takes whatever is
    /// next - so under client load the peer's reply can be crowded out of a 16-deep queue and the
    /// caller reports a timeout that blames the peer for something it did correctly. `net-stack` hit
    /// exactly that: the wire showed 7 DHCP REQUESTs sent and 7 ACKs returned while the log said
    /// "6 of 6 REQUESTs never left the host - the driver refused them".
    ///
    /// `CallDeadline` does not have that failure: `call_dequeue` matches the reply BY ITS SENDER and
    /// leaves every other message queued, which is the entire reason §8.2 added it - hand-rolling the
    /// bounded wait out of send + recv is what lost messages. No new kernel surface: syscall 50 and
    /// its semantics are existing, ratified law.
    pub fn request_with_reply_call(&self, peer: &str, msg: &crate::ipc::Message, max_secs: i64)
        -> Option<crate::ipc::Message>
    {
        self.request_with_reply_call_err(peer, msg, max_secs).ok().flatten()
    }

    /// `request_with_reply_call`, but saying WHY it failed instead of collapsing everything to None.
    ///
    /// Exists because "no answer" is not a diagnosis. A caller that turns every failure into one word
    /// then reports it as the PEER's fault - `net-stack` blamed the driver for refusing frames the wire
    /// showed it had sent - and there is no way to tell a send that never left from a reply that never
    /// came. `Ok(None)` is the deadline passing; `Err(e)` is the send failing, and `e` says how.
    fn request_with_reply_call_err_inner(&self, peer: &str, msg: &crate::ipc::Message, max_secs: i64)
        -> Result<Option<crate::ipc::Message>, crate::ipc::IpcError>
    {
        let target = match self.find_send_slot(peer) {
            Some(s) => CapHandle(s),
            None    => return Err(crate::ipc::IpcError::CapError(crate::capability::CapError::CapNotHeld)),
        };
        let (recv, grant) = match self.reply_mailbox() {
            Some((r, g)) => (r, g),
            None => match (self.recv_handle(), self.self_grant_handle()) {
                (Some(r), Some(g)) => (r, g),
                _ => return Err(crate::ipc::IpcError::CapError(crate::capability::CapError::CapNotHeld)),
            },
        };
        let reply_cap = match self.derive_cap(grant) {
            Some(c) => c,
            None    => return Err(crate::ipc::IpcError::CapError(crate::capability::CapError::CapNotHeld)),
        };
        let secs = if max_secs <= 0 { 0 } else { max_secs as u64 };
        let mut buf = [0u8; crate::ipc::MAX_PAYLOAD];
        let n = msg.payload_bytes().len().min(buf.len());
        buf[..n].copy_from_slice(&msg.payload_bytes()[..n]);
        match crate::ipc::call_deadline_into(target, reply_cap, recv, &msg.payload_bytes()[..n],
                                             &mut buf, secs) {
            Ok(Some(len)) => Ok(Some(crate::ipc::Message::from_bytes(&buf[..len]))),
            Ok(None)      => { self.remove_cap(reply_cap); Ok(None) }
            Err(e)        => { self.remove_cap(reply_cap); Err(e) }
        }
    }

    fn request_with_reply_deadline_into_inner(
        &self, peer: &str, req: &[u8], buf: &mut [u8], max_secs: i64,
    ) -> Option<usize> {
        let target = CapHandle(self.find_send_slot(peer)?);
        // Reply mailbox when the task has one, shared endpoint when it does not. The reply cap and the
        // endpoint waited on must name the SAME endpoint, or the kernel's reply-matched dequeue waits
        // for something that will never be delivered there.
        let (recv, grant) = match self.reply_mailbox() {
            Some((r, g)) => (r, g),
            None => (self.recv_handle()?, self.self_grant_handle()?),
        };
        let reply_cap = self.derive_cap(grant)?;
        let secs = if max_secs <= 0 { 0 } else { max_secs as u64 };
        let out = crate::ipc::call_deadline_into(target, reply_cap, recv, req, buf, secs);
        // The kernel consumes the reply cap on a delivered call; on any other outcome it is ours to
        // reclaim, or the slot leaks one per failed request (§8.5, the three checks).
        match out {
            Ok(Some(n)) => Some(n),
            Ok(None) => { self.remove_cap(reply_cap); None }
            Err(_)   => { self.remove_cap(reply_cap); None }
        }
    }

    /// How long one blocking wait slice lasts.
    ///
    /// 20 ms: two scheduler quanta, so a waiter never holds its core, and short enough that a caller
    /// checking for a keypress between slices still feels instant to a human. Nothing depends on the
    /// value for correctness - a message arriving mid-slice wakes the task immediately; the slice only
    /// bounds how often a caller gets to look at anything OTHER than its endpoint.
    const AWAIT_SLICE_MS: u64 = 20;

    /// Wait for a message on our own endpoint, BLOCKING, for at most one slice.
    ///
    /// The whole point of this helper, and the reason it exists rather than a `try_recv` loop: a task
    /// that spins while waiting is not idle, it is COMPETING with whatever it is waiting for. On this
    /// system that is not an abstract cost - `net-stack` waiting for a DHCP reply spun through the SDK
    /// and through its own `drain_scan`, two nested loops, and saturated `nic-driver` and the USB
    /// driver that had to fetch the very reply it was waiting for. The waiting starved the answering.
    ///
    /// Pinning the services to different cores hid it and fixed nothing: on a single-core machine the
    /// spinner simply consumes the whole system. A blocked task consumes nothing and is woken the
    /// instant a message lands (`RecvTimeout`, §12), so this is correct at any core count - which is
    /// the only kind of correct worth having.
    ///
    /// Sliced rather than one long block so callers that must also watch something else - a keypress,
    /// an abort flag - get the chance, without any of them going back to spinning.
    ///
    /// `#[inline(always)]`, and that attribute is load-bearing rather than an optimisation hint. A
    /// `Message` is 4096 bytes BY VALUE, so a wrapper that returns one is not free: it is a whole
    /// extra 4 KiB stack frame on every request in every service. Introducing this helper cost `fs`
    /// exactly that, and `fs` was already the service closest to its 256 KiB limit - it began taking a
    /// data abort at `mount`, with SP below the bottom of its stack region.
    ///
    /// It only showed on hardware. QEMU's raspi2b has no disk, so `fs` never mounts and never makes
    /// the deep block calls, and thirty-second QEMU boots kept coming back clean while the board
    /// restart-looped. A one-line wrapper around a large return value has to be inlined or it must not
    /// exist.
    fn await_slice(&self, ms: u64) -> Option<crate::ipc::Message> {
        self.recv_timeout(self.duration_cycles(ms))
    }

    /// **Shares the flaw `request_with_reply_deadline_into` was just fixed for, and is NOT fixed here.**
    /// It sends with a reply cap and then receives generically, so a caller that also SERVES on its
    /// endpoint can consume an unrelated message and drop it. `_into` moved to `CallDeadline`, which
    /// dequeues the reply specifically; this one has not, because nothing has yet been shown to be
    /// harmed by it and a blind sweep of four helpers is how a fix becomes a regression.
    ///
    /// Note that `_abortable` and `_qhint` below CANNOT simply follow: they interleave on purpose, to
    /// notice a `q` keypress while waiting. Making them dequeue only the reply would delete that. If
    /// they need this too, the answer is a bounded stash, not a substitution.
    /// Offer a request to `target`, riding out a FULL peer queue instead of calling it a failure.
    ///
    /// A full queue and an unreachable peer are different things, and reporting them the same way is
    /// what desynced the shell against `fs` on x86. The caller's answer to "send failed" is to
    /// reacquire the peer by name and RE-SEND with a fresh tag - correct when a restart made the cap
    /// stale, and actively harmful when the queue was merely full: the original request is still in
    /// flight, so its reply arrives as an orphan and every request afterwards collects the PREVIOUS
    /// one's reply. The log showed exactly that, permanently one behind:
    ///
    ///     shell: discarded an fs reply for tag 68 while awaiting 69
    ///     shell: discarded an fs reply for tag 72 while awaiting 73
    ///
    /// Congestion clears as soon as the peer is scheduled, so the fix is to wait a moment and offer
    /// the SAME request again - never a second one. Bounded, and a queue still full after that is
    /// reported honestly to the caller.
    fn offer_request(
        &self, target: CapHandle, reply_cap: CapHandle, msg: &crate::ipc::Message,
    ) -> Result<(), crate::ipc::IpcError> {
        const BUSY_MS: u64 = 2;
        const BUSY_TRIES: u32 = 8;
        let mut last = crate::ipc::IpcError::QueueFull;
        for _ in 0..BUSY_TRIES {
            match self.send_with_cap_by_handle(target, reply_cap, msg) {
                Ok(()) => return Ok(()),
                Err(crate::ipc::IpcError::QueueFull) => {
                    last = crate::ipc::IpcError::QueueFull;
                    self.sleep(self.duration_cycles(BUSY_MS));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    fn request_with_reply_deadline_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        max_secs: i64,
    ) -> Option<crate::ipc::Message> {
        let target = CapHandle(self.find_send_slot(peer)?);
        let self_grant = self.self_grant_handle()?;
        let reply_cap = self.derive_cap(self_grant)?;
        let recv = self.recv_handle()?;
        if self.offer_request(target, reply_cap, msg).is_err() {
            // The send never left, so the embedded cap was not transferred - reclaim it or a storm of
            // failures leaks the table (§8.5).
            self.remove_cap(reply_cap);
            return None;
        }
        let t0 = self.epoch_secs_monotonic();
        loop {
            // BLOCK, do not spin. See `await_slice`.
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS) {
                return Some(r);
            }
            let now = self.epoch_secs_monotonic();
            // Guard the clock going BACKWARDS as well as forwards: on hardware whose counter is
            // unreliable, `now - t0` can read huge and expire the deadline on the first pass.
            if now >= t0 && now - t0 >= max_secs {
                // Abandoned: the reply may still arrive later and sit in our queue, which is the
                // hazard `..._outcome` documents. Reclaim the cap; the caller reports the failure.
                self.remove_cap(reply_cap);
                let _ = recv;
                return None;
            }
        }
    }

    /// `request_with_reply_deadline`, bounded in MILLISECONDS instead of whole seconds.
    ///
    /// Exists because a sub-second window cannot be built out of a whole-second bound, and one was
    /// being attempted. `net-stack`'s ping window is ~900 ms and cycle-based, but the drain inside it
    /// was bounded by `LINK_SECS = 1` on `epoch_secs_monotonic`, whose resolution IS one second - so a
    /// single drain could legitimately outlast the entire window. Measured on a Pi 4: the window
    /// reported `closed after 1017663 us [budget 900000 us]` with `0 drains`, meaning it completed
    /// exactly ONE call and that call overran the whole budget. Ping sends once a second, so a window
    /// that runs 1.018 s misses roughly every third reply - which read as "the network is flaky" and
    /// was arithmetic.
    ///
    /// Bounded by the CYCLE counter, deliberately, because that is the only clock here with sub-second
    /// resolution and it is the same clock the caller's own window uses - a bound and the budget it
    /// must fit inside should not be measured by different clocks. `duration_cycles` already floors an
    /// uncalibrated counter to one quantum, so a board with no calibration degrades to "return almost
    /// at once" rather than to "wait forever".
    ///
    /// No new kernel surface: this is the same userspace poll the seconds variants use, with a
    /// different clock read in the deadline test.
    fn request_with_reply_ms_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        max_ms: u64,
    ) -> Option<crate::ipc::Message> {
        let target = CapHandle(self.find_send_slot(peer)?);
        let self_grant = self.self_grant_handle()?;
        let reply_cap = self.derive_cap(self_grant)?;
        let recv = self.recv_handle()?;
        if self.offer_request(target, reply_cap, msg).is_err() {
            self.remove_cap(reply_cap);
            return None;
        }
        let t0 = self.read_tsc();
        let budget = self.duration_cycles(max_ms);
        loop {
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS.min(max_ms.max(1))) {
                return Some(r);
            }
            // wrapping_sub, so a counter that wraps mid-wait reads as a small elapsed rather than as
            // an enormous one that expires the deadline instantly.
            if self.read_tsc().wrapping_sub(t0) >= budget {
                self.remove_cap(reply_cap);
                let _ = recv;
                return None;
            }
        }
    }

    /// Like [`Self::request_with_reply_deadline`] but returns the DISTINCTION between a send that never
    /// left (stale/unresolvable peer cap) and a peer that received the request but stayed silent past
    /// the deadline (see [`DeadlineOutcome`]). Same bounded, RTC-deadline wait; use this when a caller
    /// must reacquire+retry a *restarted* peer (`SendFailed`) but must NOT double the wait on a merely
    /// *silent* one (`Timeout`).
    fn request_with_reply_deadline_outcome_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        max_secs: i64,
    ) -> DeadlineOutcome {
        let target = match self.find_send_slot(peer) { Some(s) => CapHandle(s), None => return DeadlineOutcome::SendFailed };
        let self_grant = match self.self_grant_handle() { Some(g) => g, None => return DeadlineOutcome::SendFailed };
        let reply_cap = match self.derive_cap(self_grant) { Some(c) => c, None => return DeadlineOutcome::SendFailed };
        if let Err(e) = self.send_with_cap_by_handle(target, reply_cap, msg) {
            self.remove_cap(reply_cap);   // send failed: reclaim the untransferred reply cap (no leak)
            return match e {
                crate::ipc::IpcError::QueueFull => DeadlineOutcome::QueueFull,
                _ => DeadlineOutcome::SendFailed,
            };
        }
        // Deglitched monotonic clock, not the raw RTC: a single CMOS misread (the "4383d" glitch on the
        // T630) would otherwise make `now - t0` read huge and expire the deadline instantly.
        let t0 = self.epoch_secs_monotonic();
        loop {
            // Block, do not spin - and now it actually does. This comment said so while the line
            // beneath it polled, which is how the claim survived so long unexamined.
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS) { return DeadlineOutcome::Reply(r); }
            if self.epoch_secs_monotonic() - t0 >= max_secs {
                self.remove_cap(reply_cap);   // reply never consumed - reclaim its slot
                // CALLER BEWARE: the request was already SENT, so the peer will reply into our endpoint
                // whether we are listening or not. Reclaiming the reply CAP does not remove that message
                // from the queue - the NEXT `try_recv` on this endpoint may return the ABANDONED reply
                // instead of the answer it expects. That has bitten for real: a timed-out fs read left a
                // 1-byte `[FS_NOTFOUND]` behind, and the next command consumed it and reported a healthy
                // mounted disk as absent. `request_with_reply_abortable` avoids this with a DRAIN at its
                // own top; this variant cannot drain blindly, because a service that also SERVES on this
                // endpoint (net-stack) would discard live client requests. So a caller that can time out
                // must reclaim the late reply itself - see the shell's `reclaim_late_fs_reply`.
                return DeadlineOutcome::Timeout;
            }
            self.yield_cpu();
        }
    }

    /// Like [`Self::request_with_reply_deadline`] but ABORTABLE: while waiting it also drains the console
    /// and, on q/Q/ESC, returns [`ReqOutcome::Aborted`] IMMEDIATELY - it does NOT wait for the in-flight
    /// reply (that wait felt like "it pauses instead of quitting"). The request is already sent, so the
    /// peer replies into our endpoint whether we listen or not; that late reply is cleared by the DRAIN at
    /// the top of the *next* abortable request, so it never pollutes a later command (the `net scan` ->
    /// `net` "0.0.0.0 / 00:00 MAC" bug that drain closes). Sends the request ONCE (no re-trigger). Use for
    /// any interactive command that blocks on a peer (the "q to quit" rule, `utilities/0_conventions.md`).
    /// A service with no console foreground never sees input, so this degrades to the plain deadline wait.
    fn request_with_reply_abortable_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        max_secs: i64,
    ) -> ReqOutcome {
        // Drain any stale reply a prior INSTANT-abort left in our endpoint (the peer replied after we
        // stopped listening), so this request cannot read it as its own. Safe for the shell - a client
        // whose endpoint only holds replies; between commands it is otherwise empty.
        while self.try_recv().is_some() {}
        let target = match self.find_send_slot(peer) { Some(s) => CapHandle(s), None => return ReqOutcome::Timeout };
        let self_grant = match self.self_grant_handle() { Some(g) => g, None => return ReqOutcome::Timeout };
        let reply_cap = match self.derive_cap(self_grant) { Some(c) => c, None => return ReqOutcome::Timeout };
        if self.offer_request(target, reply_cap, msg).is_err() {
            self.remove_cap(reply_cap);
            return ReqOutcome::Timeout;
        }
        let t0 = self.epoch_secs_monotonic();
        loop {
            // BLOCK for the reply, with a short timeout - do not spin. `try_recv` + `yield_cpu` kept this
            // task permanently RUNNABLE, so a core running a waiting command never reached the scheduler's
            // idle path at all. On ARM that path is where USB hot-plug is watched, so plugging or
            // unplugging anything during a `ping` was noticed only once the ping ended: the events queued
            // up and all arrived at the prompt. It also pegged the core at 100% for a task that is, in
            // truth, doing nothing (the same busy-wait the `observe` and muted loops were already fixed
            // for - see MUTED_POLL_SLEEP_CYCLES). Blocking parks the task, the core halts, idle work runs.
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS) {
                // DO NOT remove the reply cap on a REPLY. The send already removed it.
                //
                // §8.5: a cap embedded in a message "is transferred and REMOVED from sender's table".
                // The instant the request went out this slot stopped being ours, and by the time the
                // reply lands the kernel has REUSED it - `CapTable::insert` hands out the first EMPTY
                // slot, and this one is empty. When the reply carries an embedded cap (a file cap from
                // `fs`), that cap lands in exactly this slot, so removing "the reply cap" DELETES IT.
                //
                // Measured on the Pi 4: `fcap` printed `file=12 reply=12` - the file handle and the
                // next derived cap were the same slot, which is only possible if the slot was empty.
                // Every file-cap read/write then failed before reaching `fs`, and two negative
                // sub-checks "passed" vacuously because no invoke could get that far.
                //
                // A remove-by-stale-index can bite ANY request whose reply carries a cap, not just
                // fcap. The abort and timeout paths below still remove it: there the send never
                // delivered, so the cap IS still ours.
                return ReqOutcome::Reply(r);
            }
            while let Some(b) = self.try_console_read() {
                // q/Q/ESC aborts IMMEDIATELY - never wait for the in-flight reply (that wait was the
                // "it pauses instead of quitting" complaint). The peer's late reply lands in our endpoint
                // and the drain atop the NEXT abortable request clears it, so it pollutes nothing.
                // "Immediately" still holds: the abort does not wait on the peer. What the block adds is
                // up to one poll interval before the keypress is LOOKED at - tens of milliseconds, under
                // the threshold at which a person can tell, and the same trade the observe loop makes.
                if b == b'q' || b == b'Q' || b == 0x1b { self.remove_cap(reply_cap); return ReqOutcome::Aborted; }
            }
            if self.epoch_secs_monotonic() - t0 >= max_secs {
                self.remove_cap(reply_cap);
                return ReqOutcome::Timeout;
            }
        }
    }

    /// Like [`Self::request_with_reply_abortable`], but if no reply has arrived after
    /// `hint_after_secs` it invokes `on_linger` ONCE (e.g. to print a "(q to quit)" hint) and keeps
    /// waiting/aborting. A snappy reply never fires the hint, so a fast request stays silent and only
    /// a genuinely lingering wait tells the user they can bail. Abort semantics are identical to
    /// `request_with_reply_abortable` (q/Q/ESC -> `Aborted` immediately; the request is sent once and
    /// a late reply is drained atop the next abortable/qhint request). The hint text lives in the
    /// caller's closure, so this stays mechanism - the SDK provides the *timing*, the caller the UX.
    fn request_with_reply_qhint_inner(
        &self,
        peer: &str,
        msg:  &crate::ipc::Message,
        hint_after_secs: i64,
        max_secs: i64,
        on_linger: impl FnOnce(),
    ) -> ReqOutcome {
        // Drain any stale reply a prior INSTANT-abort left in our endpoint (see the abortable variant).
        while self.try_recv().is_some() {}
        let target = match self.find_send_slot(peer) { Some(s) => CapHandle(s), None => return ReqOutcome::Timeout };
        let self_grant = match self.self_grant_handle() { Some(g) => g, None => return ReqOutcome::Timeout };
        let reply_cap = match self.derive_cap(self_grant) { Some(c) => c, None => return ReqOutcome::Timeout };
        if self.offer_request(target, reply_cap, msg).is_err() {
            self.remove_cap(reply_cap);
            return ReqOutcome::Timeout;
        }
        let t0 = self.epoch_secs_monotonic();
        let mut on_linger = Some(on_linger);   // FnOnce, fired at most once when the wait lingers
        loop {
            // Block, do not spin - see `request_with_reply_abortable`. This is the variant `net`/`ping`
            // actually use (the "press q to abort" hint), so it is the one that kept core 0 permanently
            // busy during a continuous ping and starved the idle-path USB hot-plug watch.
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS) {
                // DO NOT remove the reply cap on a REPLY. The send already removed it.
                //
                // §8.5: a cap embedded in a message "is transferred and REMOVED from sender's table".
                // The instant the request went out this slot stopped being ours, and by the time the
                // reply lands the kernel has REUSED it - `CapTable::insert` hands out the first EMPTY
                // slot, and this one is empty. When the reply carries an embedded cap (a file cap from
                // `fs`), that cap lands in exactly this slot, so removing "the reply cap" DELETES IT.
                //
                // Measured on the Pi 4: `fcap` printed `file=12 reply=12` - the file handle and the
                // next derived cap were the same slot, which is only possible if the slot was empty.
                // Every file-cap read/write then failed before reaching `fs`, and two negative
                // sub-checks "passed" vacuously because no invoke could get that far.
                //
                // A remove-by-stale-index can bite ANY request whose reply carries a cap, not just
                // fcap. The abort and timeout paths below still remove it: there the send never
                // delivered, so the cap IS still ours.
                return ReqOutcome::Reply(r);
            }
            while let Some(b) = self.try_console_read() {
                if b == b'q' || b == b'Q' || b == 0x1b { self.remove_cap(reply_cap); return ReqOutcome::Aborted; }
            }
            let elapsed = self.epoch_secs_monotonic() - t0;
            if elapsed >= hint_after_secs {
                if let Some(f) = on_linger.take() { f(); }
            }
            if elapsed >= max_secs {
                self.remove_cap(reply_cap);
                return ReqOutcome::Timeout;
            }
            self.yield_cpu();
        }
    }

    /// Wait for the next message on our own recv endpoint, ABORTABLE (q/Q/ESC) and bounded by an RTC
    /// deadline, WITHOUT sending anything. The failure-aware twin of [`Self::recv`]: where `recv`
    /// blocks forever (and loops on error), this returns [`ReqOutcome::Timeout`] if no message
    /// arrives within `max_secs` and [`ReqOutcome::Aborted`] the instant the user presses q. Use it to
    /// await a reply we already sent by some path OTHER than a named-peer request - a badged
    /// `resource_invoke` (a file/socket capability), or draining a pipe filter's stream - where
    /// [`Self::request_with_reply_abortable`] (which does its own send) does not fit. This is the wait
    /// half of the abortable request, factored out: a peer that received our invocation but died before
    /// replying, or a filter that wedges mid-stream, can no longer hang us (Commandment VIII - wait on
    /// truth *including failure*). The caller owns any reply cap it derived and reclaims it on every
    /// outcome. A service with no console foreground never sees input, so this degrades to a plain
    /// deadline wait. Does NOT drain a stale reply first - a caller that can be re-entered after an
    /// abort should `while self.try_recv().is_some() {}` before it sends (as the request variants do).
    pub fn recv_abortable_deadline(&self, max_secs: i64) -> ReqOutcome {
        let t0 = self.epoch_secs_monotonic();
        loop {
            // Block, do not spin - see `await_slice`.
            if let Some(r) = self.await_slice(Self::AWAIT_SLICE_MS) { return ReqOutcome::Reply(r); }
            while let Some(b) = self.try_console_read() {
                if b == b'q' || b == b'Q' || b == 0x1b { return ReqOutcome::Aborted; }
            }
            if self.epoch_secs_monotonic() - t0 >= max_secs {
                return ReqOutcome::Timeout;
            }
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
    #[must_use = "the peer is NOT reacquired if this is false"]
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
    /// The send cap for a named peer, or `None` if this service has no such peer.
    ///
    /// The public form of what `request_with_reply` resolves internally. Exposed for the case that
    /// helper cannot serve: a request whose reply must NOT be waited for. A single-threaded service that
    /// owns an in-memory answer (the `time` service and its wall clock) has to be able to send a
    /// best-effort request to a slow peer without becoming unable to answer its own clients meanwhile -
    /// so it pairs this with `derive_cap` + `send_with_cap_by_handle` and picks the reply up later, out
    /// of its ordinary receive loop, matched by the protocol's correlation tag.
    pub fn send_peer_handle(&self, peer: &str) -> Option<crate::capability::CapHandle> {
        self.find_send_slot(peer).map(crate::capability::CapHandle)
    }

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

    /// Acquire a fresh SEND|GRANT cap to `peer` via the kernel name directory.
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

    /// Acquire a fresh SEND cap to `peer` via the kernel name directory.
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
    /// One byte from the COM2 operator channel, or `None` when the port is empty (kernel query 21).
    ///
    /// The kernel owns the UART - hardware, and §11.4 already sanctions it owning a serial console -
    /// and hands bytes out; the `control` service owns what they MEAN. Transport in the kernel,
    /// interpretation in a service (C1-6).
    pub fn com2_byte(&self) -> Option<u8> {
        // SAFETY: syscall(13) = InspectKernel; query 21 = pop one COM2 byte, -1 when empty.
        let v = unsafe { raw_syscall(13, 21, 0, 0) };
        if v < 0 { None } else { Some(v as u8) }
    }

    /// The board's own MAC address (kernel query 23), or `None` where the board cannot say.
    ///
    /// A NIC driver runs in userspace and cannot reach the board identity: on the Pi this address lives
    /// in the VideoCore mailbox, nowhere near the controller's register window. Without it a driver has
    /// to invent an address, and an invented address that is hardcoded is the same on every board -
    /// which stops being harmless the moment two of them share a network.
    pub fn board_mac(&self) -> Option<[u8; 6]> {
        // SAFETY: syscall(13) = InspectKernel; query 23 = the board MAC packed little-endian, -1 if none.
        let v = unsafe { raw_syscall(13, 23, 0, 0) };
        if v < 0 { return None; }
        let v = v as u64;
        Some([v as u8, (v >> 8) as u8, (v >> 16) as u8,
              (v >> 24) as u8, (v >> 32) as u8, (v >> 40) as u8])
    }

    /// Inject a test interrupt. Requires the FIRE_IRQ capability, held only by `control`; a non-holder
    /// gets `false` rather than a silent no-op.
    pub fn fire_irq(&self, irq: u8) -> bool {
        // SAFETY: syscall(51) = FireIrq; the kernel validates FIRE_IRQ by holdings.
        unsafe { raw_syscall(51, irq as u64, 0, 0) == 0 }
    }

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

    /// Capacity of the in-kernel USB mass-storage device in 512-byte sectors, 0 if none is attached.
    /// Requires the `USB_DISK` capability. Syscall 46.
    pub fn usb_disk_sectors(&self) -> u64 {
        // SAFETY: syscall(46) = UsbDiskInfo; no arguments, gated by the USB_DISK capability.
        let ret = unsafe { raw_syscall(46, 0, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Read the 512-byte block at `lba` from the USB mass-storage device into `dst`. Returns false if
    /// there is no device, the LBA is past the end, or the transfer failed. Requires `USB_DISK`.
    /// Syscall 47.
    #[must_use = "the destination buffer is NOT valid data if this is false"]
    pub fn usb_disk_read(&self, lba: u64, dst: &mut [u8; 512]) -> bool {
        // SAFETY: syscall(47) = UsbDiskRead; the kernel writes exactly 512 bytes to `dst` on success,
        // through its checked user-pointer path.
        if !lba_fits_syscall_abi(lba) { return false; }
        let ret = unsafe { raw_syscall(47, lba, dst.as_mut_ptr() as u64, 0) };
        ret == 0
    }

    /// Write `src` as the 512-byte block at `lba` on the USB mass-storage device. Requires `USB_DISK`.
    /// Syscall 48.
    #[must_use = "the block did NOT reach the medium if this is false"]
    pub fn usb_disk_write(&self, lba: u64, src: &[u8; 512]) -> bool {
        // SAFETY: syscall(48) = UsbDiskWrite; the kernel reads exactly 512 bytes from `src` through its
        // checked user-pointer path.
        if !lba_fits_syscall_abi(lba) { return false; }
        let ret = unsafe { raw_syscall(48, lba, src.as_ptr() as u64, 0) };
        ret == 0
    }

    /// Read a block, returning the raw status so BUSY is distinguishable from FAILED: `0` ok, the
    /// device is busy (`USB_DISK_BUSY` - re-ask, nothing is wrong), anything else a real error. The `bool` variants
    /// above collapse those two, which is exactly the conflation that made a busy stick look broken.
    pub fn usb_disk_read_status(&self, lba: u64, dst: &mut [u8; 512]) -> i64 {
        // SAFETY: syscall(47) = UsbDiskRead; the kernel writes exactly 512 bytes through its checked
        // user-pointer path.
        if !lba_fits_syscall_abi(lba) { return -1; }
        unsafe { raw_syscall(47, lba, dst.as_mut_ptr() as u64, 0) }
    }

    /// Write a block, returning the raw status (see `usb_disk_read_status`).
    pub fn usb_disk_write_status(&self, lba: u64, src: &[u8; 512]) -> i64 {
        // SAFETY: syscall(48) = UsbDiskWrite; the kernel reads exactly 512 bytes from `src`.
        if !lba_fits_syscall_abi(lba) { return -1; }
        unsafe { raw_syscall(48, lba, src.as_ptr() as u64, 0) }
    }

    /// Make previously written blocks durable on the USB mass-storage device (SCSI SYNCHRONIZE CACHE).
    /// Requires `USB_DISK` WRITE. Syscall 49.
    ///
    /// A write is only ACKNOWLEDGED when `usb_disk_write` returns - the device may still be holding the
    /// bytes in a volatile buffer. Anything that promises durability (a format, a journal commit) has to
    /// ask for it, and check the answer: `false` means the data is NOT known to be on the medium.
    #[must_use = "prior writes are NOT durable if this is false"]
    pub fn usb_disk_flush(&self) -> bool {
        // SAFETY: syscall(49) = UsbDiskFlush; takes no arguments and touches no user memory.
        let ret = unsafe { raw_syscall(49, 0, 0, 0) };
        ret == 0
    }

    /// The SD/EMMC controller's base clock in Hz (0 if the platform does not report one), via
    /// InspectKernel query 20. The block driver needs it to compute its clock divider: on the BCM283x
    /// the controller's own capability register reports the base clock wrongly, and the driver holds
    /// only its controller's registers, so it cannot ask the platform firmware itself. A driver that
    /// gets 0 should report that rather than guess - a wrong divider runs the card's identification
    /// clock at the wrong speed, which fails on hardware and not under emulation. Ungated - task-neutral
    /// hardware info, like the console geometry and the RTC.
    pub fn emmc_base_clock_hz(&self) -> u32 {
        // SAFETY: syscall(13) = InspectKernel; query_id=20 = EMMC base clock (Hz).
        let ret = unsafe { raw_syscall(13, 20, 0, 0) };
        if ret < 0 { 0 } else { ret as u32 }
    }

    /// The discovered NIC's PCI identity as `vendor | device<<16` (0 if no NIC), via InspectKernel
    /// query 14. A NIC driver reads it to know which chip it is driving (e.g. Intel e1000 =
    /// 0x100E_8086 vs Realtek RTL8168 = 0x8168_10EC). Ungated - task-neutral hardware info.
    pub fn nic_vendor_device(&self) -> u32 {
        // SAFETY: syscall(13) = InspectKernel; query_id=14 = NIC vendor|device.
        let ret = unsafe { raw_syscall(13, 14, 0, 0) };
        if ret < 0 { 0 } else { ret as u32 }
    }

    /// Whether the PCI scan found an xHCI USB host controller (InspectKernel query 18, ungated
    /// task-neutral hardware fact). The supervisor reads it to skip spawning the `xhci` driver on a
    /// machine that has no xHCI controller (an idle driver would busy-hold a core). Falls back to
    /// `true` if the query is unavailable, preserving the always-spawn behaviour.
    pub fn xhci_present(&self) -> bool {
        // SAFETY: syscall(13) = InspectKernel; query_id=18 = USB controller presence bitmask.
        let ret = unsafe { raw_syscall(13, 18, 0, 0) };
        if ret < 0 { return true; } // query unavailable - spawn as before
        (ret & 0b01) != 0
    }

    /// Whether the PCI scan found an EHCI (USB 2.0) host controller (InspectKernel query 18, ungated).
    /// The supervisor reads it to skip spawning the `ehci` driver on a machine with no EHCI at all
    /// (e.g. the Wyse 5070), so an idle driver does not busy-hold a core. Falls back to `true` if the
    /// query is unavailable, preserving the always-spawn behaviour.
    pub fn ehci_present(&self) -> bool {
        // SAFETY: syscall(13) = InspectKernel; query_id=18 = USB controller presence bitmask.
        let ret = unsafe { raw_syscall(13, 18, 0, 0) };
        if ret < 0 { return true; } // query unavailable - spawn as before
        (ret & 0b10) != 0
    }

    /// Whether the PCI scan found a NIC this build can actually drive (present AND an e1000 or RTL8168),
    /// via InspectKernel query 18 (ungated). The supervisor reads it to skip spawning `nic-driver` (and
    /// its dependent `net-stack`) on a machine with no usable NIC, so they do not busy-hold cores.
    /// Falls back to `true` if the query is unavailable, preserving the always-spawn behaviour.
    pub fn nic_present(&self) -> bool {
        // SAFETY: syscall(13) = InspectKernel; query_id=18 = hardware-driver presence bitmask.
        let ret = unsafe { raw_syscall(13, 18, 0, 0) };
        if ret < 0 { return true; } // query unavailable - spawn as before
        (ret & 0b100) != 0
    }

    /// The NIC's register-space MMIO base (the BAR the PCI scan chose), 0 if none. InspectKernel query
    /// 15, ungated. A diagnostic - a driver reads it to confirm which BAR it was handed.
    pub fn nic_mmio_base(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=15 = NIC MMIO base.
        let ret = unsafe { raw_syscall(13, 15, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
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

    /// The endpoint task `slot` OWNS, or 0 if it owns none (InspectKernel query 24, INTROSPECT).
    ///
    /// With [`Self::task_awaits_endpoint`] this is the whole of the blocked-chain walk: map the
    /// endpoint a stuck task awaits back to the task that owns it, then ask what THAT one awaits.
    /// See `utilities/46_trace.md`.
    pub fn task_own_endpoint(&self, slot: u32) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=24 = the task's own endpoint.
        let ret = unsafe { raw_syscall(13, 24, slot as u64, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// The endpoint task `slot` is blocked-in-CALL awaiting a reply from, or 0 if no call is in
    /// flight (InspectKernel query 25, INTROSPECT).
    ///
    /// A best-effort snapshot, on the same contract as [`Self::task_stat`]: the kernel reads it
    /// without taking the routing lock, so an observer cannot stall the thing it observes.
    pub fn task_awaits_endpoint(&self, slot: u32) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=25 = the endpoint awaited in a CALL.
        let ret = unsafe { raw_syscall(13, 25, slot as u64, 0) };
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
        // "now" is the DEGLITCHED MONOTONIC clock (query 17), not the raw RTC datetime (which is frozen
        // at 0 on a board with no RTC, e.g. the Pi 2 - the reason the old `datetime - boot` read 0 there).
        let now = self.epoch_secs_monotonic();
        let boot_epoch = self.boot_datetime().epoch_secs();
        // A board with no RTC reports a zero/garbage boot datetime whose epoch is far before 2001 (a
        // packed all-zeros datetime unpacks to ~year 0 = ~-6.2e10 s - subtracting THAT would ADD ~1970
        // years). There the monotonic clock is already seconds-since-boot (its CNTPCT baseline, ARM), so
        // it IS the uptime. With a real RTC, uptime is the elapsed wall-clock since boot.
        if boot_epoch < 1_000_000_000 { now.max(0) } else { (now - boot_epoch).max(0) }
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

    /// Terminal geometry as `(rows, cols)` text cells, or `(0, 0)` if it cannot be determined.
    ///
    /// **Asked of the `console` service, not the kernel.** It used to be `InspectKernel` query 9, which
    /// is now deleted: rows and columns are derived from the safe-area inset, the cell size and the
    /// font-scale rule, all of which live in the terminal - so the terminal is the only party that can
    /// answer, and asking anyone else would be a second source of truth (Commandment III).
    ///
    /// `(0, 0)` means **unknown**, and callers already treat it that way explicitly rather than
    /// substituting a size behind the user's back (the pager prints unpaged; `edit` falls back to 24x80
    /// and says so). It is returned when there is no console service, when it is mid-restart, or when it
    /// holds no framebuffer - all cases where a guessed geometry would be a silent fallback.
    ///
    /// Costs one IPC round trip, so callers that lay out a screen should ask once and keep the answer
    /// for that screen rather than per line.
    pub fn console_dims(&self) -> (u16, u16) {
        let mut buf = [0u8; 8];
        // Opcode 1 = REQ_DIMS (`services/console/src/main.rs`). The reply is rows then cols, u16 LE.
        // ACQUIRE THE PEER BY NAME IF WE WERE NEVER WIRED TO IT.
        //
        // The shell's contract declares `console` as a send peer, but the supervisor cannot wire it:
        // the shell is spawned BEFORE the console service exists, so there is nothing to hand it. The
        // kernel name directory is exactly the answer to that (§14.3 - reacquire by name), and
        // `time_rpc` in the shell already does this. This did not, so `find_send_slot` missed, the
        // request returned None, and the caller read (0, 0).
        //
        // Zero rows is not a small error here: `cmd_help` takes it to mean "no terminal" and prints
        // its whole table instead of paging it. A feature disappeared because a lookup missed, which
        // is the quiet kind of failure §26.7 is about - nothing was reported, it simply stopped
        // behaving.
        let mut n = self.request_with_reply_deadline_into("console", &[1u8], &mut buf, 2);
        if n.is_none() && self.reacquire_by_name("console") {
            n = self.request_with_reply_deadline_into("console", &[1u8], &mut buf, 2);
        }
        match n {
            Some(k) if k >= 4 => (
                u16::from_le_bytes([buf[0], buf[1]]),
                u16::from_le_bytes([buf[2], buf[3]]),
            ),
            _ => (0, 0),
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

    /// TSC ticks per 10 ms, from the kernel's boot-time CPUID calibration (InspectKernel query 16).
    /// Convert a TSC delta to milliseconds with `delta_cycles * 10 / tsc_ticks_per_10ms()`. Returns 0
    /// if the TSC was not calibrated (callers should then skip the millisecond conversion). `ping` uses
    /// it to report round-trip time.
    pub fn tsc_ticks_per_10ms(&self) -> u64 {
        // SAFETY: syscall(13) = InspectKernel; query_id=16 = TSC ticks per 10 ms quantum.
        let ret = unsafe { raw_syscall(13, 16, 0, 0) };
        if ret < 0 { 0 } else { ret as u64 }
    }

    /// Sleep for approximately `ms` milliseconds - the PORTABLE way to pace a loop.
    ///
    /// Prefer this over `sleep(cycles)` for anything expressing a DURATION. A "cycle" is not a portable
    /// unit: on x86 it is a ~2 GHz CPU cycle, on the Pi 2 it is a tick of a ~1 MHz timer, so the same
    /// literal means two wildly different lengths of time. Services were written with x86 in mind -
    /// `60_000_000` meaning "~30 ms at 2 GHz" - and on ARM that same constant asks for ~60 SECONDS once
    /// the quantum is calibrated. Three loops (the shell's muted poll and observe q-poll, and `observe`'s
    /// repaint pace) were exactly that, and each would have become a minute-long stall.
    ///
    /// Converts through the kernel's own calibration, so it is right on any machine. Floors at one
    /// scheduler quantum: the underlying `sleep(0)` returns immediately, which would turn a paced loop
    /// into a busy spin - the opposite of what every caller wants.
    pub fn sleep_ms(&self, ms: u64) {
        self.sleep(self.duration_cycles(ms));
    }

    /// Cycles for approximately `ms` milliseconds, for the two syscalls that take a cycle count
    /// (`sleep`, `recv_timeout`). See [`Self::sleep_ms`] for why a raw cycle literal is not portable.
    ///
    /// Never returns 0. That matters beyond pacing: `recv_timeout(0)` means **block forever**, so a
    /// zero here would convert a bounded wait into an unbounded one - a hang produced by a unit
    /// conversion. An uncalibrated machine (query 16 reads 0) also lands on the floor, which is exactly
    /// the one-quantum behaviour those platforms had before any of this existed.
    pub fn duration_cycles(&self, ms: u64) -> u64 {
        let per_10ms = self.tsc_ticks_per_10ms();
        if per_10ms == 0 { return 1; }              // uncalibrated: floor to one quantum
        let c = per_10ms.saturating_mul(ms) / 10;
        if c == 0 { 1 } else { c }
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

    /// Deglitched monotonic "now" in epoch seconds (kernel query 17). Unlike `datetime().epoch_secs()`
    /// (the raw RTC, query 11 - a CMOS misread on an in-range year slips through and reads years off), this
    /// drops backward / huge-forward glitches. Use it for time-DELTA deadlines and pacing, NOT for display.
    pub fn epoch_secs_monotonic(&self) -> i64 {
        // SAFETY: syscall(13) = InspectKernel; query 17 = deglitched monotonic epoch seconds.
        unsafe { raw_syscall(13, 17, 0, 0) }
    }

    // `set_wall_clock`, `set_clock_floor` and `clock_synced_secs_ago` were REMOVED with the kernel's
    // wall clock (clock slice 3). They called syscall 50 and query 21, which no longer mean what they
    // meant: 50 is deleted, and 21 was REUSED for `com2_byte`, so the stale reader did not merely fail -
    // it popped a byte off the operator channel. Ask the `time` service; it owns the clock now.




    /// A hardware-random u32 from the SoC RNG (the BCM2835 RNG on the Pi 2), or None if this build exposes
    /// no hardware RNG. Ungated (entropy confers no authority). The `random` shell utility consumes it.
    pub fn hw_random(&self) -> Option<u32> {
        // SAFETY: syscall(13) = InspectKernel; query 19 = a hardware-random u32 (-1 if unavailable).
        let r = unsafe { raw_syscall(13, 19, 0, 0) };
        if r < 0 { None } else { Some(r as u32) }
    }

    /// Drive a SoC GPIO pin (the shell `gpio` command; the Pi 2's BCM2835). `op`: 0 input / 1 output /
    /// 2 high / 3 low / 4 read. Returns the level (0/1) for a read, 0 on success, -1 on a bad pin /
    /// unsupported arch. Gated by the GPIO_DEVICE cap (only the shell holds it).
    pub fn gpio(&self, op: u32, pin: u32) -> i64 {
        // SAFETY: syscall(45) = Gpio; the kernel validates the GPIO_DEVICE cap and bounds op/pin.
        unsafe { raw_syscall(45, op as u64, pin as u64, 0) }
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
        // Packed into ONE 32-BIT word: file[0..12] | reply[12..24] | right[24..32].
        //
        // A syscall argument is a single register, and on a 32-bit target (arm32's r1/r2/r3) that
        // register is 32 bits - so anything placed above bit 31 is silently truncated on the way in.
        // The original layout put `right` at bit 32, which arrived as 0 on ARM: the kernel then had
        // no right to validate, `fs` received a badge of 0, and every real operation failed `op <=
        // right` while a read-only cap invoked declaring WRITE sailed past the kernel check that was
        // supposed to stop it. Three symptoms, one truncated field.
        //
        // 12 bits per slot is 4095 against a MAX_CAPS_PER_TASK of 64 - room to grow by 60x - and
        // `right` is a u8, so the whole thing lands in 24 bits with 8 to spare. This is the A-U1
        // rule from arch/arm/CLAUDE.md: on a 32-bit ABI, a syscall argument that does not fit in one
        // register must be narrowed at the wrapper, never assumed to survive.
        // REJECT a handle that does not fit its field instead of letting it corrupt the next one.
        //
        // Each slot gets 12 bits. A handle above 4095 does not simply get truncated - its high bits
        // land in the NEXT field. The `fcap` forged-handle check passes 60000 (0xEA60) and the kernel
        // logged `file_slot=2656 reply_slot=14`: the 0xE spilled out of `file` and rewrote `reply`, so
        // a bad file handle silently redirected the REPLY to whatever cap happened to sit in slot 14.
        // The invocation was rejected for the right reason by luck, not by design.
        //
        // A slot above 4095 cannot be valid anyway (MAX_CAPS_PER_TASK is 64), so this rejects with the
        // same error the kernel gives for a slot it does not hold - the caller sees no difference,
        // and no neighbouring field is ever silently rewritten.
        const SLOT_MAX: u32 = 0xFFF;
        if file.0 > SLOT_MAX || reply.0 > SLOT_MAX {
            return Err(crate::ipc::i64_to_ipc_error(-2)); // -2 = capability not held
        }
        let packed = ((right as u64) << 24) | ((reply.0 as u64) << 12) | (file.0 as u64);

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
    #[must_use = "the capability is STILL VALID if this is false"]
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
        // A wrong magic means the kernel handed us a corrupt ServiceContext - `self.log()` reads the
        // same corrupt ctx, so it cannot be trusted to report; parking is the service-level analog of
        // the kernel's halt-on-corrupt-state (§6.2). The slot guard below CAN speak (audit L8).
        if data.magic != SERVICE_CTX_MAGIC { loop {} }
        let slot = data.console_read_slot;
        if slot == u32::MAX {
            // No CONSOLE_READ cap: a caller that isn't the shell reached a shell-only syscall. Say so
            // LOUDLY (inv12) rather than wedge silently, then park - the caller's contract is wrong.
            self.log("sdk: console_read called without a CONSOLE_READ cap - parking (contract error)");
            loop {}
        }
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
            Some(crate::mmio::Mmio::new(va as *mut u8, Self::ctx().xhci_mmio_len as usize))
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
            Some(crate::mmio::Mmio::new(va as *mut u8, Self::ctx().xhci_mmio_len as usize))
        }
    }

    /// Safe MMIO handle to this service's device register window, if one was
    /// granted (§12) - the neutrally-named accessor for non-USB drivers (e.g. the
    /// AHCI `block-driver`, which maps its HBA ABAR here). Same kernel-mapped
    /// The framebuffer the kernel granted this service, or `None` if it holds no grant.
    ///
    /// Held only by `console`, which renders the terminal into it (`docs/console-service.md` §9). The
    /// grant carries PIXEL geometry only - character rows and columns are the terminal's own business,
    /// and the kernel deliberately does not compute or publish them (Commandment III).
    pub fn framebuffer(&self) -> Option<crate::mmio::Framebuffer> {
        let d = Self::ctx();
        if d.fb_va == 0 || d.fb_len == 0 {
            return None;
        }
        let sh = d.fb_shifts;
        Some(crate::mmio::Framebuffer::new(
            d.fb_va as *mut u8,
            d.fb_len as usize,
            d.fb_pitch as usize,
            d.fb_bpp as usize,
            d.fb_width as usize,
            d.fb_height as usize,
            sh & 0xFF,
            (sh >> 8) & 0xFF,
            (sh >> 16) & 0xFF,
        ))
    }

    /// window as [`xhci_mmio`](Self::xhci_mmio). `None` for non-driver services.
    pub fn mmio(&self) -> Option<crate::mmio::Mmio> {
        let va = Self::ctx().xhci_mmio_va;
        if va == 0 {
            None
        } else {
            Some(crate::mmio::Mmio::new(va as *mut u8, Self::ctx().xhci_mmio_len as usize))
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
        let rc = unsafe { raw_syscall(18, 0, 0, 0) };
        // A REFUSED reboot must not look like a successful one.
        //
        // The kernel gates this on the REBOOT capability (§3.1) and returns `CapNotHeld` to anyone
        // without it. This used to fall into a bare `loop {}`, so a caller that lacked the cap simply
        // stopped - no message, no reset, a frozen prompt. "Reboot does nothing" is exactly how that
        // reads from the outside, and it is indistinguishable from a reset that failed in hardware.
        //
        // Say which it was. The kernel's own reset path is bounded and reports if the SoC does not come
        // down; this covers the other half, where the syscall never got that far. Invariant 12: failures
        // are loud, never silent.
        self.log_fmt(format_args!(
            "reboot REFUSED by the kernel (rc {}) - this service does not hold the REBOOT capability", rc));
        loop {
            self.yield_cpu();
        }
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
        if len == 0 { return; }

        // TOO LONG IS TRUNCATED, NEVER DROPPED.
        //
        // This used to be `if len == 0 || len > 256 { return; }` - a message over the limit was
        // discarded whole, with no truncation, no marker and no error. The reporting channel itself
        // failed silently, which makes every carefully-worded warning in the system conditional on
        // its own length.
        //
        // It was not hypothetical. The two longest literal log lines in the tree are both in `fs`,
        // and both are messages the constitution leans on: the durability-not-attested warning (280
        // bytes), which CLAUDE.md 6.1's backend-conditional TCB claim describes as "`fs` says so once
        // per mount", and the journal-recovery refusal (356 bytes), whose own comment argues it must
        // stay as visible as the fault it reports. Neither has ever been printed on this board, and
        // the durability one is latched before the call, so it never would be.
        //
        // Truncating loses the tail of a sentence. Dropping loses the fact. The marker says which
        // happened, so a reader is never left believing they saw the whole message.
        const LOG_MAX: usize = 256;
        if len > LOG_MAX {
            const MARK: &str = " [TRUNCATED]";
            let keep = LOG_MAX - MARK.len();
            // Cut on a CHARACTER boundary: `as_bytes` is UTF-8 and slicing mid-codepoint would hand
            // the kernel an invalid string. Walk back at most 3 bytes to the start of a codepoint.
            let mut end = keep;
            while end > 0 && (bytes[end] & 0xC0) == 0x80 {
                end -= 1;
            }
            // SAFETY: syscall(5) = Log; both slices are valid, in-bounds and within user space. Sent
            // as two calls rather than staged through a buffer, because a fixed staging buffer here
            // is exactly the kind of hidden limit this change exists to remove.
            unsafe {
                raw_syscall(5, slot as u64, bytes.as_ptr() as u64, end as u64);
                raw_syscall(5, slot as u64, MARK.as_ptr() as u64, MARK.len() as u64);
            }
            return;
        }

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

    /// Formatted variant of [`Self::console_line`].
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
    ///
    /// **Asks the SUPERVISOR when this service can reach it**, and the kernel otherwise.
    ///
    /// Step C moves service images out of the kernel (`docs/service-ownership.md`), and a service the
    /// kernel does not hold cannot be spawned by name through the kernel. Seven separate callers did
    /// exactly that - control's RESTART, the supervisor's own respawn, the shell's `spawn`, the
    /// shell's PIPES, `spawncap`, `spawnwired`, and `chaos` - and each was found by a regression
    /// rather than by reading. Routing HERE fixes all of them at once, including the ones not yet
    /// found.
    ///
    /// The condition is exactly right by construction: the SUPERVISOR has no supervisor-peer, so it
    /// keeps the kernel path it must have; a service with no such peer keeps the old behaviour and
    /// can still spawn anything the kernel still owns.
    pub fn spawn_on(&self, name: &str, core: u32) -> Result<(), crate::Error> {
        if self.find_send_slot("supervisor").is_some() {
            return self.spawn_via_supervisor(name, core, &[]).map(|_| ());
        }
        self.spawn_on_kernel(name, core)
    }

    /// Ask the supervisor to spawn `name`, returning the `SEND|GRANT` cap it hands back (`None` if
    /// the service has no recv endpoint, or the cap could not be taken).
    ///
    /// The supervisor is RESTARTABLE (6.2), so a cached cap to it goes stale on every respawn. This
    /// reacquires by name and retries ONCE on `Err` - the send itself failed, so the peer is gone -
    /// and never on `Ok(None)`, where the deadline passed and the request may well have landed
    /// (retrying a possibly-delivered spawn would start the service twice).
    pub fn spawn_via_supervisor(&self, name: &str, core: u32, peers: &[&str])
        -> Result<Option<CapHandle>, crate::Error>
    {
        let mut buf = [0u8; supcmd::MAX];
        let n = supcmd::encode(&mut buf, supcmd::SPAWN, core, name, peers)
            .ok_or(crate::Error::InvalidArgument)?;
        let msg = crate::ipc::Message::from_bytes(&buf[..n]);
        for attempt in 0..2 {
            match self.request_with_reply_call_err("supervisor", &msg, 2) {
                Ok(Some(reply)) => {
                    let ok = reply.payload_bytes().first() == Some(&supcmd::OK);
                    let cap = self.take_pending_cap();
                    if ok { return Ok(cap); }
                    // Answered "no". Reclaim any cap that rode along, or it leaks a table slot.
                    if let Some(c) = cap { self.remove_cap(c); }
                    return Err(crate::Error::InvalidArgument);
                }
                Ok(None) => return Err(crate::Error::InvalidArgument),
                Err(_) if attempt == 0 => { let _ = self.reacquire_by_name("supervisor"); }
                Err(_) => return Err(crate::Error::InvalidArgument),
            }
        }
        Err(crate::Error::InvalidArgument)
    }

    /// The kernel spawn-by-name path, unrouted. What `spawn_on` used to be.
    pub fn spawn_on_kernel(&self, name: &str, core: u32) -> Result<(), crate::Error> {
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

    /// Spawn a task from an image THIS service supplies (`SpawnImage`, syscall 52).
    ///
    /// The kernel loads what it is handed instead of looking a name up in its own catalogue - the
    /// step that ends that catalogue (`docs/service-ownership.md`). Requires the SPAWN capability,
    /// exactly as `spawn` does: supplying the image is not authority to start it.
    ///
    /// `peers` are NUL-joined into a caller-owned buffer; each name still resolves through the
    /// kernel name directory, so this grants nothing a contract's `send_peers` would not.
    /// Returns a `SEND|GRANT` cap to the new service's recv endpoint, or `None` if it has none -
    /// the spawner needs it to record `name -> cap` and re-wire dependents after a restart.
    pub fn spawn_image(&self, req: &mut SpawnRequest, peers_buf: &mut [u8], peers: &[&str])
        -> Result<Option<CapHandle>, crate::Error>
    {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(crate::Error::InvalidArgument); }
        let slot = data.spawn_slot;
        if slot == u32::MAX { return Err(crate::Error::Cap(CapError::CapNotHeld)); }

        let mut n = 0usize;
        for (i, p) in peers.iter().enumerate() {
            if i > 0 {
                if n >= peers_buf.len() { return Err(crate::Error::InvalidArgument); }
                peers_buf[n] = 0; n += 1;
            }
            let b = p.as_bytes();
            if n + b.len() > peers_buf.len() { return Err(crate::Error::InvalidArgument); }
            peers_buf[n..n + b.len()].copy_from_slice(b);
            n += b.len();
        }
        req.peers_ptr = if n > 0 { peers_buf.as_ptr() as u64 } else { 0 };
        req.peers_len = n as u32;

        // SAFETY: syscall(52) = SpawnImage; `req` is a live, fully initialised SpawnRequest whose
        // layout matches the kernel's, and the kernel copies it once before reading any field.
        let ret = unsafe {
            raw_syscall(52, req as *const SpawnRequest as u64,
                        core::mem::size_of::<SpawnRequest>() as u64, slot as u64)
        };
        // slot + 1, so 0 means "spawned, no recv endpoint" without colliding with slot 0.
        if ret < 0 { Err(crate::Error::InvalidArgument) }
        else if ret == 0 { Ok(None) }
        else { Ok(Some(CapHandle(ret as u32 - 1))) }
    }


    /// Spawn `name` on `core` (0xFFFF = round-robin) and receive a `SEND|GRANT` cap to its recv
    /// endpoint. This is the Phase-0 seam for moving naming out of the kernel
    /// (`docs/naming-design.md`): a spawner (the supervisor) collects a cap to every service it
    /// starts - a userspace `name → cap` map - instead of the kernel resolving names. Requires the
    /// SPAWN cap. `None` if the cap is not held, the spawn failed, or the service has no recv
    /// endpoint to hand back. The old name-wiring path is unchanged; this is purely additive.
    /// `Err(())` the spawn failed. `Ok(None)` it spawned but has no recv endpoint. `Ok(Some(cap))`
    /// it spawned and here is a `SEND|GRANT` cap to its endpoint.
    ///
    /// The three-way answer is the point. This returned a bare `Option` and the kernel a bare slot,
    /// so "no endpoint" and "failed" were the same value - and a caller that spawns a service without
    /// an endpoint (`mem-pressure`, `greet`, `roster`) read success as failure.
    pub fn spawn_returning_endpoint(&self, name: &str, core: u32) -> Result<Option<CapHandle>, ()> {
        let data = Self::ctx();
        if data.magic != SERVICE_CTX_MAGIC { return Err(()); }
        let slot = data.spawn_slot;
        if slot == u32::MAX { return Err(()); }
        let bytes  = name.as_bytes();
        let packed = ((core as u64 & 0xFFFF) << 16) | (slot as u64 & 0xFFFF);
        // SAFETY: syscall(38) = SpawnReturningEndpoint; slot from the kernel-written page; bytes valid.
        let ret = unsafe { raw_syscall(38, packed, bytes.as_ptr() as u64, bytes.len() as u64) };
        // slot + 1, so 0 is "spawned, no recv endpoint" and never a real slot.
        if ret < 0 { Err(()) } else if ret == 0 { Ok(None) } else { Ok(Some(CapHandle(ret as u32 - 1))) }
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
