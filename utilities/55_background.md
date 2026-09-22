# Utility: `background`, `jobs`, `foreground` - work that outlives the prompt

**Status:** **Built + QEMU-verified** (`osdev test jobs`, 36/0, QEMU with a real disk). Shell
built-ins backed by the `copier` service. Trails `CLAUDE.md`; does not amend it. The design reasoning, including
the option that was rejected, is `docs/job-control-design.md`.

**One file for three names, deliberately.** They are one mechanism with three surfaces: a way in, a
table, and a way back. Splitting them would put the state model in three files, and a state model
described in three places is the drift this repository keeps having to repair. Precedent:
`30_first-last.md`, `31_records.md`.

---

## 1. What it is

A long command should not own the prompt for its whole life.

```
gsh> background copy /big.bin /backup/big.bin
[backgrounded] job 1

gsh> jobs
JOB  STATE    PROGRESS  COMMAND
1    running   38%      copy /big.bin /backup/big.bin

gsh> foreground 1
[q] cancel   [b] background
copying... 41%
job 1 done
```

## 2. Invocation

| Command | Meaning |
|---|---|
| `background copy <src> <dst>` | start the copy detached; print its job id; return to the prompt |
| `background delete <path> recursive` | remove a whole subtree detached |
| `background drives check` | check the volume detached; `foreground` replays the verdict |
| `background drives scrub` | read-only CRC sweep, detached |
| `background churn <seconds>` | sustained write traffic, detached - the prompt stays yours |
| `jobs` | the table: id, state, progress, command |
| `jobs quit <job>` | stop a job without attaching to it first |
| `foreground <job>` | attach the console to a running job, or report a finished one |
| `q`, while attached | **stop the job**, not just the view (conventions rule 11) |
| `b`, while attached | detach it again, leaving it running |

`jobs quit` exists for the reason `background` does when `b` already would (§2 of the design note):
making somebody attach to a job in order to stop it is the interface forgetting what they just told
it. It is **not** `kill copier` - killing the service skips its cleanup, so a half-written
destination survives as a full-size file with an undefined tail. It routes through the job's own
cancel, which removes it. A job is not the service that happens to be running it.

## 3. Two commands run detached, and the test is not "is it slow"

`background` is **not a modifier that can be put in front of anything.** The question a candidate
has to pass is:

> **is this command's value its EFFECT, or its OUTPUT?**

A detached job holds no `console_push` capability (§4), so a command whose whole product is a report
has nowhere to put it. That is a fact about what the job can reach, not a policy someone chose, and
it sorts the candidates cleanly:

| command | detached? | why |
|---|---|---|
| `copy <src> <dst>` | **yes** | the effect is the file; progress is a count, not a screen |
| `delete <path> recursive` | **yes** | `fs` does the whole walk in ONE operation, so the job is one request and a wait |
| `drives check` | **yes** | one `fs` request, and its report lives in the transcript (§4a) until asked for |
| `drives scrub` | **yes** | the same shape as `check` - one request, one verdict - which is what made it nearly free |
| `churn <seconds>` | **yes** | the effect is thousands of transactions on disk. It is also the command somebody most wants the prompt back during: it holds the console for its whole run, so a ten-minute churn is ten minutes of a blind machine |
| `selfcheck`, `chaos`, `run` | no | **not an output problem.** These drive other shell built-ins through the shell's own dispatch; a service cannot call into it, and there is one console input ring with one reader. Detaching them would mean reimplementing the shell inside the job |
| `find` | not yet | the shell walks directories itself, so the service would need that walk. The transcript already solves its other half |
| `copy <src> <dst> recursive` | no | an interrupted WALK leaves a prefix of a tree - a different permitted-outcome question, and one nothing here answers |
| `delete <path>` (no `recursive`) | no | one metadata edit; it would be over before `jobs` could list it |

**Two different blockers, and they are worth keeping apart.** "Where does the output go" was solved
by the transcript below and is what let `drives check` in. "Who can run the command" is untouched by
it, and is why `selfcheck` stays out however good the buffer gets.

Refused by name, leading with the reason, and naming what to type instead:

```
gsh> background selfcheck
background: `selfcheck` not supported for it runs inside the shell - a job runs as a detachable separate service.
  detachable services: copy <src> <dst>, delete <path> recursive, drives check, drives scrub
```

**That list is built from the table the dispatch reads**, not written into the sentence. An earlier
version was a hand-kept string with a comment asking the next person to update it - the same drift
`facts_check.py` exists to catch elsewhere - so adding a kind now updates the message on its own,
and `osdev test jobs` asserts the advertised list names every verb that works.

Refusing is the honest answer rather than the incomplete one (§26.2: the preferred state of an
unneeded feature is "not implemented; will be implemented when a test requires it"). `background
chaos` in particular would be a storm nobody can watch or stop, which is worse than no answer.

**CHURN HAS A REAL PERCENTAGE, and that sharpens the rule.** The column shows a number when
something MEASURES it, not when the job happens to be a copy: churn's bound is a duration, so
elapsed-over-total is measured. A finished churn reads `100%`, because the clock stops at the full
duration rather than at the last second sampled before the deadline - a completed run sitting at
`91%` reads as one that stopped short.

**Only the duration form detaches.** `churn verify`, `churn tear` and `churn reset` are one-shot and
stay at the prompt.

**THE TEAR PATTERN LIVES IN `sdk::churn`, NOT IN EITHER CALLER.** Churn's whole purpose is that a
torn file is detectable: every byte encodes the generation that wrote it (`byte[k] = (gen + k) mod
251`), so a file holding a mix of two writes breaks the relation at the exact byte where the tear
happened. Detaching churn put the WRITER in `services/copier` and left the CHECKER in the shell, in
different crates that deliberately do not share headers - and if those two expressions ever
disagreed, `churn verify` would report `NONE torn` while no longer able to recognise a tear at all.
A safety check that passes because it broke is worse than no check, because somebody trusts it. So
the pattern moved to the SDK, where there is one of it and both sides name the same function.

**A recursive delete shows no percentage, and does not invent one.** `fs` owns the walk, so nothing
here can say how far it has got. The column shows `-`; `0%` would read as stuck and `100%` as
finished, and both would be fabrications.

```
JOB  STATE    PROGRESS  COMMAND
1    done     100%      copy /fill3.bin /copy.bin
2    stopped    0%      copy /fill3.bin /cancelme.bin
3    done        -      delete /tree recursive
```

**It also cannot be cancelled once started, and says so rather than pretending.** The tree delete is
one blocking `fs` request, so while it runs the service is not reading its endpoint; a cancel waits
behind it and arrives after the job has ended. There is no half-measure available, because `fs` owns
the walk.

## 4a. The transcript: where a detached job's output lives

A job holds no console. Its output goes into a **fixed 4 KiB ring inside the job service**, and
`foreground` pulls it out when somebody asks. Nothing is ever pushed, so the property that makes the
whole design safe is untouched: a job still cannot write over a prompt somebody is typing at,
because it still holds no capability that reaches the console.

```
gsh> background drives check
[backgrounded] job 4

gsh> jobs
JOB  STATE    PROGRESS  COMMAND
4    done        -      drives check

gsh> foreground 4
job 4 is done
  drives check - walking the volume
  ok - 0 bad, 7 file(s), 2 director(ies) scanned
```

**The verdict is RENDERED here, not passed through.** `fs` answers a check or a scrub with COUNTS -
`[files:u32, dirs:u32, bad:u32, ..]` - and the first version of this wrote that body straight into
the ring on the principle that the filesystem owns what its verdict says. That principle is right
about the FACTS and wrong about their presentation: raw little-endian integers printed as characters
put `            ,` on the screen. It survived a 48-check suite because the check path happened to
return an empty body on the test volume and a fallback sentence printed instead; the scrub returned
real numbers and showed the bug. The assertions were widened to require the rendered words, since an
assertion that cannot tell prose from binary is not testing the replay.

**It is bounded and it says when it dropped.** 4 KiB of fixed storage, no heap, no growth
(§26.6.1). A job that says more ages out its oldest LINES - not bytes, so a replay never begins
mid-word - and the replay opens with the count:

```
  ... 612 earlier byte(s) dropped - the transcript is 4 KiB and this job said more
```

A bounded buffer that silently truncates is worse than no buffer: it hands somebody a partial
`drives check` that reads as a complete one, which is precisely the silent failure invariant 12
forbids.

**There is no `jobs output <id>` verb.** `foreground <id>` already means "attach the console to this
job"; replaying what it said IS attaching. A second word for it would be a second way to say
`foreground` (§26.2).

**Only the newest job's transcript exists.** The service runs one job at a time and clears the ring
when the next starts. `foreground` on an older finished job reports its outcome and stays silent
rather than replaying a different job's text under the right job's heading - a lie that would read
perfectly.

## 4. What a job can reach

The `copier` service holds `fs` and its log. **It has no `console_push`**, so a detached job cannot
write over a prompt somebody is typing at - not by convention but because it holds no capability
that reaches the console (§3.1). It cannot spawn, reboot, or reach the network.

**The honest limit of that claim.** The design note describes minting a READ capability for the
source and a WRITE capability for the destination and handing over exactly those, so a job could
reach nothing else for its whole life. That is not what this does: the shell's `spawn` takes a name
and nothing else, and per-invocation capability delegation is listed as future work in
`10_spawn.md` §5. So the bound is the **contract's** (`fs`, entire) rather than the two paths'. It
is a real reduction from the alternative - a job running inside the shell's loop would hold the
SHELL's authority, which includes `spawn` and `reboot` - and it is not the bound the design claimed.
Recorded here rather than left to be discovered (§26.7).

## 5. States

Four words from the service, and one the shell adds:

| state | means |
|---|---|
| `running` | in progress |
| `done` | the whole file was copied |
| `failed` | it stopped on an error; the reason is printed with the row |
| `stopped` | `q` cancelled it |
| `lost` | **the shell's own verdict**: `copier` is gone, so the job did not finish and how far it got is unknown |

`lost` is a separate word from `failed` on purpose. `failed` claims knowledge of a failure that was
observed; when the service dies mid-job nobody observed anything, and saying `failed` would assert
more than is known. It is also not left as `running`, which would be a row that never changes again.

**ALIVE-AND-SILENT IS NOT GONE**, and conflating them was a real bug caught by adding the second job
kind. The job service is single-threaded, so while it is inside one long `fs` request - which a
recursive delete is, entirely - it reads no messages and answers no status. Treating "did not reply"
as "is not running" would mark a job `lost` at precisely the moment it is doing its work. There are
three cases: an answer, a service that is busy (the row keeps its last true figures), and a service
that is actually gone (`lost`).

## 6. An interrupted copy DELETES its destination

`fs` allocates a file's whole extent up front, so an interrupted copy would otherwise leave a file
**of the right size whose tail is undefined content**. That is worse than no file: `dir` shows the
expected byte count, `read` returns something, and nothing says the tail is garbage. So a cancelled
or failed job removes what it made, and says that it did. If the removal itself fails, that is
reported too - a failed recovery is still a failure (§26.7).

## 7. Bounds

- **Eight rows**, fixed. A ninth `background` is refused with the reason; the table does not grow,
  queue, or evict a row nobody has read (§26.6).
- **One job at a time.** A second `background copy` while one runs is refused, naming the job that
  is still going. A queue is unbounded growth wearing a small word.
- **Finished rows are kept** so a job that ended while nobody was looking is still reportable.

## 8. `jobs` is a producer

One row per line, so it filters like everything else:

```
jobs | where state=running
```

There is no `jobs running` subcommand. Rule 12 makes a utility's output a pipeable structure, and a
bespoke positional filter for one table would be a second way to say what `where` already says.

**`jobs quit <id>` is a subcommand and that is not a contradiction.** The rule is about FILTERING -
selecting rows is `where`'s job and must not be reinvented per table. `quit` does not select
anything; it acts on one job. The house already splits these the same way: `drives` is a producer
and `drives check` acts, `events` is a producer and `events persist start` acts.

## 9. Non-goals

- **No `wait`.** Nothing has needed one.
- **No `&` suffix.** `background <cmd>` is the word; a sigil would be a second way to say it, and
  POSIX vocabulary besides (rule 8). There is no fork, no exec and no process groups here, so
  borrowing `bg`/`fg` would borrow a model this system does not implement.
- **No nesting.** `background background x` is refused, not defined.
- **No job control over services the shell did not start.** `kill` and `restart` address those by
  name; two kinds of row would make the table mean two things.
- **No detached subtree copy.** `background copy <dir> <dir>` is refused: an interrupted walk leaves
  a prefix of a tree, which is a different permitted-outcome question and one nothing here answers.
- **No detached `run` or `selfcheck`.** Not an output problem: a `.gsh` script drives shell built-ins
  through the shell's own dispatch, and no buffer helps with that.
- **Applications do not need this.** An application IS a service, and `spawn <name>` already returns
  immediately while it runs - `status`, `kill` and `restart` manage it. `background` exists only to
  give shell BUILT-INS, which have no service of their own, somewhere to run. The real gap is that
  `spawn` takes a name and nothing else, so an application cannot be given arguments or
  invocation-scoped capabilities (`10_spawn.md` §5).
  Note that a detached recursive DELETE is fine for the opposite reason - `fs` performs that walk
  itself, in one operation, so this feature never holds a half-finished tree.
- **No cancelling a running recursive delete.** It is one blocking `fs` request; a cancel arrives
  after it has ended. Refusing to pretend is the whole of §3's last paragraph.
- **No second job while one runs.** Refused, naming the job that is still going, rather than queued.
