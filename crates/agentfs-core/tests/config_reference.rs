//! Checks docs/configuration.md against the engine: every git config key
//! agentfs reads ([`config::KEYS`]) has a row in its section, and every key
//! row there is a key agentfs reads; the same for the operator attributes
//! ([`rules::OPERATOR_ATTRIBUTES`]). The checks over the whole repository
//! are `agentfs-repo`'s tests.

use std::collections::{BTreeMap, BTreeSet};

use agentfs_core::{config, rules};
use agentfs_repo::doc_tables::{self, Section, key_cells};

fn configuration() -> Vec<Section> {
    doc_tables::sections(&doc_tables::read_doc("configuration.md"))
}

/// The section of docs/configuration.md a key's row belongs in, and the
/// key as written in that table's first column.
fn home(key: &str) -> (&'static str, String) {
    if let Some(k) = key.strip_prefix("remote.<name>.") {
        ("`[remote \"<name>\"]`", k.to_owned())
    } else if let Some(k) = key.strip_prefix("agentfsPusher.<name>.") {
        ("`[agentfsPusher \"<name>\"]`", k.to_owned())
    } else if key.starts_with("agentfs.identity.") {
        ("`[agentfs \"identity\"]`", key.to_owned())
    } else if key.starts_with("agentfs.fork.") {
        ("`[agentfs \"fork\"]`", key.to_owned())
    } else if key.starts_with("agentfs.start.") {
        ("`[agentfs \"start\"]`", key.to_owned())
    } else if key.starts_with("agentfs.") {
        ("`[agentfs]`", key.to_owned())
    } else if key.starts_with("lfs.") {
        ("`[lfs]`", key.to_owned())
    } else if key.starts_with("http.") {
        ("`[http]`", key.to_owned())
    } else if key.starts_with("include") {
        ("Includes", key.to_owned())
    } else {
        panic!("config::KEYS has {key}, which has no section in this test: add one")
    }
}

#[test]
fn every_config_key_is_documented_in_its_section() {
    let sections = configuration();
    let mut expected: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for key in config::KEYS {
        let (heading, cell) = home(key);
        expected.entry(heading).or_default().insert(cell);
    }
    let mut problems = Vec::new();
    for (heading, keys) in &expected {
        let Some(section) = doc_tables::section(&sections, heading) else {
            problems.push(format!("add a section headed {heading} with a key table"));
            continue;
        };
        let documented = key_cells(section);
        for key in keys.difference(&documented) {
            problems.push(format!(
                "add a row `| `{key}` | ... |` to the table under {heading}"
            ));
        }
        for key in documented.difference(keys) {
            problems.push(format!(
                "remove the row for `{key}` under {heading}, or add it to config::KEYS if \
                 agentfs reads it"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "docs/configuration.md and config::KEYS disagree:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn every_operator_attribute_is_documented() {
    let sections = configuration();
    let section = doc_tables::section(&sections, "Operator attributes")
        .expect("docs/configuration.md has no `## Operator attributes` section");
    let documented = key_cells(section);
    let expected: BTreeSet<String> = rules::OPERATOR_ATTRIBUTES
        .iter()
        .map(|a| match *a {
            rules::RECORD_ATTR => format!("-{a}"),
            a => a.to_owned(),
        })
        .collect();
    let missing: Vec<_> = expected.difference(&documented).collect();
    let stale: Vec<_> = documented.difference(&expected).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "docs/configuration.md, Operator attributes: add rows for {missing:?}; remove (or \
         add to rules::OPERATOR_ATTRIBUTES) {stale:?}"
    );
}
