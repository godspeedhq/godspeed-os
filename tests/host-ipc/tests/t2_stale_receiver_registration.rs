// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-RCV: slot t is recorded as blocked_receiver of endpoint E only while task t is blocked
// in a receive of E (routing.rs:458-461 "the caller must immediately call block_and_reschedule").
//
// handle_try_recv passes Some(my_slot) (dispatch.rs:516) and returns TRY_RECV_EMPTY without
// blocking (dispatch.rs:530); handle_recv_timeout re-registers and then breaks on the deadline
// (dispatch.rs:568, 581-583). Neither clears the registration. EXPECTED ON 537e6861: FAIL.
//
// The second half models the scheduler steps that are NOT host-compiled (stated, not executed):
//   wake_by_slot(t, 0) moves ANY non-Dead state to Ready, including BlockedOnSend and Running
//   (scheduler.rs:2336-2385); block_and_reschedule(BlockedOnSend) does not consume
//   TASK_WAKE_PENDING (scheduler.rs:3231) and returns TASK_WAKEUP_ERR when its CAS fails or when
//   it is woken (scheduler.rs:3247-3255, 3308); handle_send returns that value (dispatch.rs:378).
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
#[ignore = "backlog/90 D2: try_recv and a timed-out recv_timeout leave the caller registered - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t2a_try_recv_leaves_task_registered_as_blocked_receiver() {
    let et = ep(220); // task T (slot 20) owns et
    routing::register(et, 0, g(1));
    // T: try_recv on an empty endpoint (dispatch.rs:516).
    assert!(matches!(routing::dequeue(et, g(1), Some(20)), Err(IpcError::QueueEmpty)));
    // T returns to user space (TRY_RECV_EMPTY). Later a client sends to et:
    let r = routing::enqueue(et, msg(0x01, 500), g(1), None).unwrap();
    assert_eq!(r, None, "I-RCV violated: enqueue asks the dispatcher to wake slot {:?}, which is not blocked on this endpoint (dispatch.rs:369-370 wakes it)", r);
}

#[test]
#[ignore = "backlog/90 D2: try_recv and a timed-out recv_timeout leave the caller registered - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t2b_spurious_wake_turns_into_silent_loss_of_a_blocking_send() {
    let et = ep(221); let ex = ep(222);
    routing::register(et, 0, g(1));
    routing::register(ex, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(ex, msg(i as u8, 300), g(1), None).unwrap(); }
    // 1. T (slot 21) try_recv's its own endpoint: stale registration.
    let _ = routing::dequeue(et, g(1), Some(21));
    // 2. T does a BLOCKING send of M1 to the full ex: recorded blocked sender, blocks BlockedOnSend.
    assert!(matches!(routing::enqueue(ex, msg(0xE1, 221), g(1), Some(21)), Err(IpcError::QueueFull)));
    // 3. A client sends to et: routing returns slot 21 to wake. [scheduler, modelled] wake_by_slot(21, 0):
    //    BlockedOnSend -> Ready, TASK_WAKEUP_ERR=0; handle_send returns 0: T is told M1 was sent.
    let w = routing::enqueue(et, msg(0x02, 500), g(1), None).unwrap();
    assert_eq!(w, Some(21));
    // 4. T, believing M1 delivered, sends M2 to ex (still full): overwrites pending_send.
    assert!(matches!(routing::enqueue(ex, msg(0xE2, 221), g(1), Some(21)), Err(IpcError::QueueFull)));
    // 5. ex's owner drains.
    let tags = drain_tags(ex, g(1));
    assert!(tags.contains(&0xE2));
    assert!(tags.contains(&0xE1), "silent loss: T was told M1 (0xE1) was sent (returned 0) and it was discarded; delivered={:x?}", tags);
}

#[test]
#[ignore = "backlog/90 D2: try_recv and a timed-out recv_timeout leave the caller registered - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t2c_recv_timeout_expiry_leaves_registration() {
    let et = ep(223);
    routing::register(et, 0, g(1));
    // handle_recv_timeout: first dequeue registers and blocks; the timer wakes it; the loop dequeues
    // again (registers again) and then sees the deadline passed and breaks (dispatch.rs:567-583).
    let _ = routing::dequeue(et, g(1), Some(23));
    let _ = routing::dequeue(et, g(1), Some(23));
    // RECV_TIMED_OUT returned to user space. A late message arrives:
    let r = routing::enqueue(et, msg(0x03, 500), g(1), None).unwrap();
    assert_eq!(r, None, "I-RCV violated after a timed-out RecvTimeout: wake requested for slot {:?}", r);
}
