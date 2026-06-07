# Utility: `reboot`

**Utility:** `reboot` — hardware reset
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`reboot` restarts the whole machine — a hardware reset, not a service restart. The
sole member of the **Power** category.

## 2. Invocation

| Command | Meaning |
|---|---|
| `reboot` | Print `rebooting...` and reset the machine. |

## 3. Behaviour

Prints a final `rebooting...` line, then invokes the `Reboot` syscall (18), which
performs a hardware reset. Does not return. There is no confirmation prompt in v1
(an interactive guard could be added later).

## 4. Capabilities

- **Console output** for the `rebooting...` line, then the `Reboot` syscall.

## 5. Non-goals

- **No shutdown/poweroff.** v1 has no ACPI power-off path; `reboot` resets, it does
  not power down.
- **No "reboot into X".** No boot-target selection — it is a plain reset. (Limine
  handles boot; `reboot` does not negotiate with it.)

## 6. Conformance

Built-in: no `reboot help` / `reboot version` yet; listed by the shell's top-level
`help` under **Power**. See `0_conventions.md` §3.
