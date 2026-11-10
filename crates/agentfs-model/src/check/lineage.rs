//! What each version was written over: the checks' oracle for "nothing is
//! silently lost" and "no stale line reverts a newer one", read from the
//! version records in git's blobs, not from the model's replay (MODEL.md,
//! "What must always hold").

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::content::Content;
use crate::git::{git, try_git};

/// A version `b` descends from `a` when `b` was written over `a`: `a` is the
/// version `b`'s writer saw of that line, or one that version was written
/// over, and so on. From what the writer could read (R3c.2), never from which
/// commits its version merged: a sync that kept the fork's own line over the
/// upstream's did not show the upstream's version to the fork's writer.
#[derive(Default, Debug)]
pub(crate) struct Lineage {
    /// Each version, to every version a record of it names as its base.
    over: BTreeMap<String, BTreeSet<String>>,
    /// The blobs read so far.
    read: BTreeSet<String>,
}

impl Lineage {
    /// Version `v` was written over `over`, the version its writer saw
    /// (none: a new line).
    pub(crate) fn record(&mut self, v: &str, over: Option<&str>) {
        let bases = self.over.entry(v.to_string()).or_default();
        if let Some(o) = over {
            bases.insert(o.to_string());
        }
    }

    /// Records every version in every blob of `repo` not read yet, reachable
    /// or not (a rolled-back transaction's included): each file's blob holds
    /// every version's record beside its bytes.
    pub(crate) fn read_new(&mut self, repo: &Path) {
        let all = git(
            repo,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname) %(objecttype)",
            ],
        );
        let new: Vec<&str> = all
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(id, kind)| *kind == "blob" && !self.read.contains(*id))
            .map(|(id, _)| id)
            .collect();
        if new.is_empty() {
            return;
        }
        let out = try_git(
            repo,
            &["cat-file", "--batch"],
            Some(&(new.join("\n") + "\n")),
        );
        assert!(out.code == 0, "git cat-file --batch failed: {}", out.stderr);
        let mut rest = out.stdout.as_str();
        while let Some((header, body)) = rest.split_once('\n') {
            let size: usize = header
                .rsplit(' ')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or_else(|| panic!("unparsable cat-file header {header:?}"));
            let (blob, after) = body.split_at(size);
            if let Some(c) = Content::parse(blob) {
                for l in c.records() {
                    self.record(&l.id, l.over.as_deref());
                }
            }
            rest = after.strip_prefix('\n').unwrap_or(after);
        }
        self.read.extend(new.iter().map(|id| id.to_string()));
    }

    /// Whether `newer` descends from `older` (or is it), following every
    /// recorded base; a cycle (a version recorded over itself, or over one
    /// written over it) ends the walk.
    pub(crate) fn descends(&self, newer: &str, older: &str) -> bool {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut todo = vec![newer];
        while let Some(v) = todo.pop() {
            if v == older {
                return true;
            }
            if !seen.insert(v) {
                continue;
            }
            todo.extend(self.over.get(v).into_iter().flatten().map(String::as_str));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_follows_views() {
        let mut l = Lineage::default();
        l.record("m1.2", Some("a0"));
        l.record("m1.3", Some("m1.2"));
        l.record("c1.1", Some("a0"));
        l.record("c1.2", None);
        assert!(l.descends("m1.3", "a0") && l.descends("m1.3", "m1.2"));
        assert!(!l.descends("m1.2", "m1.3"));
        assert!(!l.descends("c1.1", "m1.2") && !l.descends("m1.2", "c1.1"));
        assert!(!l.descends("c1.2", "a0"));
    }

    #[test]
    fn a_cycle_ends_the_walk() {
        let mut l = Lineage::default();
        l.record("m1.2", Some("m1.3"));
        l.record("m1.3", Some("m1.2"));
        assert!(l.descends("m1.2", "m1.3") && !l.descends("m1.2", "a0"));
    }
}
