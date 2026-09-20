# 1. `PlacementInvalid` is never constructed - a contracted core is silently ignored

**Status: STILL OPEN 2026-09-20, and the TITLE is now the wrong complaint.** `PlacementInvalid`
IS constructed (`kernel/src/task/mod.rs:1218`) - but only for a core requested with
`SPAWN_FLAG_CORE_STRICT`, which is an operator's `--core N` or a restart's `placement_override`. A
CONTRACT's `placement.core` still arrives as a PREFERENCE and is still rerouted. The decision this
entry asks for below has not been made; what happened is that a third option was built for a
different path. See "What is true as of 2026-09-20" at the end.

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


---

## What is true as of 2026-09-20

This entry was surveyed as low-hanging fruit and nearly closed on the strength of one grep: the
compiler warning it opens with is gone and `PlacementInvalid` is constructed at two sites. **Closing
it there would have been wrong.** The construction is on a path this entry is not about.

`syscall/dispatch.rs` splits the two:

```rust
let strict = req.flags & SPAWN_FLAG_CORE_STRICT != 0;
let core_override  = if req.core == u32::MAX || !strict { None } else { Some(req.core) };
let core_preferred = if req.core == u32::MAX ||  strict { u32::MAX } else { req.core };
```

| what asked for the core | arrives as | core not ready |
|---|---|---|
| operator `--core N`, restart `placement_override` | `core_override` | **`PlacementInvalid`**, spawn rejected |
| a contract's `placement.core` | `core_preferred` | **round-robin**, service starts elsewhere |

So a third option was built, and it is neither of the two this entry said to choose between:
**strict for an operator, advisory for a contract.** That is a defensible design - the reason is in
the code (11.3: a machine with fewer ready cores must still come up) - but 9.2 and 13.2 still say
the strict thing about contracts, in the strictest language the document uses, and were never
amended. The entry's instruction was *"Do not implement either silently"*, and the half that got
implemented was the half nobody had asked about.

### What IS fixed now

The reroute was **silent** - no log line on any path. That is the part that needs no constitutional
decision, because invariant 12 settles it, so it is fixed rather than left.

**Observed firing**, rather than merely compiled. Booting `--smp 1` makes every service that prefers
core 1, 2 or 3 ask for a core that is not ready, which is the arm that had never printed anything:

```
task: preferred core 2 is not ready - placing on core 0 instead (§9.2 preference)
task: 'events' spawned OK on core 0 (slot 1)
...
task: preferred core 3 is not ready - placing on core 0 instead (§9.2 preference)
task: 'adv-ba8' spawned OK on core 0 (slot 28)
task: preferred core 3 is not ready - placing on core 0 instead (§9.2 preference)
task: 'adv-ba8-witness' spawned OK on core 0 (slot 29)
```

Three reroutes on that boot, and **the first one is `events`** - a service whose row prefers core 2,
placed on core 0 on every single-core boot since single-core boots began, with nothing said. That is
the exact sentence this entry opened with in 2026-09-06, still true today, and now at least audible.

A contracted core being ignored is now a thing you can see in the boot log. Whether it should be
ignored at all is still open.

### What is still open, unchanged

The 1-vs-2 decision at the top of this file, for CONTRACT placement specifically. Both options
remain exactly as written, plus the third one that is now in the code and would need 9.2 and 13.2
amended to describe it honestly. That is a CLAUDE.md change with a dated rationale, which is the
operator's call and not a code change.

The entry's second charge also stands unqualified: **contract placement has still never been
enforced on any machine**, so every `placement.core` in every contract has been advisory for the
life of the project, and any conclusion drawn from "it runs on core N" is a conclusion about
round-robin.
