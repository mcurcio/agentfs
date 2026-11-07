//! The rules ledger check (docs/target-architecture/rules-ledger.md): the
//! model may rest only on rules the owner wrote or approved. Each test reads
//! the ledger and the model's files from the repository and lists every
//! miss, so a failure names what to fix.

use std::fs;
use std::path::{Path, PathBuf};

/// One ledger rule: `- **<ID>** · <status> · <text>`, its continuation
/// lines joined to the text.
struct Rule {
    id: String,
    status: String,
    text: String,
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_dir() -> PathBuf {
    crate_dir()
        .join("../..")
        .canonicalize()
        .expect("the repository")
}

fn read(p: &Path) -> String {
    String::from_utf8_lossy(&fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
        .into_owned()
}

fn ledger() -> Vec<Rule> {
    let text = read(&repo_dir().join("docs/target-architecture/rules-ledger.md"));
    let mut rules: Vec<Rule> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("- **")
            && let Some((id, rest)) = rest.split_once("** · ")
            && let Some((status, rest)) = rest.split_once(" · ")
        {
            rules.push(Rule {
                id: id.to_string(),
                status: status.trim().to_string(),
                text: rest.to_string(),
            });
            open = true;
        } else if open && line.starts_with(' ') && !line.trim().is_empty() {
            let r = rules.last_mut().expect("an open rule");
            r.text.push(' ');
            r.text.push_str(line.trim());
        } else {
            open = false;
        }
    }
    assert!(!rules.is_empty(), "no rules parsed from the ledger");
    rules
}

/// The backticked names in `text` after `field` (to the end of the rule).
fn names_after(text: &str, field: &str) -> Vec<String> {
    let Some((_, rest)) = text.split_once(field) else {
        return Vec::new();
    };
    rest.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
            let p = e.expect("a directory entry").path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn shown(p: &Path) -> String {
    p.strip_prefix(repo_dir())
        .unwrap_or(p)
        .display()
        .to_string()
}

#[test]
fn no_rule_is_proposed() {
    let proposed: Vec<String> = ledger()
        .into_iter()
        .filter(|r| r.status == "proposed")
        .map(|r| r.id)
        .collect();
    assert!(
        proposed.is_empty(),
        "{} rules are proposed, not yet the owner's: {}",
        proposed.len(),
        proposed.join(", ")
    );
}

#[test]
fn rejected_rules_are_gone() {
    let all = files(&crate_dir());
    let mut found = Vec::new();
    for r in ledger().iter().filter(|r| r.status == "rejected") {
        for name in names_after(&r.text, "Gone when:") {
            for p in &all {
                for (n, line) in read(p).lines().enumerate() {
                    if line.contains(&name) {
                        found.push(format!("{} `{name}` at {}:{}", r.id, shown(p), n + 1));
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "rejected rules still in the model:\n  {}",
        found.join("\n  ")
    );
}

/// Each `rule N`, `rule Nx` and `rules N, M and Px` citation in `text`, as
/// (number, letter).
fn citations(text: &str) -> Vec<(String, Option<char>)> {
    let lower = text.to_lowercase();
    let b = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(k) = lower[i..].find("rule") {
        let start = i + k;
        i = start + 4;
        if start > 0 && b[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut j = i;
        if b.get(j) == Some(&b's') {
            j += 1;
        }
        if b.get(j) != Some(&b' ') {
            continue;
        }
        j += 1;
        loop {
            let d = j;
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            if j == d {
                break;
            }
            let number = lower[d..j].to_string();
            let letter = match (b.get(j), b.get(j + 1)) {
                (Some(c), next)
                    if c.is_ascii_lowercase() && !next.is_some_and(u8::is_ascii_alphanumeric) =>
                {
                    j += 1;
                    Some(*c as char)
                }
                _ => None,
            };
            out.push((number, letter));
            let rest = &lower[j..];
            let sep = [", and ", " and ", ", "].into_iter().find(|s| {
                rest.starts_with(s) && rest[s.len()..].starts_with(|c: char| c.is_ascii_digit())
            });
            match sep {
                Some(s) => j += s.len(),
                None => break,
            }
        }
        i = j.max(i);
    }
    out
}

#[test]
fn cited_rules_exist() {
    let ids: Vec<String> = ledger().into_iter().map(|r| r.id).collect();
    let known = |n: &str, letter: Option<char>| {
        ids.iter().any(|id| {
            let Some(rest) = id.strip_prefix('R').and_then(|r| r.strip_prefix(n)) else {
                return false;
            };
            let rest = rest.split('.').next().unwrap_or("");
            match letter {
                None => rest.is_empty() || rest.chars().all(|c| c.is_ascii_lowercase()),
                Some(l) => rest == l.to_string(),
            }
        })
    };
    let c = crate_dir();
    let mut sources = files(&c.join("src"));
    sources.extend(files(&c.join("tests")));
    sources.push(c.join("MODEL.md"));
    sources.push(repo_dir().join("docs/target-architecture/git-model.md"));
    let mut missing = Vec::new();
    for p in &sources {
        for (n, line) in read(p).lines().enumerate() {
            for (number, letter) in citations(line) {
                if !known(&number, letter) {
                    let cited = format!("{number}{}", letter.map(String::from).unwrap_or_default());
                    missing.push(format!("rule {cited} at {}:{}", shown(p), n + 1));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "cited rules with no ledger ID:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn named_tests_exist() {
    let mut fns = Vec::new();
    // A rule about the filesystem boundary (R1.4) is shown by the library's
    // tests: the model has no mappings. One the replay found (R8.2) is shown by the
    // replay's own tests, which hold the model and the library to one trace.
    let core_tests = repo_dir().join("crates/agentfs-core/tests");
    let replay_tests = repo_dir().join("crates/agentfs-conformance/tests");
    for p in [crate_dir().join("tests"), core_tests, replay_tests]
        .iter()
        .flat_map(|d| files(d))
    {
        if p.extension().is_some_and(|e| e == "rs") {
            for line in read(&p).lines() {
                if let Some((_, rest)) = line.split_once("fn ") {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    fns.push(name);
                }
            }
        }
    }
    let mut missing = Vec::new();
    for r in ledger() {
        for name in names_after(&r.text, "Tests:") {
            if !fns.contains(&name) {
                missing.push(format!("{}: `{name}`", r.id));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "tests the ledger names that do not exist:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn excuses_cite_approved_rules() {
    let rules = ledger();
    let p = crate_dir().join("src/check.rs");
    let mut bad = Vec::new();
    for (n, line) in read(&p).lines().enumerate() {
        let Some((_, rest)) = line.split_once("// excuse:") else {
            continue;
        };
        let id = rest.split_whitespace().next().unwrap_or("");
        let status = rules
            .iter()
            .find(|r| r.id == id)
            .map_or("not in the ledger", |r| r.status.as_str());
        if status != "owner" && status != "approved" {
            bad.push(format!("{}:{}: {id} ({status})", shown(&p), n + 1));
        }
    }
    assert!(
        bad.is_empty(),
        "check exceptions that cite no owner or approved rule:\n  {}",
        bad.join("\n  ")
    );
}
