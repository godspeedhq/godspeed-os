// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-BLK (derived from CLAUDE.md 8.4/8.6 and routing.rs:384-394, independently of the
// single-slot implementation): every task t for which `enqueue(.., Some(t))` returned QueueFull is
// blocked in BlockedOnSend (dispatch.rs:374-378, 1448-1451, 1565-1568, 1784-1786, 330-331), and
// must later be returned for waking by exactly one dequeue (its message delivered) or by
// kill_endpoint (woken with EndpointDead). Its message must be delivered or reported lost.
//
// FAILED on 537e6861 (one blocked_sender/pending_send per endpoint, overwritten unconditionally);
// FIXED 2026-10-27 by one blocked-send record per task, served oldest first (routing.rs `PENDING`).
// t1b's first assertion was written against the overwrite (it expected the kill to name the SECOND
// sender as the only one); it now states I-BLK itself: both are woken, oldest first, and no other.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

fn fill(e: host_ipc::ipc::endpoint::EndpointId) {
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
}

#[test]
fn t1a_second_blocked_sender_does_not_erase_the_first() {
    let e = ep(210);
    routing::register(e, 0, g(1));
    fill(e);
    assert!(matches!(routing::enqueue(e, msg(0xA1, 401), g(1), Some(10)), Err(IpcError::QueueFull)));
    assert!(matches!(routing::enqueue(e, msg(0xB1, 402), g(1), Some(11)), Err(IpcError::QueueFull)));
    // The receiver drains everything.
    let mut woken = Vec::new();
    let mut tags = Vec::new();
    while let Ok((m, w)) = routing::dequeue(e, g(1), None) {
        tags.push(m.payload_bytes()[0]);
        if let Some(s) = w { woken.push(s); }
    }
    assert!(woken.contains(&11), "control: B is woken");
    assert!(woken.contains(&10), "I-BLK violated: sender slot 10 was recorded blocked and is never woken (hangs in BlockedOnSend); woken={:?}", woken);
    assert!(tags.contains(&0xA1), "I-BLK violated: slot 10's message 0xA1 was accepted (sender blocked, not refused) and silently discarded; delivered={:x?}", tags);
}

#[test]
fn t1b_kill_wakes_every_blocked_sender_oldest_first() {
    let e = ep(211);
    routing::register(e, 0, g(1));
    fill(e);
    let _ = routing::enqueue(e, msg(0xA1, 401), g(1), Some(10));
    let _ = routing::enqueue(e, msg(0xB1, 402), g(1), Some(11));
    // The kill path: kill_endpoint, then take_blocked_sender until none remain (scheduler.rs).
    let (rx, tx) = routing::kill_endpoint(e);
    assert_eq!(rx, None);
    assert_eq!(tx, Some(10), "I-BLK: the OLDEST blocked sender comes first");
    assert_eq!(routing::take_blocked_sender(e), Some(11), "I-BLK violated: the second sender is never woken");
    assert_eq!(routing::take_blocked_sender(e), None, "no sender is woken twice");
}

#[test]
fn t1c_console_writers_do_not_erase_each_other() {
    // dispatch.rs:305-343 deliver_to_console_service -> enqueue_from_kernel_blocking.
    let console = ep(212);
    routing::register(console, 0, g(1));
    fill(console);
    assert!(matches!(routing::enqueue_from_kernel_blocking(console, msg(0xC1, 0), 20), Err(IpcError::QueueFull)));
    assert!(matches!(routing::enqueue_from_kernel_blocking(console, msg(0xC2, 0), 21), Err(IpcError::QueueFull)));
    let mut woken = Vec::new();
    while let Ok((_m, w)) = routing::dequeue(console, g(1), None) { if let Some(s) = w { woken.push(s); } }
    assert!(woken.contains(&20), "I-BLK violated on the console path: writer slot 20 never woken; dispatch.rs:325-331 says 'so this can never hang'; woken={:?}", woken);
}

#[test]
fn t1d_many_blocked_senders_are_delivered_in_order_and_each_woken_once() {
    let e = ep(213);
    routing::register(e, 0, g(1));
    fill(e);
    for (i, slot) in (30..35).enumerate() {
        assert!(matches!(routing::enqueue(e, msg(0xD0 + i as u8, 0), g(1), Some(slot)), Err(IpcError::QueueFull)));
    }
    let mut woken = Vec::new();
    let mut tags = Vec::new();
    while let Ok((m, w)) = routing::dequeue(e, g(1), None) {
        tags.push(m.payload_bytes()[0]);
        if let Some(s) = w { woken.push(s); }
    }
    assert_eq!(woken, vec![30, 31, 32, 33, 34], "each blocked sender woken once, in the order it blocked");
    assert_eq!(&tags[QUEUE_DEPTH..], &[0xD0, 0xD1, 0xD2, 0xD3, 0xD4], "their messages follow the queue, in order");
}
