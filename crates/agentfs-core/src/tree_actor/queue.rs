//! Queue and locks (target git-model §6): P1 eligibility, the keys open files hold, and the
//! `Cut` a prefix sync takes to.

use std::collections::BTreeSet;

use super::TreeId;
use super::tree::{MergeFileIdentity, Shape};

/// A file identity or a namespace slot involved in a change. Namespace slots
/// are included because distinct files may compete for the same name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::tree_actor) enum Key {
    /// A merge-file identity.
    File(MergeFileIdentity),
    /// A name slot which may be claimed by another file.
    Name(String),
}

/// Files and source of a queue entry, in arrival order.
#[derive(Clone, Debug)]
pub(in crate::tree_actor) struct Plan {
    /// The tree whose head the entry moves here: the upstream for a sync, a
    /// fork for a landing.
    pub source: TreeId,
    /// Identities and namespace slots the entry can read or change.
    pub files: BTreeSet<Key>,
}

/// Return the first entry that can take, respecting transitive dependencies
/// on earlier entries blocked by a writer (P1).
pub(in crate::tree_actor) fn eligible(plans: &[Plan], open: &BTreeSet<Key>) -> Option<usize> {
    let mut waiting_files = BTreeSet::new();
    let mut waiting_sources = BTreeSet::new();
    for (index, plan) in plans.iter().enumerate() {
        if plan.files.is_disjoint(open)
            && plan.files.is_disjoint(&waiting_files)
            && !waiting_sources.contains(&plan.source)
        {
            return Some(index);
        }
        waiting_files.extend(plan.files.iter().cloned());
        waiting_sources.insert(plan.source);
    }
    None
}

/// The keys a tree's write-opens hold (R9): each open object's identity and every path naming
/// it; an object no name reaches holds none.
pub(in crate::tree_actor) fn held(
    tree: &Shape,
    open_objects: impl IntoIterator<Item = u64>,
) -> BTreeSet<Key> {
    let mut keys = BTreeSet::new();
    for object in open_objects {
        if !tree.is_named(object) {
            continue;
        }
        if let Some(id) = tree.identities.get(&object) {
            keys.insert(Key::File(*id));
        }
        keys.extend(tree.paths_of(object).map(|path| Key::Name(path.to_owned())));
    }
    keys
}

/// The last upstream change a prefix sync takes (R9.3, R9.9, R3d). Its fields are private:
/// only [`cut`] makes one, and a sync is built only from a `Cut`.
pub(in crate::tree_actor) struct Cut<T> {
    taken: Option<T>,
    remaining: bool,
    keys: BTreeSet<Key>,
}

impl<T> Cut<T> {
    /// The last change taken, if any.
    pub(in crate::tree_actor) fn into_taken(self) -> Option<T> {
        self.taken
    }

    /// Whether a change after the cut waits for an open file: the queue entry stays.
    pub(in crate::tree_actor) fn remaining(&self) -> bool {
        self.remaining
    }

    /// What the take locks: the taken changes' keys; when the first change waits, that
    /// change's keys.
    pub(in crate::tree_actor) fn keys(&self) -> &BTreeSet<Key> {
        &self.keys
    }
}

/// Walks `changes` oldest first. `step` turns each into its result and the keys it writes,
/// given the last taken result (None before the first). Stops at the first change whose keys
/// meet `open` (P2) and asks for no later one.
///
/// # Errors
///
/// The first error of `step`.
pub(in crate::tree_actor) fn cut<C, T, E>(
    changes: impl IntoIterator<Item = C>,
    open: &BTreeSet<Key>,
    mut step: impl FnMut(Option<&T>, C) -> Result<(T, BTreeSet<Key>), E>,
) -> Result<Cut<T>, E> {
    let mut taken = None;
    let mut keys = BTreeSet::new();
    let mut remaining = false;
    for change in changes {
        let (result, writes) = step(taken.as_ref(), change)?;
        if !writes.is_disjoint(open) {
            remaining = true;
            if taken.is_none() {
                keys = writes;
            }
            break;
        }
        keys.extend(writes);
        taken = Some(result);
    }
    Ok(Cut {
        taken,
        remaining,
        keys,
    })
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    fn file(n: u64) -> Key {
        Key::File(MergeFileIdentity(n))
    }

    fn plan(source: u64, files: &[u64]) -> Plan {
        Plan {
            source: TreeId::Fork(source),
            files: files.iter().copied().map(file).collect(),
        }
    }

    #[test]
    fn waiting_dependencies_are_transitive_but_unrelated_source_goes_ahead() {
        let plans = [
            plan(1, &[1]),
            plan(2, &[1, 2]),
            plan(3, &[2]),
            plan(4, &[3]),
        ];
        let open = [file(1)].into_iter().collect();
        assert_eq!(eligible(&plans, &open), Some(3));
        assert_eq!(eligible(&plans, &BTreeSet::new()), Some(0));
    }

    #[test]
    fn same_source_cannot_overtake_even_for_another_file() {
        let plans = [plan(1, &[1]), plan(1, &[2]), plan(2, &[2])];
        let open = [file(1)].into_iter().collect();
        assert_eq!(eligible(&plans, &open), None);
    }

    /// Each change is its number and writes the one file of that number.
    fn cut_of(changes: &[u64], open: &[u64]) -> Cut<u64> {
        let open = open.iter().copied().map(file).collect();
        cut(changes.iter().copied(), &open, |_, n| {
            Ok::<_, Infallible>((n, [file(n)].into_iter().collect()))
        })
        .unwrap()
    }

    #[test]
    fn sync_stops_at_the_first_open_file_even_if_later_changes_are_free() {
        let blocked = cut_of(&[1, 2, 3], &[2]);
        assert!(blocked.remaining());
        assert_eq!(blocked.keys(), &[file(1)].into_iter().collect());
        assert_eq!(blocked.into_taken(), Some(1));
        let free = cut_of(&[1, 2, 3], &[]);
        assert!(!free.remaining());
        assert_eq!(
            free.keys(),
            &[file(1), file(2), file(3)].into_iter().collect()
        );
        assert_eq!(free.into_taken(), Some(3));
    }

    #[test]
    fn a_cut_blocked_at_its_first_change_locks_that_changes_keys() {
        let blocked = cut_of(&[1, 2], &[1]);
        assert!(blocked.remaining());
        assert_eq!(blocked.keys(), &[file(1)].into_iter().collect());
        assert_eq!(blocked.into_taken(), None);
        let empty = cut_of(&[], &[1]);
        assert!(!empty.remaining());
        assert!(empty.keys().is_empty());
        assert_eq!(empty.into_taken(), None);
    }

    #[test]
    fn a_cut_asks_for_no_change_after_the_stop() {
        let open = [file(2)].into_iter().collect();
        let mut asked = Vec::new();
        let result = cut([1, 2, 3, 4], &open, |last: Option<&u64>, n| {
            asked.push((last.copied(), n));
            Ok::<_, Infallible>((n, [file(n)].into_iter().collect()))
        })
        .unwrap();
        assert_eq!(asked, [(None, 1), (Some(1), 2)]);
        assert_eq!(result.into_taken(), Some(1));
    }

    #[test]
    fn a_steps_error_is_the_cuts() {
        let result = cut([1, 2], &BTreeSet::new(), |_, n: u64| {
            if n == 2 {
                Err(n)
            } else {
                Ok((n, BTreeSet::new()))
            }
        });
        assert!(matches!(result, Err(2)));
    }

    #[test]
    fn held_keys_are_an_open_objects_identity_and_names_and_none_unnamed() {
        let mut tree = Shape::default();
        tree.names.insert("a".into(), 5);
        tree.names.insert("b".into(), 5);
        tree.names.insert("c".into(), 6);
        tree.identities.insert(5, MergeFileIdentity(50));
        tree.identities.insert(6, MergeFileIdentity(60));
        tree.add_anonymous(7);
        assert_eq!(
            held(&tree, [5, 7]),
            [file(50), Key::Name("a".into()), Key::Name("b".into())]
                .into_iter()
                .collect()
        );
        assert!(held(&tree, [7]).is_empty());
        assert!(held(&tree, []).is_empty());
    }
}
