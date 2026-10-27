// SPDX-License-Identifier: GPL-2.0-only
// The REAL kernel name directory (kernel/src/ipc/names.rs, CLAUDE.md 6.1 Path C, 14.2), which until
// 2026-10-27 was host-tested only through `names_model.rs`, a copy of its logic (backlog/90 E4).
//
// The directory is one global table, so each test uses names no other test in this file uses, and
// the property test clears every name it registered before the next case.
use host_ipc::ipc::endpoint::EndpointId;
use host_ipc::ipc::names::{self, UnregisterOutcome};
use proptest::prelude::*;
use std::collections::HashMap;

fn ep(n: u64) -> EndpointId { EndpointId(n) }

#[test]
fn a_registered_name_resolves() {
    names::register("a-pong", ep(101));
    assert_eq!(names::lookup("a-pong"), Some(ep(101)));
    assert_eq!(names::lookup("a-pon"), None, "a prefix is a different name");
    assert_eq!(names::lookup("a-pong2"), None, "so is an extension");
}

// 14.2: a respawn registers the SAME name at a new endpoint, and clients that reacquire reach the new one.
#[test]
fn a_respawn_repoints_the_name() {
    names::register("b-fs", ep(201));
    names::register("b-fs", ep(202));
    assert_eq!(names::lookup("b-fs"), Some(ep(202)));
}

// The endpoint-id guard (2.5 amendment 2026-10-17): the OLD instance's death must not unregister the
// name its successor now holds - the race a respawn and a death run every time.
#[test]
fn an_old_instance_cannot_unregister_its_successor() {
    names::register("c-shell", ep(301));
    names::register("c-shell", ep(302));
    assert_eq!(names::unregister_endpoint("c-shell", ep(301)), UnregisterOutcome::KeptNewer(ep(302)));
    assert_eq!(names::lookup("c-shell"), Some(ep(302)));
    assert_eq!(names::unregister_endpoint("c-shell", ep(302)), UnregisterOutcome::Cleared);
    assert_eq!(names::lookup("c-shell"), None);
    assert_eq!(names::unregister_endpoint("c-shell", ep(302)), UnregisterOutcome::NotFound);
}

// An endpoint id belongs to one name. When a dead owner's id is handed to a new name, the stale
// mapping is evicted, or a lookup of the old name would MISROUTE to the new owner.
#[test]
fn a_reused_endpoint_id_evicts_the_stale_name() {
    names::register("d-old", ep(401));
    names::register("d-new", ep(401));
    assert_eq!(names::lookup("d-old"), None);
    assert_eq!(names::lookup("d-new"), Some(ep(401)));
}

// Pinned as it stands: a name longer than the directory holds is dropped WITHOUT a log or an error
// (register returns nothing), so the caller learns of it only when a lookup misses. Recorded in
// backlog/90; the kernel's callers never pass one, since spawn refuses a name over 64 bytes (14.1).
#[test]
fn an_over_long_name_is_never_stored() {
    let long = "e".repeat(host_ipc::task::PEER_NAME_BYTES + 1);
    names::register(&long, ep(501));
    assert_eq!(names::lookup(&long), None);
    let exact = "e".repeat(host_ipc::task::PEER_NAME_BYTES);
    names::register(&exact, ep(502));
    assert_eq!(names::lookup(&exact), Some(ep(502)));
    names::unregister(&exact);
}

proptest! {
    // The directory against a plain map, over random register / unregister_endpoint / unregister
    // sequences on a few names and ids: every lookup agrees, every unregister outcome agrees.
    #[test]
    fn the_directory_behaves_like_a_map_with_one_name_per_id(
        ops in prop::collection::vec((0u8..3, 0usize..4, 1u64..5), 1..40)
    ) {
        const N: [&str; 4] = ["p-a", "p-b", "p-c", "p-d"];
        let mut model: HashMap<&str, u64> = HashMap::new();
        for (op, n, id) in ops {
            let name = N[n];
            match op {
                0 => {
                    names::register(name, ep(9000 + id));
                    model.retain(|k, v| *k == name || *v != id);
                    model.insert(name, id);
                }
                1 => {
                    let want = match model.get(name) {
                        None => UnregisterOutcome::NotFound,
                        Some(&cur) if cur == id => { model.remove(name); UnregisterOutcome::Cleared }
                        Some(&cur) => UnregisterOutcome::KeptNewer(ep(9000 + cur)),
                    };
                    prop_assert_eq!(names::unregister_endpoint(name, ep(9000 + id)), want);
                }
                _ => { names::unregister(name); model.remove(name); }
            }
            for k in N {
                prop_assert_eq!(names::lookup(k), model.get(k).map(|&v| ep(9000 + v)));
            }
        }
        for k in N { names::unregister(k); }
    }
}
