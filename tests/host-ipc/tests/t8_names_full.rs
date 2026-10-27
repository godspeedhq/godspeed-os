// SPDX-License-Identifier: GPL-2.0-only
// The REAL name directory when it is full (kernel/src/ipc/names.rs). Its own process, because the
// directory is one global table and a full one would starve every other test sharing it.
use host_ipc::ipc::endpoint::EndpointId;
use host_ipc::ipc::names;

fn ep(n: u64) -> EndpointId { EndpointId(n) }

#[test]
fn a_full_directory_keeps_what_it_holds() {
    let held: Vec<String> = (0..128).map(|i| format!("z-{i}")).collect();
    for (i, n) in held.iter().enumerate() {
        names::register(n, ep(7000 + i as u64));
        assert!(names::lookup(n).is_some(), "{n} did not fit in an empty directory of 128");
    }
    names::register("z-overflow", ep(7999));
    assert_eq!(names::lookup("z-overflow"), None, "the 129th name has no room");
    for n in &held { assert!(names::lookup(n).is_some(), "a full table lost {n}"); }
    // ...and a freed entry is reused.
    names::unregister("z-0");
    names::register("z-overflow", ep(7999));
    assert_eq!(names::lookup("z-overflow"), Some(ep(7999)));
}
