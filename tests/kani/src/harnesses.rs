// SPDX-License-Identifier: GPL-2.0-only
//! Kani proof harnesses over the real capability code (CLAUDE.md 7.3, 7.5, 7.7).
//!
//! Each harness states one property of the capability model and Kani checks it for EVERY value the
//! symbolic inputs can take: every rights byte, every generation, every slot. A harness that names a
//! bound says so; within it the answer is a proof, not a sample.
//!
//! What this does NOT establish: anything about the callers. That `dispatch.rs` validates a cap before
//! acting is `scripts/syscall_gate_check.py`'s question; that the syscall acts on the object it
//! validated is review (backlog/90 C4's stated limit). These prove the mechanism the callers rely on.

use crate::capability::cap::{CapError, Capability, ResourceId};
use crate::capability::generation::Generation;
use crate::capability::rights::Rights;
use crate::capability::table::{self, CapTable};

/// Any rights byte, including the two bits no constant names: the code must be right for those too,
/// since `Rights` is a plain `u8` and nothing outside the type stops them being set.
fn any_rights() -> Rights {
    Rights(kani::any())
}

fn any_cap() -> Capability {
    Capability {
        resource_id: ResourceId(kani::any()),
        rights: any_rights(),
        generation: Generation(kani::any()),
    }
}

// ---- rights (7.3: non-escalating) -----------------------------------------------------------------

/// Narrowing can only remove rights: the result is inside both the original and the mask.
#[kani::proof]
fn rights_narrow_never_widens() {
    let (r, m) = (any_rights(), any_rights());
    let n = r.narrow(m);
    assert!(r.contains(n));
    assert!(m.contains(n));
}

/// `without` removes every right in the mask and adds none.
#[kani::proof]
fn rights_without_removes_exactly() {
    let (r, m) = (any_rights(), any_rights());
    let w = r.without(m);
    assert!(r.contains(w));
    assert!(w.narrow(m) == Rights::empty());
}

/// `contains` is a partial order: reflexive, antisymmetric and transitive. Every gate in the kernel is
/// a `contains` check, so a wrong order would be a wrong gate everywhere at once.
#[kani::proof]
fn rights_contains_is_a_partial_order() {
    let (a, b, c) = (any_rights(), any_rights(), any_rights());
    assert!(a.contains(a));
    if a.contains(b) && b.contains(a) {
        assert!(a == b);
    }
    if a.contains(b) && b.contains(c) {
        assert!(a.contains(c));
    }
}

/// The empty right is held by every cap (`contains(Rights(0))` is vacuously true). Pinned because it is
/// the root of backlog/90's ResourceInvoke finding: a gate whose required right the caller chooses
/// passes when the caller chooses nothing, so the OWNER must refuse a zero badge.
#[kani::proof]
fn rights_empty_is_held_by_everyone() {
    assert!(any_rights().contains(Rights::empty()));
}

/// A union holds both of its parts, and nothing that neither part holds.
#[kani::proof]
fn rights_union_is_the_least_upper_bound() {
    let (a, b, c) = (any_rights(), any_rights(), any_rights());
    let u = a.union(b);
    assert!(u.contains(a) && u.contains(b));
    if c.contains(a) && c.contains(b) {
        assert!(c.contains(u));
    }
    assert!(u == (a | b));
}

// ---- a capability (7.5, 7.7) ----------------------------------------------------------------------

/// `validate` succeeds EXACTLY when the generation matches and the right is held, and a stale
/// generation is reported before missing rights (7.7: the more specific death wins).
#[kani::proof]
fn cap_validate_is_exact() {
    let cap = any_cap();
    let want = any_rights();
    let now = Generation(kani::any());
    let ok = cap.generation.0 == now.0 && cap.rights.contains(want);
    match cap.validate(want, now) {
        Ok(()) => assert!(ok),
        Err(CapError::GenerationMismatch) => assert!(cap.generation.0 != now.0),
        Err(CapError::CapInsufficientRights) => {
            assert!(cap.generation.0 == now.0 && !cap.rights.contains(want))
        }
        Err(_) => assert!(false, "validate returned an error it does not document"),
    }
}

/// A grant-narrowed copy names the same resource at the same generation, with exactly the mask -
/// under the precondition the function asserts in debug builds (the mask is inside what is held).
#[kani::proof]
fn cap_narrow_for_grant_keeps_identity() {
    let cap = any_cap();
    let mask = any_rights();
    kani::assume(cap.rights.contains(mask));
    let n = cap.narrow_for_grant(mask);
    assert!(n.resource_id == cap.resource_id && n.generation.0 == cap.generation.0);
    assert!(n.rights == mask && cap.rights.contains(n.rights));
}

/// A cap stripped for re-delegation (SEC-7) can never be passed on again, and keeps everything else.
#[kani::proof]
fn cap_without_grant_cannot_be_regranted() {
    let cap = any_cap();
    let s = cap.without_grant();
    assert!(!s.rights.contains(Rights::GRANT));
    assert!(cap.rights.contains(s.rights));
    assert!(s.rights.union(Rights::GRANT).contains(cap.rights));
    assert!(s.resource_id == cap.resource_id && s.generation.0 == cap.generation.0);
}

// ---- generations (7.5) ----------------------------------------------------------------------------

/// A bump is strictly increasing, so a bumped resource never matches a cap minted before the bump.
#[kani::proof]
fn generation_bump_invalidates_every_older_cap() {
    let g = Generation(kani::any());
    kani::assume(g.0 < u32::MAX);
    let b = g.bump();
    assert!(b.0 > g.0);
    assert!(!g.matches(b));
}

/// ...and at the top it fails LOUDLY rather than wrapping to a generation an old cap could match.
#[kani::proof]
#[kani::should_panic]
fn generation_bump_never_wraps() {
    let _ = Generation(u32::MAX).bump();
}

// ---- a task's table, against the global record (7.5, 7.7) -----------------------------------------

/// What goes into a slot comes back out of it, once.
#[kani::proof]
fn captable_insert_then_remove_round_trips() {
    let mut t = CapTable::empty();
    let cap = any_cap();
    let slot = t.insert(cap).expect("an empty table has room");
    assert!(slot == 0);
    let out = t.remove(slot).expect("the slot just filled");
    assert!(out == cap);
    assert!(t.remove(slot).is_none());
    assert!(t.remove(kani::any()).is_none());
}

// A FULL table is not proved here, deliberately. Its fill is 64 concrete inserts, so there is nothing
// symbolic to prove, and CBMC unrolling 64 slot scans ran 25 minutes without an answer. The host
// proptest `cap_table_full_rejects_next_insert` (kernel/src/capability/table.rs) covers it in
// milliseconds. Kani is for what a test cannot enumerate.

/// THE REVOCATION CONTRACT. For a resource at each edge of the endpoint range, any rights and any
/// right asked: a cap minted from the global record validates while the record stands, and after the
/// resource is revoked or dies, the SAME held cap fails with CapRevoked or EndpointDead respectively -
/// never Ok. This is lazy invalidation (7.5): nothing touches the holder's table, and it still cannot act.
///
/// The id is CONCRETE in each harness, not symbolic. A symbolic index into the 8192-entry
/// GLOBAL_RESOURCES array, even one limited to three values, made CBMC model the whole array
/// symbolically and grow past 15 GB. The table code does not branch on the id inside the range, so
/// the two edges and one interior value stand for the rest; the host tests in table.rs cover
/// arbitrary ids.
#[kani::proof]
#[kani::unwind(66)]
fn captable_get_refuses_a_cap_after_revoke_or_death_first_endpoint() {
    revoke_or_death_contract(100);
}

#[kani::proof]
#[kani::unwind(66)]
fn captable_get_refuses_a_cap_after_revoke_or_death_mid_range() {
    revoke_or_death_contract(4096);
}

#[kani::proof]
#[kani::unwind(66)]
fn captable_get_refuses_a_cap_after_revoke_or_death_last_direct() {
    revoke_or_death_contract(8191);
}

fn revoke_or_death_contract(id: u64) {
    let rights = any_rights();
    let want = any_rights();
    table::register_resource(ResourceId(id));
    let mut t = CapTable::empty();
    let slot = t.insert(table::mint_cap(ResourceId(id), rights)).unwrap();

    match t.get(slot, want) {
        Ok(c) => assert!(rights.contains(want) && c.resource_id == ResourceId(id)),
        Err(e) => assert!(e == CapError::CapInsufficientRights && !rights.contains(want)),
    }

    let dies: bool = kani::any();
    if dies { table::mark_dead_resource(ResourceId(id)) } else { table::revoke_resource(ResourceId(id)) }
    let expect = if dies { CapError::EndpointDead } else { CapError::CapRevoked };
    assert!(t.get(slot, want) == Err(expect));
}

/// A slot that holds nothing is `CapNotHeld` for every index and right, inside the table or past it.
#[kani::proof]
fn captable_get_empty_slot_is_not_held() {
    let t = CapTable::empty();
    assert!(t.get(kani::any(), any_rights()) == Err(CapError::CapNotHeld));
}
