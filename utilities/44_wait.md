# Utility: `wait`

**Utility:** `wait` - do nothing for N seconds, abortable with `q`
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

---

## 1. Purpose

`wait` pauses for N wall-clock seconds. It is the shell's **pacing primitive**: the thing a
script (or a person) uses to space repeated work out in time. The library's `watch` loop is
built on it - `loop { <cmd>; if !wait 2 { break } }` - which is exactly why it exists
(pulled into existence by `watch`, §26.2, not added speculatively).

The name is the plain English of what it does. It is **not** POSIX `wait` (wait for a child
process) - GodspeedOS has no fork and no child processes; here `wait 2` reads as the sentence
it is.

## 2. Invocation

| Command | Meaning |
|---|---|
| `wait <seconds>` | Pause for N seconds (1..3600). `q`/`Q`/`Esc` aborts with `Err`. |

A wait longer than 3600 s is refused loudly - a pacing pause is seconds-to-minutes; an
hour-plus wait at the prompt is a typo, not a plan (§26.6 bounded).

## 3. Behaviour

- **Whole-second granularity.** Paced by the deglitched monotonic RTC second (the same clock
  `uptime` and `ping` pace by) - never the TSC, whose wall-time calibration is wrong on some
  hardware (the T630). `wait 1` ends at the next second boundary, so it can return up to a
  second early, never late.
- **`q` aborts, and the abort is an `Err`.** That is what makes it compose:
  `if !wait 2 { break }` ends a watch loop the instant the user quits. At the prompt, an
  aborted `wait` shows as `Err` in `result`.
- **Idles politely.** The pause yields between clock checks; it does not spin a core hot.

## 4. Commandment VIII note (read this before imitating)

`wait` is a **user-commanded** delay: the user (or their script) chose the cadence and holds
the `q` escape. That is policy in the user's hands, not a service coordinating with a peer -
so "wait on truth, not time" is not violated *here*. Services must still never pace their
dependencies with a timer: a service that sleeps instead of blocking on its dependency's
reply is the exact bug Commandment VIII exists to forbid.

## 5. Capabilities

- **Clock read** (the RTC epoch via the kernel, as `date`/`uptime` already use).
- **Console input** (the `q` poll) + console output for usage errors.

## 6. Conformance

Conforms: `wait help` / `wait version` (`0_conventions.md` rules 1-6); listed by the shell's
top-level `help` under **System**; in `NO_PATH_CMDS` (its argument is a number - Tab never
lists files). Pinned by `selfcheck` (ok on `wait 1`, refusals on `wait`, `wait 0`,
`wait 99999`) and a shell-test q-abort check.
