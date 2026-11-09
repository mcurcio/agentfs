//! Lock eligibility for tree queues (target git-model §6).

use std::collections::BTreeSet;

/// A file identity or a namespace slot involved in a change. Namespace slots
/// are included because distinct files may compete for the same name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// A merge-file identity.
    File(u64),
    /// A name slot which may be claimed by another file.
    Name(String),
}

/// Files and source of a queue entry, in arrival order.
#[derive(Clone, Debug)]
pub struct Plan {
    /// The upstream tree or child that originated the entry.
    pub source: String,
    /// Identities and namespace slots the entry can read or change.
    pub files: BTreeSet<Key>,
}

/// Return the first entry that can take, respecting transitive dependencies
/// on earlier entries blocked by a writer (P1).
pub fn eligible(plans: &[Plan], open: &BTreeSet<Key>) -> Option<usize> {
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
        waiting_sources.insert(plan.source.clone());
    }
    None
}

/// Number of consecutive upstream changes deliverable before a change would
/// overwrite an open file (P2). The write sets are computed for this sync,
/// so a returned copy of the fork's own content does not count as a write.
pub fn prefix(changes: &[BTreeSet<Key>], open: &BTreeSet<Key>) -> usize {
    changes
        .iter()
        .take_while(|files| files.is_disjoint(open))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(source: &str, files: &[u64]) -> Plan {
        Plan {
            source: source.into(),
            files: files.iter().copied().map(Key::File).collect(),
        }
    }

    #[test]
    fn waiting_dependencies_are_transitive_but_unrelated_source_goes_ahead() {
        let plans = [
            plan("a", &[1]),
            plan("b", &[1, 2]),
            plan("c", &[2]),
            plan("d", &[3]),
        ];
        let open = [Key::File(1)].into_iter().collect();
        assert_eq!(eligible(&plans, &open), Some(3));
        assert_eq!(eligible(&plans, &BTreeSet::new()), Some(0));
    }

    #[test]
    fn same_source_cannot_overtake_even_for_another_file() {
        let plans = [plan("a", &[1]), plan("a", &[2]), plan("b", &[2])];
        let open = [Key::File(1)].into_iter().collect();
        assert_eq!(eligible(&plans, &open), None);
    }

    #[test]
    fn sync_stops_at_the_first_open_file_even_if_later_changes_are_free() {
        let changes = [
            [Key::File(1)].into_iter().collect(),
            [Key::File(2)].into_iter().collect(),
            [Key::File(3)].into_iter().collect(),
        ];
        assert_eq!(prefix(&changes, &[Key::File(2)].into_iter().collect()), 1);
        assert_eq!(prefix(&changes, &BTreeSet::new()), 3);
    }
}
