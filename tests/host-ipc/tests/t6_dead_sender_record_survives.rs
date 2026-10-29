// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-DEAD: after kill_task_by_slot(t) no blocked-send record names t and no routing entry
// names t as blocked_receiver (the slot is reused by spawn; wake_by_slot guards only on TASK_VALID and Dead,
// scheduler.rs:2338-2342, and a reused slot is VALID).
//
// At 537e6861 the kill path killed only the dying task's OWN endpoints (scheduler.rs ~2586-2620: reply
// mailbox, then primary). A task blocked as a SENDER on someone else's endpoint stayed recorded there;
// nothing scrubbed it (no caller of the then `blocked_sender` field outside routing.rs). FAILED on 537e6861.
// FIXED 2026-10-27: the kill path withdraws a dying sender (`routing::forget_blocked_sender`,
// scheduler.rs kill_task_by_slot). Its message was never accepted - the send had not returned - so it
// is dropped and counted, never delivered: this test's old "control" expected it delivered, which
// only the single-slot design did, by accident of leaving the dead record in place.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

#[test]
fn t6_dead_blocked_sender_is_not_woken_after_its_slot_is_reused() {
    let ex = ep(240);
    routing::register(ex, 0, g(1));
    for i in 0..QUEUE_DEPTH { routing::enqueue(ex, msg(i as u8, 300), g(1), None).unwrap(); }
    // Task A (slot 70, primary 241) SendWithCap's to the full ex: the cap is in A's blocked-send record and
    // already removed from A's table (dispatch.rs:1448-1451). A blocks.
    assert!(matches!(routing::enqueue(ex, msg(0x7A, 241), g(1), Some(70)), Err(IpcError::QueueFull)));
    // A is killed. Kill path: kill_endpoint(241) etc. - ex is not A's, so killing A's endpoints leaves
    // its record there; `forget_blocked_sender` is what withdraws it.
    routing::register(ep(241), 0, g(2));
    routing::forget_blocked_sender(70);
    routing::kill_endpoint(ep(241));
    // Slot 70 is reused by a new task B. ex's owner receives:
    let (_m, w) = routing::dequeue(ex, g(1), None).unwrap();
    let tags = drain_tags(ex, g(1));
    assert!(!tags.contains(&0x7A), "a dead sender's never-accepted message is not delivered");
    assert_eq!(w, None, "I-DEAD violated: routing asks to wake slot {:?}, which now belongs to a different task (B gets a spurious wake: a blocked send of B's returns 0 while B's own message is still pending)", w);
}
