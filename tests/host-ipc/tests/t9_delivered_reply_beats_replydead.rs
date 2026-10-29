// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-REPLY (CLAUDE.md 8.2: a Call "wakes with the reply, or - if the replier dies before replying -
// with ReplyDead"): a reply that was DELIVERED before the replier died is the answer, not a death.
//
// The trace (backlog/90 D6): X replies to C, waking C with 0; before C runs, X dies, and the kill path still
// finds C awaiting X - the record is cleared only when C runs - and overwrites C's wake with ReplyDead
// (scheduler.rs, `take_call_waiter` then `wake_by_slot(.., -12)`). do_call used to return that wake result,
// so C saw ReplyDead with the reply sitting in its mailbox. FIXED 2026-10-29: do_call ignores the wake
// result and asks call_dequeue again. dispatch.rs is not host-compiled, so this pins the half it relies on:
// in exactly that state, call_dequeue hands back the reply.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError};

#[test]
fn t9_a_reply_delivered_before_the_replier_dies_is_the_answer() {
    let x = ep(190); let rc = ep(191); // caller C = slot 70
    routing::register(x, 0, g(5));
    routing::register(rc, 0, g(6));
    assert!(matches!(routing::call_dequeue(rc, g(6), x, g(5), 70), Err(IpcError::QueueEmpty)));
    // X replies, and C is woken with 0.
    let woken = routing::enqueue(rc, reply(0x01, 190, 5), g(6), None).unwrap();
    assert_eq!(woken, Some(70));
    // X dies before C runs. C's record is still there, so the kill path would wake it with ReplyDead.
    routing::kill_endpoint(x);
    assert_eq!(routing::take_call_waiter(x), Some(70),
        "the overwrite D6 describes needs C still recorded as awaiting X");
    // C runs and asks again: the delivered reply wins over the death.
    let (m, _) = routing::call_dequeue(rc, g(6), x, g(5), 70)
        .expect("I-REPLY violated: the reply X delivered before dying was not returned");
    assert_eq!(m.payload_bytes()[0], 0x01);
    // And only once: with the reply taken and X dead, the next ask is the death.
    assert!(matches!(routing::call_dequeue(rc, g(6), x, g(5), 70), Err(IpcError::ReplyDead)));
}
