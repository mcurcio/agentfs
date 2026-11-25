//! The values the landing rules read beside line versions: a file's merge
//! identity and its name history (docs/target-architecture/git-model.md §7;
//! docs/library/referee.md).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::{Error, EventNumber, Result};

// Merge identity follows an atomic replacement; the object number follows an
// open handle. Keep the types distinct even though both are allocated from
// the same durable event sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MergeFileIdentity(pub u64);

/// A file's name versions, including deletion, travel in the same commit as
/// its content. Each history is keyed by event number, which is volume-wide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameHistory {
    pub head: EventNumber,
    pub versions: NameVersions,
}

/// A name history's versions by event number, in order: a vector, because
/// most files have one version and a map's node costs hundreds of bytes for
/// it. Stored as the map it stands for, so the commit's bytes are the same.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NameVersions(Vec<(EventNumber, NameVersion)>);

impl NameVersions {
    pub fn get(&self, event: &EventNumber) -> Option<&NameVersion> {
        let at = self.0.binary_search_by_key(event, |(e, _)| *e).ok()?;
        Some(&self.0[at].1)
    }

    /// Records `version` at `event`, replacing the one there.
    pub fn insert(&mut self, event: EventNumber, version: NameVersion) {
        match self.0.binary_search_by_key(&event, |(e, _)| *e) {
            Ok(at) => self.0[at].1 = version,
            Err(at) => self.0.insert(at, (event, version)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&EventNumber, &NameVersion)> {
        self.0.iter().map(|(e, v)| (e, v))
    }

    #[cfg(test)]
    pub(crate) fn keys(&self) -> impl Iterator<Item = &EventNumber> {
        self.0.iter().map(|(e, _)| e)
    }
}

impl Serialize for NameVersions {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for NameVersions {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        let map = BTreeMap::<EventNumber, NameVersion>::deserialize(deserializer)?;
        Ok(Self(map.into_iter().collect()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameVersion {
    pub path: Option<String>,
    pub over: Option<EventNumber>,
    pub replaced_by: Option<MergeFileIdentity>,
}

impl NameHistory {
    pub fn created(event: EventNumber, path: String) -> Self {
        Self {
            head: event,
            versions: NameVersions(vec![(
                event,
                NameVersion {
                    path: Some(path),
                    over: None,
                    replaced_by: None,
                },
            )]),
        }
    }

    pub fn advance(
        &mut self,
        event: EventNumber,
        path: Option<String>,
        replaced_by: Option<MergeFileIdentity>,
    ) {
        self.versions.insert(
            event,
            NameVersion {
                path,
                over: Some(self.head),
                replaced_by,
            },
        );
        self.head = event;
    }

    /// The head's version: [`Error::Format`] if it is not on record.
    pub fn head_version(&self) -> Result<&NameVersion> {
        self.versions.get(&self.head).ok_or(Error::Format)
    }

    /// The head's path: None once the file is ended.
    pub fn path(&self) -> Result<Option<&str>> {
        Ok(self.head_version()?.path.as_deref())
    }

    /// Whether the head ends the file: an unlink or a rename-over.
    pub fn is_ended(&self) -> Result<bool> {
        Ok(self.path()?.is_none())
    }

    /// Whether this history's head is not `base`'s: the change it belongs to moved the name.
    /// A file `base` has no history of moved.
    pub fn moved_since(&self, base: Option<&NameHistory>) -> bool {
        base.is_none_or(|old| old.head != self.head)
    }

    /// Both histories' versions, keeping `self.head`; [`Error::Format`] where they record one
    /// version differently.
    pub fn combine(&self, other: &NameHistory) -> Result<NameHistory> {
        let mut combined = self.clone();
        for (event, version) in other.versions.iter() {
            match combined.versions.get(event) {
                Some(existing) if existing != version => return Err(Error::Format),
                Some(_) => {}
                None => {
                    combined.versions.insert(*event, version.clone());
                }
            }
        }
        Ok(combined)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_versions_keep_event_order() {
        let mut name = NameHistory::created(EventNumber::new(5), "a".into());
        name.advance(EventNumber::new(9), Some("b".into()), None);
        let mut older = NameHistory::created(EventNumber::new(5), "a".into());
        older.advance(EventNumber::new(7), None, None);
        let both = name.combine(&older).unwrap();
        assert_eq!(
            both.versions.keys().map(|e| e.get()).collect::<Vec<_>>(),
            [5, 7, 9]
        );
        let stored = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<NameHistory>(&stored).unwrap(), both);
    }

    #[test]
    fn combine_keeps_its_head_and_refuses_a_version_recorded_twice_differently() {
        let mut ours = NameHistory::created(EventNumber::new(1), "a".into());
        let mut theirs = ours.clone();
        ours.advance(EventNumber::new(5), Some("b".into()), None);
        theirs.advance(EventNumber::new(6), Some("c".into()), None);
        let combined = ours.combine(&theirs).unwrap();
        assert_eq!(combined.head, EventNumber::new(5));
        assert_eq!(
            combined
                .versions
                .keys()
                .map(|e| e.get())
                .collect::<Vec<_>>(),
            [1, 5, 6]
        );

        let mut other = NameHistory::created(EventNumber::new(1), "a".into());
        other.advance(EventNumber::new(5), Some("elsewhere".into()), None);
        assert!(matches!(ours.combine(&other), Err(Error::Format)));
    }

    #[test]
    fn a_name_is_ended_by_a_head_with_no_path_and_moved_by_a_new_head() {
        let created = NameHistory::created(EventNumber::new(1), "a".into());
        assert!(!created.is_ended().unwrap());
        assert!(created.moved_since(None));
        assert!(!created.moved_since(Some(&created)));
        let mut unlinked = created.clone();
        unlinked.advance(EventNumber::new(2), None, None);
        assert!(unlinked.is_ended().unwrap());
        assert!(unlinked.moved_since(Some(&created)));
    }
}
