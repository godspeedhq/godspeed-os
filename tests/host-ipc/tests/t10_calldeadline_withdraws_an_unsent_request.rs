// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-CALL-BOUND (CLAUDE.md 8.2, the CallDeadline amendment: the bound is "what 26.6 asks of every
// wait"): a CallDeadline whose request is blocked on a full queue when its deadline passes WITHDRAWS the
// request, and a request that was delivered first is not withdrawn.
//
// The trace (backlog/90 D4): the deadline was armed only after the send, so a call to a live peer whose
// queue was full and not draining waited for the peer however long that took. FIXED 2026-10-30: dispatch.rs
// (not host-compiled) arms the deadline first and, when it passes, calls `withdraw_blocked_send`. This pins
// the half it relies on: withdrawal and delivery decide on one record, so exactly one of them happens.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
fn t10_a_request_still_waiting_at_the_deadline_is_withdrawn_and_never_delivered() {
    let e = ep(210);
    routing::register(e, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
    // Caller slot 80 blocks sending its request.
    assert!(matches!(routing::enqueue(e, msg(0xD4, 301), g(1), Some(80)), Err(IpcError::QueueFull)));
    assert!(routing::is_blocked_sending(80));
    // The deadline passes first: the request is withdrawn.
    assert!(routing::withdraw_blocked_send(80), "I-CALL-BOUND violated: a waiting request was not withdrawn");
    assert!(!routing::is_blocked_sending(80));
    // The peer drains: nobody is woken for it, and the withdrawn request never arrives.
    let (_m, woke) = routing::dequeue(e, g(1), None).unwrap();
    assert_eq!(woke, None, "a withdrawn request's caller was woken as if it had been delivered");
    assert!(!drain_tags(e, g(1)).contains(&0xD4), "a withdrawn request was delivered");
}

#[test]
fn t10_a_request_delivered_before_the_deadline_is_not_withdrawn() {
    let e = ep(211);
    routing::register(e, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
    assert!(matches!(routing::enqueue(e, msg(0xD5, 301), g(1), Some(81)), Err(IpcError::QueueFull)));
    // The peer takes one first: the request is promoted into the queue and its caller woken.
    let (_m, woke) = routing::dequeue(e, g(1), None).unwrap();
    assert_eq!(woke, Some(81));
    // The deadline then passes: there is nothing to withdraw, so the call goes on to await its reply.
    assert!(!routing::withdraw_blocked_send(81), "a delivered request was withdrawn");
    assert!(drain_tags(e, g(1)).contains(&0xD5));
}
