# Structured records — typed pipes (PowerShell/nushell-style)

> **Status:** First slice built + QEMU-verified (`osdev test shell`): `status` emits a typed
> **table**; `where` filters it; `to json` renders it. Forward-looking design intent for the
> rest. Non-normative — does not amend `CLAUDE.md`.

## Why

POSIX pipes carry **text**. So `ls` flattens its structured data — name, size, type, date — to
a formatted string, and then `grep`/`awk`/`cut`/`sed` exist to *re-parse* that string and claw
the structure back out by counting columns. The whole `awk`/`sed`/`cut` zoo is compensation for
a lossy serialize-to-text step.

PowerShell's insight was to not throw the structure away — pipes carry objects, and you filter
on real fields (`Where-Object { $_.Length -gt 1MB }`). GodspeedOS does the same idea, but shaped
by **invariant 2 (no shared mutable memory)**: a live object cannot cross a service boundary by
reference, so the structured value must be a thing that can also be *serialized*. The model here
is therefore a **typed value**, with text/JSON as *renderings* of it — never the model itself.
(Closest prior art: **nushell** — Rust, structured pipelines, table-by-default, `to json`.)

## The three representations

1. **In-memory model** — a Rust value (today a `Table`: typed columns + rows). The "language
   between utilities". Between *built-in* stages (same address space) it is passed **by value**,
   never serialized.
2. **Wire codec** — a compact, *bounded* binary encoding, used **only** when a record must cross
   a service boundary. Not built yet (the only producer, `status`, is shell-side). It is
   emphatically **not** JSON — JSON on the wire is the slow, unbounded thing we are escaping.
3. **Edge renderers** — `to json` (built), `to yaml`, the default aligned table. These live at
   the *edge*: terminal output, or export for interop. A record never *is* JSON; JSON is one way
   to *print* it.

## The model — a bounded `Table`

The canonical value is a **table**: static column names + rows of typed `Value`
(`Str` interned in a byte arena, `Int`, …). Most introspection output is naturally tabular
(`status`, `ls`, `find`, `caps` are all uniform rows), so a table covers the realistic cases and
is simpler than arbitrary records. It is **bounded** (§26.6): `REC_MAX_COLS`, `REC_MAX_ROWS`, a
fixed `REC_ARENA` — all on the stack, no heap, loud on overflow. Heterogeneous (differently
shaped) records are a future generalization.

## Verbs

Structured pipes don't want `match` (substring on a line) — they want field operations:

- **`where <col> <op> <value>`** — keep rows where the column satisfies the test. Ops: `=`,
  `!=`, `>`, `<`, `>=`, `<=`, `~` (contains). Numeric when both sides are numbers, else textual.
  *Built.*
- **`to json`** — render as a JSON array of objects. *Built.* (`to yaml` is the obvious sibling.)
- **`select <col…>`**, **`sort by <col>`** — project columns / order rows. *Next.*

The text filters (`match`/`count`/`sort`/`first`/`last`) stay — for genuinely-text streams like
a file's contents. A pipeline is routed to the **record** path when its first stage is a record
producer (`is_record_producer`, currently `status`), else the **byte** path. The two coexist.

```
status                                   the default table
status | where state = BlockRecv         only blocked tasks
status | where mem > 0 | to json         filtered, as JSON
status | where name = shell | to json    one task, structured
```

## What's built vs next

- **Built:** the `Table` model + arena, `render_table` (default) + `render_json`, the `where`
  filter, `status` as the first record producer, and the record-pipeline dispatch — all
  in-process (no wire codec), QEMU-verified.
- **Next:** `select` / `sort by`; `to yaml`; a JSON string-escaper (task names are plain ASCII
  today); more producers (`ls`, `find`, `caps`, `observe`); then — only when a record first
  needs to cross a service boundary — the bounded wire codec; eventually heterogeneous records
  and JSON *input* (`from json`) for interop with the outside world (which also needs an
  out-channel — file export now, network far later).

## Discipline (so it doesn't rot into PowerShell-magic)

Keep the typing and serialization **explicit** (§26.5): no implicit property coercion, no
automatic `ToString()`. Keep everything **bounded** (§26.6): fixed cols/rows/arena, loud on
overflow. This is a *subsystem*, pulled into existence one producer at a time (§26.2), not a
speculative framework. The payoff is deleting a whole category of cryptic tools: keep the data
typed and most of `grep`/`awk`/`sed`/`cut` never needs to be born — replaced by
`where`/`select`/`sort by`, which read on sight.
