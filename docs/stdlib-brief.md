<!-- SPDX-License-Identifier: GPL-2.0-only -->
# The standard library brief

**The requirement as written by the operator, kept as the statement of intent for `feat/stdlib`.**

Lightly annotated: the prose is theirs, and every `> STATUS` block is added afterwards to say what
has since been decided or built, so reading this on a phone tells you where things stand rather than
only where they started. Nothing in the original has been softened - where the work diverged, the
block says so.

The design report that answers this is [`stdlib-design.md`](stdlib-design.md).

---

## Context

GodspeedOS is approaching v1. The next major goal is to make Godspeed pleasant for programmers to
build applications and utilities for.

The standard library should initially live inside the `godspeed-os` repository because its API is
being designed alongside the Godspeed v1 application ABI, service contracts, runtime, capabilities
and existing utilities. **Do not create a separate repository at this stage.**

**GodspeedOS is NOT POSIX.** Do not introduce POSIX semantics, libc assumptions, Unix compatibility
layers, or Linux abstractions merely because they are familiar. Godspeed has its own architecture:
MISCIS kernel responsibilities, capability-based authority, no ambient authority, IPC-based services,
restartable services, userspace filesystem/network/device functionality, architecture-neutral
interfaces, and a let-it-fail and recovery philosophy.

The standard library must make this architecture EASY TO USE. It must not hide or contradict it.

> **STATUS:** `stdlib/rust` is a crate inside this repository, depending on `sdk/rust`. No separate
> repo, no POSIX layer, no libc.

## Primary goal

A programmer should be able to write a small Godspeed utility without knowing how to manually
discover services, construct IPC messages, decode IPC replies, manipulate capability tables,
understand supervisor internals, or know architecture-specific details.

The desired experience is approximately:

```rust
use godspeed::{self as gs};

fn main() -> Result<(), godspeed::Error> {
    let contents = fs::read_to_string("/data/hello.txt")?;
    println!("{contents}");
    Ok(())
}
```

**This is an ILLUSTRATIVE API.** Do not blindly implement it without first inspecting the repository
and determining what abstractions naturally fit Godspeed.

> **STATUS: two parts of that snippet cannot exist, and the inspection is why.**
>
> - `fn main()` has no meaning yet. Every runnable thing is a service entered at
>   `service_main(ctx) -> !`, all 15 examples return `!`, and of 52 syscalls the only one that ends a
>   task is `Kill` - gated behind `service_control`, which no application should hold. **This is the
>   STOP condition the brief names below**, and it is open: `stdlib-design.md` §1 sets out three
>   options and recommends adding an `Exit(status)` syscall as its own reviewed change.
> - `read_to_string` cannot return an allocated `String`: there is no heap, deliberately (§26.6.1).
>   The shipped shape is `read_into(path, &mut buf)` - the caller owns the buffer and the bound is
>   readable in the source.

## The architectural rule

The developer expresses WHAT they want. The library handles HOW that is represented through
Godspeed's mechanisms. However:

**THE STANDARD LIBRARY MUST NOT INVENT CERTAINTY OR AUTHORITY.**

It may hide plumbing. It must not hide semantics needed to reason about authority, failure,
cancellation, service restart, or operation outcome.

## Phase 1: inspect before implementing

Inspect the repository first: SDK/runtime crates, startup, utility crates, console/filesystem/
networking/IPC/capability APIs, service discovery, allocator and runtime support, panic setup, error
representations, architecture-specific dependencies, and existing public interfaces that can be
reused. Inspect several existing utilities and determine what boilerplate is repeatedly implemented
today.

**DO NOT WRITE CODE YET. Produce a short design report first.**

> **STATUS: done, and it found three things worth naming.** The fs wire protocol is hand-copied
> between crates (`FS_OK` declared independently in four; the shell re-declares 21 of the
> filesystem's 27 opcodes). The honest failure model ALREADY EXISTS as `DeadlineOutcome`, but sits
> behind eight `request_with_reply*` variants whose shortest names return `Option` and collapse
> "never sent" into "unknown" - five services reached for a longer name to undo that. And every
> client re-implements reacquire-and-retry.

## Phase 2: the minimum useful surface

Propose the smallest useful v1. Potential areas: console, io, fs, net, task, time, cap. **These names
are suggestions only. Do not create empty modules.** Every abstraction must correspond to a
demonstrated application need. Prefer small stable primitives plus composable helpers over a large
speculative framework.

> **STATUS: shipped - 13 PUBLIC MODULES:** `addr`, `call`, `cap`, `error`, `file`, `fs`, `io`, `ipc`,
> `net`, `record`, `resource`, `task`, `trace`. (This listed seven plus "a private `resource`";
> `resource` is public now, and `file`/`ipc`/`task`/`trace`/`record` all arrived after it was written.)
> 101 `pub fn` across the crate (149 public items in all - README.md states the counting rule). `cap` was recorded as "not built" for a day and then built: the blocker was
> that the file-capability protocol carried no correlation tag, which is a protocol property and was
> fixed in userspace. `task` SHIPPED, and carries the clock surface that answers "time":
> `uptime_secs`, `epoch_secs_monotonic`, `datetime`, `clock_source`, `clock_is_set`. This said both
> were left out for want of repeated plumbing - true when written, and the plumbing turned up.

## Rust model

Do not make complete Rust std support a prerequisite. The model may use `core`, `alloc`, the Godspeed
runtime, and this library. Do not distort Godspeed semantics to resemble Rust std.

> **STATUS:** `core` only. `alloc` is not used, because there is no heap.

## Capability rules

Capabilities remain explicit authority. The library may make them convenient. It must NEVER silently
create or broaden authority. An application operates using capabilities granted by its launch
environment. Prefer typed handles so programmers need not manipulate raw capability table entries.

**The distinction must remain: requested resource != granted authority.**

> **STATUS: held, and it is the design's load-bearing line.** Every entry point takes
> `&ServiceContext`. `fs::Fs::new(&ctx)` grants nothing: a task whose contract lacks
> `ipc_send = ["fs"]` gets the same handle and every call fails with `Unreachable`.
> `examples/stdlib-hello`'s contract exists to make that visible. There is no global and no `print!`
> that finds a stdout on its own - that would be ambient authority wearing a familiar name.

## Failure semantics

Godspeed services are restartable. Handle it honestly. Do NOT blindly retry a non-idempotent request
whose service died before acknowledgement: it may have committed, not committed, or be unknown.
Investigate whether the system already represents these states. **Do not invent a parallel failure
model if one already exists.**

> **STATUS: it did exist, and it was not reinvented.** `DeadlineOutcome { Reply, SendFailed,
> QueueFull, Timeout }` is carried through one-to-one. `Error::retry_is_safe()` is the answer in one
> call, and it returns `false` for `OutcomeUnknown` on purpose - a delete that timed out may have
> deleted, and re-sending is a second delete whose failure looks like success.

## Portability

Standard-library code must be architecture neutral. Do not use `#[cfg(target_arch = ...)]` to work
around architectural differences. Adding the library must not regress the existing portability
hardening.

> **STATUS: zero arch-conditional sites added.** `shared_surface_check` green.

## Kernel rule

**DO NOT modify the kernel merely to make the library convenient.** If an apparently convenient API
requires a new kernel responsibility, STOP, document the missing mechanism, and explain why it
appears necessary before making any kernel change.

> **STATUS: no kernel change, and one mechanism documented as missing** - the terminating task.
> `stdlib-design.md` §1 argues it is probably not a seventh MISCIS responsibility (the reclaim,
> generation bump and death notification all exist; what is missing is a task saying "I am finished,
> here is my status") but it is a kernel change either way, so it waits for review.

## Unsafe rule

Do not introduce `unsafe` into services or ordinary abstractions for convenience. Keep any genuinely
required low-level code in the smallest existing trusted boundary. Do not weaken existing
`#![deny(unsafe_code)]` guarantees.

> **STATUS:** zero `unsafe` in the library, and it needs no `#[allow]` at all - unlike a service, it
> exports no `#[no_mangle]` entry symbol.

## Dogfooding

After implementing, migrate a SMALL NUMBER of existing utilities: at least one console-oriented, one
filesystem-oriented, and one network-oriented. Use only public interfaces where practical. Do not
give built-in utilities private shortcuts. **The migration is part of the test.** If utilities still
require substantial raw IPC plumbing, identify what abstraction is missing - do NOT respond by
creating giant convenience APIs.

> **STATUS: `services/recorder` migrated** (filesystem and console). 680 lines to 627; raw fs/IPC
> plumbing from 22 sites to 2; its own filesystem call helper from 55 lines to 15 and, more importantly, from `bool`
> to `Result` - it used to discard the difference between a write that timed out and one that never
> left. The migration also DROVE the API: `recorder` needed `create_sized`, `write_at` and `rename`,
> which were added as typed operations rather than behind an opcode escape hatch.
>
> **Five migrations now**, and each one found something review had not: `recorder` (filesystem and
> console), the shell's `tcp` (which exposed a lossy merge of the user's own abort into "outcome
> unknown"), `dir` (which taught the LIBRARY about page caps and partial listings), `fcap` (which
> caught the library reading a reply one byte off), and `sock` (which found a report that had been
> claiming a response nobody sent, and two deadlines shorter than the service they waited on).

## First program test

A programmer unfamiliar with Godspeed should eventually be able to write a small utility without
understanding MISCIS first. MISCIS explains why the OS behaves as it does; it should not be
prerequisite knowledge for printing text, reading a file or opening a connection. Create at least one
minimal example application.

> **STATUS:** `examples/stdlib-hello` - read a file, print it, handle the failures honestly. Its
> contract is the interesting half.

## The architectural test

For each proposed abstraction: (1) what repeated plumbing does it remove? (2) what mechanism does it
wrap? (3) does it preserve explicit authority? (4) honest failure semantics? (5) architecture
neutral? (6) can the backing service restart without the abstraction lying? (7) does it belong in a
standard library rather than a higher-level package? **If (7) is unclear, leave it out for now.**

## Testing

Test the public APIs: success, invalid input, missing capability, service unavailable, service
restart, cancellation, malformed response, unknown outcome. Existing selfcheck/chaos infrastructure
must continue to pass. **Do not reduce existing coverage or weaken assertions to make the new library
pass.**

> **STATUS: partial, and the gap is stated rather than hidden.** Four host unit tests cover the error
> model, including that `retry_is_safe` is false for `OutcomeUnknown`. Host coverage stops there
> because `godspeed_sdk` owns the `panic_handler` and so does `std`, so a dependent crate's host test build
> hits `duplicate lang item`; the pure/SDK split follows the pattern `kernel/src/clock.rs` documents.
> **The target-side tests exist now.** `osdev test fs-reuse` holds a `gs::cap::File` across a real
> `fs` kill and asserts the stale capability is refused with a NAMED error rather than a hang, a
> silent success or a wrong answer (12 cases, was 8). `osdev test files` exercises `gs::fs` across a
> real `chaos kill-storm fs 2` and now requires a real listing rather than merely the absence of an
> error string - the guard it replaced passed on silence. `osdev test file-cap` covers `gs::cap`
> end to end (15 cases, was 13).
>
> Writing them found two things: `Error::service_answered()` can never be true for a stale
> capability, because the kernel refuses it before the owning service is reached; and the library was
> reporting a REVOKED capability as "could not be reached", which named the wrong fault. `Error::
> Revoked` exists because of that test.

## Documentation

Document public APIs from the programmer's perspective: what the operation does, what authority it
requires, what failures it can return, whether it may block, whether cancellation is supported, and
what happens if the backing service restarts. Do not require reading kernel source.

## Non-goals

Not porting glibc, not POSIX, not a Unix compatibility layer, not the complete Rust std, not a
package manager, not the Godspeed CLI, not a marketplace, not redesigning MISCIS, not adding kernel
functionality for convenience, not a giant application framework.

## Future relationship with the Godspeed CLI

A separate host-side `godspeed` CLI is planned later (`new`, `build`, `test`, `run`, `deploy`). This
library must NOT depend on it. `godspeed-os` defines the platform and public contracts;
`godspeed-cli` will later consume them. Do not implement the CLI as part of this task.

## v1 intent

v1 represents architectural stability, not feature completeness. Future versions may add a display
server, mouse support, more drivers, more architectures, graphical applications, laptop support,
Vigil, and package distribution - and should build on the v1 foundation without redesigning it.
**Be conservative about declaring APIs stable.** The initial library should be small enough that we
understand the semantics of what we commit to.

## Deliverable process

1. Inspect. 2. Design report. 3. Identify repeated boilerplate. 4. Propose minimal structure.
5. Identify which contracts each API wraps. 6. Identify risks. 7. **WAIT FOR REVIEW.** 8. Implement
the first slice only after approval. 9. Migrate representative utilities. 10. Run existing tests plus
new ones. 11. **Report architectural friction instead of working around it.**

> **STATUS on 11, because it happened.** Moving reacquisition into `gs::call` made it invisible to
> the `IX-peer-reacquire` Commandment check, which looks for the API in the service's own source - so
> it correctly reported `recorder` as having no recovery path. Baselining an exemption would have
> been the wrong answer. Instead `reacquire_api` now credits the library, AND a new
> `IX-stdlib-delegates` check asserts the library really does reacquire, on `SendFailed` and not on a
> deadline. Without that second half the property decays from "this service can recover" into "this
> service calls something that used to". Commandments 20 to 21 checks, red-team probes 73 to 75.

## Success criteria

This succeeds when a Godspeed programmer can focus on their program rather than Godspeed's plumbing,
while capabilities remain explicit, failures remain honest, restart semantics remain visible where
necessary, architecture neutrality remains intact, MISCIS remains unchanged, and unsafe boundaries do
not expand casually.

The desired outcome is not "Godspeed looks like Unix", nor "Godspeed looks exactly like Rust std". It
is: **"Godspeed is simple to program for because its own architecture has a small, coherent and
honest developer interface."**

## Final rule

**Do not make Godspeed easier to use by making Godspeed less Godspeed.** Remove repetitive plumbing.
Preserve the architecture.
