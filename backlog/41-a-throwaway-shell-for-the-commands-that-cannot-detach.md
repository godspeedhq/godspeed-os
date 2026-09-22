# 41. A throwaway shell, so `selfcheck` and `run` could detach too

**Status: OPEN BY DECISION - feasible, analysed, NOT built (26.2).** Proposed 2026-09-22. Nothing is
waiting on it; this records the design and the one objection that stopped it, so the next person
argues with the reasoning rather than rediscovering it.

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

## The objection that stopped it

**A throwaway shell must hold the shell's authority, because "run any command" means "hold any
capability".** That is exactly what `docs/job-control-design.md` §4 rejected option B for:

> B gives the job the shell's own authority instead, which is every cap the shell has.

Today a job holds `fs` and its log: no console, no spawn, no reboot, no network. That is the
property that makes detaching safe, and it is what the spec claims in §4. A throwaway shell would
hand a background job `spawn`, `service_control`, `reboot` and the rest - not through an oversight
but by necessity.

Worse, it would make `background X` mean two different things depending on X: a bounded service
holding one capability, or a second shell that can do anything. The refusal message says "a job runs
as a detachable separate service", which is true and useful today; it would become "it depends".

## What would change the answer

- **A real need.** Nobody has wanted `background selfcheck` for its own sake - it came up while
  testing job control, which is the definition of a feature invented by its own test (26.2).
- **Narrowable authority.** If `spawn` could delegate invocation-scoped capabilities
  (`utilities/10_spawn.md` §5 - the same gap that narrowed the capability claim on jobs), a
  throwaway shell could be spawned holding only what its one command needs. Then the objection
  above dissolves, because the second shell would no longer be a second FULL shell. That is the
  order to do these in: delegation first, this second.

## The cheaper 80% that already exists

`run <script> save <file>` already captures a script's output to a file. What it cannot do is
detach. For a long script the gap is real but it is smaller than it looks, and `q` already aborts.
