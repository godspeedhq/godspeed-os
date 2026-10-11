// SPDX-License-Identifier: GPL-2.0-only
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // services/ask -> services/ -> workspace root
    let workspace = std::path::Path::new(&manifest)
        .parent().unwrap()
        .parent().unwrap();
    let ld = workspace.join("services").join("user.ld");
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    println!("cargo:rerun-if-changed={}", ld.display());
    println!("cargo:rustc-link-arg=--entry=service_main");

    // THE GLOSSARY, from its one source. `ask "what is MISCIS?"` answers from `GLOSSARY.md`'s table, read
    // here at build time, so there is no second copy of a definition to drift from the first
    // (Commandment III). A row is `| ABBR | Expansion | Meaning |`; anything else in the file is prose.
    let gloss = workspace.join("GLOSSARY.md");
    println!("cargo:rerun-if-changed={}", gloss.display());
    let text = std::fs::read_to_string(&gloss)
        .unwrap_or_else(|e| panic!("ask/build.rs: cannot read {} ({}) - the glossary is ask's only source of definitions", gloss.display(), e));
    let mut out = String::from("/// Generated from GLOSSARY.md by build.rs: (abbreviation, expansion, meaning).\n\
                                pub static GLOSSARY: &[(&str, &str, &str)] = &[\n");
    let mut rows = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if !t.starts_with('|') || t.starts_with("|--") || t.starts_with("| Abbreviation") { continue; }
        let cells: Vec<&str> = t.trim_matches('|').split('|').map(|c| c.trim()).collect();
        if cells.len() < 3 || cells[0].is_empty() || cells[0].starts_with('-') { continue; }
        out.push_str(&format!("    ({:?}, {:?}, {:?}),\n", cells[0], cells[1], cells[2..].join("|")));
        rows += 1;
    }
    out.push_str("];\n");
    if rows < 10 {
        panic!("ask/build.rs: read only {} glossary rows from {} - the table's shape changed; refusing to ship a glossary that answers almost nothing", rows, gloss.display());
    }
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("glossary.rs");
    std::fs::write(&dest, out).unwrap();

    // WHAT EACH SERVICE IS, in its own words: every crate under `services/` and `examples/` describes
    // itself in its `Cargo.toml` (`description`), read here so `ask "what is fs?"` has one source and no
    // copy to drift (Commandment III). A system service without one fails the build: a new service
    // says what it is, or `ask` would have nothing true to say about it.
    let mut svc = String::from("/// Generated from each crate's Cargo.toml by build.rs: (crate name, description).\n\
                                pub static SERVICES: &[(&str, &str)] = &[\n");
    let mut count = 0usize;
    for (dir, required) in [("services", true), ("examples", false)] {
        let root = workspace.join(dir);
        println!("cargo:rerun-if-changed={}", root.display());
        let mut entries: Vec<_> = std::fs::read_dir(&root)
            .unwrap_or_else(|e| panic!("ask/build.rs: cannot list {} ({})", root.display(), e))
            .filter_map(|e| e.ok()).map(|e| e.path().join("Cargo.toml")).filter(|p| p.exists()).collect();
        entries.sort();
        for toml in entries {
            println!("cargo:rerun-if-changed={}", toml.display());
            let text = std::fs::read_to_string(&toml).unwrap_or_default();
            let field = |key: &str| text.lines().map(|l| l.trim())
                .find(|l| l.starts_with(key) && l[key.len()..].trim_start().starts_with('='))
                .and_then(|l| l.splitn(2, '=').nth(1)).map(|v| v.trim().trim_matches('"').to_string());
            let name = match field("name") { Some(n) => n, None => continue };
            match field("description") {
                Some(d) => { svc.push_str(&format!("    ({:?}, {:?}),\n", name, d.replace("\\\"", "\""))); count += 1; }
                None if required => panic!("ask/build.rs: {} has no `description` - every service says what it is (`ask \"what is {}?\"` answers from it)", toml.display(), name),
                None => {}
            }
        }
    }
    svc.push_str("];\n");
    if count < 20 {
        panic!("ask/build.rs: read only {} service descriptions - the layout changed; refusing to ship an `ask` that cannot say what the services are", count);
    }
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("services.rs");
    std::fs::write(&dest, svc).unwrap();
}
