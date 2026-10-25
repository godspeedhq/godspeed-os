<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `ask` - plain questions about this machine, answered from what its services know

Implementation shape: **a foreground program for one question** (`services/ask`), started by the shell,
handed the question, read back as one record table, and ended by the shell before the next prompt.
Nothing of it stays running and nothing restarts it. Read only, always. The design, and what it does not
do yet, is `docs/ask-design.md`.

## Status, as built and honest (2026-10-11)

Built on `feat/ask`: the four questions of the design (system health, what failed, the WiFi restarting,
the filesystem, with the storage chain walked when there is no disk), the general question **why did
`<service>` `<event>`** with how the service last ended, the network, what a service is and what it can
do, who holds an authority, what to say, glossary terms, the Easter eggs, `ask evidence`, and records when
piped. QEMU-verified only; no board has run it yet. The vocabulary and every truthfulness rule are host
tests (`services/ask/src/know.rs`, run by `scripts/host_test_check.py`).

*(Note 2026-10-25: `feat/ask` merged to `main` for v0.26.0, and that merge records `chaos max-carnage`
1000 and `selfcheck` 0 failed on all five boards - the Pi 4, Pi 2, VisionFive 2 Lite, Dell Wyse 5070 and HP
T630 - with `ask` in the image. The paragraph above is the state on the day it was written.)*

## 1. The command

```
ask "<question>"            the verdict, then what it rests on, as plain sentences
ask evidence "<question>"   the same answer, every line labelled FACT, INFERENCE or UNKNOWN, with its source
ask help                    what it can be asked, with an example of each
ask version
```

A bare `ask` prints its usage and fails (rule 1). `evidence` changes how much is SHOWN: within one
invocation the summary and the evidence are one assessment.

## 2. What it can be asked

| Question | What answers it |
|---|---|
| `is my system healthy?` | the task table, the services the supervisor keeps running (`supcmd::MANAGED`), `fs`'s drive report, each radio's status |
| `what failed?` | the supervisor's record of driver deaths and restarts (`supcmd::EVENTS`, the newest 32), the task table's restart counts, what is down now |
| `why did <service> <event>?` | that service in the task table, the supervisor's record, and how it last ended (`supcmd::DEATH`, section 3). `<service>` is any name the machine knows - the task table's and the supervisor's - so a new service needs no new word; `<event>` is one of restart, fail, crash, stop, kept apart |
| `why did the wifi restart?` | the radio driver that is running, its restarts, and the radio's own status. "The WiFi" is an alias, not a service |
| `is the filesystem healthy?` | `fs`'s drive report; a scrub is offered, not run |
| `is the network up?` (`why can't I reach the internet?`, `am I online?`) | `net-stack`'s status - the address, whether DHCP leased it, whether the gateway answers ARP - and the radio where the link is wireless. An address and a gateway mean this machine is on a network, NOT that anything beyond the gateway answers: nothing is sent to find out, and the commands that would test it are offered. Started as the `ask-net` row (below) |
| `what is <service>?` (`what does xhci do?`) | the service's own description - the `description` in its crate's `Cargo.toml`, read when `ask` is built, the one place a service says what it is - and whether it is running and kept running by the supervisor. A name is a service here only while it is running or the supervisor keeps it running; any other word is looked up as a term (below) |
| `what can <service> do?` (`what can fs reach?`, `what capabilities does shell have?`) | the capabilities the kernel holds for that task (INTROSPECT): each kernel authority by the name a contract uses and what it allows, and the running services it can send to. What it CAN do, never what it has done |
| `who can <do this>?` (`who can reboot?`, `which services can kill?`) | every running task holding that kernel authority. The words are the authorities' names and the verbs for them (reboot, kill, restart, spawn, type, introspect, mint, power, pci, ...); a word that names none is answered "which authority?", never guessed. A task whose capabilities could not be read is named as UNKNOWN and counted in the verdict ("AT LEAST 2 TASK(S) ... 1 COULD NOT BE READ"), never taken to hold nothing. `ask` itself is left out |
| `what to say?` (or `what can I ask?`, `help`) | the forms above, each with an example, then suggestions from this machine as it is: a service that has restarted (most first), one that should be running and is not, the WiFi where a radio driver runs, the filesystem where `fs` does. Nothing is suggested that this machine could not answer |

A question it does not understand is said to be, with a short list of what it can answer; a question that
fits two kinds, or names two services, is not answered - both readings are named and it asks "which?"; a request to DO something (`restart
fs`) is refused - `ask` reads and changes nothing.

**What a term means** (operator, 2026-10-11): `what is MISCIS?`, `what does AMD-Vi mean?`, `define MMIO`.
Answered from `GLOSSARY.md`, read when `ask` is built, so there is one copy of every definition. A term one
or two letters off is answered and the answer says how it was read (`I read 'immou' as IOMMU`); a term the
glossary does not have is "not in the glossary", never a guess. Each ends with the published glossary's
address. A question word is never taken for a term: `what is wrong?` is still the health question.

**And a few it was not built to answer, but does** (operator, 2026-10-11): `tell me a joke`, `what is the
meaning of life?`, `sudo ...`, `rm -rf /`, `are you alive?`, `who are you?` / `what is godspeed?`. Each
says what it is - a joke's verdict is `A JOKE` - so a script reading verdicts never takes one for an
answer, and every line is true or is a joke. There are 180 jokes, fixed in the program, and the uptime
second picks one by a stride that visits every joke before any repeats: no joke comes round again within
three minutes, and two questions a few seconds apart get jokes from different parts of the table.

## 3. What every line is

- **FACT** - a source said it, or recorded it. The source is named.
- **INFERENCE** - concluded from facts by a written rule. The rule is named.
- **UNKNOWN** - not established by anything available.

**How a service ended is a fact; why is usually UNKNOWN.** The kernel's death notice says how a task
ended and the supervisor keeps the newest per service (`supcmd::DEATH`), so `ask "why did fs fail?"`
says `fs` WAS KILLED BY `shell` - and that a kill is not a failure - or that it CRASHED on a memory fault
at an address. Why `shell` killed it, or which bug faulted, is not recorded anywhere, and the answer says
so; a fault at address 0 is said to be a null pointer or a panic, which ends exactly that way. Only the
newest death per service is kept (`docs/ask-design.md` 6).

Curly quotes count as quotes. At the prompt the shell reads a curly quote as `"`, so a question typed on
a phone or pasted from a document is quoted exactly as a straight one, pipes included.

Each answer carries the uptime it was taken at. A source that did not answer is a hole the answer names,
never a pass: `HEALTHY` is said only when every required check ran.

## 4. Result, and records

An answered question succeeds whatever the answer says - "the filesystem is not mounted" is a successful
answer. A question not understood, an ambiguous one, a refused action, and an answer in which a source
did not answer all fail, and the last record says which.

Piped, the answer is records with the columns `label`, `source`, `rule`, `text` (rule 12): one row per
line, `label` being `verdict`, `fact`, `inference`, `unknown` or `note`, then one row of label `status`
whose `text` is `answered`, `source-silent`, `not-understood`, `ambiguous`, `action` or `empty`.

```
gsh> ask "what failed?" | where label=fact
gsh> ask "is my system healthy?" | where label=verdict
```

## 5. Two rows, one program

The kernel gives a task at most six send peers, and one row for every question would need seven. So the
supervisor carries two rows for the one program, as it does for `observe`: `ask` (the supervisor, `fs`,
`block-driver`, the USB host the board's disk is behind where there is one, and the radio) and `ask-net`
(the supervisor, `net-stack`, the radio - both radio drivers where a board carries two, the Pi 4 and the
VisionFive). The shell picks the row by reading the question with `ask`'s own `know.rs`, compiled into the shell from
the same file, so the two cannot disagree about which question it is. Each row's grants are pinned
(Commandment VII).

## 6. Bounds

A question is read to 512 bytes; an answer holds at most 48 lines and one record table's text, and one
that runs out of either says it is PARTIAL. Every request `ask` makes waits at most 3 s - `fs`'s drive
report 8 s, because it re-asks `block-driver` for the device's size - and the shell waits at most 25 s
for the answer, so a dead source ends one line ("did not answer") and never the
command.

That wait is quittable and says so: once 2 s pass with nothing from `ask`, the shell prints `[q] quit`,
and `q` ends it (conventions rule 10). If the answer is not whole, the shell says who ended it (`backlog/84` S5):

- `q`: only `pipe: aborted`, since the operator knows why it stopped;
- the shell's 25 s bound: `ask: no answer - nothing came for ~25s, so the shell ended ask`, or `ask: the
  answer arrived incomplete - nothing more came for ~25s, so the shell ended ask` when part of it came;
- `ask` itself, ending its stream: `ask: ask ended without an answer`, or `ask: ask ended, and what it sent
  does not read as an answer`;
- an answer larger than the shell holds: `ask: the answer was larger than the shell holds, so what arrived
  does not read as one`.

It does not print the record decoder's reason, which is about the wire format and not about what the
operator asked.
