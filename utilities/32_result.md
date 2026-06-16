# Utility: `result` — the previous command's result (Ok / Err)

**Status:** **Built + QEMU-verified** (`osdev test files`). The first slice of the shell's
command-result model. Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

Every shell command now produces a **`Result`** — Rust's, not a POSIX exit code. Success is
`Ok`; failure is `Err(ShellError)`. `result` prints the **previous** command's result:

```
gs> read /notes.txt
…contents…
gs> result
Ok
gs> read /nope
read: not found: /nope
gs> result
Err(FileNotFound)
```

It is deliberately **not** a number and **not** an "exit code". Nothing exits between commands —
the shell stays alive and `result` asks "how did the last one go?" (`exit`/`code` are the POSIX
reflexes we drop, same family as `exec`→`run`; `0_conventions.md` §8.) The value reads exactly
like a Rust `Result`: `Ok`, or `Err(<Variant>)`.

## 2. The model — Ok is the check

`execute()` returns `Result<(), ShellError>`. The **common path is just "is it `Ok`?"** — a
caller (a future `run` aggregating pass/fail, or `assert`) never needs to know the variant names.
The variants exist for when you *do* want to pin a specific failure (negative tests):

- **`Ok` is the check.** `is_ok()` answers "did it work?" for 90% of uses.
- **Variants are opt-in.** `Err(FileNotFound)` names the category when you care; the
  human-readable detail (the path, etc.) was already printed by the command itself.
- **`Unknown` is the catch-all.** A failure not yet given its own variant is `Err(Unknown)`, so
  *every* failure is at least that. Variants are added one at a time as commands are converted.

`ShellError` variants are **unit** (no payload): `no_std`/no-heap means a variant can't own a
`String`, and the detail belongs in the printed message anyway. The enum is the *category*; the
message is the *detail*.

## 3. Behaviour

- `result` reports the immediately preceding command. It is itself a command that **succeeds**,
  so running `result` twice shows the real result, then `Ok` (the first `result`'s success).
- A **blank line is not a command** — it leaves the last result unchanged.
- A number is never involved. (If an external host tool ever needed one, it would be *derived*
  from the variant at the very edge — like `date epoch` — never the source of truth. Not built.)

## 4. Conversion status (incremental)

The shell is being moved to the `Result` model **incrementally** (§26.2). Converted so far:

- **`read`** + the **file/storage commands** — `ls`, `cd`, `write` (+`append`), `mkdir`,
  `copy` (+recursive), `move`, `rename`, `delete`, `find`, `tree`, and the filter built-ins'
  direct form `match`/`count`/`sort`/`first`/`last`: `Ok` on success, `Err(FileNotFound)` for a
  missing path, `Err(Unknown)` for other failures.
- **Service-control** — `spawn`/`kill`/`restart`: `Err(Denied)` for a protected core /
  session-critical service (so `assert fails spawn supervisor` holds), `Err(Unknown)` otherwise.
- **An unknown command is `Err`** (so `assert fails typo` holds; a typo in a script counts).
- **Pipelines** — `pipe_run` returns the pipeline's `Result` (a `| assert` sink sets it; a stage
  error is `Err`).

The `ShellError` variants now in use: `FileNotFound`, `Denied`, `AssertFailed`, `Unknown`.

Still **`Ok`-wrapped** (don't meaningfully fail, so it would be noise): the info commands
(`echo`/`about`/`mem`/`cores`/`date`/`status`/`observe`/`caps`/`clear`/`help`) and `drives`
(its sub-verbs print their own status). Future variants as needs appear: `StorageUnavailable`,
`EndpointDead`, … (reuse kernel names — §7.7).

## 5. Later (separate so it can grow)

- Convert the remaining commands (file ops, spawn/kill/restart, the record verbs, pipelines).
- **`run <script>`** — **built** (`utilities/33_run.md`): executes a file of commands,
  aggregating `result` into "ran N, failed M".
- **`assert`** — **built** (`utilities/34_assert.md`): `assert ok/fails <cmd>` (result) and
  `… | assert contains X` (content), so a script self-verifies the guardrails and the output.

## 6. Implementation shape & conformance

`execute()` threads the previous result as **local session state** (no global — services hold no
global mutable state, §3.9) and returns the new one; the main loop stores it. `result` is a shell
built-in. Conforms to `0_conventions.md`: its own `result help` / `result version` via the shared
`help_block`, listed under **Console** in the top-level `help`.
