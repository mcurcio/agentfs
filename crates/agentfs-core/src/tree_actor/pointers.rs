//! The tree actor's pointers: branches, queue entries, collision refs and
//! object tables, as the journal's records leave them (target git-model §3). The journal is the commit point; Git's refs are a copy of this map.
//!
//! Pure: no I/O, so recovery and a running actor share one definition.

use std::collections::BTreeMap;

use git2::Oid;

use super::{Error, Result};
use crate::tree_git::RefUpdate;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Pointers(BTreeMap<String, Oid>);

impl Pointers {
    pub fn new(pointers: BTreeMap<String, Oid>) -> Self {
        Self(pointers)
    }

    pub fn get(&self, name: &str) -> Option<Oid> {
        self.0.get(name).copied()
    }

    /// Every pointer whose name starts with `prefix`, in name order. Queue
    /// entries sort by their zero-padded number, so this is queue order.
    pub fn under(&self, prefix: &str) -> Vec<(String, Oid)> {
        self.0
            .range(prefix.to_owned()..)
            .take_while(|(name, _)| name.starts_with(prefix))
            .map(|(name, oid)| (name.clone(), *oid))
            .collect()
    }

    /// # Errors
    ///
    /// [`Error::PointerMoved`] when a change's `expected_old` differs from the
    /// pointer now: a bug in the step that built it, caught before its record.
    pub fn check(&self, changes: &[RefUpdate]) -> Result<()> {
        for change in changes {
            if self.get(&change.refname) != change.expected_old {
                return Err(Error::PointerMoved {
                    name: change.refname.clone(),
                });
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, changes: &[RefUpdate]) {
        for change in changes {
            match change.new {
                Some(oid) => self.0.insert(change.refname.clone(), oid),
                None => self.0.remove(&change.refname),
            };
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        Oid::from_bytes(&[byte; 20]).unwrap()
    }

    fn change(name: &str, new: Option<Oid>, old: Option<Oid>) -> RefUpdate {
        RefUpdate {
            refname: name.into(),
            new,
            expected_old: old,
        }
    }

    #[test]
    fn a_change_applies_only_over_the_value_it_names() {
        let mut pointers = Pointers::default();
        let create = [change("refs/heads/root", Some(oid(1)), None)];
        pointers.check(&create).unwrap();
        pointers.apply(&create);
        assert_eq!(pointers.get("refs/heads/root"), Some(oid(1)));
        assert!(matches!(
            pointers.check(&create),
            Err(Error::PointerMoved { name }) if name == "refs/heads/root"
        ));
        let delete = [change("refs/heads/root", None, Some(oid(1)))];
        pointers.check(&delete).unwrap();
        pointers.apply(&delete);
        assert_eq!(pointers.get("refs/heads/root"), None);
    }

    #[test]
    fn under_stops_at_the_prefix_and_keeps_queue_order() {
        let mut pointers = Pointers::default();
        pointers.apply(&[
            change("refs/queue/root/00000000000000000012", Some(oid(2)), None),
            change("refs/queue/root/00000000000000000003", Some(oid(1)), None),
            change("refs/queue/rootline/1", Some(oid(3)), None),
            change("refs/heads/root", Some(oid(4)), None),
        ]);
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
    }
}
