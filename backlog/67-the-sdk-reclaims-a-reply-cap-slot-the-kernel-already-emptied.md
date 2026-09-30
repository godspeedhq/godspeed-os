# 67. After a wait times out, the SDK removes a cap slot the kernel already emptied at send - and destroys the cap that arrived in it since

**Status: OPEN - found 2026-09-30 while chasing `backlog/66`, by reading; the onset of every slow phase in that item matches it. Not fixed: the change touches thirteen sites on every service's request path and the operator parked the investigation.**

## The two kernel facts

- A successful `SendWithCap` (syscall 11) **removes the granted cap from the sender's table**
  (`kernel/src/syscall/dispatch.rs`, `current_task_remove_cap(grant_slot)` on every `Ok` branch). The
  reply cap a request carries is therefore gone from the requester's slot the moment the send succeeds.
- A received cap is inserted into the **lowest free slot** (`kernel/src/capability/table.rs`,
  `insert`), and `remove(slot)` takes whatever is there.

## What the SDK does with them

Every bounded request in `sdk/rust/src/service_context.rs` derives a reply cap, sends it, waits, and on
TIMEOUT calls `self.remove_cap(reply_cap)` - "reply never consumed - reclaim its slot". One site says
outright that this is "idempotent if the kernel already moved it out on a successful send". It is not
idempotent. The slot was emptied at send, so the first cap the task RECEIVED during the wait was
inserted into it - and that is the cap `remove_cap` destroys.

Which cap that is depends on the caller, and both cases were seen on the Pi 4:

- **net-stack's sifted waits** stash a client request that arrives mid-exchange (`Displaced`), holding
  its reply cap by slot. A timeout then removes that slot, and "serving a held client request" answers
  into a dead cap. The client (the shell) waits out its patience and re-asks.
- **nic-driver's radio wait** keeps a request that arrives while it waits on `wifi-driver`. A radio
  timeout removes the kept request's cap, and the driver's answer to it fails: `a reply send FAILED -
  the reply cap is dead - the requester stopped waiting before this reply`, logged 11 ms after the
  request was sent, at the exact onset of the slow phase in three boots running.

The sites, all on the timeout path after a successful send: the `_ms_sifted` wait, `request_with_reply`'s
error path (a `ReplyDead` after a successful send), `call_deadline_into`'s `Ok(None)` (three variants),
the seconds-bounded sifted and unsifted waits, the two abortable waits, and the `_ms` wait. The
send-failure paths (`offer_request` / `send_with_cap_by_handle` returned `Err`) are CORRECT: there the
cap was never transferred and must be reclaimed.

## The fix, when it is taken

Remove the reclaim on every path where the send succeeded, and only there. The reply cap is then
owned by the peer, who removes it after answering (`nic-driver` does; `backlog/66`'s reply-cap lesson).
A late reply after a timeout arrives capless and is sifted or dropped as today. No kernel change.

The alternative - the kernel telling a task which slot a received cap went to, so the SDK could check
before removing - is more machinery for the same result.

## Why it is recorded rather than done

It is one line at thirteen sites on the request path of every service on every port, and the item it
was found under was parked with the build that ran well on the card. It should go in on its own, with
a boot on each port, not inside another investigation.
