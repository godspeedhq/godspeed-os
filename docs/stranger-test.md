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
