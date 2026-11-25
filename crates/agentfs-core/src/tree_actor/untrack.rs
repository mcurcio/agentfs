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
//!
//! The other way is not taken: an untracked file the rules would record if
//! it were new stays untracked (rules decide only for new names). The open
//! names each such path on stderr ([`TreeActor::unadopted`]), so a looser
//! rule that changes nothing for existing files is never silent.

use super::untracked::UntrackedArea;
use super::{Error, Kind, Result, TreeActor, TreeId};
use rustix::fs::FileType;

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
                .map(|(path, object)| (path.to_string(), *object))
                .collect();
            if marked.is_empty() {
                continue;
            }
            let area = self.areas.open(tree)?;
            for (path, object) in marked {
                let t = self.tree(tree)?;
                let meta = *t.meta_of(object).ok_or(Error::Format)?;
                let bytes = t.bytes.get(&object).ok_or(Error::Format)?;
                {
                    let writing = area.writing();
                    writing
                        .containers(&path, &mut |dir| t.dirs.get(dir).map(|state| state.meta))?;
                    writing.place(&path, &mut bytes.reader(), bytes.len(), meta)?;
                }
                agentfs_failpoints::hit("tree.untrack_after_place")?;
                self.unlink(tree, &path, &by)?;
                eprintln!(
                    "tree Untracked: tree={tree} path={path}: {}",
                    agentfs_rules::Untracked::NeverRecord
                );
            }
        }
        Ok(())
    }

    /// Every untracked entry the rules would place in the tree were it new,
    /// by tree: a file, link or directory in a tree's untracked area that no
    /// tracked entry hides and that no rule now leaves out. Directories the
    /// area holds as containers of tracked ones are walked; an untracked
    /// directory is judged as one entry, not walked.
    ///
    /// # Errors
    ///
    /// A failed read of an area.
    pub fn unadopted(&self) -> Result<Vec<(TreeId, String)>> {
        let mut found = Vec::new();
        for &tree in self.trees.keys() {
            let Some(area) = self.areas.existing(tree)? else {
                continue;
            };
            self.unadopted_in(tree, &area, "", false, &mut found)?;
        }
        Ok(found)
    }

    fn unadopted_in(
        &self,
        tree: TreeId,
        area: &UntrackedArea,
        dir: &str,
        inside_repository: bool,
        found: &mut Vec<(TreeId, String)>,
    ) -> Result<()> {
        let t = self.tree(tree)?;
        for (name, kind) in area.entries(dir)? {
            let path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if let Some(tracked) = t.kind_at(&path) {
                if tracked == Kind::Dir && kind == FileType::Directory {
                    let inside = inside_repository || area.stat(&format!("{path}/.git"))?.is_some();
                    self.unadopted_in(tree, area, &path, inside, found)?;
                }
                continue;
            }
            let kind = match kind {
                FileType::Directory => agentfs_rules::NewKind::Dir,
                FileType::Symlink => agentfs_rules::NewKind::Symlink,
                FileType::RegularFile => agentfs_rules::NewKind::File,
                // FIFOs, sockets and devices are untracked whatever the rules say.
                _ => continue,
            };
            let facts = agentfs_rules::PathFacts {
                parent_untracked: false,
                inside_repository,
            };
            if self.rules.place(&path, kind, facts) == agentfs_rules::Placement::Tracked {
                found.push((tree, path));
            }
        }
        Ok(())
    }
}
