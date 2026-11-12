//! The referee's placing: where applied and kept versions go ([`splice`], R10.1,
//! R10.7), and the joins no tree may show (an unfinished line with a live line
//! after it): at a take (a landing, or a transaction's commit) they lose (R10.4,
//! R10.5, R10.8, R10.11), at a sync they are hidden until the upstream decides
//! (R10.6, R10.9).

use std::collections::{BTreeMap, BTreeSet};

use super::{Lineage, Lines, Own, Referee, Verdict, key};
use crate::content::{Content, Line};

impl<L: Lineage + ?Sized> Referee<'_, L> {
    /// At every take (a landing, or a transaction's commit into its fork), a
    /// line placed after the upstream's unfinished last line collides with it
    /// (R10.4, R10.5), and so does an applied unfinished line the upstream would
    /// keep a line after (R10.8, R10.11): the next line wins.
    pub(super) fn joins(&self, ours: &str, tip: &str, lines: &mut Lines<'_>) {
        let by_path = &lines.by_path;
        let verdicts = &mut lines.verdicts;
        for (p, ins) in by_path {
            let cur = self.file(ours, p);
            let Some(q) = cur
                .live()
                .last()
                .filter(|l| l.text().is_some_and(|t| !t.ends_with('\n')))
            else {
                continue;
            };
            let applied: Vec<Line> = ins
                .iter()
                .filter(|i| verdicts[&key(i)] == Verdict::Apply)
                .map(|i| i.version.clone())
                .collect();
            let placed = splice(&cur, &applied, &self.file(tip, p));
            let after = placed.live().skip_while(|l| l.id != q.id).skip(1);
            for l in after {
                verdicts.insert((p.to_string(), l.line.clone()), Verdict::Lose(q.id.clone()));
            }
        }
        for (p, ins) in by_path {
            let applied: Vec<Line> = ins
                .iter()
                .filter(|i| verdicts[&key(i)] == Verdict::Apply)
                .map(|i| i.version.clone())
                .collect();
            let placed = splice(&self.file(ours, p), &applied, &self.file(tip, p));
            if let Some((line, next)) = unfinished_before_kept(&placed, &applied) {
                verdicts.insert((p.to_string(), line), Verdict::Lose(next));
            }
        }
    }
}

/// A sync's files: each changed file's kept versions placed over the upstream's
/// file in the fork's order, its joins hidden; only the files that differ from
/// the upstream's. Sync's names ([`Referee::keep_own_names`]) read these
/// placed files, so they run after it.
pub(super) fn kept(own: Vec<Own>) -> BTreeMap<String, Content> {
    let mut files = BTreeMap::new();
    for Own {
        path: p,
        mine,
        theirs,
        keep,
    } in own
    {
        let own: BTreeSet<String> = keep.iter().map(|l| l.line.clone()).collect();
        let mut new = splice(&theirs, &keep, &mine);
        hide_upstreams_unfinished(&mut new, &own);
        hide_after_own_unfinished(&mut new, &own);
        if new != theirs {
            files.insert(p, new);
        }
    }
    files
}

/// Rules 3 and 10: `base` with each version of `applied` set. A line `base`
/// holds keeps its place (R10.7); a new one is placed in the order `src`
/// has the lines (design choice, as a sequence CRDT's left origin): after
/// the nearest line before it in `src` that the result holds, then past
/// any lines there that `src` lacks (lines `base` added that `src` never
/// saw), so of two inserts at one place the base's comes first. The
/// referee calls it with the upstream's file as `base` and the incoming one
/// as `src`; a sync with the upstream's file as `base` and the fork's as
/// `src`. A version of the name line replaces the name.
pub(super) fn splice(base: &Content, applied: &[Line], src: &Content) -> Content {
    let by_line: BTreeMap<&str, &Line> = applied.iter().map(|l| (l.line.as_str(), l)).collect();
    let in_src: BTreeSet<&str> = src.lines.iter().map(|l| l.line.as_str()).collect();
    let mut lines: Vec<Line> = base.lines.clone();
    for (k, s) in src.lines.iter().enumerate() {
        let Some(v) = by_line.get(s.line.as_str()) else {
            continue;
        };
        if let Some(at) = lines.iter().position(|o| o.line == s.line) {
            lines[at] = (*v).clone();
            continue;
        }
        let anchor = src.lines[..k]
            .iter()
            .rev()
            .find_map(|l| lines.iter().position(|o| o.line == l.line));
        let mut at = anchor.map_or(0, |a| a + 1);
        while at < lines.len() && !in_src.contains(lines[at].line.as_str()) {
            at += 1;
        }
        lines.insert(at, (*v).clone());
    }
    let name = by_line
        .get(base.name.line.as_str())
        .map_or_else(|| base.name.clone(), |v| (*v).clone());
    Content { name, lines }
}

/// Whether a line's bytes end without a newline; false for a deleted line.
fn unfinished(l: &Line) -> bool {
    l.text().is_some_and(|t| !t.ends_with('\n'))
}

/// R10.8: the first applied unfinished line that `placed` keeps a live line
/// after: (its line identity, the version of the next live line).
fn unfinished_before_kept(placed: &Content, applied: &[Line]) -> Option<(String, String)> {
    let mut live = placed.live().peekable();
    while let Some(l) = live.next() {
        if unfinished(l)
            && applied.iter().any(|a| a.id == l.id)
            && let Some(next) = live.peek()
        {
            return Some((l.line.clone(), next.id.clone()));
        }
    }
    None
}

/// R10.6 at a sync: remove from `c` the upstream's unfinished last line when any
/// of `own` follows it. Nothing else.
fn hide_upstreams_unfinished(c: &mut Content, own: &BTreeSet<String>) {
    let front = c
        .lines
        .iter()
        .position(|l| !own.contains(&l.line) && unfinished(l));
    if let Some(at) = front
        && c.lines[at + 1..]
            .iter()
            .any(|l| l.live() && own.contains(&l.line))
    {
        c.lines.remove(at);
    }
}

/// R10.9 at a sync: remove from `c` the upstream's live lines after a line of
/// `own` that is unfinished. Nothing else.
fn hide_after_own_unfinished(c: &mut Content, own: &BTreeSet<String>) {
    let Some(at) = c
        .lines
        .iter()
        .position(|l| own.contains(&l.line) && unfinished(l))
    else {
        return;
    };
    let later = c.lines.split_off(at + 1);
    c.lines.extend(
        later
            .into_iter()
            .filter(|l| !l.live() || own.contains(&l.line)),
    );
}
