# services/ask/

`ask` - Assisted System Knowledge. The command is `utilities/60_ask.md`; the design, and what it does not
do yet, is `docs/ask-design.md`.

## What it is

**A foreground program for one question, not a service.** The shell starts it wired to send to the shell
(`drain_service_with`), hands it the question and an end-of-stream byte, reads the answer back as one
record table, and ends it before the prompt returns. It is in no boot set and no `MANAGED` list, so
nothing restarts it, and nothing of it stays running. v1 has no way for a task to end itself, and this
one must not hold `kill`, so the shell - which started it and holds `kill` - ends it.

## The two files, and why there are two

- `src/know.rs` - what a question MEANS and what an answer may SAY. Names nothing outside `core`, so
  `scripts/host_test_check.py` compiles it on its own and runs its tests on the host: the vocabulary, and
  fixtures of source replies with what each answer must and must not say. Changing a rule here is changing
  what `ask` is willing to claim; the tests are the contract.
- `src/main.rs` - asking the system: the task table (INTROSPECT), the supervisor's read-only questions
  (`supcmd::MANAGED`, `supcmd::EVENTS`), `fs`'s drive report (`gs::fs::Fs::drive`) and each running radio
  driver's status (tagged `wire::OP_STATUS`), each bounded. Then the answer as records to `send_peers[0]`.

## The rules it keeps

Read only. Defined requests only - no log line is parsed as if it were a contract. Every line is a FACT,
an INFERENCE (by a named rule) or an UNKNOWN. A source that did not answer is a hole the answer names,
never a pass and never a fault. A service is named from what the machine knows, not from a word list, so
a new service needs no new vocabulary. "Why" is usually UNKNOWN: the supervisor records that a service
died, not why (`docs/ask-design.md` 6).
