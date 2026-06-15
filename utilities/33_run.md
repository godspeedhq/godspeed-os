# Utility: `run` — execute a script of commands

**Status:** **Built + QEMU-verified** (`osdev test files`). The shell script runner — the way to
replay a suite of commands without typing them (the point: validate on hardware). Trails
`CLAUDE.md`; does not amend it.

---

## 1. What it is

`run <path>` reads a script file and executes **each command exactly as if you typed it** at the
prompt. It is the trivial, sequential case of Appendix D.2 ("`cmd1; cmd2`") — a **command-list
runner**, deliberately **not** a scripting *language* (no variables, no `if`/`while`; those are
far-future, Appendix D).

```
gs> run /suite.gs
> read /lsr/big.txt
hello world
> result
Ok
> read /lsr/nope
read: not found: /lsr/nope
> result
Err(FileNotFound)
run: ran 4, failed 1
```

Each command is **echoed** (`> cmd`) before it runs, so the serial transcript is
self-documenting — exactly what you want when eyeballing a run on the T630.

## 2. The script format (`.gs`)

- **Lines** are split on newline; a non-comment line is further split on **`;`** into commands.
  So a script is either real multi-line, *or* `cmd ; cmd ; cmd` on one line — the latter is how
  scripts are authored today (the shell can't yet type a newline into a file; a host-side editor
  / image-baked `.gs` is the companion step).
- **`#` comments** — a line whose first non-blank character is `#` is skipped. Annotate freely.
- **Blank lines** are skipped.
- `.gs` is a naming **convention**, not a mechanism — `run` does not care about the extension
  (extension-driven behaviour is the hidden magic §26.5 forbids; cf. `read` not auto-parsing).

## 3. Result and the summary

`run` uses the command **`Result`** model (`32_result.md`): after every command it tracks
`Ok`/`Err`, and prints a summary —

```
run: ran N, failed M
```

`run` itself is `Ok` iff **every** command was `Ok` (so `result` after a `run` tells you whether
the whole script passed). Today a failing line is one that *errors* (a missing file, a bad
column, …). Verifying *correct output* (not just "didn't error") is the job of a future
**`assert`** — `… | assert contains X`, `assert fails read /nope` — which reads the same `Result`.

## 4. Bounds & safety (loud, never silent — §26.6 / §3.12)

- A script is one `fs` file, buffered whole; over `SCRIPT_MAX` (4 KiB) is reported, not silently
  truncated.
- **Scripts cannot nest.** A `run` inside a script is refused (`run` at depth > 0). This is a
  hard rule, not a nicety: unbounded `run`-calls-`run` recursion would overflow the bounded user
  stack (`execute`/`pipe_run` are `#[inline(never)]` so the per-line nesting stays shallow — the
  same stack discipline the record builders needed).
- A missing script is `Err(FileNotFound)`; storage unavailable is `Err(Unknown)`.

## 5. Later (separate so it can grow)

- **`assert`** — positive (`is_ok`) and negative (`fails-with <Variant>`) checks, so a script
  self-verifies instead of being eyeballed (the rung that makes the T630 print PASS/FAIL).
- **Image-baked `.gs`** — `osdev` writing a script into the flashed image's GSFS, so it's
  flash-and-`run` on hardware with no on-device authoring.
- Multi-line authoring on-device (a tiny editor, or newline-capable write).

## 6. Implementation shape & conformance

A shell built-in: reads the file via `fs` (op 11, like `read`), copies it off the `fs` reply,
then runs each command through the same `execute()` the prompt uses — so pipes, record verbs,
everything compose for free. Threads the per-line `Result` as local state (no global, §3.9).
Conforms to `0_conventions.md`: its own `run help` / `run version` via the shared `help_block`,
listed under **Console** in the top-level `help`.
