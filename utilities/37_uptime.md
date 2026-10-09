# Utility: `uptime`

**Utility:** `uptime` - how long the system has been up
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`uptime` answers **how long has the system been running since boot?** It reports a
human-readable elapsed time (`Nd HH:MM:SS`) and the raw total in seconds. It is a **wall-clock
delta** - the kernel records the RTC time at boot, and `uptime` subtracts it from the current
time on the kernel's monotonic clock. On a board with no RTC the monotonic clock already counts
from boot, so it is the uptime directly. This is portable and accurate regardless of the APIC
timer mode (a raw timer-tick
counter is *not*: it runs at ~100 Hz on TSC-deadline hardware but ~10 Hz under QEMU's periodic
timer, so ticks→seconds would be platform-dependent).

## 2. Invocation

| Command | Meaning |
|---|---|
| `uptime` | Render a one-row grid: `uptime` (`Nd HH:MM:SS`) + `seconds`. |
| `uptime \| to json` / `to yaml` | The same row rendered as JSON / YAML. |
| `uptime \| select seconds` | Just the total-seconds column (records pipeline). |
| `uptime help` / `uptime version` | Self-documentation (`0_conventions.md`). |

`uptime` is a **record producer** (`docs/records.md`): bare it prints the grid; piped it emits
a typed `Table`, so every record verb works (`where`, `select`, `sort`, `to`).

## 3. Output

```
gsh> uptime
uptime       seconds
0d 00:03:21  201

gsh> uptime | to yaml
- uptime: 0d 00:03:21
  seconds: 201
```

## 4. Data source

`ServiceContext::uptime_secs()` (through `gs::task::uptime_secs`) = `epoch_secs_monotonic()` −
`boot_datetime().epoch_secs()`, where:
- `epoch_secs_monotonic()` → `InspectKernel` query 17: the RTC's epoch seconds with backward and
  wild-forward readings dropped on x86, and seconds since boot on the boards with no RTC; and
- `boot_datetime()` → `InspectKernel` query 12 (`rtc::boot_datetime()`): the RTC time captured
  once at boot.

A boot datetime before 2001 (a board with no RTC reports zeros) means the monotonic value is
used as-is. The result never goes below 0. Chosen over the kernel's tick counter
because that counter's *rate* is not portable (TSC-deadline HW ticks at 100 Hz; QEMU's periodic
timer at ~10 Hz), which would make a ticks→seconds conversion platform-dependent.

## 5. Capabilities

- **None gating the read.** Queries 12 and 17 are **ungated** - uptime is task-neutral
  hardware-ish info, like the TSC (query 3) and RTC (query 11). No `INTROSPECT` cap required.
- **Console output** to print the grid.

## 6. Non-goals

- **No load averages.** Unix `uptime` also prints 1/5/15-minute load; this OS has no such
  metric (per-core CPU% lives in `observe`). `uptime` is elapsed time only (§26.2).
- **No wall-clock date.** That is `date` (the hardware RTC). `uptime` is *elapsed since boot*.
- **No POSIX vocabulary.** It happens to share the Unix name, but it is its own thing.

## 7. Conformance

Conforms: own `uptime help` / `uptime version` (with a real example, per `0_conventions.md`);
listed by the shell's top-level `help` under **System**; record-producer behaviour per
`docs/records.md`. See `0_conventions.md` §3.
