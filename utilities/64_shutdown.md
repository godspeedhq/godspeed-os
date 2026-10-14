# Utility: `shutdown` - turn the machine off

**Utility:** `shutdown` - turn the machine off, now or after a delay
**Status:** Built 2026-10-14, QEMU-verified (`osdev test shutdown`); hardware cards pending. Design: `docs/shutdown.md`.
**Shape:** shell built-in. The shell holds `POWER_OFF`; the KERNEL holds the deadline; `power` warns and tidies up.

---

## 1. Purpose

`shutdown` turns the machine off: in 3 seconds, after a delay you name, or at once. It is a polite power cut. In
its last three seconds `power` cancels a copy in flight and has `fs` flush and stop taking writes, then the power goes off
whether or not every step answered, because GodspeedOS recovers from a power cut anyway (`docs/shutdown.md` 2).

```text
gsh> shutdown 5
shutdown in 5s   [q] quit   [b] background
```

## 2. Invocation

```text
shutdown              shut down in 3 s
shutdown <delay>      shut down after <delay>: 5, 5s, 5m, 5h; a bare number is seconds; at most 24h
shutdown now          at once: no countdown, no tidy-up
shutdown status       whether one is pending, and how long is left
shutdown cancel       stop a pending shutdown
```

## 3. Behaviour

- **The deadline is the kernel's.** `shutdown` sets the machine's one pending power-off in the kernel (`PowerOff`,
  syscall 56), so it survives the death of this shell, of `power`, of every service; and it is gone at a reboot. The
  kernel logs every set and cancel with who asked, and anyone can read how long is left.
- **Earliest wins.** A second `shutdown` for LATER than the pending one is refused and names it - nothing can
  postpone a shutdown already set. One for sooner moves it forward.
- **Under a minute**, the countdown runs in the foreground on one line, rewritten each second, with `[q] quit` and
  `[b] background`. `q` cancels the shutdown (rule 11). `b` gives the prompt back and the shutdown goes on.
- **A minute or more** gives the prompt back at once: `shutdown in 5m - shutdown cancel stops it`.
- **Warnings** come from `power`: when it is set, at 1 h, 30 m, 15 m, 5 m and 1 m before, then each of the last ten
  seconds. A restarted `power` reads the kernel's deadline and carries on.
- **`jobs`** lists a pending shutdown above the jobs.
- **In the last minute** no new job starts: `background` refuses and names the shutdown.
- **`shutdown now`** warns that a running job will be cut, then turns the machine off.
- **The last line is the truth**: the machine is off, or the kernel says `every core is halting - it is safe to
  switch the machine off` where the board cannot cut its own power (`docs/shutdown.md` 5).

## 4. Output

Text at the prompt. `shutdown status` is one sentence; the rest are actions (rule 12).

## 5. Capabilities

- **`POWER_OFF`**, which this shell alone holds, beside `REBOOT`: ending the session is the operator's. A second
  holder - a power button - is a decision recorded in `COMMANDMENTS.baseline.toml`, never a door.
- **Console output**, the keys for the countdown, and a send to `power` to cue it - a cue that carries no
  authority, since `power` only re-reads the kernel's deadline. It also says whether `copier` is running, so the
  tidy-up asks for it only when a copy can be in flight.

## 6. Conformance

Conforms: `shutdown help` and `shutdown version` with real examples; listed by the shell's top-level `help` under
**Power**, beside `reboot`; rule 10 (`[q] quit` and `[b] background` on the countdown) and rule 11 (`q` stops the
shutdown). `halt` and `poweroff` are hints that answer "try `shutdown`".
