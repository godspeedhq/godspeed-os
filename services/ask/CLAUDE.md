# services/ask/

`ask` - Assisted System Knowledge. The command is `utilities/60_ask.md`; the design, and what it does not
do yet, is `docs/ask-design.md`.

## What it is

**A foreground program for one question, not a service.** The shell starts it wired to send to the shell
(`drain_service_with`), hands it the question and an end-of-stream byte, reads the answer back as one
record table, and ends it before the prompt returns. It is in no boot set and no `MANAGED` list, so
nothing restarts it, and nothing of it stays running. It does not end itself (it parks once its answer
is sent; `Exit`, syscall 57, CLAUDE.md 14.5, now exists but `ask` does not call it), and it must not hold
`kill`, so the shell - which started it and holds `kill` - ends it.

The supervisor carries TWO rows for this one program, because the kernel gives a task at most six send
peers: `ask` (storage and the radio) and `ask-net` (`net-stack` and the radio). The shell picks the row by
reading the question with this crate's own `know.rs` (`utilities/60_ask.md` 5).

## The two files, and why there are two

- `src/know.rs` - what a question MEANS and what an answer may SAY. Names nothing outside `core`, so
  `scripts/host_test_check.py` compiles it on its own and runs its tests on the host: the vocabulary, and
  fixtures of source replies with what each answer must and must not say. Changing a rule here is changing
  what `ask` is willing to claim; the tests are the contract.
- `src/main.rs` - asking the system: the task table (INTROSPECT), the supervisor's read-only questions
  (`supcmd::MANAGED`, `supcmd::EVENTS`, `supcmd::DEATH`), `fs`'s drive report (`gs::fs::Fs::drive`), the
  storage chain below `fs` when it has no disk (`block-driver`'s and its USB host's capacity),
  `net-stack`'s status (`gs::net::Net::status`, on the `ask-net` row), each running radio driver's status
  (tagged `wire::OP_STATUS`), and each task's capabilities (INTROSPECT, named from
  `gs::task::AUTHORITIES`, the table the shell's `caps` uses), each bounded. What a service IS comes from
  its crate's own `description`, and what a term means from `GLOSSARY.md`'s table, both read by
  `build.rs` at build time. Then the answer as records to `send_peers[0]`.

## The rules it keeps

Read only. Defined requests only - no log line is parsed as if it were a contract. Every line is a FACT,
an INFERENCE (by a named rule) or an UNKNOWN. A source that did not answer is a hole the answer names,
never a pass and never a fault. A service is named from what the machine knows, not from a word list, so
a new service needs no new vocabulary. HOW a service ended is a fact - the kernel's death notice says
killed (by whom), a CPU fault (kind, address) or an exit (its status, CLAUDE.md 14.5), and the supervisor
keeps the newest per name (`supcmd::DEATH`); `know.rs` reads the first two and not yet an exit. WHY is
usually UNKNOWN, and the answer says so (`docs/ask-design.md` 6).
