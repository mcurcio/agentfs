//! A tree's shared types: the event numbers a journal gives steps, and the line versions they name
//! (docs/target-architecture/git-model.md §5, §7, §12).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Error, Result, Volume, referee};
use crate::tree_reconcile::LineMap;

/// A step's journal number. It numbers the step's name versions and line versions, and the object,
/// handle or fork a step creates. Volume-wide; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct EventNumber(u64);

impl EventNumber {
    /// Once per step, in `run_step`.
    pub(super) const fn new(seq: u64) -> Self {
        Self(seq)
    }

    pub(super) const fn get(self) -> u64 {
        self.0
    }

    /// The one event-to-object step: a create's object (and so its first identity), and the new
    /// object a sync gives a file the fork ended (R11.3).
    pub(super) const fn object(self) -> u64 {
        self.0
    }
}

/// Decimal; honours width and fill, so `{:020}` is a queue entry's number.
impl fmt::Display for EventNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// One line version as history stores it, `"<event>:<index>"` (metadata.json,
/// `Agentfs-Collisions`): the event that wrote it and its place among that event's new versions,
/// from 1. A line's identity is its first version.
///
/// It **orders as its text** (`"10:1" < "9:1"`): the order a map of text keys wrote `ancestors`
/// in, so no stored blob depends on this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub(super) struct LineVersion {
    event: EventNumber,
    index: u64,
}

/// The longest text: two `u64`s of 20 digits and the colon.
const TEXT: usize = 41;

/// Writes `n` in decimal at the end of `buf[..end]`, returning where it starts.
fn digits(buf: &mut [u8; TEXT], mut end: usize, mut n: u64) -> usize {
    loop {
        end -= 1;
        // `n % 10` is below 10: the cast keeps every bit.
        buf[end] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return end;
        }
    }
}

impl LineVersion {
    pub(super) const fn event(self) -> EventNumber {
        self.event
    }

    /// The text, formatted on the stack.
    fn text(self, buf: &mut [u8; TEXT]) -> &[u8] {
        let start = digits(buf, TEXT, self.index);
        buf[start - 1] = b':';
        let start = digits(buf, start - 1, self.event.0);
        &buf[start..]
    }
}

/// The byte order of the two texts.
impl Ord for LineVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let (mut a, mut b) = ([0; TEXT], [0; TEXT]);
        self.text(&mut a).cmp(other.text(&mut b))
    }
}

impl PartialOrd for LineVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for LineVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.event, self.index)
    }
}

/// Decimal without sign or leading zero.
fn canonical(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text != "0" && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// [`Error::Format`] unless canonical: `<u64>:<u64>`, decimal, no sign, no leading zero, index
/// at least 1. So a parsed version prints back to the bytes it came from.
impl FromStr for LineVersion {
    type Err = Error;

    fn from_str(text: &str) -> std::result::Result<Self, Error> {
        let (event, index) = text.split_once(':').ok_or(Error::Format)?;
        let event = canonical(event).ok_or(Error::Format)?;
        let index = canonical(index)
            .filter(|index| *index >= 1)
            .ok_or(Error::Format)?;
        Ok(Self {
            event: EventNumber(event),
            index,
        })
    }
}

impl From<LineVersion> for String {
    fn from(version: LineVersion) -> Self {
        version.to_string()
    }
}

impl TryFrom<String> for LineVersion {
    type Error = Error;

    fn try_from(text: String) -> std::result::Result<Self, Error> {
        text.parse()
    }
}

/// One step's new line versions in order: `<event>:1`, `<event>:2`, ...
pub(super) struct Versions {
    event: EventNumber,
    last: u64,
}

impl Versions {
    pub(super) const fn of(event: EventNumber) -> Self {
        Self { event, last: 0 }
    }

    pub(super) fn next(&mut self) -> LineVersion {
        self.last += 1;
        LineVersion {
            event: self.event,
            index: self.last,
        }
    }
}

/// A file's line map as the engine stores it.
pub(super) type Lines = LineMap<LineVersion>;

/// The saved state of one tree. A step builds on a copy and installs it after
/// its commit point, so file bytes and line maps sit behind `Arc`.
#[derive(Clone, Default)]
pub(super) struct Tree {
    pub(super) names: BTreeMap<String, u64>,
    pub(super) anonymous: BTreeSet<u64>,
    pub(super) bytes: BTreeMap<u64, Arc<Vec<u8>>>,
    pub(super) lines: BTreeMap<u64, Arc<Lines>>,
    pub(super) identities: BTreeMap<u64, MergeFileIdentity>,
    pub(super) namespace: BTreeMap<MergeFileIdentity, NameHistory>,
    pub(super) fresh_names: BTreeSet<u64>,
}

impl Tree {
    /// Adds an object that has no name and no saved state.
    pub(super) fn add_anonymous(&mut self, object: u64) {
        self.anonymous.insert(object);
        self.bytes.insert(object, Arc::default());
        self.lines.insert(object, Arc::default());
        self.identities.insert(object, MergeFileIdentity(object));
        self.fresh_names.insert(object);
    }

    pub(super) fn drop_object(&mut self, object: u64) {
        self.bytes.remove(&object);
        self.lines.remove(&object);
        self.identities.remove(&object);
        self.fresh_names.remove(&object);
    }

    /// Replaces the names and files with `snapshot`'s. Two files on one object
    /// would read each other's bytes, so a view could differ from its commit: a
    /// step that would do it fails before its record (finding 39).
    pub(super) fn install(&mut self, snapshot: referee::Snapshot) -> Result<()> {
        self.names.clear();
        self.fresh_names.clear();
        let mut objects = BTreeSet::new();
        for (id, file) in snapshot.files {
            if !objects.insert(file.object) {
                return Err(Error::Format);
            }
            let history = snapshot.names.get(&id).ok_or(Error::Format)?;
            let path = history.path()?.ok_or(Error::Format)?;
            if history.head_version()?.over.is_none() {
                self.fresh_names.insert(file.object);
            }
            self.names.insert(path.to_owned(), file.object);
            self.bytes.insert(file.object, Arc::new(file.lines.bytes()));
            self.lines.insert(file.object, Arc::new(file.lines));
            self.identities.insert(file.object, id);
        }
        self.namespace = snapshot.names;
        Ok(())
    }
}

// Merge identity follows an atomic replacement; the object number follows an
// open handle. Keep the types distinct even though both are allocated from
// the same durable event sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct MergeFileIdentity(pub(super) u64);

/// A file's name versions, including deletion, travel in the same commit as
/// its content. Each history is keyed by event number, which is volume-wide.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct NameHistory {
    pub(super) head: EventNumber,
    pub(super) versions: BTreeMap<EventNumber, NameVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NameVersion {
    pub(super) path: Option<String>,
    pub(super) over: Option<EventNumber>,
    pub(super) replaced_by: Option<MergeFileIdentity>,
}

impl NameHistory {
    pub(super) fn created(event: EventNumber, path: String) -> Self {
        Self {
            head: event,
            versions: [(
                event,
                NameVersion {
                    path: Some(path),
                    over: None,
                    replaced_by: None,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    pub(super) fn advance(
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
    pub(super) fn head_version(&self) -> Result<&NameVersion> {
        self.versions.get(&self.head).ok_or(Error::Format)
    }

    /// The head's path: None once the file is ended.
    pub(super) fn path(&self) -> Result<Option<&str>> {
        Ok(self.head_version()?.path.as_deref())
    }

    /// Both histories' versions, keeping `self.head`; [`Error::Format`] where they record one
    /// version differently.
    pub(super) fn combine(&self, other: &NameHistory) -> Result<NameHistory> {
        let mut combined = self.clone();
        for (event, version) in &other.versions {
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

impl Volume {
    pub(super) fn snapshot_of(tree: &Tree) -> referee::Snapshot {
        let files = tree
            .names
            .values()
            .map(|object| {
                (
                    tree.identities[object],
                    referee::File {
                        object: *object,
                        lines: (*tree.lines[object]).clone(),
                    },
                )
            })
            .collect();
        referee::Snapshot {
            files,
            names: tree.namespace.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::referee::{Collision, CollisionKind, Version};
    use crate::tree_reconcile::Line;

    fn v(text: &str) -> LineVersion {
        text.parse().unwrap()
    }

    #[test]
    fn the_stored_text_is_unchanged() {
        let lines = Lines {
            lines: vec![
                Line {
                    identity: v("9:1"),
                    version: v("9:1"),
                    bytes: Some(b"a\n".to_vec()),
                },
                Line {
                    identity: v("9:2"),
                    version: v("10:1"),
                    bytes: None,
                },
            ],
            ancestors: [
                (v("9:1"), None),
                (v("9:2"), None),
                (v("10:1"), Some(v("9:2"))),
            ]
            .into_iter()
            .collect(),
        };
        let stored = r#"{"lines":[{"identity":"9:1","version":"9:1","bytes":[97,10]},{"identity":"9:2","version":"10:1","bytes":null}],"ancestors":{"10:1":"9:2","9:1":null,"9:2":null}}"#;
        assert_eq!(String::from_utf8(lines.encode().unwrap()).unwrap(), stored);
        assert_eq!(Lines::decode(stored.as_bytes()).unwrap(), lines);

        let mut name = NameHistory::created(EventNumber::new(9), "a".into());
        name.advance(EventNumber::new(10), Some("b".into()), None);
        assert_eq!(
            serde_json::to_string(&name).unwrap(),
            r#"{"head":10,"versions":{"9":{"path":"a","over":null,"replaced_by":null},"10":{"path":"b","over":9,"replaced_by":null}}}"#
        );

        let collision = Collision {
            lost: Version::Line {
                file: MergeFileIdentity(3),
                version: v("10:1"),
            },
            winner: Version::Name {
                file: MergeFileIdentity(2),
                version: EventNumber::new(9),
            },
            kind: CollisionKind::Line,
        };
        let text = r#"{"lost":{"Line":{"file":3,"version":"10:1"}},"winner":{"Name":{"file":2,"version":9}},"kind":"Line"}"#;
        assert_eq!(serde_json::to_string(&collision).unwrap(), text);
        assert_eq!(serde_json::from_str::<Collision>(text).unwrap(), collision);
    }

    #[test]
    fn line_versions_order_as_their_text() {
        let all: Vec<LineVersion> = (1..=120)
            .flat_map(|event| {
                let mut versions = Versions::of(EventNumber::new(event));
                (1..=12).map(move |_| versions.next())
            })
            .collect();
        for a in &all {
            for b in &all {
                assert_eq!(a.cmp(b), a.to_string().cmp(&b.to_string()), "{a} {b}");
            }
        }
        let max = LineVersion {
            event: EventNumber::new(u64::MAX),
            index: u64::MAX,
        };
        assert_eq!(v(&max.to_string()), max);
        assert_eq!(max.cmp(&v("9:1")), max.to_string().cmp(&"9:1".to_owned()));
    }

    #[test]
    fn only_canonical_text_parses() {
        for text in [
            "r1", "05:1", "+5:1", "5:0", "5", "5:1:2", ":1", "5:", "5:01", "-5:1",
        ] {
            assert!(text.parse::<LineVersion>().is_err(), "{text}");
        }
        assert_eq!(v("5:1").to_string(), "5:1");
        assert_eq!(v("0:1").to_string(), "0:1");
        assert!(serde_json::from_str::<LineVersion>(r#""05:1""#).is_err());
    }

    #[test]
    fn versions_count_from_one() {
        let mut versions = Versions::of(EventNumber::new(7));
        assert_eq!(versions.next(), v("7:1"));
        assert_eq!(versions.next(), v("7:2"));
        assert_eq!(versions.next().event(), EventNumber::new(7));
        assert_eq!(
            format!("{:020}", EventNumber::new(7)),
            "00000000000000000007"
        );
    }
}
