# ask - Assisted System Knowledge (design; v0.1 built)

> **Status: v0.1 BUILT on `feat/ask`, 2026-10-11, QEMU-verified only.** Since then: how a service ended
> (the kernel's death notice, 6), the storage chain below `fs` when it has no disk (3.4), "is the network
> up?" (3.5), "what is <service>?" from each crate's own `description`, "what can <service> do?" / "who can
> reboot?" from the capabilities the kernel holds for each task, "what to say?", glossary terms and the
> Easter eggs (3.6), and curly quotes accepted as quotes. Proposed by the operator on
> 2026-10-10. The command surface moved to `utilities/60_ask.md` when the shell began answering it; this
> stays the design. Three things changed while building it, each recorded where it applies: the general
> question "why did <service> <event>" beside the four (section 4), the question always in quotes
> (section 2), and the supervisor gained the read-only question it needed (`supcmd::MANAGED`, section 6).

**Know what your system knows.**

`ask` answers ordinary questions about the machine - is it healthy, what failed, why did the WiFi
restart, is the filesystem all right - by asking the services that already know, and saying plainly
which part of the answer is a recorded fact, which is an inference, and which nobody can tell. It is
the first command that EXPLAINS rather than reports, and its one rule is the project's rule (CLAUDE.md
26.7): **an honest "unknown" beats a convincing answer the evidence does not support.**

It is not a chatbot, an administrator, a monitor or a repair agent. It runs no model, needs no network,
keeps nothing running between questions, and changes nothing.

## 1. Shape: a foreground program for one question, not a service and not a shell built-in

`ask` is a FOREGROUND program whose whole life is one question: the shell starts it, `ask` answers, and
it is gone before the next prompt. Nothing of it stays running. It is NOT like `copier` and `recorder`,
which this section first named as its model: they are started on demand too, but they stay up afterwards
and serve later requests, which is a background service, and `ask` is not one.

How it ends, stated because v1 has no way for a task to end itself: the only call that ends a task is
`kill`, and `ask` must not hold the authority to kill. So the SHELL, which does, ends it as soon as the
answer is in, and on a bounded deadline if the answer never comes - which a dead or wedged source cannot
cause, since each request inside `ask` is bounded too (section 7). `observe now` is the nearest existing
shape, and also the difference: it parks after its frame and lingers until the next `observe now`;
`ask` does not linger.

Two reasons decide that it is a program at all, and both are about authority rather than convenience:

- **It holds only what it reads.** A shell built-in would answer with the shell's whole authority,
  which includes killing services and formatting disks. A separate program is granted, in its spawn
  row, exactly the read-only reach it needs: its log, `INTROSPECT` (read), and send capabilities to the
  services it asks. Nothing it is told can widen that - a question is data, not a grant (CLAUDE.md 3.1).
- **It costs nothing while nobody is asking.** No resident service, no background thread, no memory
  between invocations. Each question is independent of the last.

The shell passes the question and relays the answer; `ask` prints nothing itself. It sends the answer
to the shell as one record table, which the shell renders as text at the prompt and passes on as records
when it is piped (rule 12). Written on `gs` throughout, so `scripts/one_way_check.py`
holds it at zero raw SDK calls from its first commit.

## 2. The command

```
ask "<question>"            the answer
ask evidence "<question>"   the same assessment, with every observation behind it
ask help                    what it can be asked, with one real example per question kind
ask version
```

**The question is always in quotes** (operator, 2026-10-11). Outside the quotes is a mode word, inside is
the question, so a question that begins with a mode's word cannot be misread as one, a later mode needs no
new rule, and the characters the shell gives a meaning - `|`, `'` - are plain text inside. An unquoted
question is refused with the line to type.

A bare `ask` prints its usage (rule 1). `ask evidence` runs the SAME evaluation as `ask` and changes
only how much of it is shown: within one invocation the summary and the evidence come from one
assessment, never two.

**What a question returns, for a script.** An answered question is a success whatever the answer is -
"the filesystem is damaged" is a successful answer. A question `ask` does not understand, an ambiguous
one, an empty one, a refused request to act, and an answer in which a source did not answer are failures,
and the answer's last record (`status`) says which (there are no numeric
exit codes in `gsh`; `utilities/32_result.md`). A script that wants the health verdict pipes the
records - `ask "is my system healthy?" | where label=verdict` - rather than reading success as health.

## 3. The questions: the first four (v0.1), and those added since

Each is answered only from sources that exist today and that answer a defined request. Where a source
does not exist, the answer says so; section 6 lists what each question would need to say more.

### 3.1 Is the system healthy?

Sources: the task table through `task_stat` (`INTROSPECT`) - which services are live and their restart
counts; the services the supervisor keeps running (`supcmd::MANAGED`); `fs`'s drive report
(`OP_DRIVES_INFO`) - a device or none, mounted or not; and each radio's status
(`godspeed_wifi::wire::OP_STATUS`). Whether a mount is read-only is not reported by any request `fs`
answers, so it is not part of the verdict.

Verdicts: **HEALTHY** (every check that ran passed and every required check ran), **DEGRADED** (working,
with something impaired - a restarted service now live, a device attached and nothing mounted, a radio
present and down), **UNHEALTHY** (a service the supervisor keeps running is not running), **UNKNOWN**
(nothing that answered found a fault, but a source did not answer). No disk at all, and no radio
answering on its bus, are said and are not called faults: nothing here can tell them from a machine
without the part. A machine
with no recorded error is not therefore healthy, and a source that did not answer is not therefore
failed: it is a hole in the coverage, and the answer names the hole.

### 3.2 What failed?

Sources: the restart count `task_stat` carries for every watched service, the supervisor's bounded
record of driver deaths, restarts and USB arrivals (`supcmd::EVENTS`, the newest 32), and the services
it keeps running that are down now.

It keeps apart what those sources keep apart: a death the supervisor restarted, a restart that did not
take, a driver that could not be started for a device that arrived, a service down now, and a service
live again. Arrivals and departures are not failures and are not listed. This question does not read
how each service ended; `why did <service> fail?` does (section 6, 1). **A restart is not proof of a crash** - a
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

Sources: `fs`'s drive report - the device's size, mounted or not, its label and how full it is. Whether
it is mounted read-only is not reported by any request, and the answer says so. A read-only scrub
(`drives scrub`) would read every block's CRC; it is offered, never run.

When `fs` has no disk or does not answer, the links below it are asked the same read-only question in
their own framing - `block-driver`, then the USB host it holds a capability to - and the answer names the
DEEPEST link that is not running or did not answer, because a link waiting on the one below it goes
silent for that reason. Every link answering with no disk is NO DISK ATTACHED, not a fault.

It separates **now** from **before**. A journal replayed at the last mount is history, not a fault
today - but `fs` does not report that it replayed one through any request, so v0.1 cannot see it, and
says so (section 6). A scrub is offered rather than run, because it reads the whole volume.

### 3.5 Is the network up? (added after v0.1)

Sources: `net-stack`'s status - the address, whether DHCP leased it, the gateway and DNS server, whether
the gateway has answered ARP - and each radio's status where the link is wireless. An address and a
gateway that answers mean this machine is on a network (an INFERENCE, rule `configured`), NOT that
anything beyond the gateway answers: nothing is sent to find out, and `ping` and `net dns` are offered.
The kernel's six-peer limit puts this question on its own spawn row, `ask-net` (`utilities/60_ask.md` 5).

### 3.6 Added after v0.1

- **What is <service>?** - its crate's own `description`, read from `Cargo.toml` when `ask` is built
  (`build.rs` generates `SERVICES`; a crate under `services/` without one fails the build), and whether
  it is running and kept running.
- **What can <service> do?** - the capabilities the kernel holds for it (`INTROSPECT`): each kernel
  authority by its contract name (`gs::task::AUTHORITIES`, the table the shell's `caps` uses), the running
  services it can send to, and counts of delegated resources and other endpoints. What it CAN do, never
  what it has done.
- **Who can <do this>?** - every running task holding one kernel authority, named by a verb or the
  authority's own name; a word that names none is answered "which authority?".
- **What to say?** - the question forms with an example of each, then suggestions taken from this
  machine as it is.
- **What is <term>?** - a definition from `GLOSSARY.md`'s table, read when `ask` is built; a term one or
  two letters off is answered and said to be a reading, and an unknown one is never guessed.
- **Easter eggs** - a joke, the meaning of life, `sudo`, `rm -rf`, "are you alive?", "who are you?". Each
  verdict says what it is (`A JOKE`), so a script never takes one for an answer.

## 4. Understanding the question

**Beside the four, one general question: "why did (did, has) <service> <event>".** Asked for by the
operator on 2026-10-11 ("why did fs service restart", "why did fs service fail"). It is composed, not
listed: the SUBJECT is any name the machine knows - the task table's and the supervisor's - so a new
service needs no new word; the EVENT is a small closed set kept apart (restart, fail, crash, stop). What a
later release adds is an event word or a source, never a sentence. A named service with an event decides
the reading over a subsystem word ("why did fs restart" asks about the `fs` service, not the filesystem);
two services named is ambiguous; the WiFi words stay an alias, because "the wifi" names no service. It
answers when and how often, and how the service last ended (killed, by whom, or a CPU fault) - the first
gap in section 6, closed; WHY is still not recorded.

The order a question is read in, first match wins: the Easter eggs; "who can ..."; "what can <service>
..."; "what is <service>"; a term to define; "what to say"; a request to act; then a named service with
an event, the network, the WiFi or the filesystem, and health or what failed.

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
  I don't understand that question yet.
  I can answer questions about: system health, what failed, the WiFi restarting, the filesystem
  and why a service restarted, failed, crashed or stopped - name it, as in: why did fs restart?
  ask "what to say?" lists them with suggestions from this machine; ask "what is MISCIS?" defines a term
```

The vocabulary is read-only data compiled into the program, versioned apart from the diagnostics, and
pinned by regression tests: a question that was understood stays understood unless its first reading
was wrong. A request to DO something - "restart the wifi driver" - is answered with what `ask` is:

```
  I answer questions about the system; I cannot restart anything.
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

**One was closed while building it:** which services must be running. The health answer turns on it and no
request said; the supervisor knew (`MANAGED`, and which of them it wants now) and now answers
`supcmd::MANAGED`, read only - closed in the owner, as below says every gap must be.

`ask` may use only defined requests (no parsing a service's log lines as if they were a contract, no
private structures, no new syscall). Measured against that, three answers are thinner than they should
be, and each gap is a missing **contract**, closed in the service that owns the fact - never in the
kernel for `ask`'s sake:

1. **Why a service died - CLOSED 2026-10-11, how it ended.** The kernel's death notice now says how a
   task ended - killed, and by which task, or a CPU fault, its kind and address - and the supervisor keeps
   the newest per name (`supcmd::DEATH`; CLAUDE.md 6.2, amendment 2026-10-11). So "why did fs restart?"
   says `fs` WAS KILLED BY `shell`, or that it CRASHED on a memory fault at 0x0, and a kill is not called
   a failure. What stays UNKNOWN is the layer above: why the killer killed it, and which bug faulted -
   though a fault at address 0 is said to be a null pointer or a panic, which ends exactly that way. Only
   the newest death per service is kept, and the record starts again when the supervisor restarts. This
   is the one gap closed in the kernel, because the kernel owns the fact - it decides every death - and it
   is closed on the notice the kernel already sent, with nothing added for `ask`'s sake.
2. **That a journal was replayed.** `fs` logs it at mount and reports it through no request. A field in
   the drive report saying "recovered N blocks at the last mount" makes 3.4's history a fact.
3. **The network's health - ANSWERED 2026-10-11 without a new contract.** `net-stack` answers status
   (address, lease, gateway), not "healthy", so "is the network up?" says what that status establishes
   as facts and draws its verdict as an inference by a named rule (`configured`), which evidence mode
   shows (3.5). What stays UNKNOWN is whether anything beyond the gateway answers.

The boot record (`boot_record_read`) holds all of these as TEXT. Reading it would make the answers
fuller and would make every log line a contract - which is exactly the coupling this design refuses.

## 7. Bounds (the Pi 2 is the target)

No heap-heavy runtime, nothing resident. Targets, to be measured on the Pi 2 before they are claimed:
a question of at most 512 bytes, at most 256 records read per source, at most 16 KiB of output, at most
4 MiB of memory, about a second for an ordinary question. As built: 512 bytes read, at most 48 lines of
160 bytes in one record table, each request bounded at 3 s (8 s for `fs`'s drive report), the shell's
wait bounded at 25 s, and a spawn row granting 8 MiB. None of it is measured on a board yet. Every request is a bounded `gs::call`, so a
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

Language models, voice, memory between questions, repairs, running commands from a question, testing
the network by sending anything, a resident daemon, learning words from what users type, any widening of authority, and
coverage of every subsystem. Later versions may add question kinds, time windows ("since the last
boot"), correlation across sources, and versioned descriptions a service publishes of the checks it
answers - which may describe a check and must never grant anything.

## 10. The rules

1. A program for one question, not a service.
2. Defined requests only; a gap is closed in the owner, never in the kernel for `ask`'s sake.
3. Read only.
4. No invented evidence.
5. No correlation offered as cause.
6. No authority it was not granted.
7. The vocabulary grows apart from the diagnostics.
8. It runs on the Pi 2.
9. It is useful with no model at all.
10. "Unknown" over a convincing answer the evidence does not support.
