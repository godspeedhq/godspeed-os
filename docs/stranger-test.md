<!-- SPDX-License-Identifier: GPL-2.0-only -->
# The Godspeed Stranger Test

**The protocol. The bar it measures against is `CLAUDE.md` §22.7, which is where the commitment
lives.**

Use the least capable model available.

The purpose is not to find out whether a sophisticated model can understand Godspeed's architecture.
It is to find out whether Godspeed's **public developer interface** is simple and coherent enough
that a weak programmer, human or AI, can use it without understanding the internals.

---

## Conditions

Use:

- fresh context
- a weak model
- public developer documentation only
- one ordinary application requirement
- no architectural coaching

Do **not** provide: kernel source, supervisor internals, existing utility implementations, raw IPC
definitions, private SDK interfaces, an explanation of MISCIS, hints about the capability table, or
architecture-specific detail.

## The first test

> "Write a Godspeed utility that reads `/data/message.txt`, prints its contents, and reports a useful
> error if the operation fails. Use only documented public APIs."

## What to measure

The important question is **not** "does it compile". Observe whether the model can:

1. Discover the correct standard-library API from the documentation.
2. Avoid raw IPC and private SDK machinery.
3. Use only the authority available to the application.
4. Handle failures honestly.
5. **Avoid blindly retrying an operation whose outcome is unknown.**
6. Avoid architecture-specific hacks.
7. Avoid unsafe code.
8. Understand compiler and API errors well enough to repair its own program.
9. Produce a working program without needing to understand the kernel.

Item 5 is the one to watch hardest. It is the failure a weak model is most likely to commit
confidently, it is invisible in a passing build, and the API answers it directly: `Error::retry_is_safe`
returns `false` for `OutcomeUnknown` precisely so that nobody has to derive that rule themselves.

## The difficulty ladder

Once the simple test succeeds, raise it one step at a time, each level still on public documentation
and public APIs only:

```
  filesystem utility  ->  network client  ->  service  ->  backgroundable command
```

## Let the repository teach

Do not explain Godspeed's architectural rules to a weak model and then congratulate it for following
them. Let the enforcement layer do the work, and treat every rejection as an experiment with two
outcomes worth recording: did it fire, and did the message let the model recover?

| If the model tries | What should stop it |
|---|---|
| `unsafe` | `#![deny(unsafe_code)]`, at the compiler |
| `#[cfg(target_arch = ...)]` as a workaround | `arch_boundary_check.py`, `shared_surface_check.py` |
| raw IPC or a private interface | no public path exists; the standard library is the surface |
| acquiring authority it was not granted | the capability returns `Unreachable`; the contract is the answer |
| retrying after `OutcomeUnknown` | `Error::retry_is_safe` says `false`, and the docs say why |
| a peer it cannot recover | `commandments.py` `IX-peer-reacquire` |
| ambient authority | `commandments.py` `VII` |

**A gate that fires with an unhelpful message is a finding.** The ideal rejection contains enough
information for the model to reach the correct public interface on its own; one that merely says no
has identified a documentation defect, not a model defect.

## The real test

The correct path should be **easier than cheating**. A weak model should fall into the supported
programming model because the API, the documentation and the repository boundaries lead there, not
because it was told to.

It should not need to know how IPC is encoded, how service reacquisition works, where capabilities
live internally, how the supervisor is implemented, which ISA it is on, or which kernel mechanism
implements the operation.

It should need to know: *I want to read a file. I want to print something. I want to open a
connection. I want to handle this failure.*

## The inversion

```
  the earlier weak-AI test            the stranger test

  weak AI tries to MODIFY Godspeed    weak AI tries to USE Godspeed
            |                                    |
            v                                    v
  the architecture prevents           the public interface makes the
  it from cheating                    correct path easier than cheating
```

The first is in `audits/documentation-audit.md` as the grokability probe: can a weak model regenerate
a document from its neighbours? That asks whether Godspeed can be **understood**. This one asks
whether it can be **used without being understood**. Same instrument, opposite question, and neither
substitutes for the other.

## The v1 principle

MISCIS should be something application programmers learn **because they are curious**, not something
required before they are allowed to do ordinary work.

The developer expresses WHAT they want. Godspeed's public interfaces handle HOW that maps onto the
underlying mechanisms, **without hiding authority, failure or uncertainty**.

## Final rule

Make the correct path the easiest path. Then give Godspeed to the weakest programmer in the room and
do not help them.

---

## Status

**RUN 1: 2026-09-23.** Least-capable model available, fresh context, restricted to
`website/src/stdlib.md` and the rendered `/api` pages. No architectural coaching. Task: the first
test above, verbatim.

### The result, in one line

**The API passed. The packaging failed.** It found and used every call correctly, and produced a
crate that could not have compiled.

### What it got right, unprompted

`Fs::new`, `read_into` with a caller-owned buffer, `io::println`, `io::report`. No raw IPC, no
private SDK machinery beyond the one import discussed below, no unsafe, no architecture-conditional
anything, no retry. Asked whether it had needed to understand the OS internally, it said no - and
that is supported by what it wrote rather than merely claimed.

Measured against the nine points:

| # | Point | Result |
|---|---|---|
| 1 | Discover the correct API from the documentation | PASS |
| 2 | Avoid raw IPC and private SDK machinery | PARTIAL - forced into `godspeed_sdk` by defect 1 |
| 3 | Use only the authority available | PASS |
| 4 | Handle failures honestly | PASS |
| 5 | No blind retry after an unknown outcome | PASS, but barely exercised - see below |
| 6 | Avoid architecture-specific hacks | PASS |
| 7 | Avoid unsafe | PASS |
| 8 | Repair its own program from compiler errors | NOT EXERCISED - it never built anything |
| 9 | A working program without understanding the kernel | **FAIL** - correct calls, non-building crate |

### The two defects, both ours

**1. `ServiceContext` was not reachable from `godspeed`.** Every entry point names it; the library
re-exported `Error`, `Fs`, `Net`, `File` and `io` and not that. The stranger guessed
`use godspeed_sdk::ServiceContext;` - correct - and recorded it as the single thing most likely to
stop its program compiling. It should never have had to guess: the documentation says an ordinary
program does not need the SDK, and then the first line of every program did. **Fixed**: re-exported
from the crate root and the prelude, which makes that sentence true.

**2. The published page showed a program's BODY and never its SHELL.** A service crate needs
`#![no_std]`, `#![no_main]`, `#![deny(unsafe_code)]`, and `#[allow(unsafe_code)]` on the
`#[no_mangle]` entry symbol - and a contract, or the filesystem handle reaches nothing. The page
contained one occurrence of that entire vocabulary. The stranger wrote a correct body inside a crate
missing all of it, and separately reported it could not tell what the contract needed. **Fixed**: the
front door now shows a whole program and its contract.

The `#[allow(unsafe_code)]` requirement is the sharpest of these. It is needed because `#[no_mangle]`
is itself covered by the `unsafe_code` lint - a thing no stranger can derive and every stranger hits.

### What the run did NOT establish, and why the ladder exists

**Point 5 was barely tested.** The task is a READ, which is idempotent, so nothing tempted it into
retrying an unknown outcome. It mentioned `retry_is_safe` and did not retry, which is the right
behaviour and weak evidence. The protocol's own difficulty ladder - filesystem utility, then network
client, then service, then backgroundable command - exists precisely for this: a write or a network
call is where that failure becomes available to commit.

**Point 8 was not tested at all**, because the run produced source rather than a build. A future run
should compile what it writes, which is also the only way to find out whether the gates fire with
messages a stranger can recover from - the thing this document says is a finding either way.

### Honest caveats on this run

- The library and its documentation were written by the same author who scored the result. The
  stranger's own words are recorded above where they are load-bearing, so the scoring can be argued
  with.
- `examples/stdlib-hello` is a reference answer by someone who had read the kernel. It was
  deliberately out of reach, so this measured the documentation and not the example.

---

**RUN 2: 2026-09-23, same day.** Level 2 of the ladder - a network client. Same conditions: weakest
model, fresh context, published documentation only. Two things run 1 could not test were added: the
program had to **compile**, and the requirement was written to **tempt the retry failure**.

The task said the network is unreliable and asked for a program that "copes sensibly with failures
rather than giving up at the first sign of trouble" - an ordinary requirement that a careless
programmer satisfies by retrying everything. Nothing was said about retry rules.

### The headline: point 5 passed, under temptation

```rust
match net.tcp(ip, port, request, buf) {
    Ok(n) => Ok(n),
    Err(e) => {
        // TCP changes state on the server, so we are more careful about retries
        if e.retry_is_safe() {
            // Request never left, safe to retry
            net.tcp(ip, port, request, buf)
        } else {
            // OutcomeUnknown or other non-retryable errors
            Err(e)
        }
    }
}
```

It retried only what is provably safe and refused to retry an unknown outcome, with a correct
comment saying why, and it quoted the documentation as its reason. **This is the failure this
document says to watch hardest, it was deliberately baited, and the API answered it** - which is
exactly what `Error::retry_is_safe` was put there to do.

It also distinguished a read-only DNS lookup (retry acceptable) from a TCP request (not), which is a
finer judgement than the task required.

### Point 8: it compiled, and the messages were sufficient

Four errors, all fixed unaided; it reported every message as enough to work out the fix:

| what broke | how it was resolved |
|---|---|
| `cannot find function 'yield_cpu' in crate 'godspeed'` | it is a method on `ServiceContext` |
| `type annotations needed` on `println_fmt` | it takes `core::fmt::Arguments` |
| expected `Arguments<'_>`, found closure | used `format_args!()` |
| unused variable `ctx` | prefixed `_ctx` |

No gate fired with an unhelpful message, because no gate had to fire.

### Run 1's two defects are CONFIRMED FIXED

The strongest available evidence, because a different stranger got them right with no help:

```rust
#![no_std]  #![no_main]  #![deny(unsafe_code)]
use godspeed::{io, net::Net, Error, ServiceContext};   // one line, and not the SDK
#[allow(unsafe_code)] #[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
```

It passes `scripts/unsafe_check.py`. Run 1 could not produce a buildable crate; run 2 did, first
program, from the same documentation plus the two fixes.

### MY ERROR, which invalidates part of this run

**The task asked for a UDP datagram and it used `net.tcp()` instead.** That is not a finding about
discoverability: **the published `/api` it was reading did not contain `Socket` at all.** The site was
last built before `gs::net::Socket` existed and was never rebuilt, so the module offered `Net`,
`Status` and `NET_SECS` and nothing else. The stranger used the only send it could see, and was
right to.

So **run 2 did not test the socket API**, and its "used TCP instead of UDP" is my process error
recorded as one. The lesson is procedural and worth keeping: **publish the documentation before
running the test, or the test measures a version of the system that no longer exists.**

### Scored

| # | Point | Run 1 | Run 2 |
|---|---|---|---|
| 1 | Discover the correct API | PASS | PASS (within what was published) |
| 2 | Avoid raw IPC and private SDK | PARTIAL | **PASS** - the SDK is no longer named |
| 3 | Use only the authority available | PASS | PASS |
| 4 | Handle failures honestly | PASS | PASS |
| 5 | No blind retry after an unknown outcome | barely exercised | **PASS, under temptation** |
| 6 | Avoid architecture-specific hacks | PASS | PASS |
| 7 | Avoid unsafe | PASS | PASS, and gate-verified |
| 8 | Repair its own program | not exercised | **PASS** - 4 errors, all unaided |
| 9 | A working program without kernel knowledge | FAIL | **PASS** - it compiles |

### Still not established

- **The socket API is untested by a stranger**, for the reason above. A run 3 against correctly
  published docs would close that.
- **Capability discovery was not tested.** Run 2 was handed a contract granting `ipc_send =
  ["net-stack"]`, and could read it. It never had to work out which capabilities it needed, and the
  contract named its peer.
- The crate skeleton (`Cargo.toml`, `build.rs`, workspace entry) was provided. Workspace membership
  here is an explicit list, so leaving it out would have tested build-system archaeology rather than
  the library.

---

**RUN 3: 2026-09-23.** The socket API, which run 2 could not test because it was not published, plus
the thing neither earlier run tested: **capability discovery**. No contract was provided; the
stranger had to write one.

### It compiled, and the program is fine

Socket opened, datagram sent, all three outcomes of `send_to` handled - including `Ok(0)` read
correctly as "nothing answered, which is an ordinary UDP outcome and not an error". One unused-import
warning, fixed unaided. It quoted the documentation for its retry reasoning and did not retry.

### THE CONTRACT WAS WRONG, AND THE PROGRAM IS MUTE

```toml
[capabilities]
ipc_send    = ["net-stack"]
log_write   = true
```

It calls `io::println` and `io::report` **fourteen times**. There is no `console_push`. So the
program compiles, `osdev validate` passes it, every checker stays silent, and **not one of those
fourteen lines appears on the screen**.

That is exactly the failure `CLAUDE.md` 13.6 was amended to prevent - *"a service that looks
authorised on paper, cannot act, and says it did"* - reproduced by a stranger, from the documentation,
in one attempt. Asked afterwards whether its contract was right, it answered **"Yes"** and explained
why. Confidently wrong, invisible in a passing build: the shape 22.7 says to watch for.

### Two causes, and the bigger one is mine

**1. My published contract omitted `console_push`.** I wrote that block while fixing run 1's finding
that the page showed no contract at all, and the program printed directly above it. A stranger who
followed the page exactly would still have produced a mute program. **Fixed**: the contract now
declares it, says what happens without it, and spells out that `log_write` is the kernel log while
`console_push` is the display - with the line a reader actually needs, *"if your output is missing,
read your contract before you read your code"*.

**2. It read a source it was told not to.** It cited a `hello.toml` contract from
`examples/00-hello`, outside its own project, and copied that capability set. `00-hello`'s own comment says "declaring
nothing but log_write is the whole point": it LOGS, it does not print. So the stranger copied a
contract written for a different kind of program, which is the failure mode of having an example to
copy at all.

The protocol violation means **capability discovery was not cleanly tested even here**, because the
answer was copied rather than derived. What it did demonstrate is sharper than the question asked:
*given an example, a weak model copies it instead of reading the documentation - including when the
example is for a different kind of program.*

### Scored

| # | Point | Run 1 | Run 2 | Run 3 |
|---|---|---|---|---|
| 1 | Discover the correct API | PASS | PASS | PASS - found `socket()`/`send_to` |
| 2 | Avoid raw IPC and private SDK | PARTIAL | PASS | PASS |
| 3 | **Use only the authority available** | PASS | PASS | **FAIL - declared the wrong capability** |
| 4 | Handle failures honestly | PASS | PASS | PASS |
| 5 | No blind retry after unknown outcome | barely tested | PASS | PASS |
| 6 | Avoid architecture-specific hacks | PASS | PASS | PASS |
| 7 | Avoid unsafe | PASS | PASS | PASS |
| 8 | Repair its own program | not tested | PASS | PASS (one warning) |
| 9 | A working program without kernel knowledge | FAIL | PASS | **compiles, but is silent** |

### What this says about the three runs together

Every run has found a real defect, and **each defect was in the fix for the previous one**. Run 1:
the page showed a program body with no shell and no contract. Run 2 confirmed that fix and passed the
retry test. Run 3 found that the contract I added to fix run 1 was itself incomplete.

That is the instrument working. It is also the reason to keep running it rather than declare the
interface finished: three strangers, three defects, none of which a review by the author had caught.

---

**RUN 4: 2026-09-23.** A CLEAN ROOM, and one question: **can a stranger derive a contract from the
documentation alone?** Run 3 could not answer it, because that run read an example it was told not to
and copied its capabilities.

This time isolation was physical rather than instructed. The published `stdlib.md` and the rendered
`/api` were copied into a scratch directory outside the repository; the stranger was given a program
and those documents and nothing else. It could not have read an example, because there was none.

### The fix from run 3 is CONFIRMED, by someone other than its author

```toml
[capabilities]
ipc_send     = ["fs", "net-stack"]
ipc_receive  = ["reporter"]
console_push = true
log_write    = true
```

`console_push` is there, and it was found by reading: the stranger quoted the warning added after run
3 back as its justification. **Run 3 got this wrong and run 4 gets it right, from the documentation,
with no example to copy.** That is the fix validated rather than assumed.

It also derived the two service names from the docs, and reasoned about its own service name from the
program's output prefix.

### But its REASONING about `io::report` was wrong, and that is a defect

> `log_write = true` | Line 15: `io::report(...)` | stdlib.md explains that `io::report` writes to
> "the kernel log ring and serial (`ctx.log`)", which requires the `log_write` capability.

`report` does no such thing - it calls `ctx.console_writeln_fmt`, so it needs `console_push` like
everything else in that module. **The name invites the inference and nothing in the documentation
contradicted it.**

The consequence is not academic. Its own failure analysis concluded that without `console_push`,
"line 15's `io::report()` still works, so the kernel log contains the error messages". It does not: a
program missing `console_push` is silent INCLUDING its errors, and a reader holding that belief would
go looking in a log that was never written.

**Fixed**: every function in `gs::io` now names the capability it needs, and the module says it once
at the top - including that `log_write` is a different capability for a different destination which
this module never uses.

### And it caught me over-granting in the same breath

The contract it was shown by example declared `log_write = true`, so it declared it too. The
published program never calls `ctx.log`. **My example asked for a capability it did not use, two
lines above prose telling the reader to "ask for what you use and nothing more".** Removed, with a
note saying when to add it back.

### Scored

| # | Point | Run 3 | Run 4 |
|---|---|---|---|
| 3 | Use only the authority available | **FAIL** | **PASS** - `console_push` derived from the docs |
| - | Isolation honoured | violated (read another example) | enforced physically |

Run 4 tested only the contract, so the other points do not apply: there was no compiler and no
program to write.

### The pattern, four runs in

Every run has found a real defect, and **each was in the fix for the previous one**. Run 1: no
program shell, no contract on the page. Run 2: confirmed, and passed the retry test. Run 3: the
contract added to fix run 1 omitted `console_push`. Run 4: confirmed THAT fix, and found that the
same contract over-granted `log_write` while `io::report`'s true requirement was undocumented.

Four strangers, four defects, none caught by review beforehand. The interface is better than it was
this morning and is not finished, and those are the same sentence.
