# Utilities: `where` / `select` / `to` / `from` — the record-pipe verbs

**Status:** **Built + QEMU-verified** (`osdev test shell`). Four shell built-in pipe stages that
operate on **typed records** (a `Table`), not text. They are the user-facing surface of the
structured-pipe subsystem; the design rationale and the model live in `docs/records.md`. Trails
`CLAUDE.md`; does not amend it.

> `sort` is documented in `29_sort.md` — it spans **both** worlds (line-sort on text, column-sort
> on records), so it lives with the text filters and is only cross-referenced here.

---

## 1. What they are — and why a fresh family

POSIX pipes carry **text**, so the structure a producer knew (`ls` knew name/size/type) is
flattened to a string and `grep`/`awk`/`cut`/`sed` exist to claw it back. GodspeedOS keeps the
structure: a record producer (today `status`) emits a typed **table**, and these verbs operate on
real fields. The names say what they do with no POSIX heritage to learn (§ conventions rule 8):

- **`where`** — keep the rows whose field matches a predicate.
- **`select`** — keep only some columns, in order.
- **`to`** — render the records to a format (`json`, `yaml`) for the terminal or export.
- **`from`** — parse text *into* records (the bridge from the byte world).

They are **pipe-only stages** — there is no `where /file`. They appear in a pipeline after a
record producer (`status | where …`, `ls | where …`) or after `from`
(`read x.json | from json | where …`). The record producers so far are all shell-side:
**`status`** (task roster, `slot`/`name`/`core`/`state`/`mem`/`queue`/`restarts`), **`ls`**
(`name`/`type`/`size`), **`caps`** (`resource`/`rights`), **`drives`**
(`index`/`label`/`status`/`size_mib`/`free_mib`), and **`find`** (`name`/`type`/`path`).
`observe` is next but lives in a separate service, so it waits on the bounded wire codec
(`docs/records.md`).

## 2. Usage

```
where  0.1.0 — keep records whose field matches
select 0.1.0 — keep only some columns, in order
to     0.1.0 — render records to a format
from   0.1.0 — parse text into records

usage:
  <records> | where <col><op><val>   ops: = != > < >= <= ~ (contains)
      e.g. status | where mem>0
  <records> | select <col> [col…]    project the named columns, in order
      e.g. status | select name core state
  <records> | to <json|yaml>         render to JSON / YAML at the edge
      e.g. status | where mem>0 | to yaml
  <text>    | from <json>            parse text → records (the bridge)
      e.g. read /svc.json | from json | where core=1
  <verb> version                     print the version
  <verb> help                        print this message
```

## 3. Behaviour

### `where <col><op><val>` — the compact predicate
The predicate is **one token, operator attached** (no spaces, no quotes unless the value has a
space): `where mem>0`, `where state=BlockRecv`, `where name!=shell`, `where core>=1`. The parser
finds the operator inside the token by **longest match** (`!=` `>=` `<=` before `=` `>` `<`):
before it is the column, after is the value. Ops: `=` `!=` `>` `<` `>=` `<=` `~` (contains). The
comparison is **numeric** when both sides parse as numbers, else **textual**. An unknown column is
a loud error naming the available columns (§3.12).

### `select <col> [col…]` — projection
Keep only the named columns, in the order given (`select name core state` reorders too). An
unknown column is a loud error.

### `to <json|yaml>` — edge renderers
`to json` emits a JSON array of objects; `to yaml` emits a YAML list of mappings. These live at
the **edge** of the pipeline — terminal output or, piped to `write`, export. A record never *is*
JSON; JSON is one way to *print* it. With no `to`, the default rendering is the aligned table grid.

### `from <json>` — the bridge into the typed world
`read` gives **text**; `where`/`select` need **records**. `from json` parses a flat
`[ {…}, … ]` array (string / number / `true`|`false` / `null`, no nesting; the first object
defines the columns) into the table model. `read` deliberately does *not* auto-parse by extension
— that would be hidden behavior (§26.5); `from` is the explicit, symmetric partner of `to`. This
crosses the **byte ↔ record** boundary in the unified pipeline (`docs/records.md`).

```
status | where state=BlockRecv | select name core
status | where mem>0 | sort reverse mem | to json
read /svc.json | from json | where core=1 | to yaml | write /c1.yaml
```

## 4. Implementation

Pipe stages in the unified dispatcher (`pipe_transform`): one pipeline threads a
`Stream = Bytes(Cap) | Table(Table)`, and each stage is chosen by its command **and** by which it
currently holds — so `from`/`to` flip between the two worlds and `where`/`select` only accept a
`Table`. A mismatch (e.g. `where` on raw bytes) is a loud error pointing at the fix. The model is
**bounded** (§26.6): `REC_MAX_COLS` (8), `REC_MAX_ROWS` (64), a fixed `REC_ARENA` (4096) for
interned strings, `REC_COL_NAME` (24) — all on the stack, no heap, loud on overflow. No new `fs`
or kernel surface: these operate on data already in the pipeline.

## 5. Later (separate so it can grow)

- `observe` as a record producer — the one that pulls the bounded wire codec into existence
  (its data lives in a separate service). `status`/`ls`/`caps`/`drives`/`find` are done.
- `from yaml`; a JSON string-escaper (values are plain ASCII today).
- The bounded **wire codec** — only when a record first needs to cross a *service* boundary
  (today every producer is shell-side, so records pass by value). Emphatically not JSON on the
  wire (`docs/records.md`).
- Heterogeneous (differently-shaped) records.

## 6. Conformance

All four conform to `0_conventions.md`: each has its own `where`/`select`/`to`/`from` `help`
(usage with a real example per row) and `version` (number + creator credit), via the shared
`help_block` helper. Being pipe-only stages, they have no direct-file form; the help makes the
`<records> |` / `<text> |` prefix explicit so the grammar is unambiguous.
