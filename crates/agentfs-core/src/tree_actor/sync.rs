//! Pure snapshot sync (target git-model §8). Queue ordering and locks belong
//! to the actor; the caller determines undecided versions from commit ancestry.

use std::collections::{BTreeMap, BTreeSet};

use crate::tree_reconcile;

use super::referee::{File, Snapshot};
use super::tree::{EventNumber, LineVersion, Lines};
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

/// Whether the parent ended this file: it has the file's name history and no file.
pub(super) fn ended(parent: &Snapshot, id: MergeFileIdentity) -> Result<bool> {
    Ok(!parent.files.contains_key(&id)
        && match parent.names.get(&id) {
            Some(name) => path(name)?.is_none(),
            None => false,
        })
}

/// Bring the parent's current snapshot into the fork without deciding any
/// fork version. Name keys are scoped to the file; line IDs are volume-unique.
/// `last_lines` holds the parent's last lines of each file it ended that the
/// fork keeps under an undecided name: until the parent decides, that file reads
/// as any other, the parent's decided lines and the fork's undecided ones (R3a,
/// R3d, finding 44).
pub(super) fn sync(
    parent: &Snapshot,
    fork: &Snapshot,
    undecided_lines: &BTreeSet<LineVersion>,
    undecided_names: &BTreeSet<(MergeFileIdentity, EventNumber)>,
    last_lines: &BTreeMap<MergeFileIdentity, Lines>,
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
            // A file this tree ended by an unlink or a rename-over that lost comes
            // back as a new object, numbered by the event that ended it, which
            // numbers nothing else. Its old object stays with whoever holds it: an
            // old writer reaches no file (R11.3), and after an atomic save that
            // object is now the replaced file's (finding 39).
            (Some(p), None) => File {
                object: fork
                    .names
                    .get(&id)
                    .map_or(p.object, |ended| ended.head.object()),
                lines: tree_reconcile::sync(&p.lines, &Lines::default(), undecided_lines)?,
            },
            (None, Some(f)) => File {
                object: f.object,
                lines: tree_reconcile::sync(
                    last_lines.get(&id).unwrap_or(&Lines::default()),
                    &f.lines,
                    undecided_lines,
                )?,
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

    /// A line version for each name the cases use.
    fn v(name: &str) -> LineVersion {
        let event = [
            "line1", "line2", "line3", "base", "parent", "fork", "insert", "later", "own", "one",
            "two", "three",
        ]
        .iter()
        .position(|known| *known == name)
        .expect("a known name");
        format!("{}:1", event + 1).parse().unwrap()
    }

    fn file(id: u64, path: &str, version: &str, bytes: &[u8]) -> Snapshot {
        let identity = MergeFileIdentity(id);
        let line = Line {
            identity: v(&format!("line{id}")),
            version: v(version),
            bytes: Some(bytes.into()),
        };
        Snapshot {
            names: [(
                identity,
                NameHistory::created(EventNumber::new(id), path.into()),
            )]
            .into_iter()
            .collect(),
            files: [(
                identity,
                File {
                    object: id,
                    lines: Lines {
                        lines: vec![line],
                        ancestors: [(v(version), None)].into_iter().collect(),
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
            .version = v("parent");
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
            .insert(v("parent"), Some(v("base")));
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .version = v("fork");
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
            .insert(v("fork"), Some(v("base")));
        let kept = sync(
            &parent,
            &fork,
            &[v("fork")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(kept.files[&MergeFileIdentity(1)].lines.bytes(), b"fork\n");
        let decided = sync(
            &parent,
            &kept,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
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
                identity: v("insert"),
                version: v("insert"),
                bytes: Some(b"own\n".to_vec()),
            });
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert(v("insert"), None);
        let result = sync(
            &parent,
            &fork,
            &[v("insert")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(result.files[&MergeFileIdentity(1)].lines.bytes(), b"own\n");
        let result = sync(
            &parent,
            &result,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            result.files[&MergeFileIdentity(1)].lines.bytes(),
            b"partial"
        );
    }

    // R10.9: the parent's later line waits for the decision on the sandbox's
    // unfinished line, then the parent's version arrives whole.
    #[test]
    fn undecided_unfinished_line_hides_the_parents_later_line_until_decided() {
        let key = MergeFileIdentity(1);
        let mut parent = file(1, "a", "base", b"old\n");
        let mut fork = parent.clone();
        let lines = &mut parent.files.get_mut(&key).unwrap().lines;
        lines.lines.push(Line {
            identity: v("later"),
            version: v("later"),
            bytes: Some(b"later\n".to_vec()),
        });
        lines.ancestors.insert(v("later"), None);
        let lines = &mut fork.files.get_mut(&key).unwrap().lines;
        lines.lines[0].version = v("own");
        lines.lines[0].bytes = Some(b"own".to_vec());
        lines.ancestors.insert(v("own"), Some(v("base")));
        // Without the hide the sync would read `ownlater\n`.
        let kept = sync(
            &parent,
            &fork,
            &[v("own")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(kept.files[&key].lines.bytes(), b"own");
        let decided = sync(
            &parent,
            &kept,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(decided.files[&key].lines.bytes(), b"old\nlater\n");
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
        fork.names.get_mut(&MergeFileIdentity(1)).unwrap().advance(
            EventNumber::new(11),
            Some("target".into()),
            None,
        );
        parent
            .names
            .get_mut(&MergeFileIdentity(2))
            .unwrap()
            .advance(EventNumber::new(12), Some("target".into()), None);
        parent
            .names
            .get_mut(&MergeFileIdentity(3))
            .unwrap()
            .advance(EventNumber::new(13), Some("b".into()), None);
        let pending = [(MergeFileIdentity(1), EventNumber::new(11))]
            .into_iter()
            .collect();
        let result = sync(&parent, &fork, &BTreeSet::new(), &pending, &BTreeMap::new()).unwrap();
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
        let result = sync(
            &parent,
            &result,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
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
