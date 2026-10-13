# 84. Code defects found by the second 2026-10-12 `feat/ask` documentation audit

**Status: OPEN, 2026-10-12.** The second audit of `feat/ask` covered everything after `9ce5c479` and
changed documents and comments only. The code defects it found are listed here, to be fixed separately,
as for `backlog/fixed/80` and `backlog/82`. The defects it found in the uncommitted `xhci` hub cut
(`backlog/83`) are not here: that work is fixed before it is committed.

## The counter and the binding gen

- **C1. `counter_ms` stops advancing after about 10 days on x86.** `stdlib/rust/src/driver/wait.rs`:
  `counter_ms` is `us_for(per_10ms, read_tsc()) / 1000`, and `us_for` computes
  `ticks.saturating_mul(10_000) / per_10ms`. On a 2 GHz TSC the product saturates at about 1.8e15 ticks,
  about 10.7 days, and `counter_ms` is a constant from then on. `usbfn::next_gen` then gives every
  restarted `xhci` the same first gen, so `wifi-usb` again cannot tell a fresh binding, which is the
  defect `2e3c4052` fixed. On the T630 and the Wyse that happens after about 10 days of uptime. The Pi 4's
  54 MHz counter takes about a year to get there, and the Pi 2's 1 MHz one never does. `Deadline` uses
  `us_for` on differences, not on an absolute reading; whether any of those can grow as large is not
  checked. Fix: `ticks / per_10ms * 10_000 + (ticks % per_10ms) * 10_000 / per_10ms`, or 128-bit
  arithmetic.
- **C2. `next_gen` repeats across a restart where the counter is uncalibrated.** Both instances read 0,
  so each counts 1, 2, 3 ... again. At `last == u32::MAX` it returns 1, and the counter's low 32 bits wrap
  every ~49.7 days. The doc now says so. A gen that a restart cannot repeat needs a source that is not the
  counter there, for example one the supervisor hands out.

## `dwc2`

- **C3. The no-disk drain tests `MSG_DRAIN_MAX` after `try_recv`** (`services/dwc2/src/main.rs`, the
  no-disk loop). It answers before it breaks, so nothing is lost; it is only unlike the two loops
  `backlog/57` made test their bound before taking.

## Shell

- **S1. `repeat` runs its command at script depth even at the prompt** (`cmd_repeat`, `execute(...,
  depth + 1, ...)`). `run`, `selfcheck` and every library script are refused with "inside a script",
  which the operator did not type, and `help` and `docs` print rather than open. If the depth is needed
  (a recursion bound), the refusal should say "inside `repeat`".
- **S2. `cmd_repeat` writes to the console, not to its caller's output.** It uses `gs::io` and
  `Out::Console` and ignores the `out` it was given, so inside a captured run (`selfcheck save <path>`)
  its announcements, its runs and its verdict go to the screen and not to the capture.
- **S3. Typed keys are dropped.** With no `wait`, `repeat` reads one console byte between runs and
  drops it unless it is `q`. `pause_quittable` (`wait`, and `repeat`'s pause) drops every key that is not
  `q`. Neither hands it to `keep_key`, so a command typed ahead is lost.
- **S4. `kill all-services` drops names past `ALL_SERVICES_MAX - 2` without a word**
  (`n < segs.len() - 2`). A `MANAGED` set that outgrew 32 would leave services running and say nothing.
  It should refuse, or say how many it could not hold.
- **S5. `ask`'s "stopped" lines fire when `ask` did not stop.** `drain_service_with` returns true after
  the operator's `q` ("pipe: aborted") and after the shell's own 25 s bound (no EOT, and the shell kills
  `ask`), so `build_ask_table` prints "`ask` stopped before it replied" for a `q` and for a slow answer.
  The drain's outcome should reach it.
- **S6. Some request waits print no `[q] quit`** (the standing rule of 2026-10-12): `fs_request`,
  `fs_raw` (`drives`, `drives flash`, `reset`, `label`) and `ns_abortable` use plain
  `request_with_reply_abortable`, and the tagged re-waits in `fs_take_tagged` and `ns_take_tagged` use a
  bare `recv_abortable_deadline`. They are quittable, but do not say so.

## `ask`

- **A1. `Facts::tick` is dead.** `services/ask/src/main.rs` computes it and nothing reads it since
  `joke_index` picks from `now`.

## Next step

C1 first: it brings back, after ten days, a defect hardware already found.
