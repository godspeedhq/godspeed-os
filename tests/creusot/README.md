# tests/creusot

Deductive proofs of kernel functions with [Creusot](https://github.com/creusot-rs/creusot) (`backlog/90`,
layer C: Creusot for invariants of safe Rust). Kani (`tests/kani`) searches every input within a bound;
Creusot proves a function against its contract for every input, with no bound.

## What it proves, and of what

The real files: `src/capability/mod.rs` and `src/ipc/mod.rs` include `kernel/src/capability/{cap,generation,rights}.rs`
and `kernel/src/ipc/{message,queue}.rs` by `#[path]`. The contracts are written on the production functions, as
`#[cfg_attr(creusot, ...)]`: Creusot sets `cfg(creusot)`, the kernel's build never does, so the kernel carries
the contracts as inert attributes and has no dependency on `creusot-std`.

| Function | Contract, proved for every input |
|---|---|
| `Rights::contains` | `a.contains(b)` is exactly "b has no bit a lacks" |
| `Rights::narrow` | the result has no bit outside `self`, none outside the mask other than `ONCE`, and keeps `ONCE` exactly as `self` had it |
| `Rights::without` | the result has none of the mask's bits, none outside `self`, and keeps every other bit of `self` |
| `Rights::union` | the result holds every bit of either and nothing else |
| `Generation::bump` | below `u32::MAX` the result is exactly one more (at the ceiling it panics, which Kani's `generation_bump_never_wraps` shows) |
| `MessageQueue::is_full`, `is_empty` | they say exactly whether the length is 16, or 0 |
| `MessageQueue::enqueue` | from a head below 16 and a length at most 16, both stay in range, the head does not move, and the length grows by one, or the queue was full and is unchanged |
| `MessageQueue::dequeue` | the same bounds; an empty queue gives `None` and is unchanged; otherwise the head moves on by one round the ring and the length falls by one |

**Shown not vacuous, 2026-11-03:** with `narrow` changed to drop `ONCE`, its proof failed; with `enqueue` changed
to move the head, its proof failed.

**What it does not prove.** Every other function in those files is compiled and not proved: `dequeue_matching`
(a loop, which would need invariants written for it),
`Message::new`, the derived `PartialEq` impls, and the rest. `next_generation` is hidden from Creusot, which
cannot translate a `static`; it is the kernel unit tests'. That the queue's slots hold a message exactly where the
length says is not in these contracts. And, as for every result here, that the kernel's target build is the same
code as this host build is an assumption.

## Running it

Creusot needs Linux: in WSL (Ubuntu 24.04), or in `verification/Containerfile`.

```bash
./prove.sh
```

`why3find.json` names the provers: Alt-Ergo, CVC5 and CVC4. Z3 is installed with Creusot and runs, but why3find
cannot calibrate it on the machine these were first proved on, so it is not used; nothing here needs it.
