// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-RD (CLAUDE.md 8.2 "wakes with the reply, or - if the replier dies before replying -
// with ReplyDead, so it never hangs"; glossary "so it never hangs"): if the endpoint INSTANCE a Call's
// request was enqueued to dies without replying, the caller observes ReplyDead.
//
// call_dequeue step 2 tests liveness by endpoint ID only (routing.rs:582-585), not by the generation
// the request was sent at; endpoint ids are reused LIFO (ipc/mod.rs:45-53) and the kill path frees
// the primary id last (scheduler.rs ~2748), so a respawn takes the SAME id back.
// EXPECTED ON 537e6861: FAIL.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
#[ignore = "backlog/90 D5: ReplyDead keyed on a bare, reused endpoint id - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t3_replydead_lost_when_target_id_is_reused_before_caller_registers() {
    let s = ep(150);   // server S, first incarnation at generation 5
    let rc = ep(151);  // caller C's reply mailbox; C is slot 30
    routing::register(s, 0, g(5));
    routing::register(rc, 0, g(6));
    for i in 0..QUEUE_DEPTH { routing::enqueue(s, msg(i as u8, 300), g(5), None).unwrap(); }
    // do_call step 4 (dispatch.rs:1557, 1565-1568): S's queue is full, C blocks as a sender.
    assert!(matches!(routing::enqueue(s, msg(0xCA, 0 /*C has no primary in this model*/), g(5), Some(30)),
                     Err(IpcError::QueueFull)));
    // S receives once: C's request is promoted into S's queue, C is woken (Ready, not yet running).
    let (_m, w) = routing::dequeue(s, g(5), None).unwrap();
    assert_eq!(w, Some(30));
    // S dies before reaching C's request. Kill path: kill_endpoint drains it (request lost), then
    // take_call_waiter finds nobody, because C has not reached call_dequeue yet.
    routing::kill_endpoint(s);
    assert_eq!(routing::take_call_waiter(s), None);
    // Supervisor respawns S; alloc_endpoint_id hands back id 150; fresh generation 9.
    assert!(routing::try_register(s, 0, g(9)));
    // C finally runs: block_and_reschedule returned 0, so do_call falls through to await the reply.
    let r = routing::call_dequeue(rc, g(6), s, 30);
    assert!(matches!(r, Err(IpcError::ReplyDead)),
        "I-RD violated: the instance C's request went to is dead, but call_dequeue returned {:?} (C registers and blocks; the new S never saw the request; with Call 41 the deadline is 0 = forever, dispatch.rs:1586-1588)",
        r.map(|_| ()));
}
