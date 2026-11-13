//! Seeded random interleavings of client calls and queue steps across
//! the root, several forks (nested ones included) and transactions, with every
//! invariant checked after each step (`agentfs_model::check`), and, once
//! writes stop and the queues drain, convergence.
//!
//! Clients open files (some with `O_TRUNC`) and make byte calls, as POSIX
//! gives them (rule 8): `O_APPEND` writes, some of a partial line;
//! `pwrite`s at an offset, some past the end (a hole), some split in two,
//! some overlapping; `ftruncate`s; and the byte calls a program makes for a
//! line edit (a line written in place, inserted, deleted or moved, some with
//! bytes other lines repeat) or a whole-file rewrite of an earlier read.
//! They close them, so takes wait for write-opens (rule 9), and while an
//! entry waits they often open, save, unlink or rename one of its files; a
//! take is one step (the owner, 2026-11-08, finding 49: "B"), so no call runs
//! during one. They unlink and rename files (rule 11), to a new path or over
//! another file. On top of `check`, the run
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

use agentfs_model::calls::{Client, HandleId, TreeId};
use agentfs_model::check::{Checked, change_files};
use agentfs_model::content::{BLANK, Content, FILES, Layout, PATHS, UNBORN};
use agentfs_model::git::{git, with_reversed_dates};
use agentfs_model::program;
use agentfs_model::queue::{Entry, NAME_KEY, name_key};
use agentfs_model::recording::{Kind, Target, Write};
use agentfs_model::service::{HistoryService, Step};
use agentfs_model::world::{
    By, Direction, Life, LineEdit, MergeRule, Policies, TreeNames, TxnEnd, TxnOutcome, World,
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
    /// R5.3: closes and path calls deferred behind a transaction, and
    /// commits that went ahead with a file open in their fork.
    deferred_closes: usize,
    deferred_path_calls: usize,
    commits_with_open: usize,
    forks: usize,
    nested: usize,
    /// Queue heads that waited for a write-open.
    take_waits: usize,
    /// Opens, unlinks and renames made on a waiting entry's files in that
    /// tree, and saves closed then.
    calls_while_waiting: usize,
    saves_while_waiting: usize,
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
    root_deaths: usize,
    /// Creates, atomic saves (a create renamed over a file), old writers
    /// left open on a file an atomic save may replace, and closes of an
    /// orphan that dropped calls.
    creates: usize,
    atomic_saves: usize,
    old_writers: usize,
    orphans: usize,
    /// Progress (git-model.md §6): queue steps that took an entry ahead of
    /// an earlier waiting one (P1); syncs that delivered a prefix and held
    /// the rest back (P2).
    overtakes: usize,
    prefix_syncs: usize,
    /// Crashes of every agentfs process, and those during a close.
    crashes: usize,
    crashes_during_close: usize,
    /// Crashes while a transaction had a file open.
    crashes_in_txn: usize,
    /// Takes in which a new line met an upstream's new line at its place (rule
    /// 10), and changes that edited a file the upstream had unlinked (rule
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

fn live_clients(w: &World) -> Vec<TreeId> {
    w.forks
        .values()
        .filter(|f| f.life == Life::Open)
        .map(|f| f.id)
        .collect()
}

/// Whether an open writes in a transaction.
fn txn_open(w: &World) -> bool {
    w.handles()
        .any(|h| h.kind == Kind::Write && matches!(h.target, Target::Txn(..)))
}

fn in_txn(w: &World) -> Vec<TreeId> {
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
    /// R5.3, the harness's own list: the calls each fork's open transaction
    /// has deferred, in the order they were made.
    deferred: BTreeMap<TreeId, Vec<Deferred>>,
}

/// A call deferred behind a transaction: a close, or a path call (its handle,
/// and a rename's path).
#[derive(Clone, Debug)]
enum Deferred {
    Close(HandleId),
    Path(HandleId, Option<String>),
}

impl Deferred {
    fn id(&self) -> HandleId {
        match self {
            Deferred::Close(h) | Deferred::Path(h, _) => *h,
        }
    }
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
        Client::Root(n) => n.clone(),
        Client::Fork(f) | Client::Outside(f) => w.forks[f].writer.clone(),
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
            .map(|x| x.index.clone()),
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

    /// A new open: it holds its file's object in its target from now on.
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
    /// call when it runs, at once or after the transaction that defers it.
    fn path_call(
        &mut self,
        w: &mut World,
        c: &Client,
        p: &str,
        to: Option<&str>,
    ) -> Result<(), String> {
        let t = match c {
            Client::Root(_) => Target::Tree(TreeId::ROOT),
            Client::Fork(f) => match &w.forks[f].txn {
                Some(x) => Target::Txn(*f, x.id),
                None => Target::Tree(*f),
            },
            Client::Outside(f) => Target::Tree(*f),
        };
        let at = head_of(w, t).expect("live");
        let file = w.lookup(&at, p).expect("a file");
        let mut files = vec![file.clone()];
        files.extend(to.and_then(|to| w.lookup(&at, to)));
        let defers = self.defers(w, t, &files);
        match to {
            None => w.unlink(c, p),
            Some(to) => w.rename(c, p, to),
        }
        match defers {
            Some(f) => {
                let id = *w.deferred(f).last().ok_or_else(|| {
                    format!("R5.3: a path call on {files:?} ran during its transaction")
                })?;
                self.deferred
                    .entry(f)
                    .or_default()
                    .push(Deferred::Path(id, to.map(str::to_string)));
            }
            None => self.ran(w, &at, t, &file, to),
        }
        Ok(())
    }

    /// R5.3, the harness's own reading: target `t` is a fork's tree, and
    /// one of `files` is a file of the fork's open transaction: one it has
    /// saved (read from git), has open for writing, or has deferred a call on.
    /// The fork, if so.
    fn defers(&self, w: &World, t: Target, files: &[String]) -> Option<TreeId> {
        let Target::Tree(f) = t else {
            return None;
        };
        w.upstream_of(f)?;
        let txn = w.forks[&f].txn.as_ref()?;
        // Begin read from its ref; the index is the tree whose bytes
        // `check_but` holds to the oracle's at every step.
        let begin = git(w.repo(), &["rev-parse", &TreeNames::txn_branch(txn.id)]);
        let saved = git(
            w.repo(),
            &["diff-tree", "-r", "--name-only", &begin, &txn.index],
        );
        let mut mine: BTreeSet<String> = saved.lines().map(str::to_string).collect();
        for (h, (u, _, _, _)) in &self.held {
            if *u == Target::Txn(f, txn.id) {
                mine.extend(self.named(w, *h));
            }
        }
        for d in self.deferred.get(&f).into_iter().flatten() {
            match d {
                Deferred::Close(h) => mine.extend(self.named(w, *h)),
                Deferred::Path(h, _) => mine.extend(w.handle(*h).files()),
            }
        }
        files.iter().any(|x| mine.contains(x)).then_some(f)
    }

    /// R5.3: whether open `h`'s close defers, as the harness reads it.
    fn close_defers(&self, w: &World, h: HandleId) -> Option<TreeId> {
        let file = self.named(w, h)?;
        self.defers(w, w.handle(h).target, &[file])
    }

    /// A commit left transaction open `h` open: it now holds in the fork
    /// the object its file `file` has there; with none, an orphan.
    fn move_to_fork(&mut self, h: HandleId, f: TreeId, file: Option<String>) {
        let t = Target::Tree(f);
        let o = match file {
            Some(x) => match self.inodes.get(&(t, x.clone())) {
                Some(o) => *o,
                None => {
                    self.next_inode += 1;
                    self.inodes.insert((t, x), self.next_inode);
                    self.next_inode
                }
            },
            None => {
                self.next_inode += 1;
                self.next_inode
            }
        };
        let held = self.held.get_mut(&h).expect("an open the oracle saw");
        held.0 = t;
        held.1 = o;
    }

    /// No program's call ever waits (the owner, 2026-11-04, R9.2: none on a
    /// waiting landing; 2026-11-08, finding 49: a take is one step, so none
    /// on a take either), judged by the harness's record alone: every handle
    /// the model keeps is an open the oracle saw made, or a path call the
    /// oracle saw a transaction defer (R5.3).
    fn no_call_waits(&self, w: &World) -> Result<(), String> {
        for x in w.handles() {
            let deferred = self
                .deferred
                .values()
                .flatten()
                .any(|d| matches!(d, Deferred::Path(h, _) if *h == x.id));
            let open = x.kind == Kind::Write && self.held.contains_key(&x.id);
            if !open && !deferred {
                return Err(format!(
                    "oracle_no_call_waits: h{} ({:?} of {}) did not return: a call waits",
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
                        "oracle_bytes: {t:?}:{p} holds {got:?}, the oracle {:?}",
                        String::from_utf8_lossy(b)
                    ));
                }
            }
        }
        Ok(())
    }

    fn check(&self, w: &World) -> Result<(), String> {
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

/// One step ([`Checked::step_checked`]), asserting it took its queue's head and left
/// the rest in order, or waits and left the queue as it was; and that only the
/// tree a take wrote in changed bytes (the oracle reads them again after).
fn take(w: &mut Checked, o: &mut Oracle, s: &Step, tally: &mut Tally) -> Result<(), String> {
    w.note_step(s);
    let before = match s {
        Step::Next(t) => w.queue(*t),
        Step::Drop(_) | Step::Push => Vec::new(),
    };
    let numbered = match s {
        Step::Next(t) => w.queue_numbered(*t),
        _ => Vec::new(),
    };
    // The files open for writing in the tree and in its open transaction
    // (R9.18), by the harness's own object table, each with its name read in
    // its own target, when the step comes.
    let r: &World = w;
    let open_before: BTreeSet<String> = match s {
        Step::Next(t) => r
            .handles()
            .filter(|h| matches!(h.target, Target::Tree(u) | Target::Txn(u, _) if u == *t))
            .flat_map(|h| {
                let at = r.target_head(h.target);
                let mut files: Vec<String> = o.named(r, h.id).into_iter().collect();
                if h.kind != Kind::Write {
                    files.extend(h.files());
                }
                files
                    .into_iter()
                    .flat_map(move |f| [name_key(&r.file(&at, &f)), Some(f)])
            })
            .flatten()
            .collect(),
        _ => BTreeSet::new(),
    };
    let head_before = match s {
        Step::Next(t) => Some(w.head(*t)),
        _ => None,
    };
    gitledger::note_next(w, s)?;
    // P1: the entry a queue step acts on is the first one no earlier
    // waiting entry shares a file or a source with (the checks' reading).
    let first = match s {
        Step::Next(t) => agentfs_model::check::p1_first(w, *t),
        _ => None,
    };
    let taken = w.step_checked(s)?;
    tally.steps += 1;
    // Rule 9 and R9.18, by the harness's own reading of the opens: a take
    // writes no file open for writing in its tree or in that tree's open
    // transaction (read from git, not from the model's locks).
    if let (Step::Next(t), Some(before)) = (s, &head_before)
        && *before != w.head(*t)
    {
        let wrote = git(
            w.repo(),
            &["diff-tree", "-r", "--name-only", before, &w.head(*t)],
        );
        if let Some(p) = wrote.lines().find(|p| open_before.contains(*p)) {
            return Err(format!(
                "oracle_locks: {s:?} wrote {p}, which is open for writing in {t:?} or its transaction"
            ));
        }
    }
    let skip = match s {
        Step::Next(t) => Some(Target::Tree(*t)),
        _ => None,
    };
    o.check_but(w, skip)?;
    o.reload(w);
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
                        "oracle_p1: {s:?}: entry {} waits, but none of its own change's files is open (open: {open_before:?})",
                        numbered[i].0
                    ));
                }
            }
            return if after == before && w.waits().contains_key(t) {
                Ok(())
            } else {
                Err(format!("oracle_p1: {s:?} took nothing but does not wait"))
            };
        }
        let i = first.ok_or(format!(
            "oracle_p1: {s:?} took {taken} entries, none free to go"
        ))?;
        tally.overtakes += usize::from(i > 0);
        let rest: Vec<_> = before[..i]
            .iter()
            .chain(&before[i + 1..])
            .cloned()
            .collect();
        if taken != 1 || after != rest {
            return Err(format!(
                "oracle_p1: {s:?} took {taken} entries: not the first P1 lets go (entry {i}), or the rest out of order"
            ));
        }
    }
    Ok(())
}

/// Closes `h`: the save holds exactly the bytes the oracle computes (none
/// if the file was unlinked meanwhile), and nothing else changed.
fn close_checked(
    w: &mut World,
    o: &mut Oracle,
    h: HandleId,
    tally: &mut Tally,
) -> Result<(), String> {
    let t = w.handle(h).target;
    let (n, head) = (w.orphans().len(), head_of(w, t));
    // R5.3: a close of an open transaction's file returns at once and saves
    // nothing yet.
    if let Some(f) = o.close_defers(w, h) {
        w.close(h);
        if w.deferred(f).last() != Some(&h) {
            return Err(format!("oracle_deferred: R5.3: close h{h} did not defer"));
        }
        o.deferred.entry(f).or_default().push(Deferred::Close(h));
        tally.deferred_closes += 1;
        return o.check(w).map_err(|e| format!("close h{h}: {e}"));
    }
    let expect = o.closing(w, h);
    w.close(h);
    if expect.is_some() && head_of(w, t) != head {
        return Err(format!("oracle_orphans: close h{h}: an orphan saved"));
    }
    dropped(w, n, expect.into_iter().collect())?;
    gitledger::observe(w)?;
    o.check(w).map_err(|e| format!("close h{h}: {e}"))
}

/// The model raised exactly the `OrphanDropped` events the oracle expects
/// since event `n` (git-model.md §7: an orphan's calls are dropped, named).
fn dropped(w: &World, n: usize, expect: Vec<Dropped>) -> Result<(), String> {
    let got: Vec<Dropped> = w.orphans()[n..]
        .iter()
        .map(|e| (e.handle, e.writer.clone(), e.path.clone(), e.bytes))
        .collect();
    if got != expect {
        return Err(format!(
            "oracle_orphans: the model raised {got:?}, the oracle expects {expect:?}"
        ));
    }
    Ok(())
}

/// Every program closes the files it has open.
fn close_all_checked(w: &mut World, o: &mut Oracle, tally: &mut Tally) -> Result<(), String> {
    let open: Vec<_> = w
        .handles()
        .filter(|h| !h.deferred && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    for h in open {
        close_checked(w, o, h, tally)?;
    }
    Ok(())
}

/// A client's end event: the oracle closes its opens in the order the model
/// does (by handle), each as its close would; a fork's open transaction rolls
/// back first.
fn end_checked(w: &mut World, o: &mut Oracle, c: &Client) -> Result<(), String> {
    if let Client::Fork(f) = c
        && w.forks[f].txn.is_some()
    {
        rollback_checked(w, o, *f)?;
    }
    let of_it = |x: &Client| match c {
        Client::Fork(f) => *x == *c || *x == Client::Outside(*f),
        _ => *x == *c,
    };
    let mine: Vec<HandleId> = w
        .handles()
        .filter(|h| of_it(&h.client) && h.kind == Kind::Write)
        .map(|h| h.id)
        .collect();
    let n = w.orphans().len();
    let mut expect = Vec::new();
    for h in mine {
        expect.extend(o.closing(w, h));
    }
    match c {
        Client::Root(n) => w.end_root_client(n),
        Client::Fork(f) | Client::Outside(f) => w.end_client(*f),
    }
    dropped(w, n, expect)?;
    gitledger::observe(w)?;
    o.calls.retain(|h, _| w.handles().any(|x| x.id == *h));
    o.held.retain(|h, _| w.handles().any(|x| x.id == *h));
    o.check(w).map_err(|e| format!("end of {c:?}: {e}"))
}

/// A crash of every agentfs process (git-model.md §3), during open
/// `during`'s close if one is given. The crash rolls back every open
/// transaction (R5: "a failure is an implicit rollback"; R5.3, the owner's
/// pick: it then applies the calls it deferred): its opens end unsaved, and
/// its deferred calls apply in order, the oracle following each. Then the
/// oracle saves every open's calls as its close would (`during` first, then
/// the rest in the model's order, by handle), so every write call that
/// returned outside a transaction survives; an orphan's event comes once per
/// close the journal replays (twice for an orphan whose close the crash
/// interrupted: events are at least once). A take is one step, so none is in
/// progress.
fn crash_checked(w: &mut World, o: &mut Oracle, during: Option<HandleId>) -> Result<(), String> {
    let n = w.orphans().len();
    let mut expect = Vec::new();
    let mut during = during;
    let mut first = None;
    if let Some(h) = during {
        if let Some(f) = o.close_defers(w, h) {
            o.deferred.entry(f).or_default().push(Deferred::Close(h));
            during = None;
        } else {
            first = o.closing(w, h);
            expect.extend(first.clone());
        }
        // Observed after each save, so the ledger sees one tree move at a time.
        w.close_keeping_journal(h);
        gitledger::observe(w)?;
    }
    let open: Vec<TreeId> = in_txn(w);
    let replay = w.crash_down();
    if let Some(f) = open.iter().find(|f| w.forks[f].txn.is_some()) {
        return Err(format!(
            "oracle_crash: f{f}'s transaction outlived the crash"
        ));
    }
    dropped(w, n, expect)?;
    o.reload(w);
    let mut forks: Vec<TreeId> = o.deferred.keys().copied().collect();
    forks.extend(open);
    forks.sort();
    forks.dedup();
    for f in forks {
        apply_deferred_checked(w, o, f)?;
    }
    let n = w.orphans().len();
    let mut expect = Vec::new();
    for h in replay {
        if Some(h) == during {
            // Its close saved nothing git records, so the replay drops it
            // again, in handle order.
            expect.extend(first.clone());
        } else {
            expect.extend(o.closing(w, h));
        }
        w.restart_open(h);
        gitledger::observe(w)?;
    }
    dropped(w, n, expect)?;
    if let Some(h) = w.handles().next() {
        return Err(format!("oracle_crash: h{} outlived the crash", h.id));
    }
    o.calls.retain(|h, _| w.handles().any(|x| x.id == *h));
    o.held.retain(|h, _| w.handles().any(|x| x.id == *h));
    gitledger::observe(w)?;
    o.check(w).map_err(|e| format!("crash: {e}"))
}

/// R5.3, once fork `f`'s transaction has ended: the model's deferred calls
/// are the harness's, in order, and each applies now, the oracle following it
/// by its own byte code on the head it finds.
fn apply_deferred_checked(w: &mut World, o: &mut Oracle, f: TreeId) -> Result<(), String> {
    let list = o.deferred.remove(&f).unwrap_or_default();
    let model: Vec<HandleId> = w.deferred(f);
    let own: Vec<HandleId> = list.iter().map(Deferred::id).collect();
    if model != own {
        return Err(format!(
            "oracle_deferred: R5.3: f{f} deferred {model:?}, the harness made {own:?}"
        ));
    }
    let t = Target::Tree(f);
    for d in list {
        let n = w.orphans().len();
        let at = w.head(f);
        let mut expect = Vec::new();
        let run = match &d {
            Deferred::Close(h) => {
                expect.extend(o.closing(w, *h));
                None
            }
            Deferred::Path(h, to) => Some((w.handle(*h).file.clone(), to.clone())),
        };
        if w.apply_deferred(f) != Some(d.id()) {
            return Err(format!("oracle_deferred: R5.3: {d:?} did not apply next"));
        }
        if let Some((file, to)) = run
            && w.file(&at, &file).name.live()
        {
            o.ran(w, &at, t, &file, to.as_deref());
        }
        dropped(w, n, expect)?;
        o.check(w)
            .map_err(|e| format!("deferred {d:?} after its transaction: {e}"))?;
    }
    if w.apply_deferred(f).is_some() {
        return Err(format!(
            "oracle_deferred: R5.3: f{f} deferred more than the harness made"
        ));
    }
    gitledger::observe(w)
}

fn rollback_checked(w: &mut World, o: &mut Oracle, f: TreeId) -> Result<(), String> {
    let n = w.orphans().len();
    w.rollback_only(f).expect("open");
    o.reload(w);
    dropped(w, n, Vec::new())?;
    apply_deferred_checked(w, o, f)
}

/// A transaction's commit, then the calls it deferred (R5.3). The files open in the
/// fork or the transaction stay open; the transaction's write in the
/// fork from now on.
fn commit_checked(
    w: &mut World,
    o: &mut Oracle,
    f: TreeId,
    tally: &mut Tally,
) -> Result<(), String> {
    let txn = w.forks[&f].txn.as_ref().expect("open").id;
    let stay: Vec<(HandleId, Option<String>)> = o
        .held
        .iter()
        .filter(|(_, (u, _, _, _))| *u == Target::Txn(f, txn))
        .map(|(h, _)| (*h, o.named(w, *h)))
        .collect();
    tally.commits_with_open +=
        usize::from(w.handles().any(|h| {
            !h.deferred && (h.target == Target::Txn(f, txn) || h.target == Target::Tree(f))
        }));
    // Rule 5's legs, from git and the harness's own bytes: begin from the
    // transaction's ref, what the program staged from the oracle.
    let begin = git(w.repo(), &["rev-parse", &TreeNames::txn_branch(txn)]);
    let before = w.head(f);
    let mine = o
        .files
        .get(&Target::Txn(f, txn))
        .cloned()
        .unwrap_or_default();
    let outcome = w.commit_txn_only(f).expect("open");
    gitledger::observe(w)?;
    txn_committed(w, f, txn, &begin, &before, &mine, &outcome)?;
    o.reload(w);
    for (h, file) in stay {
        o.move_to_fork(h, f, file);
    }
    apply_deferred_checked(w, o, f)
}

/// `oracle_txn_commit`, rule 5 read from git against the harness's own
/// bytes: a transaction is recorded as one commit T (the owner, 2026-11-13),
/// the fork's head (a fast-forward) or its merge's second parent, whose one
/// parent is begin, as `txn/<n>` held it until the commit; T holds, in every
/// file it names, the bytes the program staged (the oracle's). An empty
/// commit made none: the fork's head did not move, and what the program
/// staged is begin's bytes.
fn txn_committed(
    w: &World,
    f: TreeId,
    txn: u32,
    begin: &str,
    before: &str,
    mine: &BTreeMap<String, Vec<u8>>,
    outcome: &TxnOutcome,
) -> Result<(), String> {
    let head = w.head(f);
    let t = if *outcome == TxnOutcome::Empty {
        if head != before {
            return Err(format!(
                "oracle_txn_commit: t{txn} was empty, but f{f} moved"
            ));
        }
        begin.to_string()
    } else {
        let parents = |c: &str| -> Vec<String> {
            git(w.repo(), &["log", "-1", "--format=%P", c])
                .split_whitespace()
                .map(str::to_string)
                .collect()
        };
        let p = parents(&head);
        let t = if p.len() == 1 {
            head.clone()
        } else {
            p[1].clone()
        };
        let cause = git(
            w.repo(),
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Agentfs-Cause,valueonly)",
                &t,
            ],
        );
        if cause != format!("Transaction t{txn}") || parents(&t) != [begin.to_string()] {
            return Err(format!(
                "oracle_txn_commit: t{txn}'s commit {t:.7} ({cause}) is not one commit on its begin {begin:.7}"
            ));
        }
        t
    };
    for (p, b) in mine {
        let file = w.file(&t, p);
        if file.name.live() && file.bytes().as_bytes() != &b[..] {
            return Err(format!(
                "oracle_txn_commit: t{txn}'s commit {t:.7} holds {p} as {:?}, the program staged {:?}",
                file.bytes(),
                String::from_utf8_lossy(b)
            ));
        }
    }
    Ok(())
}

fn drain(
    w: &mut Checked,
    o: &mut Oracle,
    tally: &mut Tally,
    at: usize,
    g: Generator,
) -> Result<(), (usize, String)> {
    for _ in 0..100_000 {
        close_all_checked(w, o, tally).map_err(|e| (at, e))?;
        checkg(w, o).map_err(|e| (at, e))?;
        let Some(s) = pending_of(w, g).into_iter().next() else {
            return w.converged().map_err(|e| (at, e));
        };
        take(w, o, &s, tally).map_err(|e| (at, e))?;
        checkg(w, o).map_err(|e| (at, e))?;
    }
    Err((at, "oracle_drained: the queues never drained".into()))
}

fn checkg(w: &mut Checked, o: &mut Oracle) -> Result<(), String> {
    w.check()?;
    gitledger::observe(w)?;
    o.no_call_waits(w)?;
    o.check(w)
}

/// What a generator may do beyond phase 1's scope. All on by default, so the
/// existing runs replay as before: a switch only skips a call, and draws no
/// random number the default would not.
#[derive(Clone, Copy)]
struct Generator {
    transactions: bool,
    nested: bool,
    drops: bool,
}

const EVERYTHING: Generator = Generator {
    transactions: true,
    nested: true,
    drops: true,
};

/// Phase 1 builds no transactions, nested forks or fork drops, so these seeds
/// are the ones that can run against the real system.
const PHASE_1: Generator = Generator {
    transactions: false,
    nested: false,
    drops: false,
};

/// The steps `g` may take now.
fn pending_of(w: &HistoryService, g: Generator) -> Vec<Step> {
    let mut steps = w.pending();
    if !g.drops {
        steps.retain(|s| !matches!(s, Step::Drop(_)));
    }
    steps
}

/// A generator of one seed's client calls and steps.
type Run = fn(&mut Checked, u64, &mut Tally) -> Result<(), (usize, String)>;

/// One seed: [`run`] under `catch_unwind`.
fn run_seed(seed: u64, policies: Policies, tally: &mut Tally) -> Result<String, String> {
    run_seed_of(run, seed, policies, tally)
}

/// One seed of `generate` under `catch_unwind`. `Ok` carries the run's
/// canonical text (`agentfs_model::digest`); `Err` the seed, step, violation and log.
fn run_seed_of(
    generate: Run,
    seed: u64,
    policies: Policies,
    tally: &mut Tally,
) -> Result<String, String> {
    let mut w = Checked::new(policies, TreeNames::new());
    w.label_trace(format!("seed{seed}"));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        generate(&mut w, seed, tally)
    }));
    let (step, e) = match out {
        Ok(Ok(())) => return Ok(agentfs_model::digest::canonical_text(&w)),
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

/// A waiting entry: its tree, the files it locks, and the (file, line
/// identity) pairs in contention there.
type Contention = (TreeId, Vec<String>, Vec<(String, String)>);

/// The client calls of a run, with the harness's state across them.
struct Calls {
    rng: Rng,
    o: Oracle,
    /// Each client's last read of each file (by identity), which a later
    /// whole-file rewrite writes back: often stale by then.
    reads: BTreeMap<(String, String), Content>,
    /// The root's live clients: one may die (its end event) and a new one start.
    root_clients: Vec<String>,
    next_root_client: usize,
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
            root_clients: vec!["m1".into(), "m2".into()],
            next_root_client: 3,
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

    /// Opens `p` for writing as `c`, writes line `slot` and closes: a save.
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
        let t = self.text(tally);
        self.line_edit(w, h, &LineEdit::At(slot), &t);
        close_checked(w, &mut self.o, h, tally)
    }

    /// Opens `p` as `c`, makes one random write call and closes.
    fn one_call(
        &mut self,
        w: &mut World,
        c: &Client,
        p: &str,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let h = self.open(w, c, p, tally);
        self.write(w, h, slot, tally);
        close_checked(w, &mut self.o, h, tally)
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
        let deferred = |w: &World| w.forks.keys().map(|f| w.deferred(*f).len()).sum::<usize>();
        let before = deferred(w);
        let out = self.path_call_as(w, c, f, p, unlink, tally);
        tally.deferred_path_calls += deferred(w) - before;
        out
    }

    fn path_call_as(
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
            self.write(w, h, slot, tally);
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
        close_checked(w, &mut self.o, h, tally)?;
        if atomic {
            tally.atomic_saves += 1;
            self.o.path_call(w, c, &tmp, Some(p))?;
        }
        Ok(())
    }

    /// R5.3: another program in a fork whose transaction is open calls on
    /// one of the transaction's files, mostly: a save, an open it leaves open
    /// to write and close later, an unlink or a rename. Steering only.
    fn outside_a_transaction(
        &mut self,
        w: &mut World,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<(), String> {
        let Some(f) = self.rng.pick(&in_txn(w)) else {
            return Ok(());
        };
        let c = Client::Outside(f);
        let txn = w.forks[&f].txn.as_ref().expect("open");
        let staged: Vec<String> = txn.staged.iter().map(|e| e.path.clone()).collect();
        let file = match self.rng.pick(&staged) {
            Some(x) if self.rng.below(4) != 0 => x,
            _ => FILES[self.rng.below(FILES.len())].to_string(),
        };
        let Some(p) = w.client_path(&c, &file) else {
            return Ok(());
        };
        match self.rng.below(4) {
            0 => self.save(w, &c, &p, slot, tally),
            1 => {
                let h = self.open(w, &c, &p, tally);
                self.write(w, h, slot, tally);
                Ok(())
            }
            2 => self.path_call(w, &c, &file, &p, true, tally),
            _ => self.path_call(w, &c, &file, &p, false, tally),
        }
    }

    /// While an entry waits for files in a tree, a client of that tree calls
    /// on one of them (or another file): opens it, saves it at once, or
    /// unlinks or renames it (each is granted, P3 being gone, and postpones the
    /// entry). The saves mostly touch a line in contention with a waiting
    /// landing, the pattern the 2026-11-03 review found by hand and 500 seeds
    /// missed (an upstream's save of a file a losing change touched); a take is
    /// one step (finding 49), so such a save comes before or after it whole.
    fn while_waiting(
        &mut self,
        w: &mut World,
        slot: usize,
        tally: &mut Tally,
    ) -> Result<bool, String> {
        // Each waiting entry with the files it locks and, for a landing, the
        // lines in contention there, read from git: the lines its commit set
        // over its first parent, and those the tree's head set over that
        // first parent (the commit against its base).
        let mut waiting: Vec<Contention> = Vec::new();
        for (t, m) in w.waits() {
            let queue = w.queue_numbered(*t);
            for (n, x) in m {
                let mut files: Vec<String> = x
                    .files
                    .iter()
                    .filter(|k| !k.starts_with(NAME_KEY))
                    .cloned()
                    .collect();
                let mut lines = Vec::new();
                if let Some((_, Entry::Landing { commit: c, .. })) =
                    queue.iter().find(|(q, _)| q == n)
                {
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
                waiting.push((*t, files, lines));
            }
        }
        let Some((t, held, contended)) = self.rng.pick(&waiting) else {
            return Ok(false);
        };
        let c = match w.upstream_of(t) {
            None => {
                Client::Root(self.root_clients[self.rng.below(self.root_clients.len())].clone())
            }
            Some(_) if w.forks[&t].life == Life::Open => Client::Fork(t),
            Some(_) => return Ok(false),
        };
        let file = match self.rng.pick(&held) {
            Some(x) if self.rng.below(4) != 0 => x,
            _ => FILES[self.rng.below(FILES.len())].to_string(),
        };
        let Some(p) = w.client_path(&c, &file) else {
            return Ok(false);
        };
        tally.calls_while_waiting += 1;
        match self.rng.below(6) {
            0 => {
                let unlink = self.rng.below(2) == 0;
                self.path_call(w, &c, &file, &p, unlink, tally)?;
            }
            1 | 2 => {
                self.open(w, &c, &p, tally);
            }
            _ => {
                let h = w.open_write(&c, &p);
                self.o.opened(w, h);
                // Mostly on a line in contention: removed (a save that
                // can turn a losing change of this file into one that
                // wins when it is taken) or written.
                tally.saves_while_waiting += 1;
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
                close_checked(w, &mut self.o, h, tally)?;
            }
        }
        Ok(true)
    }
}

fn run(w: &mut Checked, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    run_as(EVERYTHING, w, seed, tally)
}

/// [`run`], the calls phase 1 builds.
fn run_phase1(w: &mut Checked, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    run_as(PHASE_1, w, seed, tally)
}

fn run_as(
    g: Generator,
    w: &mut Checked,
    seed: u64,
    tally: &mut Tally,
) -> Result<(), (usize, String)> {
    gitledger::enable(true);
    gitledger::observe(w).map_err(|e| (0, e))?;
    let mut k = Calls::new(seed);
    k.o.reload(w);
    // The line a fork wrote last (its file's identity and position). The root
    // and forks write it again half the time, so runs often build "a fork
    // edits over its own undecided line while the root writes it
    // concurrently" (coherence finding 8).
    let mut focus: Option<(&str, usize)> = None;
    for step in 0..STEPS {
        let (f, slot) = match focus {
            Some(x) if k.rng.below(2) == 0 => x,
            _ => (FILES[k.rng.below(FILES.len())], k.rng.below(HOT_SLOTS)),
        };
        let mut clients: Vec<Client> = k.root_clients.iter().cloned().map(Client::Root).collect();
        clients.extend(live_clients(w).into_iter().map(Client::Fork));
        // Another program of a fork differs from its client only while a
        // transaction is open there.
        clients.extend(in_txn(w).into_iter().map(Client::Outside));
        let open: Vec<_> = w
            .handles()
            .filter(|h| !h.deferred && h.kind == Kind::Write)
            .map(|h| h.id)
            .collect();
        let e = |e| (step, e);
        match k.rng.below(34) {
            0..=2 => {
                let who = Client::Root(k.root_clients[k.rng.below(k.root_clients.len())].clone());
                if let Some(p) = w.client_path(&who, f) {
                    k.save(w, &who, &p, slot, tally).map_err(e)?;
                }
            }
            3 => {
                let forks = live_clients(w);
                if forks.len() < 4 {
                    let upstream = match k.rng.pick(&forks) {
                        Some(f) if k.rng.below(3) == 0 && g.nested => {
                            tally.nested += 1;
                            f
                        }
                        _ => TreeId::ROOT,
                    };
                    w.create_fork(upstream);
                    k.o.reload(w);
                    tally.forks += 1;
                }
            }
            4..=6 => {
                if let Some(c) = k.rng.pick(&live_clients(w)).map(Client::Fork)
                    && let Some(p) = w.client_path(&c, f)
                {
                    k.save(w, &c, &p, slot, tally).map_err(e)?;
                    focus = Some((f, slot));
                }
            }
            7 if g.transactions => {
                if let Some(f) = k.rng.pick(&live_clients(w)) {
                    let _ = w.begin(f);
                    k.o.reload(w);
                }
            }
            8 => {
                // Mostly commit a transaction that staged something, so
                // commits meet concurrent lines.
                if let Some(f) = k.rng.pick(&in_txn(w))
                    && (w.forks[&f]
                        .txn
                        .as_ref()
                        .is_some_and(|t| !t.staged.is_empty())
                        || k.rng.below(4) == 0)
                {
                    commit_checked(w, &mut k.o, f, tally).map_err(e)?;
                }
            }
            9 => {
                if let Some(f) = k.rng.pick(&in_txn(w))
                    && k.rng.below(2) == 0
                {
                    rollback_checked(w, &mut k.o, f).map_err(e)?;
                    tally.rolled_back += 1;
                }
            }
            10 => {
                if k.rng.below(4) == 0 {
                    // One of the root's clients dies; a new one starts.
                    let i = k.rng.below(k.root_clients.len());
                    end_checked(w, &mut k.o, &Client::Root(k.root_clients[i].clone()))
                        .map_err(e)?;
                    tally.root_deaths += 1;
                    k.root_clients[i] = format!("m{}", k.next_root_client);
                    k.next_root_client += 1;
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
                    k.open(w, &c, &p, tally);
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
                    close_checked(w, &mut k.o, h, tally).map_err(e)?;
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
            22 | 23 if !in_txn(w).is_empty() => {
                k.outside_a_transaction(w, slot, tally).map_err(e)?;
            }
            26..=29 if !w.waits().is_empty() && k.rng.below(2) == 0 => {
                // An entry waits: a fork of that tree saves a file the
                // entry does not lock, then a queue step there, so a later,
                // unrelated entry goes ahead of the waiting one (P1).
                let waiting: Vec<(TreeId, Vec<String>)> = w
                    .waits()
                    .iter()
                    .map(|(t, m)| {
                        (
                            *t,
                            m.values()
                                .flat_map(|x| x.files.iter().cloned())
                                .filter(|k| !k.starts_with(NAME_KEY))
                                .collect(),
                        )
                    })
                    .collect();
                if let Some((t, locked)) = k.rng.pick(&waiting) {
                    let free: Vec<&str> = FILES
                        .iter()
                        .copied()
                        .filter(|f| !locked.iter().any(|l| l == f))
                        .collect();
                    let kids: Vec<TreeId> = live_clients(w)
                        .into_iter()
                        .filter(|f| w.forks[f].upstream == t && w.forks[f].txn.is_none())
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
            26..=33 if !w.waits().is_empty() => {
                k.while_waiting(w, slot, tally).map_err(e)?;
            }
            _ => {
                if let Some(s) = k.rng.pick(&pending_of(w, g)) {
                    take(w, &mut k.o, &s, tally).map_err(e)?;
                }
            }
        }
        checkg(w, &mut k.o).map_err(e)?;
    }
    // Writes stop: every open file closes, every open transaction commits
    // or rolls back; the queues drain; every fork equals the root.
    close_all_checked(w, &mut k.o, tally).map_err(|e| (STEPS, e))?;
    for f in in_txn(w) {
        if k.rng.below(2) == 0 {
            commit_checked(w, &mut k.o, f, tally).map_err(|e| (STEPS, e))?;
        } else {
            rollback_checked(w, &mut k.o, f).map_err(|e| (STEPS, e))?;
            tally.rolled_back += 1;
        }
    }
    checkg(w, &mut k.o).map_err(|e| (STEPS, e))?;
    drain(w, &mut k.o, tally, STEPS, g)?;
    // Every program exits: every fork drops.
    for f in live_clients(w) {
        end_checked(w, &mut k.o, &Client::Fork(f)).map_err(|e| (STEPS + 1, e))?;
    }
    drain(w, &mut k.o, tally, STEPS + 1, g)?;
    if let (true, Some(f)) = (g.drops, w.forks.values().find(|f| f.life != Life::Dropped)) {
        return Err((STEPS + 1, format!("f{} never dropped", f.id)));
    }
    tally_run(w, tally);
    tally.fast_forwards += w.fast_forwards;
    tally.inserts_met += w.met().inserts;
    tally.unlinked_edits += w.met().unlinked_edits;
    tally.orphans += w.orphans().len();
    tally.prefix_syncs += w.prefix_syncs;
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

/// Rounds of client calls, each drained to convergence: saves by the root and by
/// forks (nested ones included), every kind of write call (line edits as bytes,
/// raw `pwrite`s, partial appends, truncates, `O_TRUNC` opens, stale rewrites),
/// unlinks, renames and rename-overs, and transactions begun and committed, with
/// no queue step between the calls of a round. So a change meets many others
/// queued behind it, which steps between the calls keep apart: this generator
/// found findings 25 to 28. Every invariant holds after every call and step.
fn rounds(w: &mut Checked, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    rounds_as(EVERYTHING, w, seed, tally)
}

/// [`rounds`], the calls phase 1 builds.
fn rounds_phase1(w: &mut Checked, seed: u64, tally: &mut Tally) -> Result<(), (usize, String)> {
    rounds_as(PHASE_1, w, seed, tally)
}

fn rounds_as(
    g: Generator,
    w: &mut Checked,
    seed: u64,
    tally: &mut Tally,
) -> Result<(), (usize, String)> {
    gitledger::enable(true);
    gitledger::observe(w).map_err(|e| (0, e))?;
    let mut k = Calls::new(seed);
    k.o.reload(w);
    let m1 = Client::Root("m1".into());
    for round in 0..ROUNDS {
        for _ in 0..ROUND_CALLS {
            let (f, slot) = (FILES[k.rng.below(FILES.len())], k.rng.below(HOT_SLOTS));
            let e = |e| (round, e);
            let fork = k.rng.pick(&live_clients(w)).map(Client::Fork);
            match k.rng.below(16) {
                0 => {
                    let forks = live_clients(w);
                    if forks.len() < 4 {
                        let upstream = match k.rng.pick(&forks) {
                            Some(f) if k.rng.below(3) == 0 && g.nested => f,
                            _ => TreeId::ROOT,
                        };
                        w.create_fork(upstream);
                        k.o.reload(w);
                    }
                }
                1..=3 => {
                    if let Some(p) = w.client_path(&m1, f) {
                        k.save(w, &m1, &p, slot, tally).map_err(e)?;
                    }
                }
                4..=6 => {
                    if let Some(c) = fork
                        && let Some(p) = w.client_path(&c, f)
                    {
                        k.save(w, &c, &p, slot, tally).map_err(e)?;
                    }
                }
                7..=9 => {
                    if let Some(c) = fork
                        && let Some(p) = w.client_path(&c, f)
                    {
                        k.one_call(w, &c, &p, slot, tally).map_err(e)?;
                    }
                }
                10 => {
                    if let Some(p) = w.client_path(&m1, f) {
                        k.one_call(w, &m1, &p, slot, tally).map_err(e)?;
                    }
                }
                11 if g.transactions => {
                    if let Some(f) = k.rng.pick(&live_clients(w)) {
                        let _ = w.begin(f);
                        k.o.reload(w);
                    }
                }
                12 => {
                    if let Some(f) = k.rng.pick(&in_txn(w)) {
                        commit_checked(w, &mut k.o, f, tally).map_err(e)?;
                    }
                }
                13 if k.rng.below(4) == 0 => {
                    // A fork's client dies: its transaction rolls back,
                    // its opens close, and the fork ends.
                    if let Some(c) = fork {
                        end_checked(w, &mut k.o, &c).map_err(e)?;
                    }
                }
                13 => {
                    let c = fork.unwrap_or_else(|| m1.clone());
                    if let Some(p) = w.client_path(&c, f) {
                        k.reads
                            .insert((format!("{c:?}"), f.to_string()), w.client_reads(&c, &p));
                    }
                }
                _ => {
                    let c = match fork {
                        Some(c) if k.rng.below(2) == 0 => c,
                        _ => m1.clone(),
                    };
                    if k.rng.below(2) == 0
                        && let Some(p) = w.client_path(&c, f)
                    {
                        if k.rng.below(4) == 0 {
                            k.create(w, &c, &p, slot, tally).map_err(e)?;
                        } else {
                            let unlink = k.rng.below(3) == 0;
                            k.path_call(w, &c, f, &p, unlink, tally).map_err(e)?;
                        }
                    }
                }
            }
            checkg(w, &mut k.o).map_err(e)?;
        }
        drain(w, &mut k.o, tally, round, g)?;
    }
    tally_run(w, tally);
    Ok(())
}

/// Rounds of [`rounds`].
const ROUNDS: usize = 4;
/// Client calls per round.
const ROUND_CALLS: usize = 12;

/// Changes that lost whole in a finished run, and their clean lines.
fn tally_run(w: &World, tally: &mut Tally) {
    for l in &w.landings {
        tally.changes_lost += usize::from(!l.won);
    }
    tally.clean_lines_lost += w.collisions.iter().filter(|c| c.by == By::Whole).count();
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
        // Takes really wait for write-opens (rule 9).
        assert!(
            tally.take_waits > 0,
            "no take waited in {} seeds: {tally:?}",
            seeds().len()
        );
        // Rules 10 and 11 met in the random runs, not in one scenario
        // each: a new line placed beside an upstream's concurrent new line,
        // an edit of a file the upstream had unlinked; and saves made while
        // an entry waited for files in their tree.
        assert!(
            tally.inserts_met > 0 && tally.unlinked_edits > 0 && tally.saves_while_waiting > 0,
            "rules 10 and 11, or saves while an entry waits, not met in {} seeds: {tally:?}",
            seeds().len()
        );
        // Atomic saves, and old writers whose calls an atomic save, an
        // unlink or a rename-over orphaned (git-model.md §7).
        // P1 and P2 met: an entry went ahead of a waiting one, and a sync
        // delivered a prefix and held the rest back.
        assert!(
            tally.overtakes > 0 && tally.prefix_syncs > 0,
            "progress rules not met in {} seeds: {tally:?}",
            seeds().len()
        );
        // R5.3 met: calls deferred behind a transaction, and commits that
        // went ahead with a file open in their fork.
        assert!(
            tally.deferred_closes > 0
                && tally.deferred_path_calls > 0
                && tally.commits_with_open > 0,
            "R5.3 not met in {} seeds: {tally:?}",
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

/// CI runs [`random_interleavings_rounds`] at seeds 1..=ROUNDS_SEEDS: each seed
/// drains four times, so 64 seeds would be the slowest test by far.
/// `AGENTFS_MODEL_SEEDS` and `AGENTFS_MODEL_SEED` apply as to every random run.
const ROUNDS_SEEDS: u64 = 16;

/// [`rounds`]: many calls with no queue step between them, every invariant.
#[test]
fn random_interleavings_rounds() {
    let explicit = ["AGENTFS_MODEL_SEED", "AGENTFS_MODEL_SEEDS"]
        .iter()
        .any(|v| std::env::var(v).is_ok());
    let seeds = if explicit {
        seeds()
    } else {
        (1..=ROUNDS_SEEDS).collect()
    };
    let mut tally = Tally::default();
    let failures: Vec<String> = seeds
        .iter()
        .filter_map(|seed| run_seed_of(rounds, *seed, Policies::default(), &mut tally).err())
        .collect();
    eprintln!("rounds, {} seeds: {tally:?}", seeds.len());
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n\n")
    );
    if seeds.len() as u64 >= ROUNDS_SEEDS {
        assert!(
            tally.changes_lost > 0 && tally.clean_lines_lost > 0,
            "rounds: no change lost whole with a clean line in {} seeds",
            seeds.len()
        );
    }
}

/// One [`Run`] over the seeds, every invariant on, no tally floor: the floors
/// of [`run_all`] count transactions, which these runs do not make.
fn run_scoped(generate: Run, seeds: &[u64], at_least: impl Fn(&Tally) -> bool) {
    let mut tally = Tally::default();
    let failures: Vec<String> = seeds
        .iter()
        .filter_map(|seed| run_seed_of(generate, *seed, Policies::default(), &mut tally).err())
        .collect();
    eprintln!("phase 1 scope, {} seeds: {tally:?}", seeds.len());
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed:\n\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n\n")
    );
    assert!(
        seeds.len() < 8 || at_least(&tally),
        "the phase 1 runs met too little: {tally:?}"
    );
}

/// The random calls phase 1 builds, for `crates/agentfs-conformance` to replay.
#[test]
fn random_interleavings_phase1_scope() {
    run_scoped(run_phase1, &seeds(), |t| {
        t.take_waits > 0 && t.overtakes > 0 && t.prefix_syncs > 0
    });
}

/// [`rounds`], for the same replay.
#[test]
fn random_interleavings_rounds_phase1_scope() {
    let explicit = ["AGENTFS_MODEL_SEED", "AGENTFS_MODEL_SEEDS"]
        .iter()
        .any(|v| std::env::var(v).is_ok());
    let seeds = if explicit {
        seeds()
    } else {
        (1..=ROUNDS_SEEDS).collect()
    };
    run_scoped(rounds_phase1, &seeds, |t| t.changes_lost > 0);
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
/// history (the digest's canonical text, `agentfs_model::digest`). An order taken from commit
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
/// upstream already decided against from one still on its way.
#[test]
#[ignore = "coherence finding 1: git's own merge base of crossing syncs and landings keeps a fork line the upstream already decided against"]
fn random_interleavings_git_merge_base() {
    run_all(Policies {
        merge: MergeRule::GitMergeBase,
        ..Policies::default()
    });
}
