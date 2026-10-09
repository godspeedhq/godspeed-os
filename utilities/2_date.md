# Utility: `date`

**Utility:** `date` - wall-clock date and time
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`date` answers **what time is it?** The clock belongs to the `time` service, which
takes it from the hardware real-time clock where the board has one, from the
network (NTP) where it does not, and otherwise from the floor it last recorded on
disk; `date` asks `time` and says which of those the reading came from.

## 2. Invocation

| Command | Meaning |
|---|---|
| `date` | Full timestamp with weekday and its source, e.g. `Sat 2026-06-06 23:45:08  (rtc, scale unknown)`. |
| `date epoch` | Seconds since 1970-01-01, the number only, e.g. `1780789511`. |
| `date sync` | Ask `time` to fetch the time from the network now, wait for it (bounded at 10 s, `q` aborts), then print the timestamp. `time` also syncs on its own. |

Default = the human stamp; the word after the verb (`epoch`) picks the machine
form. The subcommand is `epoch`, **not** `unix` - GodspeedOS is not POSIX, so the
vocabulary does not borrow that name (`0_conventions.md` §1 rule 8). `epoch` says
exactly what it is: seconds since the 1970 reference point.

## 3. Output

```
gsh> date
Sat 2026-06-06 23:45:08  (rtc, scale unknown)
gsh> date epoch
1780789511
```

Date is ISO-style `YYYY-MM-DD`; time is 24-hour `HH:MM:SS`. The weekday is
**computed** from the date (Howard Hinnant's `days_from_civil`), not read from the
RTC's own weekday register, which is unreliable.

The suffix names the source, and states the scale only where it is known:

- `UTC  (ntp, synced <age> ago)` - set from the network, which serves UTC.
- `UTC  (carried over from the last boot - AT LEAST this late; 'date sync' for the true time)` -
  advanced from the floor on disk on a board with no RTC; a lower bound.
- `(rtc, scale unknown)` - the hardware clock, which firmware may keep in local time or UTC.

With no source at all `date` prints `the clock is not set: ...`, says whether the
network link is up, and shows the last recorded floor (if any) labelled as a floor,
not a reading.

## 4. Data source

- The `time` service, over IPC: `OP_NOW` returns the epoch seconds, the source and
  the sync age; `OP_SYNC` (for `date sync`) asks it to query the network. Each
  request is bounded at 2 s (`time_rpc`).
- `time` itself reads the RTC through `InspectKernel` query 11 (packed date/time,
  ungated - wall-clock time is task-neutral hardware info, like the TSC clock,
  query 3).
- SDK: `Datetime::from_epoch_secs` and `Datetime::weekday()` do the leap-year-aware
  arithmetic.

## 5. Capabilities

- **A send capability to `time`.** No kernel query is made by `date` itself.
- **`log_write`** to print the line (printing is `log_write`, not `console_push`).

## 6. Non-goals (deliberate - §26.2 minimal surface)

- **No clock-setting.** `date` reads; it never writes the RTC.
- **No format strings.** Two fixed forms only - not a formatting mini-language.
- **No timezones.** The value assumes the RTC reads UTC; if the hardware clock is
  local time, `date epoch` is offset by the timezone. v1 has no timezone database.

This keeps `date` from sprawling into "a full-blown application."

## 7. Conformance

Conforms: own `date help` / `date version` (with real examples), plus the subcommand
help `date epoch help`, per `0_conventions.md`.
