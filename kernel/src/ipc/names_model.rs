//! Name registry model for property testing — §14.2, §22 P8.
//!
//! `TestNameModel` mirrors the algorithmic invariants of `ipc/names.rs`
//! without `SpinLock` or global statics.  Used to verify that after a
//! restart, a name always resolves to the most-recently registered endpoint.

// Local model ID — structurally equivalent to ipc::endpoint::EndpointId(u64).
// Defined here because endpoint.rs depends on crate::task which is hardware-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId(pub u64);

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

struct NameEntry {
    name:        String,
    endpoint_id: EndpointId,
}

pub struct TestNameModel {
    entries: Vec<NameEntry>,
}

impl TestNameModel {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Register or update a name → endpoint mapping.
    /// Mirrors `ipc::names::register` update-in-place semantics.
    pub fn register(&mut self, name: &str, endpoint_id: EndpointId) {
        for e in &mut self.entries {
            if e.name == name {
                e.endpoint_id = endpoint_id;
                return;
            }
        }
        self.entries.push(NameEntry { name: name.to_owned(), endpoint_id });
    }

    /// Look up a name → endpoint mapping.
    pub fn lookup(&self, name: &str) -> Option<EndpointId> {
        self.entries.iter()
            .find(|e| e.name == name)
            .map(|e| e.endpoint_id)
    }

    /// Total number of registered names.
    pub fn len(&self) -> usize { self.entries.len() }
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn name_strategy() -> impl Strategy<Value = String> {
        "[a-z]{1,8}".prop_map(|s| s)
    }

    fn ep_strategy() -> impl Strategy<Value = u64> {
        0u64..256
    }

    // -----------------------------------------------------------------------
    // P8: name resolves to the most-recently registered endpoint (§14.2, §22 P8)
    // -----------------------------------------------------------------------

    proptest! {
        /// Registering a name twice: lookup always returns the second endpoint —
        /// §14.2, §22 P8 (restart updates name → new endpoint).
        #[test]
        fn lookup_returns_most_recent_registration(
            name      in name_strategy(),
            ep1_raw   in ep_strategy(),
            ep2_raw   in ep_strategy(),
        ) {
            let ep1 = EndpointId(ep1_raw);
            let ep2 = EndpointId(ep2_raw);
            let mut registry = TestNameModel::new();
            registry.register(&name, ep1);
            registry.register(&name, ep2);
            prop_assert_eq!(registry.lookup(&name), Some(ep2));
        }

        /// A name registered once is always found — no phantom miss — §14.2, §22 P8.
        #[test]
        fn registered_name_always_found(
            name   in name_strategy(),
            ep_raw in ep_strategy(),
        ) {
            let ep = EndpointId(ep_raw);
            let mut registry = TestNameModel::new();
            registry.register(&name, ep);
            prop_assert_eq!(registry.lookup(&name), Some(ep));
        }

        /// A name that was never registered returns None — §14.2.
        #[test]
        fn unregistered_name_returns_none(
            name      in name_strategy(),
            other     in name_strategy(),
            ep_raw    in ep_strategy(),
        ) {
            prop_assume!(name != other);
            let mut registry = TestNameModel::new();
            registry.register(&other, EndpointId(ep_raw));
            prop_assert_eq!(registry.lookup(&name), None);
        }

        /// Registering N distinct names creates exactly N entries —
        /// one slot per name, no merge or loss — §14.2.
        #[test]
        fn distinct_names_each_get_own_entry(
            names in proptest::collection::hash_set(name_strategy(), 1..8),
        ) {
            let names: Vec<String> = names.into_iter().collect();
            let mut registry = TestNameModel::new();
            for (i, name) in names.iter().enumerate() {
                registry.register(name, EndpointId(i as u64));
            }
            prop_assert_eq!(registry.len(), names.len());
            for (i, name) in names.iter().enumerate() {
                prop_assert_eq!(registry.lookup(name), Some(EndpointId(i as u64)));
            }
        }
    }
}
