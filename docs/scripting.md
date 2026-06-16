# gsh — the GodspeedOS shell language (design)

> **Status:** design sketch, not yet built. Scope is Tier 1–2 below. Files keep the `.gs`
> extension. Builds on the existing `run`/`run_lines` interpreter and the command **Result**
> model (`execute` already returns `Ok`/`Err`). Not POSIX — see CLAUDE.md Appendix B.3 / D.

## Contents

1. [Spirit](#1-spirit)
2. [Lexical](#2-lexical)
3. [Variables, parameters, arithmetic](#3-variables-parameters-arithmetic)
4. [Conditionals](#4-conditionals)
5. [Loops](#5-loops)
6. [Switch](#6-switch)
7. [Functions](#7-functions)
8. [Builtins](#8-builtins)
9. [Bounds](#9-bounds)
10. [Out of scope](#10-out-of-scope)
11. [Tiers and effort](#11-tiers-and-effort)
12. [Worked example](#12-worked-example)
13. [Example programs](#13-example-programs) — [setup.gs](#131-setupgs) · [greet.gs](#132-greetgs) · [report.gs](#133-reportgs) · [check.gs](#134-checkgs) · [retry.gs](#135-retrygs)

---

## 1. Spirit

- **Simple, explicit surface.** Braces for blocks, newline-separated statements, **no trailing
  `;`**, no parentheses around conditions, a tiny keyword set.
- **Conditions are command results.** `Ok` is true, `Err` is false. The thing that makes `if`
  cheap is that we already produce a `Result` for every command.
- **Errors are checked, not hidden.** No `set -e`-style invisible mode — you inspect `result`
  right where the error can happen. See §4.
- **Explicit and loud (the Godspeed part).** Referencing an undefined `$x` is an **error**, not
  a silent empty string. Every limit is fixed; overflow is a loud error, never silent truncation
  or a runaway loop.
- **Not bash.** No `fi`/`esac`/`done`, no word-splitting surprises, no `$?` quirks.

## 2. Lexical

- **A newline ends a statement.** You don't write a trailing `;`. Use `;` *only* to put two
  statements on one line — never as a line terminator (a trailing one is just ignored).
- `#` to end of line is a comment; blank lines are ignored.
- Tokens: bare words (no spaces, literal), `"double quoted"` (spaces ok, `$var` expands),
  `'single quoted'` (literal, no expansion).
- **Multi-line content** uses a triple-quoted block — for writing real files. It expands `$vars`
  like `"…"`. It is for a command argument (bounded by `MAX_FILE_BYTES`); storing one in a `let`
  var that exceeds the 128 B value cap is a loud error, not a silent truncation.

```
echo one                        # a statement; the newline ends it
mkdir /x; cd /x                  # two statements on one line, joined by ;

echo "spaces kept, $name expands"
echo 'literal $name — no expansion'
```

```
write /work/config """
mode  = normal
cores = 4
owner = $name
"""
```

## 3. Variables, parameters, arithmetic

**Command position vs value position (the one disambiguation rule).** The interpreter is always in
one of two places, and that decides how bare text reads:

- **Command position** — a statement, or right after `if` / `for … in`. Bare text **is a
  command**; no marker needed.
- **Value position** — the right of `let x =`, or inside `"…"`. Bare text is a **literal**. To run a
  command here and use its output, promote it with `$( … )`.

The sigil rule that ties it together: **`$` means "the value of."** `$name` = the value of a
variable; `$(cmd)` = the value of running a command. (This is why `$(…)` is the capture form and a
bare `(…)` is *not* — one obvious way.)

```
let name = "Matthew"
let n    = 3
let lit  = greet               # value position, no $( ): the literal string "greet"

echo $name                     # Matthew
echo "hi $name, n=$n"          # hi Matthew, n=3
```

Capture — promote a command into a value with `$( )`:

```
let count = $(greet | count)               # 3
let when  = $(date)                         # the date stamp
let live  = $(status | where state=Running | count)
echo "running: $live, at $when"
```

Constants — immutable, loud on reassignment:

```
const ROOT = /work
const MAX  = 16
echo $ROOT/logs
# ROOT = /tmp        # error: cannot reassign a const
```

Script parameters — `run greet.gs Matthew core`:

```
echo "name=$1 role=$2"         # name=Matthew role=core
echo "got $# args: $@"          # got 2 args: Matthew core
echo "script: $0"               # script: greet.gs
```

- `let x = <value>` declares or reassigns; `const NAME = <value>` declares an immutable binding.
- A value is a literal word, a `"string"`, a capture `$( … )`, or an **arithmetic expression** (below).
- `$x` expands; **undefined `$x` is a loud error** (never a silent empty string).
- `$1 $2 …` positional, `$#` count, `$@` all, `$0` script name.

**Arithmetic.** Integer expressions, evaluated wherever a **value** is expected — the right of
`let`/`const`, and the operands of a comparison. Operators are `+ - * / %`, with bare `( )` for
grouping and the usual precedence (`* / %` before `+ -`). Operands are integer literals or `$vars`
that expand to integers.

```
let i = 0
let i = $i + 1                 # 1 — replaces the old $(add $i 1)
let area = $w * $h
let half = $n / 2              # integer division — truncates
let odd  = $n % 2
let cost = ( $base + $tax ) * $qty    # bare ( ) groups; $( ) is still capture
```

```
if $i + 1 > $max {
    echo "next step would overflow the window"
}
```

- **Signed 64-bit, checked.** Divide-by-zero and overflow are **loud errors**, never a wrap or a
  silent zero; a non-integer operand is a loud error too.
- **Space-separated operators**, so text is never mistaken for math: `$dir/sub` is a path,
  `$n / 2` is division. String building stays interpolation (`"$a$b"`, `"$dir/sub"`), never `+`.
- Arithmetic is **value-position only**. To use a result as a command argument, assign it first
  (`let d = $n + $n` → `echo $d`) — compute it, then use it.

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
```

```
if $role == "core" {
    echo "the core service"
}

if $n >= 3 {
    echo "enough"
}

if !read /sc/secret {            # ! negates a command condition
    echo "absent — good"
}
```

Membership — the condition form of a one-arm `switch`:

```
if $role in core worker courier {
    echo "known role"
}
```

- `if <cond> { … } else if <cond> { … } else { … }` — braces, no parens, no `fi`.
- Comparison ops: `== != < > <= >=` (numeric if **both** sides parse as integers, else string).
  Either side may be an arithmetic expression — `if $i + 1 > $max { … }` (§3).
- `$x in a b c` — true if `$x` equals one of the listed words.

**Failure model — explicit (no hidden `set -e`).** A failed statement is tallied (the run summary
already does this) and execution continues; you handle errors where they happen by checking
`result` right where it can fail:

```
mkdir /sc
if result == Err {
    fail "could not create /sc"
}
```

```
read /sc/cfg
if result == FileNotFound {        # compare the SPECIFIC failure kind
    write /sc/cfg "defaults"
}

kill supervisor
if result == Denied {              # a guardrail refused us
    echo "supervisor is protected"
}
```

- `result` is a first-class value: after every statement it holds that statement's outcome. It *is*
  the `Result` the shell already threads between commands — so `result == Err` is just
  `prev.is_err()`, nearly free to implement.
- Compare `result` against `Ok`, `Err` (any failure), or a specific variant — `FileNotFound`,
  `Denied`, `AssertFailed`, `Unknown` (the same set `assert fails-with` uses).
- `fail <msg>` prints `<msg>` loudly and ends the script with `Err`. `return <cond>` ends a
  function (§7) with a result.
- *Optional* one-liner sugar, addable later (desugars to the `result` check): `a and b` (run `b`
  iff `a` was `Ok`), `a or b` (run `b` iff `a` was `Err`). Not required.

## 5. Loops

Iterate a stream — byte lines, or record rows with `$row.<col>`:

```
for line in (greet) {            # the LINES of a byte stream
    echo "> $line"
}

for row in (status | where state=Running) {   # the ROWS of a record stream
    echo "$row.name on core $row.core"
}
```

Iterate params or a literal word list:

```
for arg in $@ {
    echo "arg: $arg"
}

for svc in logger fs registry {
    echo "checking $svc"
}
```

Counting loops with `range`:

```
for i in range 3 {               # 0 1 2
    echo $i
}

for i in range 2 6 {             # 2 3 4 5
    echo $i
}
```

`loop` repeats until you `break` — the one unbounded loop:

```
let i = 0
loop {
    let i = $i + 1
    if $i == 3 { continue }
    if $i > 5 { break }
    echo $i                       # 1 2 4 5
}
```

A loop that should stop on a condition just breaks on it:

```
loop {
    if read /work/ready { break }
    let waited = $waited + 1
}
```

- `for x in ( … )` iterates lines of a byte stream **or** rows of a record stream; record rows
  expose columns as `$x.<col>` (the payoff of typed pipes — nothing in POSIX has it).
- **Not `$( )` here.** `for … in` is command position — the live stream goes in directly (the
  parens are optional readability). `$( )` would *flatten* the command to text and lose `$row.col`;
  `for … in` keeps the stream so it can iterate rows. (`$( )` = "the text"; `for … in` = "the stream".)
- `for x in a b c` iterates literal words; `for x in $@` iterates the script's params.
- `for i in range N` / `range A B` counts.
- `loop { … }` repeats until `break`. There is no `while` — a conditional loop is
  `loop { if !cond { break } … }`, which keeps the exit explicit and visible.
- **Two loops, one job each — and the keyword tells you if it terminates.** `for` is **bounded**:
  it walks something finite. `loop` is the **unbounded** one: it runs until `break`, with the hard
  iteration cap (default 100k) as a loud backstop, never a silent hang.
- `break` / `continue`.

## 6. Switch

```
switch $role {
    core           { echo "the core" }
    worker courier { echo "a helper" }      # multiple values per arm
    _              { echo "unknown" }        # _ = default
}
```

```
switch $1 {
    start  { echo "starting" }
    stop   { echo "stopping" }
    status { echo "ok" }
    _      { fail "unknown command: $1" }
}
```

No fallthrough; `_` is the default; multiple values per arm.

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

greet Matthew                  # a function is just a command
ensure_dir /sc
```

Output is the value (`$( )`), result is the control (used by `if`):

```
fn full name surname {
    echo "$name $surname"
}

fn double n {
    let d = $n + $n             # arithmetic is value-position; assign, then output
    echo $d
}

let who = $(full Matthew Levi)  # capture the output
echo $who                       # Matthew Levi
echo $(double 5)                # 10
```

A function is **just a command** — that's what keeps it coherent with the rest of the language:

- `fn <name> <param…> { … }` — named positional params, bound as `$name` in the body (`$1 … $@
  $#` also available).
- **Output is the value, result is the control.** `$(f …)` captures what the function printed;
  its `Ok`/`Err` works in conditions like any command. `return <cond>` ends early with that
  result; falling off the end returns the **last statement's** result (so a helper ending in an
  `assert` returns the assert's verdict). No separate "return value" concept.
- **No ambient variables (the capability parallel).** A function sees only its **parameters** and
  its own **locals** — not the script's globals. If it needs a value, pass it. Inputs are explicit,
  exactly like a service gets only the capabilities it is handed (§3.1). Assignments inside are
  local and vanish on return.
- **Defined anywhere.** A one-pass pre-scan indexes every `fn` block, so you can call a function
  before its definition (so definition order doesn't matter).
- **Bounded, not natively recursive.** Calls use the interpreter's explicit **call-frame stack** (a
  fixed array of frames), not native recursion — so the user stack does not grow per call.
  Recursion is allowed but call depth is capped (loud error on overflow).

## 8. Builtins

Each returns a `Result`, so they compose with `if`.

- `let` / `const` — declare a mutable / immutable variable.
- `result` — the previous statement's outcome, as a value (compare `== Ok` / `== Err` / `== <Variant>`).
- `fail <msg>` — print loudly and end the script with `Err`; `return <cond>` ends a function (§7).
- `defer <command>` — run a command when the current scope (function, or the whole script) exits,
  **including on `fail`**; deferreds run LIFO.
- `Ok` / `Err` and the variant names (`FileNotFound`, `Denied`, `AssertFailed`, `Unknown`).
- `true` / `false` — always-`Ok` / always-`Err`.
- `range N` / `range A B` — the counting iterator for `for`.
- `empty <v>` — true if `<v>` is empty (a prefix test, handy in conditions).
- `break`, `continue`.

Arithmetic is **inline** (`+ - * / %`, value position) — see §3, not a builtin.

`defer` — cleanup that always runs:

```
mkdir /tmp/build
defer delete /tmp/build recursive    # runs no matter how we leave
write /tmp/build/out done
```

**Record aggregators (typed-pipe stages).** Because our pipes carry a typed `Table`, a pipeline can
*reduce* — something byte pipes can't. `count` is dual (rows of a record stream, lines of a byte
stream); the column reducers are record-only:

```
let rows  = $(roster | count)            # row count
let files = $(ls /work | count)          # entries in a directory
let used  = $(status | sum mem)          # sum a numeric column
let big   = $(ls /work | max size)       # largest file
let avgq  = $(status | avg queue)        # average a column
```

- `count` — rows (record stream) or lines (byte stream).
- `sum <col>` `min <col>` `max <col>` `avg <col>` — reduce a numeric column; a non-numeric or
  missing column is a loud error, never a silent 0.

`assert` and the whole existing command set are unchanged, and a script's own result is still `Ok`
iff every top-level statement was `Ok` (the PASS/FAIL summary keeps working).

## 9. Bounds

Bounded by design (no_std, no heap, tight 256 KiB user stack):

| Thing            | Bound (loud on overflow)                          |
|------------------|---------------------------------------------------|
| variables        | 32 × (name ≤ 24 B, value ≤ 128 B)                 |
| block nesting    | 16                                                |
| loop iterations  | 100k (configurable per loop later)                |
| function call depth | 16 frames (each = its params + locals)         |
| deferred actions | 8 per scope                                       |
| script size      | embedded: rodata; on-disk file: `MAX_FILE_BYTES`  |

The function call-frame stack is the explicit array above — a call pushes a frame, not a native
stack frame — so the user stack stays flat regardless of call depth.

**Execution model:** a line-oriented interpreter with an **explicit control stack** — brace-matched
block bounds, loops re-seek to their start, conditions evaluated by running a command or comparing
two values. **No native recursion, no AST.** This is what keeps it inside the user stack (the same
discipline behind every `#[inline(never)]` in the shell today).

## 10. Out of scope

For now:

- **Capability delegation in script syntax** (pipes-as-caps, `spawn` with explicit grants) is
  Appendix D.3 territory and waits on a service-initiated spawn API.
- A **string** toolkit (length, slice, split) — interpolation covers *building* strings; richer
  string ops wait. (Integer arithmetic is **in** — §3.)
- **Cross-file include / `source`**, and **record values held in variables** (hold a whole table in
  a `let`) — bigger storage stories; iterate with `for` for now.

These are the genuinely balloon-prone parts. (Functions and integer arithmetic are *not* here —
both are Tier 2.)

## 11. Tiers and effort

- **Tier 1** (~2–4 days): `let`/`const` + `$`-expansion + params, `if`/`else if`/`else`,
  comparisons + `in`, `result`/`fail`, `switch`.
- **Tier 2** (~6–8 days): `$( )` capture, multi-line `"""…"""`, **inline integer arithmetic**
  (`+ - * / %`, precedence, `( )`, checked), `for` (lines / rows / words / `range`), `loop`,
  `break`/`continue`, **functions** (`fn`, pre-scan index, call-frame stack, `return`), `defer`,
  and the **record aggregators** (`count`/`sum`/`min`/`max`/`avg`).
- **Tier 3** (resist): a string toolkit (length/slice/split), cross-file include / `source`,
  record-valued variables.

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

## 13. Example programs

Complete scripts — the kind the language is *for*, using only Tier 1–2 features. The bar for each
milestone is "these run."

### 13.1 setup.gs
*Provision a workspace; fail loudly if it can't.*

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
    write /work/config """
    mode  = normal
    cores = 4
    """
    echo "seeded default config"
}

echo "workspace ready"
```

### 13.2 greet.gs
*Parameters, usage check, and `switch`.*

```
# run greet.gs <name> <role>
if $# < 2 {
    fail "usage: run greet.gs <name> <role>"
}

let name = $1
let role = $2

if $role in core worker courier {
    switch $role {
        core           { echo "$name runs the core" }
        worker courier { echo "$name is a $role" }
    }
} else {
    fail "$name has an unknown role: $role"
}
```

### 13.3 report.gs
*Iterate and aggregate a record stream.*

```
echo "running services by core:"
for row in (status | where state=Running) {
    echo "  $row.name  core=$row.core  queue=$row.queue"
}

let running = $(status | where state=Running | count)   # row count (record-aware)
let memuse  = $(status | sum mem)                        # reduce a column
echo "$running running, $memuse KiB in use"
```

### 13.4 check.gs
*Test helper (functions + assert) with `defer` cleanup.*

```
const DIR = /check
mkdir $DIR
defer delete $DIR recursive        # cleaned up however we leave — even on fail

# a helper returns its last statement's result — here, the assert's verdict
fn roundtrip path text {
    write $path $text
    read $path | assert contains $text
}

roundtrip $DIR/a hello
roundtrip $DIR/b "two words"

# negative case: the file must NOT exist after delete
delete $DIR/a
if read $DIR/a {
    fail "delete did not remove $DIR/a"     # defer still runs DIR cleanup
}

echo "all checks passed"
```

### 13.5 retry.gs
*`loop`, `break`, and a bounded counter.*

```
let tries = 0
loop {
    if read /work/ready {
        echo "ready after $tries retries"
        break
    }
    if $tries >= 3 {
        fail "never became ready"
    }
    let tries = $tries + 1
}
```
