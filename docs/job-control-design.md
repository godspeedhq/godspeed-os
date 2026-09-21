# Job control: `jobs`, `background`, `foreground` - work that outlives the prompt

**Status:** **DESIGN ONLY - nothing here is built.** Written before any code so the structural
decision below can be argued with rather than discovered. Trails `CLAUDE.md`; does not amend it.

**WHY THIS IS IN `docs/` AND NOT `utilities/`, because the enforcement layer taught it.** It was
written as a numbered spec under `utilities/` first, and `scripts/commandments.py` refused the
build:
Commandment X reconciles the user-facing vocabulary against the shell, and a spec in `utilities/`
asserts that the shell ANSWERS that verb. It does not. "A documented command that does not exist
costs the user more than a missing feature: they learn the documentation cannot be trusted."
`utilities/` is the spec for what exists; `docs/` is where a design lives until it does - beside
`naming-design.md`, `cluster-design.md` and `ipc-efficiency.md`, all of which were reasoning before
they were code. A numbered spec under `utilities/` follows the implementation, not the other way round.

**One file for three names, deliberately.** They are one mechanism with three surfaces: a table, a
way in, and a way back. Splitting them would put the state model in three files, and a state model
described in three places is the drift this repository keeps having to repair.

---

## 1. What it is

A long command should not own the prompt for its whole life, and the operator should not have to
decide that in advance.

```
gsh> background copy /usb/archive /data/archive
[backgrounded] job 7

gsh> jobs
JOB  STATE      COMMAND
7    running    copy /usb/archive /data/archive

gsh> foreground 7
Copying... 38%
[q] cancel   [b] background
```

Four surfaces, one idea:

| surface | does |
|---|---|
| `background <command>` | start it detached, print its job id, return to the prompt |
| `jobs` | the table: what exists, what state it is in |
| `foreground <id>` | attach the console to it again |
| **`b`**, inside a running job | detach it now, keeping it alive |

## 2. Why `background` exists at all, given `b` already would

Because making you start a ten-minute copy, wait, and then press `b` is **the interface forgetting
what you just told it.** That is the same argument that made PgUp open `scrollback` one page back
rather than at the live end (`54_scrollback.md` 2): when the operator has already said what they
want, asking again is not a safeguard.

`b` and `background` are the same verb at two moments, not two features. If they ever disagree about
what detaching means, one of them is wrong.

## 3. The words, against the house rules

**`background` and `foreground`, not `bg` and `fg`.** Rule 8: the vocabulary is not POSIX. There is
no fork, no exec, no signals and no process groups here, so borrowing the abbreviations of a model
this system does not implement would be borrowing the model's vocabulary without its mechanism -
exactly what 26.14 warns against one layer down.

**`q` stops, `b` detaches.** Rule 11 is already normative and settles the pair: quitting stops the
TASK, not just the shell's view of it. So `q` must kill the job and `b` must not, and no third key
is needed to say "leave it running".

**`jobs` is a producer, so filtering is a pipe.** Rule 12 makes a utility's output a pipeable
structure. `jobs | where state=running`, not `jobs running` - the second is a bespoke positional
filter for one table, when `status | where name contains recorder` is already the idiom everywhere
else. One way to filter, not two.

## 4. THE STRUCTURAL DECISION: a background job is a SPAWNED SERVICE

This is the part to disagree with now rather than after it is written.

**This shell has no threads.** Nothing in this system does - a task is a service, and the shell's
main loop blocks reading keys. So a detached job makes progress in exactly one of two ways:

| | how | cost |
|---|---|---|
| **A. spawn a service** | the job is a task; the shell brokers it the caps it needs and stops thinking about it | every backgroundable command must exist as a service |
| **B. slice it in the shell's loop** | a resumable state machine per job, advanced a chunk between keystrokes | every backgroundable command must be rewritten as a state machine |

**A is the recommendation, and the reason is that it is not a new mechanism.** Appendix B.3 says the
shell IS a capability broker that spawns services and grants each the caps it should have; D.3 says
composition is capability-mediated; D.4 observes that `rm -rf /` cannot exist unless the shell hands
`rm` a WRITE cap to `/`. A background job is the clearest case of that whole argument:

```
background copy /usb/archive /data/archive
```

mints a READ cap for one path and a WRITE cap for the other, hands over exactly those, and the job
can reach nothing else for its whole life - not because it is well-behaved, but because it holds
nothing else (3.1). B gives the job the shell's own authority instead, which is every cap the shell
has.

Three further reasons A wins:

1. **Rule 11 is free.** `q` must stop the TASK. For a service that is `ctx.kill(name)` - the
   mechanism the supervisor already uses. For a state machine it is bookkeeping that has to be
   correct in every arm.
2. **The shell's stack does not grow.** `cmd_edit` is already the deepest frame at 61 KiB of the
   256 KiB user stack (`stack_fit_check.py`). N concurrent state machines live in that same budget;
   N services do not.
3. **A job that faults is a service that faults.** The supervisor already reports it, the kernel
   already reclaims its frames, and `status` already lists it. B makes a faulting job a faulting
   SHELL, which is the one service whose death takes the session with it.

**What A costs, stated plainly:** `copy` is a shell built-in today, and so is nearly everything worth
backgrounding. A is therefore gated on a `copy` service existing - which is not free, but is work
this system's own design already points at, and `copy` already streams in `IO_CHUNK` pieces rather
than buffering, so the shape is close.

**First implementation should back exactly one command.** Not "backgroundable commands" as a
category - one, chosen because somebody wanted it (26.2). `copy` is the obvious candidate: long,
bounded, streams already, and produces a progress count rather than a screen.

## 5. Where the output goes - the part with no obvious answer

A foreground job writes to the console. A detached one has nowhere to write, and this is the
question that makes background jobs unpleasant on other systems: output arriving on top of a prompt
somebody is typing at.

Three options, and the choice is not made here:

1. **Discard it.** Honest and bounded, and wrong for `copy`, whose whole output is "how far".
2. **Buffer it, bounded.** A fixed per-job ring, so `foreground` can replay what was missed.
   26.6.1 forbids growth, so this is a fixed arena with `older lines aged out` - the language
   `scrollback` already has for exactly this, and the ring shape it already uses.
3. **`save` it.** `save <path>` is already the house word for "keep this output" in three utilities
   (`selfcheck save`, `run ... save`, `chaos kill-storm ... save`). `background copy ... save
   /copy.log` composes rather than inventing anything.

**A background job must never write to the console unasked.** Whatever is chosen, the prompt belongs
to whoever is typing at it. 2 and 3 compose; 1 is the fallback when neither is asked for.

## 6. The job table is BOUNDED, and says so

A fixed number of slots - 8 is a guess, and the right number is whatever a real session needs.
`background` on a full table refuses and says so; it does not grow, queue, or evict (26.6). A job
that has finished keeps its row until it is read or a new job needs the slot, so a job that failed
while nobody was looking is still reportable - the same reason `LastWriteErr` keeps its text.

States: `running`, `done`, `failed`, `stopped`. Four words, no abbreviations, and `failed` carries
the reason when asked for.

## 7. What is NOT in this spec

- **No `wait`.** Nothing has needed one. It is one line to add when something does.
- **No job control over services the shell did not start.** `kill` and `restart` already address
  those by name, and giving `jobs` two kinds of row would make the table mean two things.
- **No `&` suffix.** `background <cmd>` is the word; a sigil would be a second way to say it and
  POSIX vocabulary besides (rule 8).
- **No nesting.** `background background x` is refused, not defined.

## 8. Testing it

The shell suite can drive all of this over the serial console, with one caveat worth writing down
before somebody is surprised by it: **a detached job's output is exactly what a test harness cannot
see by waiting for a prompt**, because the prompt comes back immediately. Assertions go on `jobs`
rows and on the job's effect (the copied file), never on output arriving at a particular moment -
which is the trap `54_scrollback.md` 6 records, in a different disguise.
