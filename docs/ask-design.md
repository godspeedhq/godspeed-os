# ask - Assisted System Knowledge (design, not built)

> **Status: DESIGN, proposed by the operator on 2026-10-10 and adapted here to this system. Nothing is
> built.** The shell does not answer `ask`, so its surface lives here and moves to `utilities/` the day
> it does (`utilities/0_conventions.md` section 2a: a spec there asserts the shell answers the verb).

**Know what your system knows.**

`ask` answers ordinary questions about the machine - is it healthy, what failed, why did the WiFi
restart, is the filesystem all right - by asking the services that already know, and saying plainly
which part of the answer is a recorded fact, which is an inference, and which nobody can tell. It is
the first command that EXPLAINS rather than reports, and its one rule is the project's rule (CLAUDE.md
26.7): **an honest "unknown" beats a convincing answer the evidence does not support.**

It is not a chatbot, an administrator, a monitor or a repair agent. It runs no model, needs no network,
keeps nothing running between questions, and changes nothing.

## 1. Shape: a short-lived program, not a shell built-in

`ask` is a program the shell starts for one question and that exits when it has answered, as
`copier` and `recorder` are started on demand (`services/CLAUDE.md`). Two reasons decide it, and both
are about authority rather than convenience:

- **It holds only what it reads.** A shell built-in would answer with the shell's whole authority,
  which includes killing services and formatting disks. A separate program is granted, in its spawn
  row, exactly the read-only reach it needs: its log, `INTROSPECT` (read), and send capabilities to the
  services it asks. Nothing it is told can widen that - a question is data, not a grant (CLAUDE.md 3.1).
- **It costs nothing while nobody is asking.** No resident service, no background thread, no memory
  between invocations. Each question is independent of the last.

The shell passes the question and relays the answer; `ask` prints through `gs::io` and, when its
output is piped, emits records (rule 12). Written on `gs` throughout, so `scripts/one_way_check.py`
holds it at zero raw SDK calls from its first commit.

## 2. The command

```
ask "<question>"            the answer
ask evidence "<question>"   the same assessment, with every observation behind it
ask help                    what it can be asked, with one real example per question kind
ask version
```

A bare `ask` prints its usage (rule 1). `ask evidence` runs the SAME evaluation as `ask` and changes
only how much of it is shown: within one invocation the summary and the evidence come from one
assessment, never two.

**What a question returns, for a script.** An answered question is a success whatever the answer is -
"the filesystem is damaged" is a successful answer. A question `ask` does not understand, a source that
did not answer, and a refused capability are failures, and `result` says which (there are no numeric
exit codes in `gsh`; `utilities/32_result.md`). A script that wants the health verdict pipes the
records - `ask "is my system healthy?" | where kind=verdict` - rather than reading success as health.

## 3. The first four questions (v0.1)

Each is answered only from sources that exist today and that answer a defined request. Where a source
does not exist, the answer says so; section 6 lists what each question would need to say more.

### 3.1 Is the system healthy?

Sources: the task table through `task_stat` (`INTROSPECT`) - which managed services are live, on which
core, their restart counts; the supervisor's device record (`supcmd::DEVICES`) - which device each driver
holds; `fs`'s drive report (`OP_DRIVES_INFO`) - mounted or not, read-only or not; and each radio's
status (`godspeed_wifi::wire::OP_STATUS`).

Verdicts: **HEALTHY** (every check that ran passed and every required check ran), **DEGRADED** (working,
with something impaired - a restarted service now live, a read-only mount), **UNHEALTHY** (a required
service is not running, storage is unavailable), **UNKNOWN** (too little answered to say). A machine
with no recorded error is not therefore healthy, and a source that did not answer is not therefore
failed: it is a hole in the coverage, and the answer names the hole.

### 3.2 What failed?

Sources: the restart count `task_stat` carries for every watched service, and the supervisor's bounded
record of driver deaths, restarts and USB arrivals (`supcmd::EVENTS`, the newest 32).

It keeps apart what those sources keep apart: a death the supervisor restarted, a service the operator
stopped, a restart that did not take, and a service live now. **A restart is not proof of a crash** - a
`kill` from the shell restarts too - and where the record does not say which it was, the answer says
it does not know.

### 3.3 Why did the WiFi restart?

Sources: the radio's status, including its down reason (`DOWN_TRAPPED`, `DOWN_BRINGUP`,
`DOWN_NO_RADIO`, `DOWN_NOT_BUILT`); the supervisor's record of the radio driver's deaths and restarts;
the restart count.

It must not treat as one thing what are five: the driver restarting, the radio being power-cycled, the
firmware failing, an association or handshake failing, and the link simply dropping. A cause is named
only when a source records it; a firmware fault seen before a restart is reported as **what came
before**, not as **why**, unless the source says it caused it.

```
gsh> ask "why did wifi restart?"
wifi-driver restarted after its previous instance died - the supervisor recorded the restart.
Why it died is not recorded anywhere this can read.
```

### 3.4 Is the filesystem healthy?

Sources: `fs`'s drive report - mounted, read-only, the durability case of the medium - and, when asked
for it explicitly, a read-only scrub (`drives scrub`'s operation), which reads every block's CRC.

It separates **now** from **before**. A journal replayed at the last mount is history, not a fault
today - but `fs` does not report that it replayed one through any request, so v0.1 cannot see it, and
says so (section 6). A scrub is offered rather than run, because it reads the whole volume.

## 4. Understanding the question

Deterministic, versioned and small. No model.

```
question -> normalise -> look up words -> choose a question kind -> take its target and time -> ask the sources
```

- **Normalise:** case, punctuation, repeated spaces, a few spelling variants (`wifi`, `wi-fi`,
  `wireless` name one subsystem).
- **Words that are NOT the same are never merged:** restart, reboot, reset, disconnect and crash each
  keep their own meaning, because the answer depends on which one happened.
- **Ambiguous** (two question kinds fit): the answer lists both and asks which. **Not understood:** it
  says so and lists what it can answer. It never picks a question kind just to have an answer.

```
gsh> ask "why is my computer sad?"
ask: I don't understand that question yet. I can answer questions about:
  system health, what failed, the WiFi restarting, the filesystem
```

The vocabulary is read-only data compiled into the program, versioned apart from the diagnostics, and
pinned by regression tests: a question that was understood stays understood unless its first reading
was wrong. A request to DO something - "restart the wifi driver" - is answered with what `ask` is:

```
ask: I answer questions about the system; I cannot restart anything. `restart wifi-driver` does that.
```

## 5. Truthfulness

Every line of an answer is one of three, and evidence mode labels it:

- **FACT** - a source said it, or recorded it.
- **INFERENCE** - concluded from facts by a written rule (the rule is named in evidence mode).
- **UNKNOWN** - not established by anything available.

Never: an invented event or timestamp; correlation offered as cause; success assumed from the absence
of an error; missing data read as healthy; complete coverage claimed when a record is bounded (the
supervisor's keeps 32 and starts empty on its own restart) or a source did not answer; a contradiction
between two sources hidden. Two invocations are two observations: each answer carries the time it was
taken, and nothing implies two answers saw the same moment.

What sources return is DATA. A log line or a device name that reads like an instruction is printed as
text, never acted on.

## 6. What v0.1 would need first - recorded here, not opened as backlog items

`ask` may use only defined requests (no parsing a service's log lines as if they were a contract, no
private structures, no new syscall). Measured against that, three answers are thinner than they should
be, and each gap is a missing **contract**, closed in the service that owns the fact - never in the
kernel for `ask`'s sake:

1. **Why a service died.** The supervisor records THAT a driver died and was restarted, not why. The
   kernel knows how a task ended (a fault, a kill, a panic in the service); a death record that carried
   it is what would let 3.2 and 3.3 name a cause.
2. **That a journal was replayed.** `fs` logs it at mount and reports it through no request. A field in
   the drive report saying "recovered N blocks at the last mount" makes 3.4's history a fact.
3. **The network's health.** `net-stack` answers status (address, lease, link), not "healthy"; a
   health verdict built from those is an inference, and evidence mode would have to show the rule.

The boot record (`boot_record_read`) holds all of these as TEXT. Reading it would make the answers
fuller and would make every log line a contract - which is exactly the coupling this design refuses.

## 7. Bounds (the Pi 2 is the target)

No heap-heavy runtime, nothing resident. Targets, to be measured on the Pi 2 before they are claimed:
a question of at most 512 bytes, at most 256 records read per source, at most 16 KiB of output, at most
4 MiB of memory, about a second for an ordinary question. Every request is a bounded `gs::call`, so a
source that is dead or wedged ends one line of the answer ("fs did not answer") rather than the
command. A bound that cuts an answer short says the answer is partial.

## 8. Testing

- **Language:** the vocabulary's questions, its synonyms, case and punctuation, the ambiguous and the
  not-understood, and every earlier version's questions.
- **Evidence:** fixtures of source replies - healthy; degraded; a required service down; a source that
  does not answer; a crash and a restart; a deliberate kill and restart; a firmware fault then a
  restart; a link drop with no driver restart; a full event record; two sources that disagree.
- **Truthfulness:** for each fixture, what the answer must say AND what it must not ("the firmware
  caused it" when only a restart was recorded).
- **Carnage:** `chaos max-carnage` while `ask` runs, killing what it asks, the supervisor included. It
  ends with an answer or with "could not tell" - never a hang, never an invented line.

## 9. Not in v0.1

Language models, voice, memory between questions, repairs, running commands from a question, the
network, a resident daemon, learning words from what users type, any widening of authority, and
coverage of every subsystem. Later versions may add question kinds, time windows ("since the last
boot"), correlation across sources, and versioned descriptions a service publishes of the checks it
answers - which may describe a check and must never grant anything.

## 10. The rules

1. A program for one question, not a service.
2. Defined requests only; a gap is closed in the owner, never the kernel.
3. Read only.
4. No invented evidence.
5. No correlation offered as cause.
6. No authority it was not granted.
7. The vocabulary grows apart from the diagnostics.
8. It runs on the Pi 2.
9. It is useful with no model at all.
10. "Unknown" over a convincing answer the evidence does not support.
