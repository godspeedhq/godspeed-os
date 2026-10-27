// SPDX-License-Identifier: GPL-2.0-only
// Positive controls. These MUST pass; if one fails, the harness is wrong or routing broke what works.
// They are also what a mutation of the blocked-sender record must break:
// t0_single_blocked_sender_is_woken_on_dequeue and t0_kill_wakes_single_blocked_sender must FAIL.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
fn t0_single_blocked_sender_is_woken_on_dequeue() {
    let e = ep(200);
    routing::register(e, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
    // Sender in slot 5 blocks (dispatch.rs:374-378).
    assert!(matches!(routing::enqueue(e, msg(0xA1, 301), g(1), Some(5)), Err(IpcError::QueueFull)));
    // Receiver takes one: the pending message is promoted and slot 5 is returned for waking.
    let (_m, woke) = routing::dequeue(e, g(1), None).unwrap();
    assert_eq!(woke, Some(5));
    assert!(drain_tags(e, g(1)).contains(&0xA1));
}

#[test]
fn t0_kill_wakes_single_blocked_sender() {
    let e = ep(201);
    routing::register(e, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
    assert!(matches!(routing::enqueue(e, msg(0xA1, 301), g(1), Some(6)), Err(IpcError::QueueFull)));
    let (_rx, tx) = routing::kill_endpoint(e);
    assert_eq!(tx, Some(6));
}

#[test]
fn t0_reply_matched_and_replydead_on_registered_waiter() {
    let r = ep(202); let s = ep(203);
    routing::register(r, 0, g(1));
    routing::register(s, 0, g(2));
    // Caller slot 7 awaits a reply from s.
    assert!(matches!(routing::call_dequeue(r, g(1), s, g(2), 7), Err(IpcError::QueueEmpty)));
    // An unrelated message does not satisfy the call; the reply from s does.
    routing::enqueue(r, msg(0x55, 999), g(1), None).unwrap();
    routing::enqueue(r, reply(0x66, 203, 2), g(1), None).unwrap();
    let (m, _) = routing::call_dequeue(r, g(1), s, g(2), 7).unwrap();
    assert_eq!(m.payload_bytes()[0], 0x66);
    // A registered waiter is found when s dies.
    assert!(matches!(routing::call_dequeue(r, g(1), s, g(2), 7), Err(IpcError::QueueEmpty)));
    routing::kill_endpoint(s);
    assert_eq!(routing::take_call_waiter(s), Some(7));
    // And a call to an already-dead target refuses at once.
    assert!(matches!(routing::call_dequeue(r, g(1), s, g(2), 7), Err(IpcError::ReplyDead)));
}

#[test]
fn t0_stale_generation_send_is_endpoint_dead() {
    let e = ep(204);
    routing::register(e, 0, g(10));
    routing::kill_endpoint(e);
    routing::register(e, 0, g(20)); // respawn, reused id, fresh generation
    assert!(matches!(routing::enqueue(e, msg(1, 300), g(10), None), Err(IpcError::EndpointDead)));
    assert!(routing::enqueue(e, msg(1, 300), g(20), None).is_ok());
}
