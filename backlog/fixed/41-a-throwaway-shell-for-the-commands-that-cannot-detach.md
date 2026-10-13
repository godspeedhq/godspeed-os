# 41. A throwaway shell, so `selfcheck` and `run` could detach too

**Status: CLOSED 2026-09-22 - REJECTED on a security objection, not deferred and not blocked.** Proposed and analysed the
same day, then scrapped by the operator for the reason in "The objection that stopped it" below.
This entry exists so nobody re-attempts it: the mechanics are easy and attractive, and the reason
not to is not visible from the mechanics.

## The idea

`background selfcheck` and `background run <script>` are refused because a `.gsh` script drives shell
built-ins through the shell's own dispatch, and the job service cannot call into it. The proposal:
let `background` spawn a SECOND, throwaway shell instance to run them, with its output captured in
bounded RAM and replayed by `foreground` - the same shape as the job transcript.

## It would work, and here is what it costs

1. **A second spawn-table row.** The kernel refuses a duplicate live name (`task/mod.rs` singleton
   guard), so `spawn shell` twice is out. A row like `("jobshell", SHELL_ELF, ..)` pointing at the
   same image is mechanically cheap.
2. **A headless mode.** The throwaway must never take the console. There is ONE input ring with ONE
   reader slot (`docs/console-service.md` §5a), so a second shell reading keys would fight the real
   one for the operator's typing. It needs a mode that reads no keys and writes no console.
3. **A way to be told what to run.** `spawn` takes a name and nothing else, so the throwaway cannot
   be launched with a command. It would have to idle on its endpoint and be sent one - the
   `recorder`/`copier` pattern, but new code in the shell's main loop.
4. **Output.** Either the existing `save` path (write to a file, `foreground` reads it) or the job
   transcript. The shell can already redirect its own output - `Out::File` and `Out::Capture` exist
   for `run ... save` and pipes - so this is the cheapest part.

None of that is hard. Perhaps two days.

## The objection that stopped it: a CONFUSED DEPUTY holding every cap the shell has

**A throwaway shell must hold the shell's authority, because "run any command" means "hold any
capability".** That much was the first objection, and it is the one `docs/job-control-design.md` §4
already rejected option B for:

> B gives the job the shell's own authority instead, which is every cap the shell has.

**The sharper form, and the one that ended it:** such a service's whole purpose is to execute
instructions that arrive on its endpoint. So its authority becomes exercisable by whoever can SEND
to it. That is the confused-deputy shape exactly - a deputy acting on instructions without knowing
whether the instructor held the authority for what it is being asked to do.

Today the shell's authority is exercised by **whoever is at the console**. A throwaway shell makes
the same authority exercisable **over IPC**, which is a different and much wider door.

**Stated precisely, without overstating it.** `AcquireSendCap` is gated (§22 Test A13 pins that a
service without the capability is refused), so this is not "anything on the machine could drive it".
But the GUARANTEE changes shape, and that is the problem:

| | what protects it |
|---|---|
| today's job service | it holds `fs` and nothing else. A property of the SERVICE |
| a throwaway shell | it holds everything, and is safe because of who can currently reach it. A property of the whole GRANT TABLE |

The first is true no matter what else changes. The second is one grant away from being false, and
nothing would fail when it became false. `chaos` already holds `ACQUIRE_ANY`.

And it is the failure the capability model exists to prevent: authority flowing from IDENTITY - "the
shell is allowed to do this" - rather than from possession. That is invariant 3, and a service that
acts on any instruction it receives launders exactly that distinction away.

It would also make `background X` mean two different things depending on X: a bounded service
holding one capability, or a second shell that can do anything. The refusal message says "a job runs
as a detachable separate service", which is true and useful today; it would become "it depends".

## What would have to be true first - and it is not a small list

- **Invocation-scoped `spawn` delegation** (`utilities/10_spawn.md` §5, the same gap that narrowed
  the capability claim on jobs). A throwaway spawned holding only what its ONE command needs is not
  a second full shell, and the deputy argument weakens accordingly - it can only be misused for what
  it was granted.
- **AND a reason the instruction channel is safe**, which delegation alone does not give. Even a
  narrowed deputy acts on whatever arrives at its endpoint; something must establish that the sender
  was entitled to that particular command. This project has no mechanism for "who asked" - by
  design, because authority is possession, not identity. So the honest version is: the caller would
  have to PASS the capability with the request rather than name a command, at which point it is not
  a shell any more, it is the job service that already exists.
- **A real need.** Nobody has wanted `background selfcheck` for its own sake - it came up while
  testing job control, which is a feature invented by its own test (26.2).

That third point is the one to weigh first if this ever comes back. The first two are real work; the
third is the reason the work has not been worth starting.

## The cheaper 80% that already exists

`run <script> save <file>` already captures a script's output to a file. What it cannot do is
detach. For a long script the gap is real but it is smaller than it looks, and `q` already aborts.
