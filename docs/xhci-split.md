# Splitting input from storage in the `xhci` service

## The problem, stated exactly

The in-kernel driver never had an input-latency problem because it never had one loop. Keyboard
polling was called from the TIMER TICK and disk reads from the SYSCALL path - two independent
execution contexts. A disk transfer could take as long as it liked and could not delay a keystroke,
because the timer interrupt preempted it. The kernel got that scheduling for free by being the
kernel. (That in-kernel driver was deleted when the stack moved to `services/xhci`, so this is the
shape being replaced, not code to go and read.)

The userspace service does both in ONE pass:

```
loop {
    recv_timeout(deadline)      // wake
    serve block requests        // <- disk work
    drain event ring            // HID reports <- input work
    poll hub ports              // hot-plug
}
```

Every constant tuned during the port - the request budget, the drain bounds, the log rate-limits -
was an attempt to hand-schedule two workloads that the kernel version had preemption for. Each one
worked briefly and the latency reappeared elsewhere, because the problem is structural, not a tuning
parameter. **A single-threaded loop serving a latency-critical client and a throughput-critical one
has no correct budget.**

## What must change

Storage work must not run on the pass that polls HID. Three candidate shapes, in preference order.

### 1. Deliver HID reports DURING disk waits (smallest real fix)

`msc::await_on_slot` already SEES keyboard transfer events while waiting for a BOT completion. It
FILES each one in the `eaten` mailbox keyed by slot, and the poll loop calls `deliver_hid_report`
for every filed slot before re-arming that endpoint - so the report is no longer thrown away. The
residual latency is WHEN: the keystroke lands after the disk pass rather than inside it, and block
serving is held to one request per pass to bound how long that is. Both are interim measures and the
code says so.

The full fix is to give `await_on_slot` a callback invoked when an event for a non-disk slot arrives,
and have the poll loop pass a closure that decodes the report and pushes it to the console
immediately - removing the coupling rather than bounding it.

- Keystrokes land during disk I/O, which is the whole complaint.
- No new service, no new capability, no protocol change.
- Cost: the closure needs `devs`, `kb_rep`, `kb_caps` and `dma` while `disk` is also borrowed
  mutably. Expect to restructure the poll loop's state into a small struct so one `&mut` covers it.
  This is the real work and it is borrow-checker shaped, not conceptual.

### 2. A separate `usb-storage` service

Matches how the kernel split it, and is the cleanest boundary - but the two would share one
controller, one event ring and one DMA arena. Only one service can own those, so this needs the xhci
service to proxy bulk transfers for the storage service, which reintroduces the coupling at the IPC
layer and adds a hop per sector.

Not obviously better than (1) and considerably more moving parts.

### 3. Make the block work resumable

`await_on_slot` returns "not yet" after one HID interval, the loop polls input, then resumes the
outstanding transfer. Correct in principle; requires BOT state to survive across passes, which is a
larger rewrite of `msc.rs` than (1).

## Recommendation

Do (1). It is the smallest change that removes the structural cause, and it is testable the same way
everything else was: type continuously during a `read` and during a `drives` with no stick attached.
