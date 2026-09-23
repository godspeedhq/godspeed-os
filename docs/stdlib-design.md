<!-- SPDX-License-Identifier: GPL-2.0-only -->
# A native standard library for GodspeedOS - Phase 1 design report

**Status: REPORT ONLY. No code written, no crate created, nothing implemented.** This is step 2 of
the deliverable process, and step 7 is "WAIT FOR REVIEW". What follows is what the repository
actually contains, what it says about the shape of a standard library, and the one finding that has
to be settled before a single line is written.

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
