# The [q] quit audit, 2026-10-13

**Question (the operator's words):** "Any utility or blocking action that requires a user to wait should have
[q] quit or [b] background (if it's backgroundable) beyond 2 seconds. This should be an absolute rule to allow
the user experience to be fluid and non blocking to the user. GodspeedOS should always be responsive and
should never trap the user behind a block."

**Scope:** every wait a user can sit through at the shell prompt, at `a94c277d` (`feat/filesystem-layout`):
the filesystem waits, the waits on every other service, the sleeps and loops (scripts, `selfcheck`, chaos),
the wait primitives in `gs` and the SDK, and every utility spec against the code. Five read-only passes, each
cross-checked by at least one other; the two claims the conclusions rest on were re-read by hand (below).
Durations are read from the code's own deadlines and loops, judged on the slowest board (the Pi 2 over USB),
and **not measured** unless a line says so. Code defects found here are recorded in `backlog/89`, not fixed
in this audit.

**How a wait is judged.** A wait on the USER (a `[y/N]` confirm, `input`, a full-screen program with its own
quit key) is exempt. Every other wait that can pass 2 s must show `[q] quit` and honour `q` (and Esc, by rule
10), and say truthfully what happened when it was quit: stopped, or still running underneath, or outcome
unknown. `[b]` is judged only where the work can genuinely run detached.

---

## 1. The verdict

**The rule does not hold, and it is not enforced by construction.** The memory and the conventions both say
"[q] quit is automatic - built into the wait primitive". It is not. Every DEFAULT wait in `gs` and the SDK is
a silent bounded wait that cannot be quit; `[q]` exists only where a caller chose a quittable form, and no
checker notices when one does not.

- **The standard library is opt-in at every layer.** `gs::call::request` / `request_within` (`call.rs`, default
  `DEFAULT_SECS = 5`) ride the kernel's `CallDeadline` and cannot be quit. `gs::fs::Fs::new` / `from_tag` and
  `gs::net::Net::new` carry no notice; `with_notice` / `.noticing(..)` are opt-in. `resource::invoke` (file and
  socket caps) can be quit but prints nothing and drops type-ahead.
- **In the shell, 32 of 41 `Fs` handles have no `[q]`**, every one bounded over 2 s (5 s typical, 120 s for a
  recursive delete); **32 more service requests** have none, several over 2 s.
- **A loop of fast requests never shows `[q]` at all (the largest gap).** A quittable request starts its own
  2 s clock and reads no key before its notice. So a command made of many requests that each answer in under
  2 s - `read` of a large file, `copy`, `copy ... recursive`, `find /`, `tree /`, a big `dir`, a large
  `| write` - runs as long as it likes with no `[q]` shown, and a `q` typed during it waits as type-ahead
  until the command is over.
- **No checker enforces rule 10.** `scripts/` has none; `one_way_check.py` steers raw requests toward the
  non-quittable `gs::call::request`.

## 2. The written rule is weaker than the one asked for

`utilities/0_conventions.md` rule 10 says a wait of "more than a moment" advertises `[q] quit`. **2 s is not
the stated threshold** (it appears only as `NOTICE_AFTER_SECS`), **`[b]` is not in the rule at all**, the rule
is **not absolute** (it carries recorded exceptions), and the operator's ruling that a command with no `[b]`
simply has none, with no notice, is recorded nowhere.

Its text is also out of date, in three places:

- **The 2026-10-09 and 2026-10-12 notes** say `cd`, `read`, `find`, `tree` and the filters lend no notice. They
  all do since `backlog/fixed/80` H16 (2026-10-10). So do the specs that still say "Does NOT currently conform
  to rule 10" (`17_cd.md` 6, and `18_read`, `25_find`, `26_tree`, `27`-`29`, `41_sock`).
- **The recorded exception for the `wifi radio` power requests** describes code that no longer exists: since
  `bd7a3e28` (2026-10-13) `on`, `off`, `off hard` and `powercycle` wait through `ask_sifted_quittable` and honour
  `q`. `56_wifi.md` still says "BLOCKS ... with NO key". (That change also offers `q` on `off hard`, which its
  own SDK comment says "would promise a stop that cannot happen" - rule 11.)
- **The 2026-10-13 note's reason for the silent waits is wrong for four of five.** It says the waits on
  `recorder`, `copier`, `time` and `ns_deadline` stay silent because they are kernel `Call`s. They are not:
  they use `request_with_reply_deadline`, which sends and then waits in 20 ms `await_slice` slices on the
  ordinary endpoint (`service_context.rs`, `request_with_reply_deadline_inner`, re-read for this audit) - the
  same shape as the quittable form. Only the 2 s spawn request and `hw_ask` (`gs::call::request_within`) are
  real `CallDeadline`s.

## 3. Where a user is trapped, most likely first

Each line: the command, the worst case from the code, and what is missing.

1. **`run <script>` and the library scripts cannot be stopped at all.** `run` and `health` / `size` / `online`
   / `busiest` pass `abortable = false`; `for` and `loop` never look for `q` (a `loop` runs to 100,000
   iterations). A `q` during a `wait` inside a loop ends only that `wait`.
2. **The fast-request loops** (section 1): `read`, `copy`, `copy ... recursive`, `find`, `tree`, `dir`, a big
   `| write`. Unbounded by size; no `[q]`, and `q` is not read.
3. **`chaos kill-storm`, `mem-pressure`, `spawn-storm`, `flood-storm`** (the shell's own modes): no console
   read at all in kill-storm (20 rounds by default, up to 8 s + 8 s each); spawn-storm reads `q` only between
   spawns. All end with 60,000 settle YIELDS - a count, not a duration; `services/chaos` records ~10 ms per
   yield on a Pi 2, which would make it minutes (not measured).
4. **`delete <path> recursive`**: one silent `delete_all`, up to 120 s (`SWEEP_SECS`), though
   `background delete` exists. The same 120 s sits silently in `selfcheck`'s start-up tidy (up to ~375 s over
   three remnants, and it runs exactly after a run stopped with `q`), `churn reset` and the shell's first use
   of scratch (`ensure_scratch`).
5. **Every plain mutating command is silent up to 5 s**: `write`, `mkdir`, `rename`, `move`, `delete`, `seal`,
   `| write`, `write append` / `prepend`, `edit` save, `fmt`, `scrollback save`, `run ... save`.
6. **`selfcheck`**: `q` is honoured between statements, but a statement whose own wait takes the `q` ends only
   that statement and the suite carries on; `selfcheck <part>` shows no `[q]` banner.
7. **`net lease`, the `net dns` failure path, `sock`, `serve`'s start**: `net_link_up` (2 + 2 s) and
   `ns_deadline` (3 + 3 s) are silent, and net-stack holds its loop during a DHCP exchange, so 4-10 s of
   silence right after a link comes up is the realistic case. `sock`'s 30 s `send_to` can be quit but says
   nothing.
8. **`foreground`, `jobs`, `jobs quit`, `background`**: 8-28 s silent against a busy `copier`; in
   `foreground`, `q` and `b` are read only between 8 s status asks, and `q` prints "job N stopped" whether or
   not the cancel was acknowledged.
9. **`wait N`** and **`ping <ip> count N`**: `q` works, but nothing says so.
10. **Tab completion** of a path (5 s per page, no notice) and of `wifi join` / `forget` (3 s): the prompt
    freezes; `backlog/38` records completion timing out about one run in three.
11. **The history**: the up-arrow load (4-6 s once a session) and the write before every command (2 s, up to
    6 s for a long history) are silent on the key path.
12. **`events persist start` / `stop` / `status`**: 8-16 s silent.
13. **`date`** (4 s per `time_rpc`, up to 16 s) and **`date sync`** ("(q aborts)" is printed but not honoured
    for its first ~10 s).
14. **`wifi hardware`, `wifi hardware use`**: `[q]` is shown, but `wifi_ask` clears `ctx.quit` at its start
    (re-read for this audit), so a `q` in the first ask is erased by the next, and the command goes on.
15. **`spawn a,b,c` / `restart a,b,c`**: up to 32 s silent (16 items x a 2 s request).
16. **`hardware`, `audio hardware`** on the PC, Pi 4 and VisionFive boards: a loop of 2 s `CallDeadline`s, one
    per driver and device; ~100 s if `hw-enumerator` is alive and stalled. Rare, very long.
17. **Before the first prompt**: `sticky_resume` reads the sticky record with up to 4 attempts at 8 s before
    `gsh>` is drawn - up to about a minute if `fs` is slow but alive (not measured).
18. **Smaller, once each**: `hardware compare` (50 s), `chaos ... save` (~38 s), the chaos seed record (6 s,
    added today), `churn verify` / `tear` (20 s per file), `wifi hardware use`'s record, `fcap reuse`
    (shows `[q]`, ignores it).

## 4. Where `q` works but does not tell the truth

- **`drives flash` / `drives reset`**: `q` prints "storage unavailable (no fs?)" while the format carries on.
- **`wifi join`**: `q` prints "not joined - aborted" while the driver goes on joining; the honest answer is
  "outcome unknown - see `wifi status`".
- **`read`, `copy`, `drives label`, `for line`**: a quit is reported as a storage or read failure, and `copy`
  leaves the destination partly written.
- **`ctx.fs_unknown`** is set once and never cleared, so a later unrelated failure can claim OUTCOME UNKNOWN.
- **`drives check` and the `wifi radio powercycle` watch**: `q` leaves the WAIT, the work continues - which
  they say. Rule 11 asks that quitting stop the task; where it cannot, the rule should say what is allowed
  instead.

## 5. What passes

The prompt's own key read, the muted loop, Esc handling, `ping` (continuous), `date sync`'s watch, `cores
ticks`, `audio` (every verb, `tone`, `play`), `wifi scan` (with `[q] [b]`), `wifi status` / `list` / `leave`,
`wifi radio ...`, `net`, `net scan`, `tcp`, `serve`, `trace` and `events` queries, `observe now` and live,
`ask`, every pipe stage's drain, `chaos link-flap`, `repeat`, `drives` / `drives check` / `scrub`, `dir` /
`find` / `tree` for one slow request, and every wait on the user: confirms, `input`, `help`, `docs`,
`paginate`, `scrollback`, the tab menus, `edit`'s keys.

## 6. `[b]`

- **Offered today**: `foreground`'s `[b]`; the copier kinds (`background copy`, `delete ... recursive`,
  `drives check`, `drives scrub`, `churn`), but only when typed as `background ...` first - a foreground
  `delete recursive` has no `b` key; and two places outside the copier, where the driver is the job: `wifi scan`
  and the `wifi radio powercycle` / cold-start watch.
- **Good candidates**: `find` and `tree` (fs-only and read-only; the copier's transcript must say when it is
  truncated); a `b` key on the foreground copier kinds (which today would mean "start again, detached",
  because one `fs` request cannot be handed over mid-flight); `wifi join` and a cold `radio on`, like the
  powercycle watch, because the driver carries on by itself.
- **Permanently no** (`services/copier/CLAUDE.md`, `55_background.md` 3a): `selfcheck`, `chaos`, `run` - an
  orchestrator or an authority the copier must not hold.
- **`drives check` in the foreground** prints `[q] quit (detach it next time: background drives check)`. Under
  the operator's ruling that a command with no `[b]` says nothing about it, this is the kind of line to
  remove, or to replace with a real `b`.

## 7. What would make the rule true by construction (design, not built)

1. **One quittable wait in the SDK, matched to its reply.** The pieces exist: the reply mailbox
   (`reply_mailbox`) and the sliced `await_slice`. A request sent with a reply cap to the mailbox, waited on in
   20 ms slices on the mailbox alone, can read the keyboard between slices, stays safe in a task that serves
   others, and keeps `gs`'s retry and error distinctions. Whether the kernel signals a peer's death to a
   non-`Call` wait is not checked.
2. **Keyed on authority, not on an argument.** A task that holds the console-read slot gets `[q]` on every
   `gs` wait - `call`, `Fs`, `Net`, `File`, `ipc::recv_within` - and keeps every other key; a task without it
   gets the plain bounded wait. The opt-in constructors become the only constructors.
3. **The words in one place**: the SDK prints `[q] quit`, so no call site can forget it.
4. **A command-level watch**, not only a per-request one: the 2 s clock belongs to the COMMAND, so a loop of
   fast requests shows `[q]` at 2 s and reads `q` between requests. This closes section 1's largest gap and is
   the part a per-request design cannot.
5. **A sub-second clock.** Elapsed time is whole seconds today (`epoch_secs_monotonic`), so "after 2 s" means
   anywhere from 1 to 3 s.
6. **One type-ahead policy**: today the request side reads no key before its notice and the receive side reads
   from the first slice; `resource::invoke` drops what it reads.
7. **`[b]` as a primitive**: a leave key with "the work continues" meaning, stated honestly.
8. **A ratchet checker in `EXTRA_CHECKS`**: count, in interactive crates, every silent wait (`Fs::from_tag`
   without a notice, `gs::call::request*`, raw `request_with_reply_*`, `spawn_via_supervisor`) and refuse any
   rise, as `one_way_check.py` does - until (2) makes the count zero.

## 8. Not checked

Real durations on a Pi 2 (yield cost, whether `fs` answers while mounting); the kernel `CallDeadline`
granularity; the `chaos` service's own `q` after `max-carnage` launches beyond its round loop; `observe`'s
painter internals; the library scripts' durations; the `godspeed_wifi::keyfile::make_dir` deadline.
