<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `ask` - plain questions about this machine, answered from what its services know

Implementation shape: **a foreground program for one question** (`services/ask`), started by the shell,
handed the question, read back as one record table, and ended by the shell before the next prompt.
Nothing of it stays running and nothing restarts it. Read only, always. The design, and what it does not
do yet, is `docs/ask-design.md`.

## Status, as built and honest (2026-10-11)

Built on `feat/ask`: the four questions of the design (system health, what failed, the WiFi restarting,
the filesystem), the general question **why did `<service>` `<event>`**, `ask evidence`, records when
piped. QEMU-verified only; no board has run it yet. The vocabulary and every truthfulness rule are host
tests (`services/ask/src/know.rs`, run by `scripts/host_test_check.py`).

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
| `why did <service> <event>?` | that service in the task table and the supervisor's record. `<service>` is any name the machine knows - the task table's and the supervisor's - so a new service needs no new word; `<event>` is one of restart, fail, crash, stop, kept apart |
| `why did the wifi restart?` | the radio driver that is running, its restarts, and the radio's own status. "The WiFi" is an alias, not a service |
| `is the filesystem healthy?` | `fs`'s drive report; a scrub is offered, not run |
| `what to say?` (or `what can I ask?`, `help`) | the forms above, each with an example, then suggestions from this machine as it is: a service that has restarted (most first), one that should be running and is not, the WiFi where a radio driver runs, the filesystem where `fs` does. Nothing is suggested that this machine could not answer |

A question it does not understand is said to be, with the list above; a question that fits two kinds, or
names two services, is answered with both and the question "which?"; a request to DO something (`restart
fs`) is refused - `ask` reads and changes nothing.

**What a term means** (operator, 2026-10-11): `what is MISCIS?`, `what does AMD-Vi mean?`, `define MMIO`.
Answered from `GLOSSARY.md`, read when `ask` is built, so there is one copy of every definition. A term one
or two letters off is answered and the answer says how it was read (`I read 'immou' as IOMMU`); a term the
glossary does not have is "not in the glossary", never a guess. Each ends with the published glossary's
address. A question word is never taken for a term: `what is wrong?` is still the health question.

**And a few it was not built to answer, but does** (operator, 2026-10-11): `tell me a joke`, `what is the
meaning of life?`, `sudo ...`, `rm -rf /`, `are you alive?`, `who are you?` / `what is godspeed?`. Each
says what it is - a joke's verdict is `A JOKE` - so a script reading verdicts never takes one for an
answer, and every line is true or is a joke. The jokes are fixed in the program and rotate with the
uptime second.

## 3. What every line is

- **FACT** - a source said it, or recorded it. The source is named.
- **INFERENCE** - concluded from facts by a written rule. The rule is named.
- **UNKNOWN** - not established by anything available.

**Why something happened is usually UNKNOWN.** The supervisor records THAT a service died and was
restarted, not why, so `ask "why did fs fail?"` says when and how often, and that the cause is not
recorded. A restart is not reported as a failure: a deliberate `kill` restarts a service too, and the
record does not say which (`docs/ask-design.md` 6).

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

## 5. Bounds

A question is read to 512 bytes; an answer holds at most 48 lines and one record table's text, and one
that runs out of either says it is PARTIAL. Every request `ask` makes waits at most 3 s, and the shell
waits at most 20 s for the answer, so a dead source ends one line ("did not answer") and never the
command.
