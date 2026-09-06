# 1. `PlacementInvalid` is never constructed - a contracted core is silently ignored

**Severity:** constitutional. The code and CLAUDE.md disagree, so one of them is wrong (26.3).
**Found:** 2026-09-06, booting `osdev run --smp 1` to see what single-core costs.

## What the constitution promises

9.2, and again in 13.2, in the strictest language the document uses anywhere:

> If core N exists and is ready, spawn on N. Otherwise, spawn rejected with `PlacementInvalid`.
> The supervisor logs the rejection and skips the service.

with the rationale spelled out: *"Contracts are enforced, not interpreted. A contract that names a
core means the developer expressed deployment intent. Silently rerouting to a different core would
be exactly the kind of reinterpretation a capability-based system is designed to forbid."*

## What actually happens

The compiler says it without being asked:

```
warning: struct `PlacementInvalid` is never constructed
  --> kernel/src/smp/placement.rs:12:12
```

And a single-core boot shows the consequence:

```
supervisor: SPAWN stress-bs10-victim core=1
task: 'stress-bs10-victim' spawned OK on core 0      <- asked for 1, got 0, silently
```

`xhci` declares `core = 2` in its contract and started on core 0 with no notice of any kind.

## Why this is worse than a missing feature

It is a **silent fallback at the kernel boundary** - 21 lists that as an automatic PR rejection,
and invariant 12 forbids it outright. Nothing in the system is lying loudly; it is lying quietly,
which is the failure mode this project spends the most effort avoiding.

It also means **placement has never been tested**, on any machine. Every `placement.core` in every
contract has been advisory the whole time, so any conclusion drawn from "it runs on core N" was
drawn from a fact the kernel never enforced.

## What is ruled out

- Not a supervisor bug: the supervisor logs `SPAWN <name> core=1`, so it passed the intent down.
- Not a missing type: `smp/placement.rs` HAS `PlacementInvalid` and `resolve()` returns
  `Result<u32, PlacementInvalid>`. The type exists, the error path is simply never taken.

## The decision to make first, before any code

Two honest options, and this is a CLAUDE.md question, not an implementation one:

1. **Enforce it** as written: an unavailable contracted core rejects the spawn, loudly, and the
   service does not start. This is what the document says. It also means a single-core boot starts
   NO `fs` (core 1), NO `xhci` (core 2), NO `ehci` (core 3) until their contracts change - which is
   arguably the correct, honest outcome and exactly the pressure item 2 wants to apply.
2. **Amend 9.2** to say placement is a PREFERENCE, with the reroute reported loudly. This is a
   weaker guarantee and needs the rationale in 9.2 rewritten, because that paragraph argues
   specifically against this.

Option 1 is what the constitution says today. Do not implement either silently.

## Next step

Settle 1 vs 2, then either construct the error at the one site in `smp/placement.rs::resolve` and
handle it in the spawn path, or amend 9.2 and 13.2 with a dated rationale. 22 Test 10 and the
coverage matrix both name `PlacementInvalid`, so whichever way it goes, a test has to pin it.
