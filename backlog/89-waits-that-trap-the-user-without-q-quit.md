# 89. Waits that trap the user without `[q] quit`

**Status: OPEN, 2026-10-13.** Found by `audits/2026-10-13-quit-audit.md`, which has the evidence (file:line for
every site), what passes and what was not checked. Nothing is fixed yet: the operator decides the order.

## The rule asked for

"Any utility or blocking action that requires a user to wait should have [q] quit or [b] background (if it's
backgroundable) beyond 2 seconds ... GodspeedOS should always be responsive and should never trap the user
behind a block." Waits on the user (confirms, `input`, full-screen programs with their own quit key) are
exempt.

## What is wrong, by cause

- **Q1. `[q]` is opt-in, not built into the wait.** Every default wait in `gs` and the SDK is silent and cannot
  be quit; 32 `Fs` handles and 32 service requests in the shell are such waits, most over 2 s. No checker
  notices a new one.
- **Q2. A loop of fast requests never shows `[q]`.** The 2 s clock is per request, so `read`, `copy`,
  `copy ... recursive`, `find`, `tree`, `dir` and a big `| write` run unbounded with no `[q]`, and `q` is not
  read until they end.
- **Q3. Commands with no console read at all**: `run` and the library scripts (`abortable = false`, and loops
  never look for `q`); the shell's `chaos kill-storm`, `mem-pressure`, `spawn-storm` (between spawns only) and
  `flood-storm`, which also settle with 60,000 yields - a count, not a duration.
- **Q4. Silent single waits over 2 s**: `delete ... recursive` (120 s, and the same in `selfcheck`'s tidy,
  `churn reset`, `ensure_scratch`); the plain mutating commands (5 s each); `net lease` / `net dns` failure /
  `sock` / `serve` start (`net_link_up`, `ns_deadline`); `foreground` / `jobs` / `background` (8-28 s, `q` and
  `b` read between 8 s asks); `events persist` (8-16 s); `date` / `date sync`; tab completion; the history on
  the key path; `spawn` / `restart` lists; `hardware`; the sticky resume before the first prompt.
- **Q5. `q` works and says nothing**: `wait N`, `ping <ip> count N`, `sock`'s `send_to`, `selfcheck <part>`.
- **Q6. `q` works and says the wrong thing**: `drives flash` / `reset` ("storage unavailable" while formatting),
  `wifi join` ("not joined" while joining), `read`, `copy` (a partial destination), `drives label`, `for line`,
  `foreground` ("stopped" unacknowledged), the never-cleared `ctx.fs_unknown`.
- **Q7. `q` is shown and lost**: `wifi_ask` clears `ctx.quit` at its start, so `wifi hardware` and
  `wifi hardware use` go on after a `q`; `fcap reuse` shows `[q]` and ignores it; inside `selfcheck` and
  `repeat`, a `q` taken by the running statement ends only that statement.
- **Q8. The written rule** (`utilities/0_conventions.md` rule 10) has no 2 s threshold, no `[b]`, an exception
  that is now stale (the `wifi radio` requests became quittable on 2026-10-13), dated notes describing gaps
  fixed on 2026-10-10, and a wrong reason for its "still silent" list (four of the five are not kernel `Call`s).
  Several utility specs still declare non-conformance that was fixed. This part is documentation and is the
  audit's to correct once the rule's new wording is agreed.

## The route (the audit's section 7)

The fix is a primitive, not 64 edits: a reply-matched sliced wait in the SDK, turned on by holding the
console-read slot rather than by an argument, the `[q] quit` words printed in one place, a COMMAND-level 2 s
watch so a loop of fast requests is covered, a sub-second clock, one type-ahead policy, `[b]` as a leave key
with honest "still running" wording, and a ratchet checker that refuses a new silent wait. Q3, Q6 and Q7 need
their own changes either way. Q8 is the rule text, rewritten to say: 2 s, on the slowest board; `[q]` always,
and `[b]` where the work can run detached; never trapped; and what `q` must say when the work cannot stop.

## Next step

The operator chooses: the primitive first (it removes Q1, Q2, Q5 and most of Q4 at once), or the user-facing
traps first (Q3 `run`, Q3 `chaos kill-storm`, Q4 `delete ... recursive`, Q6 `drives flash`), which are small and
independent.
