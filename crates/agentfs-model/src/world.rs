//! The model: main, forks and transaction forks as git branches with their
//! own worktrees, and the target design's landing rules layered on top.
//!
//! | Design term (system.md) | Here |
//! |---|---|
//! | main's tree and line | the repository's own worktree, branch `main` |
//! | main's journal tail and commit window (history.md §2) | records land in main's worktree at once and wait in a tail; a `Window` step commits the tail, one commit per group of consecutive records of one writer and one cause |
//! | an in-volume fork F and its line | branch `fork/F` and its worktree; files F holds its own copy of live there, the rest read through to the parent |
//! | a save; a fork commit | one commit naming its one writer in an `Agentfs-Writer` trailer |
//! | a fork run (§3.5) | whole fork commits, in order, merged into the parent and recorded there as ONE record |
//! | a sync (§3.5) | the parent's newer record of a file merged into a fork's own copy, committed on the fork's line with the parent's commit as second parent |
//! | a transaction fork T (§3.6) | branch `txn/T` and its worktree; seal lands it in F as one commit; rollback deletes the branch |
//! | a path base (forks.md §6, F3, I11) | [`Base`]: a commit on the fork's line (`Kept`) or a captured blob (`Synced`, copy-up); never a content a landing computed |
//! | the resolver chain (§3.7) | `Exact` here, `Lines` is `git merge-file`; `Structure` and `Agent` are stubs that never merge |
//! | the history setting (§4) | the run commit's second parent: the fork's commit (`FullLineage`) or a squash commit on `refs/squash/F` (`Squashed`) |
//!
//! Every landing goes through the two phases of §3.5's state machine: a
//! step in `Waiting` prepares it outside any gate (the merge against what
//! the target tracked then); a later step enters `Writing` once nothing is
//! ahead of it and no write-open blocks it, verifies the tracked content
//! and re-merges once if it moved, then writes. Other client calls and
//! landings interleave between the two. `Writing` itself (gate up, verify,
//! rename, move bases, record) is one step: the gate is the freeze, so
//! nothing interleaves inside it.
//!
//! "Through git" means the repository holds every line, commit, squash
//! chain and captured base, and line merges run `git merge-file`. The merge
//! base is always the harness's own path base, never git's merge-base
//! (system.md I4: "never git's merge base").

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, Lineage, PATHS, SLOTS};
use crate::git::{git, git_in, try_git};

pub type ForkId = u32;
pub type HandleId = u32;

/// A tree: main's, or a fork's (transaction forks included).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tree {
    Main,
    Fork(ForkId),
}

/// The history setting (system.md §4, R12, A7): one per volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryPolicy {
    FullLineage,
    Squashed,
}

/// The open question under system.md §3.6: what a seal whose merge
/// conflicts does. `Held` is the docs' current choice; `Rollback` the
/// alternative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealConflict {
    Held,
    Rollback,
}

/// Which forks a parent's record syncs into once they have ended (design
/// finding 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndedForkCopies {
    /// The docs today: at `ForkEnded` "`copies` stops naming it" (forks.md
    /// §5 events; §3 "`copies` names open editable children"), yet "after
    /// `ForkEnded` a waiting sync into the fork lands" (system.md §3.5). A
    /// newer record, the fork's own landing record included, no longer
    /// joins that waiting sync, so the stale sync lands.
    StopAtEnd,
    /// The fix the model proposes: `copies` names a fork until it drops, so
    /// a newer record still joins a waiting sync into it.
    UntilDropped,
}

/// The resolver chain (system.md §3.7), in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolver {
    /// One side changed, or both made the same change.
    Exact,
    /// Three-way line merge: `git merge-file`.
    Lines,
    /// Stub: the model's files name no structured format (Rules), so it
    /// never merges.
    Structure,
    /// Stub: off by default (§4), never merges.
    Agent,
}

#[derive(Clone, Debug)]
pub struct Policies {
    pub history: HistoryPolicy,
    pub seal_conflict: SealConflict,
    pub resolvers: Vec<Resolver>,
    pub ended_fork_copies: EndedForkCopies,
}

impl Default for Policies {
    /// The docs' current settings (system.md §4): `FullLineage`, a
    /// conflicting seal held, the first three resolvers on, `copies` naming
    /// only open forks.
    fn default() -> Self {
        Self {
            history: HistoryPolicy::FullLineage,
            seal_conflict: SealConflict::Held,
            resolvers: vec![Resolver::Exact, Resolver::Lines, Resolver::Structure],
            ended_fork_copies: EndedForkCopies::StopAtEnd,
        }
    }
}

/// A program writing through a view: one of main's clients (by writer
/// name), or the one client of an editable fork (one writer per fork, for
/// life: system.md §3.3 `ForkTaken`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Client {
    Main(String),
    Fork(ForkId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Open,
    /// Its client left (`LastDetached`); it drops once nothing is open.
    Ended,
    Dropped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnState {
    Open,
    Sealed,
    Held,
    Landed,
    RolledBack,
}

/// Why a transaction fork rolled back (system.md §3.6, "Rollback, client
/// death", and the open question).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollbackCause {
    Abandoned,
    ClientEnded,
    /// Only under [`SealConflict::Rollback`].
    SealConflict,
    /// A held seal's `withdraw`.
    Withdrawn,
}

/// What `seal` answers once settled (system.md §3.6, "Seal": "`seal`
/// answers once the run is `Landed` or `Held`"; the alternative answers a
/// rollback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealAnswer {
    Landed,
    Held,
    RolledBack,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    /// system.md §3.3 `ForkError::InTransaction`: F has one open already.
    InTransaction,
    NotEditable,
    Ended,
}

/// A path base (forks.md §3 `BaseMove`, F3; system.md I11): it names only
/// what grouping or truncation never removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// A run's: the fork's own commit at its `through`, on its line.
    Kept { commit: String },
    /// A sync's, a gained copy's or a copy-up's: the parent content the
    /// merge read, captured as a blob.
    Captured { blob: String },
    /// No copy, no base.
    Trimmed,
}

/// A token one write put into one slot of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub slot: usize,
    pub token: String,
}

/// One fork commit (system.md §3.6): a save, a child's landed run, or a
/// sealed transaction.
#[derive(Clone, Debug)]
pub struct ForkCommit {
    pub seq: u64,
    pub writer: String,
    pub files: BTreeMap<String, Content>,
    /// The commit on the fork's line (on `txn/T` for a transaction's saves).
    pub git: String,
    /// The tokens it wrote; empty for a save that took the target's lines.
    pub edits: Vec<Edit>,
}

/// A landing prepared in `Waiting`, outside any gate (system.md §3.5).
#[derive(Clone, Debug, Default)]
struct Prepared {
    /// The target's tracked content of each file, read at prepare.
    tracked: BTreeMap<String, Content>,
    /// Each base read: (fork, path, position, value). One moved since means
    /// "prepares on the base left" (§3.5, "One path base, one order").
    bases: Vec<(ForkId, String, u64, Base)>,
    merged: BTreeMap<String, Content>,
    /// A run: how many commits it lands.
    n: usize,
    /// A run: the commit after the prefix conflicted, so it holds next.
    held_after: bool,
}

pub struct Fork {
    pub id: ForkId,
    pub parent: Tree,
    /// Its one client's writer name.
    pub writer: String,
    pub life: Life,
    /// `Some`: a transaction fork with this scope.
    pub scope: Option<BTreeSet<String>>,
    pub txn: Option<TxnState>,
    /// A transaction fork that was sealed.
    pub sealed: bool,
    pub rollback_cause: Option<RollbackCause>,
    /// Paths it holds its own copy of; the rest read through to the parent.
    pub own: BTreeSet<String>,
    /// Path bases (forks.md §6): per path, entries keyed by (the fork
    /// position after which they apply, wall order).
    pub(crate) bases: BTreeMap<String, BTreeMap<(u64, u64), Base>>,
    pub seq: u64,
    /// Fork commits, in order. For a transaction fork: its saves.
    pub commits: Vec<ForkCommit>,
    /// How many of `commits` have landed in the parent: always a prefix.
    pub landed: usize,
    /// The held commit (always `landed`) while its run is held.
    pub held: Option<usize>,
    /// landing.md §2: the run re-judged at the fork's next commit merges
    /// the held commit and the newer ones as one change.
    as_one: bool,
    pub withdrawn: bool,
    pub open_txn: Option<ForkId>,
    /// A held seal: the parent recorded a file in scope since (§3.6).
    pub rejudge: bool,
    /// When its landing (its run; a transaction fork's seal) entered
    /// `Waiting`.
    made: u64,
    prep: Option<Prepared>,
    wt: PathBuf,
    branch: String,
    head: Option<String>,
    pub fork_point: Option<String>,
    squash_head: Option<String>,
}

impl Fork {
    pub fn is_txn(&self) -> bool {
        self.scope.is_some()
    }
}

struct Handle {
    client: Client,
    tree: Tree,
    path: String,
}

/// Where a sync's record sits: main's record seq (its commit is the newest
/// main commit at or below it, §5), or a fork's commit.
#[derive(Clone, Debug)]
enum Origin {
    MainSeq(u64),
    Commit(String),
}

struct PendingSync {
    made: u64,
    writers: Vec<String>,
    content: Content,
    /// The parent's content, captured from the record (`Synced`).
    blob: String,
    from: Origin,
    prep: Option<Prepared>,
}

/// A record in main's journal tail, not yet committed.
struct MainRecord {
    seq: u64,
    writer: String,
    cause: String,
    files: BTreeMap<String, Content>,
    second: Option<String>,
}

/// A step a landing can take, chosen by a test or the random scheduler.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Run(ForkId),
    Sync(ForkId, String),
    Seal(ForkId),
    Trim(ForkId, String),
    Drop(ForkId),
    /// Main's commit window ticks: its tail becomes commits.
    Window,
}

/// A landing in `Waiting`, for "what is ahead" (landing.md §2).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Run(ForkId),
    Sync(ForkId, String),
    Seal(ForkId),
}

struct Waiting {
    key: Key,
    made: u64,
    target: Tree,
    scope: BTreeSet<String>,
    /// The (fork, path) bases it moves.
    moves: BTreeSet<(ForkId, String)>,
}

/// A token that landed in a tree: it must stay there or be overwritten only
/// by something that saw it.
#[derive(Clone, Debug)]
pub struct Landed {
    pub tree: Tree,
    pub edit: Edit,
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

pub struct World {
    pub policies: Policies,
    root: PathBuf,
    main_wt: PathBuf,
    scratch: PathBuf,
    /// Main's newest commit (the tail is not in it yet).
    pub main_head: String,
    /// Main's commits, oldest first, with the last record seq each holds.
    pub main_commits: Vec<String>,
    main_commit_seq: Vec<u64>,
    main_seq: u64,
    main_tail: Vec<MainRecord>,
    pub forks: BTreeMap<ForkId, Fork>,
    next_fork: ForkId,
    pub lineage: Lineage,
    /// Slot changes a tree's own client made knowingly, without a new
    /// token: (tree, path, slot, from, to). A sandbox saving the target's
    /// line over its own to fix a held conflict (landing.md §2).
    pub accepted: BTreeSet<(Tree, String, usize, String, String)>,
    counters: BTreeMap<String, u32>,
    handles: BTreeMap<HandleId, Handle>,
    next_handle: HandleId,
    ordinal: u64,
    syncs: BTreeMap<(ForkId, String), PendingSync>,
    objects: RefCell<BTreeMap<String, Content>>,
    /// Syncs that met a conflict (`Deferred`).
    pub deferred: usize,
    /// Syncs that joined a newer record of another writer.
    pub joined_across_writers: usize,
    /// Prepared landings that `Writing` found stale: a moved base or
    /// tracked content (re-prepared or re-merged).
    pub re_merges: usize,
    pub log: Vec<String>,
    pub rolled_back: BTreeSet<String>,
    pub landed: Vec<Landed>,
    /// The causal oracle's last view of each tree (`check`).
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
    /// `check`'s caches over immutable git objects.
    pub(crate) checked_commits: BTreeSet<String>,
    pub(crate) reach: BTreeMap<String, BTreeSet<String>>,
    pub(crate) main_checked: Option<String>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) blobs_ok: BTreeSet<String>,
}

fn message(subject: &str, writer: &str, cause: &str, tree: &str) -> String {
    format!("{subject}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: {cause}\nAgentfs-Tree: {tree}\n")
}

impl World {
    pub fn new(policies: Policies) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agentfs-model-{}-{}",
            std::process::id(),
            WORLDS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let main_wt = root.join("main");
        let scratch = root.join("scratch");
        std::fs::create_dir_all(&main_wt).expect("create main's tree");
        std::fs::create_dir_all(&scratch).expect("create scratch");
        git(&main_wt, &["init", "-q", "-b", "main"]);
        for p in PATHS {
            std::fs::write(main_wt.join(p), Content::empty().render()).expect("write");
        }
        git(&main_wt, &["add", "-A"]);
        let tree = git(&main_wt, &["write-tree"]);
        let msg = message("start", "agentfs", "Root", "main");
        let c = git_in(&main_wt, &["commit-tree", &tree, "-F", "-"], &msg);
        git(&main_wt, &["update-ref", "HEAD", &c]);
        Self {
            policies,
            root,
            main_wt,
            scratch,
            main_head: c.clone(),
            main_commits: vec![c],
            main_commit_seq: vec![0],
            main_seq: 0,
            main_tail: Vec::new(),
            forks: BTreeMap::new(),
            next_fork: 1,
            lineage: Lineage::default(),
            accepted: BTreeSet::new(),
            counters: BTreeMap::new(),
            handles: BTreeMap::new(),
            next_handle: 1,
            ordinal: 0,
            syncs: BTreeMap::new(),
            objects: RefCell::new(BTreeMap::new()),
            deferred: 0,
            joined_across_writers: 0,
            re_merges: 0,
            log: Vec::new(),
            rolled_back: BTreeSet::new(),
            landed: Vec::new(),
            views: BTreeMap::new(),
            checked_commits: BTreeSet::new(),
            reach: BTreeMap::new(),
            main_checked: None,
            grepped: (0, BTreeSet::new()),
            blobs_ok: BTreeSet::new(),
        }
    }

    pub fn repo(&self) -> &Path {
        &self.main_wt
    }

    fn note(&mut self, s: String) {
        self.log.push(s);
    }

    /// Logs a scheduler's choice, so a failing seed's log replays it.
    pub fn note_step(&mut self, s: &Step) {
        self.note(format!("step {s:?}"));
    }

    fn next_ordinal(&mut self) -> u64 {
        self.ordinal += 1;
        self.ordinal
    }

    // ── reads ────────────────────────────────────────────────────────────

    /// What tree `t` shows for `p`: its own copy, else its parent's (a fork
    /// reads its parent live: system.md §5, "A new view opens").
    pub fn read(&self, t: Tree, p: &str) -> Content {
        match t {
            Tree::Main => read_file(&self.main_wt.join(p)),
            Tree::Fork(f) => {
                let fork = &self.forks[&f];
                if fork.own.contains(p) {
                    read_file(&fork.wt.join(p))
                } else {
                    self.read(fork.parent, p)
                }
            }
        }
    }

    /// `t` and every tree it reads through, up to main.
    pub fn reads_through(&self, t: Tree) -> Vec<Tree> {
        let mut v = vec![t];
        while let Some(Tree::Fork(f)) = v.last().copied() {
            v.push(self.forks[&f].parent);
        }
        v
    }

    /// What a client's program reads: while its transaction is open, the
    /// transaction fork, then its fork (system.md §3.6, "Who sees what").
    pub fn client_reads(&self, c: &Client, p: &str) -> Content {
        self.read(self.client_tree(c), p)
    }

    fn client_tree(&self, c: &Client) -> Tree {
        match c {
            Client::Main(_) => Tree::Main,
            Client::Fork(f) => match self.forks[f].open_txn {
                Some(t) => Tree::Fork(t),
                None => Tree::Fork(*f),
            },
        }
    }

    /// The tree a write-open marks: the open transaction fork if its scope
    /// holds `p`, else the client's own tree (system.md §3.6, "Staged
    /// writes"; landing.md §6, "A transaction").
    fn mark_tree(&self, c: &Client, p: &str) -> Tree {
        match c {
            Client::Main(_) => Tree::Main,
            Client::Fork(f) => match self.forks[f].open_txn {
                Some(t) if self.forks[&t].scope.as_ref().is_some_and(|s| s.contains(p)) => {
                    Tree::Fork(t)
                }
                _ => Tree::Fork(*f),
            },
        }
    }

    fn writer(&self, c: &Client) -> String {
        match c {
            Client::Main(w) => w.clone(),
            Client::Fork(f) => self.forks[f].writer.clone(),
        }
    }

    /// The tip of `t`'s line; a fork with no commit yet: its parent's.
    pub fn head(&self, t: Tree) -> String {
        match t {
            Tree::Main => self.main_head.clone(),
            Tree::Fork(f) => {
                let fork = &self.forks[&f];
                fork.head.clone().unwrap_or_else(|| self.head(fork.parent))
            }
        }
    }

    /// The commits `t`'s line reaches or is about to: main's head and the
    /// second parents of the records in its tail; a fork's head.
    pub fn line_tips(&self, t: Tree) -> Vec<String> {
        let mut v = vec![self.head(t)];
        if t == Tree::Main {
            v.extend(self.main_tail.iter().filter_map(|r| r.second.clone()));
        }
        v
    }

    /// Every write-open's tree, for the "no handle outlives its tree" check.
    pub fn handle_trees(&self) -> Vec<(HandleId, Tree)> {
        self.handles.iter().map(|(h, x)| (*h, x.tree)).collect()
    }

    /// git content of an object spec (`<commit>:<path>` or a blob id),
    /// cached: objects never change.
    fn object(&self, spec: &str) -> Content {
        if let Some(c) = self.objects.borrow().get(spec) {
            return c.clone();
        }
        let c = Content::parse(&git(&self.main_wt, &["cat-file", "-p", spec]))
            .expect("a model file in git");
        self.objects
            .borrow_mut()
            .insert(spec.to_string(), c.clone());
        c
    }

    fn capture(&self, c: &Content) -> String {
        git_in(
            &self.main_wt,
            &["hash-object", "-w", "--stdin"],
            &c.render(),
        )
    }

    /// F's base of p for its record at `at` (forks.md §6: per position; the
    /// only merge base, never git's: system.md I4).
    pub fn base_ref(&self, f: ForkId, p: &str, at: u64) -> Base {
        self.forks[&f]
            .bases
            .get(p)
            .and_then(|m| m.range(..(at, 0)).next_back())
            .map_or(Base::Trimmed, |(_, b)| b.clone())
    }

    fn base_content(&self, b: &Base, p: &str) -> Option<Content> {
        match b {
            Base::Kept { commit } => Some(self.object(&format!("{commit}:{p}"))),
            Base::Captured { blob } => Some(self.object(blob)),
            Base::Trimmed => None,
        }
    }

    fn set_base(&mut self, f: ForkId, p: &str, after: u64, b: Base) {
        let o = self.next_ordinal();
        self.forks
            .get_mut(&f)
            .expect("fork")
            .bases
            .entry(p.to_string())
            .or_default()
            .insert((after, o), b);
    }

    // ── clients ──────────────────────────────────────────────────────────

    /// A launcher creates a fork over `parent` (system.md §5, "A new view
    /// opens"): an empty layer that reads its parent live.
    pub fn create_fork(&mut self, parent: Tree) -> ForkId {
        if let Tree::Fork(p) = parent {
            // §3.6 table: "A transaction fork has no child".
            assert!(!self.forks[&p].is_txn(), "a transaction fork has no child");
        }
        self.new_fork(parent, None)
    }

    fn new_fork(&mut self, parent: Tree, scope: Option<BTreeSet<String>>) -> ForkId {
        let id = self.next_fork;
        self.next_fork += 1;
        let txn = scope.is_some();
        let branch = if txn {
            format!("txn/{id}")
        } else {
            format!("fork/{id}")
        };
        let writer = match parent {
            Tree::Fork(p) if txn => self.forks[&p].writer.clone(),
            _ => format!("c{id}"),
        };
        let wt = self.root.join(format!("f{id}"));
        let at = self.head(parent);
        git(
            &self.main_wt,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                wt.to_str().expect("utf-8 path"),
                &at,
            ],
        );
        self.forks.insert(
            id,
            Fork {
                id,
                parent,
                writer,
                life: Life::Open,
                txn: txn.then_some(TxnState::Open),
                sealed: false,
                rollback_cause: None,
                scope,
                own: BTreeSet::new(),
                bases: BTreeMap::new(),
                seq: 0,
                commits: Vec::new(),
                landed: 0,
                held: None,
                as_one: false,
                withdrawn: false,
                open_txn: None,
                rejudge: false,
                made: 0,
                prep: None,
                wt,
                branch,
                head: None,
                fork_point: None,
                squash_head: None,
            },
        );
        if txn {
            self.note(format!("begin t{id} over {parent:?}"));
        } else {
            self.note(format!("create f{id} over {parent:?}"));
        }
        id
    }

    /// Open, write slot `slot`, close: one save (system.md §5, first flow).
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let tree = self.mark_tree(c, p);
        let writer = self.writer(c);
        self.write_in(tree, &writer, p, slot)
    }

    /// A write-open that stays open; its mark decides its tree now
    /// (system.md §3.6: "The mark is the one decider of the tree").
    pub fn open(&mut self, c: &Client, p: &str) -> HandleId {
        let tree = self.mark_tree(c, p);
        let h = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            h,
            Handle {
                client: c.clone(),
                tree,
                path: p.to_string(),
            },
        );
        self.note(format!("open h{h} {c:?} {p} in {tree:?}"));
        h
    }

    /// Writes slot `slot` through handle `h` and closes it. `None` if the
    /// handle was dropped with its client or its transaction.
    pub fn close(&mut self, h: HandleId, slot: usize) -> Option<String> {
        let handle = self.handles.remove(&h)?;
        let writer = self.writer(&handle.client);
        self.note(format!("close h{h}"));
        Some(self.write_in(handle.tree, &writer, &handle.path, slot))
    }

    pub fn open_handles(&self) -> Vec<HandleId> {
        self.handles.keys().copied().collect()
    }

    pub fn handle_client(&self, h: HandleId) -> Option<Client> {
        self.handles.get(&h).map(|x| x.client.clone())
    }

    fn token(&mut self, writer: &str, tree: Tree) -> String {
        let n = self.counters.entry(writer.to_string()).or_default();
        *n += 1;
        let n = *n;
        match tree {
            Tree::Fork(t) if self.forks[&t].is_txn() => format!("{writer}.{n}~t{t}"),
            _ => format!("{writer}.{n}"),
        }
    }

    fn write_in(&mut self, tree: Tree, writer: &str, p: &str, slot: usize) -> String {
        let cur = self.read(tree, p);
        let prev = cur.slot(slot).to_string();
        let token = self.token(writer, tree);
        self.lineage.wrote(&token, &prev);
        let new = cur.with(slot, &token);
        let edit = Edit {
            path: p.to_string(),
            slot,
            token: token.clone(),
        };
        self.note(format!("save {token} into {tree:?}:{p}[{slot}]"));
        match tree {
            Tree::Main => {
                self.record_main(writer, "Direct", one(p, new), None);
            }
            Tree::Fork(f) => {
                self.copy_up(f, p);
                self.record_fork(f, writer, "Direct", one(p, new), None, Some(vec![edit]));
            }
        }
        token
    }

    /// A fork's first write of p copies it up; the base is what the parent
    /// showed, captured (forks.md §6, "A fork's first write of p").
    fn copy_up(&mut self, f: ForkId, p: &str) {
        if self.forks[&f].own.contains(p) {
            return;
        }
        let parent = self.forks[&f].parent;
        let blob = self.capture(&self.read(parent, p));
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Base::Captured { blob });
        self.forks
            .get_mut(&f)
            .expect("fork")
            .own
            .insert(p.to_string());
    }

    /// A copy a landing gives F takes as its base the content the merge read
    /// as ours, captured (forks.md §6, "Transaction T over F": `Synced`, F3).
    fn gain_copy(&mut self, f: ForkId, p: &str, ours: &Content) {
        if self.forks[&f].own.contains(p) {
            return;
        }
        let blob = self.capture(ours);
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Base::Captured { blob });
        self.forks
            .get_mut(&f)
            .expect("fork")
            .own
            .insert(p.to_string());
    }

    /// The sandbox fixes its held conflict by saving again (landing.md §2:
    /// "the sandbox fixes a conflict by saving again"): told of the conflict
    /// (the integrator notice), its program saves the target's line over
    /// each slot where its held change and the target's differ. One save
    /// per file; the next run merges it with the held commit as one change.
    /// `false` if F has no held run or no client.
    pub fn save_target_lines(&mut self, f: ForkId) -> bool {
        let fork = &self.forks[&f];
        if fork.held.is_none() || fork.life != Life::Open || fork.open_txn.is_some() {
            return false;
        }
        let (target, writer) = (fork.parent, fork.writer.clone());
        let mut first: BTreeMap<String, u64> = BTreeMap::new();
        for c in &fork.commits[fork.landed..] {
            for p in c.files.keys() {
                first.entry(p.clone()).or_insert(c.seq);
            }
        }
        let mut any = false;
        for (p, at) in first {
            let Some(base) = self.base_content(&self.base_ref(f, &p, at), &p) else {
                continue;
            };
            let ours = self.read(target, &p);
            let theirs = self.read(Tree::Fork(f), &p);
            let mut new = theirs.clone();
            for i in 0..SLOTS {
                let (b, o, t) = (base.slot(i), ours.slot(i), theirs.slot(i));
                if b != o && b != t && o != t {
                    new = new.with(i, o);
                    self.accepted.insert((
                        Tree::Fork(f),
                        p.clone(),
                        i,
                        t.to_string(),
                        o.to_string(),
                    ));
                }
            }
            if new != theirs {
                self.note(format!("f{f} saves {target:?}'s lines of {p}"));
                self.record_fork(f, &writer, "Direct", one(&p, new), None, Some(vec![]));
                any = true;
            }
        }
        any
    }

    // ── transactions (system.md §3.6) ────────────────────────────────────

    /// `Forks::begin`: T over the client's fork F. F has at most one open.
    pub fn begin(&mut self, f: ForkId, scope: &[&str]) -> Result<ForkId, ModelError> {
        let fork = &self.forks[&f];
        if fork.is_txn() {
            return Err(ModelError::NotEditable);
        }
        if fork.life != Life::Open {
            return Err(ModelError::Ended);
        }
        if fork.open_txn.is_some() {
            return Err(ModelError::InTransaction);
        }
        let scope: BTreeSet<String> = scope.iter().map(|s| s.to_string()).collect();
        let t = self.new_fork(Tree::Fork(f), Some(scope));
        self.forks.get_mut(&f).expect("fork").open_txn = Some(t);
        Ok(t)
    }

    /// `Forks::seal`: T ends `Sealed`; new write-opens go to F. Its run
    /// enters `Writing` only once its last write-open closes. The answer
    /// comes later: [`World::seal_answer`].
    pub fn seal(&mut self, t: ForkId) {
        let made = self.next_ordinal();
        let fork = self.forks.get_mut(&t).expect("txn");
        assert_eq!(fork.txn, Some(TxnState::Open), "seal an open transaction");
        fork.txn = Some(TxnState::Sealed);
        fork.sealed = true;
        fork.made = made;
        let parent = fork.parent;
        if let Tree::Fork(f) = parent {
            self.forks.get_mut(&f).expect("fork").open_txn = None;
        }
        self.note(format!("seal t{t}"));
    }

    /// What `seal` has answered: `None` while it still waits.
    pub fn seal_answer(&self, t: ForkId) -> Option<SealAnswer> {
        let fork = &self.forks[&t];
        if !fork.sealed {
            return None;
        }
        match fork.txn? {
            TxnState::Landed => Some(SealAnswer::Landed),
            TxnState::Held => Some(SealAnswer::Held),
            TxnState::RolledBack => Some(SealAnswer::RolledBack),
            TxnState::Open | TxnState::Sealed => None,
        }
    }

    /// `Forks::abandon`: the rollback (A13). T drops with its layer, bases
    /// and stream; F's files never changed.
    pub fn abandon(&mut self, t: ForkId) {
        self.rollback(t, RollbackCause::Abandoned);
    }

    /// A held seal's `withdraw` rolls it back (§3.6, "Seal, held").
    pub fn withdraw_seal(&mut self, t: ForkId) {
        assert_eq!(self.forks[&t].txn, Some(TxnState::Held));
        self.rollback(t, RollbackCause::Withdrawn);
    }

    pub fn txn_state(&self, t: ForkId) -> TxnState {
        self.forks[&t].txn.expect("a transaction fork")
    }

    fn rollback(&mut self, t: ForkId, cause: RollbackCause) {
        let tokens: Vec<String> = self.forks[&t]
            .commits
            .iter()
            .flat_map(|c| c.edits.iter().map(|e| e.token.clone()))
            .collect();
        self.rolled_back.extend(tokens);
        self.handles.retain(|_, h| h.tree != Tree::Fork(t));
        self.forks.get_mut(&t).expect("txn").rollback_cause = Some(cause);
        self.note(format!("rollback t{t} ({cause:?})"));
        self.drop_txn(t, TxnState::RolledBack);
    }

    fn drop_txn(&mut self, t: ForkId, state: TxnState) {
        self.remove_line(t);
        let fork = self.forks.get_mut(&t).expect("txn");
        fork.txn = Some(state);
        fork.life = Life::Dropped;
        fork.own.clear();
        fork.prep = None;
        if let Tree::Fork(f) = fork.parent {
            let parent = self.forks.get_mut(&f).expect("fork");
            if parent.open_txn == Some(t) {
                parent.open_txn = None;
            }
        }
    }

    fn remove_line(&mut self, f: ForkId) {
        let (wt, branch) = {
            let fork = &self.forks[&f];
            (fork.wt.clone(), fork.branch.clone())
        };
        git(
            &self.main_wt,
            &[
                "worktree",
                "remove",
                "--force",
                wt.to_str().expect("utf-8 path"),
            ],
        );
        git(&self.main_wt, &["branch", "-q", "-D", &branch]);
        let _ = try_git(
            &self.main_wt,
            &["update-ref", "-d", &format!("refs/squash/{f}")],
            None,
        );
    }

    /// The client leaves or dies (`AttachmentEnded`, then `LastDetached`):
    /// its write-opens are dropped, an open transaction rolls back (A13), a
    /// sealed one still lands (§3.6, "Rollback, client death"), and the
    /// fork ends.
    pub fn end_client(&mut self, f: ForkId) {
        self.handles.retain(|_, h| h.client != Client::Fork(f));
        if let Some(t) = self.forks[&f].open_txn {
            self.rollback(t, RollbackCause::ClientEnded);
        }
        self.forks.get_mut(&f).expect("fork").life = Life::Ended;
        self.note(format!("end f{f}"));
    }

    /// An actor withdraws F's held run: it and every later commit are
    /// rejected; F lands nothing more (§3.6).
    pub fn withdraw(&mut self, f: ForkId) {
        let fork = self.forks.get_mut(&f).expect("fork");
        assert!(fork.held.is_some(), "withdraw a held run");
        fork.held = None;
        fork.withdrawn = true;
        fork.prep = None;
        self.note(format!("withdraw f{f}"));
    }

    /// An actor's `resolve` of F's held commit (system.md §3.5, Held to
    /// Waiting "or resolve"; landing.md §3 `resolve`: "an actor's merge,
    /// named for that actor"). The actor merges the held commit alone: lines
    /// as the chain does, and where both sides changed a slot, a new line
    /// of its own that saw both. It lands as one record named for the actor;
    /// F's later commits form its next run. Atomic here: the actor's merge
    /// enters `Writing` at once.
    pub fn resolve(&mut self, f: ForkId) {
        let fork = &self.forks[&f];
        assert!(fork.held.is_some(), "resolve a held run");
        let c = fork.commits[fork.landed].clone();
        let target = fork.parent;
        let mut merged = BTreeMap::new();
        for (p, theirs) in &c.files {
            let base = self
                .base_content(&self.base_ref(f, p, c.seq), p)
                .expect("a fork commit's path has a base");
            let ours = self.read(target, p);
            let m = match self.merge(&base, &ours, theirs) {
                Some(m) => m,
                None => {
                    let mut m = ours.clone();
                    for i in 0..SLOTS {
                        let (b, o, t) = (base.slot(i), ours.slot(i), theirs.slot(i));
                        if b == o {
                            m = m.with(i, t);
                        } else if b != t && o != t {
                            let tok = self.token("actor", target);
                            self.lineage.wrote(&tok, o);
                            self.lineage.wrote(&tok, t);
                            m = m.with(i, &tok);
                        }
                    }
                    m
                }
            };
            merged.insert(p.clone(), m);
        }
        self.note(format!("resolve f{f} at its record {}", c.seq));
        let cause = format!("Landing Resolve f{f} at {}", c.seq);
        self.land_run(f, 1, merged, Some(("actor", &cause)));
    }

    /// An operator restores `p` in main to its content at `from` (system.md
    /// §5, "Other sources": `Restore`, writer the operator). Each slot it
    /// changes gets a fresh operator token that saw the one it replaces, so
    /// the lineage stays acyclic. Atomic here (`LandingFirst`).
    pub fn restore(&mut self, p: &str, from: &str) {
        let restored = self.object(&format!("{from}:{p}"));
        let cur = self.read(Tree::Main, p);
        let mut new = cur.clone();
        let mut edits = Vec::new();
        for i in 0..SLOTS {
            if restored.slot(i) != cur.slot(i) {
                let tok = self.token("op", Tree::Main);
                self.lineage.wrote(&tok, cur.slot(i));
                new = new.with(i, &tok);
                edits.push(Edit {
                    path: p.to_string(),
                    slot: i,
                    token: tok,
                });
            }
        }
        self.note(format!("restore main:{p} from {from}"));
        if edits.is_empty() {
            return;
        }
        self.record_main("op", "Restore", one(p, new), None);
        for edit in edits {
            self.landed.push(Landed {
                tree: Tree::Main,
                edit,
            });
        }
    }

    // ── records and commits ──────────────────────────────────────────────

    /// One record in main: in its tree at once, in its tail until the commit
    /// window (history.md §2). Every record of p syncs into the forks that
    /// hold their own p (§3.5, "A sync").
    fn record_main(
        &mut self,
        writer: &str,
        cause: &str,
        files: BTreeMap<String, Content>,
        second: Option<String>,
    ) -> u64 {
        for (p, c) in &files {
            std::fs::write(self.main_wt.join(p), c.render()).expect("write a file");
        }
        self.main_seq += 1;
        let seq = self.main_seq;
        for (p, content) in &files {
            self.make_syncs(Tree::Main, p, writer, content, Origin::MainSeq(seq));
        }
        self.main_tail.push(MainRecord {
            seq,
            writer: writer.to_string(),
            cause: cause.to_string(),
            files,
            second,
        });
        seq
    }

    /// The commit window ticks (history.md §2, commit group: "on main,
    /// consecutive records of one writer and one cause"; a landing record is
    /// a group of one): the tail becomes commits.
    fn step_window(&mut self) -> bool {
        if self.main_tail.is_empty() {
            return false;
        }
        let tail = std::mem::take(&mut self.main_tail);
        let mut groups: Vec<Vec<MainRecord>> = Vec::new();
        for r in tail {
            match groups.last_mut() {
                Some(g)
                    if g[0].second.is_none()
                        && r.second.is_none()
                        && g[0].writer == r.writer
                        && g[0].cause == r.cause =>
                {
                    g.push(r)
                }
                _ => groups.push(vec![r]),
            }
        }
        for g in groups {
            let mut files = BTreeMap::new();
            for r in &g {
                files.extend(r.files.clone());
            }
            let tree = self.tree_with(&self.main_head.clone(), &files);
            let (writer, cause) = (g[0].writer.clone(), g[0].cause.clone());
            let msg = message(&format!("{cause} by {writer}"), &writer, &cause, "main");
            let mut args = vec!["commit-tree".to_string(), tree, "-p".into()];
            args.push(self.main_head.clone());
            if let Some(s) = &g[0].second {
                args.extend(["-p".to_string(), s.clone()]);
            }
            args.extend(["-F".to_string(), "-".to_string()]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let c = git_in(&self.main_wt, &args, &msg);
            git(&self.main_wt, &["update-ref", "refs/heads/main", &c]);
            self.main_head = c.clone();
            self.main_commits.push(c);
            self.main_commit_seq
                .push(g.last().expect("a group has a record").seq);
        }
        self.note("window: main committed its tail".into());
        true
    }

    /// The newest main commit at or below record `seq` (§5: a sync's second
    /// parent).
    fn main_commit_at(&self, seq: u64) -> String {
        let i = self
            .main_commit_seq
            .iter()
            .rposition(|s| *s <= seq)
            .expect("the root is at 0");
        self.main_commits[i].clone()
    }

    /// One record in fork `f`, one commit on its line (history.md H3). A
    /// save or a landed child run is also a fork commit (§3.6); a new one
    /// joins F's waiting run, or re-judges its held one.
    fn record_fork(
        &mut self,
        f: ForkId,
        writer: &str,
        cause: &str,
        files: BTreeMap<String, Content>,
        second: Option<String>,
        fork_commit: Option<Vec<Edit>>,
    ) -> String {
        let seq = {
            let fork = self.forks.get_mut(&f).expect("fork");
            fork.seq += 1;
            fork.seq
        };
        let c = self.commit_fork(f, &files, second.as_deref(), writer, cause);
        if let Some(edits) = fork_commit {
            let made = self.next_ordinal();
            let fork = self.forks.get_mut(&f).expect("fork");
            fork.commits.push(ForkCommit {
                seq,
                writer: writer.to_string(),
                files: files.clone(),
                git: c.clone(),
                edits,
            });
            if !fork.is_txn() && !fork.withdrawn {
                // §3.5: "a run takes the fork's newer commits"; §3.6: "A held
                // run leaves Held at the fork's next commit".
                fork.prep = None;
                if fork.held.take().is_some() {
                    fork.as_one = true;
                    fork.made = made;
                } else if fork.landed + 1 == fork.commits.len() {
                    fork.made = made;
                }
            }
        }
        for (p, content) in &files {
            self.make_syncs(Tree::Fork(f), p, writer, content, Origin::Commit(c.clone()));
            // A held seal is re-judged at its parent's next record in scope.
            for t in self.forks.values_mut() {
                if t.parent == Tree::Fork(f)
                    && t.txn == Some(TxnState::Held)
                    && t.scope.as_ref().is_some_and(|s| s.contains(p))
                {
                    t.rejudge = true;
                }
            }
        }
        c
    }

    /// A sync per editable, non-transaction child holding its own p that
    /// `copies` names (open ones; ended ones too under
    /// [`EndedForkCopies::UntilDropped`]); one open sync per fork and file:
    /// a newer record joins it, so a stale sync never lands (§3.5). No new
    /// sync into a copy that already equals the record (landing.md §2), but
    /// such a record still joins an open one (design finding 5).
    fn make_syncs(&mut self, from: Tree, p: &str, writer: &str, content: &Content, at: Origin) {
        let until_dropped = self.policies.ended_fork_copies == EndedForkCopies::UntilDropped;
        let targets: Vec<ForkId> = self
            .forks
            .values()
            .filter(|f| {
                f.parent == from
                    && !f.is_txn()
                    && (f.life == Life::Open || (until_dropped && f.life == Life::Ended))
                    && f.own.contains(p)
            })
            .map(|f| f.id)
            .filter(|f| {
                self.syncs.contains_key(&(*f, p.to_string()))
                    || self.read(Tree::Fork(*f), p) != *content
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        let blob = self.capture(content);
        for f in targets {
            let made = self.next_ordinal();
            match self.syncs.get_mut(&(f, p.to_string())) {
                Some(s) => {
                    if s.writers.iter().any(|w| w != writer) {
                        self.joined_across_writers += 1;
                    }
                    s.writers.push(writer.to_string());
                    s.content = content.clone();
                    s.blob = blob.clone();
                    s.from = at.clone();
                    s.prep = None;
                }
                None => {
                    self.syncs.insert(
                        (f, p.to_string()),
                        PendingSync {
                            made,
                            writers: vec![writer.to_string()],
                            content: content.clone(),
                            blob: blob.clone(),
                            from: at.clone(),
                            prep: None,
                        },
                    );
                }
            }
        }
    }

    /// A commit on fork `f`'s line, at once (history.md: "A fork: one
    /// commit per fork commit").
    fn commit_fork(
        &mut self,
        f: ForkId,
        files: &BTreeMap<String, Content>,
        second: Option<&str>,
        writer: &str,
        cause: &str,
    ) -> String {
        if self.forks[&f].head.is_none() {
            // A line starts at its parent's head when its first commit is
            // built: the fork point (history.md §1).
            let at = self.head(self.forks[&f].parent);
            let fork = self.forks.get_mut(&f).expect("fork");
            git(&fork.wt, &["reset", "-q", &at]);
            fork.fork_point = Some(at.clone());
            fork.head = Some(at);
        }
        let (wt, name) = {
            let fork = &self.forks[&f];
            (fork.wt.clone(), fork.branch.clone())
        };
        let mut add = vec!["add", "--"];
        for (p, c) in files {
            std::fs::write(wt.join(p), c.render()).expect("write a file");
            add.push(p);
        }
        git(&wt, &add);
        let t = git(&wt, &["write-tree"]);
        let parent = self.head(Tree::Fork(f));
        let msg = message(&format!("{cause} by {writer}"), writer, cause, &name);
        let mut args = vec!["commit-tree", &t, "-p", &parent];
        if let Some(s) = second {
            args.push("-p");
            args.push(s);
        }
        args.extend(["-F", "-"]);
        let c = git_in(&wt, &args, &msg);
        git(&wt, &["update-ref", "HEAD", &c]);
        self.forks.get_mut(&f).expect("fork").head = Some(c.clone());
        c
    }

    /// `parent`'s tree with `files` replaced.
    fn tree_with(&self, parent: &str, files: &BTreeMap<String, Content>) -> String {
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        for line in git(&self.main_wt, &["ls-tree", parent]).lines() {
            if let Some((_, name)) = line.split_once('\t') {
                entries.insert(name.to_string(), line.to_string());
            }
        }
        for (p, c) in files {
            let blob = self.capture(c);
            entries.insert(p.clone(), format!("100644 blob {blob}\t{p}"));
        }
        let listing: String = entries.values().map(|l| format!("{l}\n")).collect();
        git_in(&self.main_wt, &["mktree"], &listing)
    }

    /// Under `Squashed`, a run's second parent: one commit per landing,
    /// holding only the landed paths, on the fork's previous squash commit or
    /// its fork point (history.md §2, "squash chain").
    fn squash_commit(
        &mut self,
        f: ForkId,
        files: &BTreeMap<String, Content>,
        writer: &str,
        through: u64,
    ) -> String {
        let parent = {
            let fork = &self.forks[&f];
            fork.squash_head
                .clone()
                .or_else(|| fork.fork_point.clone())
                .expect("a landed fork has a line")
        };
        let tree = self.tree_with(&parent, files);
        let cause = format!("Squash f{f} through {through}");
        let msg = message(&cause, writer, &cause, &format!("squash/{f}"));
        let c = git_in(
            &self.main_wt,
            &["commit-tree", &tree, "-p", &parent, "-F", "-"],
            &msg,
        );
        git(
            &self.main_wt,
            &["update-ref", &format!("refs/squash/{f}"), &c],
        );
        self.forks.get_mut(&f).expect("fork").squash_head = Some(c.clone());
        c
    }

    // ── the resolver chain (system.md §3.7) ──────────────────────────────

    fn merge(&self, base: &Content, ours: &Content, theirs: &Content) -> Option<Content> {
        for r in &self.policies.resolvers {
            match r {
                Resolver::Exact => {
                    if ours == theirs || base == theirs {
                        return Some(ours.clone());
                    }
                    if base == ours {
                        return Some(theirs.clone());
                    }
                }
                Resolver::Lines => {
                    let (o, b, t) = (
                        self.scratch.join("ours"),
                        self.scratch.join("base"),
                        self.scratch.join("theirs"),
                    );
                    std::fs::write(&o, ours.render()).expect("write");
                    std::fs::write(&b, base.render()).expect("write");
                    std::fs::write(&t, theirs.render()).expect("write");
                    let out = try_git(
                        &self.scratch,
                        &["merge-file", "-p", "ours", "base", "theirs"],
                        None,
                    );
                    if out.code == 0 {
                        return Some(Content::parse(&out.stdout).expect("a merged model file"));
                    }
                }
                Resolver::Structure | Resolver::Agent => {}
            }
        }
        None
    }

    // ── landing steps (system.md §3.5) ───────────────────────────────────

    fn run_waiting(&self, f: &Fork) -> bool {
        !f.is_txn()
            && f.life != Life::Dropped
            && !f.withdrawn
            && f.held.is_none()
            && f.landed < f.commits.len()
    }

    /// Every landing in `Waiting`, with what it lands and which bases it
    /// moves (landing.md §2, "What ahead means").
    fn waiting(&self) -> Vec<Waiting> {
        let mut out = Vec::new();
        let gains = |target: Tree, p: &str| -> Option<(ForkId, String)> {
            match target {
                Tree::Fork(g) if !self.forks[&g].own.contains(p) => Some((g, p.to_string())),
                _ => None,
            }
        };
        for f in self.forks.values() {
            if self.run_waiting(f) {
                let scope: BTreeSet<String> = match &f.prep {
                    Some(p) => p.merged.keys().cloned().collect(),
                    None => f.commits[f.landed..]
                        .iter()
                        .flat_map(|c| c.files.keys().cloned())
                        .collect(),
                };
                let mut moves: BTreeSet<_> = scope.iter().map(|p| (f.id, p.clone())).collect();
                moves.extend(scope.iter().filter_map(|p| gains(f.parent, p)));
                out.push(Waiting {
                    key: Key::Run(f.id),
                    made: f.made,
                    target: f.parent,
                    scope,
                    moves,
                });
            }
            if f.txn == Some(TxnState::Sealed) {
                let scope = f.own.clone();
                let mut moves: BTreeSet<_> = scope.iter().map(|p| (f.id, p.clone())).collect();
                moves.extend(scope.iter().filter_map(|p| gains(f.parent, p)));
                out.push(Waiting {
                    key: Key::Seal(f.id),
                    made: f.made,
                    target: f.parent,
                    scope,
                    moves,
                });
            }
        }
        for ((f, p), s) in &self.syncs {
            out.push(Waiting {
                key: Key::Sync(*f, p.clone()),
                made: s.made,
                target: Tree::Fork(*f),
                scope: BTreeSet::from([p.clone()]),
                moves: BTreeSet::from([(*f, p.clone())]),
            });
        }
        out
    }

    /// landing.md §2, "What ahead means": an earlier waiting landing into
    /// the same tree whose scope overlaps, or one moving the same fork's
    /// base of the same path. A held run or deferred sync is never ahead.
    fn blocked(&self, key: &Key) -> bool {
        let all = self.waiting();
        let Some(me) = all.iter().find(|w| w.key == *key) else {
            return false;
        };
        all.iter().any(|o| {
            o.key != me.key
                && o.made < me.made
                && ((o.target == me.target && !o.scope.is_disjoint(&me.scope))
                    || !o.moves.is_disjoint(&me.moves))
        })
    }

    /// A prepared landing whose bases moved since prepares again on the base
    /// left (§3.5, "One path base, one order").
    fn bases_moved(&self, prep: &Prepared) -> bool {
        prep.bases
            .iter()
            .any(|(f, p, at, b)| self.base_ref(*f, p, *at) != *b)
    }

    /// Every landing step that could act now.
    pub fn pending(&self) -> Vec<Step> {
        let mut out = Vec::new();
        for f in self.forks.values() {
            if f.life == Life::Dropped {
                continue;
            }
            if f.is_txn() {
                match f.txn {
                    Some(TxnState::Sealed) => out.push(Step::Seal(f.id)),
                    Some(TxnState::Held) if f.rejudge => out.push(Step::Seal(f.id)),
                    _ => {}
                }
                continue;
            }
            if self.run_waiting(f) {
                out.push(Step::Run(f.id));
            }
            for p in &f.own {
                if self.trimmable(f.id, p) {
                    out.push(Step::Trim(f.id, p.clone()));
                }
            }
            if self.droppable(f.id) {
                out.push(Step::Drop(f.id));
            }
        }
        for (f, p) in self.syncs.keys() {
            out.push(Step::Sync(*f, p.clone()));
        }
        if !self.main_tail.is_empty() {
            out.push(Step::Window);
        }
        out
    }

    /// Takes one step; `false` if it had to keep waiting.
    pub fn step(&mut self, s: &Step) -> bool {
        match s {
            Step::Run(f) => self.step_run(*f),
            Step::Sync(f, p) => self.step_sync(*f, p),
            Step::Seal(t) => self.step_seal(*t),
            Step::Trim(f, p) => self.step_trim(*f, p),
            Step::Drop(f) => self.step_drop(*f),
            Step::Window => self.step_window(),
        }
    }

    /// Steps every landing until none can act.
    pub fn settle(&mut self) {
        for _ in 0..10_000 {
            let mut any = false;
            for s in self.pending() {
                if self.pending().contains(&s) && self.step(&s) {
                    any = true;
                }
            }
            if !any {
                return;
            }
        }
        panic!("landings never settled");
    }

    fn open_in(&self, t: Tree, p: &str) -> bool {
        self.handles.values().any(|h| h.tree == t && h.path == p)
    }

    /// The commits F's next run may take: its own writer's consecutive
    /// commits, or one commit of another writer alone (§3.5: "A commit of
    /// another writer ... lands alone").
    fn run_end(&self, f: ForkId) -> usize {
        let fork = &self.forks[&f];
        let start = fork.landed;
        if fork.commits[start].writer != fork.writer {
            return start + 1;
        }
        let mut end = start;
        while end < fork.commits.len() && fork.commits[end].writer == fork.writer {
            end += 1;
        }
        end
    }

    /// Prepares F's run against the target as it is (§3.5, A8): each commit
    /// in order merges F's base at its position, what the target shows, and
    /// F's content there, stopping at the first conflict. Re-judged after a
    /// hold, the held commit and the newer ones merge as one change (base:
    /// the held one's; content: the newest; landing.md §2). `cap` limits the
    /// commits (verify's one re-merge). `None`: the first commit conflicts.
    fn prepare_run(&self, f: ForkId, cap: Option<usize>) -> Option<Prepared> {
        let fork = &self.forks[&f];
        let target = fork.parent;
        let start = fork.landed;
        let end = cap.map_or_else(|| self.run_end(f), |n| start + n);
        let mut prep = Prepared::default();
        if fork.as_one {
            let mut first: BTreeMap<String, u64> = BTreeMap::new();
            let mut newest: BTreeMap<String, Content> = BTreeMap::new();
            for c in &fork.commits[start..end] {
                for (p, content) in &c.files {
                    first.entry(p.clone()).or_insert(c.seq);
                    newest.insert(p.clone(), content.clone());
                }
            }
            for (p, theirs) in newest {
                let at = first[&p];
                let b = self.base_ref(f, &p, at);
                let base = self.base_content(&b, &p).expect("a held file has a base");
                let ours = self.read(target, &p);
                let m = self.merge(&base, &ours, &theirs)?;
                prep.tracked.insert(p.clone(), ours);
                prep.bases.push((f, p.clone(), at, b));
                prep.merged.insert(p, m);
            }
            prep.n = end - start;
            return Some(prep);
        }
        for c in &fork.commits[start..end] {
            let mut next = prep.clone();
            let mut ok = true;
            for (p, theirs) in &c.files {
                let b = self.base_ref(f, p, c.seq);
                let base = self
                    .base_content(&b, p)
                    .expect("a fork commit's path has a base");
                let ours = match next.merged.get(p) {
                    Some(m) => m.clone(),
                    None => {
                        let o = self.read(target, p);
                        next.tracked.insert(p.clone(), o.clone());
                        o
                    }
                };
                next.bases.push((f, p.clone(), c.seq, b));
                match self.merge(&base, &ours, theirs) {
                    Some(m) => {
                        next.merged.insert(p.clone(), m);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                if prep.n == 0 {
                    return None;
                }
                prep.held_after = true;
                break;
            }
            next.n += 1;
            prep = next;
        }
        Some(prep)
    }

    fn hold_run(&mut self, f: ForkId, why: &str) {
        let fork = self.forks.get_mut(&f).expect("fork");
        fork.held = Some(fork.landed);
        fork.as_one = false;
        fork.prep = None;
        let at = fork.commits[fork.landed].seq;
        self.note(format!("held f{f} at its record {at} ({why})"));
    }

    /// A fork run (§3.5, §3.6, A8): `Waiting` prepares it; `Writing` waits
    /// for nothing ahead and no write-open on its files in the target
    /// (`ClientsFirst`, §7), verifies, re-merges once if the target moved,
    /// and lands it as ONE record named for its one writer.
    fn step_run(&mut self, f: ForkId) -> bool {
        let Some(prep) = self.forks[&f].prep.clone() else {
            match self.prepare_run(f, None) {
                Some(p) => {
                    self.forks.get_mut(&f).expect("fork").prep = Some(p);
                    self.note(format!("prepared f{f}'s run"));
                }
                None => self.hold_run(f, "conflict"),
            }
            return true;
        };
        let target = self.forks[&f].parent;
        if prep.merged.keys().any(|p| self.open_in(target, p)) || self.blocked(&Key::Run(f)) {
            return false;
        }
        if self.bases_moved(&prep) {
            self.re_merges += 1;
            self.forks.get_mut(&f).expect("fork").prep = None;
            self.note(format!("f{f}'s run: a base moved, prepares again"));
            return true;
        }
        let fresh = prep.tracked.iter().all(|(p, c)| self.read(target, p) == *c);
        let ready = if fresh {
            prep
        } else {
            self.re_merges += 1;
            match self.prepare_run(f, Some(prep.n)) {
                Some(mut r) if r.n == prep.n && !r.held_after => {
                    r.held_after = prep.held_after;
                    r
                }
                _ => {
                    self.hold_run(f, "verify's re-merge conflicts");
                    return true;
                }
            }
        };
        self.land_run(f, ready.n, ready.merged, None);
        if ready.held_after {
            self.hold_run(f, "conflict after the prefix");
        }
        true
    }

    /// `Writing` for a run of F's next `n` commits: one record in the
    /// parent, then F's bases move to its own commit at `through`
    /// (forks.md §3 `Kept { at: F:through }`). `actor`: a resolve's writer
    /// and cause.
    fn land_run(
        &mut self,
        f: ForkId,
        n: usize,
        merged: BTreeMap<String, Content>,
        actor: Option<(&str, &str)>,
    ) {
        let (target, start, last_writer, through, through_git, edits) = {
            let fork = &self.forks[&f];
            let start = fork.landed;
            let last = &fork.commits[start + n - 1];
            // A token the sandbox itself replaced by the target's line
            // (`save_target_lines`), or a later one of its own that saw it
            // and was so replaced, is not part of what lands.
            let edits: Vec<Edit> = fork.commits[start..start + n]
                .iter()
                .flat_map(|c| c.edits.iter().cloned())
                .filter(|e| {
                    !self.accepted.iter().any(|(t, p, i, from, _)| {
                        *t == Tree::Fork(f)
                            && *p == e.path
                            && *i == e.slot
                            && self.lineage.descends(from, &e.token)
                    })
                })
                .collect();
            (
                fork.parent,
                start,
                last.writer.clone(),
                last.seq,
                last.git.clone(),
                edits,
            )
        };
        let (writer, cause) = match actor {
            Some((w, c)) => (w.to_string(), c.to_string()),
            None => (
                last_writer,
                format!("Landing ForkRun f{f} through {through}"),
            ),
        };
        let second = match self.policies.history {
            HistoryPolicy::FullLineage => through_git.clone(),
            HistoryPolicy::Squashed => self.squash_commit(f, &merged, &writer, through),
        };
        match target {
            Tree::Main => {
                self.record_main(&writer, &cause, merged.clone(), Some(second));
            }
            Tree::Fork(p) => {
                for path in merged.keys() {
                    let ours = self.read(target, path);
                    self.gain_copy(p, path, &ours);
                }
                self.record_fork(
                    p,
                    &writer,
                    &cause,
                    merged.clone(),
                    Some(second),
                    Some(edits.clone()),
                );
            }
        }
        for path in merged.keys() {
            self.set_base(
                f,
                path,
                through,
                Base::Kept {
                    commit: through_git.clone(),
                },
            );
        }
        let made = self.next_ordinal();
        let fork = self.forks.get_mut(&f).expect("fork");
        fork.landed = start + n;
        fork.held = None;
        fork.as_one = false;
        fork.prep = None;
        // The rest becomes the fork's next run, waiting from now.
        fork.made = made;
        for edit in edits {
            self.landed.push(Landed { tree: target, edit });
        }
        self.note(format!(
            "landed f{f} run of {n} through {through} into {target:?}"
        ));
    }

    /// A sync (§3.5): prepared from the captured parent record; `Writing`
    /// waits only for F's write-opens of p and what is ahead, verifies F's
    /// copy, re-merges once. A conflict defers it.
    fn step_sync(&mut self, f: ForkId, p: &str) -> bool {
        let key = (f, p.to_string());
        if !self.forks[&f].own.contains(p) || self.forks[&f].life == Life::Dropped {
            self.syncs.remove(&key);
            return true;
        }
        let (content, prep) = {
            let s = &self.syncs[&key];
            (s.content.clone(), s.prep.clone())
        };
        let Some(prep) = prep else {
            let ours = self.read(Tree::Fork(f), p);
            if ours == content {
                self.syncs.remove(&key);
                return true;
            }
            let b = self.base_ref(f, p, u64::MAX);
            let base = self.base_content(&b, p).expect("a held copy has a base");
            match self.merge(&base, &ours, &content) {
                Some(m) => {
                    self.syncs.get_mut(&key).expect("sync").prep = Some(Prepared {
                        tracked: one(p, ours),
                        bases: vec![(f, p.to_string(), u64::MAX, b)],
                        merged: one(p, m),
                        ..Prepared::default()
                    });
                    self.note(format!("prepared sync of {p} into f{f}"));
                }
                None => self.defer(f, p),
            }
            return true;
        };
        if self.open_in(Tree::Fork(f), p) || self.blocked(&Key::Sync(f, p.to_string())) {
            return false;
        }
        if self.bases_moved(&prep) {
            self.re_merges += 1;
            self.syncs.get_mut(&key).expect("sync").prep = None;
            self.note(format!(
                "sync of {p} into f{f}: its base moved, prepares again"
            ));
            return true;
        }
        let ours = self.read(Tree::Fork(f), p);
        let merged = if prep.tracked[p] == ours {
            prep.merged[p].clone()
        } else {
            self.re_merges += 1;
            if ours == content {
                self.syncs.remove(&key);
                return true;
            }
            let base = self
                .base_content(&prep.bases[0].3, p)
                .expect("a held copy has a base");
            match self.merge(&base, &ours, &content) {
                Some(m) => m,
                None => {
                    self.defer(f, p);
                    return true;
                }
            }
        };
        let s = self.syncs.remove(&key).expect("a pending sync");
        let writer = s.writers.last().expect("a writer").clone();
        let second = match &s.from {
            Origin::MainSeq(seq) => self.main_commit_at(*seq),
            Origin::Commit(c) => c.clone(),
        };
        self.record_fork(
            f,
            &writer,
            "Landing Sync",
            one(p, merged),
            Some(second),
            None,
        );
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Base::Captured { blob: s.blob });
        self.note(format!("synced {p} into f{f}, writers {:?}", s.writers));
        true
    }

    fn defer(&mut self, f: ForkId, p: &str) {
        self.syncs.remove(&(f, p.to_string()));
        self.deferred += 1;
        self.note(format!("deferred sync of {p} into f{f}"));
    }

    /// The seal's run merged against F's newest content (`Merge` isolation:
    /// commit after pull). `None`: a conflict.
    fn prepare_seal(&self, t: ForkId) -> Option<Prepared> {
        let f = match self.forks[&t].parent {
            Tree::Fork(f) => f,
            Tree::Main => unreachable!("a transaction fork's parent is a fork"),
        };
        let mut prep = Prepared::default();
        for p in self.forks[&t].own.iter() {
            let b = self.base_ref(t, p, u64::MAX);
            let base = self.base_content(&b, p).expect("a staged file has a base");
            let ours = self.read(Tree::Fork(f), p);
            let theirs = self.read(Tree::Fork(t), p);
            let m = self.merge(&base, &ours, &theirs)?;
            prep.tracked.insert(p.clone(), ours);
            prep.bases.push((t, p.clone(), u64::MAX, b));
            prep.merged.insert(p.clone(), m);
        }
        Some(prep)
    }

    fn seal_conflicts(&mut self, t: ForkId) {
        match self.policies.seal_conflict {
            SealConflict::Held => {
                let fork = self.forks.get_mut(&t).expect("txn");
                fork.txn = Some(TxnState::Held);
                fork.prep = None;
                self.note(format!("seal t{t} held"));
            }
            SealConflict::Rollback => {
                self.note(format!("seal t{t} conflicts: rolled back"));
                self.rollback(t, RollbackCause::SealConflict);
            }
        }
    }

    /// A sealed transaction's run into F (§3.6, "Seal"): prepared once T's
    /// last write-open closes (before that a save could still join it);
    /// `Writing` waits for F's write-opens of its files and what is ahead,
    /// verifies F's files, re-merges once, and lands ONE fork commit in F
    /// named for the client. A held seal re-judged goes back to `Waiting`.
    fn step_seal(&mut self, t: ForkId) -> bool {
        match self.forks[&t].txn {
            Some(TxnState::Held) if self.forks[&t].rejudge => {
                let made = self.next_ordinal();
                let fork = self.forks.get_mut(&t).expect("txn");
                fork.txn = Some(TxnState::Sealed);
                fork.rejudge = false;
                fork.made = made;
                fork.prep = None;
                self.note(format!("seal t{t} re-judged"));
                return true;
            }
            Some(TxnState::Sealed) => {}
            _ => return false,
        }
        if self.handles.values().any(|h| h.tree == Tree::Fork(t)) {
            return false;
        }
        let f = match self.forks[&t].parent {
            Tree::Fork(f) => f,
            Tree::Main => unreachable!("a transaction fork's parent is a fork"),
        };
        let Some(prep) = self.forks[&t].prep.clone() else {
            match self.prepare_seal(t) {
                Some(p) if p.merged.is_empty() => {
                    self.note(format!("seal t{t} staged nothing"));
                    self.drop_txn(t, TxnState::Landed);
                }
                Some(p) => {
                    self.forks.get_mut(&t).expect("txn").prep = Some(p);
                    self.note(format!("prepared seal t{t}"));
                }
                None => self.seal_conflicts(t),
            }
            return true;
        };
        if prep.merged.keys().any(|p| self.open_in(Tree::Fork(f), p)) || self.blocked(&Key::Seal(t))
        {
            return false;
        }
        let fresh = prep
            .tracked
            .iter()
            .all(|(p, c)| self.read(Tree::Fork(f), p) == *c);
        let ready = if fresh {
            prep
        } else {
            self.re_merges += 1;
            match self.prepare_seal(t) {
                Some(p) => p,
                None => {
                    self.seal_conflicts(t);
                    return true;
                }
            }
        };
        let writer = self.forks[&t].writer.clone();
        let edits: Vec<Edit> = self.forks[&t]
            .commits
            .iter()
            .flat_map(|c| c.edits.iter().cloned())
            .collect();
        for (p, ours) in &ready.tracked {
            self.gain_copy(f, p, ours);
        }
        self.record_fork(
            f,
            &writer,
            &format!("Landing TxnRun t{t}"),
            ready.merged,
            None,
            Some(edits.clone()),
        );
        for edit in edits {
            self.landed.push(Landed {
                tree: Tree::Fork(f),
                edit,
            });
        }
        self.note(format!("seal t{t} landed in f{f}"));
        self.drop_txn(t, TxnState::Landed);
        true
    }

    /// Trim (§3.5): a closed copy equal to its parent's goes, with its base;
    /// no record. It waits behind any landing moving that base.
    fn trimmable(&self, f: ForkId, p: &str) -> bool {
        let fork = &self.forks[&f];
        fork.own.contains(p)
            && !self.open_in(Tree::Fork(f), p)
            && !self.syncs.contains_key(&(f, p.to_string()))
            && self.read(Tree::Fork(f), p) == self.read(fork.parent, p)
            && !self
                .waiting()
                .iter()
                .any(|w| w.moves.contains(&(f, p.to_string())))
    }

    fn step_trim(&mut self, f: ForkId, p: &str) -> bool {
        if !self.trimmable(f, p) {
            return false;
        }
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Base::Trimmed);
        self.forks.get_mut(&f).expect("fork").own.remove(p);
        self.note(format!("trim f{f}:{p}"));
        true
    }

    /// A fork drops once it has ended, has no live child, and no landing of
    /// it is open (I5, §5 "A fork ends and is dropped"). A held run stays
    /// open until resolved or withdrawn.
    fn droppable(&self, f: ForkId) -> bool {
        let fork = &self.forks[&f];
        fork.life == Life::Ended
            && !self
                .forks
                .values()
                .any(|c| c.parent == Tree::Fork(f) && c.life != Life::Dropped)
            && !self.syncs.keys().any(|(g, _)| *g == f)
            && (fork.landed == fork.commits.len() || fork.withdrawn)
    }

    fn step_drop(&mut self, f: ForkId) -> bool {
        if !self.droppable(f) {
            return false;
        }
        self.remove_line(f);
        let fork = self.forks.get_mut(&f).expect("fork");
        fork.life = Life::Dropped;
        fork.own.clear();
        self.note(format!("drop f{f}"));
        true
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn read_file(path: &Path) -> Content {
    let text = std::fs::read_to_string(path).expect("read a model file");
    Content::parse(&text).expect("a model file")
}

fn one(p: &str, c: Content) -> BTreeMap<String, Content> {
    BTreeMap::from([(p.to_string(), c)])
}
