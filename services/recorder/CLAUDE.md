# services/recorder/

Drains the `events` log to a file, so a capture outlives the screen. **Spawned on demand, never at
boot, and deliberately not restarted.**

Driven entirely through the shell: `events persist start|stop|status`. Nothing else talks to it.

## Why this is not part of `events`

The question that decides it is never "does this component block" but **"who else stops when it
does"**.

Writing to disk blocks: a file write is a request/reply to `fs` and the caller waits for the answer.
`events` is a single-threaded recv loop, so while it waited it would not be draining its endpoint - and
every service's log copies, trace events and metric samples would pile into its 16-deep queue and be
dropped. On a healthy disk that is milliseconds. On a **sick** one it is the full deadline, repeatedly,
which is exactly when the interesting events are happening. `events` would go blind at the moment it is
most needed.

This service has the identical blocking problem and it does not matter, because **nothing depends on
it**. Disk stalls, `recorder` stalls, `events` keeps its window, and the operator can still read what
happened. The dependency points the right way: `recorder` needs `events` and `fs`; neither needs
`recorder`.

That is also `docs/logging.md`'s rule in its concrete form - `events` may hold bounded VOLATILE state
and must never acquire a durable-storage dependency, because a service that reports a storage failure
must not be downstream of storage.

## Why it is not restarted on death

A respawned recorder would not know its target path. It would be **alive and writing nothing** while
`events persist status` said "running" - worse than dead, because dead is visible.

Instead the capture file opens with a header line and closes with a footer, so a file with a header and
no footer says plainly that the recorder died. Staying out of the kernel's managed-service lists is
also what keeps this whole feature a zero-kernel-change one.

## Bounded by construction, not by counting

`fs` allocates a file's whole extent up front (`OP_WRITE_NEW`), so a capture is a fixed size chosen
when it starts. There is no check to get wrong and no way for it to grow until the disk is full and
take the filesystem down - the failure that would turn a troubleshooting tool into an outage.

`PIECES` files rotate (2 today, one constant). Total disk use is the budget, forever. It **rotates
rather than stopping**, because stopping keeps the wrong half: a fixed file that stops when full
preserves the start of a session and discards the crash at the end, and catching the end is usually why
the capture was running.

Rotation **renames**, so `/log.txt` is always the newest piece and `/log.txt.1` the one before. An
earlier version alternated between the two names, which meant the current file depended on a rotation
count only `status` could tell you.

## Two things that had to be learned the hard way

**The file must be zero-filled on creation.** `OP_WRITE_NEW` allocates the extent but writes no data
blocks, so everything past the last chunk written had a stored CRC of zero and `fs` correctly refused
the whole file:

```
fs: data block CRC mismatch at lba 4229 (stored 0x00000000, actual 0x0fbb6d54) - refusing
read: storage error
```

Every capture was unretrievable, and the tests missed it because they asserted that BYTES WERE WRITTEN
and never that they could be read back. The selfcheck asserts the read now.

**`fs` replies `[tag, status]`, not `[status]`.** Reading byte 0 as the status made every successful
write look like a failure - the file was created AND the service reported that it could not be. Both
halves convincing, which is the worst kind of wrong.

## Coverage is measured, never promised

A duration (`7d`) is converted to bytes with an assumed fill rate, which is a prediction about how
chatty the machine will be - and the machine decides that. So the service carries elapsed time and
lifetime bytes, and `status` reports the real rate and what the budget buys at it. Ask for a week on a
box four times chattier than assumed and it answers `~2d`, rather than letting the operator discover it
when the log they needed had already rotated away.

## Restartability

**Not restarted, on purpose** (see above). Its death costs the capture and nothing else: `events` keeps
its volatile window, and every line also reached serial and the kernel ring by syscall before the
recorder ever saw a copy.

Full treatment: `utilities/47_events.md`, `docs/observability.md` §9b.
