//! The tree actor's pointers: branches, queue entries, collision refs and
//! object tables, as the journal's records leave them (target git-model §3). The journal is
//! the commit point; Git's refs are a copy of the built map.
//!
//! Pure: no I/O, so recovery and a running actor share one definition.

use std::collections::BTreeMap;

use git2::Oid;

use super::journal::{PointerChange, Target};
use super::{Error, Result};
use crate::tree_git::RefUpdate;

/// Pointer names to values: oids as built (History, the builder), or a
/// record's [`Target`]s as the actor appended them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Pointers<V = Oid>(BTreeMap<String, V>);

impl<V> Default for Pointers<V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<V: Clone + PartialEq> Pointers<V> {
    pub fn new(pointers: BTreeMap<String, V>) -> Self {
        Self(pointers)
    }

    pub fn get(&self, name: &str) -> Option<V> {
        self.0.get(name).cloned()
    }

    /// Every pointer whose name starts with `prefix`, in name order. Queue
    /// entries sort by their zero-padded number, so this is queue order.
    pub fn under(&self, prefix: &str) -> Vec<(String, V)> {
        self.0
            .range(prefix.to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Sets or removes one pointer.
    pub fn set(&mut self, name: &str, value: Option<V>) {
        match value {
            Some(value) => self.0.insert(name.to_owned(), value),
            None => self.0.remove(name),
        };
    }

    /// Every pointer, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.0.iter()
    }
}

impl Pointers<Target> {
    /// # Errors
    ///
    /// [`Error::PointerMoved`] when a change's `old` differs from the pointer
    /// now: a bug in the step that made it, caught before its record. A step's
    /// value and an oid never match: a value read before a compaction is
    /// named as the oid it was built as before this check
    /// (`TreeActor::resolve_compacted`).
    pub fn check_targets(&self, changes: &[PointerChange]) -> Result<()> {
        for change in changes {
            if self.0.get(&change.name) != change.old.as_ref() {
                return Err(Error::PointerMoved {
                    name: change.name.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn apply_targets(&mut self, changes: &[PointerChange]) {
        for change in changes {
            self.set(&change.name, change.new.clone());
        }
    }
}

impl Pointers<Oid> {
    /// The changes that create every pointer in an empty ref store.
    pub fn creates(&self) -> Vec<RefUpdate> {
        self.0
            .iter()
            .map(|(name, oid)| RefUpdate {
                refname: name.clone(),
                new: Some(*oid),
                expected_old: None,
            })
            .collect()
    }

    /// The changes that take `self` to `after`, each naming its old value.
    pub fn changes_to(&self, after: &Self) -> Vec<RefUpdate> {
        let mut changes = Vec::new();
        for (name, oid) in &after.0 {
            if self.0.get(name) != Some(oid) {
                changes.push(RefUpdate {
                    refname: name.clone(),
                    new: Some(*oid),
                    expected_old: self.get(name),
                });
            }
        }
        for (name, oid) in &self.0 {
            if !after.0.contains_key(name) {
                changes.push(RefUpdate {
                    refname: name.clone(),
                    new: None,
                    expected_old: Some(*oid),
                });
            }
        }
        changes
    }

    /// Every pointer's object, for the check that recovery can serve them.
    pub fn tips(&self) -> Vec<Oid> {
        self.0.values().copied().collect()
    }

    /// The pointers as a checkpoint stores them: hex object ids.
    pub fn to_hex(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .map(|(name, oid)| (name.clone(), oid.to_string()))
            .collect()
    }

    /// The pointers as a record names them.
    pub fn to_targets(&self) -> Pointers<Target> {
        Pointers(
            self.0
                .iter()
                .map(|(name, oid)| (name.clone(), Target::Oid(oid.to_string())))
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).unwrap()
    }

    fn step(n: u64) -> Target {
        Target::Step(super::super::journal::durable::RecordNo::new(n))
    }

    #[test]
    fn a_change_applies_only_over_the_value_it_names() {
        let mut pointers = Pointers::<Target>::default();
        let create = [PointerChange {
            name: "refs/heads/root".into(),
            new: Some(step(1)),
            old: None,
        }];
        pointers.check_targets(&create).unwrap();
        pointers.apply_targets(&create);
        assert_eq!(pointers.get("refs/heads/root"), Some(step(1)));
        assert!(matches!(
            pointers.check_targets(&create),
            Err(Error::PointerMoved { name }) if name == "refs/heads/root"
        ));
        let delete = [PointerChange {
            name: "refs/heads/root".into(),
            new: None,
            old: Some(step(1)),
        }];
        // The oid step 1 was built as is not step 1 here: the step's
        // record must name what it read.
        let built = [PointerChange {
            name: "refs/heads/root".into(),
            new: None,
            old: Some(Target::Oid(oid(1).to_string())),
        }];
        assert!(pointers.check_targets(&built).is_err());
        pointers.check_targets(&delete).unwrap();
        pointers.apply_targets(&delete);
        assert_eq!(pointers.get("refs/heads/root"), None);
    }

    #[test]
    fn under_stops_at_the_prefix_and_keeps_queue_order() {
        let mut pointers = Pointers::default();
        pointers.set("refs/queue/root/00000000000000000012", Some(oid(2)));
        pointers.set("refs/queue/root/00000000000000000003", Some(oid(1)));
        pointers.set("refs/queue/rootline/1", Some(oid(3)));
        pointers.set("refs/heads/root", Some(oid(4)));
        let names: Vec<_> = pointers
            .under("refs/queue/root/")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            [
                "refs/queue/root/00000000000000000003",
                "refs/queue/root/00000000000000000012"
            ]
        );
        assert_eq!(pointers.creates().len(), 4);
        assert!(pointers.creates().iter().all(|c| c.expected_old.is_none()));
        let mut after = pointers.clone();
        after.set("refs/heads/root", Some(oid(5)));
        after.set("refs/queue/rootline/1", None);
        let changes = pointers.changes_to(&after);
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|c| c.expected_old.is_some()));
    }
}
