//! The referee's ancestry: what git answers about versions and commits
//! ([`Lineage`], the port the world implements), and whether a writer saw a
//! version (R3c.2).

use super::Referee;
use crate::content::{Content, Line};

/// Ancestry and commit reachability, over git in the model: every read the
/// referee makes. Each answer is about immutable objects, and the world caches
/// it. A version is written once, so the order of reads does not change an
/// answer, though `origin_of` keeps the answer of the first asker.
pub(crate) trait Lineage {
    /// File `p` (its identity) at a commit or tree.
    fn file(&self, at: &str, p: &str) -> Content;
    /// Whether commit `a` is `b` or an ancestor of it.
    fn is_ancestor(&self, a: &str, b: &str) -> bool;
    /// The commit that wrote version `id` of file `p`, from commit `at`, which holds it.
    fn origin_of(&self, at: &str, p: &str, id: &str) -> String;
    /// The versions version `v` of file `p` (written by commit `origin`) was written over.
    fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String>;
}

impl<L: Lineage + ?Sized> Referee<'_, L> {
    pub(super) fn file(&self, at: &str, p: &str) -> Content {
        self.git.file(at, p)
    }

    pub(super) fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git.is_ancestor(a, b)
    }

    pub(super) fn origin_of(&self, at: &str, p: &str, id: &str) -> String {
        self.git.origin_of(at, p, id)
    }

    pub(super) fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String> {
        self.git.written_over(origin, p, v)
    }

    /// Whether the writer of commit `tip` (its file `p` is `t`) saw the
    /// upstream's version `q`: `tip` holds it, or a version written over it.
    pub(super) fn seen(&self, tip: &str, t: &Content, p: &str, q: &Line) -> bool {
        t.member(&q.line).is_some_and(|v| {
            v.id == q.id
                || self
                    .written_over(&self.origin_of(tip, p, &v.id), p, v)
                    .contains(&q.id)
        })
    }
}
