//! File content as numbered slots, and the lineage of the tokens written
//! into them.
//!
//! Every write puts one fresh token into one slot of one file. A token's
//! name carries its writer (`<writer>.<n>`, plus `~t<T>` when staged in
//! transaction T), so a commit's diff says whose changes it carries.
//! Each slot is one line. [`Layout::Separated`] puts separator lines
//! between slots, so git's line merge sees two slots as two hunks;
//! [`Layout::Adjacent`] puts them on consecutive lines, so changes to
//! neighbouring slots form one git hunk.

use std::collections::{BTreeMap, BTreeSet};

/// Slots per file.
pub const SLOTS: usize = 6;
/// The files of the modelled tree. All exist from main's first commit.
pub const PATHS: [&str; 4] = ["a", "b", "c", "d"];
/// An unwritten slot.
pub const EMPTY: &str = "-";
const SEPARATOR: &str = "--";

/// How a file's slots sit on its lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Two separator lines after each slot: every slot is its own hunk.
    Separated,
    /// Slots on consecutive lines: neighbouring slots share a hunk.
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

/// One file's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content(Vec<String>);

impl Content {
    pub fn empty() -> Self {
        Self(vec![EMPTY.to_string(); SLOTS])
    }

    pub fn slot(&self, i: usize) -> &str {
        &self.0[i]
    }

    /// This content with slot `i` set to `token`.
    pub fn with(&self, i: usize, token: &str) -> Self {
        let mut v = self.0.clone();
        v[i] = token.to_string();
        Self(v)
    }

    pub fn tokens(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str).filter(|t| *t != EMPTY)
    }

    pub fn contains(&self, token: &str) -> bool {
        self.0.iter().any(|t| t == token)
    }

    /// The slot after the last written one: where an append goes (rule 8).
    /// A model file has a fixed number of lines, its unwritten trailing
    /// slots the room an append fills.
    pub fn end(&self) -> usize {
        self.0.iter().rposition(|t| t != EMPTY).map_or(0, |i| i + 1)
    }

    /// The 1-based line slot `i` renders on.
    pub fn line_of(i: usize, layout: Layout) -> usize {
        (1 + layout.gap()) * i + 1
    }

    pub fn render(&self, layout: Layout) -> String {
        let mut s = String::new();
        for (i, t) in self.0.iter().enumerate() {
            s.push_str(&format!("s{i}: {t}\n"));
            for _ in 0..layout.gap() {
                s.push_str(SEPARATOR);
                s.push('\n');
            }
        }
        s
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut v = Vec::new();
        for line in text.lines() {
            if line == SEPARATOR {
                continue;
            }
            let (k, t) = line.split_once(": ")?;
            let i: usize = k.strip_prefix('s')?.parse().ok()?;
            if i != v.len() {
                return None;
            }
            v.push(t.to_string());
        }
        (v.len() == SLOTS).then_some(Self(v))
    }
}

/// The token on a rendered slot line (`s2: m1.4`), if it is one.
pub fn line_token(line: &str) -> Option<&str> {
    let (k, t) = line.split_once(": ")?;
    k.strip_prefix('s')?.parse::<usize>().ok()?;
    (t != EMPTY).then_some(t)
}

/// The bytes a token's line holds: a line a whole-file rewrite put back
/// (`<writer>.<n>=<bytes>`) holds the bytes it copied; any other line its
/// own token.
pub fn bytes(token: &str) -> &str {
    token.split_once('=').map_or(token, |(_, b)| b)
}

/// The writer a token names.
pub fn writer_of(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
}

/// What each write had in view on its line: the oracle for "nothing is
/// silently lost" and "no stale line reverts a newer one", kept apart from
/// git. A token `b` descends from `a` when `b` was written over `a`: `a` is
/// the line `b`'s writer read on that slot, or one that line was written
/// over, and so on. Per line, from what the writer read, never from which
/// commits its version merged: a sync that kept the fork's own line over
/// the parent's did not show the parent's line to the fork's writer.
#[derive(Default, Debug)]
pub struct Lineage {
    saw: BTreeMap<String, BTreeSet<String>>,
}

impl Lineage {
    /// `token` was written over `base`, the line its writer read there.
    pub fn wrote(&mut self, token: &str, base: &str) {
        let mut over = BTreeSet::new();
        if base != EMPTY {
            over.insert(base.to_string());
            over.extend(self.saw.get(base).into_iter().flatten().cloned());
        }
        self.saw.insert(token.to_string(), over);
    }

    /// Whether `newer` descends from `older` (or is it). Every token
    /// descends from the unwritten slot.
    pub fn descends(&self, newer: &str, older: &str) -> bool {
        newer == older || older == EMPTY || self.saw.get(newer).is_some_and(|s| s.contains(older))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips() {
        let c = Content::empty().with(2, "m1.1");
        for layout in [Layout::Separated, Layout::Adjacent] {
            assert_eq!(Content::parse(&c.render(layout)), Some(c.clone()));
        }
    }

    #[test]
    fn lineage_follows_views() {
        let mut l = Lineage::default();
        l.wrote("m1.1", EMPTY);
        l.wrote("m1.2", "m1.1");
        l.wrote("m1.3", "m1.2");
        l.wrote("c1.1", "m1.1");
        assert!(l.descends("m1.3", EMPTY) && l.descends("m1.3", "m1.1"));
        assert!(!l.descends("m1.1", "m1.2"));
        assert!(!l.descends("c1.1", "m1.2") && !l.descends("m1.2", "c1.1"));
    }

    #[test]
    fn slots_render_on_their_lines() {
        let c = Content::empty().with(2, "m1.1");
        for layout in [Layout::Separated, Layout::Adjacent] {
            let line = c
                .render(layout)
                .lines()
                .nth(Content::line_of(2, layout) - 1)
                .map(str::to_string);
            assert_eq!(line.as_deref(), Some("s2: m1.1"), "{layout:?}");
        }
    }
}
