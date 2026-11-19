//! [`OperatorRules`] judges a corpus of paths as git does: `git check-ignore
//! --no-index -v` and `git check-attr agentfs`, with the `git` on the tree
//! engine's fixed PATH and the same `info/exclude` and `info/attributes` in a
//! temporary repository's git directory.

use std::path::Path;
use std::process::Command;

use agentfs_rules::{AttrState, OperatorRules};

const EXCLUDE: &str = "\
# comments and blank lines are skipped

*.log
!keep.log
/top-only
build/
!build/re-included
/*.tmp
docs/**/draft*
/allow/*
!/allow/yes/
Case.TXT
\\#literal
trailing\\ 
";

const ATTRIBUTES: &str = "\
*.db -agentfs
*.db-wal -agentfs
.env -agentfs
**/secrets/** -agentfs
keep.db !agentfs
*.md agentfs text
";

/// Paths, `/` at the end for a directory.
const CORPUS: &[&str] = &[
    "a.log",
    "keep.log",
    "d/keep.log",
    "d/a.log",
    "top-only",
    "d/top-only",
    "build/",
    "build/x",
    "build/re-included",
    "d/build/",
    "d/build/y",
    "x.tmp",
    "d/x.tmp",
    "docs/a/b/draft1",
    "docs/draft2",
    "allow/no",
    "allow/yes/",
    "allow/yes/file",
    "allow/no-dir/",
    "allow/no-dir/file",
    "Case.TXT",
    "case.txt",
    "#literal",
    "trailing ",
    "s.db",
    "d/s.db-wal",
    ".env",
    "d/.env",
    "x/secrets/y",
    "keep.db",
    "notes.md",
    "plain",
];

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .env_clear()
        .env("PATH", agentfs_align::GIT_PATH)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .current_dir(dir.join("work"))
        .env("GIT_DIR", dir.join("work/.git"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn operator_rules_agree_with_git() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let init = git(tmp.path(), &["init", "-q"]);
    assert!(init.status.success(), "{init:?}");
    let info = work.join(".git/info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("exclude"), EXCLUDE).unwrap();
    std::fs::write(info.join("attributes"), ATTRIBUTES).unwrap();
    // git reads a directory's kind from the work tree.
    for entry in CORPUS {
        let path = work.join(entry.trim_end_matches('/'));
        if entry.ends_with('/') {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"").unwrap();
        }
    }
    let rules = OperatorRules::parse(EXCLUDE.as_bytes(), ATTRIBUTES.as_bytes());
    let mut differ = Vec::new();
    for entry in CORPUS {
        let is_dir = entry.ends_with('/');
        let path = entry.trim_end_matches('/');
        let out = git(
            tmp.path(),
            &["check-ignore", "--no-index", "-v", "-n", path],
        );
        let text = String::from_utf8_lossy(&out.stdout);
        // `<source>:<line>:<pattern>\t<path>`, or `::\t<path>` for none; a
        // negative pattern that matched last means not ignored.
        let pattern = text
            .split('\t')
            .next()
            .and_then(|head| head.splitn(3, ':').nth(2))
            .filter(|p| !p.is_empty() && !p.starts_with('!'))
            .map(str::to_owned);
        let ours = rules.ignored(path, is_dir);
        if ours.is_some() != pattern.is_some() {
            differ.push(format!("ignore {entry}: git {pattern:?}, ours {ours:?}"));
        }
        let out = git(tmp.path(), &["check-attr", "agentfs", "--", path]);
        let text = String::from_utf8_lossy(&out.stdout);
        let value = text.trim_end().rsplit(": ").next().unwrap_or("").to_owned();
        let ours = match rules.attributes(path).get("agentfs") {
            None | Some(None) => "unspecified",
            Some(Some(AttrState::Set)) => "set",
            Some(Some(AttrState::Unset)) => "unset",
            Some(Some(AttrState::Value(_))) => "value",
        };
        if value != ours {
            differ.push(format!("attr {entry}: git {value}, ours {ours}"));
        }
    }
    assert!(differ.is_empty(), "{differ:#?}");
}
