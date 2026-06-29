# Utility Spec: `observe`

**Utility:** `observe` - system metrics viewer
**Version:** 0.1.0
**Status:** SPEC (not yet built). This document is written before implementation, per the utility-spec-first rule.
**Last updated:** 2026-06-03

---

## 1. Purpose

`observe` answers one question: **what is the system doing right now?** It surfaces
the per-task and per-core metrics the kernel already tracks (state, core, memory,
queue depth, restarts, CPU time) plus a system summary (frames, per-core CPU%,
endpoint count).

`observe` reports raw metrics. It does **not** render a verdict - "is everything
OK?" is the future job of a separate `status` utility (see §9). Keeping the two
apart is deliberate: `observe` = *what is happening*; `status` = *is it healthy*.

---

## 2. Invocation surface

| Command | Meaning |
|---|---|
| `observe` | **Continuous** live-refreshing view. Repaints in place until you press `q`. |
| `observe now` | **Static** one-shot frame. Prints once, returns to `gsh>`. |
| `observe help` | Usage for the utility (modes, subcommands, version header). |
| `observe now help` | Usage for the static subcommand (what the columns mean). |
| `observe version` | Prints `observe 0.1.0`. |

`observe` (bare verb) means *ongoing observation* - that is why the live view is
the default and the static one is the modified form. The word after the verb picks
the cadence: nothing = ongoing, `now` = a single instant. No flags, no interval
math, no `--`.

### 2.1 `observe now` as a record producer (typed pipes)

Piped, **`observe now`** is a record producer (`docs/records.md`,
`utilities/31_records.md`): it emits the task roster plus the metric `status` omits -
**`ticks`**, each task's cumulative `run_ticks` (timer ticks spent running since boot).
That column is what distinguishes `observe` (how busy) from `status` (who's alive):

```
observe now | sort reverse ticks     the native "top": busiest tasks first
observe now | where core=1           tasks on a core, with their cpu-time
observe now | select name ticks      just the cpu-time view
observe now | to json                a snapshot frame as JSON
```

`ticks` is a **snapshot-honest** value - cumulative ticks, not an instantaneous %.
A true rate needs two samples, which only the live view has (and the live view's
per-task "CPU%" is in fact its *core's* utilisation, so it would not sort
meaningfully per task anyway). **Only `observe now` is pipeable.** Bare `observe` is
the live, screen-owning loop - it never yields a discrete stream, so `observe | …`
is a **loud refusal** (`observe: the live view can't be piped - use 'observe now |
…'`), never a silent hang. This is the general rule: a live-loop utility's *snapshot*
form is the record producer; its *live* form is not (`docs/records.md`).

> **Build-order note:** `observe now` (static) is built first - it needs no kernel
> or console change. Until the live view exists, bare `observe` prints
> `live view coming soon - try 'observe now'`. `observe`'s *meaning* never changes
> later (it is "the live view" from day one); it only stops being unfinished.

---

## 3. Utility conventions

The general rules that apply to **every** GodspeedOS utility - `help` is the only
help form (no `-h`/`--help`), subcommands are words not single-letter flags,
per-utility `version`, raw-facts-only (no editorializing), non-POSIX vocabulary -
now live in **`0_conventions.md`**. They were hoisted there once a second utility
existed, exactly as this section originally anticipated (§26.2: pull the
abstraction into existence, don't build it speculatively). `observe` follows all of
them; what remains below is `observe`'s own normative help output.

### Help output shape (normative)

```
observe 0.1.0 - system metrics viewer

usage:
  observe          watch live metrics (refreshes until you press q)
  observe now      print a single metrics frame and return
  observe version  print the version
  observe help     print this message

subcommand help:
  observe now help
```

---

## 4. `observe now` - static frame (build first)

### 4.1 Layout

```
observe 0.1.0  ·  snapshot

SLOT  NAME              CORE  STATE         MEM KiB (used/lim)   Q  RST
----  ----------------  ----  ------------  ------------------  --  ---
   0  init                 0  Running              256 / 65536   0    0
   1  supervisor           0  BlockedRecv          512 / 65536   0    0
   2  shell                0  Running              384 / 65536   0    0
   3  xhci                 1  Running             1024 / 65536   0    0
   4  registry             0  Ready                256 / 65536   0    0

cores: 4    cpu:  c0 38%   c1 9%   c2 2%   c3 1%
memory: 18.4 / 4096.0 MiB used  (4061 MiB free)
endpoints: 14
```

### 4.2 Per-task columns

| Column | Source (`TaskStat`) | Notes |
|---|---|---|
| SLOT | the slot index 0..N | scheduler slot, stable for task lifetime |
| NAME | `name` / `name_len` | truncated to 16 |
| CORE | `core` | pinned core (§9.1) |
| STATE | `state` | Ready / Running / BlockedRecv / BlockedSend / Dead |
| MEM | `mem_used` / `mem_limit` | shown in KiB |
| Q | `queue_depth` | inbound IPC queue depth, 0–16 |
| RST | `generation` | endpoint generation = restart counter (§7.5, §14.2) |

Only slots with `valid == true` are listed.

### 4.3 Summary line

| Field | Source | Notes |
|---|---|---|
| cores | `inspect_core_count()` | |
| cpu cN% | `inspect_core_active_ticks(N) / inspect_core_total_ticks(N)` | cumulative-since-boot share for the static frame (see §5.3) |
| memory used | `inspect_kernel_total_frames() - inspect_kernel_free_frames()` × 4 KiB | |
| memory free/total | `inspect_kernel_free_frames()`, `inspect_kernel_total_frames()` | |
| endpoints | `inspect_kernel_endpoint_count()` | |

**No kernel changes are required for the static frame** - every value above is an
existing introspection syscall the shell already has authority to call.

---

## 5. `observe` - continuous view (build second)

### 5.1 Behaviour

Repaints the §4 frame in place on a fixed cadence (default **1 s**) until the
user presses **`q`**. Any other key is ignored.

### 5.2 Exit behaviour (decided)

On `q`: stop refreshing, **leave the last frame on screen**, print `gsh>` beneath
it. The display does NOT vanish. Rationale: (a) the framebuffer console has no
alternate-screen buffer to restore from, so htop-style "restore previous screen"
is not a capability that exists; (b) the final frame persisting on the TV is
strictly better for hand-transcription - you quit, then read at your own pace,
with the prompt right there confirming the shell is back.

### 5.3 CPU% - cumulative vs instantaneous

The static frame shows cumulative-since-boot CPU share (active/total ticks). The
live view SHOULD show *instantaneous* CPU% - the delta in active/total ticks
between successive frames - so the numbers reflect current load, not lifetime
average. This requires the utility to remember the previous frame's tick counts.

### 5.4 Console prerequisite (kernel/console work - gates the live view)

A live in-place repaint needs the framebuffer console to support **clear-screen +
cursor-home**. Today the fbcon only streams glyphs and scrolls; it has no
clear/home. The first concrete piece of the live view is therefore a console
control (e.g. interpret `0x0C` form-feed as clear+home, or an explicit escape),
not the metrics. `observe` then: clear+home → paint frame → poll the console
input ring for `q` for ~1 s → repeat. The `q` keypress rides the same console
input ring that the USB keyboard pushes into (closing the loop with the xHCI work).

---

## 6. Data sources (summary)

All present today in `sdk/rust/src/service_context.rs`:

- `task_stat(slot) -> TaskStat` - per-task: `valid, state, core, mem_used,
  mem_limit, name, generation, queue_depth, run_ticks`.
- `inspect_core_count()`, `inspect_core_active_ticks(c)`, `inspect_core_total_ticks(c)`.
- `inspect_kernel_free_frames()`, `inspect_kernel_total_frames()`,
  `inspect_kernel_endpoint_count()`, `inspect_kernel_alloc_bytes()`.

---

## 7. Architecture: standalone utility service (DECIDED 2026-06-03)

`observe` is a **standalone utility service brokered by the shell** - NOT a shell
built-in.

**Rationale (the spirit of GodspeedOS):**

- **Least authority - the decisive one.** The shell holds `spawn`/`kill`/`restart`
  authority; it is one of the most dangerous userspace authorities. `observe` needs
  only to *read* metrics. A built-in would run the metrics code in the same
  protection domain as kill/restart authority. As its own service, `observe` holds
  an introspection cap and nothing else - it cannot kill or restart anything *by
  construction*, not by being careful (§3.1 no ambient authority, §3.3 authority is
  explicit, §26.9 authority stays visible and scoped).
- **The shell's role.** Appendix B.3 defines the shell as a capability *broker*: it
  holds authority and hands scoped caps to children it spawns. Brokering a
  least-authority `observe` is the shell doing its job; *being* `observe` is the
  shell stepping out of its role.
- **Isolation & restartability** (§2.4, §3.6): `observe` crashing cannot take down
  the shell or its authority. Matches the utilities-as-services vision (Appendix D)
  and reuses the existing `observe` service rather than duplicating its logic in the
  shell.

§26.2 (don't build speculative infrastructure) does **not** argue for a built-in
here: the service is not speculative (it already exists on hardware), and the
least-authority requirement is real today, not hypothetical.

**Cost accepted:** more plumbing (spawn-on-demand, console output, and - for the
live view - a console-ownership/input handoff for the `q` keypress) and a
spawn-per-invocation (~ms, fine - performance is third, §20).

**Introspection capability (resolved 2026-06-03).** Introspection is now gated by
`INTROSPECT_RESOURCE` (READ): `TaskStat` and the system-state `InspectKernel`
queries require it; self-state and the TSC stay ambient. `observe` is granted the
cap at spawn (name-gated, like `shell`), so its least-authority story is now
literal - it holds the introspection cap plus a console cap, never the shell's
`spawn`/`kill`/`restart`. Done on branch `feat/introspect-cap`; see
`docs/introspection-capability.md`.

---

## 8. Capabilities required

`observe` runs as a standalone service (§7), so its **contract declares exactly what
it needs and nothing more**:

- an **introspection capability** - read-only access to the `inspect_*` / `task_stat`
  surface (see the §7 note on making this explicit if it is currently ambient);
- a **console output capability** to render its frame;
- for the live view only, a **console input capability** to read the `q` keypress.

It does NOT hold `spawn`/`kill`/`restart` - that authority stays with the shell. The
shell brokers the spawn; the kernel mints these caps from the contract at spawn time
(§13, §14.1).

---

## 9. Out of scope / non-goals

- **Health verdicts.** Reserved for a future `status` utility (is-everything-OK).
- **Filtering/sorting/search** (top-style `P`/`M` sort keys). Not v1.
- **Per-task drill-down** (cap list, held resources). A later `observe <name>`
  subcommand could add this; not now.
- **Historical/graphing.** `observe` is point-in-time, not a time series.
- **Serial-specific rendering.** Output targets the framebuffer console; serial is
  best-effort (logs may interleave across cores).

---

## 10. Open questions

1. Live-view refresh cadence: fixed 1 s, or `observe <interval>` to tune it later?
   (Deferred - `now` vs bare covers the static/live split; interval tuning is a
   possible future addition, not v1.)

### Resolved

- **Help form (2026-06-03):** the word `help` is the *only* form - no `-h`, no
  `--help`, no tolerated synonyms. An undocumented synonym would be a hidden, unsaid
  rule, which the system forbids; `-h` is simply `unknown:` and the response teaches
  the real word (§3 rule 3). Subcommands stay words - no single-letter flags
  (§3 rule 4).
- **Architecture (2026-06-03):** standalone utility service brokered by the shell,
  not a built-in (§7) - least authority + the shell's broker role.
- **Typing economy (2026-06-03):** addressed by future shell ergonomics
  (tab-completion, command history), NOT by abbreviating the utility vocabulary.
  Not to be relitigated per-utility.

---

## 11. Build order

1. **Console: clear+home control** - only needed for the live view; static does
   not need it. (Gates step 3, not step 2.)
2. **`observe now` static frame** - as the brokered standalone service (§7), §4.
   The shell spawns `observe` in `now` mode; it prints one frame via its
   introspection + console caps and exits. Wire `observe now`, `observe version`,
   `observe help`, `observe now help`, and bare `observe` → "live view coming soon
   - try 'observe now'". QEMU-verify (screendump).
3. **`observe` live view** - §5, after step 1. Delta CPU%, `q`-to-quit, persist on
   exit.
