//! File content as numbered slots, and the lineage of the tokens written
//! into them.
//!
//! Every write puts one fresh token into one slot of one file. A token's
//! name carries its writer (`<writer>.<n>`, plus `~t<T>` when staged in
//! transaction T), so a commit's diff says whose changes it carries.
//! Slots are kept on separate lines with separator lines between them, so
//! git's line merge sees two slots as two hunks: a change to different
//! slots merges, a change to the same slot conflicts.

use std::collections::{BTreeMap, BTreeSet};

/// Slots per file.
pub const SLOTS: usize = 4;
/// The files of the modelled tree. All exist from main's first commit.
pub const PATHS: [&str; 4] = ["a", "b", "c", "d"];
/// An unwritten slot.
pub const EMPTY: &str = "-";
const SEPARATOR: &str = "--";

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

    /// The 1-based line slot `i` renders on.
    pub fn line_of(i: usize) -> usize {
        3 * i + 1
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for (i, t) in self.0.iter().enumerate() {
            s.push_str(&format!("s{i}: {t}\n{SEPARATOR}\n{SEPARATOR}\n"));
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

/// The writer a token names.
pub fn writer_of(token: &str) -> &str {
    token.split('.').next().unwrap_or(token)
}

/// What each write had in view: the oracle for "nothing is silently lost"
/// and "no stale line reverts a newer one", kept apart from git. A token
/// `b` descends from `a` when `b`'s writer could see `a`: `a` was in the
/// tree it wrote on, or in any commit that tree merged or carried. Views
/// only grow along history, so the relation is transitive as recorded.
#[derive(Default, Debug)]
pub struct Lineage {
    saw: BTreeMap<String, BTreeSet<String>>,
}

impl Lineage {
    /// `token` was written by a writer who could see `known`.
    pub fn wrote(&mut self, token: &str, known: BTreeSet<String>) {
        self.saw.insert(token.to_string(), known);
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
        assert_eq!(Content::parse(&c.render()), Some(c));
    }

    #[test]
    fn lineage_follows_views() {
        let mut l = Lineage::default();
        l.wrote("m1.1", BTreeSet::new());
        l.wrote("m1.2", BTreeSet::from(["m1.1".to_string()]));
        assert!(l.descends("m1.2", EMPTY) && l.descends("m1.2", "m1.1"));
        assert!(!l.descends("m1.1", "m1.2"));
    }

    #[test]
    fn slots_render_on_their_lines() {
        let c = Content::empty().with(2, "m1.1");
        let line = c
            .render()
            .lines()
            .nth(Content::line_of(2) - 1)
            .map(str::to_string);
        assert_eq!(line.as_deref(), Some("s2: m1.1"));
    }
}
