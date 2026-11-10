//! The referee: every line and name rule of one pair of trees, a fork and its
//! upstream (`docs/target-architecture/future/module-cut.md`). It reads the
//! history only through its port ([`Lineage`]), writes nothing, and gives back a
//! plan: land ([`land`]) the files a change writes into its upstream and the
//! records of what lost; sync ([`sync`]) the files a sync writes into the fork.
//! The world builds the trees, the commits and the decisions.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::{Content, Line, PATHS, State};
use crate::world::{By, Direction, Edit};

/// Ancestry and commit reachability, over git in the model: every read the
/// referee makes. Each answer is about immutable objects, so it is the same
/// whenever it is asked.
pub(crate) trait Lineage {
    /// File `p` (its identity) at a commit or tree.
    fn file(&self, at: &str, p: &str) -> Content;
    /// Whether commit `a` is `b` or an ancestor of it.
    fn is_ancestor(&self, a: &str, b: &str) -> bool;
    /// The commit that wrote version `id` of file `p`, from commit `at`, which holds it.
    fn origin_of(&self, at: &str, p: &str, id: &str) -> String;
    /// The versions version `v` of file `p` (written by commit `origin`) was written over.
    fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String>;
}

/// The referee over one port.
struct Referee<'a, L: Lineage + ?Sized> {
    git: &'a L,
}

/// What a landing plans: the files it writes over the upstream's head, by
/// identity, and what of the change lost.
pub(crate) struct Plan {
    pub(crate) files: BTreeMap<String, Content>,
    pub(crate) collisions: Vec<Lost>,
}

/// Rules 3, 10 and 11 at the upstream's queue: one change (`incoming`, from
/// commit `tip`) into the upstream's head `ours`.
pub(crate) fn land<L: Lineage + ?Sized>(
    git: &L,
    dir: Direction,
    ours: &str,
    incoming: &[Incoming],
    tip: &str,
) -> Plan {
    Referee { git }.land_all(dir, ours, incoming, tip)
}

/// [`crate::world::MergeRule::PerLine`]'s sync of the upstream's `up` into the
/// fork at `ours`: the files it writes over `up`, by identity. `changed`: the
/// files `up` changes from `ours`.
pub(crate) fn sync<L: Lineage + ?Sized>(
    git: &L,
    ours: &str,
    up: &str,
    changed: BTreeSet<String>,
) -> BTreeMap<String, Content> {
    Referee { git }.sync_all(ours, up, changed)
}

/// One file's incoming versions of one change lost in a landing, for one
/// cause: their line identities, the versions, and the tree's versions
/// that won, one for each.
#[derive(Clone, Debug)]
pub(crate) struct Lost {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
    pub(crate) lost: Vec<String>,
    pub(crate) won: Vec<String>,
    pub(crate) by: By,
    /// The change that lost.
    pub(crate) loser: String,
}

/// One line version a landing carries.
#[derive(Clone, Debug)]
pub(crate) struct Incoming {
    pub(crate) path: String,
    pub(crate) version: Line,
    /// The commit that wrote it (`git blame`).
    pub(crate) origin: String,
}

impl Incoming {
    pub(crate) fn edit(&self) -> Edit {
        Edit {
            path: self.path.clone(),
            line: self.version.line.clone(),
            token: self.version.id.clone(),
            gone: !self.version.live(),
        }
    }
}

/// The referee's verdict on one incoming version (rule 3).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    Apply,
    /// The same version, or a stale one.
    Drop,
    /// The upstream's version (or the line that decided a whole loss) wins.
    Lose(String),
}

impl<L: Lineage + ?Sized> Referee<'_, L> {
    fn file(&self, at: &str, p: &str) -> Content {
        self.git.file(at, p)
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.git.is_ancestor(a, b)
    }

    fn origin_of(&self, at: &str, p: &str, id: &str) -> String {
        self.git.origin_of(at, p, id)
    }

    fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String> {
        self.git.written_over(origin, p, v)
    }

    /// Rules 3, 10 and 11 at the upstream's queue. Each incoming version
    /// meets the version `ours` (the upstream's head) holds of the same line
    /// identity ([`Referee::verdict`]). Then a file's existence couples with
    /// its content (rule 11): an edit of a file the upstream unlinked loses,
    /// all of it; an unlink loses whole if the upstream has a live line its
    /// writer did not see ([`Referee::seen`]); a rename onto a path another
    /// file holds in the result loses.
    /// At a landing, a line placed after the upstream's unfinished last line
    /// collides with it (R10.4, the owner, 2026-11-05: "collide"), so no
    /// newline appears that no program wrote. So does an applied unfinished
    /// line the upstream would keep a line after (R10.8): the next line wins.
    /// Then the change is judged whole (rule 3e): if any version lost, none
    /// applies. The applied versions are placed by [`splice`], in the order
    /// `tip` (the incoming change) has them.
    fn land_all(&self, dir: Direction, ours: &str, incoming: &[Incoming], tip: &str) -> Plan {
        let mut by_path: BTreeMap<&str, Vec<&Incoming>> = BTreeMap::new();
        for i in incoming {
            by_path.entry(&i.path).or_default().push(i);
        }
        let key = |i: &Incoming| (i.path.clone(), i.version.line.clone());
        let mut verdicts: BTreeMap<(String, String), Verdict> = BTreeMap::new();
        let mut by_file: BTreeSet<String> = BTreeSet::new();
        for (p, ins) in &by_path {
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
        if dir == Direction::Landing {
            for (p, ins) in &by_path {
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
            for (p, ins) in &by_path {
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
        for (p, ins) in &by_path {
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

    /// Whether the writer of commit `tip` (its file `p` is `t`) saw the
    /// upstream's version `q`: `tip` holds it, or a version written over it.
    fn seen(&self, tip: &str, t: &Content, p: &str, q: &Line) -> bool {
        t.member(&q.line).is_some_and(|v| {
            v.id == q.id
                || self
                    .written_over(&self.origin_of(tip, p, &v.id), p, v)
                    .contains(&q.id)
        })
    }

    /// [`crate::world::MergeRule::PerLine`]'s sync: the upstream's version `up`, with each
    /// version of the fork's `ours` the upstream has not decided yet (`up`'s
    /// history does not reach its writing commit: a line still on its way to
    /// the upstream's queue, won or lost there being in that history alike)
    /// kept over the upstream's version of that line, placed in the fork's
    /// order ([`splice`]). No tree shows bytes nobody wrote: the upstream's
    /// unfinished last line in front of a kept line (R10.6) and the upstream's
    /// lines after a kept unfinished line (R10.9) wait for the decision.
    fn sync_all(
        &self,
        ours: &str,
        up: &str,
        changed: BTreeSet<String>,
    ) -> BTreeMap<String, Content> {
        let mut files = BTreeMap::new();
        for p in changed {
            let (mine, theirs) = (self.file(ours, &p), self.file(up, &p));
            let keep: Vec<Line> = mine
                .records()
                .filter(|r| {
                    theirs.member(&r.line).is_none_or(|u| u.id != r.id)
                        && !self.is_ancestor(&self.origin_of(ours, &p, &r.id), up)
                })
                .cloned()
                .collect();
            let own: BTreeSet<String> = keep.iter().map(|l| l.line.clone()).collect();
            let mut new = splice(&theirs, &keep, &mine);
            hide_upstreams_unfinished(&mut new, &own);
            hide_after_own_unfinished(&mut new, &own);
            if new != theirs {
                files.insert(p, new);
            }
        }
        self.keep_own_names(ours, up, &mut files);
        files
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
    fn keep_own_names(&self, ours: &str, up: &str, files: &mut BTreeMap<String, Content>) {
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

/// Rules 3 and 10: `base` with each version of `applied` set. A line `base`
/// holds keeps its place (R10.7); a new one is placed in the order `src`
/// has the lines (design choice, as a sequence CRDT's left origin): after
/// the nearest line before it in `src` that the result holds, then past
/// any lines there that `src` lacks (lines `base` added that `src` never
/// saw), so of two inserts at one place the base's comes first. The
/// referee calls it with the upstream's file as `base` and the incoming one
/// as `src`; a sync with the upstream's file as `base` and the fork's as
/// `src`. A version of the name line replaces the name.
fn splice(base: &Content, applied: &[Line], src: &Content) -> Content {
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
