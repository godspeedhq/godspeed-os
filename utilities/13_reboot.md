# Utility: `reboot`

**Utility:** `reboot` - reset the machine, now or after a delay
**Status:** Built. As-built reference. The delayed forms built 2026-10-15 and QEMU-verified (`osdev test shutdown`);
hardware cards pending. Design: `docs/shutdown.md` (S6).
**Shape:** shell built-in (see `0_conventions.md` §2). The shell holds `REBOOT`; the KERNEL holds the deadline;
`power` warns and tidies up.

---

## 1. Purpose

`reboot` restarts the whole machine - a hardware reset, not a service restart. It takes the same forms as
`shutdown` (`64_shutdown.md`): in 3 seconds, after a delay you name, or at once. A countdown gets the same tidy-up a
shutdown does, so a plain `reboot` flushes the disk first, which the old immediate reset never did.

```text
gsh> reboot
reboot in 3s   [q] quit   [b] background
```

## 2. Invocation

| Command | Meaning |
|---|---|
| `reboot` | Reboot in 3 s, counting down. |
| `reboot <delay>` | Reboot after `<delay>`: 5, 5s, 5m, 5h; a bare number is seconds; at most 24h. |
| `reboot now` | Print `rebooting...` and reset at once: no countdown, no tidy-up. |
| `reboot status` | Whether one is pending, and how long is left. |
| `reboot cancel` | Stop a pending reboot. |
| **Ctrl+Alt+Del** (USB keyboard, at the shell prompt) | Reset at once, as `reboot now` does. |

## 3. Behaviour

- **The same deadline as a shutdown.** `reboot <delay>` sets the machine's one pending action in the kernel
  (`PowerOff`, syscall 56, op 3), so it survives the death of this shell, of `power`, of every service. The kernel
  logs every set and cancel with who asked, and anyone can read how long is left and that it is a reboot.
- **One deadline, one action.** While a shutdown is pending, `reboot <delay>` is refused and names it, and the
  other way round: neither can quietly replace the other. `shutdown status` and `shutdown cancel` name a pending
  reboot rather than pretending there is nothing.
- **Earliest wins.** A second `reboot` for LATER than the pending one is refused; one for sooner moves it forward.
- **Under a minute**, the countdown runs in the foreground on one line with `[q] quit` and `[b] background`. `q`
  cancels the reboot (rule 11); `b` gives the prompt back and the reboot goes on. **A minute or more** gives the
  prompt back at once.
- **Warnings and the tidy-up** come from `power`, as for a shutdown: when it is set, at 1 h, 30 m, 15 m, 5 m and
  1 m before, each of the last ten seconds, and three seconds out a copy in flight is cancelled and `fs` flushes and
  stops taking writes. Then the kernel resets the machine (`reboot: the pending reboot fell due`).
- **`jobs`** lists a pending reboot above the jobs, and in its last minute `background` refuses to start a job.
- **`reboot now`** prints a final `rebooting...` line, then invokes the `Reboot` syscall (18). Nothing goes between
  the command and the reset. A refused reboot (a caller without the `REBOOT` capability) is reported rather than
  hung on (`ServiceContext::reboot`).

**Ctrl+Alt+Del** is routed through the shell (CLAUDE.md §6.4, SEC-2 follow-up). The
USB keyboard drivers (`xhci`, `ehci`) recognise the chord (either Ctrl + either Alt +
Delete) in the HID report with `godspeed_sdk::hid::is_ctrl_alt_del`, checked for
keyboard devices only (a mouse button byte can alias the modifier bits), and only
**signal** it: they push `hid::CTRL_ALT_DEL_SIGNAL` (`0x80`, a byte no typed key
produces on its own) onto the console stream. The shell, which holds `REBOOT`, sees that byte at
its prompt and resets at once, as `reboot now` does. Only a `0x80` standing alone is the chord: inside a UTF-8
character typed over serial or pasted it is a continuation byte, and the shell decodes such a
character whole (a curly quote, `E2 80 9C`, rebooted the machine until 2026-10-11). The drivers hold no `REBOOT` capability and cannot reset
the machine themselves. So the chord works at the shell prompt; while a full-screen app
such as `edit` owns the console the shell is not reading, so quit it first. The Pi 2's
`dwc2` driver does not signal the chord. It does not prompt.

## 4. Output

Text at the prompt. `reboot status` is one sentence; the rest are actions (rule 12).

## 5. Capabilities

- **`REBOOT`** (resource 8), held by the shell alone; the kernel refuses the `Reboot` syscall (18) and the reboot
  operations of `PowerOff` (56) to anyone without it.
- **Console output**, the keys for the countdown, and a send to `power` to cue it - a cue that carries no
  authority, since `power` only re-reads the kernel's deadline.

## 6. Non-goals

- **Turning the machine off** is `shutdown` (`64_shutdown.md`), on the same deadline.
- **No "reboot into X".** No boot-target selection - it is a plain reset. (Limine
  handles boot; `reboot` does not negotiate with it.)
- **No clock time** (`reboot at 03:00`), for the reason `docs/shutdown.md` 7 gives for `shutdown`.

## 7. Conformance

Conforms: own `reboot help` / `reboot version` (with a real example, per `0_conventions.md`); listed by the shell's
top-level `help` under **Power**, beside `shutdown`; rule 10 (`[q] quit` and `[b] background` on the countdown) and
rule 11 (`q` stops the reboot). See `0_conventions.md` §3.
