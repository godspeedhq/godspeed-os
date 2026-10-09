# 70. The shell counts what the radio driver owes it by counting replies in its mailbox, and every peer's replies land there

**Status: CLOSED 2026-10-02 - fix 2 below, chosen by the operator.** Every shell request to the radio
carries a tag and the driver echoes it (`docs/wifi.md` 58); every radio wait sifts by it, so a late answer
is recognised as late and the owed count is exact. The reproduction below now holds the second request
back (`not sent - ... still owes 1 answer`) and clears the late answer by its tag. On the card the same
day: every wifi verb, and `chaos max-carnage` 50 rounds recovered.

**Previous status: OPEN - found 2026-10-02 in QEMU, reproduced on purpose. Not fixed: each fix is a change
to a surface (the kernel's, or the driver's reply protocol) that the operator should choose.**

## What the count is for

A `Call` takes "the oldest queued message SENT BY" the peer (`dequeue_reply_locked` matches by sender,
not by request). So when the shell gives up on a radio request, the driver's late answer to it is later
taken by the NEXT radio request as its own - the desync of boots 2026-10-01 13:58 and 14:30, where
`wifi radio on` read "already on" and a stale OK made `powercycle` kill the driver mid-cycle. `wifi_ask`
guards against it by COUNTING what the driver owes (`wifi_owed`): a request that timed out adds one, and
until that many answers have arrived and been discarded, no new request is sent.

## Why the count is wrong

It is decremented by every message drained from the shell's REPLY MAILBOX (`drain_stale_replies`,
`drain_owed_replies`), on the premise that only the driver's answers land there. They do not. The
mailbox takes the replies of EVERY peer the shell asks - `time`, `fs`, `net-stack`, the radio - and an
SDK `Message` carries no sender the shell can read (the kernel has `sender_ep`; nothing exposes it). So a
late answer from any other peer is counted as an answer the driver owed.

## The reproduction

QEMU `raspi4b`, a test-only image whose `wifi-driver` (in `serve_unavailable`) slept 8 s before answering
`OP_STATUS`, three `wifi status` in a row, with temporary logging in `wifi_ask` and `drain_stale_replies`:

```
gsh> wifi status
DBG wifi_ask: replies 0 other 0 owed_in 0 owed 0
DBG drain_stale_replies: len 2 first [1, 4] sender-badge 0      <- not the driver's: it is still asleep
shell: the radio driver did not answer op 0x06 within 3 s (1 stale message(s) cleared from this shell's queue)
gsh> wifi status
DBG wifi_ask: replies 0 other 0 owed_in 0 owed 0                <- the owed answer was cancelled
shell: the radio driver answered op 0x06 after 3326 ms          <- the FIRST request's late answer, 8 s in
```

The second `wifi status` was sent while the first was still owed and printed the first one's answer -
harmless here only because both asked the same thing.

## What was fixed alongside, and what was not

- `wifi_drain_stale` used to count the main endpoint's messages against the owed count too; it now
  returns `(replies, other)` and only `replies` are subtracted. Correct, and not enough: the stray
  `[1, 4]` above came out of the mailbox.
- `wifi_not_answering` now says "not sent - the radio driver still owes N answer(s)" when `wifi_ask`
  held a request back; it used to say "the radio driver is not answering" about a request it never sent.
  That path is correct, but this item keeps it from being reached when it should be.

## The fixes, for the operator to choose

1. **The kernel exposes a received message's sender endpoint** (it already stores `sender_ep` for the
   Call's matching). A query in the shape of `last_recv_badge`. Mechanism, not policy - but a new
   syscall surface, which Commandment I pins and a CLAUDE.md amendment must record.
2. **The driver echoes the request's op (or a tag) in every reply**, as `fs` replies carry a tag. No
   kernel change; a protocol change on both sides of every radio op.
3. **The shell asks the radio from a second mailbox of its own.** No kernel or protocol change, but a
   reply mailbox is an OPTIONAL endpoint (`routing::try_register_optional`), refused when the routing
   table nears full - QEMU `raspi4b` already refuses two at boot - so it can silently not exist.

2 is the smallest that is always there.
