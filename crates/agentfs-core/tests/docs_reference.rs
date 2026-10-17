//! Checks the documentation against the code (design §4.2, §4.3):
//!
//! - every git config key agentfs reads ([`config::KEYS`]) has a row in
//!   its section of docs/configuration.md, and every key row there is a
//!   key agentfs reads;
//! - the same for the operator attributes ([`rules::OPERATOR_ATTRIBUTES`]);
//! - every relative link and `#anchor` in README.md and docs/ resolves;
//! - every CI job runs on the pinned runner, `ubuntu-24.04` (design §18);
//! - every script under ci/ is executable;
//! - every page under docs/ is in the book's docs/SUMMARY.md.
//!
//! The flags and environment variables are checked by the binaries' own
//! unit tests (`crates/agentfs/src/reference_tests.rs`, and the
//! `reference_tests` module of `crates/agentfs-csi/src/main.rs`), which
//! can walk their clap definitions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agentfs_core::{config, rules};

#[path = "../../agentfs/src/doc_tables.rs"]
mod doc_tables;

use doc_tables::Section;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn configuration() -> Vec<Section> {
    doc_tables::sections(&doc_tables::read_doc(
        env!("CARGO_MANIFEST_DIR"),
        "configuration.md",
    ))
}

/// The section of docs/configuration.md a key's row belongs in, and the
/// key as written in that table's first column.
fn home(key: &str) -> (&'static str, String) {
    if let Some(k) = key.strip_prefix("remote.<name>.") {
        ("`[remote \"<name>\"]`", k.to_owned())
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

/// The key cells of a section's tables: first cells that are exactly one
/// code span.
fn key_cells(section: &Section) -> BTreeSet<String> {
    section
        .rows()
        .filter_map(|row| doc_tables::sole_code_span(&row[0]).map(str::to_owned))
        .collect()
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

/// The `.rs` files under a crate's `src/`, relative to it, without the
/// `tests.rs` files of test-only submodules.
fn modules(krate: &str) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != "tests.rs")
            {
                let rel = path.strip_prefix(root).unwrap();
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let root = workspace().join("crates").join(krate).join("src");
    let mut out = BTreeSet::new();
    walk(&root, &root, &mut out);
    out
}

#[test]
fn every_module_is_in_the_architecture_map() {
    let sections = doc_tables::sections(&doc_tables::read_doc(
        env!("CARGO_MANIFEST_DIR"),
        "architecture.md",
    ));
    let mut problems = Vec::new();
    for krate in ["agentfs-core", "agentfs-fuse", "agentfs", "agentfs-csi"] {
        let heading = format!("`{krate}`");
        let Some(section) = doc_tables::section(&sections, &heading) else {
            problems.push(format!(
                "add a section headed ### {heading} with a module table"
            ));
            continue;
        };
        let documented = key_cells(section);
        let actual = modules(krate);
        assert!(actual.len() > 3, "no modules found for {krate}");
        for module in actual.difference(&documented) {
            problems.push(format!("{krate}: add a row for `{module}`"));
        }
        for module in documented.difference(&actual) {
            problems.push(format!(
                "{krate}: remove the row for `{module}`: no such file"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "docs/architecture.md's module map is out of date:\n  {}",
        problems.join("\n  ")
    );
}

/// Every markdown file the link check covers: README.md and docs/**.
fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let root = workspace();
    let mut files = vec![root.join("README.md")];
    walk(&root.join("docs"), &mut files);
    files
}

/// GitHub's anchor for a heading: lower case, everything but letters,
/// digits, `-`, `_` and spaces dropped, spaces turned into `-`; repeats
/// get `-1`, `-2`, ...
fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn anchors(markdown: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for section in doc_tables::sections(markdown) {
        if section.level == 0 {
            continue;
        }
        let base = slug(&section.heading);
        let n = seen.entry(base.clone()).or_default();
        out.insert(if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        });
        *n += 1;
    }
    out
}

/// The targets of inline links (`[text](target)`) outside code, and of
/// reference definitions (`[label]: target`).
fn links(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some((_, target)) = rest.split_once("]: ")
        {
            out.push(target.trim().to_owned());
            continue;
        }
        // Drop code spans, whose brackets are not links.
        let mut text = String::new();
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 0 {
                text.push_str(part);
            }
        }
        let mut rest = text.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            let target = after[..end].split_whitespace().next().unwrap_or("");
            out.push(target.to_owned());
            rest = &after[end + 1..];
        }
    }
    out
}

#[test]
fn relative_links_and_anchors_resolve() {
    let mut broken = Vec::new();
    let mut checked = 0;
    for file in markdown_files() {
        let markdown = std::fs::read_to_string(&file).unwrap();
        let shown = file
            .strip_prefix(workspace())
            .unwrap_or(&file)
            .display()
            .to_string();
        for target in links(&markdown) {
            if target.is_empty()
                || target.contains("://")
                || target.starts_with("mailto:")
                || target.starts_with('<')
            {
                continue;
            }
            checked += 1;
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            let resolved = if path.is_empty() {
                file.clone()
            } else {
                file.parent().unwrap().join(path)
            };
            if !resolved.exists() {
                broken.push(format!("{shown}: {target}: no such file"));
                continue;
            }
            let Some(anchor) = anchor else { continue };
            if resolved.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let anchors = anchors(&std::fs::read_to_string(&resolved).unwrap());
            if !anchors.contains(anchor) {
                broken.push(format!("{shown}: {target}: no heading with that anchor"));
            }
        }
    }
    assert!(
        checked > 50,
        "only {checked} links found: is the parser broken?"
    );
    assert!(
        broken.is_empty(),
        "broken links:\n  {}",
        broken.join("\n  ")
    );
}

#[test]
fn slugs_follow_github() {
    assert_eq!(slug("`[remote \"<name>\"]`"), "remote-name");
    assert_eq!(slug("4. Flags and environment"), "4-flags-and-environment");
    assert_eq!(slug("SSH remotes"), "ssh-remotes");
    assert_eq!(
        slug("Restores with fronts (§14)"),
        "restores-with-fronts-14"
    );
    let a = anchors("# A\n## Same\n## Same\n");
    assert!(a.contains("same") && a.contains("same-1"), "{a:?}");
    assert_eq!(
        links("see [x](a.md#b) and `[y](not)` [z]: c.md\n[ref]: d.md\n"),
        ["a.md#b", "d.md"]
    );
}

/// The runner every CI job uses (design §18). Pinned, not `-latest`, so a
/// new image cannot change the kernel, FUSE, kind or nginx under the
/// tests.
const RUNNER: &str = "ubuntu-24.04";

#[test]
fn workflows_pin_runners() {
    let dir = workspace().join(".github/workflows");
    let mut problems = Vec::new();
    let mut jobs = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for (n, line) in text.lines().enumerate() {
            let Some(value) = line.trim().strip_prefix("runs-on:") else {
                continue;
            };
            jobs += 1;
            let value = value.split('#').next().unwrap_or("").trim();
            if value != RUNNER {
                problems.push(format!("{name}:{}: runs-on: {value} (use {RUNNER})", n + 1));
            }
        }
        if text.contains("ubuntu-latest") {
            problems.push(format!("{name} mentions ubuntu-latest"));
        }
    }
    assert!(jobs > 0, "no runs-on lines found in {}", dir.display());
    // The docs name the runner the numbers were measured on. A line may
    // name ubuntu-latest only to explain the pin.
    for path in markdown_files() {
        if path.components().any(|c| c.as_os_str() == "reviews") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.contains("ubuntu-latest") && !line.contains(RUNNER) {
                problems.push(format!(
                    "{}:{}: names ubuntu-latest; CI runs on {RUNNER}",
                    path.display(),
                    n + 1
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Workflows and other scripts run the CI scripts directly, so each needs
/// its execute bit (a sourced library too, so none is missed).
#[test]
fn ci_scripts_are_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mut files = Vec::new();
    files_under(&workspace().join("ci"), &mut files);
    let scripts: Vec<_> = files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sh" || e == "py"))
        .collect();
    assert!(scripts.len() > 10, "no ci scripts found: {scripts:?}");
    let offenders: Vec<_> = scripts
        .iter()
        .filter(|p| std::fs::metadata(p).unwrap().permissions().mode() & 0o111 == 0)
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "not executable (git update-index --chmod=+x):\n{}",
        offenders.join("\n")
    );
}

/// The book (book.toml) renders only the pages docs/SUMMARY.md lists, so a
/// page left out of it is unreachable there and every link to it breaks.
#[test]
fn every_page_is_in_the_book() {
    let docs = workspace().join("docs");
    let summary = std::fs::read_to_string(docs.join("SUMMARY.md")).unwrap();
    let listed: BTreeSet<String> = links(&summary).into_iter().collect();
    let missing: Vec<_> = markdown_files()
        .iter()
        .filter_map(|p| p.strip_prefix(&docs).ok())
        .map(|p| p.display().to_string())
        .filter(|p| p != "SUMMARY.md" && !listed.contains(p))
        .collect();
    assert!(
        missing.is_empty(),
        "pages missing from docs/SUMMARY.md:\n  {}",
        missing.join("\n  ")
    );
}
