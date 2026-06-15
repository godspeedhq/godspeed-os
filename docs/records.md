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

## The grammar (compact, not English)

Structured pipes don't want `match` (substring on a line) — they want field operations. The
grammar is deliberately **terse and code-like**, not an English sentence:

- **Predicates are one token, operator attached:** `where mem>0`, `where state=BlockRecv`,
  `where name!=shell`, `where core>=1`. The parser finds the operator inside the token (longest
  match — `!=`/`>=`/`<=` before `=`/`>`/`<`); before it is the column, after is the value. Ops:
  `=` `!=` `>` `<` `>=` `<=` `~`(contains). Numeric when both sides are numbers, else textual.
  **No quotes needed** unless the value has a space (then the usual minimal quoting:
  `where "name=block driver"`). *Built.*
- **Column ops are verb + columns:** `select name core` (keep those columns, in order),
  `sort mem` / `sort name reverse` (order rows by a column — "by" dropped, same reason the
  spaces are). *Built.*
- **Conversions are short directional words** — `to <fmt>` / `from <fmt>`. The direction is
  named because you need both: `to` renders the model → text, `from` parses text → model. A bare
  `| json` couldn't express "parse incoming json" without the word pointing two ways (the
  implicit magic §26.5 forbids). `to json` and `to yaml` are *built*; `from json` is next.

The text filters (`match`/`count`/`sort`/`first`/`last`) stay — for genuinely-text streams like
a file's contents. A pipeline is routed to the **record** path when its first stage is a record
producer (`is_record_producer`, currently `status`), else the **byte** path. They coexist; the
default rendering (no `to`) is the table grid.

```
status                                   the default table
status | where state=BlockRecv           only blocked tasks
status | where mem>0 | select name mem   filter, then keep two columns
status | sort mem reverse | to json      ordered, as JSON
status | where name=shell | to yaml      one task, as YAML
```

## `from` — the bridge from text into the typed world

`read file.json` gives **text** (bytes); `where`/`select` need **records**. So `from json`
parses text → the table model: `read svc.json | from json | where core=1`. `read` deliberately
does *not* auto-parse by extension — that would be hidden behavior (§26.5). `from` is the
explicit bridge, and the symmetric partner of `to`:

```
read a.json | from json | to yaml | write a.yaml   convert json → yaml
read svc.json | from json | where mem>0            external data, filtered
```

This is the one piece that crosses the **byte ↔ record** boundary, so it's the next (bigger)
step: it needs the pipeline to carry *either* bytes or a table and transition between them —
the two-world unification the current split (record-path-if-first-stage-is-`status`) doesn't yet
do. `json`/`yaml`/`ini` formats then plug into the same `to`/`from` pair (though `ini` suits a
single record or key-value view, not a many-row table — json/yaml are the table-native pair).

## What's built vs next

- **Built:** the `Table` model + arena; `render_table` (default), `render_json`, `render_yaml`;
  the compact `where` predicate, `select`, and `sort <col> [reverse]`; `status` as the first
  record producer; and the record-pipeline dispatch — all in-process (no wire codec),
  QEMU-verified.
- **Next:** `from json` (parse text → records) + the byte↔record pipeline unification it needs;
  a JSON string-escaper (task names are plain ASCII today); more producers (`ls`, `find`,
  `caps`, `observe`); then — only when a record first needs to cross a *service* boundary — the
  bounded wire codec; eventually heterogeneous records and richer interop (which also needs an
  out-channel — file export now, network far later).

## Discipline (so it doesn't rot into PowerShell-magic)

Keep the typing and serialization **explicit** (§26.5): no implicit property coercion, no
automatic `ToString()`. Keep everything **bounded** (§26.6): fixed cols/rows/arena, loud on
overflow. This is a *subsystem*, pulled into existence one producer at a time (§26.2), not a
speculative framework. The payoff is deleting a whole category of cryptic tools: keep the data
typed and most of `grep`/`awk`/`sed`/`cut` never needs to be born — replaced by
`where`/`select`/`sort by`, which read on sight.
