//! A tree's shared types: the event numbers a journal gives steps, and the line versions they name
//! (docs/target-architecture/git-model.md §5, §7, §12).

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::Error;
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

    fn from_str(text: &str) -> Result<Self, Error> {
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

    fn try_from(text: String) -> Result<Self, Error> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree_actor::referee::{Collision, CollisionKind, Version};
    use crate::tree_actor::{MergeFileIdentity, NameHistory};
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
