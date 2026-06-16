# Utility: `caps`

**Utility:** `caps` — capability viewer
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`caps` answers **what authority does a service hold?** — it lists the capabilities
in a task's table. Authority in GodspeedOS is explicit and inspectable (§26.9); this
is the command that surfaces it.

## 2. Invocation

| Command | Meaning |
|---|---|
| `caps` | List **this shell's** own held capabilities (default). |
| `caps <service>` | List the named service's held capabilities. |

No argument shows the shell itself — a service can inspect its own authority like
any other (authority is explicit, not hidden).

## 3. Output

```
gsh> caps
caps for shell
  resource 2  spawn            rights: WRITE
  resource 5  introspect       rights: READ
  resource 6  service_control  rights: WRITE
  ...
```

Each row is one held cap: the resource it targets and its rights bitfield (READ,
WRITE, SEND, RECV, GRANT, REVOKE). Stable kernel resources have well-known ids
(1=log_write, 2=spawn, 3=console_read, 4=console_push, 5=introspect,
6=service_control); larger ids are IPC endpoints or other grants.

## 3a. As a record producer (typed pipes)

Bare `caps` prints the list above; **in a pipe** it is a record producer
(`docs/records.md`, `utilities/31_records.md`) emitting a typed table with columns
**`resource`** (the target name) and **`rights`** (the spelled-out right words), so authority
is queryable as data:

```
caps logger | where rights~send     services logger can send to
caps | where resource=spawn         does this shell hold spawn? (one row if yes)
caps shell | select resource        just the resources, no rights column
caps logger | to json               logger's authority as JSON
```

## 4. Data source

`task_caps(slot)` / `query_cap_rights` over the introspection surface, resolving the
service name to its scheduler slot first. The record form (`build_caps_table`) decodes the
same `task_caps` reply into `resource`/`rights` rows.

## 5. Capabilities

- **`INTROSPECT`** (READ) — reading another task's cap table is gated; the shell
  holds the cap.
- **Console output** to print the list.

## 6. Non-goals

- **No editing.** `caps` reads authority; it never grants, narrows, or revokes.
  Mutating authority is the kernel's job, driven by contracts and IPC transfer
  (§7.6), never by a viewer.

## 7. Conformance

Conforms: own `caps help` / `caps version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services** as `caps [service]`. See `0_conventions.md` §3.
