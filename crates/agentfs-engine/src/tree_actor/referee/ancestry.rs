//! The one ancestry walk and the one union of line ancestries, for names and lines alike
//! (docs/target-architecture/git-model.md §7).

use std::collections::{BTreeMap, BTreeSet};

use crate::tree_actor::tree::{EventNumber, LineVersion, NameHistory};
use crate::tree_actor::{Error, Result};

/// Versions, each written over at most one other.
pub(in crate::tree_actor) trait Graph {
    type Version: Copy + Ord;
    /// What `v` was written over: `Ok(None)` for a first version; [`Broken::Missing`] if `v` is
    /// not on record.
    fn over(&self, v: Self::Version) -> std::result::Result<Option<Self::Version>, Broken>;
}

/// A file's line ancestry: each line version and the version it was written over.
pub(in crate::tree_actor) type Ancestors = BTreeMap<LineVersion, Option<LineVersion>>;

impl Graph for Ancestors {
    type Version = LineVersion;

    fn over(&self, v: LineVersion) -> std::result::Result<Option<LineVersion>, Broken> {
        self.get(&v).copied().ok_or(Broken::Missing)
    }
}

impl Graph for NameHistory {
    type Version = EventNumber;

    fn over(&self, v: EventNumber) -> std::result::Result<Option<EventNumber>, Broken> {
        self.versions
            .get(&v)
            .map(|version| version.over)
            .ok_or(Broken::Missing)
    }
}

/// An ancestry that is not a forest: history on record is damaged.
#[derive(Debug, thiserror::Error)]
pub(in crate::tree_actor) enum Broken {
    #[error("cyclic ancestry")]
    Cycle,
    #[error("missing ancestor")]
    Missing,
}

/// Whether `v` is `ancestor` or was written over it, at any depth.
pub(in crate::tree_actor) fn descends<G: Graph + ?Sized>(
    g: &G,
    mut v: G::Version,
    ancestor: G::Version,
) -> std::result::Result<bool, Broken> {
    let mut seen = BTreeSet::new();
    loop {
        if v == ancestor {
            return Ok(true);
        }
        if !seen.insert(v) {
            return Err(Broken::Cycle);
        }
        match g.over(v)? {
            Some(previous) => v = previous,
            None => return Ok(false),
        }
    }
}

/// Both graphs; [`Error::Format`] where they record one version over two others (a version is
/// written once).
pub(in crate::tree_actor) fn union(a: &Ancestors, b: &Ancestors) -> Result<Ancestors> {
    let mut graph = a.clone();
    for (version, over) in b {
        if graph.get(version).is_some_and(|existing| existing != over) {
            return Err(Error::Format);
        }
        graph.insert(*version, *over);
    }
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> LineVersion {
        text.parse().unwrap()
    }

    #[test]
    fn descends_walks_to_any_depth_and_refuses_a_broken_graph() {
        let graph: Ancestors = [
            (v("1:1"), None),
            (v("2:1"), Some(v("1:1"))),
            (v("3:1"), Some(v("2:1"))),
        ]
        .into_iter()
        .collect();
        assert!(descends(&graph, v("3:1"), v("1:1")).unwrap());
        assert!(descends(&graph, v("2:1"), v("2:1")).unwrap());
        assert!(!descends(&graph, v("1:1"), v("3:1")).unwrap());
        let cycle: Ancestors = [(v("1:1"), Some(v("2:1"))), (v("2:1"), Some(v("1:1")))]
            .into_iter()
            .collect();
        assert!(matches!(
            descends(&cycle, v("1:1"), v("3:1")),
            Err(Broken::Cycle)
        ));
        let missing: Ancestors = [(v("2:1"), Some(v("1:1")))].into_iter().collect();
        assert!(matches!(
            descends(&missing, v("2:1"), v("3:1")),
            Err(Broken::Missing)
        ));
    }

    #[test]
    fn union_refuses_a_version_over_two_others() {
        let a: Ancestors = [(v("1:1"), None), (v("2:1"), Some(v("1:1")))]
            .into_iter()
            .collect();
        let b: Ancestors = [(v("1:1"), None), (v("3:1"), Some(v("1:1")))]
            .into_iter()
            .collect();
        assert_eq!(union(&a, &b).unwrap().len(), 3);
        let conflict: Ancestors = [(v("2:1"), None)].into_iter().collect();
        assert!(matches!(union(&a, &conflict), Err(Error::Format)));
    }
}
