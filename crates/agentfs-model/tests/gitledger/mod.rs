//! Test-only: which changes each take judged, and which of them won or
//! lost, re-derived from git alone (commit trailers, parents, collision
//! refs as they appear) and the harness's own record of
//! when it observed each branch move, then compared with the model's own
//! `Landed` record. The harness rebuilds each tree's landing queue itself
//! from the branch moves it sees, so a change the model never judged is
//! caught even when the model's world and its record agree.

use agentfs_model::calls::TreeId;
use agentfs_model::git::{git, try_git};
use agentfs_model::world::{Direction, Step, World};
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
    Landing { from: TreeId, commit: String },
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
    queues: BTreeMap<TreeId, Vec<Q>>,
    /// The queue entries (by index) each tree's takes have taken: P1 lets
    /// a landing go ahead of an earlier waiting entry.
    done: BTreeMap<TreeId, BTreeSet<usize>>,
    at_next: BTreeMap<TreeId, usize>,
    landings_seen: usize,
    takes: usize,
}

thread_local! {
    static L: RefCell<Option<Ledger>> = const { RefCell::new(None) };
    static ON: RefCell<bool> = const { RefCell::new(false) };
}

/// Turns the ledger on for the World built next.
pub fn enable(on: bool) {
    ON.with(|o| *o.borrow_mut() = on);
    L.with(|l| *l.borrow_mut() = None);
}

fn name(t: TreeId) -> String {
    if t == TreeId::ROOT {
        "main".into()
    } else {
        format!("fork/{t}")
    }
}

fn tree_of_name(s: &str) -> Option<TreeId> {
    if s == "main" {
        return Some(TreeId::ROOT);
    }
    s.strip_prefix("fork/")?.parse().ok().map(TreeId::new)
}

impl Ledger {
    fn meta(&mut self, c: &str) -> Meta {
        if let Some(m) = self.meta.get(c) {
            return m.clone();
        }
        let out = git(
            &self.repo,
            &[
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

    fn tree_id(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^{{tree}}")])
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
            l.heads.insert("main".into(), w.head(TreeId::ROOT));
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
        for line in refs.lines() {
            let (oid, r) = line.split_once(' ').expect("ref line");
            if let Some(b) = r.strip_prefix("refs/heads/") {
                if b == "main" || b.starts_with("fork/") {
                    heads.insert(b.to_string(), oid.to_string());
                }
            } else if r.starts_with("refs/collisions/") {
                collisions.insert(r.to_string(), oid.to_string());
            }
        }
        let all: BTreeSet<String> = w.all_commits().into_iter().collect();
        let new: BTreeSet<String> = all.difference(&l.seen).cloned().collect();
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
                    t != x && w.upstream_of(t).is_some() && w.upstream(t).contains(&x)
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
            if let Some(upstream) = w.upstream_of(x) {
                let f = x;
                let q = l.queues.entry(upstream).or_default();
                for c in &queued {
                    q.push(Q::Landing { from: f, commit: c.clone() });
                }
            }
            for child in w.forks.values().filter(|k| k.upstream == x) {
                if heads.contains_key(&name(child.id)) {
                    let q = l.queues.entry(child.id).or_default();
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
            let (change, won): (String, bool) = match dir {
                Direction::Transaction => {
                    let m = l.meta(&r);
                    let head = m.stored.get(1).cloned().ok_or("ledger: a transaction commit with one parent")?;
                    let lost = losers.contains(&head);
                    losers.retain(|o| *o != head);
                    if lost && l.tree_id(&r) != l.tree_id(&m.stored[0]) {
                        return Err(format!("ledger: transaction {r:.7} lost but changed the tree"));
                    }
                    (head, !lost)
                }
                Direction::Landing => {
                    // The harness's queue: the entries not yet taken that
                    // were queued when the take began. Which one the take
                    // began with is read from git: the commit the landing
                    // names.
                    let q = l.queues.get(&x).cloned().unwrap_or_default();
                    let limit = (*l.at_next.get(&x).unwrap_or(&q.len())).min(q.len());
                    let done = l.done.get(&x).cloned().unwrap_or_default();
                    let open: Vec<usize> = (0..limit)
                        .filter(|i| !done.contains(i) && matches!(q[*i], Q::Landing { .. }))
                        .collect();
                    let named = if before.is_none() {
                        Some(r.clone())
                    } else {
                        l.meta(&r).stored.get(1).cloned()
                    };
                    let start = open.iter().copied().find(
                        |i| matches!(&q[*i], Q::Landing { commit, .. } if Some(commit) == named.as_ref()),
                    );
                    let Some(i) = start else {
                        return Err(format!("ledger: a landing into {x:?} ({r:.7}) of nothing queued"));
                    };
                    let Some(Q::Landing { from, commit }) = q.get(i).cloned() else {
                        unreachable!("a landing entry");
                    };
                    // One fork's entries keep their order (P1).
                    if let Some(j) = open
                        .iter()
                        .find(|j| **j < i && matches!(&q[**j], Q::Landing { from: g, .. } if *g == from))
                    {
                        return Err(format!(
                            "ledger: {x:?} took {commit:.7} of f{from} ahead of its earlier entry {j}"
                        ));
                    }
                    l.done.entry(x).or_default().insert(i);
                    // A fast-forward: the queued commit itself, won.
                    let lost = before.is_some() && losers.contains(&commit);
                    if lost && l.tree_id(&r) != l.tree_id(before.as_ref().unwrap()) {
                        return Err(format!("ledger: {commit:.7} lost in {r:.7}, but its tree changed"));
                    }
                    losers.retain(|o| *o != commit || !lost);
                    (commit, !lost)
                }
            };
            if rec.change != change || rec.won != won {
                return Err(format!(
                    "ledger: take {r:.7} into {x:?}: from git change {change} won {won}; the model's record change {} won {}",
                    rec.change, rec.won
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
