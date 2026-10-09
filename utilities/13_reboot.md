# Utility: `reboot`

**Utility:** `reboot` - hardware reset
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`reboot` restarts the whole machine - a hardware reset, not a service restart. The
sole member of the **Power** category.

## 2. Invocation

| Command | Meaning |
|---|---|
| `reboot` | Print `rebooting...` and reset the machine. |
| **Ctrl+Alt+Del** (USB keyboard, at the shell prompt) | Reset the machine, as `reboot` does. |

## 3. Behaviour

Prints a final `rebooting...` line, then invokes the `Reboot` syscall (18), which
performs a hardware reset. Does not return on success. There is no confirmation
prompt in v1 (an interactive guard could be added later). A refused reboot (a caller
without the `REBOOT` capability) is reported rather than hung on
(`ServiceContext::reboot`).

**Ctrl+Alt+Del** is routed through the shell (CLAUDE.md §6.4, SEC-2 follow-up). The
USB keyboard drivers (`xhci`, `ehci`) recognise the chord (either Ctrl + either Alt +
Delete) in the HID report with `godspeed_sdk::hid::is_ctrl_alt_del`, checked for
keyboard devices only (a mouse button byte can alias the modifier bits), and only
**signal** it: they push `hid::CTRL_ALT_DEL_SIGNAL` (`0x80`, a byte no typed key
produces) onto the console stream. The shell, which holds `REBOOT`, sees that byte at
its prompt and runs `reboot`. The drivers hold no `REBOOT` capability and cannot reset
the machine themselves. So the chord works at the shell prompt; while a full-screen app
such as `edit` owns the console the shell is not reading, so quit it first. The Pi 2's
`dwc2` driver does not signal the chord. Like the `reboot` command, it does not
prompt.

## 4. Capabilities

- **`REBOOT`** (resource 8), held by the shell; the kernel refuses the `Reboot`
  syscall (18) to anyone without it.
- **Console output** for the `rebooting...` line.

## 5. Non-goals

- **No shutdown/poweroff.** `reboot` resets - it does not power down, and there is
  no `poweroff` command: cutting power needs ACPI S5 + an AML interpreter the
  firmware's `\_PTS` gate requires (verified on hardware). See `14_poweroff.md`
  for why it was considered, built, tested, and removed.
- **No "reboot into X".** No boot-target selection - it is a plain reset. (Limine
  handles boot; `reboot` does not negotiate with it.)

## 6. Conformance

Conforms: own `reboot help` / `reboot version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Power**. See `0_conventions.md` §3.
