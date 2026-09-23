<!-- SPDX-License-Identifier: GPL-2.0-only -->
# A native standard library for GodspeedOS - Phase 1 design report

**Status: Phase 1 report, REVIEWED AND APPROVED 2026-09-23. The first slice is implemented** on
`feat/stdlib` - `stdlib/rust` with `error`, `call`, `fs`, `io`; `examples/stdlib-hello`;
`services/recorder` migrated. Option A was taken (services only, no kernel change); the terminating
task of §1 remains open and unimplemented, which is the point of recording it here.

What follows is the report as written before any code, kept as it was argued. §7 at the end records
what building it changed.

---

## 1. The headline: there is nothing for a standard library to be a library FOR, yet

The brief asks for this to become possible:

```rust
use godspeed::fs;

fn main() -> Result<(), godspeed::Error> {
    let contents = fs::read_to_string("/data/hello.txt")?;
    println!("{contents}");
    Ok(())
}
```

Three things in those six lines do not exist in GodspeedOS, and only one of them is a library
problem.

**`fn main()` does not exist.** Every runnable thing in this repository is a SERVICE, and its entry
point is:

```rust
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> !
```

**All 14 examples return `!`.** They cannot return. There is no exit syscall: of the 52 syscalls in
`kernel/src/syscall/dispatch.rs` the only one that ends a task is `Kill` (8), which kills a task *by
name* and is gated behind `service_control` - a capability held by the supervisor, and one no
application should hold, since holding it means being able to kill anything.

**There are no applications.** `utilities/` holds 56 utility specifications. Not one of them is a
program. `read`, `dir`, `copy`, `seal`, `churn` and the rest are FUNCTIONS INSIDE
`services/shell/src/main.rs` - `cmd_read`, `cmd_dir`, `cmd_copy` - in a single crate of roughly
17,000 lines. The shell is not a program launcher; it is the program, and the utilities are its
subroutines.

So "a programmer writes a small Godspeed utility" currently means "a programmer adds a function to
the shell". A standard library cannot change that on its own, and a standard library that pretends
otherwise would be the first thing in the repository to lie about the architecture.

**This is the STOP condition the brief names**, and it is being reported rather than worked around:

> If an apparently convenient standard-library API requires a new kernel responsibility, STOP.
> Document the missing mechanism and explain why it appears necessary before making any kernel
> change.

### What is actually missing

A **terminating task**: something that is spawned, runs, produces a result, and ends, releasing its
resources and reporting an outcome to whoever started it.

Is that a new MISCIS responsibility? Careful reading says **no, and that is the important part**:

- The kernel already reclaims a task's frames, kstack and capability table when it dies, and already
  bumps its generation and wakes blocked peers (§14.2). Voluntary exit needs none of that built.
- The kernel already notifies the supervisor of a death, and the supervisor already decides what to
  do about it. Today the answer is always "respawn".
- What is missing is a task's ability to say **"I am finished, and here is my status"** rather than
  dying by fault or by someone else's `Kill`.

That is arguably not a seventh responsibility but a completion of the first six: it is the
Scheduling and Capability machinery that already exists, reached through one more syscall, with the
*policy* (what a finished task's status means, whether it is restarted) staying in the supervisor
where §26.10 puts it.

**But it is still a kernel change, so it is not mine to make.** Three options, and a recommendation:

| Option | What it costs | What it buys |
|---|---|---|
| **A. Ship the stdlib for SERVICES only** | Nothing in the kernel. The `fn main()` experience does not arrive. | Every finding in §3 below is still fixed. Services are what exist. |
| **B. Add an `Exit(status)` syscall** | One syscall, one supervisor arm. Needs a CLAUDE.md amendment (§8.2's `CallDeadline` is the precedent: a new syscall is a new kernel responsibility and the surface is pinned). | The `fn main()` model becomes real, and the shell can stop being the only place a utility can live. |
| **C. Do it in userspace** | A "runner" service spawns the program and kills it when it signals completion over IPC. No kernel change. | Works today. Costs an IPC round trip and a second service holding `service_control`, which is a confused deputy of exactly the shape `backlog/41` was rejected for. |

**Recommendation: A now, B as a separate reviewed change, never C.** C reintroduces the security
objection that killed `backlog/41`: a deputy holding `service_control` on behalf of arbitrary
programs. A is honest and immediately useful. B is the real answer and deserves its own discussion
rather than arriving as a side effect of a convenience library.

---

## 2. What the repository already has

Inspected: `sdk/rust/` (6,944 lines across 12 modules), all 20 services, all 14 examples, the 52
syscalls, and the fs/block wire protocols.

| Area | What exists | Quality |
|---|---|---|
| **Entry + runtime** | `service_main(ctx) -> !`, no `std`, **no heap** (§26.6.1: stack arrays and bounded arenas only) | Solid, but ceremonial: 3 crate attributes, a `#[no_mangle]`, and an `#[allow(unsafe_code)]` with a 7-line comment explaining itself |
| **Authority** | `ServiceContext` is the single door. The contract declares, the SPAWN REQUEST grants (§13.6) | Excellent. Nothing to fix |
| **IPC** | `send`/`try_send`/`recv`/`call`/`call_deadline` | Good primitives |
| **Request/reply** | **8** `request_with_reply*` variants, **3** outcome enums | See §3.2. The problem area |
| **Console** | `console_write`, `console_writeln`, `console_write_fmt`, `console_read` | Clean. A stdlib `io` module is mostly a rename |
| **Filesystem** | An opcode/byte wire protocol over IPC | See §3.1. The other problem area |
| **Capabilities** | `reacquire_cap`, `reacquire_cap_detail` | Correct, and clients must use them (§14.3) |
| **Records** | `record.rs` (677 lines), a typed row/table model the shell pipes through | Already a small stdlib. Worth studying as the house style |

---

## 3. The repeated plumbing, measured

### 3.1 The filesystem wire protocol is hand-copied between crates

`FS_OK` is declared independently in **four** crates: `fs`, `shell`, `copier`, `recorder`.
`OP_STAT_FILE = 12` in two. The shell re-declares **21** of the filesystem's 27 opcodes.

Reading a file means knowing the byte layout. From `cmd_read` in the shell, abbreviated:

```rust
let stat = fs_request_q(ctx, OP_STAT_FILE, path, &[]);      // three outcome arms
let sp = stat.payload_bytes();
let exists = sp.first() == Some(&FS_OK) && sp.len() >= 11 && sp[1] == 1;
let is_dir = exists && sp[10] == 1;
let size = u64::from_le_bytes([sp[2], sp[3], sp[4], sp[5], sp[6], sp[7], sp[8], sp[9]]);
// then loop read_at in IO_CHUNK pieces, handling errors at each step
```

Roughly thirty lines, and every one of them is an opportunity to index the wrong byte. This branch
already shipped a bug of exactly that shape: a guessed opcode (`FS_OP_WRITE_FILE = 1`, actually 10)
made `churn` write nothing for twelve seconds and report success.

**This is the clearest candidate in the repository.** The wire format is a fact the filesystem owns;
every other crate is currently re-deriving it from a comment.

### 3.2 The honest failure model exists, and the ergonomic path throws it away

`DeadlineOutcome` already distinguishes the states the brief asks about:

```rust
pub enum DeadlineOutcome { Reply(Message), SendFailed, QueueFull, Timeout }
```

That is the right model and **the brief's instruction not to invent a parallel one is correct**.
`SendFailed` means the request never left, so retrying is safe. `Timeout` means it may have
committed, so retrying is how one copy becomes two. The distinction is load-bearing.

The problem is that it is one of **eight** variants and **three** enums (`ReqOutcome`,
`DeadlineOutcome`, `DeadlineOutcomeInto`), and the shortest-named ones return `Option<Message>`,
which collapses `SendFailed` and `Timeout` into a single `None`. `services/copier` says so at the
call site:

> "Retry only on `Err` (the send failed), NEVER on `Ok(None)` (the deadline passed)... a slow `fs`
> is alive, and re-sending a write to it is how one copy becomes two. `request_with_reply_deadline`
> cannot express the difference."

**Five services** reached for `request_with_reply_call_err` to get the distinction back:
`block-driver`, `control`, `copier`, `nic-driver`, `recorder`.

So the stdlib's job here is not to design a failure model. It is to make the honest one the DEFAULT
and the SHORTEST path, and to stop offering the lossy one for new code.

### 3.3 Every client re-implements reacquire-and-retry

§14.3 puts the obligation on the client: a cap to a restarted service is stale forever. Every
service that talks to `fs` therefore carries its own "send, and if the send failed reacquire by name
and try once more" loop. It is about fifteen lines, it is subtle (the retry must NOT cover the
timeout case), and it is written out separately in each crate.

---

## 4. Proposed minimum surface

Every module below answers the brief's seven-question architectural test. Nothing is proposed
because it would round out a list.

| Module | Wraps | Removes | Notes |
|---|---|---|---|
| **`gs::fs`** | The filesystem wire protocol | §3.1, the whole of it | The big win. Typed ops, one owner for the byte layout |
| **`gs::io`** | `console_write*` | Little plumbing, but it is where `print` lives | Thin by design. Mostly naming |
| **`gs::call`** | `request_with_reply*` + reacquire | §3.2 and §3.3 | ONE honest primitive, not eight |
| **`gs::Error`** | `DeadlineOutcome` + fs status bytes | Per-crate error mapping | Derived from the existing model, not a new one |

**Explicitly NOT proposed yet**, because the brief says to leave out what is unclear:

- **`net`** - the brief asks for a network-oriented migration, and I would want to inspect
  `net-stack`'s socket surface properly before claiming a shape. Deferred to a follow-up, not
  dismissed.
- **`task`, `time`, `cap`** - no demonstrated repeated plumbing found. `time` is one syscall,
  `cap` is already `ServiceContext`, and `task` needs the §1 answer first.

### The API shape Godspeed actually wants

`fs::read_to_string` returning a `String` **cannot exist**: there is no heap (§26.6.1), deliberately.
Distorting Godspeed to resemble Rust std here is exactly what the brief forbids. The honest shape is
the one the repository already uses everywhere:

```rust
// The caller owns the buffer. Bounded by construction, no allocator, no hidden growth.
let mut buf = [0u8; 4096];
let n = gs::fs::read_into("/data/hello.txt", &mut buf)?;
gs::io::println(str::from_utf8(&buf[..n])?);
```

That is six lines against thirty, removes every raw byte index, and does not pretend the machine has
a heap.

---

## 5. Risks

1. **The stdlib must not become a second SDK.** `ServiceContext` is 4,059 lines and already the
   single door. If `gs::` wraps all of it, there are two doors and Commandment IV's "what can this
   service reach" question gets two answers. Proposal: the stdlib wraps the WIRE PROTOCOLS and the
   REQUEST PATTERN, and re-exports `ServiceContext` untouched for everything else.

2. **Authority must stay visible.** `gs::fs::read_into` needs the caller's existing `fs` send
   capability. It must take the `ServiceContext` (or a handle derived from it) as an argument rather
   than reaching for a global, or the "no ambient authority" invariant becomes a comment. This is
   the single most important line in the whole design.

3. **Portability.** Nothing proposed needs `#[cfg(target_arch)]`. The wire protocols are
   little-endian by contract and already proven to cross architectures (`cross_isa.py` 12/0, one
   volume carried x86-64 to riscv64 and back). The stdlib must not become the 45th arch-conditional
   site.

4. **Where does it live?** `sdk/rust` is the services' SDK. A separate `stdlib/rust` crate that
   depends on it keeps the layering honest and lets the SDK stay the low-level ABI. This needs a
   decision and is not mine to take alone.

---

## 6. What I recommend, and what I need from review

**Recommended first slice, assuming Option A** (services only, no kernel change):

1. `gs::fs` over the wire protocol, with the byte layout owned once.
2. `gs::call` - one honest request primitive returning the existing `DeadlineOutcome` shape, with
   reacquire-and-retry built in and correct (retry on `SendFailed`, never on `Timeout`).
3. `gs::io` - a thin, documented console surface.
4. Migrate `services/recorder` (filesystem) and one console path, and report what still needs raw
   plumbing.

**Three questions for you:**

1. **Option A, B, or C for §1?** This decides whether the deliverable is "a standard library for
   services" or "the application model plus a standard library". I recommend A now and B as its own
   reviewed change.
2. **Crate boundary:** new `stdlib/rust` depending on `sdk/rust`, or a `gs` module inside the SDK?
3. **Is `net` in the first slice?** The brief asks for a network migration; I would rather inspect
   `net-stack` first and propose it separately than guess at a socket API now.

Nothing will be implemented until these are answered.

---

## 7. What building it changed

The design above is kept as argued. This records where reality differed.

**The migration drove the API, which is what it was for.** `services/recorder` needed three
operations the proposed surface did not have - `create_sized` (allocate an extent), `write_at`
(positional write) and `rename`. The tempting shortcut was a `raw_op(opcode, ..)` escape hatch,
which would have leaked the opcodes straight back out and defeated the module's whole purpose. They
were added as typed operations instead.

**Measured result on `recorder`:** 680 lines to 627, and raw fs/IPC plumbing from 22 sites to 2. The
two that remain are a protocol tag constant of its own and a comment. Its `fs_call` went from 55
lines to 15 - but the line count is the least of it: **it used to return `bool`**, so a write that
hit the deadline (and may therefore have landed) was indistinguishable from one that never left.
Eleven lines of comment explained that difference and the signature discarded it. It returns
`Result` now.

**A Commandment check broke, and it was right to.** Moving reacquisition into `gs::call` made it
invisible to `IX-peer-reacquire`, which looks for a reacquisition API in the SERVICE's own source.
It reported `recorder` as having no recovery path.

That is the real architectural friction in this work, and it generalises: **a standard library that
absorbs a Commandment-enforced behaviour makes that behaviour unverifiable where the checker looks
for it.** Baselining an exemption would have been the wrong answer - a weakened check is worse than
no check. The fix has two halves:

1. `reacquire_api` now credits `gs::call`, so a service routing through it gets the path it has.
2. **`IX-stdlib-delegates` is a new check** asserting that `stdlib/rust/src/call.rs` really does
   reacquire, and does it on `SendFailed` rather than on a deadline. Without it, half 1 is a hole:
   a service would pass IX for calling a library that had quietly stopped doing the work, and the
   property would decay from "this service can recover" into "this service calls something that
   used to".

Commandments went 20 checks to 21, red-team probes 73 to 75, both new probes firing.

**Host tests cover the error model only, and deliberately.** `godspeed_sdk` provides `panic_impl`
and so does `std`, so any dependent crate's host test build hits `duplicate lang item`. The split
follows the pattern `kernel/src/clock.rs` documents: the pure logic - which is most of the semantics
worth testing - has no SDK import and is unit-tested on the host; anything that talks to a service is
for the target, where it can meet a filesystem that can really be restarted. A mock would only prove
this library agrees with a mock.

**Still open, unchanged:** the terminating task (§1), `net` (needs `net-stack` inspected properly),
and target-side suites for `fs`/`call` against a real service restart.

---

## 8. `net`: inspected, and the answer is NOT YET

§4 deferred `net` rather than guessing at a socket API. Inspected properly now, and the deferral
holds - but the inspection found something better than a socket module.

### Why not now

| | `fs` (shipped) | `net-stack` |
|---|---|---|
| Crates duplicating its constants | **4** (`fs`, `shell`, `copier`, `recorder`) | 0 |
| Real clients | 3 | **2** - `shell` (17 references) and `time` (ONE fire-and-forget `try_send`) |
| Protocol commits, v0.16.0 to now | - | **34**, one of them on this branch |

Two clients is not repeated application plumbing, it is two call sites, and one of them sends a
single byte. Against that, the protocol moved 34 times in three releases. The brief is explicit on
both counts: *"Every abstraction must correspond to a demonstrated application need"*, and *"be
conservative about declaring APIs stable... small enough that we understand the semantics of what we
commit to."*

Wrapping it now would freeze a shape that is still moving, for one and a half consumers.

### What the inspection found instead

**A socket is a delegated resource capability**, the same §7.10 mechanism as file-as-capability -
`resource_mint` to issue, `resource_invoke` to use, `last_recv_badge` to authenticate, with a rights
check on every operation (`LOP_ACCEPT` and `LOP_CLOSE` both demand `RIGHT_WRITE`, because both change
what the machine does on the wire).

That mechanism has **two independent issuers already** - `fs` for files and `net-stack` for sockets -
plus the shell as a consumer of both, and two worked examples. So the abstraction with demonstrated
need is not `net`. It is:

> **`gs::cap` - hold, invoke and narrow a delegated resource capability, without hand-rolling the
> badge and rights dance.**

That is the honest next slice by the brief's own test: it removes plumbing that is genuinely repeated
across two unrelated services, it wraps one named Godspeed mechanism rather than a protocol still in
flux, and it cannot widen authority because rights only ever narrow on transfer (§7.3).

It is NOT being built now either, for the same reason `net` is not: the shell is the only real
consumer today, and a second would settle the shape. **Recorded, not started.**

### What would change the answer for `net`

A second substantive client. If `time` ever needs more than one byte, or an application wants a
socket, the repeated plumbing appears and `gs::net` earns its place - most likely on top of `gs::cap`
rather than beside it.

---

## 9. `net` built, and the dogfood earned its keep

Built at the operator's direction, overruling §8's "not yet". The surface is `status`, `resolve`,
`ping`, `arp`, `tcp` and `renew`, plus a pure `addr::Ipv4`.

**Two deliberate boundaries**, stated in the module rather than left to be discovered: UDP sockets
and TCP listeners are NOT included, because those are delegated resource capabilities and need
machinery shared with file capabilities - half-building that twice is how it goes wrong. And `ping`
returns `Ok(false)` for silence rather than an error, because a diagnostic tool that conflates "no
answer" with "the request failed" lies about which half is broken.

### The bug the migration found, which is the point of migrating

The first draft sent `[opcode, ..]`. The real framing is `[tag, patience_secs, opcode, ..]`:

```rust
// services/net-stack
None => match (pl_raw.first(), pl_raw.len()) {
    (Some(t), n) if n >= 2 => (&pl_raw[2..], Reply { tag: Some(*t) }),
    _                      => (pl_raw,      Reply { tag: None }),
}
```

Any request of two bytes or more has its first two bytes eaten. So `status()` - one byte, below the
strip threshold - worked by accident, while `resolve("example.com")` had its opcode taken as a tag
and dispatched on `'x'`. Every call but one was wrong, and all of them compiled.

It was found by reading `services/shell`'s own client while looking for something to migrate, which
is exactly what the brief predicts: *"the migration is part of the test."* Nothing short of a target
run would otherwise have caught it, because the failure mode is a machine quietly talking to the
wrong thing.

The module header had already warned that `net-stack`'s protocol moved 34 times in three releases and
that the cost of being wrong here is not a compile error. That warning turned out to be about the
draft immediately below it.

### What is still owed

- **The network migration itself.** `gs::net` is written and correct against the protocol, but no
  utility has been moved onto it yet: the shell's net client lives behind `ShellCtx`, whose
  `ServiceContext` is private, so the migration needs a small accessor rather than a rewrite. That
  is the next commit, not a design problem.
- **Target-side tests**, for the same reason as §7: the interesting failures are a service that
  restarts and a deadline that passes, and neither is reachable on the host.
