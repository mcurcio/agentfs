//! File content as numbered slots, and the lineage of the tokens written
//! into them.
//!
//! Every write puts one fresh token into one slot of one file. A token's
//! name carries its writer (`<writer>.<n>`, plus `~t<T>` when staged in
//! transaction fork T), so a commit's diff says whose changes it carries.
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

/// Which token each write had in view when it replaced another: the oracle
/// for "nothing is silently lost". A slot may move from token `a` to token
/// `b` only if `b` descends from `a`: its writer saw `a`, or saw something
/// that did.
#[derive(Default, Debug)]
pub struct Lineage {
    saw: BTreeMap<String, BTreeSet<String>>,
}

impl Lineage {
    /// `token` was written over `over`.
    pub fn wrote(&mut self, token: &str, over: &str) {
        if token != over {
            self.saw
                .entry(token.to_string())
                .or_default()
                .insert(over.to_string());
        }
    }

    /// Whether `newer` descends from `older` (or is it).
    pub fn descends(&self, newer: &str, older: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![newer.to_string()];
        while let Some(t) = stack.pop() {
            if t == older {
                return true;
            }
            if seen.insert(t.clone())
                && let Some(prev) = self.saw.get(&t)
            {
                stack.extend(prev.iter().cloned());
            }
        }
        false
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
    fn lineage_follows_chains() {
        let mut l = Lineage::default();
        l.wrote("m1.1", EMPTY);
        l.wrote("m1.2", "m1.1");
        assert!(l.descends("m1.2", EMPTY));
        assert!(!l.descends("m1.1", "m1.2"));
    }
}
