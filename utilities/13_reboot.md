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
| **Ctrl+Alt+Del** (USB keyboard) | Reset the machine immediately, from any context. |

## 3. Behaviour

Prints a final `rebooting...` line, then invokes the `Reboot` syscall (18), which
performs a hardware reset. Does not return. There is no confirmation prompt in v1
(an interactive guard could be added later).

**Ctrl+Alt+Del** is a hardware *secure-attention* reset: the USB keyboard drivers
(`xhci`/`ehci`) recognise the chord (either Ctrl + either Alt + Delete) directly in
the HID report and invoke the same `Reboot` syscall — so it works from *any* context,
including inside a full-screen app like `edit` or at a wedged prompt, not just at the
shell. Detection is `godspeed_sdk::hid::is_ctrl_alt_del`, checked per poll for keyboard
devices only (a mouse button byte can alias the modifier bits). Like the `reboot`
command, it does not prompt — it resets immediately.

## 4. Capabilities

- **Console output** for the `rebooting...` line, then the `Reboot` syscall.

## 5. Non-goals

- **No shutdown/poweroff.** `reboot` resets — it does not power down, and there is
  no `poweroff` command: cutting power needs ACPI S5 + an AML interpreter the
  firmware's `\_PTS` gate requires (verified on hardware). See `14_poweroff.md`
  for why it was considered, built, tested, and removed.
- **No "reboot into X".** No boot-target selection — it is a plain reset. (Limine
  handles boot; `reboot` does not negotiate with it.)

## 6. Conformance

Conforms: own `reboot help` / `reboot version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Power**. See `0_conventions.md` §3.
