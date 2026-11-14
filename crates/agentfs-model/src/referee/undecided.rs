//! The referee's undecided versions: at a landing, the change's own versions by
//! file; at a sync, each fork version the upstream has not decided yet (R3a,
//! R3d), unless the upstream holds that same version (R3e.2).

use std::collections::{BTreeMap, BTreeSet};

use super::{Incoming, Lineage, Lines, Referee};
use crate::content::{Content, Line};

/// One file a sync changes: the fork's file, the upstream's, and the fork's
/// versions it keeps (undecided).
pub(super) struct Own {
    pub(super) path: String,
    pub(super) mine: Content,
    pub(super) theirs: Content,
    pub(super) keep: Vec<Line>,
}

impl<'i> Lines<'i> {
    /// A landing's undecided versions: every version of the change, by file.
    pub(super) fn undecided(incoming: &'i [Incoming]) -> Self {
        let mut by_path: BTreeMap<&str, Vec<&Incoming>> = BTreeMap::new();
        for i in incoming {
            by_path.entry(&i.path).or_default().push(i);
        }
        Lines {
            by_path,
            verdicts: BTreeMap::new(),
            by_file: BTreeSet::new(),
        }
    }
}

impl<L: Lineage + ?Sized> Referee<'_, L> {
    /// A sync's undecided versions, per file `up` changes from `ours`.
    pub(super) fn undecided(&self, ours: &str, up: &str, changed: BTreeSet<String>) -> Vec<Own> {
        let mut out = Vec::new();
        for p in changed {
            let (mine, theirs) = (self.file(ours, &p), self.file(up, &p));
            let keep: Vec<Line> = mine
                .records()
                .filter(|r| {
                    theirs.member(&r.line).is_none_or(|u| u.id != r.id)
                        && !self.is_ancestor(&self.origin_of(ours, &p, &r.id), up)
                })
                .cloned()
                .collect();
            out.push(Own {
                path: p,
                mine,
                theirs,
                keep,
            });
        }
        out
    }
}
