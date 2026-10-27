// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-RCV: slot t is recorded as blocked_receiver of endpoint E only while task t is blocked
// in a receive of E (routing.rs:458-461 "the caller must immediately call block_and_reschedule").
//
// handle_try_recv passes Some(my_slot) (dispatch.rs:516) and returns TRY_RECV_EMPTY without
// blocking (dispatch.rs:530); handle_recv_timeout re-registers and then breaks on the deadline
// (dispatch.rs:568, 581-583). Neither clears the registration. FAILED on 537e6861.
// FIXED 2026-10-27, in dispatch.rs, which is not host-compiled, so these tests make the calls it now
// makes: try_recv dequeues with None (it never blocks, so it never registers), and RecvTimeout calls
// routing::withdraw_receiver on every exit.
//
// The second half models the scheduler steps that are NOT host-compiled (stated, not executed):
//   wake_by_slot(t, 0) moves ANY non-Dead state to Ready, including BlockedOnSend and Running
//   (scheduler.rs:2336-2385); block_and_reschedule(BlockedOnSend) does not consume
//   TASK_WAKE_PENDING (scheduler.rs:3231) and returns TASK_WAKEUP_ERR when its CAS fails or when
//   it is woken (scheduler.rs:3247-3255, 3308); handle_send returns that value (dispatch.rs:378).
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
fn t2a_try_recv_does_not_register_as_blocked_receiver() {
    let et = ep(220); // task T (slot 20) owns et
    routing::register(et, 0, g(1));
    // T: try_recv on an empty endpoint (dispatch.rs:516).
    assert!(matches!(routing::dequeue(et, g(1), None), Err(IpcError::QueueEmpty)));
    // T returns to user space (TRY_RECV_EMPTY). Later a client sends to et:
    let r = routing::enqueue(et, msg(0x01, 500), g(1), None).unwrap();
    assert_eq!(r, None, "I-RCV violated: enqueue asks the dispatcher to wake slot {:?}, which is not blocked on this endpoint (dispatch.rs:369-370 wakes it)", r);
}

#[test]
fn t2b_no_spurious_wake_reaches_a_blocking_send() {
    let et = ep(221); let ex = ep(222);
    routing::register(et, 0, g(1));
    routing::register(ex, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(ex, msg(i as u8, 300), g(1), None).unwrap(); }
    // 1. T (slot 21) try_recv's its own endpoint: stale registration.
    let _ = routing::dequeue(et, g(1), None);
    // 2. T does a BLOCKING send of M1 to the full ex: recorded blocked sender, blocks BlockedOnSend.
    assert!(matches!(routing::enqueue(ex, msg(0xE1, 221), g(1), Some(21)), Err(IpcError::QueueFull)));
    // 3. A client sends to et: routing returns slot 21 to wake. [scheduler, modelled] wake_by_slot(21, 0):
    //    BlockedOnSend -> Ready, TASK_WAKEUP_ERR=0; handle_send returns 0: T is told M1 was sent.
    let w = routing::enqueue(et, msg(0x02, 500), g(1), None).unwrap();
    assert_eq!(w, None, "no wake for T: it is not receiving on et, it is blocked sending to ex");
    // 4. ex's owner drains: M1 is delivered, and T is woken for it, once.
    let mut woken = Vec::new();
    let mut tags = Vec::new();
    while let Ok((m, w)) = routing::dequeue(ex, g(1), None) {
        tags.push(m.payload_bytes()[0]);
        if let Some(s) = w { woken.push(s); }
    }
    assert!(tags.contains(&0xE1), "M1 delivered; delivered={:x?}", tags);
    assert_eq!(woken, vec![21], "T woken exactly once, by the delivery of M1");
}

#[test]
fn t2c_recv_timeout_expiry_withdraws_its_registration() {
    let et = ep(223);
    routing::register(et, 0, g(1));
    // handle_recv_timeout: first dequeue registers and blocks; the timer wakes it; the loop dequeues
    // again (registers again) and then sees the deadline passed and breaks (dispatch.rs:567-583).
    let _ = routing::dequeue(et, g(1), Some(23));
    let _ = routing::dequeue(et, g(1), Some(23));
    routing::withdraw_receiver(et, 23); // handle_recv_timeout's exit
    // RECV_TIMED_OUT returned to user space. A late message arrives:
    let r = routing::enqueue(et, msg(0x03, 500), g(1), None).unwrap();
    assert_eq!(r, None, "I-RCV violated after a timed-out RecvTimeout: wake requested for slot {:?}", r);
}
