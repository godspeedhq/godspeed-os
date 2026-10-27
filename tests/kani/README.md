# tests/kani

Bounded model checking of the kernel's capability code with [Kani](https://github.com/model-checking/kani)
(`backlog/90`, layer C: Kani for bounded counterexample discovery).

## What it checks

The real files, not a model of them: `src/capability/mod.rs` and `src/smp/mod.rs` include
`kernel/src/capability/{cap,generation,rights,table}.rs` and `kernel/src/smp/spinlock.rs` by `#[path]`,
with the same thin seam `kernel/src/lib.rs` gives the host tests (a `kprintln` that prints nothing, an
interrupt mask that does nothing). `src/harnesses.rs` holds the proofs:

| Harness | Property (CLAUDE.md) |
|---|---|
| `rights_narrow_never_widens` | narrowing only removes rights (7.3) |
| `rights_without_removes_exactly` | `without` removes the mask and adds nothing |
| `rights_contains_is_a_partial_order` | every gate is a `contains`, and it is a partial order |
| `rights_empty_is_held_by_everyone` | the empty right passes every `contains`: why an owner must refuse a zero badge |
| `rights_union_is_the_least_upper_bound` | `union` and `\|` agree and are the least upper bound |
| `cap_validate_is_exact` | `validate` is Ok exactly when the generation matches and the right is held; a stale generation is reported first (7.7) |
| `cap_narrow_for_grant_keeps_identity` | a narrowed copy keeps the resource and generation |
| `cap_without_grant_cannot_be_regranted` | a stripped cap cannot be passed on (SEC-7) |
| `generation_bump_invalidates_every_older_cap` | a bump is strictly increasing (7.5) |
| `generation_bump_never_wraps` | at `u32::MAX` it panics rather than wrapping |
| `captable_insert_then_remove_round_trips` | a slot gives back what it was given, once |
| `captable_get_refuses_a_cap_after_revoke_or_death_{first_endpoint,mid_range,last_direct}` | lazy invalidation: after a revoke or a death the same held cap is `CapRevoked` or `EndpointDead`, for any rights and right asked, at ids 100, 4096 and 8191 - one concrete id per harness (7.5; a symbolic id over the 8192-entry table, even one of three values, took CBMC past 15 GB; a concrete one runs in seconds in about 120 MB) |
| `captable_get_empty_slot_is_not_held` | an empty or out-of-range slot is `CapNotHeld` |

Each is a proof for every value its symbolic inputs can take, within the loop bound it names. What none
of them establishes: that a syscall validates before it acts (`scripts/syscall_gate_check.py`), or that
it acts on the object it validated (review).

## Running it

Kani needs Linux. On Windows, in WSL (Ubuntu 24.04, the image `.github/workflows/verify.yml` runs on):

```bash
cargo install --locked kani-verifier && cargo kani setup   # once
cd tests/kani
CARGO_TARGET_DIR=$HOME/kani-target cargo kani              # every harness
cargo kani --harness cap_validate_is_exact                 # one
```

A target directory on the Linux side matters under WSL: building into `/mnt/c` is several times slower.

This crate is not a workspace member and has its own `[workspace]`, so `cargo build` at the root never
sees it, and nothing in the kernel build depends on Kani being installed. It runs in a pinned CI job,
`.github/workflows/verify.yml`, on a pull request that touches the capability code or this crate.

Run harnesses one at a time when working locally. A harness whose index into the 8192-entry
`GLOBAL_RESOURCES` is symbolic makes CBMC model the whole array and can take 15 GB; keep such ids
concrete.
