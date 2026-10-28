# 84. Code defects found by the second 2026-10-12 `feat/ask` documentation audit

**Status: OPEN for C2's cards only - C2 FIX BUILT 2026-10-28**, QEMU-verified (below, "C2 built"); awaiting the boards that run `xhci`, `dwc2` and `ehci`. *Was:* OPEN for C2 only, 2026-10-13. C1, C3, S1-S6 and A1 are FIXED in code on `feat/ask`
(2026-10-13), each noted below; verified by the host tests (C1 has a test that fails under the old
arithmetic), `ask`'s 41 host tests and `osdev test shell` 272/0 in QEMU. C2 needs a decision: where a
gen that no restart can repeat comes from on a machine whose counter is uncalibrated.

*Was:* OPEN, 2026-10-12. The second audit of `feat/ask` covered everything after `9ce5c479` and
changed documents and comments only. The code defects it found are listed here, to be fixed separately,
as for `backlog/fixed/80` and `backlog/fixed/82`. The defects it found in the uncommitted `xhci` hub cut
(`backlog/fixed/83`) are not here: that work is fixed before it is committed.

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
  **FIXED 2026-10-13:** `us_for` divides before it multiplies, exact for any reading
  (`elapsed_does_not_stop_after_ten_days`); the doc caveats it put in `usbfn` and `wait` are gone.
- **C2. `next_gen` repeats across a restart where the counter is uncalibrated.** Both instances read 0,
  so each counts 1, 2, 3 ... again. At `last == u32::MAX` it returns 1, and the counter's low 32 bits wrap
  every ~49.7 days. The doc now says so. A gen that a restart cannot repeat needs a source that is not the
  counter there, for example one the supervisor hands out.

## `dwc2`

- **C3. The no-disk drain tests `MSG_DRAIN_MAX` after `try_recv`** (`services/dwc2/src/main.rs`, the
  no-disk loop). It answers before it breaks, so nothing is lost; it is only unlike the two loops
  `backlog/57` made test their bound before taking.
  **FIXED 2026-10-13:** it tests before taking, like the other two.

## Shell

- **S1. `repeat` runs its command at script depth even at the prompt** (`cmd_repeat`, `execute(...,
  depth + 1, ...)`). `run`, `selfcheck` and every library script are refused with "inside a script",
  which the operator did not type, and `help` and `docs` print rather than open. If the depth is needed
  (a recursion bound), the refusal should say "inside `repeat`".
  **FIXED 2026-10-13:** the depth stays (it bounds the stack, and keeps a view from stopping the runs);
  the three refusals name `repeat` and `assert` beside scripts.
- **S2. `cmd_repeat` writes to the console, not to its caller's output.** It uses `gs::io` and
  `Out::Console` and ignores the `out` it was given, so inside a captured run (`selfcheck save <path>`)
  its announcements, its runs and its verdict go to the screen and not to the capture.
  **FIXED 2026-10-13:** they go to `out`, so a function capture, `$(f)`, holds them; its own refusals
  still go to the screen. **The example above was wrong:** `selfcheck save` and `run ... save` save only
  the run's report, and every command's own output goes to the screen there, so `repeat` was not
  different from the rest in that case.
- **S3. Typed keys are dropped.** With no `wait`, `repeat` reads one console byte between runs and
  drops it unless it is `q`. `pause_quittable` (`wait`, and `repeat`'s pause) drops every key that is not
  `q`. Neither hands it to `keep_key`, so a command typed ahead is lost.
  **FIXED 2026-10-13:** both go through `quit_key_or_keep`, which keeps any other key and reads the
  console itself rather than the type-ahead buffer it fills.
- **S4. `kill all-services` drops names past `ALL_SERVICES_MAX - 2` without a word**
  (`n < segs.len() - 2`). A `MANAGED` set that outgrew 32 would leave services running and say nothing.
  It should refuse, or say how many it could not hold.
  **FIXED 2026-10-13:** it refuses, killing nothing, and says how many the supervisor runs. Not seen
  firing: the set is 18 against room for 32.
- **S5. `ask`'s "stopped" lines fire when `ask` did not stop.** `drain_service_with` returns true after
  the operator's `q` ("pipe: aborted") and after the shell's own 25 s bound (no EOT, and the shell kills
  `ask`), so `build_ask_table` prints "`ask` stopped before it replied" for a `q` and for a slow answer.
  The drain's outcome should reach it.
  **FIXED 2026-10-13:** `drain_service_with` returns how it ended (`Drained`), and `ask` words its line
  by it: `q` prints nothing more, the shell's bound says the shell ended it, and `ask` ending says so.
- **S6. Some request waits print no `[q] quit`** (the standing rule of 2026-10-12): `fs_request`,
  `fs_raw` (`drives`, `drives flash`, `reset`, `label`) and `ns_abortable` use plain
  `request_with_reply_abortable`, and the tagged re-waits in `fs_take_tagged` and `ns_take_tagged` use a
  bare `recv_abortable_deadline`. They are quittable, but do not say so.
  **FIXED 2026-10-13:** the requests go through `ask_with_quit_notice` and the re-waits through
  `recv_quittable`, so each says `[q] quit` once it lingers.

## `ask`

- **A1. `Facts::tick` is dead.** `services/ask/src/main.rs` computes it and nothing reads it since
  `joke_index` picks from `now`.
  **FIXED 2026-10-13:** the field and its reading are deleted.

## Next step

C1 first: it brings back, after ten days, a defect hardware already found.

## C2 built, 2026-10-28

The gen's high half is now **which life of the host this is**: the kernel's restart count for its name,
written into the task's own context page at spawn (`ServiceContextData::instance`, read as
`ctx.instance()`), beside `probe_mode` and `core_id`. It only rises within a boot and survives the
supervisor's own restarts, so a restarted host never repeats an earlier instance's gen, whatever its
clock does. The low half is that instance's bind count. Never 0. Bounds, in `usbfn::next_gen`: 65,536
instances a boot, 65,535 binds an instance.

**Why the kernel holds it and not the supervisor, against the first plan.** The supervisor cannot answer
"which life is this" without either giving every USB host a capability to send it commands (it accepts
`RESTART`, so that widens authority) or racing a spawn whose count does not exist yet. The kernel already
keeps the count (per name, bumped at each death, shown to INTROSPECT holders by `TaskStat`); the task
learns its own and nothing about any other. No syscall, no query, no authority, and MISCIS unchanged: it
is the scheduler's record of the task, told to the task, as `core_id` is. The field took the context
page's four bytes of tail padding, so the layout's size did not change and an older binary reads 0 there.

QEMU riscv64 `virt` with a stick: `xhci: driver starting (instance 0)`, then 1 and 2 across two kills,
the stick bound each time; then 1, a supervisor kill and respawn, then **2** - the count kept rising
across the supervisor's restart. `osdev test driver-packages` 36/0 (bindings through `xhci` and `ehci`).
