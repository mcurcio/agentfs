//! File content as a sequence of lines, each line a version of a line
//! identity, and the lineage of those versions.
//!
//! A file is a byte buffer. A line is its bytes up to and including a
//! newline, or the bytes after the last newline (a partial last line: the
//! file has no final newline). A live version holds its line's bytes,
//! terminator included, so the file's bytes are its live lines' bytes in
//! order ([`Content::bytes`]); a record escapes them to stay on one git line.
//!
//! A line's identity is fixed when a save first records it (a new line
//! the save's alignment found, an append, or the root commit). Every later
//! change to that line is a new version of the same identity, written over
//! the version its writer saw: new bytes in place, or a delete (a hidden
//! version marking it gone). The alignment reads a moved line as a delete
//! plus a new line. Each version has its own id, unique
//! in the volume (`<writer>.<n>`, plus `~t<T>` when staged in transaction
//! T); an identity is the id of its first version. A file has one more
//! line, its name line: its identity is the file's identity (fixed at
//! create, kept by a rename), its live versions hold the file's path, a
//! deleted version marks the file unlinked. A file a create can make is in
//! main's first commit as unborn ([`UNBORN`]): its name line deleted, no
//! lines.
//!
//! The model keeps each line's record (version, identity, the version it
//! was written over, live or gone) in the file's blob beside its bytes,
//! one record per git line. A version's writing commit is found by walking
//! back through parents to the first commit holding it (`origin_in`), not
//! by `git blame`, which is not proven on these files. [`Layout::Separated`] puts
//! separator lines after each record, so git's line merge sees two records
//! as two hunks; [`Layout::Adjacent`] puts them on consecutive lines.

use std::collections::{BTreeMap, BTreeSet};

/// Lines each file starts with: six root lines, all holding [`BLANK`].
pub const SLOTS: usize = 6;
/// The files that exist from main's first commit: their identities, which
/// are also their first paths.
pub const FILES: [&str; 4] = ["a", "b", "c", "d"];
/// The file identities a create can take, in this order, each once in the
/// volume (rule 11: a file's identity is fixed at create). Main's first
/// commit holds each as unborn: a deleted name version and no lines, so a
/// create is a new version of the name line written over it, and every
/// reader that walks the files sees an unborn file as an unlinked one.
pub const UNBORN: [&str; 3] = ["g", "h", "i"];
/// Every file identity: [`FILES`], then [`UNBORN`].
pub const PATHS: [&str; 7] = ["a", "b", "c", "d", "g", "h", "i"];
/// The text of every root line (its bytes are this and a newline): six
/// identical lines in every file.
pub const BLANK: &str = "-";
const SEPARATOR: &str = "--";
const NONE: &str = "-";

/// How a file's records sit on its git lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Two separator lines after each record: every record is its own hunk.
    Separated,
    /// Records on consecutive lines: neighbouring records share a hunk.
    Adjacent,
}

impl Layout {
    fn gap(self) -> usize {
        match self {
            Layout::Separated => 2,
            Layout::Adjacent => 0,
        }
    }
}

/// What a version holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// A live line with these bytes, its newline included unless it is a
    /// partial last line (for a name line: the file's path).
    Live(String),
    /// Deleted: a content line removed, or a file unlinked.
    Gone,
    /// A name line only: the file was unlinked by a rename of this file
    /// over it (rename-over), one call with that rename.
    Replaced(String),
}

/// One version of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// This version's id, unique in the volume.
    pub id: String,
    /// The line's identity: the id of its first version.
    pub line: String,
    /// The version it was written over: the one its writer saw there.
    pub over: Option<String>,
    pub state: State,
}

impl Line {
    pub fn live(&self) -> bool {
        matches!(self.state, State::Live(_))
    }

    /// Its bytes, terminator included, if live.
    pub fn text(&self) -> Option<&str> {
        match &self.state {
            State::Live(t) => Some(t),
            _ => None,
        }
    }

    fn render(&self) -> String {
        let state = match &self.state {
            State::Live(t) => format!("+{}", escape(t)),
            State::Gone => "-".to_string(),
            State::Replaced(f) => format!(">{}", escape(f)),
        };
        format!(
            "{}\t{}\t{}\t{state}",
            self.id,
            self.line,
            self.over.as_deref().unwrap_or(NONE)
        )
    }

    /// A record as [`Line::render`] writes it.
    pub fn parse(s: &str) -> Option<Self> {
        let mut f = s.splitn(4, '\t');
        let (id, line, over, state) = (f.next()?, f.next()?, f.next()?, f.next()?);
        let state = match state.split_at_checked(1)? {
            ("+", t) => State::Live(unescape(t)?),
            ("-", "") => State::Gone,
            (">", f) => State::Replaced(unescape(f)?),
            _ => return None,
        };
        Some(Self {
            id: id.to_string(),
            line: line.to_string(),
            over: (over != NONE).then(|| over.to_string()),
            state,
        })
    }
}

/// A line's bytes on one git line: a backslash, a newline and a NUL byte
/// (a hole a `pwrite` past the end leaves) escaped.
fn escape(t: &str) -> String {
    let mut s = String::with_capacity(t.len());
    for c in t.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\0' => s.push_str("\\0"),
            c => s.push(c),
        }
    }
    s
}

fn unescape(t: &str) -> Option<String> {
    let mut s = String::with_capacity(t.len());
    let mut it = t.chars();
    while let Some(c) = it.next() {
        s.push(match c {
            '\\' => match it.next()? {
                '\\' => '\\',
                'n' => '\n',
                '0' => '\0',
                _ => return None,
            },
            c => c,
        });
    }
    Some(s)
}

/// A byte buffer's lines: each up to and including a newline, then the
/// bytes after the last newline, if any (a partial last line).
pub fn split_lines(bytes: &str) -> Vec<String> {
    bytes.split_inclusive('\n').map(str::to_string).collect()
}

/// Whether `id` is a root version (written by the root commit).
pub fn is_root(id: &str) -> bool {
    PATHS.iter().any(|p| {
        id == *p
            || id
                .strip_prefix(p)
                .and_then(|n| n.parse::<usize>().ok())
                .is_some_and(|n| n < SLOTS)
    })
}

/// One file's content: its name line and its lines in order, deleted
/// versions included (hidden from what a program reads).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content {
    pub name: Line,
    pub lines: Vec<Line>,
}

impl Content {
    /// File `file`'s root content: its name line holding its path, and
    /// [`SLOTS`] root lines `<file>0`.. all holding [`BLANK`] and a newline;
    /// for an [`UNBORN`] identity, a deleted name line and no lines.
    pub fn root(file: &str) -> Self {
        if UNBORN.contains(&file) {
            return Self {
                name: Line {
                    id: file.into(),
                    line: file.into(),
                    over: None,
                    state: State::Gone,
                },
                lines: Vec::new(),
            };
        }
        let line = |id: String| Line {
            id: id.clone(),
            line: id,
            over: None,
            state: State::Live(format!("{BLANK}\n")),
        };
        Self {
            name: Line {
                state: State::Live(file.into()),
                ..line(file.into())
            },
            lines: (0..SLOTS).map(|i| line(format!("{file}{i}"))).collect(),
        }
    }

    /// The file's path, unless unlinked.
    pub fn path(&self) -> Option<&str> {
        self.name.text()
    }

    /// The live lines, as a program reads them.
    pub fn live(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter().filter(|l| l.live())
    }

    /// The live lines' texts, each without its newline: a partial last
    /// line and a full one read alike here; [`Content::bytes`] tells them
    /// apart.
    pub fn texts(&self) -> Vec<String> {
        self.live()
            .filter_map(|l| l.text())
            .map(|t| t.strip_suffix('\n').unwrap_or(t).to_string())
            .collect()
    }

    /// The file's bytes, as a program reads them: its live lines' bytes in
    /// order. agentfs adds no newline a program did not write (R10.4).
    pub fn bytes(&self) -> String {
        self.live().filter_map(|l| l.text()).collect()
    }

    /// The `i`th live line, as a program reads it.
    pub fn get(&self, i: usize) -> Option<&Line> {
        self.live().nth(i)
    }

    /// The version id of the `i`th live line.
    pub fn at(&self, i: usize) -> &str {
        &self.get(i).unwrap_or_else(|| panic!("no line {i}")).id
    }

    /// The version id of the last live line.
    pub fn last(&self) -> &str {
        &self.live().last().expect("a live line").id
    }

    /// Whether a live line is this version.
    pub fn contains(&self, id: &str) -> bool {
        self.live().any(|l| l.id == id)
    }

    /// The name line, then every line, deleted versions included.
    pub fn records(&self) -> impl Iterator<Item = &Line> {
        std::iter::once(&self.name).chain(&self.lines)
    }

    /// Whether any record (name line, deleted versions included) is this
    /// version.
    pub fn holds(&self, id: &str) -> bool {
        self.records().any(|l| l.id == id)
    }

    /// The version this file holds of line identity `line`.
    pub fn member(&self, line: &str) -> Option<&Line> {
        self.records().find(|l| l.line == line)
    }

    /// The index among [`Content::records`] of version `id`.
    pub fn index(&self, id: &str) -> Option<usize> {
        self.records().position(|l| l.id == id)
    }

    /// The 1-based git line record `i` (0: the name line) renders on.
    pub fn line_of(i: usize, layout: Layout) -> usize {
        (1 + layout.gap()) * i + 1
    }

    pub fn render(&self, layout: Layout) -> String {
        let mut s = String::new();
        for l in self.records() {
            s.push_str(&l.render());
            s.push('\n');
            for _ in 0..layout.gap() {
                s.push_str(SEPARATOR);
                s.push('\n');
            }
        }
        s
    }

    /// A rendered file; none unless it holds one version per identity.
    pub fn parse(text: &str) -> Option<Self> {
        let mut records = text
            .lines()
            .filter(|l| *l != SEPARATOR)
            .map(Line::parse)
            .collect::<Option<Vec<_>>>()?
            .into_iter();
        let name = records.next()?;
        let lines: Vec<Line> = records.collect();
        let mut seen = BTreeSet::from([name.line.clone()]);
        lines
            .iter()
            .all(|l| seen.insert(l.line.clone()))
            .then_some(Self { name, lines })
    }
}

/// The writer a version id names.
pub fn writer_of(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
}

/// What each version was written over: the oracle for "nothing is silently
/// lost" and "no stale line reverts a newer one", kept apart from git. A
/// version `b` descends from `a` when `b` was written over `a`: `a` is the
/// version `b`'s writer saw of that line, or one that version was written
/// over, and so on. From what the writer could read (R3c.2), never from which
/// commits its version merged: a sync that kept the fork's own line over
/// the parent's did not show the parent's version to the fork's writer.
#[derive(Default, Debug)]
pub struct Lineage {
    saw: BTreeMap<String, BTreeSet<String>>,
}

impl Lineage {
    /// `token` was written over `base`, the version its writer saw (none:
    /// a new line).
    pub fn wrote(&mut self, token: &str, base: Option<&str>) {
        let mut over = BTreeSet::new();
        if let Some(base) = base {
            over.insert(base.to_string());
            over.extend(self.saw.get(base).into_iter().flatten().cloned());
        }
        self.saw.insert(token.to_string(), over);
    }

    /// Whether `newer` descends from `older` (or is it).
    pub fn descends(&self, newer: &str, older: &str) -> bool {
        newer == older || self.saw.get(newer).is_some_and(|s| s.contains(older))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips() {
        let mut c = Content::root("a");
        c.lines[2] = Line {
            id: "m1.1".into(),
            line: "a2".into(),
            over: Some("a2".into()),
            state: State::Live("x\ty\\n\0".into()),
        };
        c.lines[3].state = State::Live("partial".into());
        c.lines.push(Line {
            id: "m1.2".into(),
            line: "m1.2".into(),
            over: None,
            state: State::Gone,
        });
        c.name = Line {
            id: "m1.3".into(),
            line: "a".into(),
            over: Some("a".into()),
            state: State::Replaced("b".into()),
        };
        for layout in [Layout::Separated, Layout::Adjacent] {
            assert_eq!(Content::parse(&c.render(layout)), Some(c.clone()));
        }
    }

    #[test]
    fn root_lines_repeat_their_bytes_but_not_their_identity() {
        let c = Content::root("b");
        assert_eq!(c.texts(), vec![BLANK.to_string(); SLOTS]);
        assert_eq!(c.bytes(), format!("{BLANK}\n").repeat(SLOTS));
        assert_eq!(split_lines("a\nb"), ["a\n", "b"]);
        assert_eq!(split_lines("a\n\n"), ["a\n", "\n"]);
        assert!(split_lines("").is_empty());
        let mut d = Content::root("d");
        d.lines[5].state = State::Live("y".into());
        assert_eq!(d.bytes(), "-\n-\n-\n-\n-\ny");
        assert_eq!(c.at(3), "b3");
        assert!(is_root("b3") && is_root("b") && !is_root("b6") && !is_root("c1.1"));
        assert_eq!(c.member("b").and_then(Line::text), Some("b"));
    }

    #[test]
    fn lineage_follows_views() {
        let mut l = Lineage::default();
        l.wrote("m1.2", Some("a0"));
        l.wrote("m1.3", Some("m1.2"));
        l.wrote("c1.1", Some("a0"));
        l.wrote("c1.2", None);
        assert!(l.descends("m1.3", "a0") && l.descends("m1.3", "m1.2"));
        assert!(!l.descends("m1.2", "m1.3"));
        assert!(!l.descends("c1.1", "m1.2") && !l.descends("m1.2", "c1.1"));
        assert!(!l.descends("c1.2", "a0"));
    }

    #[test]
    fn records_render_on_their_lines() {
        let c = Content::root("a");
        for layout in [Layout::Separated, Layout::Adjacent] {
            let line = c
                .render(layout)
                .lines()
                .nth(Content::line_of(3, layout) - 1)
                .map(|l| Line::parse(l).expect("a record").id);
            assert_eq!(line.as_deref(), Some("a2"), "{layout:?}");
        }
    }
}
