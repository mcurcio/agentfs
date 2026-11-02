//! The model: main, forks and transaction forks as git branches with their
//! own worktrees, and the target design's landing rules layered on top.
//!
//! | Design term (system.md) | Here |
//! |---|---|
//! | main's tree and line | the repository's own worktree, branch `main` |
//! | an in-volume fork F and its line | branch `fork/F` and its worktree; files F holds its own copy of live there, the rest read through to the parent |
//! | a save; a fork commit | one commit naming its one writer in an `Agentfs-Writer` trailer |
//! | a fork run (§3.5) | whole fork commits, in order, merged into the parent and committed there as ONE commit |
//! | a sync (§3.5) | the parent's newer record of a file merged into a fork's own copy, committed on the fork's line with the parent's commit as second parent |
//! | a transaction fork T (§3.6) | branch `txn/T` and its worktree; seal lands it in F as one commit; rollback deletes the branch |
//! | the resolver chain (§3.7) | `Exact` here, `Lines` is `git merge-file`; `Structure` and `Agent` are stubs that never merge |
//! | the history setting (§4) | the run commit's second parent: the fork's commit (`FullLineage`) or a squash commit on `refs/squash/F` (`Squashed`) |
//!
//! Every landing step is atomic: prepare, verify and write happen in one
//! step, so the interleavings explored are between whole landings and client
//! calls. The rules each step applies cite their doc section.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, Lineage, PATHS};
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
}

impl Default for Policies {
    /// The docs' current settings (system.md §4): `FullLineage`, a
    /// conflicting seal held, the first three resolvers on.
    fn default() -> Self {
        Self {
            history: HistoryPolicy::FullLineage,
            seal_conflict: SealConflict::Held,
            resolvers: vec![Resolver::Exact, Resolver::Lines, Resolver::Structure],
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

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    /// system.md §3.3 `ForkError::InTransaction`: F has one open already.
    InTransaction,
    NotEditable,
    Ended,
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
    pub edits: Vec<Edit>,
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
    /// Paths it holds its own copy of; the rest read through to the parent.
    pub own: BTreeSet<String>,
    /// Path bases (forks.md §6): per path, entries keyed by (the fork
    /// position after which they apply, wall order). `None` is `Trimmed`.
    bases: BTreeMap<String, BTreeMap<(u64, u64), Option<Content>>>,
    /// Its own copy of each path, at each of its records.
    contents: BTreeMap<String, BTreeMap<u64, Content>>,
    pub seq: u64,
    /// Fork commits, in order. For a transaction fork: its saves.
    pub commits: Vec<ForkCommit>,
    /// How many of `commits` have landed in the parent: always a prefix.
    pub landed: usize,
    /// A held run: `commits.len()` when it was held. The fork's next commit
    /// re-judges it (system.md §3.6).
    pub held: Option<usize>,
    pub withdrawn: bool,
    pub open_txn: Option<ForkId>,
    /// A held seal: the parent recorded a file in scope since (§3.6).
    pub rejudge: bool,
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

struct PendingSync {
    writers: Vec<String>,
    content: Content,
    commit: String,
}

/// A step a landing can take, chosen by a test or the random scheduler.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Run(ForkId),
    Sync(ForkId, String),
    Seal(ForkId),
    Trim(ForkId, String),
    Drop(ForkId),
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
    pub main_head: String,
    pub main_commits: Vec<String>,
    pub forks: BTreeMap<ForkId, Fork>,
    next_fork: ForkId,
    pub lineage: Lineage,
    counters: BTreeMap<String, u32>,
    handles: BTreeMap<HandleId, Handle>,
    next_handle: HandleId,
    ordinal: u64,
    syncs: BTreeMap<(ForkId, String), PendingSync>,
    /// Syncs that met a conflict (`Deferred`).
    pub deferred: usize,
    /// Syncs that joined a newer record of another writer.
    pub joined_across_writers: usize,
    pub log: Vec<String>,
    pub rolled_back: BTreeSet<String>,
    pub landed: Vec<Landed>,
    /// The causal oracle's last view of each tree (`check`).
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
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
            forks: BTreeMap::new(),
            next_fork: 1,
            lineage: Lineage::default(),
            counters: BTreeMap::new(),
            handles: BTreeMap::new(),
            next_handle: 1,
            ordinal: 0,
            syncs: BTreeMap::new(),
            deferred: 0,
            joined_across_writers: 0,
            log: Vec::new(),
            rolled_back: BTreeSet::new(),
            landed: Vec::new(),
            views: BTreeMap::new(),
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

    /// F's base of p for its record at `at` (forks.md §6: per position; the
    /// only merge base, never git's: system.md I4).
    fn base(&self, f: ForkId, p: &str, at: u64) -> Option<Content> {
        self.forks[&f]
            .bases
            .get(p)?
            .range(..(at, 0))
            .next_back()
            .and_then(|(_, b)| b.clone())
    }

    fn latest_base(&self, f: ForkId, p: &str) -> Option<Content> {
        self.base(f, p, u64::MAX)
    }

    fn set_base(&mut self, f: ForkId, p: &str, after: u64, b: Option<Content>) {
        self.ordinal += 1;
        let o = self.ordinal;
        self.forks
            .get_mut(&f)
            .expect("fork")
            .bases
            .entry(p.to_string())
            .or_default()
            .insert((after, o), b);
    }

    fn content_at(&self, f: ForkId, p: &str, at: u64) -> Content {
        self.forks[&f]
            .contents
            .get(p)
            .and_then(|m| m.range(..=at).next_back())
            .map(|(_, c)| c.clone())
            .expect("a fork commit's path has a recorded content")
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
                scope,
                own: BTreeSet::new(),
                bases: BTreeMap::new(),
                contents: BTreeMap::new(),
                seq: 0,
                commits: Vec::new(),
                landed: 0,
                held: None,
                withdrawn: false,
                open_txn: None,
                rejudge: false,
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
    /// showed (forks.md §6, "A fork's first write of p").
    fn copy_up(&mut self, f: ForkId, p: &str) {
        if self.forks[&f].own.contains(p) {
            return;
        }
        let parent = self.forks[&f].parent;
        let base = self.read(parent, p);
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Some(base));
        self.forks
            .get_mut(&f)
            .expect("fork")
            .own
            .insert(p.to_string());
    }

    /// A copy a landing gives F takes as its base the content the merge read
    /// as ours (forks.md §6, "Transaction T over F": `Synced`).
    fn gain_copy(&mut self, f: ForkId, p: &str, ours: &Content) {
        if self.forks[&f].own.contains(p) {
            return;
        }
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, Some(ours.clone()));
        self.forks
            .get_mut(&f)
            .expect("fork")
            .own
            .insert(p.to_string());
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

    /// `Forks::seal`: T ends `Sealed`; new write-opens go to F. Its run lands
    /// at a later step, once its last write-open closes.
    pub fn seal(&mut self, t: ForkId) {
        let fork = self.forks.get_mut(&t).expect("txn");
        assert_eq!(fork.txn, Some(TxnState::Open), "seal an open transaction");
        fork.txn = Some(TxnState::Sealed);
        let parent = fork.parent;
        if let Tree::Fork(f) = parent {
            self.forks.get_mut(&f).expect("fork").open_txn = None;
        }
        self.note(format!("seal t{t}"));
    }

    /// `Forks::abandon`: the rollback (A13). T drops with its layer, bases
    /// and stream; F's files never changed.
    pub fn abandon(&mut self, t: ForkId) {
        self.rollback(t);
    }

    /// A held seal's `withdraw` rolls it back (§3.6, "Seal, held").
    pub fn withdraw_seal(&mut self, t: ForkId) {
        assert_eq!(self.forks[&t].txn, Some(TxnState::Held));
        self.rollback(t);
    }

    pub fn txn_state(&self, t: ForkId) -> TxnState {
        self.forks[&t].txn.expect("a transaction fork")
    }

    fn rollback(&mut self, t: ForkId) {
        let tokens: Vec<String> = self.forks[&t]
            .commits
            .iter()
            .flat_map(|c| c.edits.iter().map(|e| e.token.clone()))
            .collect();
        self.rolled_back.extend(tokens);
        self.handles.retain(|_, h| h.tree != Tree::Fork(t));
        self.note(format!("rollback t{t}"));
        self.drop_txn(t, TxnState::RolledBack);
    }

    fn drop_txn(&mut self, t: ForkId, state: TxnState) {
        self.remove_line(t);
        let fork = self.forks.get_mut(&t).expect("txn");
        fork.txn = Some(state);
        fork.life = Life::Dropped;
        fork.own.clear();
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
            self.rollback(t);
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
        self.note(format!("withdraw f{f}"));
    }

    /// An operator restores `p` in main to its content at `from` (system.md
    /// §5, "Other sources": `Restore`, writer the operator).
    pub fn restore(&mut self, p: &str, from: &str) {
        let text = git(&self.main_wt, &["show", &format!("{from}:{p}")]);
        let restored = Content::parse(&text).expect("a model file");
        let cur = self.read(Tree::Main, p);
        for i in 0..crate::content::SLOTS {
            // The operator saw what it replaced.
            self.lineage.wrote(restored.slot(i), cur.slot(i));
        }
        self.note(format!("restore main:{p} from {from}"));
        self.record_main("op", "Restore", one(p, restored), None);
    }

    // ── records and commits ──────────────────────────────────────────────

    /// One record in main is one commit (main's commit grouping is not
    /// modelled: each group is one record). Every record of p syncs into the
    /// forks that hold their own p (§3.5, "A sync").
    fn record_main(
        &mut self,
        writer: &str,
        cause: &str,
        files: BTreeMap<String, Content>,
        second: Option<String>,
    ) -> String {
        let c = self.commit_files(Tree::Main, &files, second.as_deref(), writer, cause);
        self.main_commits.push(c.clone());
        for (p, content) in &files {
            self.make_syncs(Tree::Main, p, writer, content, &c);
        }
        c
    }

    /// One record in fork `f`, one commit on its line (history.md H3). A
    /// save or a landed child run is also a fork commit (§3.6).
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
        let c = self.commit_files(Tree::Fork(f), &files, second.as_deref(), writer, cause);
        let fork = self.forks.get_mut(&f).expect("fork");
        for (p, content) in &files {
            fork.contents
                .entry(p.clone())
                .or_default()
                .insert(seq, content.clone());
        }
        if let Some(edits) = fork_commit {
            fork.commits.push(ForkCommit {
                seq,
                writer: writer.to_string(),
                files: files.clone(),
                git: c.clone(),
                edits,
            });
        }
        for (p, content) in &files {
            self.make_syncs(Tree::Fork(f), p, writer, content, &c);
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

    /// A sync per open, editable, non-transaction child holding its own p;
    /// one open sync per fork and file: a newer record joins it, so a stale
    /// sync never lands (§3.5). No new sync into a copy that already equals
    /// the record (landing.md §6: "makes no sync into F where F's copy
    /// equals it"), but such a record still supersedes an open one.
    fn make_syncs(&mut self, from: Tree, p: &str, writer: &str, content: &Content, commit: &str) {
        let targets: Vec<ForkId> = self
            .forks
            .values()
            .filter(|f| {
                f.parent == from && !f.is_txn() && f.life == Life::Open && f.own.contains(p)
            })
            .map(|f| f.id)
            .filter(|f| {
                self.syncs.contains_key(&(*f, p.to_string()))
                    || self.read(Tree::Fork(*f), p) != *content
            })
            .collect();
        for f in targets {
            match self.syncs.get_mut(&(f, p.to_string())) {
                Some(s) => {
                    if s.writers.iter().any(|w| w != writer) {
                        self.joined_across_writers += 1;
                    }
                    s.writers.push(writer.to_string());
                    s.content = content.clone();
                    s.commit = commit.to_string();
                }
                None => {
                    self.syncs.insert(
                        (f, p.to_string()),
                        PendingSync {
                            writers: vec![writer.to_string()],
                            content: content.clone(),
                            commit: commit.to_string(),
                        },
                    );
                }
            }
        }
    }

    fn commit_files(
        &mut self,
        tree: Tree,
        files: &BTreeMap<String, Content>,
        second: Option<&str>,
        writer: &str,
        cause: &str,
    ) -> String {
        let (wt, name) = match tree {
            Tree::Main => (self.main_wt.clone(), "main".to_string()),
            Tree::Fork(f) => {
                if self.forks[&f].head.is_none() {
                    // A line starts at its parent's head when its first commit
                    // is built: the fork point (history.md §1).
                    let at = self.head(self.forks[&f].parent);
                    let fork = self.forks.get_mut(&f).expect("fork");
                    git(&fork.wt, &["reset", "-q", &at]);
                    fork.fork_point = Some(at.clone());
                    fork.head = Some(at);
                }
                let fork = &self.forks[&f];
                (fork.wt.clone(), fork.branch.clone())
            }
        };
        let mut add = vec!["add", "--"];
        for (p, c) in files {
            std::fs::write(wt.join(p), c.render()).expect("write a file");
            add.push(p);
        }
        git(&wt, &add);
        let t = git(&wt, &["write-tree"]);
        let parent = self.head(tree);
        let msg = message(&format!("{cause} by {writer}"), writer, cause, &name);
        let mut args = vec!["commit-tree", &t, "-p", &parent];
        if let Some(s) = second {
            args.push("-p");
            args.push(s);
        }
        args.extend(["-F", "-"]);
        let c = git_in(&wt, &args, &msg);
        git(&wt, &["update-ref", "HEAD", &c]);
        match tree {
            Tree::Main => self.main_head = c.clone(),
            Tree::Fork(f) => self.forks.get_mut(&f).expect("fork").head = Some(c.clone()),
        }
        c
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
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        for line in git(&self.main_wt, &["ls-tree", &parent]).lines() {
            if let Some((_, name)) = line.split_once('\t') {
                entries.insert(name.to_string(), line.to_string());
            }
        }
        for (p, c) in files {
            let blob = git_in(
                &self.main_wt,
                &["hash-object", "-w", "--stdin"],
                &c.render(),
            );
            entries.insert(p.clone(), format!("100644 blob {blob}\t{p}"));
        }
        let listing: String = entries.values().map(|l| format!("{l}\n")).collect();
        let tree = git_in(&self.main_wt, &["mktree"], &listing);
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
            if !f.withdrawn
                && f.landed < f.commits.len()
                && f.held.is_none_or(|h| f.commits.len() > h)
            {
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

    /// A fork run (§3.5, §3.6, A8): the longest prefix of F's unlanded
    /// commits that merges and is admitted, landed as ONE record in the
    /// parent named for their one writer. A commit of another writer lands
    /// alone. `ClientsFirst` (§7): a commit touching a file open for writing
    /// in the target waits. A conflict holds the run; the fork's next commit
    /// re-judges it.
    fn step_run(&mut self, f: ForkId) -> bool {
        let (target, start, len) = {
            let fork = &self.forks[&f];
            (fork.parent, fork.landed, fork.commits.len())
        };
        let first_writer = self.forks[&f].commits[start].writer.clone();
        let mut scratch: BTreeMap<String, Content> = BTreeMap::new();
        let mut n = 0;
        let mut conflict = false;
        for k in start..len {
            let c = self.forks[&f].commits[k].clone();
            if c.writer != first_writer {
                break;
            }
            if c.files.keys().any(|p| self.open_in(target, p)) {
                break;
            }
            let mut tmp = scratch.clone();
            let mut ok = true;
            for (p, theirs) in &c.files {
                let base = self
                    .base(f, p, c.seq)
                    .expect("a fork commit's path has a base");
                let ours = tmp.get(p).cloned().unwrap_or_else(|| self.read(target, p));
                match self.merge(&base, &ours, theirs) {
                    Some(m) => {
                        tmp.insert(p.clone(), m);
                    }
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                conflict = true;
                break;
            }
            scratch = tmp;
            n += 1;
        }
        if n > 0 {
            self.land_run(f, start, n, scratch);
        }
        if conflict {
            let fork = self.forks.get_mut(&f).expect("fork");
            fork.held = Some(fork.commits.len());
            let at = fork.commits[fork.landed].seq;
            self.note(format!("held f{f} at its record {at}"));
        }
        n > 0 || conflict
    }

    fn land_run(&mut self, f: ForkId, start: usize, n: usize, merged: BTreeMap<String, Content>) {
        let (target, writer, through, through_git, edits) = {
            let fork = &self.forks[&f];
            let last = &fork.commits[start + n - 1];
            let edits: Vec<Edit> = fork.commits[start..start + n]
                .iter()
                .flat_map(|c| c.edits.iter().cloned())
                .collect();
            (
                fork.parent,
                last.writer.clone(),
                last.seq,
                last.git.clone(),
                edits,
            )
        };
        let second = match self.policies.history {
            HistoryPolicy::FullLineage => through_git,
            HistoryPolicy::Squashed => self.squash_commit(f, &merged, &writer, through),
        };
        let cause = format!("Landing ForkRun f{f} through {through}");
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
        // Bases move to the fork's content at the run's `through`
        // (forks.md §6: `Kept { at: F:through }`).
        for path in merged.keys() {
            let kept = self.content_at(f, path, through);
            self.set_base(f, path, through, Some(kept));
        }
        self.forks.get_mut(&f).expect("fork").landed += n;
        for edit in edits {
            self.landed.push(Landed { tree: target, edit });
        }
        self.note(format!(
            "landed f{f} run of {n} through {through} into {target:?}"
        ));
    }

    /// A sync (§3.5): the parent's newer record of p merged into F's own
    /// copy, waiting only for F's write-opens of p. A conflict defers it.
    fn step_sync(&mut self, f: ForkId, p: &str) -> bool {
        if self.open_in(Tree::Fork(f), p) {
            return false;
        }
        let key = (f, p.to_string());
        let s = self.syncs.remove(&key).expect("a pending sync");
        if !self.forks[&f].own.contains(p) || self.forks[&f].life == Life::Dropped {
            return true;
        }
        let base = self.latest_base(f, p).expect("a held copy has a base");
        let ours = self.read(Tree::Fork(f), p);
        if ours == s.content {
            return true;
        }
        match self.merge(&base, &ours, &s.content) {
            Some(m) => {
                let writer = s.writers.last().expect("a writer").clone();
                self.record_fork(f, &writer, "Landing Sync", one(p, m), Some(s.commit), None);
                let seq = self.forks[&f].seq;
                self.set_base(f, p, seq, Some(s.content));
                self.note(format!("synced {p} into f{f}, writers {:?}", s.writers));
            }
            None => {
                self.deferred += 1;
                self.note(format!("deferred sync of {p} into f{f}"));
            }
        }
        true
    }

    /// A sealed transaction's run into F (§3.6, "Seal"): it waits for T's
    /// last write-open and for F's write-opens of its files; it merges
    /// against F's newest content (`Merge` isolation: commit after pull) and
    /// lands as ONE fork commit in F named for the client.
    fn step_seal(&mut self, t: ForkId) -> bool {
        if self.handles.values().any(|h| h.tree == Tree::Fork(t)) {
            return false;
        }
        let f = match self.forks[&t].parent {
            Tree::Fork(f) => f,
            Tree::Main => unreachable!("a transaction fork's parent is a fork"),
        };
        let paths: Vec<String> = self.forks[&t].own.iter().cloned().collect();
        if paths.iter().any(|p| self.open_in(Tree::Fork(f), p)) {
            return false;
        }
        self.forks.get_mut(&t).expect("txn").rejudge = false;
        let mut merged = BTreeMap::new();
        let mut ours_of = BTreeMap::new();
        for p in &paths {
            let base = self.latest_base(t, p).expect("a staged file has a base");
            let ours = self.read(Tree::Fork(f), p);
            let theirs = self.read(Tree::Fork(t), p);
            match self.merge(&base, &ours, &theirs) {
                Some(m) => {
                    merged.insert(p.clone(), m);
                    ours_of.insert(p.clone(), ours);
                }
                None => {
                    match self.policies.seal_conflict {
                        SealConflict::Held => {
                            self.forks.get_mut(&t).expect("txn").txn = Some(TxnState::Held);
                            self.note(format!("seal t{t} held"));
                        }
                        SealConflict::Rollback => {
                            self.note(format!("seal t{t} conflicts: rolled back"));
                            self.rollback(t);
                        }
                    }
                    return true;
                }
            }
        }
        if merged.is_empty() {
            self.drop_txn(t, TxnState::Landed);
            return true;
        }
        let writer = self.forks[&t].writer.clone();
        let edits: Vec<Edit> = self.forks[&t]
            .commits
            .iter()
            .flat_map(|c| c.edits.iter().cloned())
            .collect();
        for (p, ours) in &ours_of {
            self.gain_copy(f, p, ours);
        }
        self.record_fork(
            f,
            &writer,
            &format!("Landing TxnRun t{t}"),
            merged,
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
    /// no record.
    fn trimmable(&self, f: ForkId, p: &str) -> bool {
        let fork = &self.forks[&f];
        fork.own.contains(p)
            && !self.open_in(Tree::Fork(f), p)
            && !self.syncs.contains_key(&(f, p.to_string()))
            && self.read(Tree::Fork(f), p) == self.read(fork.parent, p)
    }

    fn step_trim(&mut self, f: ForkId, p: &str) -> bool {
        if !self.trimmable(f, p) {
            return false;
        }
        let seq = self.forks[&f].seq;
        self.set_base(f, p, seq, None);
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
