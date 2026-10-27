// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-CORR (message.rs:24-36, routing.rs:512-517, queue.rs:77-86): a Call takes as its reply
// only a message sent by the task that owns `target` NOW; "a stale reply from a dead incarnation -
// whose endpoint id differs" stays queued.
//
// sender_ep is a bare endpoint id (dispatch.rs:741) with no generation, and ids are reused LIFO
// (ipc/mod.rs:45-53), so the dead incarnation's id does NOT differ in the common restart case.
// EXPECTED ON 537e6861: FAIL (both tests).
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError};

#[test]
#[ignore = "backlog/90 D5: reply correlation keyed on a bare, reused endpoint id - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t4a_late_reply_of_dead_incarnation_is_accepted_by_call_to_respawn() {
    let s = ep(160); let rc = ep(161); // caller C = slot 40
    routing::register(s, 0, g(5));
    routing::register(rc, 0, g(6));
    // C's CallDeadline to S timed out (call_dequeue registered, deadline passed: dispatch.rs:1632-1633).
    let _ = routing::call_dequeue(rc, g(6), s, 40);
    // S answers late: its reply lands in C's mailbox, stamped sender_ep = 160.
    routing::enqueue(rc, msg(0x01 /*answer to request #1*/, 160), g(6), None).unwrap();
    // S is killed and respawned at the same id (LIFO reuse) with a new generation.
    routing::kill_endpoint(s);
    let _ = routing::take_call_waiter(s); // the kill path wakes C spuriously here (see t5)
    assert!(routing::try_register(s, 0, g(9)));
    // C makes request #2 to the NEW S; before the new S has seen it, C looks for its reply.
    let r = routing::call_dequeue(rc, g(6), s, 40);
    match r {
        Ok((m, _)) => panic!("I-CORR violated: request #2 to the respawned S was answered with the DEAD incarnation's reply to request #1 (tag {:#x}); the comment's 'endpoint id differs' does not hold under LIFO id reuse", m.payload_bytes()[0]),
        Err(e) => assert!(matches!(e, IpcError::QueueEmpty)),
    }
}

#[test]
#[ignore = "backlog/90 D5: reply correlation keyed on a bare, reused endpoint id - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t4b_request_from_dead_client_is_accepted_as_reply_from_new_owner_of_its_id() {
    // A service S2 with NO reply mailbox (spawn refuses one past the reserve: routing.rs:286-328,
    // task/mod.rs ~1749-1756) awaits replies on its primary endpoint, which also gets requests.
    let s2 = ep(170);
    routing::register(s2, 0, g(5));
    // Client K (primary id 171) sends S2 a request, then dies; id 171 is freed and reused by N.
    routing::enqueue(s2, msg(0x4B /*K's request*/, 171), g(5), None).unwrap();
    let n = ep(171);
    routing::register(n, 0, g(7)); // N, the new owner of id 171
    // S2 calls N (request delivered to N), then awaits N's reply on its primary endpoint.
    let r = routing::call_dequeue(s2, g(5), n, 50);
    match r {
        Ok((m, _)) => panic!("I-CORR violated: S2's call to N returned the dead client K's REQUEST (tag {:#x}) as N's reply", m.payload_bytes()[0]),
        Err(e) => assert!(matches!(e, IpcError::QueueEmpty)),
    }
}
