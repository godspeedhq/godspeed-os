# Utility: `status`

**Utility:** `status` - live task list
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`status` answers **what tasks are alive right now, and where?** - a compact table of
every live scheduler slot. It reports raw facts and renders no health verdict
(`0_conventions.md` §1 rule 7).

> **Naming note.** The `observe` spec (`1_observe.md` §9) reserves the *name*
> `status` for a future **health-verdict** utility ("is everything OK?"). The
> current `status` command is a raw task list, closer to a one-shot task table. If
> the health utility is built, expect this command's name/role to be revisited so
> the two stay distinct (raw facts vs verdict).

## 2. Invocation

| Command | Meaning |
|---|---|
| `status` | Print the live-task table and return. |

## 3. Output

```
gsh> status
slot  name          core  state      mem     queue  restarts
0     supervisor    0     BlockRecv  131072  0      0
1     events        0     BlockRecv  65536   0      0
2     shell         0     Running    262144  0      0
3     xhci          1     Ready      98304   0      0
...
```

(Slot numbers and values illustrative.) Every valid scheduler slot is listed, at
most `REC_MAX_ROWS` (a longer table ends with `status: more than N rows shown
(bounded)`). STATE is one of Ready / Running / BlockRecv / BlockSend / Dead; `mem`
is bytes.

## 4. Data source

`task_stat(slot)` for each slot 0..255 (valid slots only): name, pinned core, state,
memory in use, queue depth and restart count. (CPU% is `observe`'s.)

## 4a. As a record producer (typed pipes)

`status` is the first **record producer** of the structured-pipe subsystem
(`docs/records.md`, `utilities/31_records.md`). Bare or piped it is the same typed **table** -
columns **slot / name / core / state / mem / queue / restarts** - so the record verbs operate on
real fields:

```
status | where mem>0                  only tasks holding memory
status | where state=BlockRecv | select name core
status | sort reverse mem | to json   ordered desc, rendered as JSON
status | where name=shell | to yaml
```

The bare `status` renders that table as a grid (`build_status_table`, then `to_grid`), so the
console and the pipe show the same columns; `select` projects whichever are wanted.

## 5. Capabilities

- **`INTROSPECT`** (READ) - `task_stat` discloses any task's state and is gated;
  the shell holds the cap.
- **Console output** to print the table.

## 6. Non-goals

- **No health verdict** (reserved for a future `status`-the-health-utility).
- **No full metrics.** CPU% and the live view belong to `observe`.

## 7. Conformance

Conforms: own `status help` / `status version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services**. See `0_conventions.md` §3.
