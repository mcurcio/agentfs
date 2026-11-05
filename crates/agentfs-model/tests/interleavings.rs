//! Seeded random interleavings of client calls and queue steps across
//! main, several forks (nested ones included) and transactions, with every
//! invariant checked after each step (`agentfs_model::check`), and, once
//! writes stop and the queues drain, convergence.
//!
//! Clients open files (some with `O_TRUNC`) and make byte calls, as POSIX
//! gives them (rule 8): `O_APPEND` writes, some of a partial line;
//! `pwrite`s at an offset, some past the end (a hole), some split in two,
//! some overlapping; `ftruncate`s; and the byte calls a program makes for a
//! line edit (a line written in place, inserted, deleted or moved, some with
//! bytes other lines repeat) or a whole-file rewrite of an earlier read.
//! They close them, so takes wait for write-opens and opens wait for takes
//! (rule 9), and while a take holds files they often open, save, unlink or
//! rename one of that tree's files; they unlink and rename files (rule
//! 11), to a new path or over another file. On top of `check`, the run
//! itself asserts that nothing else waits: every close returns at once, and
//! every queue step takes its queue's head, leaving
//! the rest of the queue in order, or records that it waits. And it keeps
//! its own byte oracle ([`Oracle`]): a plain byte array per tree (and open
//! transaction) and file, and each open's calls, updated by the same calls
//! by its own byte code; every close must save exactly the bytes the oracle
//! computes, and every tree must hold the oracle's bytes after every step a
//! referee did not take (a queue step, a transaction's commit), which only
//! the tree it took may change.
//!
//! The seeds are every seed in `1..=SEEDS`, never a set picked because it
//! passes. `AGENTFS_MODEL_SEED=<n>` runs one seed; `AGENTFS_MODEL_SEEDS=<n>`
//! runs seeds 1..=n. Each seed runs under `catch_unwind`, so a harness
//! panic too reports its seed and the event log. The generator is a
//! SplitMix64 written here: fixed seeds replay exactly.

use agentfs_model::check::{change_files, check, check_converged, entry_files, step_checked};
use agentfs_model::content::{BLANK, Content, FILES, Layout, PATHS, UNBORN};
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::program;
use agentfs_model::world::{
    By, Client, Delayed, Direction, Entry, ForkId, HandleId, Kind, Life, LineEdit, MergeRule,
    ModelError, Policies, Step, Take, Target, Tree, TxnEnd, TxnOutcome, World, Write,
};
use std::collections::{BTreeMap, BTreeSet};

mod gitledger;

/// CI runs seeds 1..=SEEDS.
const SEEDS: u64 = 64;
const STEPS: usize = 80;
/// Positions the random clients write at: the first three lines, so edits
/// collide often.
const HOT_SLOTS: usize = 3;
/// Bytes the random clients give some new lines: lines that repeat others'
/// bytes, as blank lines and braces do (rule 10).
const REPEATED: [&str; 2] = [BLANK, "}"];
/// Bytes a raw `pwrite` draws from: newlines among them, so a write splits
/// or joins lines.
const RAW: [&str; 5] = ["x", "y", "\n", "-", "}"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Clone>(&mut self, v: &[T]) -> Option<T> {
        (!v.is_empty()).then(|| v[self.below(v.len())].clone())
    }

    /// 1 to `n` bytes of [`RAW`].
    fn raw(&mut self, n: usize) -> String {
        (0..1 + self.below(n))
            .map(|_| RAW[self.below(RAW.len())])
            .collect()
    }
}

#[derive(Default, Debug)]
struct Tally {
    steps: usize,
    fast_forwards: usize,
    landing_collisions: usize,
    transaction_collisions: usize,
    committed: usize,
    /// Transactions that lost whole at their commit, and the staged lines
    /// their answers named.
    lost: usize,
    not_applied: usize,
    /// Changes a landing judged that lost whole, and those whose clean
    /// lines (`By::Whole`) lost with them.
    changes_lost: usize,
    clean_lines_lost: usize,
    empty: usize,
    rolled_back: usize,
    forks: usize,
    nested: usize,
    /// Queue heads that waited for a write-open, and opens that waited for
    /// a take.
    take_waits: usize,
    open_waits: usize,
    /// Opens, unlinks and renames made while a take held files in that
    /// tree, and saves closed then.
    calls_during_takes: usize,
    saves_during_takes: usize,
    rewrites: usize,
    /// Appends, and those two opens made to one file at once.
    appends: usize,
    concurrent_appends: usize,
    /// Byte calls: appends of a partial line, raw `pwrite`s (and those
    /// past the end), split and overlapping writes, truncates, `O_TRUNC`
    /// opens.
    partial_appends: usize,
    pwrites: usize,
    holes: usize,
    split_writes: usize,
    overlapping: usize,
    truncates: usize,
    o_truncs: usize,
    inserts: usize,
    deletes: usize,
    moves: usize,
    /// New lines whose bytes repeat other lines'.
    repeated: usize,
    unlinks: usize,
    renames: usize,
    rename_overs: usize,
    main_deaths: usize,
    /// Creates, atomic saves (a create renamed over a file), old writers
    /// left open on a file an atomic save may replace, and closes of an
    /// orphan that dropped calls.
    creates: usize,
    atomic_saves: usize,
    old_writers: usize,
    orphans: usize,
    /// Progress (git-model.md §6): queue steps that took an entry ahead of
    /// an earlier waiting one (P1); syncs that delivered a prefix and held
    /// the rest back (P2); opens granted on a file a waiting take waits
    /// for, each an alarm (no P3).
    overtakes: usize,
    prefix_syncs: usize,
    delayed: usize,
    /// Crashes of every agentfs process, and those during a close.
    crashes: usize,
    crashes_during_close: usize,
    /// Crashes while a transaction had a file open.
    crashes_in_txn: usize,
    /// Takes in which a new line met a parent's new line at its place (rule
    /// 10), and changes that edited a file the parent had unlinked (rule
    /// 11), as `check` counted them.
    inserts_met: usize,
    unlinked_edits: usize,
    git_takes: usize,
    git_takes_total: usize,
}

fn seeds() -> Vec<u64> {
    if let Ok(s) = std::env::var("AGENTFS_MODEL_SEED") {
        return vec![s.parse().expect("AGENTFS_MODEL_SEED is a number")];
    }
    if let Ok(n) = std::env::var("AGENTFS_MODEL_SEEDS") {
        let n: u64 = n.parse().expect("AGENTFS_MODEL_SEEDS is a number");
        return (1..=n).collect();
    }
    (1..=SEEDS).collect()
}

fn live_clients(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| f.life == Life::Open)
        .map(|f| f.id)
        .collect()
}

/// A client blocked in a call that waits (an open or a path call waiting
/// for a take): a program makes no other call until it returns, but it can
/// still die.
fn blocked(w: &World, c: &Client) -> bool {
    w.handles().any(|h| h.client == *c && h.waiting)
}

/// The sandboxes' clients that can make a call now.
fn free_clients(w: &World) -> Vec<ForkId> {
    live_clients(w)
        .into_iter()
        .filter(|f| !blocked(w, &Client::Fork(*f)))
        .collect()
}

/// Whether a granted open writes in a transaction.
fn txn_open(w: &World) -> bool {
    w.handles()
        .any(|h| !h.waiting && h.kind == Kind::Write && matches!(h.target, Target::Txn(..)))
}

fn in_txn(w: &World) -> Vec<ForkId> {
    w.forks
        .values()
        .filter(|f| f.txn.is_some())
        .map(|f| f.id)
        .collect()
}

/// The harness's own byte account (rule 8's oracle), kept apart from the
/// model: each target's files' bytes (by file identity) and each open's
/// byte calls, updated by the same calls by [`apply`], never by the model's
/// replay. Read from git only after a referee's step ([`Oracle::reload`]).
#[derive(Default)]
struct Oracle {
    files: BTreeMap<Target, BTreeMap<String, Vec<u8>>>,
    calls: BTreeMap<HandleId, Vec<Write>>,
    /// Objects (git-model.md §7), the harness's own account: each file's
    /// object per target, and each open's object, path and writer.
    inodes: BTreeMap<(Target, String), u64>,
    held: BTreeMap<HandleId, (Target, u64, String, String)>,
    next_inode: u64,
    /// Path calls that wait for a take: (target, file, rename's path).
    waiting_calls: BTreeMap<HandleId, (Target, String, Option<String>)>,
    /// The referee of "no program's call waits on a waiting landing" (the
    /// owner, 2026-11-04), the harness's own record, not the model's
    /// `waits`: each tree with a take in progress (a queue step that began
    /// one and no finish since), with the files it holds (a landing's read
    /// from git, every file its taken changes touch; a sync's, the files
    /// it writes). A call waits only there, on one of those files.
    taking: BTreeMap<Tree, BTreeSet<String>>,
    /// The first alarm fault [`Oracle::alarmed`] found at an open (an open
    /// returns no error), reported by the next [`Oracle::check`].
    alarm_fault: Option<String>,
}

/// The alarm on a waiting take (a choice; no P3), judged per open by the
/// harness's own record of which opens it made and when each was granted:
/// a write-open granted in a tree, at its open or at a take's release
/// (`raised`: the alarms raised then), raises exactly one alarm for each
/// queue entry of that tree then waiting for one of its files, and none for
/// any other entry. An open never makes an entry wait or wakes one, so the
/// tree's waiting entries after the call are those at its grant.
fn alarmed(w: &World, h: HandleId, raised: &[Delayed]) -> Result<(), String> {
    let x = w.handle(h);
    let Target::Tree(t) = x.target else {
        return Ok(());
    };
    let mine: Vec<u64> = raised
        .iter()
        .filter(|d| d.handle == h)
        .map(|d| d.entry)
        .collect();
    if x.waiting || x.kind != Kind::Write {
        return match mine.first() {
            Some(n) => Err(format!(
                "h{h} raised an alarm on entry {n}, but was not granted"
            )),
            None => Ok(()),
        };
    }
    let files = x.files();
    let expect: Vec<u64> = w
        .waits()
        .get(&t)
        .into_iter()
        .flatten()
        .filter(|(_, e)| files.iter().any(|f| e.until.contains(f)))
        .map(|(n, _)| *n)
        .collect();
    if mine != expect {
        return Err(format!(
            "h{h}, granted in {t:?}, raised alarms on entries {mine:?}, not {expect:?} (those waiting for its file)"
        ));
    }
    Ok(())
}

/// An `OrphanDropped` event as the oracle expects it: (open, writer, path,
/// bytes).
type Dropped = (HandleId, String, String, usize);

/// Whether a file is new since its create, read from its line map: an
/// unborn identity whose name version was written over the unborn one. The
/// harness's own reading of git-model.md §7's atomic save.
fn new_file(c: &Content) -> bool {
    UNBORN.contains(&c.name.line.as_str()) && c.name.over.as_deref() == Some(c.name.line.as_str())
}

fn writer(w: &World, c: &Client) -> String {
    match c {
        Client::Main(n) => n.clone(),
        Client::Fork(f) => w.forks[f].writer.clone(),
    }
}

/// One byte call on a plain byte array: the harness's own code.
fn apply(b: &mut Vec<u8>, call: &Write) {
    match call {
        Write::Append(x) => b.extend_from_slice(x.as_bytes()),
        Write::Pwrite { offset, bytes } => {
            for (j, x) in bytes.bytes().enumerate() {
                if b.len() <= offset + j {
                    b.resize(offset + j + 1, 0);
                }
                b[offset + j] = x;
            }
        }
        Write::Truncate(n) => b.resize(*n, 0),
    }
}

fn head_of(w: &World, t: Target) -> Option<String> {
    match t {
        Target::Tree(x) => w.alive(x).then(|| w.head(x)),
        Target::Txn(f, n) => w.forks[&f]
            .txn
            .as_ref()
            .filter(|x| x.id == n)
            .map(|x| x.head.clone()),
    }
}

impl Oracle {
    /// After a referee's step (a queue step, a transaction's commit) or a
    /// new tree or transaction: every live target's bytes, read from git.
    fn reload(&mut self, w: &World) {
        self.files.clear();
        let mut targets: Vec<Target> = w.live_trees().into_iter().map(Target::Tree).collect();
        targets.extend(
            w.forks
                .values()
                .filter_map(|f| f.txn.as_ref().map(|x| Target::Txn(f.id, x.id))),
        );
        for t in targets {
            let head = head_of(w, t).expect("live");
            let files = PATHS
                .iter()
                .map(|p| (p.to_string(), w.file(&head, p).bytes().into_bytes()))
                .collect();
            self.files.insert(t, files);
        }
        self.calls.retain(|h, _| w.handles().any(|x| x.id == *h));
        self.held.retain(|h, _| w.handles().any(|x| x.id == *h));
    }

    /// A new open, granted or waiting: it holds its file's object in its
    /// target from now on.
    fn opened(&mut self, w: &World, h: HandleId) {
        let x = w.handle(h);
        let key = (x.target, x.file.clone());
        let o = match self.inodes.get(&key) {
            Some(o) => *o,
            None => {
                self.next_inode += 1;
                self.inodes.insert(key, self.next_inode);
                self.next_inode
            }
        };
        let who = writer(w, &x.client);
        self.held.insert(h, (x.target, o, x.path.clone(), who));
        if let Err(e) = alarmed(w, h, &w.delayed) {
            self.alarm_fault.get_or_insert(e);
        }
    }

    /// The live file open `h`'s object is in its target, if a path names it.
    fn named(&self, w: &World, h: HandleId) -> Option<String> {
        let (t, o, _, _) = self.held.get(&h)?;
        let head = head_of(w, *t)?;
        self.inodes
            .iter()
            .find(|((u, f), x)| u == t && *x == o && w.file(&head, f).name.live())
            .map(|((_, f), _)| f.clone())
    }

    /// A path call ran in target `t` on the state at commit `at` (before
    /// it): its effect on objects, and an atomic save's on bytes.
    fn ran(&mut self, w: &World, at: &str, t: Target, file: &str, to: Option<&str>) {
        let Some(to) = to else {
            self.inodes.remove(&(t, file.to_string()));
            return;
        };
        let Some(g) = w.lookup(at, to).filter(|g| g != file) else {
            return;
        };
        let o = self.inodes.remove(&(t, g.clone()));
        if new_file(&w.file(at, file)) {
            let o = self.inodes.remove(&(t, file.to_string()));
            if let Some(o) = o {
                self.inodes.insert((t, g.clone()), o);
            }
            if let Some(fs) = self.files.get_mut(&t) {
                let b = fs.get(file).cloned().unwrap_or_default();
                fs.insert(g, b);
            }
        } else {
            let _ = o;
        }
    }

    /// Unlinks `p` (`to` none) or renames it as `c`: the oracle follows the
    /// call when it runs, at once or when a take's release runs it.
    fn path_call(
        &mut self,
        w: &mut World,
        c: &Client,
        p: &str,
        to: Option<&str>,
    ) -> Result<(), String> {
        let t = match c {
            Client::Main(_) => Target::Tree(Tree::Main),
            Client::Fork(f) => match &w.forks[f].txn {
                Some(x) => Target::Txn(*f, x.id),
                None => Target::Tree(Tree::Fork(*f)),
            },
        };
        let at = head_of(w, t).expect("live");
        let file = w.lookup(&at, p).expect("a file");
        let waits = match to {
            None => w.unlink(c, p),
            Some(to) => w.rename(c, p, to),
        };
        match waits {
            None => self.ran(w, &at, t, &file, to),
            Some(h) => {
                self.waiting_calls
                    .insert(h, (t, file, to.map(str::to_string)));
            }
        }
        self.no_wait_on_waiting(w)
    }

    /// After a take's release in `t`: the waiting path calls it ran, in
    /// the order it ran them (by handle), each on the state the one before
    /// left: the head the take's write left, then each call's own commit
    /// (read down the tree's first parents). A call on a file no path names
    /// then, or a rename to the path it has, makes no commit. Calls dropped
    /// with their client go too.
    fn released(&mut self, w: &World, t: Tree) {
        let gone: Vec<HandleId> = self
            .waiting_calls
            .keys()
            .filter(|h| !w.handles().any(|x| x.id == **h))
            .copied()
            .collect();
        let Some(start) = w.taken_head().map(str::to_string) else {
            return;
        };
        let mut chain = Vec::new();
        let mut c = w.head(t);
        while c != start {
            chain.push(c.clone());
            c = git(w.repo(), &["rev-parse", &format!("{c}^1")]);
        }
        let mut at = start;
        for h in gone {
            let (u, file, to) = self.waiting_calls.remove(&h).expect("listed");
            if u != Target::Tree(t) {
                continue;
            }
            let cur = w.file(&at, &file);
            self.ran(w, &at, u, &file, to.as_deref());
            if cur.name.live() && to.as_deref() != cur.path() {
                at = chain.pop().expect("a granted call's commit");
            }
        }
    }

    /// No program's call ever waits on a waiting landing (the owner,
    /// 2026-11-04; P3 deleted), judged by the harness's record alone: every
    /// call that waits, waits in a tree with a take in progress, on a file
    /// that take holds (the owner's lock of 2026-11-03: a take writing its
    /// files holds new write-opens back until it finishes).
    fn no_wait_on_waiting(&self, w: &World) -> Result<(), String> {
        for x in w.handles().filter(|x| x.waiting) {
            let Target::Tree(t) = x.target else {
                return Err(format!("h{} waits in a transaction", x.id));
            };
            if !self
                .taking
                .get(&t)
                .is_some_and(|held| x.files().iter().any(|f| held.contains(f)))
            {
                return Err(format!(
                    "h{} ({:?} of {}) waits in {t:?}, but no take there is writing its file: a call waits on a waiting take",
                    x.id, x.kind, x.file
                ));
            }
        }
        Ok(())
    }

    /// Every target but `skip` holds the oracle's bytes.
    fn check_but(&self, w: &World, skip: Option<Target>) -> Result<(), String> {
        for (t, files) in &self.files {
            let Some(head) = head_of(w, *t).filter(|_| Some(*t) != skip) else {
                continue;
            };
            for (p, b) in files {
                let got = w.file(&head, p).bytes();
                if got.as_bytes() != &b[..] {
                    return Err(format!(
                        "bytes: {t:?}:{p} holds {got:?}, the oracle {:?}",
                        String::from_utf8_lossy(b)
                    ));
                }
            }
        }
        Ok(())
    }

    fn check(&self, w: &World) -> Result<(), String> {
        if let Some(e) = &self.alarm_fault {
            return Err(e.clone());
        }
        self.check_but(w, None)
    }

    /// What open `h`'s program reads: the oracle's bytes of its file with
    /// its own calls applied.
    fn view(&self, w: &World, h: HandleId) -> String {
        let t = w.handle(h).target;
        let mut b = match self.named(w, h) {
            Some(f) => self.files[&t][&f].clone(),
            // An orphan: what it reads is not checked (its calls are dropped).
            None => Vec::new(),
        };
        for c in self.calls.get(&h).into_iter().flatten() {
            apply(&mut b, c);
        }
        String::from_utf8(b).expect("ASCII")
    }

    fn write(&mut self, w: &mut World, h: HandleId, call: Write) {
        self.calls.entry(h).or_default().push(call.clone());
        w.write(h, call);
    }

    /// Open `h`'s close as the oracle sees it: its calls applied to its
    /// target's bytes, unless its file is unlinked there (as on Linux, its
    /// calls then reach no path). Call before the model closes it.
    /// Its object no path names (an orphan): its calls are dropped, and the
    /// event the model must raise is returned (none if it made no call).
    fn closing(&mut self, w: &World, h: HandleId) -> Option<Dropped> {
        let t = w.handle(h).target;
        let named = self.named(w, h);
        let calls = self.calls.remove(&h).unwrap_or_default();
        let (_, _, path, who) = self.held.remove(&h).expect("an open the oracle saw");
        match named {
            Some(f) => {
                if let Some(b) = self.files.get_mut(&t).and_then(|x| x.get_mut(&f)) {
                    for c in &calls {
                        apply(b, c);
                    }
                }
                None
            }
            None if calls.is_empty() => None,
            None => {
                let bytes = calls
                    .iter()
                    .map(|c| match c {
                        Write::Append(b) | Write::Pwrite { bytes: b, .. } => b.len(),
                        Write::Truncate(_) => 0,
                    })
                    .sum();
                Some((h, who, path, bytes))
            }
        }
    }
}

/// One step ([`step_checked`]), asserting it took its queue's head and left
/// the rest in order, or waits and left the queue as it was; and that only the
/// tree a take finished in changed bytes (the oracle reads them again
/// after).
fn take(w: &mut World, o: &mut Oracle, s: &Step, tally: &mut Tally) -> Result<(), String> {
    w.note_step(s);
    let before = match s {
        Step::Next(t) | Step::Finish(t) => w.queue(*t),
        Step::Drop(_) | Step::Replicate => Vec::new(),
    };
    let numbered = match s {
        Step::Next(t) => w.queue_numbered(*t),
        _ => Vec::new(),
    };
    // The files open for writing in the tree, by the harness's own object
    // table, when the step comes.
    let open_before: BTreeSet<String> = match s {
        Step::Next(t) => w
            .handles()
            .filter(|h| !h.waiting && h.kind == Kind::Write && h.target == Target::Tree(*t))
            .filter_map(|h| o.named(w, h.id))
            .collect(),
        _ => BTreeSet::new(),
    };
    gitledger::note_next(w, s)?;
    // P1: the entry a queue step acts on is the first one no earlier
    // waiting entry shares a file or a source with (the checks' reading).
    let first = match s {
        Step::Next(t) => agentfs_model::check::p1_first(w, *t),
        _ => None,
    };
    // The opens waiting before the step: those it grants raise their
    // alarms then (no P3).
    let waiting: Vec<HandleId> = w.handles().filter(|x| x.waiting).map(|x| x.id).collect();
    let raised = w.delayed.len();
    let taken = step_checked(w, s)?;
    tally.steps += 1;
    for h in waiting {
        if w.handles().any(|x| x.id == h && !x.waiting) {
            alarmed(w, h, &w.delayed[raised..])?;
        }
    }
    match s {
        Step::Finish(t) => {
            o.taking.remove(t);
        }
        Step::Next(t) if w.holds().contains_key(t) => {
            // The files it holds: a landing's read from git (every file its
            // taken changes touch); a sync's are the files it writes.
            let held = match &w.holds()[t].take {
                Take::Landing { .. } => numbered
                    .iter()
                    .skip(first.unwrap_or(0))
                    .take(taken)
                    .flat_map(|(_, e)| entry_files(w, *t, e))
                    .collect(),
                Take::Sync { .. } => w.holds()[t].files.clone(),
            };
            o.taking.insert(*t, held);
        }
        _ => {}
    }
    let skip = match s {
        Step::Finish(t) => Some(Target::Tree(*t)),
        _ => None,
    };
    o.check_but(w, skip)?;
    o.reload(w);
    if let Step::Finish(t) = s {
        o.released(w, *t);
    }
    if let Step::Finish(t) = s
        && w.queue(*t) != before
    {
        return Err(format!("{s:?} changed {t:?}'s own queue"));
    }
    if let Step::Next(t) = s
        && w.alive(*t)
    {
        let after = w.queue(*t);
        if taken == 0 {
            tally.take_waits += 1;
            if let Some(i) = first {
                // P1: a landing waits only for its own change's files (the
                // review of 2026-11-04, P1-c).
                if let (_, Entry::Landing { commit, .. }) = &numbered[i]
                    && change_files(w, commit).is_disjoint(&open_before)
                {
                    return Err(format!(
                        "{s:?}: entry {} waits, but none of its own change's files is open (open: {open_before:?})",
                        numbered[i].0
                    ));
                }
            }
            return if after == before && w.waits().contains_key(t) {
                Ok(())
            } else {
                Err(format!("{s:?} took nothing but does not wait"))
            };
        }
        let i = first.ok_or(format!("{s:?} took {taken} entries, none free to go"))?;
        tally.overtakes += usize::from(i > 0);
        let rest: Vec<_> = before[..i]
            .iter()
            .chain(&before[i + 1..])
            .cloned()
            .collect();
        if taken != 1 || after != rest {
            return Err(format!(
                "{s:?} took {taken} entries: not the first P1 lets go (entry {i}), or the rest out of order"
            ));
        }
    }
    Ok(())
}

/// Closes `h`: the save holds exactly the bytes the oracle computes (none
/// if the file was unlinked meanwhile), and nothing else changed.
fn close_checked(w: &mut World, o: &mut Oracle, h: HandleId) -> Result<(), String> {
    let t = w.handle(h).target;
    let (n, head) = (w.orphans.len(), head_of(w, t));
    let expect = o.closing(w, h);
    w.close(h);
    if expect.is_some() && head_of(w, t) != head {
        return Err(format!("close h{h}: an orphan saved"));
    }
    dropped(w, n, expect.into_iter().collect())?;
    gitledger::observe(w)?;
    o.check(w).map_err(|e| format!("close h{h}: {e}"))
}

/// The model raised exactly the `OrphanDropped` events the oracle expects
/// since event `n` (git-model.md §7: an orphan's calls are dropped, named).
fn dropped(w: &World, n: usize, expect: Vec<Dropped>) -> Result<(), String> {
    let got: Vec<Dropped> = w.orphans[n..]
        .iter()
        .map(|e| (e.handle, e.writer.clone(), e.path.clone(), e.bytes))
        .collect();
    if got != expect {
        return Err(format!(
            "orphans: the model raised {got:?}, the oracle expects {expect:?}"
        ));
    }
    Ok(())
}

/// Every program closes the files it has open (granted ones; a waiting
/// open is closed once a take's release grants it).
fn close_all_checked(w: &mut World, o: &mut Oracle) -> Result<(), String> {
    let open: Vec<_> = w
        .handles()
        .filter(|h| !h.waiting && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    for h in open {
        close_checked(w, o, h)?;
    }
    Ok(())
}

/// A client's end event: the oracle closes its granted opens in the order
/// the model does (by handle), each as its close would, and drops its
/// waiting ones; a sandbox's open transaction rolls back first.
fn end_checked(w: &mut World, o: &mut Oracle, c: &Client) -> Result<(), String> {
    if let Client::Fork(f) = c
        && w.forks[f].txn.is_some()
    {
        rollback_checked(w, o, *f)?;
    }
    let mine: Vec<HandleId> = w
        .handles()
        .filter(|h| h.client == *c && !h.waiting && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    let n = w.orphans.len();
    let mut expect = Vec::new();
    for h in mine {
        expect.extend(o.closing(w, h));
    }
    match c {
        Client::Main(n) => w.end_main_client(n),
        Client::Fork(f) => w.end_client(*f),
    }
    dropped(w, n, expect)?;
    gitledger::observe(w)?;
    o.calls.retain(|h, _| w.handles().any(|x| x.id == *h));
    o.held.retain(|h, _| w.handles().any(|x| x.id == *h));
    o.waiting_calls
        .retain(|h, _| w.handles().any(|x| x.id == *h));
    o.check(w).map_err(|e| format!("end of {c:?}: {e}"))
}

/// A crash of every agentfs process (git-model.md §3), during open
/// `during`'s close if one is given: the oracle saves every granted open's
/// calls as its close would (`during` first, then the rest in the model's
/// order, by handle), so every write call that returned survives; an
/// orphan's event comes once per close the journal replays (twice for an
/// orphan whose close the crash interrupted: events are at least once);
/// waiting opens and path calls are dropped. Takes in progress go back to
/// their queues and change no bytes.
fn crash_checked(w: &mut World, o: &mut Oracle, during: Option<HandleId>) -> Result<(), String> {
    let n = w.orphans.len();
    let mut granted: Vec<HandleId> = w
        .handles()
        .filter(|h| !h.waiting && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    granted.sort();
    let mut expect = Vec::new();
    let first = during.and_then(|h| o.closing(w, h));
    expect.extend(first.clone());
    for h in granted {
        if Some(h) == during {
            // Its close saved nothing git records, so the replay drops it
            // again, in handle order.
            expect.extend(first.clone());
        } else {
            expect.extend(o.closing(w, h));
        }
    }
    // Observed after each save, so the ledger sees one tree move at a time.
    if let Some(h) = during {
        w.close_keeping_journal(h);
        gitledger::observe(w)?;
    }
    for h in w.crash_down() {
        w.restart_open(h);
        gitledger::observe(w)?;
    }
    dropped(w, n, expect)?;
    if let Some(h) = w.handles().next() {
        return Err(format!("h{} outlived the crash", h.id));
    }
    o.waiting_calls.clear();
    o.calls.clear();
    o.held.clear();
    // A crash ends every take (they go back to their queues).
    o.taking.clear();
    gitledger::observe(w)?;
    o.check(w).map_err(|e| format!("crash: {e}"))
}

fn rollback_checked(w: &mut World, o: &mut Oracle, f: ForkId) -> Result<(), String> {
    let n = w.orphans.len();
    w.rollback(f).expect("open");
    o.reload(w);
    dropped(w, n, Vec::new())
}

fn drain(
    w: &mut World,
    o: &mut Oracle,
    tally: &mut Tally,
    at: usize,
) -> Result<(), (usize, String)> {
    for _ in 0..100_000 {
        close_all_checked(w, o).map_err(|e| (at, e))?;
        checkg(w, o).map_err(|e| (at, e))?;
        let Some(s) = w.pending().into_iter().next() else {
            return check_converged(w).map_err(|e| (at, e));
        };
        take(w, o, &s, tally).map_err(|e| (at, e))?;
        checkg(w, o).map_err(|e| (at, e))?;
    }
    Err((at, "the queues never drained".into()))
}

fn checkg(w: &mut World, o: &mut Oracle) -> Result<(), String> {
    check(w)?;
    gitledger::observe(w)?;
    o.no_wait_on_waiting(w)?;
    o.check(w)
}

/// One seed: [`run`] under `catch_unwind`. `Ok` carries the run's
/// [`World::fingerprint`]; `Err` the seed, step, violation and log.
fn run_seed(seed: u64, policies: Policies, tally: &mut Tally) -> Result<String, String> {
    let mut w = World::new(policies);
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&mut w, seed, tally)));
    let (step, e) = match out {
        Ok(Ok(())) => return Ok(w.fingerprint()),
        Ok(Err(e)) => e,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            (usize::MAX, format!("harness panic: {msg}"))
        }
    };
    Err(format!(
        "seed {seed}, step {step}: {e}\nreplay: AGENTFS_MODEL_SEED={seed}\nlog:\n  {}",
        w.log.join("\n  ")
    ))
}

/// A take in progress: its tree, the files its changes touch, and the
/// (file, line identity) pairs in contention there.
type Contention = (Tree, Vec<String>, Vec<(String, String)>);

/// The client calls of a run, with the harness's state across them.
struct Calls {
    rng: Rng,
    o: Oracle,
    /// Each client's last read of each file (by identity), which a later
    /// whole-file rewrite writes back: often stale by then.
    reads: BTreeMap<(String, String), Content>,
    /// Main's live clients: one may die (its end event) and a new one start.
    mains: Vec<String>,
    next_main: usize,
    next_name: usize,
    /// A counter for new lines' bytes, so most new lines differ.
    next_text: usize,
    /// Creates made: each takes one unborn identity.
    creates: usize,
}

impl Calls {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng(seed),
            o: Oracle::default(),
            reads: BTreeMap::new(),
            mains: vec!["m1".into(), "m2".into()],
            next_main: 3,
            next_name: 1,
            next_text: 1,
            creates: 0,
        }
    }

    /// A new line's text: a third repeat other lines' bytes.
    fn text(&mut self, tally: &mut Tally) -> String {
        if self.rng.below(3) == 0 {
            tally.repeated += 1;
            REPEATED[self.rng.below(REPEATED.len())].to_string()
        } else {
            self.next_text += 1;
            format!("n{}", self.next_text)
        }
    }

    fn line_edit(&mut self, w: &mut World, h: HandleId, edit: &LineEdit, text: &str) {
        let view = self.o.view(w, h);
        for c in program::calls(&view, edit, text) {
            self.o.write(w, h, c);
        }
    }

    /// One random byte call, or a line edit's byte calls, on granted open
    /// `h`, at line `slot` where it names one.
    fn write(&mut self, w: &mut World, h: HandleId, slot: usize, tally: &mut Tally) {
        let (c, file, target) = {
            let x = w.handle(h);
            (x.client.clone(), x.file.clone(), x.target)
        };
        let view = self.o.view(w, h);
        let len = view.len();
        match self.rng.below(12) {
            0 => {
                tally.appends += 1;
                tally.concurrent_appends += usize::from(w.handles().any(|o| {
                    o.id != h && o.kind == Kind::Write && o.target == target && o.file == file
                }));
                let t = self.text(tally);
                self.line_edit(w, h, &LineEdit::Append, &t);
            }
            1 => {
                let t = self.text(tally);
                self.line_edit(w, h, &LineEdit::At(slot), &t);
            }
            2 => {
                tally.inserts += 1;
                let t = self.text(tally);
                self.line_edit(w, h, &LineEdit::Insert(slot), &t);
            }
            3 => {
                tally.deletes += 1;
                self.line_edit(w, h, &LineEdit::Delete(slot), "");
            }
            4 => {
                tally.moves += 1;
                let to = self.rng.below(HOT_SLOTS + 2);
                self.line_edit(w, h, &LineEdit::Move { from: slot, to }, "");
            }
            5 => {
                let read = self.reads.get(&(format!("{c:?}"), file.clone())).cloned();
                let Some(read) =
                    read.or_else(|| w.client_path(&c, &file).map(|p| w.client_reads(&c, &p)))
                else {
                    return;
                };
                tally.rewrites += 1;
                self.line_edit(w, h, &LineEdit::Rewrite(read), "");
            }
            6 => {
                // An append with no final newline: the next append, or this
                // file's next writer's, finishes its line.
                tally.partial_appends += 1;
                let mut t = self.text(tally);
                if self.rng.below(2) == 0 {
                    t = format!("{t}\n{}", self.rng.raw(2).replace('\n', ""));
                }
                self.o.write(w, h, Write::Append(t));
            }
            7 => {
                tally.pwrites += 1;
                let offset = self.rng.below(len + 3);
                tally.holes += usize::from(offset > len);
                let bytes = self.rng.raw(4);
                self.o.write(w, h, Write::Pwrite { offset, bytes });
            }
            8 => {
                // A line edit's bytes in two calls, split at a byte inside.
                tally.split_writes += 1;
                let t = self.text(tally);
                let edit = if self.rng.below(2) == 0 {
                    LineEdit::At(slot)
                } else {
                    LineEdit::Insert(slot)
                };
                for c in program::calls(&view, &edit, &t) {
                    match c {
                        Write::Pwrite { offset, bytes } if bytes.len() > 1 => {
                            let k = 1 + self.rng.below(bytes.len() - 1);
                            let (a, b) = bytes.split_at(k);
                            self.o.write(
                                w,
                                h,
                                Write::Pwrite {
                                    offset,
                                    bytes: a.into(),
                                },
                            );
                            let bytes = b.to_string();
                            self.o.write(
                                w,
                                h,
                                Write::Pwrite {
                                    offset: offset + k,
                                    bytes,
                                },
                            );
                        }
                        c => self.o.write(w, h, c),
                    }
                }
            }
            9 => {
                // Two writes, the second over part of the first.
                tally.overlapping += 1;
                let offset = self.rng.below(len + 1);
                let t = self.text(tally);
                let first = format!("{t}\n");
                let k = self.rng.below(first.len());
                self.o.write(
                    w,
                    h,
                    Write::Pwrite {
                        offset,
                        bytes: first,
                    },
                );
                let bytes = self.rng.raw(3);
                self.o.write(
                    w,
                    h,
                    Write::Pwrite {
                        offset: offset + k,
                        bytes,
                    },
                );
            }
            10 => {
                tally.truncates += 1;
                let n = self.rng.below(len + 2);
                tally.holes += usize::from(n > len);
                self.o.write(w, h, Write::Truncate(n));
            }
            _ => {
                // A line written in place, its bytes repeating another's.
                tally.repeated += 1;
                let t = REPEATED[self.rng.below(REPEATED.len())];
                self.line_edit(w, h, &LineEdit::At(slot), t);
            }
        }
    }

    /// Opens `p` for writing as `c`, a sixth with `O_TRUNC`.
    fn open(&mut self, w: &mut World, c: &Client, p: &str, tally: &mut Tally) -> HandleId {
        let h = if self.rng.below(6) == 0 {
            tally.o_truncs += 1;
            let h = w.open_trunc(c, p);
            self.o.calls.insert(h, vec![Write::Truncate(0)]);
            h
        } else {
            w.open_write(c, p)
        };
        self.o.opened(w, h);
        h
    }

    /// Opens `p` for writing as `c` and, if granted, writes line `slot` and
    /// closes: a save. A waiting open stays, to be written once granted.
    fn save(
        &mut self,
        w: &mut World,
        c: &Client,
        p: &str,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let h = w.open_write(c, p);
        self.o.opened(w, h);
        if w.handle(h).waiting {
            tally.open_waits += 1;
            return Ok(());
        }
        let t = self.text(tally);
        self.line_edit(w, h, &LineEdit::At(slot), &t);
        close_checked(w, &mut self.o, h)
    }

    /// Unlinks `p`, or renames it: to a new path, or a third of the time
    /// over another file the client sees.
    fn path_call(
        &mut self,
        w: &mut World,
        c: &Client,
        f: &str,
        p: &str,
        unlink: bool,
        tally: &mut Tally,
    ) -> Result<(), String> {
        if unlink {
            tally.unlinks += 1;
            return self.o.path_call(w, c, p, None);
        }
        let over = PATHS
            .iter()
            .filter(|g| **g != f)
            .filter_map(|g| w.client_path(c, g))
            .collect::<Vec<_>>();
        match self.rng.pick(&over) {
            Some(q) if self.rng.below(3) == 0 => {
                tally.rename_overs += 1;
                self.o.path_call(w, c, p, Some(&q))
            }
            _ => {
                tally.renames += 1;
                let to = format!("e{}", self.next_name);
                self.next_name += 1;
                self.o.path_call(w, c, p, Some(&to))
            }
        }
    }

    /// A new file, or an editor's atomic save of `p` (git-model.md §7), as
    /// `c`: create a temporary file, write it, close it; for an atomic
    /// save the bytes are `p`'s with a line written at `slot`, and the
    /// temporary file is renamed over `p`. Half the time an old writer of
    /// `p` opens it first and stays open, to write and close later (its
    /// object no path names once the save runs). Each create takes an
    /// unborn identity; none left: nothing.
    fn create(
        &mut self,
        w: &mut World,
        c: &Client,
        p: &str,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<(), String> {
        if self.creates >= UNBORN.len() {
            return Ok(());
        }
        let atomic = self.rng.below(3) != 0;
        if atomic && self.rng.below(2) == 0 {
            let h = self.open(w, c, p, tally);
            tally.old_writers += 1;
            if !w.handle(h).waiting {
                self.write(w, h, slot, tally);
            }
        }
        let tmp = format!("t{}", self.next_name);
        self.next_name += 1;
        let Some(h) = w.create(c, &tmp) else {
            return Ok(());
        };
        self.creates += 1;
        tally.creates += 1;
        self.o.opened(w, h);
        // The create's own save (the new file's name) leaves no bytes.
        self.o.check(w)?;
        let text = self.text(tally);
        let bytes = if atomic {
            let file = w.lookup(&w.target_head(w.handle(h).target), p);
            let old = file
                .and_then(|f| {
                    self.o
                        .files
                        .get(&w.handle(h).target)
                        .and_then(|x| x.get(&f))
                        .cloned()
                })
                .unwrap_or_default();
            program::edited(
                &String::from_utf8(old).expect("ASCII"),
                &LineEdit::At(slot),
                &text,
            )
        } else {
            format!("{text}\n")
        };
        self.o.write(w, h, Write::Append(bytes));
        close_checked(w, &mut self.o, h)?;
        if atomic {
            tally.atomic_saves += 1;
            self.o.path_call(w, c, &tmp, Some(p))?;
        }
        Ok(())
    }

    /// While a take holds files in a tree, a client of that tree calls on
    /// one of them (or another file): opens it (it waits if held), saves
    /// it at once if granted, or unlinks or renames it. This is the
    /// pattern the 2026-11-03 review found by hand and 500 seeds missed (a
    /// parent's save of a file a losing change touched, during the take).
    fn during_take(
        &mut self,
        w: &mut World,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<bool, String> {
        // Each take with the files its changes touch and the lines in
        // contention there, read from git: the lines the commit of the
        // take set over its first parent, and those the tree's head set
        // over that first parent (the commit against its base).
        let holds: Vec<Contention> = w
            .holds()
            .iter()
            .map(|(t, x)| {
                let mut files: Vec<String> = x.files.iter().cloned().collect();
                let mut lines = Vec::new();
                if let Take::Landing { commit: c, .. } = &x.take {
                    let base = git(w.repo(), &["rev-parse", &format!("{c}^1")]);
                    for e in w
                        .edits(&base, c)
                        .into_iter()
                        .chain(w.edits(&base, &w.head(*t)))
                    {
                        files.push(e.path.clone());
                        lines.push((e.path, e.line));
                    }
                }
                (*t, files, lines)
            })
            .collect();
        // And each entry waiting for its files (a new call on one of them
        // is granted and postpones it, raising an alarm). Steering only.
        let mut holds = holds;
        for (t, m) in w.waits() {
            for x in m.values() {
                holds.push((*t, x.files.iter().cloned().collect(), Vec::new()));
            }
        }
        let Some((t, held, contended)) = self.rng.pick(&holds) else {
            return Ok(false);
        };
        let c = match t {
            Tree::Main => Client::Main(self.mains[self.rng.below(self.mains.len())].clone()),
            Tree::Fork(f) if w.forks[&f].life == Life::Open => Client::Fork(f),
            Tree::Fork(_) => return Ok(false),
        };
        if blocked(w, &c) {
            return Ok(false);
        }
        let file = match self.rng.pick(&held) {
            Some(x) if self.rng.below(4) != 0 => x,
            _ => FILES[self.rng.below(FILES.len())].to_string(),
        };
        let Some(p) = w.client_path(&c, &file) else {
            return Ok(false);
        };
        tally.calls_during_takes += 1;
        match self.rng.below(6) {
            0 => {
                let unlink = self.rng.below(2) == 0;
                self.path_call(w, &c, &file, &p, unlink, tally)?;
            }
            1 | 2 => {
                let h = self.open(w, &c, &p, tally);
                tally.open_waits += usize::from(w.handle(h).waiting);
            }
            _ => {
                let h = w.open_write(&c, &p);
                self.o.opened(w, h);
                if w.handle(h).waiting {
                    tally.open_waits += 1;
                } else {
                    // Mostly on a line in contention: removed (a save that
                    // can turn a losing change of this file into one that
                    // wins at the take's replan) or written.
                    tally.saves_during_takes += 1;
                    let at = w.client_reads(&c, &p);
                    let spots: Vec<usize> = at
                        .live()
                        .enumerate()
                        .filter(|(_, l)| contended.contains(&(file.clone(), l.line.clone())))
                        .map(|(i, _)| i)
                        .collect();
                    let spot = self.rng.pick(&spots).unwrap_or(slot);
                    if self.rng.below(2) == 0 {
                        self.line_edit(w, h, &LineEdit::Delete(spot), "");
                    } else {
                        self.write(w, h, spot, tally);
                    }
                    close_checked(w, &mut self.o, h)?;
                }
            }
        }
        Ok(true)
    }
}

fn run(w: &mut World, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    gitledger::enable(true);
    gitledger::observe(w).map_err(|e| (0, e))?;
    let mut k = Calls::new(seed);
    k.o.reload(w);
    // The line a fork wrote last (its file's identity and position). Main
    // and forks write it again half the time, so runs often build "a fork
    // edits over its own undecided line while main writes it
    // concurrently" (coherence finding 8).
    let mut focus: Option<(&str, usize)> = None;
    for step in 0..STEPS {
        let (f, slot) = match focus {
            Some(x) if k.rng.below(2) == 0 => x,
            _ => (FILES[k.rng.below(FILES.len())], k.rng.below(HOT_SLOTS)),
        };
        // Only a client not blocked in a waiting call makes one.
        let mut clients: Vec<Client> = k.mains.iter().cloned().map(Client::Main).collect();
        clients.extend(live_clients(w).into_iter().map(Client::Fork));
        clients.retain(|c| !blocked(w, c));
        let open: Vec<_> = w
            .handles()
            .filter(|h| !h.waiting && h.kind == Kind::Write && !blocked(w, &h.client))
            .map(|h| h.id)
            .collect();
        let e = |e| (step, e);
        match k.rng.below(34) {
            0..=2 => {
                let who = Client::Main(k.mains[k.rng.below(k.mains.len())].clone());
                if !blocked(w, &who)
                    && let Some(p) = w.client_path(&who, f)
                {
                    k.save(w, &who, &p, slot, tally).map_err(e)?;
                }
            }
            3 => {
                let forks = live_clients(w);
                if forks.len() < 4 {
                    let parent = match k.rng.pick(&forks) {
                        Some(f) if k.rng.below(3) == 0 => {
                            tally.nested += 1;
                            Tree::Fork(f)
                        }
                        _ => Tree::Main,
                    };
                    w.create_fork(parent);
                    k.o.reload(w);
                    tally.forks += 1;
                }
            }
            4..=6 => {
                if let Some(c) = k.rng.pick(&free_clients(w)).map(Client::Fork)
                    && let Some(p) = w.client_path(&c, f)
                {
                    k.save(w, &c, &p, slot, tally).map_err(e)?;
                    focus = Some((f, slot));
                }
            }
            7 => {
                if let Some(f) = k.rng.pick(&free_clients(w)) {
                    let _ = w.begin(f);
                    k.o.reload(w);
                }
            }
            8 => {
                // Mostly commit a transaction that staged something, so
                // commits meet concurrent lines.
                if let Some(f) = k.rng.pick(&in_txn(w))
                    && !blocked(w, &Client::Fork(f))
                    && (w.forks[&f]
                        .txn
                        .as_ref()
                        .is_some_and(|t| !t.staged.is_empty())
                        || k.rng.below(4) == 0)
                {
                    let _ = w.commit_txn(f);
                    gitledger::observe(w).map_err(e)?;
                    k.o.reload(w);
                }
            }
            9 => {
                if let Some(f) = k.rng.pick(&in_txn(w))
                    && !blocked(w, &Client::Fork(f))
                    && k.rng.below(2) == 0
                {
                    rollback_checked(w, &mut k.o, f).map_err(e)?;
                    tally.rolled_back += 1;
                }
            }
            10 => {
                if k.rng.below(4) == 0 {
                    // One of main's clients dies; a new one starts.
                    let i = k.rng.below(k.mains.len());
                    end_checked(w, &mut k.o, &Client::Main(k.mains[i].clone())).map_err(e)?;
                    tally.main_deaths += 1;
                    k.mains[i] = format!("m{}", k.next_main);
                    k.next_main += 1;
                } else if let Some(f) = k.rng.pick(&live_clients(w))
                    && k.rng.below(3) == 0
                {
                    end_checked(w, &mut k.o, &Client::Fork(f)).map_err(e)?;
                }
            }
            11 | 12 => {
                if let Some(c) = k.rng.pick(&clients)
                    && let Some(p) = w.client_path(&c, f)
                {
                    let h = k.open(w, &c, &p, tally);
                    tally.open_waits += usize::from(w.handle(h).waiting);
                }
            }
            13..=15 => {
                if let Some(h) = k.rng.pick(&open) {
                    k.write(w, h, slot, tally);
                    // A quarter of the writes in a transaction meet a crash
                    // before their close (rule 12 inside a transaction).
                    if matches!(w.handle(h).target, Target::Txn(..)) && k.rng.below(4) == 0 {
                        tally.crashes += 1;
                        tally.crashes_in_txn += 1;
                        crash_checked(w, &mut k.o, None).map_err(e)?;
                    }
                }
            }
            16 => {
                if let Some(h) = k.rng.pick(&open) {
                    close_checked(w, &mut k.o, h).map_err(e)?;
                }
            }
            17 => {
                if let Some(c) = k.rng.pick(&clients)
                    && let Some(p) = w.client_path(&c, f)
                {
                    k.reads
                        .insert((format!("{c:?}"), f.to_string()), w.client_reads(&c, &p));
                }
            }
            18 | 19 => {
                if k.rng.below(3) == 0
                    && let Some(c) = k.rng.pick(&clients)
                    && let Some(p) = w.client_path(&c, f)
                {
                    let unlink = k.rng.below(2) == 0;
                    k.path_call(w, &c, f, &p, unlink, tally).map_err(e)?;
                }
            }
            21 if k.rng.below(6) == 0 || (txn_open(w) && k.rng.below(2) == 0) => {
                // Every agentfs process dies; half the time during a close.
                // More often while a transaction has a file open, so a
                // crash meets an open inside one (rule 12).
                tally.crashes += 1;
                tally.crashes_in_txn += usize::from(txn_open(w));
                let during = k.rng.pick(&open).filter(|_| k.rng.below(2) == 0);
                tally.crashes_during_close += usize::from(during.is_some());
                crash_checked(w, &mut k.o, during).map_err(e)?;
            }
            20 if k.rng.below(2) == 0 => {
                if let Some(c) = k.rng.pick(&clients)
                    && let Some(p) = w.client_path(&c, f)
                {
                    k.create(w, &c, &p, slot, tally).map_err(e)?;
                }
            }
            26..=29 if !w.waits().is_empty() && k.rng.below(2) == 0 => {
                // An entry waits: a sandbox of that tree saves a file the
                // entry does not lock, then a queue step there, so a later,
                // unrelated entry goes ahead of the waiting one (P1).
                let waiting: Vec<(Tree, Vec<String>)> = w
                    .waits()
                    .iter()
                    .map(|(t, m)| {
                        (
                            *t,
                            m.values().flat_map(|x| x.files.iter().cloned()).collect(),
                        )
                    })
                    .collect();
                if let Some((t, locked)) = k.rng.pick(&waiting) {
                    let free: Vec<&str> = FILES
                        .iter()
                        .copied()
                        .filter(|f| !locked.iter().any(|l| l == f))
                        .collect();
                    let kids: Vec<ForkId> = free_clients(w)
                        .into_iter()
                        .filter(|f| w.forks[f].parent == t && w.forks[f].txn.is_none())
                        .collect();
                    if let (Some(c), Some(g)) = (k.rng.pick(&kids), k.rng.pick(&free))
                        && let Some(p) = w.client_path(&Client::Fork(c), g)
                    {
                        k.save(w, &Client::Fork(c), &p, slot, tally).map_err(e)?;
                    }
                    if w.pending().contains(&Step::Next(t)) {
                        take(w, &mut k.o, &Step::Next(t), tally).map_err(e)?;
                    }
                }
            }
            26..=33 if !w.holds().is_empty() || !w.waits().is_empty() => {
                k.during_take(w, slot, tally).map_err(e)?;
            }
            _ => {
                if let Some(s) = k.rng.pick(&w.pending()) {
                    take(w, &mut k.o, &s, tally).map_err(e)?;
                }
            }
        }
        checkg(w, &mut k.o).map_err(e)?;
    }
    // Writes stop: every open file closes, every open transaction commits
    // or rolls back; the queues drain; every fork equals main.
    close_all_checked(w, &mut k.o).map_err(|e| (STEPS, e))?;
    for f in in_txn(w) {
        if k.rng.below(2) == 0 {
            // A commit first finishes the sandbox's take, which may grant
            // an open of a file the commit writes: close it too.
            if w.commit_txn(f) != Err(ModelError::OpenFiles) {
                gitledger::observe(w).map_err(|e| (STEPS, e))?;
                k.o.reload(w);
                continue;
            }
            k.o.reload(w);
            close_all_checked(w, &mut k.o).map_err(|e| (STEPS, e))?;
            w.commit_txn(f).expect("open");
            gitledger::observe(w).map_err(|e| (STEPS, e))?;
            k.o.reload(w);
        } else {
            rollback_checked(w, &mut k.o, f).map_err(|e| (STEPS, e))?;
            tally.rolled_back += 1;
        }
    }
    checkg(w, &mut k.o).map_err(|e| (STEPS, e))?;
    drain(w, &mut k.o, tally, STEPS)?;
    // Every program exits: every fork drops.
    for f in live_clients(w) {
        end_checked(w, &mut k.o, &Client::Fork(f)).map_err(|e| (STEPS + 1, e))?;
    }
    drain(w, &mut k.o, tally, STEPS + 1)?;
    if let Some(f) = w.forks.values().find(|f| f.life != Life::Dropped) {
        return Err((STEPS + 1, format!("f{} never dropped", f.id)));
    }
    for l in &w.landings {
        tally.changes_lost += usize::from(!l.won);
    }
    tally.clean_lines_lost += w.collisions.iter().filter(|c| c.by == By::Whole).count();
    tally.fast_forwards += w.fast_forwards;
    tally.inserts_met += w.inserts_met;
    tally.unlinked_edits += w.unlinked_edits;
    tally.orphans += w.orphans.len();
    tally.prefix_syncs += w.prefix_syncs;
    tally.delayed += w.delayed.len();
    if gitledger::takes() != w.landings.len() {
        return Err((
            STEPS + 1,
            format!(
                "ledger: git showed {} takes, the model recorded {}",
                gitledger::takes(),
                w.landings.len()
            ),
        ));
    }
    tally.git_takes = gitledger::takes();
    tally.git_takes_total += gitledger::takes();
    for c in &w.collisions {
        match c.direction {
            Direction::Landing => tally.landing_collisions += 1,
            Direction::Transaction => tally.transaction_collisions += 1,
        }
    }
    for e in &w.ended {
        match &e.how {
            TxnEnd::Commit(TxnOutcome::Committed) => tally.committed += 1,
            TxnEnd::Commit(TxnOutcome::Lost { lines }) => {
                tally.lost += 1;
                tally.not_applied += lines.len();
            }
            TxnEnd::Commit(TxnOutcome::Empty) => tally.empty += 1,
            TxnEnd::Rollback => {}
        }
    }
    Ok(())
}

fn run_all(policies: Policies) {
    let mut tally = Tally::default();
    let mut failures = Vec::new();
    for seed in seeds() {
        if let Err(e) = run_seed(seed, policies, &mut tally) {
            failures.push(e);
        }
    }
    eprintln!("{policies:?}, {} seeds: {tally:?}", seeds().len());
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds().len(),
        failures.join("\n\n")
    );
    // Across the CI seeds (or more) the rules really met: changes lost
    // whole with clean lines in them (rule 3e), and rules 10 and 11 (below).
    // One seed alone may have none.
    if seeds().len() as u64 >= SEEDS {
        assert!(
            tally.changes_lost > 0 && tally.clean_lines_lost > 0,
            "no change lost whole with a clean line in {} seeds",
            seeds().len()
        );
        // Rules 10 and 11 met in the random runs, not in one scenario
        // each: a new line placed beside a parent's concurrent new line,
        // an edit of a file the parent had unlinked; and saves made while
        // a take held files in their tree.
        assert!(
            tally.inserts_met > 0 && tally.unlinked_edits > 0 && tally.saves_during_takes > 0,
            "rules 10 and 11, or saves during takes, not met in {} seeds: {tally:?}",
            seeds().len()
        );
        // Atomic saves, and old writers whose calls an atomic save, an
        // unlink or a rename-over orphaned (git-model.md §7).
        // P1 and P2 met, and the alarm: an entry went ahead of a waiting
        // one, a sync delivered a prefix and held the rest back, and an
        // open postponed a waiting take.
        assert!(
            tally.overtakes > 0 && tally.prefix_syncs > 0 && tally.delayed > 0,
            "progress rules not met in {} seeds: {tally:?}",
            seeds().len()
        );
        assert!(
            tally.atomic_saves > 0
                && tally.orphans > 0
                && tally.crashes_during_close > 0
                && tally.crashes_in_txn > 0,
            "no atomic save, no orphan, no crash during a close or none inside a transaction, in {} seeds: {tally:?}",
            seeds().len()
        );
    }
}

/// The model's choices: per line, per-line syncs. Every invariant.
#[test]
fn random_interleavings_full_lineage() {
    run_all(Policies::default());
}

/// The per-line rule does not depend on slots being separate git hunks:
/// the same runs with every slot on a neighbouring line.
#[test]
fn random_interleavings_adjacent_lines() {
    run_all(Policies {
        layout: Layout::Adjacent,
        ..Policies::default()
    });
}

/// Rule 2, "no clock is read anywhere", on the random runs: seeds 1..=8 run
/// again with every commit dated before its parents decide exactly the same
/// trees and collisions ([`World::fingerprint`]). An order taken from commit
/// dates would differ.
#[test]
fn random_interleavings_commit_dates_decide_nothing() {
    let policies = Policies::default();
    for seed in seeds().into_iter().take(8) {
        let mut tally = Tally::default();
        let pinned = run_seed(seed, policies, &mut tally);
        // Each run must pass on its own first: a failure names commit
        // hashes, which reversed dates change, so comparing two failures
        // would report a broken invariant as a date mismatch.
        let pinned = pinned.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let reversed = with_reversed_dates(|| run_seed(seed, policies, &mut tally))
            .unwrap_or_else(|e| panic!("seed {seed}, dates reversed: {e}"));
        assert_eq!(pinned, reversed, "seed {seed}");
    }
}

/// Coherence finding 1: git's own merge base cannot tell a fork line the
/// parent already decided against from one still on its way.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings keeps a fork line the parent already decided against"]
fn random_interleavings_git_merge_base() {
    run_all(Policies {
        merge: MergeRule::GitMergeBase,
        ..Policies::default()
    });
}
