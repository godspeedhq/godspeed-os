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
SLOT  NAME               CORE STATE
0     init               C0   BlockRecv
1     supervisor         C0   BlockRecv
2     shell              C0   Running
3     registry           C0   BlockRecv
4     xhci               C1   Ready
...
```

Only slots with a live task are listed. STATE is one of Ready / Running /
BlockRecv / BlockSend / Dead.

## 4. Data source

`task_stat(slot)` for each slot 0..N (valid slots only): name, pinned core, state.
(The fuller per-task metrics - memory, queue depth, restarts, CPU% - are rendered
by `observe`; `status` is the short roster.)

## 4a. As a record producer (typed pipes)

`status` is the first **record producer** of the structured-pipe subsystem
(`docs/records.md`, `utilities/31_records.md`). Piped, it emits a typed **table** rather than the
flat console text above - columns **slot / name / core / state / mem / queue / restarts** - so the
record verbs operate on real fields:

```
status | where mem>0                  only tasks holding memory
status | where state=BlockRecv | select name core
status | sort reverse mem | to json   ordered desc, rendered as JSON
status | where name=shell | to yaml
```

The bare `status` (no pipe) still prints the short SLOT/NAME/CORE/STATE roster; the extra
columns surface only on the record path, where `select` can project whichever are wanted.

## 5. Capabilities

- **`INTROSPECT`** (READ) - `task_stat` discloses any task's state and is gated;
  the shell holds the cap.
- **Console output** to print the table.

## 6. Non-goals

- **No health verdict** (reserved for a future `status`-the-health-utility).
- **No full metrics.** Memory/queue/restart/CPU columns belong to `observe`.

## 7. Conformance

Conforms: own `status help` / `status version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services**. See `0_conventions.md` §3.
