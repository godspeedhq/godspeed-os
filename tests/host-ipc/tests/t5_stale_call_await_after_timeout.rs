// SPDX-License-Identifier: GPL-2.0-only
// INVARIANT I-AWAIT (routing.rs:83-90, 98-99): CALL_AWAIT_EP[t] != 0 only while task t is blocked
// in a Call awaiting that endpoint.
//
// CallDeadline's timeout exit (dispatch.rs:1632-1633) breaks right after call_dequeue step 3 set both
// the blocked_receiver and the call record (routing.rs:593-596) and clears neither; only
// clear_wake_deadline runs (dispatch.rs:1644). FAILED on 537e6861.
// FIXED 2026-10-27: do_call now withdraws both on every exit (routing::withdraw_receiver, then
// routing::clear_call_await). dispatch.rs is not host-compiled, so `timed_out` makes those calls.
use host_ipc::*;
use host_ipc::ipc::{routing, message::IpcError};

/// What do_call does when its deadline passes (dispatch.rs, after the wait loop).
fn timed_out(mailbox: host_ipc::ipc::endpoint::EndpointId, slot: usize) {
    routing::withdraw_receiver(mailbox, slot);
    routing::clear_call_await(slot);
}

#[test]
fn t5a_timed_out_call_is_withdrawn() {
    let s = ep(180); let rc = ep(181);
    routing::register(s, 0, g(5));
    routing::register(rc, 0, g(6));
    assert!(matches!(routing::call_dequeue(rc, g(6), s, g(5), 60), Err(IpcError::QueueEmpty)));
    timed_out(rc, 60);
    // ... deadline passes, RECV_TIMED_OUT returned to user space, task 60 goes on with other work.
    assert_eq!(routing::call_await_endpoint(60), 0,
        "I-AWAIT violated: task 60 left its Call but is still recorded as awaiting endpoint {}", routing::call_await_endpoint(60));
}

#[test]
fn t5b_later_death_of_old_target_wakes_no_one() {
    let s = ep(182); let rc = ep(183);
    routing::register(s, 0, g(5));
    routing::register(rc, 0, g(6));
    let _ = routing::call_dequeue(rc, g(6), s, g(5), 61);
    timed_out(rc, 61);
    routing::kill_endpoint(s);
    // The kill path (scheduler.rs ~2635-2637) will wake_by_slot(61, -12) for every waiter returned.
    let w = routing::take_call_waiter(s);
    assert_eq!(w, None, "spurious ReplyDead: the death of an endpoint task 61 stopped waiting for returns waiter {:?}; task 61 is woken with -12 in whatever it is doing now (a recv returns -12; a blocked send returns -12 though its message is still pending, scheduler.rs:3308)", w);
}

#[test]
fn t5c_late_reply_does_not_wake_the_timed_out_caller() {
    let s = ep(184); let rc = ep(185);
    routing::register(s, 0, g(5));
    routing::register(rc, 0, g(6));
    let _ = routing::call_dequeue(rc, g(6), s, g(5), 62);
    timed_out(rc, 62);
    let w = routing::enqueue(rc, msg(0x01, 184), g(6), None).unwrap();
    assert_eq!(w, None, "late reply requests a wake of slot {:?}, which is no longer receiving on this mailbox", w);
}
