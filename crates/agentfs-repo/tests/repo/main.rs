//! Checks over the whole repository, one program in three modules:
//!
//! - [`docs`]: every member's modules are in docs/architecture.md's module
//!   map, and its "Ranks" table is the workspace's; every relative link and
//!   `#anchor` in README.md and docs/ resolves; every path under `crates/`
//!   that a tracked file names (Rust: comments only; records of past work
//!   excepted) exists; every page under docs/ is in the book's
//!   docs/SUMMARY.md; every `git push` in a runbook writes only refs
//!   literally under `refs/agentfs/` and never forces (F8 S2).
//! - [`ci`]: every CI job runs on the pinned runner, `ubuntu-24.04`, runs
//!   its tests with cargo-nextest, pinned to one release, and pulls Docker
//!   Hub through the mirror; the Rust release is pinned once; the
//!   Dockerfile's dependency-cache stage stubs every workspace member;
//!   every script under ci/ is executable, registers its EXIT cleanup with
//!   `on_exit`, and no pipe in one ends in a reader that exits early
//!   (pipefail turns its SIGPIPE into a failure); publishing an image
//!   retries a failed push; every path filter of a workflow names a path
//!   that exists.
//! - [`crates`]: every dependency follows the ranks, the library front
//!   `agentfs` re-exports, and only re-exports, every library of the ranks
//!   it uses but the recording engine, only the service crate (with the
//!   front and the tests) depends on the domains it composes, every `failpoints`
//!   feature is forwarded, every cargo command in `.github/`, the scripts,
//!   docs, the crates' doc comments and Python scripts names packages,
//!   features, binaries and test programs the workspace has, and in a file
//!   that runs (not markdown, not Rust) selects each by a literal name and
//!   builds only packages with a binary;
//!   every failpoint call is in `agentfs_failpoints::POINTS`, and the tree
//!   engine (agentfs-engine) starts `git` only through
//!   `agentfs_git::git_cli::counted`.
//!
//! The configuration keys and operator attributes are checked by
//! `agentfs-core`'s `tests/config_reference.rs`; the flags and environment
//! variables by the binaries' own unit tests
//! (`crates/agentfs-cli/src/reference_tests.rs`, and the `reference_tests`
//! module of `crates/agentfs-csi/src/main.rs`), which can walk their clap
//! definitions.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

mod ci;
mod crates;
mod docs;

pub(crate) use agentfs_repo::workspace::root;

/// Every file under `dir`, recursively.
pub(crate) fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Why `entry`, a path a check read (a workflow's path filter, a path
/// under `crates/` a text names), names nothing among `paths` (every path
/// under the root, `/`-separated): without a `*` it must be one of them;
/// with one, the text before the first `*` must start one of them.
pub(crate) fn path_problem(entry: &str, paths: &BTreeSet<String>) -> Option<String> {
    match entry.split_once('*') {
        None if paths.contains(entry) => None,
        None => Some(format!("{entry}: no such path")),
        Some((prefix, _)) if paths.iter().any(|p| p.starts_with(prefix)) => None,
        Some((prefix, _)) => Some(format!("{entry}: no path starts with {prefix:?}")),
    }
}

/// Every file and directory under the root, `/`-separated, but `target/`
/// and `.git/`.
pub(crate) fn repository_paths() -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let shown = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if shown == "target" || shown == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            }
            out.insert(shown);
        }
    }
    let root = root();
    let mut out = BTreeSet::new();
    walk(&root, &root, &mut out);
    out
}

/// Every markdown file the link check covers: README.md and docs/**.
pub(crate) fn markdown_files() -> Vec<PathBuf> {
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
    let root = root();
    let mut files = vec![root.join("README.md")];
    walk(&root.join("docs"), &mut files);
    files
}

/// Whether [`lex`] ends a line at a `#` that starts a word.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comments {
    /// A `#` starting a word ends the line, as sh reads it.
    End,
    /// Comments are read like any other words.
    Read,
}

/// The shell words of `line`: split on blanks outside quotes, with the
/// quotes removed (`$VAR` is left as written, so a destination is checked
/// as the reader sees it). `;`, `|`, `&`, `(`, `)` and `` ` `` outside
/// quotes are words of their own, separators.
pub(crate) fn lex(line: &str, comments: Comments) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut in_word = false;
    for c in line.chars() {
        match (quote, c) {
            (None, '#') if !in_word && comments == Comments::End => break,
            (None, ' ' | '\t') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, ';' | '|' | '&' | '(' | ')' | '`') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                words.push(c.to_string());
            }
            (_, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Whether `word` is one [`lex`] makes of a separator.
pub(crate) fn is_separator(word: &str) -> bool {
    matches!(word, ";" | "|" | "&" | "(" | ")" | "`")
}
