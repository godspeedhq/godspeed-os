# kernel/src/invariants/

Runtime enforcement of constitutional invariants (§3, §22).

## Files

| File              | Responsibility |
|-------------------|---------------|
| `mod.rs`          | Module declaration |
| `assertions.rs`   | One function per invariant; panic on violation |

## Philosophy

These are not debug-only checks. They run in release builds. If one fires, the system is in a state the spec says cannot exist — panic is the correct response (§25: "If an identity test fails, the system is no longer this system").

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
| `assert_cap_validated`              | §3.1 — no ambient authority   |
| `assert_no_mid_execution_migration` | §3.11 / §9.1                  |
| `assert_tcb_alive`                  | §6.2                          |
| `assert_cap_table_consistent`       | §7.8                          |

## Adding a new assertion

1. Add the function to `assertions.rs`.
2. Call it from the relevant hot path (syscall entry, scheduler, spawn).
3. Reference the spec section it pins in a `// Pins: §X.Y` comment on the function.
4. Add a row to the table above.
5. Add a corresponding identity or property test that would catch the invariant being violated.
