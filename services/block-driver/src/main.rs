// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `block-driver` - userspace **AHCI (SATA)** disk driver (persistence, v2; §6.3,
//! docs/ahci.md, docs/persistence.md).
//!
//! An MMIO + DMA driver: the kernel maps the AHCI HBA's ABAR and grants a
//! physically-contiguous DMA arena at spawn (the same path as the USB drivers).
//! It IDENTIFYs the disk, runs a boot self-test, then serves block read/write to
//! `fs` over IPC.
//!
//! (ATA PIO + the `hw_pio` capability were the bring-up backend; retired once AHCI
//! proved out - the T630's SSD is AHCI-only, so AHCI is the production path.)

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Backend by architecture: x86 talks AHCI (SATA, MMIO+DMA); ARM (Raspberry Pi 2) storage is a USB stick
// (`usbdisk`, through the in-kernel DWC2 stack). Both satisfy the same block-IPC protocol below.
//
// The BCM2835 EMMC / Arasan SDHCI backend (`sdhci.rs`) is DELIBERATELY NOT COMPILED IN. On the Pi 2 the
// EMMC IS the SD card the board boots from - firmware + kernel + FAT boot partition - and using it as
// GSFS storage writes over the boot partition and corrupts the card to RAW (it destroyed two boot
// cards). There is no safe way to use a single-slot Pi's boot card as storage, so the backend is a
// hazard, not a fallback. The file is kept for reference (a future board with a SEPARATE storage medium
// could use it), but it is not a module here so it cannot be reached - see `backend_run`.
/// A reply cap plus the CORRELATION TAG of the request it answers.
///
/// Every reply to `fs` carries the tag of the request that asked for it, so a reply that arrives out
/// of order is DETECTABLE rather than silently believed. Chaos proved the need: after 98 rounds of
/// killing and restarting these services the block protocol came back out of step, `fs` read LBA 7702
/// and got a 17-byte reply where a 513-byte block belonged, and the only reason it noticed was that
/// the SHAPE happened to be wrong. Two reads of different blocks are both 513 bytes and shape cannot
/// separate them at all - one would simply be believed, which is silent data corruption.
///
/// The tag lives in the reply HANDLE rather than being remembered separately, so it cannot be
/// forgotten: there is no way to answer a request without it, and the compiler says so.
#[derive(Clone, Copy)]
pub struct Reply {
    pub cap: godspeed_sdk::CapHandle,
    pub tag: u8,
    /// What to do to this completion (carnage §3.7). `Fault::None` on every shipping build.
    ///
    /// **Decided at CONSTRUCTION, carried here, applied at send** - and the shape is forced by
    /// ownership. The injector's counters cannot be `static` (Commandment VI, no unowned global
    /// mutable state in services - the build refuses it), so they live in a `Chaos` the main loop
    /// owns. But `send` is called from sixteen places across three files and a reply is BUILT in
    /// four, so threading `&mut Chaos` to the senders means touching all sixteen, including a
    /// closure where it would collide with an existing borrow. Deciding once, at the four
    /// construction sites, costs nothing and reaches every sender for free.
    pub fault: Fault,
}

impl Reply {
    /// A reply that will be answered faithfully - every path except the one injecting faults.
    pub fn plain(cap: godspeed_sdk::CapHandle, tag: u8) -> Self {
        Reply { cap, tag, fault: Fault::None }
    }
}

/// What to do to one completion, for the `completion-chaos` build (carnage §3.7).
#[derive(Clone, Copy, PartialEq)]
pub enum Fault {
    /// Answer normally.
    None,
    /// Answer TWICE. The second is an orphan the caller never asked for.
    Duplicate,
    /// Do not answer at all. The caller's deadline must fire rather than hanging it.
    Missing,
    /// Answer with somebody else's tag - what an out-of-order or previous-instance completion
    /// looks like from the caller's side, and the one shape a length check cannot catch.
    WrongTag,
}

/// The injector's state, OWNED BY `service_main` - which is the point, not a detail.
///
/// The first version kept two `static` atomics and `commandments.py` refused the build:
/// Commandment VI, no unowned global mutable state in services. It was right, and "it is only a
/// test build" is not an exemption - the rule says an exemption must cite a constitutional
/// amendment that accepts the violation, and none does. There is exactly one call site, so the
/// counters are simply a local the main loop holds and passes down (Invariant 8).
///
/// Zero-sized on a shipping build, so the parameter costs nothing and the faults cannot exist.
#[cfg(feature = "completion-chaos")]
pub struct Chaos { seen: u32, hit: u32 }
#[cfg(not(feature = "completion-chaos"))]
pub struct Chaos;

impl Chaos {
    #[cfg(feature = "completion-chaos")]
    pub fn new() -> Self { Chaos { seen: 0, hit: 0 } }
    #[cfg(not(feature = "completion-chaos"))]
    pub fn new() -> Self { Chaos }

    /// Which fault, if any, this completion should suffer.
    ///
    /// **THIS ATTACKS THE PROTOCOL, NOT THE DEVICE, AND THAT IS THE GAP IT FILLS.** `io-error-test`
    /// makes the disk fail, which `fs` already handles by retrying. Carnage §3.7 asks for the other
    /// half - late, duplicate, missing and out-of-order COMPLETIONS - and names why: `backlog/31`
    /// recorded exactly that one layer up, a service reading replies that belonged to earlier
    /// requests. The fs/block channel has the same shape, carries the same correlation tag, and
    /// gained a drain-before-request repair that nothing had ever exercised adversarially.
    ///
    /// **The bar is RECOVERY, not detection** - `backlog/31`'s tag was reverted precisely because
    /// rejecting a stale reply is not the same as surviving one.
    ///
    /// One of each shape and no more: a MISSING completion costs the caller its full 30 s deadline
    /// by design, so more injections buy no proof and starve the verification phase of clock. The
    /// warm-up is MEASURED - a boot consumes ~795 completions - so boot and mount run untouched and
    /// the first fault lands in ordinary filesystem work.
    #[cfg(feature = "completion-chaos")]
    pub fn fault(&mut self, ctx: &godspeed_sdk::ServiceContext) -> Fault {
        const WARMUP: u32 = 800;
        const STRIDE: u32 = 8;
        const LIMIT:  u32 = 3;
        let n = self.seen;
        self.seen += 1;
        if n < WARMUP || (n - WARMUP) % STRIDE != 0 { return Fault::None; }
        let k = self.hit;
        if k >= LIMIT { return Fault::None; }
        self.hit += 1;
        let f = match k % 3 {
            0 => Fault::Duplicate,
            1 => Fault::WrongTag,
            _ => Fault::Missing,
        };
        ctx.log_fmt(format_args!(
            "block-driver: [completion-chaos] injecting {} at completion {} (injection {} of {})",
            match f { Fault::Duplicate => "DUPLICATE", Fault::WrongTag => "WRONG-TAG",
                      Fault::Missing => "MISSING", Fault::None => "none" },
            n, k + 1, LIMIT));
        f
    }

    #[cfg(not(feature = "completion-chaos"))]
    #[inline(always)]
    pub fn fault(&mut self, _ctx: &godspeed_sdk::ServiceContext) -> Fault { Fault::None }
}

impl Reply {
    /// Answer, with the tag in front. `body` is the reply exactly as it was before tagging.
    pub fn send(&self, ctx: &godspeed_sdk::ServiceContext, body: &[u8]) {
        // 513 is the largest body (status + one block); one more for the tag. Fixed, on the stack,
        // no allocation (§26.6.1).
        let mut out = [0u8; 514];
        out[0] = self.tag;
        let n = body.len().min(out.len() - 1);
        out[1..1 + n].copy_from_slice(&body[..n]);
        // TRY_SEND, NEVER SEND - §8.9, and a deadlock rather than a style preference.
        //
        // A blocking reply can be held forever by a caller whose queue is full, and the chain is real:
        // `fs` sends a request and then waits for the answer ON ITS OWN ENDPOINT, which keeps receiving
        // client requests meanwhile. Sixteen arrive - a busy shell, or chaos flood-storming it - and the
        // queue is full. With a blocking send nothing breaks it: this driver waits for room, `fs` waits
        // for the reply, the shell waits behind `fs`. Dropping it instead is recoverable, because the
        // caller's deadline fires and it retries. Every other service in the tree already does this;
        // `block-driver` and `fs` were the two exceptions, and they are the persistence path.
        //
        // Logged on EVERY failure, deliberately un-rate-limited, and the protocol is why that is
        // bounded: this driver's caller is `fs`, which has at most one request outstanding, so a repeat
        // means its queue is persistently full - abnormal, and worth saying every time. A caller that
        // has DIED yields at most the requests already in flight. Neither can flood (§26.7, §26.6).
        // Measured: 55 in a 1,000-round chaos soak, every one beside a `kill_task` of the caller.
        //
        // The message names BOTH causes because they are different faults and this cannot tell them
        // apart from here. Under chaos it is almost always the caller being killed mid-request
        // (`liveness=Dead` sits beside each one), which is ordinary. A full queue on a LIVE caller is
        // genuine backpressure and is the one worth chasing - so the line must not assert the second
        // when it is seeing the first, which the earlier wording did.
        let fault = self.fault;
        if fault == Fault::Missing { return; }
        if fault == Fault::WrongTag {
            // Not a random byte: a tag the caller could plausibly be awaiting, which is what makes
            // this the case a length check cannot separate. `+1` cannot collide with `self.tag`.
            out[0] = self.tag.wrapping_add(1);
            if out[0] == 0 { out[0] = 1; }
        }
        self.emit(ctx, &out[..1 + n]);
        if fault == Fault::Duplicate { self.emit(ctx, &out[..1 + n]); }
    }

    /// One send, with the undelivered report. Split out so a fault can emit zero, one or two.
    fn emit(&self, ctx: &godspeed_sdk::ServiceContext, msg: &[u8]) {
        if ctx.try_send_by_handle(self.cap, &godspeed_sdk::Message::from_bytes(msg)).is_err() {
            ctx.log("block-driver: reply undelivered (caller is gone, or its queue is full) - it will time out and retry");
        }
    }
}

// WHICH BACKEND, asked as a board fact rather than as an instruction set. `storage_is_usb` and
// `STORAGE_HOST` are set by `build.rs` from the target cargo is already building for, and the whole
// mapping lives there in one table; see its comment for why it is derived rather than a feature.
#[cfg(not(storage_is_usb))]
mod ahci;
#[cfg(storage_is_usb)]
mod usbdisk;
// Every USB board reaches its host controller through this one client - arm32 the `dwc2` service,
// aarch64 and riscv64 the `xhci` service. The wire format is identical, so only the name differs,
// and the name comes from `STORAGE_HOST`. Compiled everywhere, including x86 where nothing calls it.
mod xhciblk;

// Block IPC protocol (fs <-> block-driver). MUST match `services/fs`.
//   Request : [op:u8, lba:u64 LE, (WriteBlock only: 512 data bytes)]
//   Reply   : [status:u8, (ReadBlock only: 512 data bytes)]
// The LBA is u64 (persistence §6.3): GSFS capacity fields are u64, so the block
// address reaches the device at full width.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
// Capacity request: [OP_CAPACITY] → reply [STATUS_OK, sectors:u64 LE]. Lets `fs`
// size a freshly-flashed filesystem to the real disk (drives §7, persistence §6.3).
const OP_CAPACITY: u8 = 3;
// Write-zeros: [OP_WRITE_ZEROS, lba:u64, count:u64] → zero `count` blocks from `lba`,
// batched into multi-sector AHCI commands (no per-block IPC, no data carried). `fs` uses
// it to zero the free bitmap at format time so `drives flash` stays fast on a big disk.
const OP_WRITE_ZEROS: u8 = 4;
// Flush: [OP_FLUSH] → make every previously acknowledged write durable on the medium, reply
// [STATUS_OK] or [STATUS_ERR]. A write completing does NOT mean the bytes reached the disk - a
// USB stick acknowledges into its own volatile buffer - so `fs` asks for durability explicitly at
// the points where it promises it (format, journal commit). Backends that cannot flush say so
// once at startup rather than quietly implying the data is safe (§26.7).
const OP_FLUSH: u8 = 5;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// Drive the AHCI controller directly, through the BAR the kernel granted this service.
#[cfg(not(storage_is_usb))]
fn backend_run(ctx: &ServiceContext, m: &godspeed_sdk::Mmio) -> ! { ahci::run(ctx, m) }
/// On a board whose only card slot is its boot medium, storage is the USB stick and ONLY the USB
/// stick. Never the SD/EMMC card.
///
/// **Every `storage_is_usb` board, for the same reason.** The Pi 4 has the Pi 2's single-slot topology, and
/// so does the VisionFive 2: one SD card, which is what the machine boots from. The rule below was
/// established on the Pi 2 and applies unchanged to both of the others - the JH7110 has an eMMC
/// controller and an SD slot, and the slot is where U-Boot, the device tree and this kernel live.
///
/// The Pi 2 has one SD slot and boots from it: that card carries the firmware, the kernel image, and a
/// FAT boot partition. It is the boot medium, full stop - there is no safe way to also hand it to GSFS,
/// because GSFS's superblock lives at LBA 0, exactly where the card's partition table is. Reaching the
/// card through the SD/EMMC backend let `fs` write GSFS over the boot partition and **corrupt the card
/// to RAW - observed destroying two boot cards** (the `foreign_disk` guard is not enough: once any
/// GSFS-looking bytes are on the card, or after a single `drives flash ... force` aimed at the wrong
/// disk, `fs` mounts it and writes). So the SD backend is not a fallback; it is a hazard, and it is
/// removed. With no USB stick, there is simply NO storage - exactly what x86 reports with no disk
/// attached - and `fs` comes up storage-unavailable. `usbdisk::run` with a 0 sector count serves that
/// no-disk state (capacity 0, every read/write refused) WITHOUT touching the card.
#[cfg(storage_is_usb)]
fn backend_run(ctx: &ServiceContext) -> ! {
    // Where the sector count comes from is the same build-time choice usbdisk.rs documents: the
    // in-kernel stack by syscall, or the `xhci` service by IPC. No probe, no fallback.
    // arm32 now asks the `dwc2` SERVICE over IPC, exactly as aarch64 asks `xhci` - the in-kernel
    // stack's `usb_disk_*` syscalls are no longer the path. Same client, same wire format, different
    // service name (`xhciblk::XHCI`), which is why this is a cfg flip rather than a port.
    // ONE quick question at startup, not a 20 s wait for an answer we no longer use.
    //
    // This called `sectors()`, which blocks up to 20 s for a capacity. That made every boot with no
    // stick in it stall for 20 s before the first `drives` could answer - the reported "substantial
    // delay on startup, subsequent ones are quick".
    //
    // The wait existed to capture a boot-time sector count. Since `serve()` re-derives capacity on
    // every request, that number no longer answers anything: it is used for the startup log line and
    // then shadowed. So the wait buys nothing and costs 20 s on exactly the machine that has the
    // least to wait for.
    //
    // A stick present but still enumerating now reports 0 here and mounts slightly later, on the
    // first request, through the same self-heal that already recovers a stick plugged in after boot -
    // hardware-proven across several plug/unplug cycles. Trading a guaranteed 20 s stall for a
    // recovery path that is exercised constantly is the right way round.
    // THE STARTUP COUNT IS FOR THE LOG LINE ONLY - `serve` re-derives capacity on every request, so
    // this number is shadowed the moment anything asks. What matters is that it does not ANNOUNCE a
    // verdict it has not earned: "no USB storage stick" said on an unanswered query is the sentence
    // that made a restarting `xhci` look like an empty bay in the log, and a reader who trusted it
    // went looking for the stick rather than for the peer.
    let sectors = match xhciblk::sectors_now(&ctx) {
        xhciblk::Capacity::Sectors(n) => {
            if n == 0 {
                ctx.log("block-driver: no USB storage stick - NO disk (the SD card is the boot medium and is never written)");
            }
            n
        }
        xhciblk::Capacity::Unreachable => {
            // "No USABLE capacity" covers both cases this arm carries - a peer that said nothing
            // and one whose reply did not parse - without claiming either. Saying "not answering"
            // for a reply that arrived but was too short is the same species of wrong sentence
            // this whole change is about.
            ctx.log("block-driver: no usable capacity from the USB host service yet (no answer, or an answer that did not parse) - re-derived on the first request");
            0
        }
    };
    usbdisk::run(ctx, sectors)
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column, and
    // worse, every METRIC published lands under a BLANK owner: the metric key is (owner, name), so
    // ten unnamed services all collide into one row and their counters interleave. Observed as a
    // single `msgs.received 1920` belonging to nobody.
    ctx.trace_as("block-driver");
    // A USB board's backend needs NO MMIO: the disk is reached by IPC to the service that owns the
    // host controller (`STORAGE_HOST`), so this service is granted no window and asks for none. Going
    // through the `ctx.mmio()` gate would refuse a perfectly good USB stick on any board that does not
    // also hand this service a peripheral window it never reads - which is the Pi 4 exactly.
    //
    // (This used to say the USB stack was in the kernel and the disk reached through syscalls. That
    // stopped being true on aarch64 in 2026-08 and on arm32 a week later - both stacks are services
    // now, and `xhciblk.rs` is the client. The syscall route survives in `usbdisk.rs` for a board
    // that has no such service, and no shipping port is one.)
    #[cfg(storage_is_usb)]
    backend_run(&ctx);
    #[cfg(not(storage_is_usb))]
    match ctx.mmio() {
        Some(m) => backend_run(&ctx, &m),
        None => {
            ctx.log("block-driver: no AHCI controller found (no SATA disk?)");
            // DRAIN our IPC endpoint forever, never just yield: a registered service that idles here without
            // recv'ing lets a flood (or any stray send) fill its 16-deep queue and sit at 16/16 FOREVER - the
            // flood-endpoint disease (`events` stub / xhci idle()). recv() parks between messages, so the
            // core still idles. We POLL (try_recv) rather than block on recv: a cross-core flood that must
            // WAKE a deeply-blocked recv on an AP is unreliable under QEMU TCG (the drain flaked in the
            // flood-storm pin); the self-driven poll drains every quantum with no wake needed. Pinned by the
            // shell-test `chaos flood-storm block-driver` step (QEMU's pc machine has no AHCI, so it sits here).
            // ANSWER while draining. The loop here used to be
            //     loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
            // which retired every request and replied to none - so `fs` blocked forever in its first
            // `block_capacity()`, never reached its own storage-unavailable degraded path, and never
            // printed `fs: serving file API`; every file command in the shell hung behind it. On any
            // machine with no AHCI (an NVMe-only box, QEMU's default `pc`) that is the whole storage
            // stack silently dead, on a branch whose comment says the two services "come up and idle
            // gracefully".
            //
            // The correct version was already in this crate: `ahci::serve_no_disk` answers CAPACITY
            // with a truthful zero and everything else with STATUS_ERR. The drain reasoning below it
            // still holds - what was missing was the reply.
            //
            // It keeps polling rather than blocking on `recv` for the reason the original comment
            // gives (a cross-core flood must not depend on waking a deeply-blocked recv), so the
            // answer path is inlined here rather than delegating to the blocking version.
            loop {
                while let Some(msg) = ctx.try_recv() {
                    let reply = match ctx.take_pending_cap() {
                        Some(c) => c,
                        None => continue,   // nothing to answer on; dropping is all that is left
                    };
                    let raw = msg.payload_bytes();
                    let (tag, p) = match raw.split_first() {
                        Some((t, rest)) => (*t, rest),
                        None => (0, &raw[..0]),
                    };
                    let reply = Reply::plain(reply, tag);
                    let mut out = [0u8; 9];
                    let n = if !p.is_empty() && p[0] == OP_CAPACITY {
                        // Capacity is [STATUS_OK, sectors u64 LE]; zero sectors is the truth here and
                        // is exactly what `fs` reads as "genuinely no disk".
                        out[0] = STATUS_OK;
                        9
                    } else {
                        out[0] = STATUS_ERR;
                        1
                    };
                    reply.send(&ctx, &out[..n]);
                    ctx.remove_cap(reply.cap);
                }
                ctx.yield_cpu();
            }
        }
    }
}
