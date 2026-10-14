# Utility: `shutdown` - turn the machine off (not provided yet: designed, being built)

**Utility:** `shutdown` - turn the machine off, now or after a delay
**Status:** Design, 2026-10-14, not built (`docs/shutdown.md`, branch `feat/shutdown`).
**Shape:** shell built-in that asks the `power` service (see `0_conventions.md` section 2).

---

## 1. Purpose

`shutdown` turns the machine off: in 3 seconds, after a delay you name, or at once. It is a polite power cut. A
short, fixed tidy-up flushes the disk and cancels a copy in flight, then the power goes off whether or not every
step answered, because GodspeedOS recovers from a power cut anyway (`docs/shutdown.md` section 2).

<!-- foreign-ok: `shutdown` is designed here and not built; it leaves the shell's FOREIGN_HINTS in step S4 -->
```text
gsh> shutdown 5
shutdown in 5s   [q] quit   [b] background
```

## 2. Invocation

<!-- foreign-ok: `shutdown` is designed here and not built; it leaves the shell's FOREIGN_HINTS in step S4 -->
```text
shutdown              shut down in 3 s
shutdown <delay>      shut down after <delay>: 5, 5s, 5m, 5h; a bare number is seconds; at most 24 h
shutdown now          at once: no countdown, no tidy-up; names a copy in flight that it will cut
shutdown status       whether one is pending, when, and how long is left
shutdown cancel       stop a pending shutdown
```
One shutdown is pending at a time; a second is refused and names the first.

## 3. Behaviour

- **Under a minute**, the countdown runs in the foreground on one line, rewritten each second, with `[q] quit` and
  `[b] background`. `q` cancels the shutdown (rule 11). `b` gives the prompt back and the shutdown goes on.
- **A minute or more** gives the prompt back at once and says when: `shutdown at 14:05 (in 5h) - shutdown cancel
  stops it` (the clock time only when `time` has one).
- **Warnings** come from `power` at 1 h, 30 m, 15 m, 5 m and 1 m before, then every second for the last 10.
- **In the last minute** no new job starts; `background` and a typed copy refuse and name the shutdown.
- **At the deadline**, about 1 s at most: a running copy is cancelled (its partial file removed), `fs` flushes and
  stops taking writes, then the power-off.
- **The last line is the truth**: `powered off`, or `halted - it is safe to switch off` where the board cannot cut
  its own power (`docs/shutdown.md` section 5).
- If `power` is not running, `shutdown` says so and nothing happens.

## 4. Output

`shutdown status` is a report: at the prompt, a sentence; piped, one record with `state` (`none`, `pending`),
`left_secs` and `at` (a clock time, or empty). `shutdown`, `shutdown now` and `shutdown cancel` are actions and
refuse a pipe with a sentence (rule 12).

## 5. Capabilities

- A send right to **`power`**, which holds the timer and the one capability that turns the machine off
  (`POWER_OFF`). The shell does not hold it.
- **Console output**, and the keys for the countdown.

## 6. Conformance

To be built to: its `help` and `version` subcommands with real examples; listed by the shell's top-level `help`
beside `reboot`; rule 10 (`[q] quit` and `[b] background` on the countdown) and rule 11 (`q` stops the shutdown).
