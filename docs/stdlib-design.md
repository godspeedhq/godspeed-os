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
two that remain are a protocol tag constant of its own and a comment. Its filesystem call helper went from 55
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

**Host tests cover the error model only, and deliberately.** `godspeed_sdk` provides the `panic_handler`
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

## 10. The network dogfood: `cmd_tcp`, and the result that went the wrong way

The migration §9 said was next is done. The shell's `tcp` command no longer knows the wire format.

**`ServiceContext` was not private after all.** §9 recorded that this needed "a small accessor rather
than a rewrite". Wrong: `ShellCtx` has a `Deref` impl, so `&*ctx` yields `&ServiceContext` and the
migration needed no shell change at all beyond the call site. Recorded because the previous entry
sent the next reader after work that did not exist.

**The line count went UP, and that is the honest result.** `recorder` shrank 680 lines to 627. This
did the opposite:

| | before | after |
|---|---|---|
| lines, comments included | 61 | 75 |
| lines, comments excluded | 50 | 57 |
| sites naming the wire format (`payload[..]`, opcode `21`, `>> 8`, `ReqOutcome`) | 10 | **0** |

So the plumbing did disappear - all ten sites of it - and the function still grew seven code lines.
The growth is entirely in the outcome arms: `ReqOutcome` has three variants and `Error` has thirteen,
so what was three arms is now four plus a catch-all. That is not overhead, it is the honest cost of a
richer failure model: the old code could not distinguish "connected to nothing" from "net-stack
refused" because the SDK had no way to say it. **A library that makes the error model finer will make
some call sites longer, and reporting only the case that shrank would be choosing the flattering
instrument.**

**`Error::Cancelled` exists because of this migration, and it caught me making the exact mistake the
library exists to prevent.** The shell is explicit that the user's own `q` is not a fault. My first
cut of `request_within_notice` folded `ReqOutcome::Aborted` into `OutcomeUnknown` - collapsing "you
pressed a key" into "we do not know what happened to your request", which would have taught the
operator that a deliberate keypress produces an error line. That is the same lossy merge §7 records
the SDK making with `DeadlineOutcome`, committed by the library built to stop it, one branch later.
It survived a compile and every gate; what caught it was reading the three arms it had to preserve.

**`Net::with_notice` exists for the same reason.** `ns_request` always passed an `on_linger` callback
printing `[q] quit`, so a migration that ignored it would have silently deleted an affordance. The
callback takes nothing and returns nothing - it means only "you have been waiting a while" - so the
console stays entirely on the shell's side and the library learns nothing about terminals.

**The wire format was wrong in the first draft, and only the dogfood found it.** `gs::net` sent
`[opcode, ..]`. `net-stack` strips TWO bytes from any request of two bytes or more and echoes the tag
at reply byte 0, so a request must be `[tag, patience, opcode, ..]`. Everything except `status()`
dispatched on garbage - and `status()` worked *by accident*, because a one-byte request falls below
the strip threshold and reaches a default arm that answers status anyway. **A single passing call
concealed a module-wide framing error.** Fixed at the header, with the echoed tag now checked on
return (a mismatched tag is `Error::Malformed`, never read as this call's answer).

**No `net-stack` change was needed, or made.** `git diff main...HEAD -- services/net-stack/` is empty.
The format was always net-stack's; the library was taught to speak it. So this migration owes no
hardware re-test of the protocol - what it owes is the same target-side testing §7 and §9 already
record.

**Still owed, unchanged:** target-side tests against a service that really restarts, and `gs::cap`.

## 11. Four file operations, and three things the shell knew that the library did not

`gs::fs` gains `list_dir`, `create_dir_all`, `move_to` and `delete_all` - the ordinary file
operations that had callers hand-rolling them. Two corrections to what section 9 claimed about the
ceiling, made while reading the service rather than its comments:

- **`OP_CAPACITY` and `OP_FLUSH` are not file operations.** They are `block-driver` opcodes that
  `fs` uses as a CLIENT. Counting them as candidates for `gs::fs` was reading a constant list
  without reading what serves it.
- **`send_res!` already sends a REASON.** On `FS_ERR` the service appends its own words - "name
  already exists", "cannot move root", "source not found" - and `from_fs_status` mapped the status
  byte and dropped the sentence. A loud failure was being made quieter in transit (26.7). `Fs::reason`
  keeps it in a bounded buffer the handle owns, documented as diagnostic-only: branch on the
  `Error`, print the reason.

### The shell's hand-rolled version was better than mine, twice

`services/shell` has nine `OP_LIST_DIR` call sites behind one `DirCursor`. I wrote `list_dir` first
and read `DirCursor` after, which is the wrong order, and it had two properties I had missed:

1. **A page cap.** My loop followed `next` for as long as the service said `more` - an unbounded
   execution scope (26.6) resting entirely on the service behaving. `DIR_PAGE_MAX` is now the same
   512 the shell uses.
2. **A `cut` flag.** Hitting that cap must not look like finishing. The shell's own comment is the
   rule: *"a partial answer that reads as a complete one is the thing this whole mechanism exists to
   remove"*. My `Result<usize, Error>` had nowhere to put it - an error would have been a lie, since
   the entries WERE delivered, and a bare count would have been exactly the silent truncation the
   pagination handling existed to prevent. Hence `Listing { visited, complete }`: the count cannot be
   read without the field that says whether it is the whole directory.

**This is the dogfood working in the opposite direction from the usual story.** The library did not
teach the caller; the caller taught the library. Worth recording plainly, because a design report
that only lists what the new code improved is not an honest instrument.

### A hazard that had to be fixed before any migration was safe

`gs::fs` minted tags from a `0xC0..0xFF` band; the shell mints from the whole `1..=255` range. **Two
independent counters on one channel, with overlapping ranges.** A tag check only REJECTS a mismatch,
so a collision does not fail loudly - it lets a stale reply be ACCEPTED as the current request's
answer, which is precisely the "run it twice and the protocol loses step" desync tagging was added
to remove. Rare, silent, and therefore worse than common and loud.

So migrating one shell site while the others still called `next_fs_tag` would have introduced a rare
wrong answer. The band bought nothing (a tag need only differ from the request before it on the SAME
channel, and `net` is a different channel), so it is gone: `Fs` advances exactly as the shell does,
and `Fs::from_tag` / `Fs::tag` let a caller lend its counter and take it back. One counter per
channel.

### Architectural friction, reported rather than worked around

**`list_dir`'s closure borrows the handle mutably, so you cannot make another `fs` call while
listing.** That makes the API right for the simple case the Stranger Test asks about ("list a
directory") and wrong for an interleaved one.

`cmd_churn_verify` - the power-cut verifier - is the interleaved case: it lists `/churn` and reads
each file as it goes. Migrating it would have forced a two-pass collect into a roughly 16 KB stack
buffer the original never needed, on a shell stack already known to be tight. That is a memory
discipline regression caused by the shape of my API, so **it was not migrated**, and the reason is
recorded here instead of being engineered around. The page-at-a-time alternative that would fix it
is what `DirCursor` already is; whether the library should offer that second shape is an open
question and not a thing to add speculatively (26.2).

### What was migrated, and what it proves

`cmd_dir` - a pure listing, decoding exactly the five fields `DirEntry` carries. It also needed
`Fs::with_notice`, an inconsistency I had introduced rather than found: `cmd_tcp` got
`Net::with_notice` so a lingering request keeps offering `[q] quit`, and every shell filesystem call
uses `fs_request_q`, so migrating any of them without the equivalent would have silently deleted
that affordance (`utilities/0_conventions.md` rule 9).

What deliberately stayed in the shell: a name is rendered SAFE there, because that is a property of
writing to a terminal, not of reading a directory. A name holding `ESC [ 2J` clears the screen when
listed, scrolling away the listing meant to reveal it (found by `osdev test fs-fuzz`).
`DirEntry::name` is raw bytes precisely so this layer can decide - a library that pre-sanitised
would have made the hostile name unprintable AND unreachable, so `delete` could not remove it either.

Verified in QEMU, `osdev test files`, **245 passed 0 failed**, including the three cases that pin
exactly what this change risked:

```
many: dir lists all 45 entries across pages
many: dir does not report a truncated listing
many: dir shows both the first and the last entry
```

### A gate that was covering nothing

`scripts/doc_symbols_check.py` never scanned `stdlib/rust/src`. Every backticked symbol in
`docs/stdlib-design.md` was being checked against a source set that EXCLUDED the crate the document
is about, so the gate's silence read as a pass while it verified nothing. Found because a correct
reference (`request_within_notice`) failed. Fixed; the scan now covers the crate, and one stale
baseline entry could be dropped as a result.

**Still owed, unchanged:** target-side tests against a service that really restarts, and `gs::cap`.
Newly recorded: `backlog/45`, the published book has no standard-library section, which blocks the
Stranger Test from an honest first run.

## 12. `gs::cap` is NOT built, and the reason is a gap in the kernel surface

Section 10 named `gs::cap` - delegated resource capabilities, the mechanism behind "a file is a
capability" (7.10) - as the best next candidate. Reading the path before writing it turned up a
blocker, so it is recorded rather than built. `backlog/46` is the full entry.

**`resource_invoke` (syscall 31) is a SEND.** It embeds a reply cap and returns on delivery; the
caller then waits with a plain `recv`. That is exactly the shape `CLAUDE.md` 8.2's `CallDeadline`
amendment condemns:

> a service that SERVES clients on the endpoint it awaits replies on would therefore consume an
> unrelated client request, fail to match it, and drop it

That amendment fixed the NAMED-PEER path - `request_with_reply_call` matches the reply by its sender
- and left the RESOURCE path on the primitive it had just condemned. There is no `CallDeadline` form
of `resource_invoke`.

**The one existing caller is exempt by accident, not by design.** `services/shell` opens every
file-cap invocation with `while ctx.try_recv().is_some() {}`, draining whatever is queued. That is
safe only because the shell serves nobody on that endpoint. In a service that does, the same line
discards live client requests - so the shell working is not evidence the pattern is sound.

**A library cannot do either thing.** It cannot drain, because it does not know whether its caller
serves clients; and without draining it inherits a wait that can eat a message. `gs::cap` is meant
for ordinary services - a facility only usable by a task with no clients is not the public interface
22.7 measures - so it is not started.

**A correction to what this section first said.** It concluded "a new syscall is a new kernel
responsibility, so this is the operator's gate". That reasoning is wrong, and the operator corrected
it: **syscall count is not responsibility count.** A primitive completing the semantics of a
responsibility the kernel already owns is not kernel growth. The question is not "does this add a
syscall" but "does an existing MISCIS mechanism have an incomplete semantic".

Applying that test properly changes the finding, and makes it larger rather than smaller.

**The hole is generic, and it predates this library.** Three services mint delegated resource caps -
`fs` (files), `net-stack` (TCP connection and listener caps) and `shell` - and 7.10 is written in
terms of an opaque `ResourceId` whose meaning only the owner knows. `examples/resource-server` and
`examples/holder` are a worked pair naming no filesystem at all, and `holder` waits like this:

```rust
Ok(())  => Ok(ctx.recv()),   // routed: block for the owner's reply on our endpoint
```

An unbounded `recv`. If the owner dies after receiving the invocation and before replying, `holder`
hangs forever - Commandment VIII broken in the example that teaches the mechanism, and 8.6 has a row
promising exactly that caller is woken on the `Call` path.

So there are **two** holes, not one: no reply correlation (hole 1, above) and no `ReplyDead` reaching
resource invocation (hole 2), because a SEND never tells the kernel a reply is awaited.

**Answering the MISCIS test - if `gs::cap` never existed, would the hole exist?** Yes, and it already
does, in a published example and on the shell's socket path. `gs::cap` did not create this; it was
the first caller that could not look away from it, because a library cannot assume its caller serves
nobody.

**No kernel change is being proposed or made.** That a standard-library feature exposed this creates
no urgency to change the kernel, and a mechanism must justify itself independently of the feature
that revealed it. `backlog/46` records the question, the first-principles answers, the genericity
test, and what a future investigation should settle - including the entirely acceptable outcome that
`resource_invoke` is intentionally narrow, in which case its contract gets written down and
`examples/holder` gets fixed or explained.

`gs::cap` stays unbuilt. Nothing is lost: file-as-capability works today for the caller it has.

**The useful thing here is not a feature.** No code shipped from this section. What it produced is a
hidden assumption made visible - that resource invocation is safe only from a task whose endpoint is
otherwise idle, a contract nothing states and one published example does not honour.

## 13. The governing constraint: the library CONSUMES capability, it never CREATES it

Stated by the operator, and it is the rule the rest of this report should have been written against
from the start:

> The stdlib's job is to make existing Godspeed functionality pleasant and safe to consume. It isn't
> supposed to create functionality that the OS doesn't already possess.
>
> So if implementing a stdlib API apparently requires
> `stdlib API -> new syscall -> kernel modification`,
> that is a reason to stop and question the stdlib/SDK composition, not an invitation to modify the
> kernel.

### What this does to the `gs::cap` conclusion

Section 12 and the `backlog/46` triage answered "is the mechanism incomplete, and how expensive is
the remedy" - and concluded rung D with a remedy cheaper than first thought. **That framing was
subtly wrong, because it is not the library's question.** Under this constraint the answer is
shorter and firmer:

**`gs::cap` correctly does not exist, because the OS does not currently possess safe general
resource invocation.** A standard library cannot offer a guarantee the system underneath it does not
make. The work ended the moment the gap was identified; filling it would have been the library
manufacturing a capability rather than re-serving one.

The triage in `backlog/46` keeps its value - it rules out B and C with evidence, so a future branch
does not repeat the search - but it is a note for whoever picks the mechanism up, not a plan for
this branch. `feat/stdlib` touches neither `kernel/` nor `sdk/`: both diffs against `main` are
empty, and that is now stated at the top of the backlog entry so the analysis cannot be misread as a
proposal.

### The constraint is STRUCTURAL now, not a promise

The property that enforces this is `#![deny(unsafe_code)]` on the crate. Without `unsafe` the
library cannot issue a syscall, so it is confined to the SDK's safe surface and can only ever
re-serve what already exists. A stdlib able to reach the raw ABI could quietly grow a capability the
system does not have.

The attribute was present from the first commit. **Nothing was checking it**, and the check that
should have was broken in two independent ways, both found by adding the root and then deliberately
deleting the attribute to watch the gate fire:

1. **`unsafe_check.py`'s `DENY_ROOTS` was `("services", "examples", "osdev")`** - the stdlib was not
   scanned at all. The same gap class as `doc_symbols_check.py` not scanning `stdlib/rust/src`,
   found the same day. A new top-level crate is invisible to every gate whose roots are a hand-kept
   list, and being invisible reads exactly like passing.
2. **The presence test was satisfiable by a COMMENT.** It asked `DENY_ATTR not in text` against the
   raw file, and `stdlib/rust/src/lib.rs` explains the attribute in its module documentation five
   lines above declaring it. Deleting the real attribute left the gate green. The `#[allow]` scan in
   the same function already strips `//` before matching, for exactly this reason - the lesson had
   been learned for one half of the function and not the other.

Both fixed, and the guard now fires on a deleted attribute. Worth stating plainly: **for the whole
of this branch, the one property making "the library cannot manufacture capability" structural
rather than aspirational was resting on nobody deleting a line by accident.**

## 14. `gs::cap` is built, and the answer was rung B all along

Section 12 said this module would not be built because the OS did not possess safe general resource
invocation. That was right about the OS and **wrong about why**, and the error was mine: I had
recorded, in `backlog/46`,

> **A service-layer tag cannot fix this.** Echoing a caller-supplied tag would let the caller
> RECOGNISE a wrong message, but `recv` has already CONSUMED it and there is no requeue. Selective
> dequeue is inherently kernel-side.

The first sentence is true and the conclusion does not follow. **A caller does not have to requeue a
message it should not have taken. It can HOLD it and hand it back.** Correlation is what makes
holding possible, and correlation is a protocol property, not a kernel one. So this was rung B -
existing userspace machinery, composed wrongly - and it needed no kernel change. `kernel/` and `sdk/`
are still untouched on this branch.

### What was actually missing

`services/fs` speaks two protocols. The NAMED one (`OP_WRITE_FILE` and friends) carries a tag at byte
0 of the request and echoes it at byte 0 of the reply. The FILE-CAP one (`FOP_READ`/`WRITE`/`STAT`/
`CLOSE`) carried none. **Same service, same file, opposite answers to the same question.**

That asymmetry was the whole blocker. With nothing to match on, a holder must either block on a bare
`recv` and read whatever arrives as its reply, or drain its queue and destroy anything else in it.
The shell drains, which is safe only because the shell serves nobody.

The fix is one tag, restoring the convention the sibling protocol always had:

```text
request   [tag, FOP_*, ..]      (was [FOP_*, ..])
reply     [tag, FS_*,  ..]      (was [FS_*,  ..])
```

The edit is deliberately tiny: `p` is shadowed past the tag so every offset below is unchanged, and
the `send` closure prepends so every reply site is unchanged. One extra 3562-byte reply buffer in
`serve_filecap`, against a 256 KiB frame limit.

### What the library does with it

`File::invoke` sends tagged and waits. A message that is not the reply is **held in a bounded array
and handed back** via `File::take_held`, never dropped. If there is no room left to hold, the
operation stops rather than dropping: what was taken is handed back, what was not taken is still in
the kernel queue, and the caller gets `OutcomeUnknown` because that is the truth.

`HELD_MAX` is 2. A held message is a full 4 KiB `Message`, so the bound is small and readable from
the source (26.6.1) rather than generous and invisible.

**The obligation is stated rather than hidden**: a task that serves clients must drain `take_held`
after each operation. Ignoring those messages loses them just as surely as draining would have - the
difference is that here you are told.

### One design decision worth naming

`File` borrows `&mut Fs`. The two speak to one service over one endpoint, so they must share ONE tag
counter; two counters can mint the same tag for two exchanges in flight, and the result is not a loud
rejection but a stale reply silently accepted as the current answer. The borrow makes that
structural - **the borrow checker will not let the bug be spelled**. The cost is one open file per
handle, and `Fs::from_tag`/`Fs::tag` are the escape for anyone who needs two.

### The dogfood earned its keep again

`fcap` - the `osdev test file-cap` self-check that pins 22 Test 14 - gained a section repeating the
core property through `gs::cap`. It failed on the first run:

```text
fcap: FAIL gs::cap round trip - the service sent a reply this library could not parse
```

`invoke` VERIFIES the tag and returns the message whole; it does not strip it, because stripping
means rebuilding a 4 KiB `Message` per read. My own doc comment claimed otherwise - *"the tag is
already checked and stripped"* - and `read_at` and `size` believed the comment rather than the code,
reading the status byte as the first byte of the length. **A comment that lies is worse than no
comment, and this one lied to its own author within the hour.**

Fixed, and the two library checks are now NAMED assertions rather than folded into the aggregate,
because `gs::cap` is the one caller that must work from a task which also serves clients.

### Verified

- `osdev test file-cap`: **15 passed, 0 failed** (was 13 - the two new assertions).
- `osdev test files`: **245 passed, 0 failed**. An earlier run reported one failure,
  `tab abs-path timeout`, on a code path this change does not touch; it passed on re-run and is
  recorded here as the host-load flake it was rather than quietly re-run until green.
- `osdev test fs-restart`: **11 passed, 0 failed**.
- 13 of 13 gates green; `cargo test -p godspeed` 7/7.

### What is still open

`backlog/46` stays open, narrowed. The fs file-capability path is correlated now, but the same gap
remains wherever else a delegated resource cap is invoked:

- **`net-stack`'s socket and listener caps** carry no tag, so `sock` still relies on draining.
- **`examples/holder`** still does a bare `ctx.recv()` and hangs forever if its owner dies - hole 2,
  the missing `ReplyDead`, which a tag does NOT fix. That one is genuinely about the mechanism, not
  the protocol, and is left recorded rather than papered over.

## 15. The target-side tests, and the error the restart found

Section 7 recorded target-side tests as owed, and section 9 repeated it. They exist now, and the
first one turned up a defect that no amount of reading would have.

### `gs::fs` across a real restart was already covered - by a vacuous assertion

`osdev test files` kills `fs` twice (`chaos kill-storm fs 2`) and then runs `dir /`. Since `cmd_dir`
walks `gs::fs::list_dir`, that has been exercising the library against a genuinely dead-and-respawned
service all along. The assertion, though, was:

```rust
check!(!r.contains("storage unavailable"), "shell reacquires fs after its own restart")
```

**That passes on silence.** A hang, a dropped reply, an empty listing, a command that quietly gave up
- all of them contain no error string. It is the same vacuous shape that let `"390 writes"` satisfy a
guard looking for `"0 writes"` earlier in this branch. It now requires a real listing (the trailing
count) and the absence of both error lines, which makes it an assertion rather than a hope.

### `gs::cap` across a real restart: `fcap gsreuse`

A new command holds a `gs::cap::File` across a real `fs` kill and reads through it afterwards.
Pinned by four named assertions in `osdev test fs-reuse`, which went from 8 cases to 12.

**Scope, stated rather than quietly narrowed.** `fcap reuse` additionally deletes the original and
writes a same-length replacement into the freed blocks, proving the stale cap cannot read the
REPLACEMENT. This test does not, and the reason is the library's own design: `File` borrows
`&mut Fs`, so while the capability is held the handle cannot write the replacement - and dropping the
`File` to free the handle CLOSES the capability, which is the thing under test. The borrow is
deliberate (it makes a second tag counter unspellable), so the constraint is real. What is pinned
here is the half that is the library's to get right: a stale capability yields a NAMED error rather
than a hang, a silent success, or a wrong answer. The block-reuse case stays pinned by `fcap reuse`
against the same kernel and the same `fs`.

### Two things the test found that review did not

**1. `service_answered()` can never be true for a stale capability.** My first probe waited for `fs`
to come back by invoking the held capability and watching for `service_answered()`. It never fires:
**the kernel rejects a stale cap on the generation check BEFORE routing**, so the owning service is
never reached and never answers. The error model was right and the instrument was wrong. The probe
moved to `inspect_endpoint_generation`, a kernel query needing no `fs` request and therefore no tag -
which matters because the handle is borrowed for the whole scope.

**2. The library named the wrong cause.** The refusal printed:

```text
fcap gsreuse: the stale cap was refused - the service could not be reached (nothing happened)
```

`fs` was up and serving. What happened is that the capability was REVOKED when its issuer died (7.5).
"Could not be reached" names a different fault and sends an operator to check a service that is
running perfectly - precisely the complaint this branch wrote into `cmd_tcp` two sections ago.

**The information was there and I was discarding it.** `resource_invoke` returns
`IpcError::CapError(CapError)`, and `CapError` separates `CapRevoked` and `EndpointDead` from
`CapInsufficientRights`. My `invoke` collapsed all of them by GUESSING from the rights mask instead
of reading the error. It now reads it, and `Error::Revoked` exists:

```text
the capability was revoked - re-open it (nothing happened)
```

**`retry_is_safe` is FALSE for it, for the less obvious of the two reasons.** Retrying cannot
double-apply anything - the kernel refused before routing - so it is harmless. It also cannot ever
succeed, and a caller looping on that predicate would spin forever. The question the predicate
answers is "should I retry", and the honest answer is no: re-open. The host test says so in those
words, because the next person to add a variant will face the same choice.

### Verified

- `osdev test fs-reuse`: **12 passed, 0 failed** (was 8).
- `osdev test file-cap`: 15 passed, 0 failed.
- `osdev test files`: 245 passed, 0 failed.
- 13 of 13 gates green; `cargo test -p godspeed` 7/7.

Still owed, and now the only owed item from section 7: nothing. The remaining open work is
`backlog/46`'s hole 2 and the `net-stack` socket caps, neither of which is a library gap.

## 16. The socket capabilities, and a comment that had the problem backwards

`backlog/46`'s hole 1 is now closed for every delegated-resource issuer in the system. `net-stack`'s
socket, listener and connection capabilities carry a correlation tag, as `fs`'s file capabilities
already do.

**The machinery was already there and deliberately switched off.** `net-stack` has carried
`Reply { tag: Option<u8> }` - stripped in one place, echoed in one place - since the named protocol
was tagged. The badged path opted out, with a comment:

> A BADGED request is untagged: it is a socket capability invoking its owner, the badge already names
> the socket, and the client holds no ambiguity to resolve.

**That has the direction of the problem backwards.** The badge names the socket FOR THE SERVICE. It
does nothing for the CLIENT, which waits on its own single endpoint and cannot tell this reply from
any other message landing there. The client is the only party with an ambiguity, and it was the one
left without the means to resolve it.

The cost was visible two functions away, in the shell's `sock_invoke`: a queue drain, plus a
capability-reclaim the drain made necessary (SEC-35 - an ACCEPT reply carries an embedded CONNECTION
CAPABILITY, and a discarded reply leaves it in the pending FIFO for the next open to receive by
mistake). Safe only because the shell serves nobody.

A badged request needs ONE header byte, not the two a named request carries: the tag to echo.
Patience is the stash's business and a badged request does not go through the stash.

**The drain stays**, for the SEC-35 reason rather than the correlation one - an aborted invoke can
still leave a reply whose embedded capability must be reclaimed. What changed is that correctness no
longer RESTS on it. And a tag mismatch now reclaims any capability that arrived with the wrong reply
before discarding it, which is SEC-35's own failure approached from the other side: believing the
wrong reply would hand the caller a capability to the wrong connection.

### A guard that accepted the failure it was guarding against

The QEMU assertion for `sock` read:

```rust
check!(out.contains("sock: UDP socket cap - sent") || out.contains("socket cap invocation returned nothing"), ..)
```

The second string is printed on exactly one condition: `sock_invoke` returned `None` - the capability
invocation failed. **The guard for the socket-capability mechanism accepted the mechanism being
broken**, which matters precisely when the framing changes underneath it.

And the leniency it was protecting was not needed. The success line carries a count - "received {n}
bytes back", where `n` may be 0 - so a silent external peer already passes through it. The `None`
branch was never the external case; it is the local one. The assertion now requires the success line.

### What is verified, and what is NOT

- **Verified in QEMU** (`osdev test shell`, 203 passed 0 failed): the UDP socket capability opens and
  is invoked through the tagged framing, reporting the strong outcome rather than the fallback -
  `sock: UDP socket cap - sent 29 bytes to 10.0.2.3:53, received 1 bytes back`.
- **NOT verified anywhere:** `serve` - the TCP listener and connection capabilities (`LOP_ACCEPT`,
  `COP_RECV`/`SEND`/`CLOSE`). **No QEMU suite exercises it**, which was true before this change and
  is stated here because this change touches that path.

The framing itself is shared: one strip site, one echo site, common to all three capability kinds, so
the UDP test does exercise the framing that `serve` relies on. What is untested is the op-specific
behaviour - and no op arm was modified. The residual risk is concentrated in one place: the
mismatch branch of `sock_invoke`, which reclaims embedded capabilities, and which only an ACCEPT
exercises. That wants a real listener, which means hardware.

## 17. `gs::net::Socket`, and two deadline bugs the dogfood found

A UDP socket is a delegated resource capability exactly as a file is (7.10), so `Socket` and
`cap::File` now share one invocation path: `crate::resource`. The parts that are easy to get subtly
wrong - the one-shot reply cap's lifetime, holding a message that is not ours rather than dropping
it, reading the kernel's refusal instead of inferring it - live in one place, because two copies of a
subtle rule is how they diverge.

`cmd_sock` walks it, and the migration deleted the shell's old hand-rolled socket opener outright.

### A report that had been wrong the whole time

`net-stack` answers "nothing came back" with a single zero byte, and `cmd_sock` printed
`received {} bytes back` from the reply LENGTH. So a query nobody answered read as **"received 1
bytes back"** - and under QEMU that is exactly what happens, every run. The line had been claiming a
response that never arrived.

`send_to` returns `Ok(0)` for the sentinel, and the command now says so. The residual ambiguity is
the protocol's and is documented rather than hidden: a genuine one-byte `0x00` response is
byte-identical to the sentinel, so no reading of it can tell them apart.

### The rule this cost me twice

**A client's deadline must be LONGER than the worst case of the operation it waits on.** A deadline
shorter than the service's own bound does not bound anything useful - it converts a slow success into
[`Error::OutcomeUnknown`], which is strictly worse than waiting, because an unknown outcome forbids
the retry that would have fixed it.

Broken twice in this branch:

1. **`send_to` waited `NET_SECS` (10s)** while `net-stack` retries a datagram six times at two
   seconds apiece. Intermittent: one run said "nothing came back", the next said "THE OUTCOME IS
   UNKNOWN", from identical code.
2. **`cmd_tcp` lost half its patience in the migration**, silently. It waited `NET_TXN_SECS` (20s)
   before moving onto `gs::net`, and the move put it on `NET_SECS` (10s). Nothing in QEMU is slow
   enough to notice; a real site on real hardware is, which is where it would have cost the most to
   find. There is a `TCP_SECS` now, and the shell's timeout message names the constant it actually
   waits on rather than a different one.

**`SOCKET_SECS` is 30, and it is DERIVED - see section 19, which root-causes it.** It was recorded
here as "measured, not derived" and that gap is now closed: the retry budget is not 6 x 2 = 12, it is
6 x (2 + 1) = up to 18, because the waits are bounded by whole-second arithmetic.

### The assertion that had been hiding it

This was a PRE-EXISTING intermittent failure, and the reason nobody saw it is the guard:

```rust
check!(out.contains("sock: UDP socket cap - sent") || out.contains("socket cap invocation returned nothing"), ..)
```

The old path waited five seconds - shorter still - and printed "returned nothing" on timeout, which
that second string accepted. **The guard for the socket-capability mechanism accepted the mechanism
timing out.** Tightening it in section 16 is what turned a silent intermittent into a visible one,
and then into a fixed one. A run that goes 203/0, then 199/4, then 203/0 again is worth stopping for
rather than re-running until green.

### Verified

- `osdev test shell`: **203 passed, 0 failed** - with the honest line, `sock: UDP socket cap - sent
  29 bytes to 10.0.2.3:53, nothing came back`.
- `osdev test file-cap`: 15 passed, 0 failed - the `cap::File` refactor onto `resource` is
  regression-tested by the suite that pins 22 Test 14.
- `osdev test fs-reuse`: 12 passed, 0 failed.
- 13 of 13 gates green; `cargo test -p godspeed` 7/7. Still no `kernel/` or `sdk/` change.

## 18. `copier`, the fifth dogfood, and a mistake made three times

`services/copier` moves onto `gs::fs`. It is the best migration so far by the only measure that
matters - what it found.

| | before | after |
|---|---|---|
| lines | 937 | 870 |
| wire-format sites | 47 | **1** (its own control protocol, which is correct) |
| hand-copied fs opcodes | 11 | 0 |

### The bug it closes

```rust
const FS_TAG: u8 = 0xC0;
req[0] = FS_TAG;                                       // every request
if b.first() != Some(&FS_TAG) { return Fs::Failed; }   // every reply
```

**A CONSTANT tag.** It distinguishes copier's traffic from an untagged sender and nothing else - one
copier request is indistinguishable from the next, which is the entire purpose of a correlation tag.
A late reply to a request that already timed out passes that check and is read as the answer to the
following one. copier has explicit `Slow` (deadline-passed) handling, so the path is reachable rather
than theoretical. `gs::fs` carries a wrapping counter owned by the handle, and the handle now lives
for the life of the service.

### What the migration drove into the library

Two things, both pulled by a real caller rather than invented:

- **`read_at`** - a POSITIONAL read. `read_into` reads a whole file; copier copies one chunk at a
  time at an offset it chooses, so an interrupted copy can resume and a file larger than any buffer
  can move at all. The primitive already existed inside `read_into` and simply was not reachable.
- **Paths accept BYTES.** Every path parameter took `&str`, and copier stores paths as
  `[u8; PATH_MAX]` - as it must, because `services/fs` accepts any byte above 0x1f except `/` and
  0x7f. A `&str`-only API forces such a caller through `from_utf8(..).unwrap_or("")`, which turns an
  unreadable name into a request for a DIFFERENT file, silently. I had already reasoned this out for
  `DirEntry::name` - raw bytes, precisely so a hostile name stays reachable by `delete` - and then
  took `&str` on every path anyway. `impl AsRef<[u8]>` accepts both, so the documented
  `fs.read_into("/data/message.txt", ..)` still reads as it does on the published page.

### The same mistake, three times

`delete_all` waited `DEFAULT_SECS`, and its doc said:

> **Blocks** up to `DEFAULT_SECS`, and longer for a large tree than any single-file call.

**Which cannot happen.** A call does not block past its own deadline; it gives up. I wrote the right
intuition and then failed to give it a number - the third instance in this branch of a client
deadline shorter than the worst case it waits on, after the UDP socket and the TCP transaction.

copier had already worked this out for its own hand-rolled version, with a comment worth quoting
because it names the cost exactly: five seconds "was not nearly enough and said so in the worst
possible way - by blaming the filesystem". Its `TREE_SECS` is 120, which is the number `SWEEP_SECS`
already carried, arrived at independently.

**Three times is a pattern, not bad luck.** The rule belongs where an author will meet it: a deadline
is part of the OPERATION's contract, not a property of the caller, and every one of these was written
by reaching for the nearest existing constant instead of asking what the service does.

### What was deliberately NOT changed

`fs_idempotent`'s re-send-on-`Slow`, and its reasoning about which operations may use it -
*"re-sending must not be able to produce a different outcome than sending once"*, a stricter and
better rule than "reads are safe". The library refuses to retry on its own because it cannot know
whether a caller's operation is idempotent; that judgement is copier's and stays there, with its
comment carried across verbatim rather than retyped.

The three-way `Fs { Ok, Slow, Failed }` result also stays at the call sites, so ten of them did not
churn - but `Failed` now NAMES the failure in the log. It used to collapse NotFound, NoFilesystem,
PermissionDenied and a malformed reply into one word while `fs` had said which (26.7).

### Verified

`osdev test files` **245 passed, 0 failed**, including a real 45-file directory tree copied through
the library (`copied /many -> /manycopy (1 dirs, 45 files)`). `file-cap` 15/0 and `fs-reuse` 12/0
confirm the byte-path change disturbed nothing. 13 of 13 gates; host tests 7/7.

## 19. Root cause: whole-second deadlines carry a hidden +1

Section 17 left `SOCKET_SECS = 30` recorded as "measured, not derived - something in that path costs
more than the retry budget accounts for, and I have not root-caused it". Here is the cause, and it is
worth more than the constant.

`udp_roundtrip` retries `DANCE_TRIES` (6) times at `DANCE_SECS` (2) apiece, which reads as a
12-second budget. Each of those waits is `request_with_reply_deadline_sifted`, and its bound is:

```rust
let t0 = self.epoch_secs_monotonic();
...
if self.epoch_secs_monotonic() - t0 >= max_secs { return DeadlineOutcome::Timeout }
```

The deadline does NOT restart per sifted message - `t0` is taken once, which was my first hypothesis
and was wrong. What it does instead is arithmetic in **whole seconds**: `epoch_secs_monotonic`
returns an integer count, so a "2 second" deadline elapses when the COUNTER advances by two. That is
anywhere between just over 1s and just under 3s of real time, depending where in the second `t0`
fell. The worst case per wait is `max_secs + 1`.

```text
6 tries x (2 + 1) = up to 18 seconds, not 12
```

Which explains the behaviour exactly: 10 and 15 both sit below 18 and produced intermittent
[`Error::OutcomeUnknown`]; 30 sits above it and is stable. The constant is derived now, with margin,
and the doc says how it was arrived at.

### The rule, which is the useful part

**A deadline built from whole-second differences carries up to +1s of slop, so N chained waits of S
seconds bound at N x (S + 1), never N x S.**

Any budget computed the obvious way is short. And short is not a small error here: it converts a slow
SUCCESS into an unknown outcome, which is the one error that forbids the retry that would have fixed
it. That is the same failure mode as the three deadline bugs in sections 17 and 18 - this is their
cause rather than a fourth instance.

**What is NOT affected**, checked rather than assumed: `tcp_transact` takes a MILLISECOND budget
(8000), so it does not chain second-granularity waits, and `TCP_SECS` at 20 has real margin over 8.
`SWEEP_SECS` at 120 is a single wait on one whole-volume operation, not a chain, and `services/copier`
arrived at the same 120 independently for its own version.

### Worth knowing beyond this library

This is a property of every deadline in the system built on `epoch_secs_monotonic` differences, not
of the standard library. Anyone sizing a budget out of a retry count and a per-try timeout is
computing N x S and getting a number that is up to N seconds short.

### The fourth instance, in the test harness

Section 19's rule broke once more before the day was out, and this time I caused the breakage by
fixing something else. Recorded because the shape is now unmistakable.

Raising `SOCKET_SECS` to 30 made `sock` able to take thirty seconds. The harness step that reads its
output waits:

```rust
let sock_out = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(8))
```

**Eight seconds.** Correct while the shell gave up after five; wrong the moment the command could
outlive it. And the failure is not local - when `collect_until` returns early the harness is reading
the wrong place in the stream, so EVERY LATER STEP cascades.

The symptom was five consecutive runs of one build giving **12, 6, 1, 11 and 50** failures. I spent
two of those runs treating it as host load, which it was not: the serial log carried **zero kernel
panics and zero liveness wedges**, and simply stopped partway. That is the signature of a harness
losing its place, not a guest breaking, and reading it would have been quicker than re-running.

What decided each run was whether the DNS peer answered inside eight seconds.

So the rule holds in one more place than stated: **a waiter's bound must exceed the work it waits
on - and a test harness is a waiter.** Four instances now: the UDP socket, the TCP transaction, the
recursive delete, and the harness step that watches them.

## 20. `services/time` is correctly NOT migrated, and it names a real gap

`time` was the last service talking to `fs` and `net-stack` by hand, so it was the obvious sixth
dogfood. It should not be one, and the reason is worth more than the migration would have been.

```rust
/// Ask `fs` for the persisted floor. Non-blocking: the answer arrives later, tagged.
fn floor_load(ctx: &ServiceContext) -> bool {
```

**`time` never waits for a reply.** It sends a request carrying a reply cap and returns immediately;
the answer arrives later and is matched by tag in its main loop. That is not an accident of style -
it is the clock service, and a clock that blocks on the filesystem stops being one.

`gs::fs` and `gs::net` are synchronous request/reply. Putting `time` on them would make it block,
which is a functional regression in the one service that must never stall. The brief's own rule
settles it: **do not make Godspeed easier to use by making Godspeed less Godspeed.**

Its net-stack use is smaller still - a fire-and-forget `try_send(&[11])`, the capless clock nudge,
with no reply expected at all. There is nothing there for a request/reply library to remove.

### The gap, stated plainly

**This library has no answer for a client that cannot block.** Everything in it is
send-then-wait-for-this-reply. A service that must keep serving while an answer is outstanding has to
do what `time` does: hand-roll the reply cap, tag the request, and demultiplex replies in its own
loop - which is exactly the plumbing the library exists to remove, in the one case where it cannot.

That is not an argument for adding an asynchronous surface now. It has ONE caller, and a second
would be needed before the shape could be designed from evidence rather than imagination (26.2). It
is an argument for writing the boundary down, so the next person does not read "no repeated plumbing
was found in `time`" - which is what the brief says - and conclude the plumbing is not there. It is
there; it is just a different shape, and this library does not fit it.

**Five migrations, and the sixth candidate correctly refused.** `recorder`, the shell's `tcp`, `dir`,
`fcap`, `sock` and `copier` all shrank and all found defects. `time` would have grown a bug.

## 21. TCP listen and accept, and the objection that was wrong

Section 20 ended by arguing against wrapping TCP listen/accept: `serve` had no QEMU coverage, so a
migration could not be verified, and the surface would have no proven caller.

**The premise was wrong, and the operator said so: create a caller from outside and connect to it.**

SLIRP restricts the GUEST reaching outward - its only peer is the gateway, which is the limitation
`docs/` records and which I had generalised into "inbound cannot be tested either". Inbound is
exactly what `hostfwd` is for, and the harness that already drives the guest over serial can equally
open a TCP socket and be the client. One line:

```
-netdev user,id=n0,hostfwd=tcp:127.0.0.1:18080-:8080
```

### The test, before the feature

`osdev test shell` now runs `serve 8080`, connects from the HOST, sends bytes and requires them back.
Three assertions in increasing strength: the guest is listening, **a host client's bytes make the
round trip**, and the guest accepted through ACCEPT's embedded capability.

That last one is why this mattered beyond coverage. `LOP_ACCEPT` returns an **embedded connection
capability**, and that is the riskiest part of the socket-capability tagging added in section 16: a
reply believed on a mismatched tag would hand the caller a capability to the WRONG CONNECTION -
SEC-35 approached from the other side. Nothing exercised it. I had flagged that gap and then proposed
to leave it open, which was wrong twice.

### Then the feature

`Net::listen` returns a `Listener`; `Listener::accept` returns `Ok(None)` when nobody is waiting -
an ordinary poll result, not an error - and `Ok(Some(Conn))` carrying a capability to one connection.
`Conn` has `recv`, `send` and `close`.

**The borrow structure is the design, not an artefact:**

```text
Net  --&mut-->  Listener  --&mut-->  Conn
```

One connection at a time, and one tag counter owner all the way down. That is `serve`'s actual model
- accept one, answer it, close, accept the next - now enforced by the compiler rather than remembered
by the author. It also forced one genuine improvement: the accept poll became a labelled loop that
YIELDS the connection, because an `Option<Conn>` carried across iterations would still hold the
listener borrow when the next `accept()` wanted it. The loop now has one job and one result.

### The sixth migration

`cmd_serve` runs on it. What stayed is what `serve` MEANS: the address banner (asked of net-stack
rather than remembered, so a changed lease cannot go stale), the ten-second live sign, `q`, the
printable-only rendering of a peer's bytes, and the 250 ms poll with its reasoning. What went is the
wire format - the shell's socket-invoke helper, its listener-release helper and three opcode
constants deleted, 87 lines net.

Verified by the test written first: **206 passed, 0 failed**, with a real host TCP client.

### One diagnosis worth recording

After the dead-code cleanup the suite reported 200/6. The cleanup removed only items whose sole
remaining occurrence was their own definition, and all 30 services built - so it was inert, and the
serial log said so plainly: **the shell never printed its listening banner, net-stack started three
times, and one of its passes took 8 seconds.** net-stack had restarted twice under host load. A clean
re-run gave 206/0.

That is the fourth time on this branch that a load-induced cascade has looked like a regression, and
the third time the serial log answered it faster than another run would have. The tell is consistent:
**no kernel panic, no wedge, and output that simply stops.**

## 22. The dogfood is finished, and what it found in the last mile

The sweep is done: every `fs` and net-stack call in `services/shell` goes through the library except
two groups that are kept on purpose and named below. `services/copier` and `services/recorder` were
migrated earlier. The shell is the interesting one because it is the biggest client of both services
and the one an operator is looking at when something goes wrong.

### What the last five commits moved

| Commit | Sites | What it removed |
|--------|-------|-----------------|
| dogfood 17 | five `STAT` sites, `churn reset` | five independent decodings of the same eleven bytes |
| dogfood 18 | `mkdir`, `delete` | two commands whose whole job is a mutation, reporting it as a status byte |
| dogfood 19 | five directory walks | five copies of the page loop and the 15-byte entry stride |
| dogfood 20 | seven deadline-bounded sites | `fs_request_bounded` and the shell's `OP_READ_FILE` decoder |
| dogfood 21 | `churn verify` | the last walk that could be collected before it reads |

Plus 195 lines of helpers that had nothing left calling them.

### What stays hand-written, and why

**The seven `fcap` sites.** Deliberate. §22 Test 14 is the identity test for file-as-capability, and a
test that exercises the library rather than the protocol proves less about the protocol. They are the
control group and should stay one.

**`copy <src> <dst> recursive`.** It calls `fs` again for every entry it lists - `mkdir` for a
directory, a streaming copy for a file - and `list_dir`'s closure holds the handle for the length of
the walk, so a nested call cannot be spelled. `churn verify` had the same shape and was migrated by
collecting names first, because its set is bounded at eight by construction; a subtree is not bounded
that way, and buffering one is several kilobytes on a frame that is already tight
(`[[project-shell-stack-pipe]]`).

The route that would close it is a **paging API** - hand back one page, drop the borrow, let the
caller do what it likes between pages - which is `DirCursor`'s shape, already proven in the shell. It
is not written, because two call sites is not yet a reason to have one (§26.2), and this is recorded
rather than half-done (§26.7). Nothing about the protocol forbids the nested call: the page reply is
already in hand when the closure runs, so the tags cannot interleave. It is a borrow, not a race.

### Three defects the last mile found, none of them in the code it was migrating

**A tree delete cannot say "it failed".** `delete <path> recursive` frees in batches, so a lost reply
leaves the tree possibly whole, possibly half gone, possibly untouched - and the old line asserted the
last of the three. Same for `churn reset`. Both name the command that settles it now.

**A badged request had no way to say how long its client would wait.** `net-stack` held one for a
fixed 1500 ms and dropped it, while `serve`'s `accept` waited twenty seconds for an answer that no
longer existed. The service had diagnosed exactly this for its NAMED path and fixed it there;
`HOLD_MS`'s doc comment says so in the words the defect deserved - "a constant cannot know a client's
deadline, so it stopped guessing and the client now says". It stopped one path short. Slowest serve
pass: 64,063 ms before, 1,735 ms after.

**The library was unsafe for a caller that serves clients.** `gs::call::request_within` waited with a
plain recv, which takes whatever lands next - so a client request arriving mid-wait was consumed,
misparsed, and lost. `services/recorder` demonstrated it the day it stopped hand-rolling its own
request: the selfcheck polls `events persist status` once a second, and a capture died because the
shell asked it a question at the wrong moment. The kernel has `CallDeadline` for exactly this
(CLAUDE.md §8.2), the SDK exposes it, and the recorder had been using it before the migration - so
the migration traded a correct primitive for a convenient one. Fixed by using the primitive; the
whole named path is now safe for a serving caller, which is what `gs::cap`'s module header had been
claiming for the library as a whole.

That third one is the most important thing the dogfooding produced, and it is worth being plain about
why: **it was found by a service USING the library, not by reading it.** The module header that
promised the property and the function that did not deliver it are 300 lines apart in the same crate
and had both been reviewed.

### Two gates were wrong, and both are fixed rather than worked around

`Commandment IX`'s standard-library check required the literal `DeadlineOutcome::SendFailed` in
`gs::call`. It failed a change that preserves exactly what it guards, and would have passed a file
that named the variant in a comment while reacquiring on the deadline. It reads the match arm the
reacquire sits in now, which is the question IX actually asks.

The harness's `selfcheck` window was 150 seconds. `fail` ends a gsh run, so while one statement in the
middle of the suite was failing, only 326 of its 509 statements ever executed - and the window had
been chosen against that shortened run. Fixing the failure made the suite 56% longer and the window
was then under the work it waits on. Three runs gave 5/0, 5/0, 3/2; at 300 seconds, three for three.

Both are the same shape as the four deadline bugs in §19, one layer up: **a bound chosen against a
smaller version of the work is not a bound on the work.**

---

## 23. What this branch is validated on, and what it is not

Recorded here rather than left to be inferred, because the answer is not uniform across the tree and
the uneven part is the part that matters (§26.7: a limitation that cannot be closed today is written
down at the place a reader would otherwise rely on the claim).

### The strongest fact is the smallest diff

```
git diff main...HEAD -- kernel/ sdk/     ->  empty
```

No kernel source change and no SDK change. So nothing on this branch can have altered kernel
behaviour, and every property main held, it still holds. That is not a claim resting on a test run;
it is a claim resting on there being nothing to test. It takes the whole §22 surface off the risk
list by construction.

What the branch DID change that runs on every port:

```
services/shell/src/main.rs      2490 lines
services/recorder/src/main.rs    132
services/net-stack/src/main.rs    48     (the badged header byte and client patience)
services/fs, services/copier     the rest
```

9,813 insertions and 2,482 deletions across 72 files in total.

### Validated on hardware: x86_64 only

HP T630, on the image built from this branch:

- `selfcheck` 516/0, all nine parts, one tally, zero skips, zero detail-cap lines
- `net` lease, gateway and ping; `tcp` real SYN/RST with on-link ARP
- **`serve` 3/3 accepted with byte-exact echo from a peer on the LAN** - an inbound connection
  through a minted connection capability. This is the one path QEMU structurally cannot produce,
  because SLIRP's only peer IS the gateway, so it had never run before this branch.

### Validated by build and gate: all four ISAs

The other three ports had never been COMPILED on this branch, which made the stack-fit gate's green
misleading: it checks every target that has been BUILT and silently covers no others, so it was
reporting on x86 alone. All four are built now and it is no longer blind:

```
stack fit: deepest single frames (limit 256 KiB)
      81920 bytes (31.2%)  console: service_main
      61440 bytes (23.4%)  shell: cmd_edit
      57344 bytes (21.9%)  fs: service_main
      49152 bytes (18.8%)  net-stack: service_main
stack-fit: every frame fits the 262144-byte user stack:
  aarch64-unknown-none (28 services), armv7a-none-eabi (27),
  riscv64imac-unknown-none-elf (28), x86_64-unknown-none (30)
```

Zero hard errors on any port. The warning counts (372 / 385 / 393) are pre-existing categories, and
the ones naming SDK functions are main's by definition since `sdk/` is zero-diff.

19 of 19 checker scripts pass with all four targets present: `scripts/arch_boundary_check.py`,
`scripts/arch_seam_check.py`, `scripts/backlog_check.py`, `scripts/commandments.py`,
`scripts/contract_check.py`, `scripts/dash_check.py`, `scripts/doc_refs.py`,
`scripts/doc_symbols_check.py`, `scripts/embed_order_check.py`, `scripts/facts_check.py`,
`scripts/foreign_word_check.py`, `scripts/line_ending_check.py`, `scripts/line_ref_check.py`,
`scripts/port_scope_check.py`, `scripts/scaffold_check.py`, `scripts/service_embed_check.py`,
`scripts/shared_surface_check.py`, `scripts/site_check.py`, `scripts/unsafe_check.py`.

### What is NOT validated, stated plainly

**No non-x86 machine has booted this branch.** A build and a stack-fit are a real bound but they are
a static one, and two risks on this branch are dynamic:

1. **The shell changed 2,490 lines** and is the crate with the least headroom in the tree. Stack-fit
   now covers it on every ISA, which is the cheap half of that question; the expensive half is that a
   frame the checker cannot see (a prologue form it does not match, recursion it cannot count) only
   shows up when the machine runs it.
2. **`net-stack`'s wire format changed.** The badged path strips two header bytes instead of one and
   `Displaced::note` reads patience from `pl.get(1)`. That is protocol, and it sits in front of four
   different controllers (e1000/RTL8168, smsc95xx, GENET, dwmac). x86 exercised one of them.

An ARM boot running `selfcheck` and `net` is what would close both, because ARM is where both live.
The Wyse adds a second x86 machine with different firmware and the 4K console path, which is worth
having and touches neither.

### Open items that are not about this branch

- `backlog/48` and `backlog/49` - the userspace-reachable kernel panic class. A genuine violation of
  an absolute bar (§22: the kernel must never panic on user-controllable input), and the kernel is
  zero-diff here, so this branch neither causes it nor worsens it.
- `backlog/50` - `nic-driver` read 100% on the T630. One sighting, one machine, not root-caused;
  `services/nic-driver/` is byte-identical to main.
- `backlog/51` - the display-only console status region. Designed, deliberately not built: it changes
  the terminal every port renders through.

### Both x86 machines, and why their selfcheck counts DIFFER (2026-09-25)

**Dell Wyse 5070, on the same image as the T630:**

```
selfcheck   ran 518, failed 0, skipped 0
sock        sent 29 bytes to 192.168.4.1:53, received 94 bytes back
net         192.168.4.83, gw 192.168.4.1, ping ok, lease ok (DHCP), dns 192.168.4.1
serve       3 inbound connections from a LAN peer, every byte returned unchanged
```

**The T630 reported `ran 516` on that same binary, and both numbers are correct.** This is worth
writing down because the difference reads as a regression and is not one; it cost a round of
investigation here and would cost the next person the same.

`ran` counts STATEMENTS EXECUTED, and the suite has conditional blocks. The one that bit is in
`60-data.gsh`:

```
if dir /churn {
    echo 'selfcheck: an earlier churn is still on disk - verifying it BEFORE it is overwritten'
    if churn verify { echo 'PASS  churn - the earlier run holds no torn file ...' }
}
```

The Wyse's disk carried churn files from an earlier session, so that branch ran and added exactly two
statements. The T630's disk had been freshly flashed, so the block did not run at all. 516 + 2 = 518,
and the extra two PASSED - the leftover data verified intact, which is precisely what that block
exists to check before overwriting it.

**So a machine with disk history does MORE checking, not less, and a higher count is the healthier
reading.** The numbers that matter are `failed 0` and `skipped 0`; `ran` is a function of machine
state and is not comparable across machines without knowing the state. The retry loops in
`40-persist.gsh` (`for i in range 30`) and `80-network.gsh` (`for i in range 4`) vary the same way:
their bodies are guarded, so a first-try success executes fewer statements than a retry.

This is a documentation gap rather than a defect - nothing told the operator that `ran` is
state-dependent, and `selfcheck` has no `utilities/` spec to say it in (`audits/documentation-audit.md`
A7-4). Recorded here until it does.

**What the Wyse adds over the T630**, since a second x86 machine is not automatically new evidence:
different firmware, a different storage controller, the 4K console path whose framebuffer memory type
was the 596 ms -> 29-41 ms per-scroll fix, and a disk with history rather than a fresh format. The
~2,750-line selfcheck ran in 96 seconds, so the console fix is intact on this board.

**`sock` on a second machine.** Byte-identical result to the T630 - 29 out, 94 back, from the
lease-supplied resolver. The `udp_roundtrip` fix (`5716da17`) is now confirmed on two boards, and it
is a path that had never once completed on real hardware before that commit.

**`serve` on a second machine.** Three inbound connections from a separate LAN peer, each echoed byte
for byte. QEMU structurally cannot produce this (SLIRP's only peer is the gateway), so it is two
machines' worth of evidence for the one thing emulation cannot test at all.

### The Pi 2 boots this branch, and both named risks are CLOSED (2026-09-25)

The section above says "**No non-x86 machine has booted this branch**" and names two risks that only
an ARM boot could answer. That is no longer true, and this is the correction rather than a new claim.

**Raspberry Pi 2 (ARMv7, BCM2836), on the branch image:**

```
selfcheck   ran 509, failed 0, skipped 1
sock        sent 29 bytes to 192.168.4.1:53, received 94 bytes back   (twice)
net         192.168.4.84, gw 192.168.4.1, ping ok, lease ok (DHCP), dns 192.168.4.1
serve       3 inbound connections from a LAN peer, every byte returned unchanged
```

**The one skip is correct and says so**: `hw-enumerator - this machine has no PCI to enumerate
(Pi 2); not a failure`. A skip that names its reason is the behaviour this suite is built for; a
silent one would be a test that had quietly stopped testing.

#### Risk 1: the shell's stack headroom on ARM - CLOSED

`services/shell` changed 2,490 lines on this branch and ARM has the tightest user stack in the tree.
Static stack-fit passed at 23.4% of budget on all four ISAs, but a frame whose prologue the checker
cannot match is invisible to it - which is why the static pass was recorded as the cheap half of the
question and a boot as the expensive half.

The Pi 2 reached the prompt, ran 509 checks across all nine parts including the pipe and record paths,
then ran `serve` and answered three inbound connections. No fault, no wedge. The expensive half is
answered.

#### Risk 2: the changed net-stack header in front of a different NIC - CLOSED

`services/net-stack` changed 48 lines, and the badged path now strips two header bytes instead of one
with `Displaced::note` reading patience from `pl.get(1)`. That is a wire format, and both x86 boards
share an RTL8168; this board is smsc95xx over USB, a different driver and a different bus.

`net` took a lease and pinged, `sock` completed its round trip **twice**, and `serve` accepted three
connections. The header change holds across three controllers now (e1000/RTL8168, RTL8168, smsc95xx).

#### What `sock` proves here specifically

29 bytes out, 94 back, to the lease-supplied resolver - **byte-identical to both x86 boards**. That
path had never once completed on real hardware before `5716da17`, and `udp_roundtrip`'s three defects
(re-transmitting instead of RX-polling, never answering an ARP for us, never pacing) were all things
QEMU structurally cannot expose. Three boards, three NICs, three drivers, same answer.

#### The count, again, and why it is not comparable

`ran 509` against the T630's 516 and the Wyse's 518. All three are correct: `ran` counts statements
EXECUTED and the suite is full of guarded blocks, so the figure is a function of machine state and
hardware, not of correctness. The Pi 2 skipped the PCI block it has no bus for and did not carry the
leftover churn files the Wyse did. **`failed 0` is the comparable number**; `ran` is not, and reading
it as one has now produced a false alarm once.

### Coverage after this run

| board | ISA | NIC | selfcheck | sock | serve |
|-------|-----|-----|-----------|------|-------|
| HP T630 | x86-64 | RTL8168 | 516 / 0 | 94 B | 3/3 |
| Dell Wyse 5070 | x86-64 | RTL8168 | 518 / 0 | 94 B | 3/3 |
| Raspberry Pi 2 | ARMv7 | smsc95xx | 509 / 0 | 94 B | 3/3 |

Three boards, two ISAs, two NIC families, and `serve` answering a real LAN peer on every one of them
- the single thing QEMU cannot test at all, because SLIRP's only peer is the gateway.

**Still unbooted on this branch: the Pi 4 (AArch64) and the VisionFive 2 (RISC-V 64).** Both build
clean and both pass stack-fit, and the two risks that made the ARM boot load-bearing are now answered
on ARM - but neither of those boards has run this code, and that is stated here rather than implied
away.
