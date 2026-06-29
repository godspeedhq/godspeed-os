# Utility: `assert` - verify a result or output (the test verb)

**Status:** **Built + QEMU-verified** (`osdev test files`). The top rung of the script-testing
ladder (`result` → `run` → **`assert`**): it lets a `.gsh` script **self-verify**, so the device
prints PASS/FAIL instead of you reading the serial. Trails `CLAUDE.md`; does not amend it.

---

## 1. What it is

`assert` checks a claim and produces a command **`Result`** (`32_result.md`): `Ok` + `assert: ok`
if the claim holds, `Err(AssertFailed)` + an `assert: FAILED (…)` line if it doesn't. Inside a
`run` script those `Err`s are what the `ran N, failed M` summary counts - so a suite grades
itself. Two forms, because tests check two different things:

- **Result form** - *did a command succeed/fail?* (catches errors, incl. negative tests)
- **Content form** - *did the output contain the right thing?* (catches wrong-but-valid output)

```
gsh> assert ok read /notes.txt          # must succeed
assert: ok
gsh> assert fails read /nope            # must FAIL (a negative test)
read: not found: /nope
assert: ok
gsh> roster | where role=core | assert contains vesta
assert: ok
```

## 2. Result form - `assert ok|fails <command>`

Runs `<command>` (its own output/errors print as usual), then judges its `Result`:

- `assert ok <command>` - holds iff the command returned `Ok`.
- `assert fails <command>` - holds iff it returned `Err` (any failure).
- `assert fails-with <Variant> <command>` - holds iff it returned `Err(<Variant>)` *specifically*
  (e.g. `assert fails-with FileNotFound read /nope`, `assert fails-with Denied spawn supervisor`).

`assert fails …` is the **negative-test** surface - §22's discipline that every test has a
positive *and* a negative case, runnable on hardware: `assert fails read /nope` proves the
guardrail refuses, and `fails-with` pins *which* refusal (the variant names are in `32_result.md`:
`FileNotFound`, `Denied`, …).

## 3. Content form - `<producer> | assert <check> [text]`

The **verifying pipe sink**: it materialises the piped stream to text (a record `Table` renders
to its grid) and checks it. Because it's the last stage, *its* verdict is the pipeline's `Result`.

| Check | Holds when |
|-------|-----------|
| `contains <text>` | the output contains `<text>` |
| `lacks <text>`    | the output does **not** contain `<text>` |
| `empty`           | the output is blank |

```
roster | where role=core | assert contains vesta
ls / | assert lacks secret
find *.tmp | assert empty
```

This is what verifies *correctness*, not just absence of errors: `roster | where role=core`
returns `Ok` whether or not it found `vesta`; `| assert contains vesta` is what actually checks it.

## 4. In a script - the self-checking suite

```
# /check.gsh
assert ok    read /lsr/big.txt
assert fails read /lsr/nope
```
```
gsh> run /check.gsh
> assert ok read /lsr/big.txt
hello world
assert: ok
> assert fails read /lsr/nope
read: not found: /lsr/nope
assert: ok
run: ran 2, failed 0
```

`run: ran N, failed 0` is the green bar - the T630 telling you the suite passed, no eyeballing.

> **Authoring a suite with piped asserts.** A script line containing `|` can't be written via the
> on-device `write` (the shell pipes the `write` line itself). The answer is **host-side baking**:
> `osdev script-disk build/suite.img my_suite.gsh` produces a GSFS data disk with the suite baked
> in; `dd` it to the data drive, boot, and `run /my_suite.gsh`. `osdev test script` runs exactly
> this loop in CI (a suite full of piped asserts → `ran 6, failed 0`). Result-form asserts (no
> `|`) can also be authored on-device with `write … cmd ; cmd`.

## 5. Bounds & safety (§26.6 / §3.12)

- A failed assert is **loud** (`assert: FAILED (contains 'vesta')`) and returns `Err(AssertFailed)`
  - never a silent miss.
- The content sink materialises the stream into one bounded `Cap` (held off `pipe_run`'s frame -
  `assert_stream` is `#[inline(never)]`, the same user-stack discipline pipes/`run` need).
- The result form runs the command one nesting level deeper (`execute` is `#[inline(never)]`, so
  the frame stays shallow).

## 6. Later (separate so it can grow)

- `assert fails-with <Variant>` - pin the *specific* `Err` (needs more commands to name variants).
- More content checks (`is <text>` exact, `lines <N>`), if a suite needs them.
- Host-side **image-baked `.gsh`** - **built** (`osdev script-disk`, `osdev test script`): a suite
  of piped asserts ships on a GSFS data disk and runs from a script on hardware.

## 7. Implementation shape & conformance

`assert ok/fails` is dispatched in `execute` (it re-runs the command via `execute`); `… | assert`
is a sink recognised in `pipe_run` (which now returns the pipeline `Result`). Both print a terse
verdict via one `assert_verdict` helper. Conforms to `0_conventions.md`: own `assert help` /
`assert version` via the shared `help_block`; listed under **Console** in the top-level `help`.
