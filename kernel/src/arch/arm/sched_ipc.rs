// SPDX-License-Identifier: GPL-2.0-only
//! IPC on ARM: `ping` sends, `pong` receives - two USER services talking through a kernel endpoint.
//!
//! This hand-wires what the supervisor will later do from a manifest:
//!
//!   1. Create an **endpoint** owned by `pong` (`alloc_endpoint_id` + register in the capability,
//!      routing, and name tables - the exact sequence `task::spawn_service_with_config` uses).
//!   2. Load `pong` with a **RECV** cap to that endpoint, and `ping` with a **SEND** cap to it.
//!   3. Fill each service's `ServiceContext` so the SDK resolves them: `pong.recv_slot` -> its RECV
//!      cap, and `ping.send_peers["pong"]` -> its SEND cap.
//!   4. Commit both as scheduled USER tasks and run.
//!
//! The intent: `ping` `ctx.try_send("pong", ...)` and `pong` `ctx.recv()` + logs `pong: received
//! "..."` - a capability-mediated message from one ring-3 service to another, copied sender ->
//! receiver by the kernel (§8.5), under preemption. No zero-copy, no shared memory.
//!
//! **KNOWN BUG (increment 3b, under investigation - gated behind `arm-sched-ipc`, off by default).**
//! The wiring is verified correct (a ctx dump confirmed `send_peer_count=1, slot=1, name="pong"`; both
//! services reach PL0 and log `ping: starting` / `pong: ready on core 0`). But once `ping` issues
//! syscalls in a loop alongside a *second* running user task, a task's register state is corrupted and
//! it jumps to a wild PC. Bisected precisely:
//!   - `pong`'s blocking `recv` alone is fine (a diag kernel task kept ticking while pong blocked).
//!   - `ping` ALONE (self-scheduling, no second user task) runs its `try_send`/`yield` loop forever,
//!     clean.
//!   - `ping` + `pong` together corrupts after a few syscalls (garbage syscall numbers, then a DATA
//!     ABORT to `0xfffffeae` from a PC in a data page).
//! So the fault is in a **real cross-task `switch_context` reached from a *syscall* context** (yield/
//! block) - a path #1/#2/#3a never exercised (they switch only via the timer IRQ; #3a's two user tasks
//! never yield/block, they busy-loop on a non-blocking `recv`). The USER-banked `SP_usr`/`LR_usr` were
//! the first suspect and were ruled out (saving/restoring them in `stub_svc` and in `switch_context`
//! both left the corruption unchanged). Next leads: the AAPCS callee-saved contract across a
//! syscall-context switch between two *different* user address spaces (TTBR0 change), or the SVC-stack
//! nesting when the timer preempts a task mid-syscall.

use core::sync::atomic::Ordering;

use crate::arch::imp::context_switch::TaskContext;
use crate::capability::{mint_cap, next_generation, register_resource_at_gen, Rights, ResourceId};
use super::pl011_write;
use super::spawn::{USER_STACK_TOP, SERVICE_CTX_MAGIC};

/// The ARM `ping`/`pong` ELFs, embedded by `build.rs` (empty placeholder on a not-yet-ported arch).
static PING_ELF: &[u8] = include_bytes!(env!("SVC_PING_ELF"));
static PONG_ELF: &[u8] = include_bytes!(env!("SVC_PONG_ELF"));

const KSTACK: usize = 8192;

/// One kernel stack per USER task (for its trap frames): ping + pong.
#[repr(align(8))]
struct Stacks([[u8; KSTACK]; 2]);
static mut STACKS: Stacks = Stacks([[0; KSTACK]; 2]);

/// Write the SDK `ServiceContextData` into a freshly-allocated ctx frame.
///
/// The frame is not zeroed by the allocator (SEC-21), so the whole 208-byte struct is zeroed first,
/// then only the fields the ARM `ping`/`pong` read are set. Byte offsets follow the `#[repr(C)]`
/// layout in `sdk/rust/src/service_context.rs` (u64 xHCI fields at 32..72 stay zero; `send_peers`
/// starts at byte 80, each entry `slot:u32, name_len:u32, name:[u8;24]`).
fn write_ipc_ctx(ctx_frame: u32, recv_slot: u32, send_peer: Option<(&str, u32)>) {
    // SAFETY: `ctx_frame` is a fresh, identity-mapped frame we own; writing the SDK context struct.
    unsafe {
        let base = ctx_frame as *mut u8;
        core::ptr::write_bytes(base, 0, 208);
        let w = ctx_frame as *mut u32;
        w.add(0).write_volatile(SERVICE_CTX_MAGIC);   // magic
        w.add(1).write_volatile(0);                   // log_write_slot = 0 (cap at slot 0)
        w.add(2).write_volatile(recv_slot);           // recv_slot
        w.add(3).write_volatile(u32::MAX);            // spawn_slot = none
        w.add(4).write_volatile(if send_peer.is_some() { 1 } else { 0 }); // send_peer_count
        // word 5 core_id = 0, word 6 probe_mode = 0 (left zero)
        w.add(7).write_volatile(u32::MAX);            // console_read_slot = none
        // words 8..18 = xHCI u64 fields, left zero
        w.add(18).write_volatile(u32::MAX);           // console_push_slot (byte 72) = none
        w.add(19).write_volatile(u32::MAX);           // self_grant_slot   (byte 76) = none
        if let Some((name, slot)) = send_peer {
            w.add(20).write_volatile(slot);               // send_peers[0].slot     (byte 80)
            w.add(21).write_volatile(name.len() as u32);  // send_peers[0].name_len (byte 84)
            let np = base.add(88);                        // send_peers[0].name     (byte 88)
            let nb = name.as_bytes();
            for i in 0..nb.len().min(24) { np.add(i).write_volatile(nb[i]); }
        }
    }
}

/// Commit a loaded service as a scheduled USER task on core 0, using kernel stack `kstack_idx`.
///
/// SAFETY: single-threaded boot; `STACKS[kstack_idx]` is a distinct static; `svc.slot` is freshly
/// reserved by the loader; the ctx has been written and the kernel identity filled by the caller.
unsafe fn commit_user(svc: &super::spawn::RawService, name: &'static str, kstack_idx: usize,
                      endpoint: Option<crate::ipc::EndpointId>) {
    unsafe {
        let kstack_top = (core::ptr::addr_of_mut!(STACKS.0[kstack_idx]) as usize + KSTACK) as *mut u8;
        let ctx = TaskContext::new_user(kstack_top, svc.entry as u64, USER_STACK_TOP as u64, svc.pt_root as u64);
        crate::task::scheduler::commit_task(svc.slot, name, ctx, false, kstack_top as u64, endpoint);
    }
}

/// Bring up the neutral subsystems, wire a `ping` -> `pong` endpoint, commit both as scheduled USER
/// tasks, and enter `scheduler::run(0)`. Does not return. See the module KNOWN BUG note.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);

    // --- 1. Create pong's endpoint (owner on core 0). ---
    let ep = crate::ipc::alloc_endpoint_id();
    let rid = ResourceId::from(ep);
    let gen = next_generation();
    register_resource_at_gen(rid, gen);
    crate::ipc::routing::register(ep, 0, gen);
    crate::ipc::names::register("pong", ep);

    // --- 2. pong (receiver): a RECV cap to the endpoint at cap-slot 1 (LOG_WRITE is slot 0). ---
    let pong = match super::spawn::load_service_raw(PONG_ELF, &[mint_cap(rid, Rights::RECV)]) {
        Some(s) => s,
        None => { pl011_write(b"sched-ipc: pong failed to load - halting\r\n"); halt(); }
    };
    write_ipc_ctx(pong.ctx_frame, /*recv_slot=*/1, /*send_peer=*/None);
    // SAFETY: pt_root is pong's freshly-built L1, not yet in use.
    unsafe { super::page_tables::fill_kernel_identity(pong.pt_root); }
    // SAFETY: see commit_user; pong owns `ep`, so record it for the death-cleanup path.
    unsafe { commit_user(&pong, "pong", 0, Some(ep)); }

    // --- 3. ping (sender): a SEND cap to pong's endpoint at cap-slot 1, wired as send-peer "pong". ---
    let ping = match super::spawn::load_service_raw(PING_ELF, &[mint_cap(rid, Rights::SEND)]) {
        Some(s) => s,
        None => { pl011_write(b"sched-ipc: ping failed to load - halting\r\n"); halt(); }
    };
    write_ipc_ctx(ping.ctx_frame, /*recv_slot=*/u32::MAX, /*send_peer=*/Some(("pong", 1)));
    // SAFETY: pt_root is ping's freshly-built L1, not yet in use.
    unsafe { super::page_tables::fill_kernel_identity(ping.pt_root); }
    // SAFETY: see commit_user; ping owns no endpoint.
    unsafe { commit_user(&ping, "ping", 1, None); }

    pl011_write(b"sched-ipc: ping + pong committed; endpoint wired (SEND->ping, RECV->pong).\r\n");

    // Make every service page-table descriptor visible to the non-cacheable walker ONCE.
    // SAFETY: pure cache maintenance; all page tables are built by this point.
    unsafe { super::page_tables::clean_invalidate_dcache_all(); }

    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-ipc: entering scheduler::run(0) - watch for 'pong: received'.\r\n");
    crate::task::scheduler::run(0)
}

fn halt() -> ! {
    loop {
        // SAFETY: WFI is always architecturally valid.
        unsafe { core::arch::asm!("wfi") }
    }
}
