# IPC efficiency - backlog

**Status:** tracked as [`backlog/10-ipc-efficiency.md`](../backlog/10-ipc-efficiency.md) - the status
lives there, the reasoning lives here. Nothing here is built. Recorded because the cost it addresses grows with every
step of §4.4's kernel-shrinking work, and the decisions are easier to make before a hot path depends
on them.

## Why this exists

The more the kernel shrinks, the more work crosses an IPC boundary instead of a syscall boundary. That
trade is deliberate (§20: "In a microkernel, every meaningful operation crosses an IPC boundary") but
it is not free, and the ratio is worth knowing rather than assuming.

## What an IPC round trip costs

From `tests/qemu/perf/baseline.json`. QEMU under TCG, so the absolute cycle counts are inflated - the
RATIOS are the useful part.

| | cycles | x syscall floor |
|---|---:|---:|
| syscall floor (`yield` round trip, B3) | 2,498,563 | 1.0x |
| cap validation (B4) | 12,364 | 0.005x |
| scheduler decision (B10) | 232,151 | 0.09x |
| message copy 4 KiB (B9) | 1,810,549 | 0.7x |
| **IPC same-core round trip (B1)** | **26,247,656** | **10.5x** |

On the T630 the same comparison lands nearer 5x (102,600 vs 19,281 cycles, ~51 us vs ~9.6 us), and
that IPC figure was measured under contention, so the true gap is smaller again.

**It will not reach parity, and not because of inefficiency.** A syscall is one privilege transition
and a return. An IPC round trip is minimally two transitions, two context switches into a different
address space, and two copies. The context switch is irreducible.

**Where the cost actually lives** is more useful than the headline. Cap validation is 0.5% - the
capability check is free, and no optimisation should ever start there. The scheduler decision is 9%. A
4 KiB copy is 70% of a whole syscall, but that is the worst case; the kernel copies `payload.len()`,
not the buffer. The remaining majority is block, wake and switch - which is why the lever is the NUMBER
of round trips, not the cost of one.

## The test to apply before moving anything out of the kernel

**Does this become an IPC in a loop, or an IPC once at setup?**

Once at setup is free. D3's `ask_bdf_for_class` runs about six times a boot: ~300 us total, irrelevant.
Drivers are usually safe for a different reason - device latency swamps the message. A measured block
operation took 31 ms, of which IPC is roughly 0.2%.

What would hurt is a high-frequency path with no device wait: something called thousands of times a
second where the work itself is microseconds.

## Backlog, ranked by payoff

### 1. Batch the block protocol (biggest win, hottest path)

`fs: op 10 took 534451 us, 17 block ops` - seventeen round trips for one filesystem operation, because
the block protocol carries one block per request. A 4 KiB message holds eight 512-byte blocks, so a
multi-block request cuts round trips by roughly 8x on the busiest path in the system. It also reduces
queue pressure, which is what the `QueueFull` retry in `fs::block_rpc` exists to survive.

### 2. Treat co-location as a rule, not luck

Cross-core IPC measured ~14x same-core on the T630 (1,433,087 vs ~102,600 cycles) - an IPI plus
cache-line bouncing on the queue's head/tail. Already true of the chatty pairs by round-robin accident:
`fs` + `block-driver` on core 1, `net-stack` + `nic-driver` on core 1. `shell` (core 0) reaching `fs`
(core 1) is the remaining cross-core hop, and it matters during scripts rather than at human pace.

§9.2 placement is contract-declared and re-evaluated from scratch on every restart, so this is a
statement a contract can make - but note §3.11: contracted placement is deployment-coupled by design,
and a pair pinned together is a pair that cannot be separated when a core is missing.

### 3. Keep new code on `call` / `call_deadline`

They fuse send-and-await into ONE syscall and one block, against two syscalls and a separate wait.
Already the norm; the thing to watch for in review is new code hand-rolling `send` + `recv`, which
costs an extra transition and reintroduces the reply-matching hazard §8.2's amendment describes.

### 4. Fewer HOPS, not fewer layers

`shell -> fs -> block-driver` is two round trips per file operation. Batching at the TOP - one `fs`
request that covers the work - beats optimising either hop, and costs no layer. Collapsing layers to
save a hop is the trade this project does not make (§26.1).

## Explicitly off the table

**Shared-memory rings and zero-copy IPC.** §2.5 rejects them permanently: they violate "no shared
mutable memory by default" (invariant 2). This is the first thing anyone reaching for IPC performance
will propose, so it is recorded here as closed rather than unconsidered. The answer in this system is
to reduce the NUMBER of transitions, not to make one cheaper by sharing memory.

**Fire-and-forget to skip the reply** is only valid where loss is acceptable, because §8.6 is explicit
that a successful `send` means the message was QUEUED, not processed. Logging qualifies. A block write
does not.

## Fix the instrument before trusting it

`B2` (cross-core round trip) reads 26,115,151 cycles against `B1`'s 26,247,656 - identical, while the
T630 measured the two 14x apart. The QEMU benchmark is probably not crossing cores at all. **Nothing
here should be judged by B2 until that is resolved**; a benchmark that does not measure what it claims
is the same defect class as a cross-check that passes on a broken machine.
