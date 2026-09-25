# 47 - `selfcheck.gsh` is at the baked-script ceiling

**Opened:** 2026-09-23
**Status:** CLOSED 2026-09-24 - split into nine parts
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

## Closed 2026-09-24 - option 1, and two corrections to it

The suite is **nine files** under `scripts/selfcheck/`, largest 13,367 bytes against the same 65,536
ceiling. `selfcheck` runs them in order and prints ONE tally; `selfcheck <part>` runs one.

```
00-language   13367     50-files      10947
10-meta        8563     60-data        7156
20-hardware    8639     70-cleanup     1042
30-events      6651     80-network     6468
40-persist     9725
```

**The seams were clean, which is what this entry said to confirm.** Measured before committing: all
six `fn` definitions sit between lines 89 and 194 with every call site in that same range, and each of
the thirteen `let` bindings is used only inside its own section. The one apparent exception - `name`,
bound at line 50 and "used" at 303 - was a mention inside a comment. What crosses the parts is the
working directory and the disk, and both survive, because `cwd` is threaded by `&mut` and the
filesystem is the filesystem.

### Two things this entry got wrong, both of which changed the design

**"`selfcheck` would become a small driver that runs each part in turn" cannot be a script.** `run`
and every library command is prompt-level only - `LIBRARY`'s own doc says two nested interpreter
frames would blow the bounded user stack - so a `selfcheck.gsh` that runs the parts is exactly the
nesting that is refused. The driver is `cmd_selfcheck`, in Rust: it walks `SELFCHECK_PARTS` and calls
`run_lines` once per part, sequentially, at one depth. One frame at a time, never two.

**"The pass/fail tally lives in the SHELL, not in the script - so a split does not have to thread
counters between the parts" is half true, and the false half was the work.** The counters are in Rust,
yes - but as LOCALS of one `run_lines` call, which printed them itself. Nine parts would have printed
nine `run: ran N, failed M` lines, and that line is an interface: nineteen harness checks match
`failed 0` against it. `run_lines` now takes an `Option<&mut Tally>`, adding its counts to a
caller-owned total and leaving the line to whoever knows the run is over.

### Two things it did not anticipate

**The u16 ceiling is enforced TWICE.** `prescan_fns` indexes `fn` definitions with u16 offsets - and
so do `run_lines`' own per-statement record arrays (`soff`, `fail_off`, `skip_off`), which index the
same buffer. Option 2 (widen to u32) would have had to widen both, or fix one and leave the other
wrapping silently. Splitting satisfies both, because each part is interpreted from its own buffer.

**The per-statement detail cap stops biting.** `RUN_MAX_CMDS` is 256 per `run_lines` call and the
single file ran 509 statements, so the report said "per-statement detail covers the first 256 of 509"
and 253 statements were counted in the tally and named nowhere. Nine parts, nine budgets: the
transcript now carries zero of those lines and every statement is named.

Verified: `osdev test script` 7/0 - `ran 509, failed 0` unchanged, twice in one boot, plus
`selfcheck files` running 131 statements alone and a name that is not a part being refused with the
list of the ones that are.
