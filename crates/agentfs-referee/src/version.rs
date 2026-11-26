//! The event numbers a journal gives steps, and the line versions they name
//! (docs/target-architecture/git-model.md §5, §7, §12; docs/library/referee.md).

use alloc::borrow::ToOwned;
use alloc::string::{String, ToString};
use core::cmp::Ordering;
use core::fmt;
use core::str::FromStr;

use agentfs_lines::{Id, LineMap};
use serde::{Deserialize, Serialize};

/// A step's journal number. It numbers the step's name versions and line versions, and the object,
/// handle or fork a step creates. Volume-wide; never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventNumber(u64);

impl EventNumber {
    /// Once per step, in `run_step`.
    #[inline]
    pub const fn new(seq: u64) -> Self {
        Self(seq)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The one event-to-object step: a create's object (and so its first identity), and the new
    /// object a sync gives a file the fork ended (R11.3).
    #[inline]
    pub const fn object(self) -> u64 {
        self.0
    }
}

/// Decimal; honours width and fill, so `{:020}` is a queue entry's number.
impl fmt::Display for EventNumber {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// One line version as history stores it, `"<event>:<index>"` (a line map's
/// runs, `Agentfs-Collisions`): the event that wrote it and its place among that event's new versions,
/// from 1. A line's identity is its first version.
///
/// It **orders as its text** (`"10:1" < "9:1"`): the order a map of text keys wrote `ancestors`
/// in, so no stored blob depends on this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct LineVersion {
    event: EventNumber,
    index: u64,
}

/// Text that is not a line version: [`LineVersion`]'s `FromStr` and `TryFrom<String>` refuse it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{0:?} is not a line version: `<event>:<index>`, decimal, no sign or leading zero, index from 1"
)]
pub struct NotALineVersion(pub String);

/// The longest text: two `u64`s of 20 digits and the colon.
const TEXT: usize = 41;

/// Writes `n` in decimal at the end of `buf[..end]`, returning where it starts.
#[inline]
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
    #[inline]
    pub const fn event(self) -> EventNumber {
        self.event
    }

    /// The text, formatted on the stack.
    #[inline]
    fn text(self, buf: &mut [u8; TEXT]) -> &[u8] {
        let start = digits(buf, TEXT, self.index);
        buf[start - 1] = b':';
        let start = digits(buf, start - 1, self.event.0);
        &buf[start..]
    }
}

/// An event's new versions number on: `<event>:<index>`, `<event>:<index + 1>`, ...
impl Id for LineVersion {
    #[inline]
    fn at(writer: u64, index: u64) -> Self {
        Self {
            event: EventNumber(writer),
            index,
        }
    }

    #[inline]
    fn writer(&self) -> u64 {
        self.event.0
    }

    #[inline]
    fn index(&self) -> u64 {
        self.index
    }
}

/// The byte order of the two texts.
impl Ord for LineVersion {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        let (mut a, mut b) = ([0; TEXT], [0; TEXT]);
        self.text(&mut a).cmp(other.text(&mut b))
    }
}

impl PartialOrd for LineVersion {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for LineVersion {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.event, self.index)
    }
}

/// Decimal without sign or leading zero.
#[inline]
fn canonical(text: &str) -> Option<u64> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text != "0" && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// [`NotALineVersion`] unless canonical: `<u64>:<u64>`, decimal, no sign, no leading zero, index
/// at least 1. So a parsed version prints back to the bytes it came from.
impl FromStr for LineVersion {
    type Err = NotALineVersion;

    #[inline]
    fn from_str(text: &str) -> Result<Self, NotALineVersion> {
        let refused = || NotALineVersion(text.to_owned());
        let (event, index) = text.split_once(':').ok_or_else(refused)?;
        let event = canonical(event).ok_or_else(refused)?;
        let index = canonical(index)
            .filter(|index| *index >= 1)
            .ok_or_else(refused)?;
        Ok(Self {
            event: EventNumber(event),
            index,
        })
    }
}

impl From<LineVersion> for String {
    #[inline]
    fn from(version: LineVersion) -> Self {
        version.to_string()
    }
}

impl TryFrom<String> for LineVersion {
    type Error = NotALineVersion;

    #[inline]
    fn try_from(text: String) -> Result<Self, NotALineVersion> {
        text.parse()
    }
}

/// One step's new line versions in order: `<event>:1`, `<event>:2`, ...
pub struct Versions {
    event: EventNumber,
    last: u64,
}

impl Versions {
    #[inline]
    pub const fn of(event: EventNumber) -> Self {
        Self { event, last: 0 }
    }

    #[cfg(test)]
    pub(crate) fn next(&mut self) -> LineVersion {
        self.take(1)
    }

    /// `n` new versions in order; the first of them.
    #[inline]
    pub fn take(&mut self, n: u64) -> LineVersion {
        let first = LineVersion {
            event: self.event,
            index: self.last + 1,
        };
        self.last += n;
        first
    }
}

/// A file's line map as the engine stores it.
pub type Lines = LineMap<LineVersion>;

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> LineVersion {
        text.parse().unwrap()
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
            assert_eq!(
                text.parse::<LineVersion>(),
                Err(NotALineVersion(text.to_owned())),
                "{text}"
            );
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
