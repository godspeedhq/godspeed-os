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

1. **In-memory model** — a Rust value, the `Table` (typed columns + rows). The "language between
   utilities", and it lives in the **SDK** (`godspeed_sdk::record`) so *any* service can build,
   filter, and render records — not just the shell. Between same-address-space stages it is
   passed **by value**, never serialized.
2. **Wire codec** — a compact, *bounded* binary encoding, used when a record crosses a **service**
   boundary as records. **Built** (`Table::encode`/`decode` in the SDK): magic + dims + column
   names + tagged cells, all bounded by the table's own limits, loud on a truncated/oversized
   buffer. It is emphatically **not** JSON — it is the `Table` itself on the wire. A record
   *service* (`examples/roster`) emits this; the shell decodes it straight into a `Table` with no
   `from json`. (JSON is still available for human/interop output via `to json`, and `from json`
   for parsing external JSON text.)
3. **Edge renderers** — `to json`, `to yaml`, the default aligned grid (all built, in the SDK).
   These live at the *edge*: terminal output, or export for interop. A record never *is* JSON;
   JSON is one way to *print* it.

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
  `sort mem` / `sort reverse name` (order rows by a column — "by" dropped, same reason the
  spaces are; `reverse` *leads*, like `match except`, so the column lands at the end and nothing
  dangles). *Built.*
- **Conversions are short directional words** — `to <fmt>` / `from <fmt>`. The direction is
  named because you need both: `to` renders the model → text, `from` parses text → model. A bare
  `| json` couldn't express "parse incoming json" without the word pointing two ways (the
  implicit magic §26.5 forbids). `to json` and `to yaml` are *built*; `from json` is next.

The text filters (`match`/`count`/`sort`/`first`/`last`) stay — for genuinely-text streams like
a file's contents. A pipeline is routed to the **record** path when its first stage is a record
producer (`is_record_producer` — `status`, `ls`, `caps`, `drives`, `find`, `observe now`), else
the **byte** path. They coexist; the default rendering (no `to`) is the table grid. A *text*
filter applied to a record stream (e.g. `ls | match foo`) is a loud, guided error — use
`where`/`select`/`sort <col>`, or `to json` to drop back to text first.

**A live, screen-owning loop cannot be a pipe stage.** `observe` (bare) is the continuous
full-screen view — it owns the console and never yields a discrete stream, so piping it would
block the shell on a `recv` that never comes (the stage-1 hazard `docs/pipes.md` guards). Only
its one-shot snapshot, **`observe now`**, is pipeable; bare `observe | …` is a loud refusal, not
a hang. This is the general rule for any future live-loop utility: the *snapshot* form is the
record producer, the *live* form is not.

```
status                                   the default table
status | where state=BlockRecv           only blocked tasks
status | where mem>0 | select name mem   filter, then keep two columns
status | sort reverse mem | to json       ordered desc, as JSON
status | where name=shell | to yaml      one task, as YAML
```

## A service producing records — the `roster` example

`examples/roster` is the executable proof that records are not shell-only: it builds a `Table`
with the SDK, **`encode`s it with the binary wire codec**, and emits the bytes through the
shell-delegated pipe cap (zero ambient authority, like `greet`). The shell knows `roster` is a
record service, so it `decode`s the stream straight into a `Table` — **no `from json`**:

```text
gs> roster | sort reverse core
name    role     core
hermes  courier  2
atlas   worker   1
vesta   core     0
gs> roster | where role=core | to json
[
  {"name": "vesta", "role": "core", "core": 0}
]
gs> roster | select name core
name    core
atlas   1
hermes  2
vesta   0
```

This is the whole point of the SDK record API: a third-party service emits typed data *as
records*, and the verbs (`where`/`select`/`sort`/`to`) operate on it directly — no text, no
re-parsing. (`osdev test files` pins it.)

## `from` — the bridge from text into the typed world

`read file.json` gives **text** (bytes); `where`/`select` need **records**. So `from json`
parses text → the table model: `read svc.json | from json | where core=1`. `read` deliberately
does *not* auto-parse by extension — that would be hidden behavior (§26.5). `from` is the
explicit bridge, and the symmetric partner of `to`:

```
read a.json | from json | to yaml | write a.yaml   convert json → yaml
read svc.json | from json | where mem>0            external data, filtered
```

This crosses the **byte ↔ record** boundary, and that unification is **built**: one pipeline
threads a `Stream` that is *either* `Bytes` (a text buffer) or a `Table` (records), and each
stage is dispatched by its command **and** by which it currently holds — so `sort` is a
line-sort on `Bytes` and a column-sort on a `Table`, and `from`/`to` flip between the two. A
mismatch (e.g. `where` on bytes, `match` on a table) is a loud error pointing at the fix. `from
json` parses a flat `[ {…}, … ]` array (string/number/`true`|`false`/`null`, no nesting; the
first object defines the columns). `ini` could plug into the same `to`/`from` pair later, but it
suits a single record or key-value view, not a many-row table — json/yaml are the table-native
pair.

## What's built vs next

- **Built:** the `Table` model (owned column names + arena); `render_table` (default, full
  string cells — no clipping), `render_json`, `render_yaml`; the compact `where`, `select`,
  `sort [reverse] <col>`; **six shell-side record producers — `status` (task roster),
  `ls` (`name`/`type`/`size`), `caps` (`resource`/`rights`), `drives`
  (`index`/`label`/`status`/`size_mib`/`free_mib`), `find` (`name`/`type`/`path`), and
  `observe now` (the roster + a `ticks` cumulative-cpu-time column — the native "top",
  `observe now | sort reverse ticks`)**; **`from json`** (text → records); and the **unified
  byte↔record pipeline** (`Stream = Bytes | Table`, dispatched by command + data type, `from`/`to`
  bridging). All in-process (no wire codec), QEMU-verified incl. a json → records → yaml → file
  round-trip and `ls | where type=file | sort reverse size`.
- **Also built:** the **SDK record API** — the `Table` model, `where`/`select`/`sort`,
  `to_json`/`to_yaml`/`to_grid` (over a `RecordSink`), `from_json`, **and the binary wire codec
  (`encode`/`decode`)** now live in `godspeed_sdk::record`, so any service can produce records,
  render them, or put them on the wire as records (`sdk/rust/CLAUDE.md`). `examples/roster` is a
  record *service*: it `encode`s a `Table`, the shell `decode`s it — `roster | where role=core`,
  no `from json`.
- **Next:** a JSON string-escaper (values are plain ASCII today); `from yaml`; record streams
  larger than one IPC message (chunked `encode` + the streaming-pipe work); eventually
  heterogeneous records and richer interop.

## Discipline (so it doesn't rot into PowerShell-magic)

Keep the typing and serialization **explicit** (§26.5): no implicit property coercion, no
automatic `ToString()`. Keep everything **bounded** (§26.6): fixed cols/rows/arena, loud on
overflow. This is a *subsystem*, pulled into existence one producer at a time (§26.2), not a
speculative framework. The payoff is deleting a whole category of cryptic tools: keep the data
typed and most of `grep`/`awk`/`sed`/`cut` never needs to be born — replaced by
`where`/`select`/`sort by`, which read on sight.
