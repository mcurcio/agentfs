//! Test-only: which changes each take judged, and which of them won or
//! lost, re-derived from git alone (commit trailers, stored and grafted
//! parents, collision refs as they appear) and the harness's own record of
//! when it observed each branch move, then compared with the model's own
//! `Landed` record. The harness rebuilds each tree's landing queue itself
//! from the branch moves it sees, so a change the model never judged, or
//! a squash that took the wrong saves, is caught even when the model's
//! world and its record agree.

use agentfs_model::git::{git, try_git};
use agentfs_model::world::{Direction, HistoryPolicy, Step, Tree, World};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Debug)]
struct Meta {
    stored: Vec<String>,
    cause: String,
    tree: String,
}

#[derive(Clone, Debug, PartialEq)]
enum Q {
    Landing { from: u32, commit: String },
    Sync,
}

#[derive(Default)]
struct Ledger {
    repo: PathBuf,
    refs: String,
    seen: BTreeSet<String>,
    heads: BTreeMap<String, String>,
    collisions: BTreeSet<String>,
    meta: BTreeMap<String, Meta>,
    queues: BTreeMap<Tree, Vec<Q>>,
    cursor: BTreeMap<Tree, usize>,
    at_next: BTreeMap<Tree, usize>,
    landings_seen: usize,
    takes: usize,
}

thread_local! {
    static L: RefCell<Option<Ledger>> = const { RefCell::new(None) };
    static ON: RefCell<bool> = const { RefCell::new(false) };
}

/// Turns the ledger on for the World built next (the random runs only:
/// `rounds` makes several calls between checks).
pub fn enable(on: bool) {
    ON.with(|o| *o.borrow_mut() = on);
    L.with(|l| *l.borrow_mut() = None);
}

fn name(t: Tree) -> String {
    match t {
        Tree::Main => "main".into(),
        Tree::Fork(f) => format!("fork/{f}"),
    }
}

fn tree_of_name(s: &str) -> Option<Tree> {
    if s == "main" {
        return Some(Tree::Main);
    }
    s.strip_prefix("fork/")?.parse().ok().map(Tree::Fork)
}

impl Ledger {
    fn meta(&mut self, c: &str) -> Meta {
        if let Some(m) = self.meta.get(c) {
            return m.clone();
        }
        let out = git(
            &self.repo,
            &[
                "--no-replace-objects",
                "log",
                "-1",
                "--format=%P%x1f%(trailers:key=Agentfs-Cause,valueonly)%x1f%(trailers:key=Agentfs-Tree,valueonly)",
                c,
            ],
        );
        let f: Vec<&str> = out.split('\x1f').collect();
        let m = Meta {
            stored: f[0].split_whitespace().map(str::to_string).collect(),
            cause: f.get(1).unwrap_or(&"").trim().to_string(),
            tree: f.get(2).unwrap_or(&"").trim().to_string(),
        };
        self.meta.insert(c.to_string(), m.clone());
        m
    }

    /// Lineage parents: grafts applied.
    fn lineage(&self, c: &str) -> Vec<String> {
        git(&self.repo, &["rev-list", "--parents", "-n1", c])
            .split(' ')
            .skip(1)
            .map(str::to_string)
            .collect()
    }

    fn tree_id(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^{{tree}}")])
    }

    /// The changes a queued commit brings, from git: a squashed landing
    /// commit (Squashed) the saves its graft names, recursively; else itself.
    fn expand(&mut self, w: &World, c: &str) -> Vec<String> {
        let m = self.meta(c);
        if w.policies.history == HistoryPolicy::Squashed && m.cause == "Landing" {
            let lin = self.lineage(c);
            return lin[1..].iter().flat_map(|u| self.expand(w, u)).collect();
        }
        vec![c.to_string()]
    }
}

pub fn note_next(w: &World, s: &Step) -> Result<(), String> {
    observe(w)?;
    if let Step::Next(t) = s {
        L.with(|l| {
            if let Some(l) = l.borrow_mut().as_mut() {
                let n = l.queues.get(t).map_or(0, Vec::len);
                l.at_next.insert(*t, n);
            }
        });
    }
    Ok(())
}

pub fn takes() -> usize {
    L.with(|l| l.borrow().as_ref().map_or(0, |l| l.takes))
}

pub fn observe(w: &World) -> Result<(), String> {
    if !ON.with(|o| *o.borrow()) {
        return Ok(());
    }
    L.with(|cell| {
        let mut g = cell.borrow_mut();
        if g.as_ref().is_none_or(|l| l.repo != w.repo()) {
            let mut l = Ledger {
                repo: w.repo().to_path_buf(),
                ..Default::default()
            };
            l.heads.insert("main".into(), w.head(Tree::Main));
            l.seen = w.all_commits().into_iter().collect();
            *g = Some(l);
        }
        let l = g.as_mut().expect("ledger");
        let refs = git(&l.repo, &["for-each-ref", "--format=%(objectname) %(refname)"]);
        if refs == l.refs {
            return Ok(());
        }
        l.refs = refs.clone();
        let mut heads = BTreeMap::new();
        let mut collisions = BTreeMap::new();
        let mut replaced = BTreeSet::new();
        for line in refs.lines() {
            let (oid, r) = line.split_once(' ').expect("ref line");
            if let Some(b) = r.strip_prefix("refs/heads/") {
                if b == "main" || b.starts_with("fork/") {
                    heads.insert(b.to_string(), oid.to_string());
                }
            } else if r.starts_with("refs/collisions/") {
                collisions.insert(r.to_string(), oid.to_string());
            } else if r.starts_with("refs/replace/") {
                replaced.insert(oid.to_string());
            }
        }
        let all: BTreeSet<String> = w.all_commits().into_iter().collect();
        let new: BTreeSet<String> = all
            .difference(&l.seen)
            .filter(|c| !replaced.contains(*c))
            .cloned()
            .collect();
        let new_refs: Vec<String> = collisions
            .iter()
            .filter(|(r, _)| !l.collisions.contains(*r))
            .map(|(_, o)| o.clone())
            .collect();
        let mut losers: Vec<String> = new_refs.clone();
        let mut moved = Vec::new();
        for (b, h) in &heads {
            if l.heads.get(b) != Some(h) {
                moved.push((tree_of_name(b).expect("branch"), l.heads.get(b).cloned(), h.clone()));
            }
        }
        if moved.len() > 1 {
            return Err(format!("ledger: {} trees moved between two observations", moved.len()));
        }
        let mut found = Vec::new();
        for (x, old, h) in moved {
            // Walk the new commits back by first (stored) parent.
            let mut chain = Vec::new();
            let mut c = h.clone();
            while new.contains(&c) {
                chain.push(c.clone());
                let m = l.meta(&c);
                c = m.stored.first().cloned().unwrap_or_default();
            }
            chain.reverse();
            let mut moves = chain.len();
            let mut queued = Vec::new();
            let mut ff: Option<String> = None;
            if old.as_deref() != Some(c.as_str()) && old.is_some() {
                moves += 1;
                let m = l.meta(&c);
                let below = tree_of_name(&m.tree).is_some_and(|t| {
                    t != x && matches!(t, Tree::Fork(_)) && w.upstream(t).contains(&x)
                });
                if below {
                    ff = Some(c.clone());
                    queued.push(c.clone());
                }
            }
            for c in &chain {
                let m = l.meta(c);
                if m.tree == name(x)
                    && (m.cause == "Direct" || m.cause == "Landing" || m.cause.starts_with("Transaction "))
                {
                    queued.push(c.clone());
                }
            }
            if let Tree::Fork(f) = x {
                let parent = w.forks[&f].parent;
                let q = l.queues.entry(parent).or_default();
                for c in &queued {
                    q.push(Q::Landing { from: f, commit: c.clone() });
                }
            }
            for child in w.forks.values().filter(|k| k.parent == x) {
                if heads.contains_key(&name(Tree::Fork(child.id))) {
                    let q = l.queues.entry(Tree::Fork(child.id)).or_default();
                    for _ in 0..moves {
                        q.push(Q::Sync);
                    }
                }
            }
            // The takes into x this window: a fast-forward, landing commits,
            // transaction commits.
            if let Some(k) = ff {
                found.push((x, Direction::Landing, k.clone(), None::<String>));
            }
            for c in &chain {
                let m = l.meta(c);
                if m.tree != name(x) {
                    continue;
                }
                if m.cause == "Landing" {
                    found.push((x, Direction::Landing, c.clone(), Some(m.stored[0].clone())));
                } else if m.cause.starts_with("Transaction ") {
                    found.push((x, Direction::Transaction, c.clone(), Some(m.stored[0].clone())));
                }
            }
        }
        let squashed = w.policies.history == HistoryPolicy::Squashed;
        let records: Vec<_> = w.landings[l.landings_seen..].to_vec();
        if records.len() != found.len() {
            return Err(format!(
                "ledger: git shows {} takes this step, the model recorded {}",
                found.len(),
                records.len()
            ));
        }
        for (x, dir, r, before) in found {
            l.takes += 1;
            let rec = records
                .iter()
                .find(|d| d.tree == x && d.direction == dir && d.result == r)
                .ok_or_else(|| format!("ledger: take {r:.7} into {x:?} has no Landed record"))?;
            let (units, winners): (Vec<String>, Vec<String>) = match dir {
                Direction::Transaction => {
                    let m = l.meta(&r);
                    let head = m.stored.get(1).cloned().ok_or("ledger: a transaction commit with one parent")?;
                    let lost = losers.contains(&head);
                    losers.retain(|o| *o != head);
                    if lost && l.tree_id(&r) != l.tree_id(&m.stored[0]) {
                        return Err(format!("ledger: transaction {r:.7} lost but changed the tree"));
                    }
                    (vec![head.clone()], if lost { vec![] } else { vec![head] })
                }
                Direction::Landing => {
                    // The harness's queue: the first entries not yet taken.
                    let q = l.queues.get(&x).cloned().unwrap_or_default();
                    let mut i = *l.cursor.get(&x).unwrap_or(&0);
                    while i < q.len() && q[i] == Q::Sync {
                        i += 1;
                    }
                    let Some(Q::Landing { from, commit }) = q.get(i).cloned() else {
                        return Err(format!("ledger: a landing into {x:?} with nothing queued"));
                    };
                    let mut slice = vec![commit.clone()];
                    let limit = *l.at_next.get(&x).unwrap_or(&q.len());
                    let mut j = i + 1;
                    let plain = |l: &mut Ledger, c: &str| {
                        let m = l.meta(c);
                        m.cause == "Direct"
                    };
                    if squashed && plain(l, &commit) {
                        while j < limit {
                            match &q[j] {
                                Q::Landing { from: g, commit: c } if *g == from && plain(l, c) => {
                                    slice.push(c.clone());
                                    j += 1;
                                }
                                _ => break,
                            }
                        }
                    }
                    l.cursor.insert(x, j);
                    let expect: Vec<String> = slice.iter().flat_map(|c| l.expand(w, c)).collect();
                    let units = expect.clone();
                    let winners: Vec<String> = if squashed {
                        let lin = l.lineage(&r);
                        let m = l.meta(&r);
                        if m.stored != [before.clone().unwrap_or_default()] {
                            return Err(format!("ledger: squashed {r:.7} stored parents {:?}", m.stored));
                        }
                        if lin[0] != m.stored[0] {
                            return Err(format!("ledger: {r:.7}'s graft drops its first parent"));
                        }
                        lin[1..].to_vec()
                    } else if before.is_none() {
                        // A fast-forward: the queued commit itself, won.
                        if r != commit {
                            return Err(format!("ledger: fast-forward to {r:.7}, queue head {commit:.7}"));
                        }
                        vec![r.clone()]
                    } else {
                        let m = l.meta(&r);
                        if m.stored.get(1) != Some(&commit) {
                            return Err(format!(
                                "ledger: {r:.7} landed {:?}, the harness's queue head is {commit:.7}",
                                m.stored.get(1)
                            ));
                        }
                        if losers.contains(&commit) { vec![] } else { vec![commit.clone()] }
                    };
                    let lost: Vec<String> = units.iter().filter(|u| losers.contains(u)).cloned().collect();
                    for u in &units {
                        let (won, lo) = (winners.contains(u), lost.contains(u));
                        if won == lo {
                            return Err(format!(
                                "ledger: {u:.7} into {x:?} ({r:.7}): from git won {won}, lost {lo}"
                            ));
                        }
                    }
                    if let Some(s) = winners.iter().find(|s| !units.contains(s)) {
                        return Err(format!("ledger: {r:.7}'s graft names {s:.7}, which the queue did not hold"));
                    }
                    let order: Vec<&String> = units.iter().filter(|u| winners.contains(u)).collect();
                    if order != winners.iter().collect::<Vec<_>>() {
                        return Err(format!("ledger: {r:.7}'s winners out of queue order"));
                    }
                    if winners.is_empty() && before.is_some() && l.tree_id(&r) != l.tree_id(before.as_ref().unwrap()) {
                        return Err(format!("ledger: every change of {r:.7} lost, but its tree changed"));
                    }
                    losers.retain(|o| !lost.contains(o));
                    (units, winners)
                }
            };
            let rec_units: Vec<String> = rec.units.iter().map(|(u, _)| u.clone()).collect();
            if rec_units != units || rec.winners != winners {
                return Err(format!(
                    "ledger: take {r:.7} into {x:?}: from git units {units:?} winners {winners:?}; the model's record units {rec_units:?} winners {:?}",
                    rec.winners
                ));
            }
        }
        if !losers.is_empty() {
            return Err(format!("ledger: collision refs to {losers:?} name no change a take judged"));
        }
        l.landings_seen = w.landings.len();
        l.collisions = collisions.into_keys().collect();
        l.heads = heads;
        l.seen = all;
        let _ = try_git;
        Ok(())
    })
}
