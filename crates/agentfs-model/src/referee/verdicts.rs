//! The referee's verdicts: rule 3 per line identity, then rule 11's coupling of
//! a file's existence with its content.

use super::{Incoming, Lineage, Lines, Referee, Verdict, key};
use crate::content::Content;

impl<L: Lineage + ?Sized> Referee<'_, L> {
    /// Each incoming version against the upstream's of the same line
    /// ([`Referee::verdict`]); then an edit of a file the upstream unlinked
    /// loses, all of it, and an unlink loses whole if the upstream has a live
    /// line its writer did not see ([`Referee::seen`]).
    pub(super) fn verdicts(&self, ours: &str, tip: &str, lines: &mut Lines<'_>) {
        let by_path = &lines.by_path;
        let verdicts = &mut lines.verdicts;
        let by_file = &mut lines.by_file;
        for (p, ins) in by_path {
            let cur = self.file(ours, p);
            for i in ins {
                verdicts.insert(key(i), self.verdict(ours, &cur, i));
            }
            let unlink = ins
                .iter()
                .find(|i| i.version.line == cur.name.line && !i.version.live());
            // A create: a live name version written over the upstream's
            // deleted one, which only an unborn file's create can be (a
            // program cannot rename a file no path names).
            let creates = ins.iter().any(|i| {
                i.version.line == cur.name.line
                    && i.version.live()
                    && verdicts[&key(i)] == Verdict::Apply
            });
            if !cur.name.live() && unlink.is_none() && !creates {
                // An edit of a file the upstream unlinked: the unlink wins.
                for i in ins {
                    if verdicts[&key(i)] != Verdict::Drop {
                        verdicts.insert(key(i), Verdict::Lose(cur.name.id.clone()));
                        by_file.insert(p.to_string());
                    }
                }
            } else if let Some(u) = unlink
                && verdicts[&key(u)] == Verdict::Apply
                && cur.name.live()
            {
                // An unlink loses whole to a line its writer did not see. Not to a
                // line of a file the upstream unlinked too: two deletes of one name
                // line agree (R10.3), whatever the file held (finding 47, seed 484).
                let t = self.file(tip, p);
                if let Some(q) = cur.live().find(|q| !self.seen(tip, &t, p, q)) {
                    for i in ins {
                        verdicts.insert(key(i), Verdict::Lose(q.id.clone()));
                    }
                    by_file.insert(p.to_string());
                }
            }
        }
    }

    /// Rule 3, per line identity: an incoming version against the version
    /// `cur` (the upstream's, at `ours`) holds of the same line. None there
    /// (a line new to the upstream): it applies. The same version: nothing.
    /// Written over the upstream's: it replaces it. The upstream's written over
    /// it: stale, dropped. Two concurrent deletes agree: the incoming one
    /// applies, an ordinary version of its change. Else the two are
    /// concurrent, and the upstream's wins.
    fn verdict(&self, ours: &str, cur: &Content, i: &Incoming) -> Verdict {
        let Some(p) = cur.member(&i.version.line) else {
            return Verdict::Apply;
        };
        if p.id == i.version.id {
            Verdict::Drop
        } else if self
            .written_over(&i.origin, &i.path, &i.version)
            .contains(&p.id)
        {
            Verdict::Apply
        } else if self
            .written_over(&self.origin_of(ours, &i.path, &p.id), &i.path, p)
            .contains(&i.version.id)
        {
            Verdict::Drop
        } else if !p.live() && !i.version.live() {
            Verdict::Apply
        } else {
            Verdict::Lose(p.id.clone())
        }
    }
}
