//! Contract validation — §13.4, §22.
//!
//! Two jobs:
//!   1. `validate_all_contracts` — structural validation of all service.toml
//!      files against `contracts/schema/service.schema.json`.
//!   2. `run_identity_tests` — boot the OS in QEMU and run the §22 test suite,
//!      asserting serial output matches expected patterns.

use std::path::Path;

/// Validate every `contracts/*.toml` against the JSON schema.
///
/// Guarantees (§13.4): correct structure, valid capability names, valid resource
/// declarations, valid core IDs, required fields present.
///
/// Non-guarantees: behavioral correctness, limit reasonableness,
/// core availability at spawn time.
pub fn validate_all_contracts() {
    let schema_path = Path::new("contracts/schema/service.schema.json");
    let schema = load_schema(schema_path);

    let contracts = find_contracts();
    let mut failures = 0;

    for contract_path in &contracts {
        match validate_contract(&schema, contract_path) {
            Ok(()) => println!("OK  {}", contract_path.display()),
            Err(e) => {
                eprintln!("FAIL {} — {}", contract_path.display(), e);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        std::process::exit(1);
    }
}

/// Boot the OS in QEMU and assert the §22 identity test suite passes.
///
/// Each test boots with `-smp 4`, streams serial output, and matches
/// expected `TEST:` lines within a 30 s timeout.
pub fn run_identity_tests() {
    todo!(
        "for each test in tests/qemu/identity/: \
         boot QEMU, stream serial, assert TEST:PASS lines appear within 30s"
    )
}

fn load_schema(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read schema at {}: {}", path.display(), e));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid JSON schema: {}", e))
}

fn find_contracts() -> Vec<std::path::PathBuf> {
    todo!("walk the repo for all files matching */contracts/*.toml")
}

fn validate_contract(schema: &serde_json::Value, path: &Path) -> Result<(), String> {
    todo!(
        "parse TOML → JSON, validate against schema using jsonschema crate, \
         return Ok or the first validation error"
    )
}
