//! Untrack at open: a tracked file or link the operator's `info/attributes`
//! now marks `-agentfs` leaves history when the volume opens, as `git rm
//! --cached` would take it out of the index while its file stays
//! (docs/trees.md, "Rules and the untracked area").
//!
//! For each such path of each tree, in this order: its saved bytes and
//! metadata are placed in the tree's untracked area (under its containers),
//! then its unlink is recorded as writer `agentfs`. A crash between the two
//! leaves the path tracked with a copy in the area; the next open places it
//! again, replacing that copy, and records the unlink. Directories stay
//! tracked: they are the containers of what is under them.

use super::{Error, Result, TreeActor, TreeId};

impl TreeActor {
    /// Moves every tracked file or link the rules never record into its
    /// tree's untracked area, and records its unlink as writer `agentfs`:
    /// what an open runs once recovery has closed every abandoned open. One
    /// stderr line per path: `tree Untracked: tree=<id> path=<path>: <reason>`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for a failed placement, and a failed unlink.
    pub(super) fn untrack_never_recorded(&mut self) -> Result<()> {
        let by = self.agentfs();
        let trees: Vec<TreeId> = self.trees.keys().copied().collect();
        for tree in trees {
            let marked: Vec<(String, u64)> = self
                .tree(tree)?
                .names
                .iter()
                .filter(|(path, _)| self.rules.never_records(path))
                .map(|(path, object)| (path.clone(), *object))
                .collect();
            if marked.is_empty() {
                continue;
            }
            let area = self.areas.open(tree)?;
            for (path, object) in marked {
                let t = self.tree(tree)?;
                let meta = *t.meta.get(&object).ok_or(Error::Format)?;
                let bytes = t.bytes.get(&object).ok_or(Error::Format)?;
                {
                    let writing = area.writing();
                    writing
                        .containers(&path, &mut |dir| t.dirs.get(dir).map(|state| state.meta))?;
                    writing.place(&path, &mut bytes.reader(), bytes.len(), meta)?;
                }
                crate::failpoints::hit("tree.untrack_after_place")?;
                self.unlink(tree, &path, &by)?;
                eprintln!(
                    "tree Untracked: tree={tree} path={path}: {}",
                    agentfs_rules::Untracked::NeverRecord
                );
            }
        }
        Ok(())
    }
}
