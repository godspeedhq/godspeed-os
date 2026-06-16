# gsh — the GodspeedOS shell language (design)

> **Status:** design sketch, not yet built. Scope is Tier 1–2 below. Files keep the `.gs`
> extension. Builds on the existing `run`/`run_lines` interpreter and the command **Result**
> model (`execute` already returns `Ok`/`Err`). Not POSIX — see CLAUDE.md Appendix B.3 / D.

## 1. Spirit

- **Go-simple surface.** Braces for blocks, newline-separated statements, **no trailing `;`**,
  no parentheses around conditions, a tiny keyword set.
- **Conditions are command results.** `Ok` is true, `Err` is false. The thing that makes
  `if`/`while` cheap is that we already produce a `Result` for every command.
- **Errors are checked, not hidden.** No `set -e`-style invisible mode — you inspect `result`
  where the error happens, the way Go writes `if err != nil { … }`. See §4.
- **Explicit and loud (the Godspeed part).** Referencing an undefined `$x` is an **error**, not
  a silent empty string. Every limit is fixed; overflow is a loud error, never silent truncation
  or a runaway loop.
- **Not bash.** No `fi`/`esac`/`done`, no word-splitting surprises, no `$?` quirks.

## 2. Lexical

- **A newline ends a statement.** Like Go, you don't write a trailing `;`. Use `;` *only* to put
  two statements on one line — `mkdir /x; cd /x` — never as a line terminator (a trailing one is
  just ignored).
- `#` to end of line is a comment; blank lines are ignored.
- Tokens: bare words (no spaces, literal), `"double quoted"` (spaces ok, `$var` expands),
  `'single quoted'` (literal, no expansion).

## 3. Variables, expansion, parameters

**Command position vs value position (the one disambiguation rule).** The interpreter is always in
one of two places, and that decides how bare text reads:

- **Command position** — a statement, or right after `if` / `while` / `for … in`. Bare text **is a
  command**; no marker needed.
- **Value position** — the right of `let x =`, or inside `"…"`. Bare text is a **literal**. To run a
  command here and use its output, promote it with `$( … )`.

The sigil rule that ties it together: **`$` means "the value of."** `$name` = the value of a
variable; `$(cmd)` = the value of running a command. (This is why `$(…)` is the capture form and a
bare `(…)` is *not* — one obvious way.)

```
let name = "Matthew"
let n    = 3
let out  = $(greet | count)    # value position: $( ) runs the command, captures its text (trimmed)
let lit  = greet               # value position, no $( ): the literal string "greet"

echo $name                     # Matthew
echo "hi $name, n=$n"          # hi Matthew, n=3
echo "live: $(status | where state=Running | count)"
```

- `let x = <value>` declares or reassigns. Value is a word, a `"string"`, or a capture `$( … )`.
- `$x` expands; **undefined `$x` is a loud error** (never a silent empty string).
- Script parameters: `run build.gs a b c` → `$1 $2 $3`, `$#` (count), `$@` (all), `$0` (name).

## 4. Conditionals

A **condition** is either a *comparison* (its first token starts with `$`, `"`, or a digit) or a
*command* (true iff it returns `Ok`). Pipelines are commands.

```
if read /sc/a.txt {
    echo "exists"
} else if ls /sc {
    echo "dir but no file"
} else {
    echo "nothing"
}

if $role == "core" {
    echo "the core service"
}

if !read /sc/secret {            # ! negates a command condition
    echo "absent — good"
}
```

- `if <cond> { … } else if <cond> { … } else { … }` — Go-style, no parens, no `fi`.
- Comparison ops: `== != < > <= >=` (numeric if **both** sides parse as integers, else string).

**Failure model — explicit, Go-style (no hidden `set -e`).** A failed statement is tallied (the run
summary already does this) and execution continues; you handle errors where they happen by checking
`result`, the way Go checks `if err != nil`:

```
mkdir /sc
if result == Err {
    fail "could not create /sc"
}

read /sc/cfg
if result == FileNotFound {        # compare the SPECIFIC variant, like errors.Is
    write /sc/cfg "defaults"
}
```

- `result` is a first-class value: after every statement it holds that statement's outcome. It *is*
  the `Result` the shell already threads between commands — so `result == Err` is just
  `prev.is_err()`, nearly free to implement.
- Compare `result` against `Ok`, `Err` (any failure), or a specific variant — `FileNotFound`,
  `Denied`, `AssertFailed`, `Unknown` (the same set `assert fails-with` uses).
- `fail <msg>` prints `<msg>` loudly and ends the script with `Err` (the script-level analog of
  `return err`). `return <cond>` ends a function (Tier 3) with a result.
- *Optional* one-liner sugar, addable later (it just desugars to the `result` check): `a and b`
  (run `b` iff `a` was `Ok`), `a or b` (run `b` iff `a` was `Err`). Not required — checking `result`
  is the canonical way.

## 5. Loops

```
for line in (greet) {            # iterate the LINES of a byte stream
    echo "> $line"
}

for row in (status | where state=Running) {   # iterate the ROWS of a record stream
    echo "$row.name on core $row.core"         # record fields via $row.<column>
}

for i in range 3 {               # 0 1 2
    echo $i
}

while $n > 0 {
    echo $n
    let n = (sub $n 1)
}
```

- `for x in ( … )` iterates lines of a byte stream **or** rows of a record stream; record rows
  expose columns as `$x.<col>` (this is the payoff of typed pipes — nothing in POSIX has it).
- `for i in range N` is the counting loop; `while <cond> { … }` reuses the §4 grammar.
- `break` / `continue`. **Every loop has a hard iteration cap (default 100k)** → exceeding it is a
  loud error, never a silent hang.

## 6. Switch

```
switch $role {
    core           { echo "the core" }
    worker courier { echo "a helper" }      # multiple values per arm
    _              { echo "unknown" }        # _ = default
}
```

Go-style: no fallthrough, `_` default.

## 7. Functions

```
fn greet name {
    echo "hello $name"
}

fn ensure_dir path {
    if !ls $path {
        mkdir $path
    }
}

fn double n {
    add $n $n
}

greet Matthew                  # a function is just a command
ensure_dir /sc
let d = $(double 5)            # its OUTPUT is its value — capture with $( )
if double 5 { echo "ok" }      # its RESULT (Ok/Err) drives if/while
```

A function is **just a command** — that's what keeps it coherent with the rest of the language:

- `fn <name> <param…> { … }` — named positional params, bound as `$name` in the body (`$1 … $@
  $#` also available).
- **Output is the value, result is the control.** `$(f …)` captures what the function printed;
  its `Ok`/`Err` works in `if`/`while` like any command. `return <cond>` ends early with that
  result; falling off the end returns the **last statement's** result (so a helper ending in an
  `assert` returns the assert's verdict). No separate "return value" concept.
- **No ambient variables (the capability parallel).** A function sees only its **parameters** and
  its own **locals** — not the script's globals. If it needs a value, pass it. Inputs are explicit,
  exactly like a service gets only the capabilities it is handed (§3.1). Assignments inside are
  local and vanish on return.
- **Defined anywhere.** A one-pass pre-scan indexes every `fn` block, so you can call a function
  before its definition (like Go's package funcs).
- **Bounded, not Rust-recursive.** Calls use the interpreter's explicit **call-frame stack** (a
  fixed array of frames), not native recursion — so the user stack does not grow per call.
  Recursion is allowed but call depth is capped (loud error on overflow).

## 8. New builtins (each returns a Result, so they compose with if/while)

- `let` — declare/assign a variable.
- `result` — the previous statement's outcome, as a value (compare with `== Ok` / `== Err` / `== <Variant>`).
- `fail <msg>` — print loudly and end the script with `Err`; `return <cond>` ends a function (§7).
- `Ok` / `Err` and the variant names (`FileNotFound`, `Denied`, `AssertFailed`, `Unknown`) — the
  result values you compare against.
- `true` / `false` — always-`Ok` / always-`Err` (safe in `while true { … break }` thanks to the loop cap).
- `range N` — the counting iterator for `for`.
- `add` / `sub` — Tier-1 integer math (full inline expressions are Tier 3).
- `eq ne lt gt le ge` / `empty <v>` — prefix comparisons, when you prefer them to infix.
- `break`, `continue`.

`assert` and the whole existing command set are unchanged, and a script's own result is still `Ok`
iff every top-level statement was `Ok` (the PASS/FAIL summary keeps working).

## 9. Bounded by design (no_std, no heap, tight 256 KiB user stack)

| Thing            | Bound (loud on overflow)                          |
|------------------|---------------------------------------------------|
| variables        | 32 × (name ≤ 24 B, value ≤ 128 B)                 |
| block nesting    | 16                                                |
| loop iterations  | 100k (configurable per loop later)                |
| function call depth | 16 frames (each = its params + locals)         |
| script size      | embedded: rodata; on-disk file: `MAX_FILE_BYTES`  |

The function call-frame stack is the explicit array above — a call pushes a frame, not a Rust
stack frame — so the user stack stays flat regardless of call depth.

**Execution model:** a line-oriented interpreter with an **explicit control stack** — brace-matched
block bounds, loops re-seek to their start, conditions evaluated by running a command or comparing
two values. **No native recursion, no AST.** This is what keeps it inside the user stack (the same
discipline behind every `#[inline(never)]` in the shell today).

## 10. Out of scope (for now)

Capability delegation in script syntax (pipes-as-caps, `spawn` with explicit grants) is Appendix
D.3 territory and waits on a service-initiated spawn API. An inline arithmetic/string **expression
evaluator** (`$i + 1`, length, slice) and **cross-file include / `source`** stay Tier 3 — those are
the genuinely balloon-prone parts. (Functions moved *out* of Tier 3 — see §7/§11.)

## 11. Tiers / effort

- **Tier 1** (~2–4 days): `let` + `$`-expansion + params, `if`/`else if`/`else`, comparisons,
  `result`/`fail`, `switch`.
- **Tier 2** (~4–6 days): `$( )` capture, `for` (lines / rows / `range`), `while`,
  `break`/`continue`, and **functions** (`fn`, pre-scan index, call-frame stack, `return`) —
  pulled in early because helpers earn their keep from the first real script.
- **Tier 3** (resist): inline arithmetic/string **expression evaluator**, cross-file
  include / `source`.

## 12. Worked example

```
# a helper that takes only what it is given (no ambient vars)
fn ensure_dir path {
    if !ls $path {
        mkdir $path
        if result == Err {
            fail "could not create $path"
        }
    }
}

# provision once, then report — re-runnable
ensure_dir /sc

for row in (roster | where core > 0) {
    echo "$row.name is a $row.role on core $row.core"
}

let lines = $(greet | count)
echo "greet emitted $lines"
```

## 13. Example programs (what we'll be implementing)

Each of these uses only Tier 1–2 features (vars, params, `if`/comparison, `result`/`fail`,
`switch`, `for`, `while`, functions, `$( )` capture). They are the kind of script the language is
*for* — and the bar each milestone is "these run."

### 13.1 `setup.gs` — provision a workspace, fail loudly if it can't

```
fn ensure_dir path {
    if !ls $path {
        mkdir $path
        if result == Err {
            fail "cannot create $path"
        }
        echo "created $path"
    }
}

ensure_dir /work
ensure_dir /work/logs

if !read /work/config {
    write /work/config "mode=normal"
    echo "seeded default config"
}

echo "workspace ready"
```

### 13.2 `greet.gs` — parameters, usage check, and `switch`

```
# run greet.gs <name> <role>
if $# < 2 {
    fail "usage: run greet.gs <name> <role>"
}

let name = $1
let role = $2

switch $role {
    core           { echo "$name runs the core" }
    worker courier { echo "$name is a $role" }
    _              { echo "$name has an unknown role: $role" }
}
```

### 13.3 `report.gs` — iterate a record stream (`$row.col`)

```
echo "running services by core:"
for row in (status | where state=Running) {
    echo "  $row.name  core=$row.core  queue=$row.queue"
}

# byte streams can be counted; record rows are iterated
let n = $(greet | count)
echo "greet still emits $n"
```

### 13.4 `check.gs` — a reusable test helper (functions + assert + result)

```
# a helper returns its last statement's result — here, the assert's verdict
fn roundtrip path text {
    write $path $text
    read $path | assert contains $text
}

roundtrip /a.chk hello
roundtrip /b.chk "two words"

# negative case: the file must NOT exist after delete
delete /a.chk
if read /a.chk {
    fail "delete did not remove /a.chk"
}

delete /b.chk
echo "all checks passed"
```

### 13.5 `retry.gs` — `while`, `break`, and a bounded counter

```
let tries = 0
while $tries < 3 {
    if read /work/ready {
        echo "ready after $tries retries"
        break
    }
    let tries = $(add $tries 1)
}

if $tries >= 3 {
    fail "never became ready"
}
```
