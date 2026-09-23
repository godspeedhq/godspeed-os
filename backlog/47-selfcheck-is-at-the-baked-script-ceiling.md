# 47 - `selfcheck.gsh` is at the baked-script ceiling

**Opened:** 2026-09-23
**Status:** OPEN - 397 bytes of headroom
**Raised by:** the operator.

## The number

```
scripts/selfcheck.gsh   65,139 bytes
ceiling                 65,536 bytes      (99.4% full)
headroom                   397 bytes
```

One more check of any size ends the build.

## Why the ceiling exists, and why it is not a mistake

`prescan_fns` indexes a script's `fn` definitions with **u16 offsets**, so a baked script past 64 KiB
would wrap silently and dispatch the wrong bodies. That is caught at COMPILE TIME rather than left to
runtime:

```rust
// audit U6: baked scripts must stay under the u16 offset ceiling `prescan_fns` uses (64 KiB), or the
// fn/summary offsets wrap silently and dispatch the wrong bodies. Fail the build, not at runtime.
const _: () = assert!(SELFCHECK_GS.len() < 65536, "selfcheck.gsh exceeds the 64 KiB baked-script ceiling");
```

So the failure mode is a loud build break, which is the right one. Nothing is wrong with the guard;
the script has simply grown into it.

## Three ways out, and which one to prefer

**1. SPLIT IT, using machinery that already exists. (Recommended.)**

The shell already bakes SEVERAL scripts - `LIBRARY` is a table of them (`scripts/lib/*.gsh`), each
subject to the same ceiling individually. `selfcheck.gsh` carries roughly twenty
`# ===== section =====` headings that are natural seams: meta, self-documentation, system info,
introspection, observe, hw-enumerator, lifecycle, trace, events, files, directories, networking.

`selfcheck` would become a small driver that runs each part in turn. **The pass/fail tally lives in
the SHELL, not in the script** - `assert`, `fail` and `skip` are shell builtins - so a split does not
have to thread counters between the parts, which is the thing that would otherwise make this
awkward. Worth confirming before committing to it.

This is also the only option that improves anything other than headroom: a 65 KiB single file is a
comprehension problem as well as a size one, and per-part invocation makes `selfcheck files` or
`selfcheck net` possible without running the whole suite.

**2. Widen the offsets to u32.**

One type change in `prescan_fns` and its `FnTable`. It raises the ceiling and changes nothing else,
but it also spends memory on every script to solve a problem that one script has - and the shell's
stack is already tight (`backlog` has prior art on `pipe_run` frames near the 64 KiB user stack).
Reach for this only if the split proves genuinely impractical.

**3. Move checks out of the baked script into `osdev` suites.**

Cheapest to do and the worst answer. `selfcheck` runs ON THE MACHINE, including on hardware where no
host harness exists; moving checks to QEMU-only suites would shrink the file by deleting coverage
from the place it matters most. Recorded so it is visibly rejected rather than quietly available.

## Not started

Recorded rather than done (26.7). Nothing is broken today - the build passes - but the margin is
small enough that the next person to add a check will hit it, and they should find this entry rather
than a puzzling compile error.
