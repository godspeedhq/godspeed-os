# 10. IPC cost - the lever is fewer ROUND TRIPS, not a tighter protocol

**Severity:** performance. Nothing here is broken; this is about what to do when a hot path needs it.
**Status:** analysed, not built. The full treatment is [`docs/ipc-efficiency.md`](../docs/ipc-efficiency.md)
- that document owns the reasoning, this file owns the status.

It was titled "IPC efficiency - backlog" and marked "Status: backlog" while living in `docs/`, where
nobody scanning for open work would find it. That is the same drift this folder exists to stop.

## The measurement, and the part people get wrong

An IPC round trip costs about **5x a syscall round trip** on real hardware (T630: 102,600 vs 19,281
cycles), and ~10x under QEMU's TCG where everything is inflated. It will never reach parity, and not
because of inefficiency: a syscall is one privilege transition and a return, while an IPC round trip
is minimally two transitions, two context switches into a different address space, and two copies.
**The context switch is irreducible.**

Where the cost actually sits is the useful part:

| | share of a syscall floor |
|---|---:|
| capability validation | **0.5%** |
| scheduler decision | 9% |
| message copy, 4 KiB WORST case | 70% |
| the rest - block, wake, switch | the majority |

**No optimisation should ever start at the capability check.** It is half a percent. The model is not
what costs anything; the crossing is.

A caution about the numbers: the J5005 column in CLAUDE.md 23.3 makes IPC look far cheaper (1.4x)
because that build was `perf-brutal` and its syscall floor was contention-inflated. Comparing a
contended floor against a contended IPC flatters the ratio. The T630 isolated figures are the ones to
reason from.

## Ranked, from `docs/ipc-efficiency.md`

1. **Batch the block protocol** - the hottest path, one block per request today. Observed:
   `fs: op 10 took 534451 us, 17 block ops` - seventeen round trips for one filesystem operation. A
   4 KiB message holds eight 512-byte blocks, so a multi-block request is roughly an 8x cut on the
   busiest path, and it relieves the queue pressure `fs::block_rpc`'s `QueueFull` retry exists to
   survive.
2. **Co-location as a rule, not luck** - cross-core IPC measured **~14x same-core** on the T630
   (1,433,087 vs ~102,600 cycles): an IPI plus cache-line bouncing on the queue indices. Chatty pairs
   are co-located today by round-robin accident, not by contract.
3. **Keep new code on `call` / `call_deadline`** rather than hand-rolled send-then-recv.
4. **Fewer HOPS, not fewer layers** - the layering is the design; the number of crossings is the cost.

## What this analysis did NOT cover, and is worth adding

**The message is a fixed `[u8; 4096]` struct.** The kernel copies only `payload_len` from userspace,
so the syscall path is honest - but `Message` itself is ~4,224 bytes, a queue slot is that size
regardless of payload, and the routing table is therefore **72 KiB per endpoint x 96 endpoints =
6.8 MiB** of the kernel's 22.5 MiB `.bss` (see backlog/09).

A small-message representation - inline for payloads under some threshold, the big buffer only when
needed - would cut both that memory AND every in-kernel move of a struct that is mostly empty. Most
messages in this system are tens of bytes: a block request is 10, a status reply 77.

It is NOT free to do: `MAX_MESSAGE_SIZE` is constitutional (8.5, "one page"), and a two-shape message
adds a branch to the hottest path in the kernel. Worth measuring before believing.

## Explicitly not on the table

Zero-copy IPC. 2.5 rejects it permanently - it violates "no shared mutable memory by default", and
that is a decision, not an omission.
