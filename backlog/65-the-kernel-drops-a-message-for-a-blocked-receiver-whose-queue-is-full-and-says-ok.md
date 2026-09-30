# 65. The kernel drops a message sent to a blocked receiver whose queue is full, and tells the sender it was delivered

**Status: FIXED 2026-09-30 in `ipc/routing.rs` (the branch returns `QueueFull` and leaves the receiver blocked), with the Pi 4 RNG in the same kernel change; CLOSED once the identity suite and a Pi 4 boot have run on it.**
**Found:** 2026-09-30, on the Pi 4, while explaining why every exchange between `nic-driver` and the
radio was timing out at exactly its bound.

## What the code does

`enqueue_locked` in `kernel/src/ipc/routing.rs`:

```rust
if let Some(slot) = table[idx].blocked_receiver.take() {
    table[idx].queue.enqueue(msg).ok();
    return Ok(Some(slot));
}
```

If the endpoint has a blocked receiver, the message is enqueued with the result DISCARDED and the
sender is told `Ok`. A receiver normally blocks only on an empty queue, so the enqueue cannot fail -
except in the one case where it can: a task blocked in `Call` (§8.2) is a blocked receiver waiting for
ONE specific reply, and every other message sent to it meanwhile goes into its queue. Sixteen of those
and the queue is full while the receiver is still blocked. The seventeenth request, and the reply the
caller is waiting for, both hit this branch: dropped, and the sender told they were delivered.

That is a silent fallback at the kernel boundary (invariant 12; CLAUDE.md 21 rejects a change that
introduces one, and this one predates the rule being written down). The sender cannot know; the
receiver wakes for nothing and re-blocks; the caller waits out its whole deadline for an answer the
kernel accepted and threw away.

## Where it was seen

`observe now` during the first frame-path boot: `nic-driver  BlockRecv  16/16!`, `net-stack  BlockSend`,
`wifi-driver  BlockRecv  0/16`. `nic-driver` was blocked in a call to the radio with a full inbox of
`net-stack` requests it had not got to; the radio's answers were sent, accepted, and never arrived. The
CAUSE of that boot's failure was in `wifi-driver` (a leaked reply cap, fixed the same day, `docs/wifi.md`
41), but the shape it took - every exchange timing out at exactly its bound, with the radio idle - is
this branch. `nic-driver`'s bound on the radio was shortened so its inbox cannot fill behind a slow
radio (`RADIO_MS`), which removes the trigger on that path and not the kernel behaviour.

## What closing it takes

Return `Err(QueueFull)` from that branch instead of `Ok`, leaving the receiver recorded as blocked (it is
still waiting) and recording the sender as blocked if it asked to be, exactly as the non-blocked-receiver
branch below it does. About six lines, no `unsafe`, in the IPC fast path - so it wants the §22 identity
suite and P6/P10 (queue invariants; every send returns exactly one of Ok or a defined error) run on every
port, and a probe that fills a caller's queue behind its own call and asserts the reply's send returns
`QueueFull`. It is held for the next kernel change rather than folded into a userspace flash, because a
kernel edit reopens "is the kernel still correct?" on four boards and this one is not what the boot was
about.
