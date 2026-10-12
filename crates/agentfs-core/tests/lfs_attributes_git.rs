//! The managed `.gitattributes` block (design §7) against the real `git`
//! binary and against gix-attributes, which agentfs's own rules use.
//!
//! A root `.gitattributes` composed by `agentfs_core::lfs::attributes` must
//! give `filter=lfs diff=lfs merge=lfs -text` to exactly the managed
//! paths: names with spaces, glob characters, `#`, `!`, backslashes and
//! non-ASCII, and none of their look-alikes. The one documented superset:
//! `[[:space:]]` also matches a tab, so `a\tb` matches the line for
//! `a b` (paths with tabs are never stored as LFS themselves).
//!
//! Skipped, with a message, when `git` is not installed.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use agentfs_core::lfs::attributes::{compose, split};
use agentfs_core::paths::RepoPath;
use agentfs_core::rules::{AttrState, RuleOptions, Rules};

const MANAGED: &[&str] = &[
    "big.bin",
    "a b/c d.bin",
    "a b",
    "st*r.bin",
    "q?.bin",
    "[x].bin",
    "x].bin",
    "#hash.bin",
    "!bang.bin",
    "back\\slash.bin",
    "ünïcødé/日本.bin",
    "deep/er/still/file.psd",
];

const LOOK_ALIKES: &[&str] = &[
    "sub/big.bin",
    "big.bin.x",
    "big.binx",
    "stXr.bin",
    "star.bin",
    "q1.bin",
    "x.bin",
    "[x]bin",
    "hash.bin",
    "bang.bin",
    "backslash.bin",
    "back/slash.bin",
    "a b/c d.binx",
    "a_b/c d.bin",
    "ab/cd.bin",
    "deep/er/file.psd",
    "unicode/日本.bin",
];

fn have_git() -> bool {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            eprintln!("using {}", String::from_utf8_lossy(&out.stdout).trim());
            true
        }
        _ => {
            eprintln!("skipped: git is not installed");
            false
        }
    }
}

/// `git check-attr filter diff merge text` for `paths` in the repository at
/// `dir`: `(path, attr) -> value`.
fn check_attr(dir: &Path, paths: &[&str]) -> Vec<(String, String, String)> {
    let mut child = Command::new("git")
        .args([
            "check-attr",
            "-z",
            "--stdin",
            "filter",
            "diff",
            "merge",
            "text",
        ])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("running git");
    {
        use std::io::Write as _;
        let mut input = child.stdin.take().unwrap();
        for path in paths {
            input.write_all(path.as_bytes()).unwrap();
            input.write_all(b"\0").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git check-attr failed");
    let fields: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .map(|f| String::from_utf8(f.to_vec()).unwrap())
        .collect();
    fields
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| (c[0].clone(), c[1].clone(), c[2].clone()))
        .collect()
}

fn composed(user: &[u8]) -> Vec<u8> {
    let paths: BTreeSet<RepoPath> = MANAGED.iter().map(|p| RepoPath::new(*p).unwrap()).collect();
    let content = compose(user, &paths);
    assert_eq!(split(&content).1, paths, "the block reads back exactly");
    content
}

#[test]
fn git_gives_lfs_attributes_to_exactly_the_managed_paths() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
    // The workload's own lines come first; the block's lines win.
    let user = b"*.bin -text diff=binary\n*.psd filter=other\n";
    std::fs::write(dir.path().join(".gitattributes"), composed(user)).unwrap();

    let mut all: Vec<&str> = MANAGED.to_vec();
    all.extend_from_slice(LOOK_ALIKES);
    all.push("a\tb");
    let results = check_attr(dir.path(), &all);
    assert_eq!(results.len(), all.len() * 4);
    for (path, attr, value) in results {
        let managed = MANAGED.contains(&path.as_str()) || path == "a\tb";
        let expected = match (managed, attr.as_str()) {
            (true, "text") => "unset",
            (true, _) => "lfs",
            (false, "filter") if path.ends_with(".psd") => "other",
            (false, "filter") => "unspecified",
            (false, "text") if path.ends_with(".bin") => "unset",
            (false, "diff") if path.ends_with(".bin") => "binary",
            (false, _) => "unspecified",
        };
        assert_eq!(value, expected, "{path:?} {attr}");
    }
}

#[test]
fn gix_attributes_agree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitattributes"), composed(b"")).unwrap();
    let options = RuleOptions {
        respect_tree_rules: true,
        ..RuleOptions::default()
    };
    let rules = Rules::new(dir.path(), options, b"", b"");
    let lfs = AttrState::Value("lfs".into());
    for path in MANAGED.iter().chain(LOOK_ALIKES) {
        let attrs = rules.attributes(&RepoPath::new(*path).unwrap()).unwrap();
        let managed = MANAGED.contains(path);
        assert_eq!(attrs.get("filter") == Some(&lfs), managed, "{path:?}");
        assert_eq!(attrs.get("merge") == Some(&lfs), managed, "{path:?}");
        if managed {
            assert_eq!(attrs.get("text"), Some(&AttrState::Unset), "{path:?}");
        }
    }
}
