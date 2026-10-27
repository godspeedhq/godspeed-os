// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-BLK (derived from CLAUDE.md 8.4/8.6 and routing.rs:384-394, independently of the
// single-slot implementation): every task t for which `enqueue(.., Some(t))` returned QueueFull is
// blocked in BlockedOnSend (dispatch.rs:374-378, 1448-1451, 1565-1568, 1784-1786, 330-331), and
// must later be returned for waking by exactly one dequeue (its message delivered) or by
// kill_endpoint (woken with EndpointDead). Its message must be delivered or reported lost.
//
// EXPECTED ON 537e6861: these tests FAIL. RoutingEntry has ONE blocked_sender/pending_send
// (routing.rs:46-51) and enqueue_locked overwrites it unconditionally (routing.rs:445-449,
// 431-435; enqueue_from_kernel_blocking 715-717, 725-726).
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError, queue::QUEUE_DEPTH};

fn fill(e: host_ipc::ipc::endpoint::EndpointId) {
    for i in 0..QUEUE_DEPTH { routing::enqueue(e, msg(i as u8, 300), g(1), None).unwrap(); }
}

#[test]
#[ignore = "backlog/90 D1: a second blocked sender erases the first - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t1a_second_blocked_sender_erases_first() {
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
#[ignore = "backlog/90 D1: a second blocked sender erases the first - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t1b_kill_does_not_wake_erased_sender() {
    let e = ep(211);
    routing::register(e, 0, g(1));
    fill(e);
    let _ = routing::enqueue(e, msg(0xA1, 401), g(1), Some(10));
    let _ = routing::enqueue(e, msg(0xB1, 402), g(1), Some(11));
    let (rx, tx) = routing::kill_endpoint(e);
    assert_eq!(tx, Some(11), "control: B is woken with EndpointDead");
    // Nothing else in routing names slot 10, so the kill path (scheduler.rs ~2620-2627) cannot wake it.
    assert!(rx == Some(10) || tx == Some(10),
        "I-BLK violated: endpoint died, kill_endpoint returned rx={:?} tx={:?}; slot 10 stays BlockedOnSend forever (no deadline is armed on any send path, scan_timed_wakes wakes BlockedOnRecv only: scheduler.rs:464-472)", rx, tx);
}

#[test]
#[ignore = "backlog/90 D1: a second blocked sender erases the first - reproduces the defect; remove this ignore with its fix (host_test_check.py fails when it passes)"]
fn t1c_console_writers_erase_each_other() {
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
