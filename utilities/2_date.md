# Utility: `date`

**Utility:** `date` — wall-clock date and time
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`date` answers **what time is it?** from the hardware real-time clock. Two forms,
nothing more.

## 2. Invocation

| Command | Meaning |
|---|---|
| `date` | Full timestamp with weekday, e.g. `Sat 2026-06-06 23:45:08`. |
| `date epoch` | Seconds since 1970-01-01, e.g. `1780789511`. |

Default = the human stamp; the word after the verb (`epoch`) picks the machine
form. The subcommand is `epoch`, **not** `unix` — GodspeedOS is not POSIX, so the
vocabulary does not borrow that name (`0_conventions.md` §1 rule 8). `epoch` says
exactly what it is: seconds since the 1970 reference point.

## 3. Output

```
gsh> date
Sat 2026-06-06 23:45:08
gsh> date epoch
1780789511
```

Date is ISO-style `YYYY-MM-DD`; time is 24-hour `HH:MM:SS`. The weekday is
**computed** from the date (Howard Hinnant's `days_from_civil`), not read from the
RTC's own weekday register, which is unreliable.

## 4. Data source

- Kernel: the MC146818 CMOS RTC, read in the arch layer (`kernel/src/arch/x86_64/
  rtc.rs`) and exposed via `InspectKernel` query 11 (packed date/time). The read is
  **ungated** — wall-clock time is task-neutral hardware info, like the TSC clock
  (query 3) — so no capability is required.
- SDK: `ServiceContext::datetime() -> Datetime`; `Datetime::epoch_secs()` and
  `Datetime::weekday()` do the leap-year-aware arithmetic.

## 5. Capabilities

- **The clock itself: none.** Query 11 is ungated.
- **Console output** to print the line.

## 6. Non-goals (deliberate — §26.2 minimal surface)

- **No clock-setting.** `date` reads; it never writes the RTC.
- **No format strings.** Two fixed forms only — not a formatting mini-language.
- **No timezones.** The value assumes the RTC reads UTC; if the hardware clock is
  local time, `date epoch` is offset by the timezone. v1 has no timezone database.

This keeps `date` from sprawling into "a full-blown application."

## 7. Conformance

Conforms: own `date help` / `date version` (with real examples), plus the subcommand
help `date epoch help`, per `0_conventions.md`.
