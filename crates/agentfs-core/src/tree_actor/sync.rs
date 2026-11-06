//! Pure snapshot sync (target git-model §8). Queue ordering and locks belong
//! to the actor; the caller determines undecided versions from commit ancestry.

use std::collections::{BTreeMap, BTreeSet};

use crate::tree_reconcile::{self, LineMap};

use super::referee::{File, Snapshot};
use super::{Error, MergeFileIdentity, NameHistory, Result};

fn path(history: &NameHistory) -> Result<Option<&str>> {
    Ok(history
        .versions
        .get(&history.head)
        .ok_or(Error::Format)?
        .path
        .as_deref())
}

fn combine(parent: &NameHistory, fork: &NameHistory) -> Result<NameHistory> {
    let mut result = parent.clone();
    for (id, version) in &fork.versions {
        if let Some(old) = result.versions.get(id) {
            if old.path != version.path
                || old.over != version.over
                || old.replaced_by != version.replaced_by
            {
                return Err(Error::Format);
            }
        } else {
            result.versions.insert(*id, version.clone());
        }
    }
    Ok(result)
}

/// Bring the parent's current snapshot into the fork without deciding any
/// fork version. Name keys are scoped to the file; line IDs are volume-unique.
pub(super) fn sync(
    parent: &Snapshot,
    fork: &Snapshot,
    undecided_lines: &BTreeSet<String>,
    undecided_names: &BTreeSet<(MergeFileIdentity, u64)>,
) -> Result<Snapshot> {
    let mut names = BTreeMap::new();
    let ids: BTreeSet<_> = parent
        .names
        .keys()
        .chain(fork.names.keys())
        .copied()
        .collect();
    for id in ids {
        let p = parent.names.get(&id);
        let f = fork.names.get(&id);
        if p.is_none() && f.is_some_and(|f| !undecided_names.contains(&(id, f.head))) {
            continue;
        }
        let mut name = match (p, f) {
            (Some(p), Some(f)) => combine(p, f)?,
            (Some(p), None) => p.clone(),
            (None, Some(f)) => f.clone(),
            (None, None) => unreachable!(),
        };
        if let Some(f) = f
            && (p.is_none() || undecided_names.contains(&(id, f.head)))
        {
            name.head = f.head;
        }
        path(&name)?;
        names.insert(id, name);
    }

    // A parent name that clashes with an undecided fork name falls back to
    // the name that file had in the fork. Repeat for chains of displaced names.
    loop {
        let mut occupied: BTreeMap<String, Vec<MergeFileIdentity>> = BTreeMap::new();
        for (&id, name) in &names {
            if let Some(p) = path(name)? {
                occupied.entry(p.to_owned()).or_default().push(id);
            }
        }
        let Some(ids) = occupied.values().find(|ids| ids.len() > 1) else {
            break;
        };
        // Both already show their fork names: the input fork is invalid.
        let id = ids
            .iter()
            .find(|id| fork.names.get(id).map(|h| h.head) != Some(names[id].head))
            .copied()
            .ok_or(Error::Format)?;
        if let Some(f) = fork.names.get(&id) {
            names.get_mut(&id).ok_or(Error::Format)?.head = f.head;
        } else {
            // The fork never had this file; it is hidden until the clash ends.
            names.remove(&id);
        }
    }

    let mut files = BTreeMap::new();
    for (&id, name) in &names {
        if path(name)?.is_none() {
            continue;
        }
        let p = parent.files.get(&id);
        let f = fork.files.get(&id);
        let file = match (p, f) {
            (Some(p), Some(f)) => File {
                object: f.object,
                lines: tree_reconcile::sync(&p.lines, &f.lines, undecided_lines)?,
            },
            (Some(p), None) => File {
                object: p.object,
                lines: tree_reconcile::sync(&p.lines, &LineMap::default(), undecided_lines)?,
            },
            (None, Some(f)) => File {
                object: f.object,
                lines: tree_reconcile::sync(&LineMap::default(), &f.lines, undecided_lines)?,
            },
            (None, None) => return Err(Error::Format),
        };
        files.insert(id, file);
    }
    Ok(Snapshot { files, names })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_reconcile::Line;

    fn file(id: u64, path: &str, version: &str, bytes: &[u8]) -> Snapshot {
        let identity = MergeFileIdentity(id);
        let line = Line {
            identity: format!("line{id}"),
            version: version.into(),
            bytes: Some(bytes.into()),
        };
        Snapshot {
            names: [(identity, NameHistory::created(id, path.into()))]
                .into_iter()
                .collect(),
            files: [(
                identity,
                File {
                    object: id,
                    lines: LineMap {
                        lines: vec![line],
                        ancestors: [(version.into(), None)].into_iter().collect(),
                    },
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn undecided_line_is_kept_then_parent_decision_arrives() {
        let mut parent = file(1, "a", "base", b"old\n");
        let mut fork = parent.clone();
        parent
            .files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .version = "parent".into();
        parent
            .files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .bytes = Some(b"parent\n".to_vec());
        parent
            .files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert("parent".into(), Some("base".into()));
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .version = "fork".into();
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .bytes = Some(b"fork\n".to_vec());
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert("fork".into(), Some("base".into()));
        let kept = sync(
            &parent,
            &fork,
            &["fork".into()].into_iter().collect(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(kept.files[&MergeFileIdentity(1)].lines.bytes(), b"fork\n");
        let decided = sync(&parent, &kept, &BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            decided.files[&MergeFileIdentity(1)].lines.bytes(),
            b"parent\n"
        );
    }

    #[test]
    fn unfinished_parent_tail_does_not_acquire_a_phantom_newline() {
        let parent = file(1, "a", "parent", b"partial");
        let mut fork = parent.clone();
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines
            .push(Line {
                identity: "insert".into(),
                version: "insert".into(),
                bytes: Some(b"own\n".to_vec()),
            });
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert("insert".into(), None);
        let result = sync(
            &parent,
            &fork,
            &["insert".into()].into_iter().collect(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(result.files[&MergeFileIdentity(1)].lines.bytes(), b"own\n");
        let result = sync(&parent, &result, &BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            result.files[&MergeFileIdentity(1)].lines.bytes(),
            b"partial"
        );
    }

    #[test]
    fn names_fall_back_through_a_chain_then_take_parent_decision() {
        let mut fork = file(1, "a", "one", b"one\n");
        for snapshot in [
            file(2, "b", "two", b"two\n"),
            file(3, "c", "three", b"three\n"),
        ] {
            fork.names.extend(snapshot.names);
            fork.files.extend(snapshot.files);
        }
        let mut parent = fork.clone();
        fork.names
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .advance(11, Some("target".into()), None);
        parent
            .names
            .get_mut(&MergeFileIdentity(2))
            .unwrap()
            .advance(12, Some("target".into()), None);
        parent
            .names
            .get_mut(&MergeFileIdentity(3))
            .unwrap()
            .advance(13, Some("b".into()), None);
        let pending = [(MergeFileIdentity(1), 11)].into_iter().collect();
        let result = sync(&parent, &fork, &BTreeSet::new(), &pending).unwrap();
        assert_eq!(
            path(&result.names[&MergeFileIdentity(1)]).unwrap(),
            Some("target")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(2)]).unwrap(),
            Some("b")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(3)]).unwrap(),
            Some("c")
        );
        let result = sync(&parent, &result, &BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            path(&result.names[&MergeFileIdentity(1)]).unwrap(),
            Some("a")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(2)]).unwrap(),
            Some("target")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(3)]).unwrap(),
            Some("b")
        );
    }
}
