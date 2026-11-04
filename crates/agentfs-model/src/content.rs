//! File content as git holds it: lines of bytes, nothing else. A save
//! commits the file exactly as its write calls left it; git merges it.
//!
//! The model gives every line a program writes unique bytes, its version id
//! (`<writer>.<n>`, plus `~t<T>` when staged in transaction T), so the
//! checks can tell versions apart by their bytes alone. Some lines repeat
//! other lines' bytes on purpose ([`REPEATED`]: blank lines, braces), as
//! real files do; every file starts as [`SLOTS`] such lines. Repeated bytes
//! are untracked: no check can say which of two identical lines a write
//! meant, and neither can git.
//!
//! [`Oracle`] is the checks' own record of who wrote what over what, kept
//! by the model at each write call and save, apart from git. No decision
//! reads it.

use std::collections::{BTreeMap, BTreeSet};

/// Blank lines each file starts with.
pub const SLOTS: usize = 6;
/// The files at main's first commit.
pub const PATHS: [&str; 4] = ["a", "b", "c", "d"];
/// The bytes of every root line.
pub const BLANK: &str = "-";
/// Bytes that repeat across lines, as blank lines and braces do.
pub const REPEATED: [&str; 2] = [BLANK, "}"];

/// A file: its lines' bytes, in order.
pub type File = Vec<String>;

/// File `p`'s root content: [`SLOTS`] blank lines, then a line naming the
/// file. That last line keeps the files' contents apart, so git's rename
/// detection can tell them apart (coherence finding 13 is what happens
/// without it).
pub fn root(p: &str) -> File {
    let mut f = vec![BLANK.to_string(); SLOTS];
    f.push(format!("@{p}"));
    f
}

/// Whether a line's bytes name one version (not a repeated line, nor a
/// root file's header).
pub fn tracked(line: &str) -> bool {
    !REPEATED.contains(&line) && !line.starts_with('@')
}

/// The writer a version id names.
pub fn writer_of(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
}

/// One version, as the oracle records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// Its bytes; none for a delete (of a line, or of a file's lines by an
    /// unlink or a rename over it).
    pub bytes: Option<String>,
    /// The version its writer saw there and wrote over.
    pub over: Option<String>,
    /// The commit that wrote it (a save).
    pub commit: String,
}

/// What each version was written over, and where its bytes came from: the
/// checks' account of "nothing is silently lost" and "no stale version
/// reverts a newer one", kept apart from git. A version `b` descends from
/// `a` when `b` was written over `a`: `a` is the version `b`'s writer saw
/// there, or one that version was written over, and so on.
///
/// A rewrite of an earlier read puts old bytes back: that is its writer's
/// new version (a revival), with its own id, holding those bytes. So one
/// byte string can hold several versions over time; which one a commit
/// holds is the latest whose writing commit (or a rebased copy of it) that
/// commit reaches (`World::version_in`).
#[derive(Default, Debug)]
pub struct Oracle {
    pub versions: BTreeMap<String, Version>,
    saw: BTreeMap<String, BTreeSet<String>>,
    /// Bytes to the versions holding them, oldest first.
    pub by_bytes: BTreeMap<String, Vec<String>>,
    /// A version to the deletes written over it.
    pub deletes: BTreeMap<String, Vec<String>>,
    /// Moves: a rename's record over each line it carried to a new path
    /// (no bytes, not a delete).
    pub moves: BTreeSet<String>,
}

impl Oracle {
    /// `v` was written by `commit` over `over` (none: a new line), holding
    /// `bytes` (none: a delete).
    pub fn wrote(&mut self, v: &str, bytes: Option<&str>, over: Option<&str>, commit: &str) {
        let mut seen = BTreeSet::new();
        if let Some(o) = over {
            seen.insert(o.to_string());
            seen.extend(self.saw.get(o).into_iter().flatten().cloned());
            if bytes.is_none() {
                self.deletes
                    .entry(o.to_string())
                    .or_default()
                    .push(v.to_string());
            }
        }
        self.saw.insert(v.to_string(), seen);
        if let Some(b) = bytes {
            self.by_bytes
                .entry(b.to_string())
                .or_default()
                .push(v.to_string());
        }
        self.versions.insert(
            v.to_string(),
            Version {
                bytes: bytes.map(str::to_string),
                over: over.map(str::to_string),
                commit: commit.to_string(),
            },
        );
    }

    /// Whether `newer` descends from `older` (or is it).
    pub fn descends(&self, newer: &str, older: &str) -> bool {
        newer == older || self.saw.get(newer).is_some_and(|s| s.contains(older))
    }

    /// Neither was written over the other.
    pub fn concurrent(&self, a: &str, b: &str) -> bool {
        !self.descends(a, b) && !self.descends(b, a)
    }

    /// `m`, written by `commit`, moved version `v` to another path.
    pub fn moved(&mut self, m: &str, v: &str, commit: &str) {
        let mut seen = BTreeSet::from([v.to_string()]);
        seen.extend(self.saw.get(v).into_iter().flatten().cloned());
        self.saw.insert(m.to_string(), seen);
        self.moves.insert(m.to_string());
        self.versions.insert(
            m.to_string(),
            Version {
                bytes: None,
                over: Some(v.to_string()),
                commit: commit.to_string(),
            },
        );
    }

    /// Whether `v` is a delete.
    pub fn is_delete(&self, v: &str) -> bool {
        !self.moves.contains(v) && self.versions.get(v).is_some_and(|x| x.bytes.is_none())
    }

    /// Every version written over `v`, directly or not (`v` excluded).
    pub fn over(&self, v: &str) -> impl Iterator<Item = &str> + '_ {
        let v = v.to_string();
        self.saw
            .iter()
            .filter(move |(_, s)| s.contains(&v))
            .map(|(d, _)| d.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descends_follows_what_each_writer_saw() {
        let mut o = Oracle::default();
        o.wrote("m1.2", Some("m1.2"), None, "c");
        o.wrote("m1.3", Some("m1.3"), Some("m1.2"), "c");
        o.wrote("c1.1", Some("c1.1"), None, "c");
        o.wrote("c1.2", None, Some("m1.3"), "c");
        assert!(o.descends("m1.3", "m1.2") && o.descends("c1.2", "m1.2"));
        assert!(!o.descends("m1.2", "m1.3") && o.concurrent("c1.1", "m1.2"));
        assert!(o.is_delete("c1.2") && o.deletes["m1.3"] == ["c1.2"]);
        assert_eq!(o.over("m1.2").collect::<Vec<_>>(), ["c1.2", "m1.3"]);
    }

    #[test]
    fn root_lines_are_untracked() {
        assert!(root("a").iter().all(|l| !tracked(l)) && tracked("c1.1"));
        assert_eq!(writer_of("c2.5~t3"), "c2");
    }
}
