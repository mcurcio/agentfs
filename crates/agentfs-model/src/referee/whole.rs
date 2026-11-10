//! The referee's whole (land only, rule 3e): the change lands whole or loses
//! whole; its applied versions are placed ([`super::placing::splice`]).

use std::collections::BTreeMap;

use super::placing::splice;
use super::{By, Lineage, Lines, Lost, Plan, Referee, Verdict, key};

impl<L: Lineage + ?Sized> Referee<'_, L> {
    /// A landing's plan: if any version lost, every other version loses
    /// `By::Whole` to the first loss's winner and none applies; else the
    /// applied versions are placed in the order `tip` has them.
    pub(super) fn whole(&self, ours: &str, tip: &str, lines: &Lines<'_>) -> Plan {
        let by_path = &lines.by_path;
        let verdicts = &lines.verdicts;
        let by_file = &lines.by_file;
        // Rule 3e: the change lands whole or loses whole. Any lost version
        // loses the change: its other versions lose too, named for the
        // first loss's winner, and nothing of it applies.
        let decider = by_path
            .values()
            .flatten()
            .find_map(|i| match &verdicts[&key(i)] {
                Verdict::Lose(won) => Some(won.clone()),
                _ => None,
            });
        let mut files = BTreeMap::new();
        let mut collisions = Vec::new();
        for (p, ins) in by_path {
            let cur = self.file(ours, p);
            let lost = |by| Lost {
                path: p.to_string(),
                lines: Vec::new(),
                lost: Vec::new(),
                won: Vec::new(),
                by,
                loser: tip.to_string(),
            };
            let mut real = lost(if by_file.contains(*p) {
                By::File
            } else {
                By::Line
            });
            let mut whole = lost(By::Whole);
            let mut applied = Vec::new();
            for i in ins {
                let (into, won) = match (&verdicts[&key(i)], &decider) {
                    (Verdict::Drop, _) => continue,
                    (Verdict::Apply, None) => {
                        applied.push(i.version.clone());
                        continue;
                    }
                    (Verdict::Apply, Some(d)) => (&mut whole, d.clone()),
                    (Verdict::Lose(won), _) => (&mut real, won.clone()),
                };
                into.lines.push(i.version.line.clone());
                into.lost.push(i.version.id.clone());
                into.won.push(won);
            }
            let new = splice(&cur, &applied, &self.file(tip, p));
            if new != cur {
                files.insert(p.to_string(), new);
            }
            collisions.extend([real, whole].into_iter().filter(|l| !l.lost.is_empty()));
        }
        Plan { files, collisions }
    }
}
