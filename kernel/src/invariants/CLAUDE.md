# kernel/src/invariants/

Runtime enforcement of constitutional invariants (§3, §22).

## Files

| File              | Responsibility |
|-------------------|---------------|
| `mod.rs`          | Module declaration |
| `assertions.rs`   | One function per invariant; panic on violation |

## Philosophy

These are not debug-only checks. They run in release builds. If one fires, the system is in a state the spec says cannot exist - panic is the correct response (§25: "If an identity test fails, the system is no longer this system").

## When to add an assertion

Add one when:
- A constitutional invariant (§3) has a concrete checkable form.
- The check is cheap enough to run on every syscall or scheduling point (O(1) or O(small-constant)).

Do NOT add assertions that:
- Are only meaningful in debug builds.
- Require O(N) walks of global state on hot paths (put those in the test suite instead, §22).
- Duplicate what the type system already enforces.

## Existing assertions

| Function                            | Invariant pinned               |
|-------------------------------------|-------------------------------|
| `assert_no_mid_execution_migration` | §3.11 / §9.1                  |
| `assert_cap_table_consistent`       | §7.8                          |

`assert_cap_table_consistent` halts only on the CALLER's own table. It reads every other task's table with
no lock while those tables change on other cores, so a violation found there is reported, not a panic:
a torn read deciding a halt would let a userspace `kill` take the kernel down (`backlog/90` D24). That is
the one exception to "panic on violation" above, and it is the reading that is unsound, not the invariant.

`assert_cap_validated` (passed a literal `Ok`, so it could not fire) and `assert_tcb_alive` (over an
empty TCB set since Phase 6) were deleted 2026-10-10, `backlog/fixed/80` K20: an assertion that cannot fail
reports a pass while measuring nothing.

## Adding a new assertion

1. Add the function to `assertions.rs`.
2. Call it from the relevant hot path (syscall entry, scheduler, spawn).
3. Reference the spec section it pins in a `// Pins: §X.Y` comment on the function.
4. Add a row to the table above.
5. Add a corresponding identity or property test that would catch the invariant being violated.
