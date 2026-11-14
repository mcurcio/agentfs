//! File content as a sequence of lines, each line a version of a line
//! identity.
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
//! the root's first commit as unborn ([`UNBORN`]): its name line deleted, no
//! lines.
//!
//! The model keeps each line's record (version, identity, the version it
//! was written over, live or gone) in the file's blob beside its bytes,
//! one record per git line. A version's writing commit is found by walking
//! back through parents to the first commit holding it (`origin_in`), not
//! by `git blame`, which is not proven on these files. [`Layout::Separated`] puts
//! separator lines after each record, so git's line merge sees two records
//! as two hunks; [`Layout::Adjacent`] puts them on consecutive lines.

use std::collections::BTreeSet;

/// Lines each file starts with: six root lines, all holding [`BLANK`].
pub const SLOTS: usize = 6;
/// The files that exist from the root's first commit: their identities, which
/// are also their first paths.
pub const FILES: [&str; 4] = ["a", "b", "c", "d"];
/// The file identities a create can take, in this order, each once in the
/// volume (rule 11: a file's identity is fixed at create). The root's first
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

/// One line version a commit set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The file's identity.
    pub path: String,
    /// The line's identity.
    pub line: String,
    /// The version.
    pub token: String,
    /// The version is a delete (of a line, or an unlink).
    pub gone: bool,
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

/// A transaction's one commit, file by file (MODEL.md rule 5): the file
/// `index` holds after the transaction's staged saves, as one save over
/// the file `begin` holds, so it passes the check every save passes
/// (`check::save_aligned`) and holds the index's bytes. A line `begin`
/// holds keeps `begin`'s version if its last staged version holds the same
/// bytes and state (a line staged and written back), else that last
/// version written over `begin`'s: the versions staged between are in no
/// commit. A line `begin` lacks that is live at the end is a new line, its
/// identity its last version's id; one gone at the end is not in the file.
/// A file whose name line ends as `begin` has it, not live, is as `begin`
/// has it (a file the transaction created and ended changed nothing
/// anyone sees). A moved line is a delete and a new line in the index
/// already: each save aligns it so.
pub fn one_save(begin: &Content, index: &Content) -> Content {
    fn over(b: &Line, x: &Line) -> Line {
        if x.id == b.id || x.state == b.state {
            return b.clone();
        }
        Line {
            id: x.id.clone(),
            line: b.line.clone(),
            over: Some(b.id.clone()),
            state: x.state.clone(),
        }
    }
    let name = over(&begin.name, &index.name);
    if name == begin.name && !name.live() {
        return begin.clone();
    }
    let lines = index
        .lines
        .iter()
        .filter_map(|x| match begin.member(&x.line) {
            Some(b) => Some(over(b, x)),
            None => x.live().then(|| Line {
                id: x.id.clone(),
                line: x.id.clone(),
                over: None,
                state: x.state.clone(),
            }),
        })
        .collect();
    Content { name, lines }
}

/// The writer a version id names.
pub fn writer_of(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
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

    /// `one_save`'s cases, each from begin's root file `a` (or unborn `g`).
    #[test]
    fn one_save_is_one_save_over_begin() {
        let live = |id: &str, line: &str, over: Option<&str>, t: &str| Line {
            id: id.into(),
            line: line.into(),
            over: over.map(Into::into),
            state: State::Live(t.into()),
        };
        let gone = |id: &str, line: &str, over: Option<&str>| Line {
            id: id.into(),
            line: line.into(),
            over: over.map(Into::into),
            state: State::Gone,
        };
        let begin = Content::root("a");
        // Nothing staged: begin.
        assert_eq!(one_save(&begin, &begin), begin);
        let mut index = begin.clone();
        // a0 staged twice: its last version, written over begin's.
        index.lines[0] = live("w.2~t1", "a0", Some("w.1~t1"), "x\n");
        // a1 staged, then written back to begin's bytes: begin's version.
        index.lines[1] = live("w.4~t1", "a1", Some("w.3~t1"), "-\n");
        // a2 deleted: a delete written over begin's.
        index.lines[2] = gone("w.5~t1", "a2", Some("a2"));
        // A new line staged twice: one new line, its identity its last version.
        index
            .lines
            .insert(3, live("w.7~t1", "w.6~t1", Some("w.6~t1"), "n\n"));
        // A new line staged and deleted: not in the file.
        index
            .lines
            .insert(4, gone("w.9~t1", "w.8~t1", Some("w.8~t1")));
        let t = one_save(&begin, &index);
        assert_eq!(t.lines[0], live("w.2~t1", "a0", Some("a0"), "x\n"));
        assert_eq!(t.lines[1], begin.lines[1]);
        assert_eq!(t.lines[2], gone("w.5~t1", "a2", Some("a2")));
        assert_eq!(t.lines[3], live("w.7~t1", "w.7~t1", None, "n\n"));
        assert_eq!(t.lines.len(), begin.lines.len() + 1);
        assert_eq!(t.bytes(), index.bytes());
        assert_eq!(t.name, begin.name);
        // A file created and ended in the transaction: as begin has it.
        let unborn = Content::root("g");
        let mut g = unborn.clone();
        g.name = gone("w.11~t1", "g", Some("w.10~t1"));
        g.lines.push(live("w.12~t1", "w.12~t1", None, "z\n"));
        assert_eq!(one_save(&unborn, &g), unborn);
        // Created and renamed: one create at its last path.
        g.name = live("w.11~t1", "g", Some("w.10~t1"), "e");
        let t = one_save(&unborn, &g);
        assert_eq!(t.name, live("w.11~t1", "g", Some("g"), "e"));
        assert_eq!(t.lines, g.lines);
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
