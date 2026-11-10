//! The referee's names, each to a fixed point: at a landing, two files claiming
//! one path, the upstream's keeps it (rule 11); at a sync, the fork's names
//! shown back where the upstream's clash (R11.1).

use std::collections::{BTreeMap, BTreeSet};

use super::{Incoming, Lineage, Lines, Referee, Verdict, key};
use crate::content::{Content, Line, PATHS, State};

impl<L: Lineage + ?Sized> Referee<'_, L> {
    /// A landing's names: a rename onto a path another file holds in the
    /// result loses.
    pub(super) fn names(&self, ours: &str, incoming: &[Incoming], lines: &mut Lines<'_>) {
        let verdicts = &mut lines.verdicts;
        let by_file = &mut lines.by_file;
        // A rename-over (one call: both name lines, in one save) needs no
        // rule of its own: if either half loses, the save loses whole
        // (rule 3e, below). So the path its unlink half would have freed is
        // no claim against its rename half (finding 25): a file this change
        // itself replaces keeps its name only because the change lost, and
        // the rename loses with it (`By::Whole`), not to that name.
        let replaced: BTreeSet<&str> = incoming
            .iter()
            .filter(|i| i.version.line == i.path && matches!(i.version.state, State::Replaced(_)))
            .map(|i| i.path.as_str())
            .collect();
        loop {
            let mut changed = false;
            // Two files claiming one path: the upstream's keeps it.
            let names: Vec<(String, Line)> = PATHS
                .iter()
                .map(|f| {
                    let k = (f.to_string(), f.to_string());
                    let name = match verdicts.get(&k) {
                        Some(Verdict::Apply) => incoming
                            .iter()
                            .find(|i| key(i) == k)
                            .map(|i| i.version.clone()),
                        _ => None,
                    };
                    (
                        f.to_string(),
                        name.unwrap_or_else(|| self.file(ours, f).name),
                    )
                })
                .collect();
            for (f, n) in &names {
                let k = (f.clone(), f.clone());
                if verdicts.get(&k) != Some(&Verdict::Apply) || n.text().is_none() {
                    continue;
                }
                if let Some((_, g)) = names
                    .iter()
                    .find(|(g, m)| g != f && m.text() == n.text() && !replaced.contains(g.as_str()))
                {
                    verdicts.insert(k, Verdict::Lose(g.id.clone()));
                    by_file.insert(f.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Finding 11, two renames to one path before the upstream decides (a
    /// choice, recommended on 2026-11-04 without objection): a sync keeps
    /// the fork's undecided name; had the upstream meanwhile given that path
    /// to another file (a rename, a rename-over, a create), the fork shows
    /// the upstream's file under the name it has in the fork (its previous
    /// name; none, so not shown, for a file the fork has never had) until
    /// the upstream's queue decides the fork's rename. A chain (the upstream
    /// then gave that previous name to a third file) is the same rule
    /// again: each file whose upstream's name clashes with a name the sync
    /// shows is shown under its fork's name, until no two clash; the fork's
    /// own tree has no clash, so this ends. `files`: the sync's changed
    /// files, by identity, over `up`.
    pub(super) fn keep_own_names(
        &self,
        ours: &str,
        up: &str,
        files: &mut BTreeMap<String, Content>,
    ) {
        let now = |files: &BTreeMap<String, Content>, f: &str| {
            files.get(f).cloned().unwrap_or_else(|| self.file(up, f))
        };
        loop {
            let mut at: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for f in PATHS {
                if let Some(p) = now(files, f).path() {
                    at.entry(p.to_string()).or_default().push(f);
                }
            }
            // A file in a clash showing a name other than its fork's.
            let back = at
                .values()
                .filter(|v| v.len() > 1)
                .flatten()
                .find(|f| now(files, f).name.id != self.file(ours, f).name.id);
            let Some(f) = back.map(|f| f.to_string()) else {
                return;
            };
            let mut c = now(files, &f);
            c.name = self.file(ours, &f).name.clone();
            files.insert(f, c);
        }
    }
}
