//! The model: main and each fork as a git branch, each save a commit naming
//! one writer, and the owner's rules on top (MODEL.md).
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | a close ([`World::close`]) is never refused; its save is a commit on the writer's tree at once, its first parent the tree's head at that moment |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`]s per tree; a [`Step::Next`] takes its head, or records that it waits (rule 9) |
//! | 3. The parent's queue decides | `land` (a fork's commit into its parent) is the only place a collision with the parent is decided, through `resolve`: per line, a line written over the parent's replaces it, a stale one is dropped, and of two concurrent lines the parent's wins ([`MergeRule`]). `sync` decides nothing: it takes the parent's version and keeps only the fork's lines the parent's queue has not taken yet |
//! | 4. Nothing waits behind a collision | there is no held state: a collision is recorded as a [`Collision`], the fork's losing commit kept under `refs/collisions/<n>`, which Replication later pushes to the shared remote ([`Step::Replicate`]), and the queue moves on |
//! | 5. Transactions | [`World::begin`]: a branch `txn/<n>` frozen at the fork's head; [`World::commit_txn`] lands it in the fork like a child's commit (the fork is its parent, so the fork's lines win) and answers with the overwritten lines; [`World::rollback`] drops it |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a sync is a merge with the parent's commit as second parent |
//! | 7. History setting | [`HistoryPolicy`]: a landing is a merge with the fork's commit as second parent (or a fast-forward), or one single-parent commit, carrying a fork's pending saves as one changeset, whose carried fork commit is kept beside history as a `git replace` graft |
//! | 8. A save records the write calls | [`World::write`] keeps each call on the open file; [`World::close`] replays them on the tree's head then: an append fills the slot after the file's end as it is at close (never a written slot, another writer's append included), a write at an offset replaces that slot, a [`Write::Rewrite`] replaces the whole file |
//! | 9. A take locks the files it overwrites | [`World::open_write`]; a [`Step::Next`] whose sync or landing would overwrite a file open for writing in that tree waits ([`World::waits`]) until the close; then it holds its files ([`World::holds`]) until [`Step::Finish`] writes them; a write-open of a held file waits until then; a client's death ([`World::end_client`], [`World::end_main_client`]) closes its files |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git history.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, EMPTY, Layout, Lineage, PATHS, SLOTS, bytes};
use crate::git::{git, git_in, try_git};

pub type ForkId = u32;
pub type TxnId = u32;
pub type HandleId = u32;

/// A tree: main's, or a fork's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tree {
    Main,
    Fork(ForkId),
}

/// Rule 7: one history setting per volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryPolicy {
    /// A landing is a fast-forward or a merge with the fork's commit as
    /// second parent: main's ancestry reaches every fork commit. Each fork
    /// commit lands on its own.
    FullLineage,
    /// A landing is one single-parent commit: main's history holds only
    /// main's own commits. A fork's plain saves still waiting in the
    /// parent's queue when it takes the first land together as one
    /// changeset; a transaction's commit (or a nested landing) lands on its
    /// own. The last fork commit it carried is recorded beside history, as
    /// a graft (`git replace --graft <commit> <parent> <fork commit>` under
    /// `refs/replace/`), which git's ancestry and blame read and
    /// `--no-replace-objects` (main's history as stored and pushed) does
    /// not. Without it a sync cannot tell that the parent's queue has
    /// taken a fork's line (coherence finding 2, resolved).
    Squashed,
}

/// The design choice under rule 3: how much of the losing side a
/// same-line collision costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionScope {
    /// Only the colliding lines: the incoming change's other lines in that
    /// file still apply. The recommendation.
    Line,
    /// The whole file: the parent's version of a colliding file wins whole,
    /// and the incoming change's other lines in it are dropped too.
    File,
}

/// The design choice under rules 2 and 3: how a sync builds the fork's
/// new version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRule {
    /// Per line, from git alone. A sync takes the parent's current version
    /// and keeps each fork line whose writing commit (`git blame`) the
    /// parent's lineage does not reach yet: a line still waiting for the
    /// parent's queue to decide it. The model's choice.
    PerLine,
    /// git's own three-way merge of the parent's current version into the
    /// fork (`git merge-tree -X ours`, git's merge base): a conflicting
    /// hunk keeps the fork's side, since a fork never decides a collision.
    /// Crossing syncs and landings give two merge bases, so the fork keeps
    /// a line the parent already decided against (coherence finding 1); a
    /// hunk spans neighbouring lines (coherence finding 6).
    GitMergeBase,
}

#[derive(Clone, Copy, Debug)]
pub struct Policies {
    pub history: HistoryPolicy,
    pub collision: CollisionScope,
    pub merge: MergeRule,
    pub layout: Layout,
}

impl Policies {
    /// The model's choices under `history` and `collision`, slots on
    /// separate hunks.
    pub fn new(history: HistoryPolicy, collision: CollisionScope) -> Self {
        Self {
            history,
            collision,
            merge: MergeRule::PerLine,
            layout: Layout::Separated,
        }
    }
}

impl Default for Policies {
    fn default() -> Self {
        Self::new(HistoryPolicy::FullLineage, CollisionScope::Line)
    }
}

/// A program writing through a view: one of main's clients (by writer
/// name), or the one client of a fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Client {
    Main(String),
    Fork(ForkId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Life {
    Open,
    /// Its client left; it drops once its commits have landed and it has
    /// no live child.
    Ended,
    Dropped,
}

/// Rule 5: an open transaction, a branch frozen at the fork's head.
#[derive(Clone, Debug)]
pub struct Txn {
    pub id: TxnId,
    pub start: String,
    pub head: String,
    /// What it staged, in order.
    pub staged: Vec<Edit>,
}

pub struct Fork {
    pub id: ForkId,
    pub parent: Tree,
    /// Its one client's writer name.
    pub writer: String,
    pub life: Life,
    pub head: String,
    pub txn: Option<Txn>,
}

/// A queued change into a tree (rule 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A child fork's commit, to land in this tree.
    Landing { from: ForkId, commit: String },
    /// The parent moved: bring its current decided version into this fork
    /// (a sync carries the parent's current version, never a stale one).
    Sync,
}

/// Where a collision was decided: always a parent's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A fork's commit into its parent.
    Landing,
    /// A transaction's branch into its fork, at commit: the fork is the
    /// transaction's parent.
    Transaction,
}

/// Rule 3's event: a same-line collision, which the parent's version won.
#[derive(Clone, Debug)]
pub struct Collision {
    /// The tree that decided it: the parent the incoming change landed in.
    pub tree: Tree,
    pub direction: Direction,
    pub path: String,
    /// The slots the incoming change lost.
    pub slots: Vec<usize>,
    /// The incoming tokens on those slots, which lost.
    pub lost: Vec<String>,
    /// The tree's tokens that won (it kept them), slot for slot.
    pub won: Vec<String>,
    /// The tree's head before: the parent's version, which won.
    pub winner: String,
    /// The incoming commit, which lost: the fork's commit (the last of a
    /// squashed run) or the transaction's branch. Kept reachable by
    /// `reference`.
    pub loser: String,
    /// The tree's commit after the landing.
    pub result: String,
    pub reference: String,
}

/// How a transaction ended (rule 5), with what it staged: `check` holds
/// every staged edit to its end (in the fork at commit, reported as
/// overwritten, or discarded).
#[derive(Clone, Debug)]
pub struct Ended {
    pub fork: ForkId,
    pub txn: TxnId,
    pub staged: Vec<Edit>,
    pub how: TxnEnd,
    /// The fork's commit that carries it, when committed.
    pub result: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnEnd {
    Commit(TxnOutcome),
    /// Rollback, abandon, or the client's death.
    Rollback,
}

/// What a transaction's commit tells the program (rule 5). No collision
/// refuses a commit (only the program's own open files do:
/// [`ModelError::OpenFiles`]). It lands in the fork as a child's change does in its
/// parent, so the fork's own concurrent lines win; the answer names every
/// staged line that lost, so the program is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    Committed {
        overwritten: Vec<Overwritten>,
    },
    /// It staged nothing.
    Empty,
}

impl TxnOutcome {
    /// A commit in which every staged line applied.
    pub fn clean() -> Self {
        Self::Committed {
            overwritten: Vec::new(),
        }
    }
}

/// A staged line a transaction's commit did not apply: the fork's
/// concurrent line `kept` won it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overwritten {
    pub path: String,
    pub slot: usize,
    pub staged: String,
    pub kept: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The fork has one open already.
    InTransaction,
    NoTransaction,
    Ended,
    /// Commit while the program holds open for writing a file of the
    /// transaction, or one of the fork the commit would write (a choice).
    OpenFiles,
    /// An append with no room left in the model file (a model file has a
    /// fixed number of lines), after this open's calls and every other
    /// open's pending appends to it.
    FileFull,
}

/// A step the scheduler can take: each completes at once (a [`Step::Next`]
/// may record that its head waits).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Take the head of a tree's queue: hold the files it overwrites, or
    /// wait for the write-opens of them in that tree to close (rule 9).
    Next(Tree),
    /// Write the change a tree's [`Step::Next`] holds its files for, then
    /// release them.
    Finish(Tree),
    Drop(ForkId),
    /// Replication pushes to the shared remote: due once a collision is
    /// recorded since its last push (the event it waits for), taken in any
    /// order with the queue steps, so the push is eventual and never part
    /// of a referee's step.
    Replicate,
}

/// One slot a commit changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub slot: usize,
    pub token: String,
}

/// Rule 8: one write call on an open file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Adds a line at the file's end as it is at the save (`O_APPEND`: the
    /// kernel seeks to the end and writes in one step), so it never writes
    /// over a line, another writer's concurrent append included.
    Append,
    /// Replaces exactly that slot.
    At(usize),
    /// Truncate, then write this whole content: every slot is the
    /// program's, a slot that differs from the tree's a new line by it.
    Rewrite(Content),
}

/// One write call as the open file keeps it until close (rule 8): an
/// append's or an offset write's new line, or a rewrite's content.
#[derive(Clone, Debug)]
enum Call {
    Append(String),
    At(usize, String),
    Rewrite(Content),
}

/// What a save leaves on one slot it wrote: a write call's new line, or
/// the bytes a rewrite copied there.
#[derive(Clone, Debug)]
enum Mark {
    Own(String),
    Copied(String),
}

/// Where a write-open writes: its tree, or its client's open transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Tree(Tree),
    Txn(ForkId, TxnId),
}

/// A file a client has open for writing (rule 8), or waits to (rule 9).
/// Its write calls stay private to it until close (close-to-open, a
/// choice: git-model.md §3).
#[derive(Clone, Debug)]
pub struct Handle {
    pub id: HandleId,
    pub client: Client,
    pub target: Target,
    pub path: String,
    /// The open waits for a take's hold on the file to be released.
    pub waiting: bool,
    /// Its write calls, in order, applied to the head at close.
    calls: Vec<Call>,
}

/// What a tree's take holds its files for (rule 9).
#[derive(Clone, Debug)]
pub enum Take {
    /// The parent's head when the take began, the tree it plans, and
    /// whether a commit of the fork was then on its way to the parent.
    Sync {
        up: String,
        tree: Option<String>,
        landing: bool,
    },
    /// A child's commits (a squashed run, or one) and the planned merge.
    Landing {
        from: ForkId,
        run: Vec<String>,
        merged: Merged,
    },
}

/// Rule 9: an incoming change taking the files it will overwrite in a tree.
#[derive(Clone, Debug)]
pub struct Hold {
    pub files: BTreeSet<String>,
    pub take: Take,
    /// The tree's head its plan was made on.
    ours: String,
    /// `entered.len()` when it began: what the parent had decided then.
    entered: usize,
}

/// One landing's result and its same-line collisions.
#[derive(Clone, Debug)]
pub struct Merged {
    tree: String,
    /// The files the result changes.
    files: BTreeSet<String>,
    collisions: Vec<Lost>,
}

/// One file's incoming lines lost in a landing: the slots, the incoming
/// tokens there, and the tree's tokens that won them, slot for slot.
#[derive(Clone, Debug)]
struct Lost {
    path: String,
    slots: Vec<usize>,
    lost: Vec<String>,
    won: Vec<String>,
}

/// One line a landing sets.
struct Incoming {
    path: String,
    slot: usize,
    token: String,
    /// The commit that wrote `token` (`git blame`).
    origin: String,
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

pub struct World {
    pub policies: Policies,
    root: PathBuf,
    repo: PathBuf,
    /// The shared remote (a bare repository): Replication pushes main, the
    /// grafts and every collision ref there.
    remote: PathBuf,
    pub main_head: String,
    pub forks: BTreeMap<ForkId, Fork>,
    next_fork: ForkId,
    next_txn: TxnId,
    queues: BTreeMap<Tree, VecDeque<Entry>>,
    handles: BTreeMap<HandleId, Handle>,
    next_handle: HandleId,
    holds: BTreeMap<Tree, Hold>,
    /// A queue whose head waits for these files' write-opens to close.
    waits: BTreeMap<Tree, BTreeSet<String>>,
    pub lineage: Lineage,
    counters: BTreeMap<String, u32>,
    pub collisions: Vec<Collision>,
    /// Every token that entered a tree: written in it, carried into it by a
    /// landing or a transaction (whether it won or lost there), or taken
    /// from the parent by a sync.
    pub entered: Vec<(Tree, Edit)>,
    /// Every sync that moved a fork: (the fork, the parent's head it
    /// carried, the fork's head after, `entered.len()` when it began).
    pub syncs: Vec<(Tree, String, String, usize)>,
    /// Every landing taken: (target, the commit landed), each commit of a
    /// squashed changeset included.
    pub landings: Vec<(Tree, String)>,
    /// Landings that were fast-forwards.
    pub fast_forwards: usize,
    /// Landings that carried more than one fork commit as one changeset.
    pub squashed_runs: usize,
    /// Tokens of rolled-back transactions.
    pub discarded: BTreeSet<String>,
    /// Every line an append wrote, recorded at the write call, before the
    /// close places it: `check` holds that each landed on an unwritten slot.
    pub appended: BTreeSet<String>,
    /// Main's clients that ended (rule 9: their end event released their
    /// locks).
    pub ended_main: BTreeSet<String>,
    pub ended: Vec<Ended>,
    pub log: Vec<String>,
    objects: RefCell<BTreeMap<String, Content>>,
    blames: RefCell<BTreeMap<String, Vec<String>>>,
    ancestry: RefCell<BTreeMap<(String, String), bool>>,
    over: RefCell<BTreeMap<String, Vec<String>>>,
    /// `check`'s state: its last view of each tree and caches over
    /// immutable git objects.
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
    /// Every (tree, file, slot, token) a tree has shown and then replaced.
    pub(crate) left: BTreeSet<(Tree, String, usize, String)>,
    pub(crate) checked_commits: BTreeSet<String>,
    /// Per head: the commits its stored history reaches, and those its
    /// lineage (history plus grafts) reaches.
    pub(crate) reach: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    /// Main's stored history, checked for `Squashed`.
    pub(crate) main_history: BTreeSet<String>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) collisions_checked: usize,
    /// A collision was recorded since Replication last pushed.
    replication_due: bool,
    pub(crate) syncs_checked: usize,
    pub(crate) ended_checked: usize,
    pub(crate) ref_moves: u64,
    pub(crate) refs_checked: Option<u64>,
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
        let repo = root.join("repo");
        let remote = root.join("origin.git");
        std::fs::create_dir_all(&repo).expect("create the repository");
        std::fs::create_dir_all(&remote).expect("create the remote");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let mut w = Self {
            policies,
            root,
            repo,
            remote,
            main_head: String::new(),
            forks: BTreeMap::new(),
            next_fork: 1,
            next_txn: 1,
            queues: BTreeMap::new(),
            handles: BTreeMap::new(),
            next_handle: 1,
            holds: BTreeMap::new(),
            waits: BTreeMap::new(),
            lineage: Lineage::default(),
            counters: BTreeMap::new(),
            collisions: Vec::new(),
            entered: Vec::new(),
            syncs: Vec::new(),
            landings: Vec::new(),
            fast_forwards: 0,
            squashed_runs: 0,
            discarded: BTreeSet::new(),
            appended: BTreeSet::new(),
            ended_main: BTreeSet::new(),
            ended: Vec::new(),
            log: Vec::new(),
            objects: RefCell::new(BTreeMap::new()),
            blames: RefCell::new(BTreeMap::new()),
            ancestry: RefCell::new(BTreeMap::new()),
            over: RefCell::new(BTreeMap::new()),
            views: BTreeMap::new(),
            left: BTreeSet::new(),
            checked_commits: BTreeSet::new(),
            reach: BTreeMap::new(),
            grepped: (0, BTreeSet::new()),
            collisions_checked: 0,
            replication_due: false,
            syncs_checked: 0,
            ended_checked: 0,
            main_history: BTreeSet::new(),
            ref_moves: 0,
            refs_checked: None,
        };
        let files = PATHS
            .iter()
            .map(|p| (p.to_string(), Content::empty()))
            .collect();
        let tree = w.tree_with(None, &files);
        let c = w.commit(&[], &tree, "agentfs", "Root", "main");
        w.set_head(Tree::Main, &c);
        w
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// The shared remote's bare repository.
    pub fn remote(&self) -> &Path {
        &self.remote
    }

    fn note(&mut self, s: String) {
        self.log.push(s);
    }

    /// Logs a scheduler's choice, so a failing seed's log replays it.
    pub fn note_step(&mut self, s: &Step) {
        self.note(format!("step {s:?}"));
    }

    // ── reads ────────────────────────────────────────────────────────────

    pub fn head(&self, t: Tree) -> String {
        match t {
            Tree::Main => self.main_head.clone(),
            Tree::Fork(f) => self.forks[&f].head.clone(),
        }
    }

    /// The ref holding `t`'s head.
    pub fn branch(t: Tree) -> String {
        match t {
            Tree::Main => "refs/heads/main".into(),
            Tree::Fork(f) => format!("refs/heads/fork/{f}"),
        }
    }

    pub(crate) fn name(t: Tree) -> String {
        match t {
            Tree::Main => "main".into(),
            Tree::Fork(f) => format!("fork/{f}"),
        }
    }

    /// What tree `t` shows for `p`.
    pub fn read(&self, t: Tree, p: &str) -> Content {
        self.file(&self.head(t), p)
    }

    /// What a client's program reads: its open transaction's branch (its
    /// own staged edits on the frozen snapshot), else its tree.
    pub fn client_reads(&self, c: &Client, p: &str) -> Content {
        match c {
            Client::Main(_) => self.read(Tree::Main, p),
            Client::Fork(f) => match &self.forks[f].txn {
                Some(t) => self.file(&t.head, p),
                None => self.read(Tree::Fork(*f), p),
            },
        }
    }

    /// `p` at a commit or tree id, cached: objects never change.
    pub fn file(&self, at: &str, p: &str) -> Content {
        let spec = format!("{at}:{p}");
        if let Some(c) = self.objects.borrow().get(&spec) {
            return c.clone();
        }
        let c = Content::parse(&git(&self.repo, &["cat-file", "-p", &spec]))
            .expect("a model file in git");
        self.objects.borrow_mut().insert(spec, c.clone());
        c
    }

    pub fn queue_len(&self, t: Tree) -> usize {
        self.queues.get(&t).map_or(0, VecDeque::len)
    }

    pub fn queue(&self, t: Tree) -> Vec<Entry> {
        self.queues
            .get(&t)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Every open file, waiting opens included.
    pub fn handles(&self) -> impl Iterator<Item = &Handle> {
        self.handles.values()
    }

    pub fn handle(&self, h: HandleId) -> &Handle {
        &self.handles[&h]
    }

    /// Each tree's take in progress, with the files it holds (rule 9).
    pub fn holds(&self) -> &BTreeMap<Tree, Hold> {
        &self.holds
    }

    /// Each tree whose queue head waits, with the files it waits for.
    pub fn waits(&self) -> &BTreeMap<Tree, BTreeSet<String>> {
        &self.waits
    }

    /// The files a client of `t` has open for writing (granted opens).
    pub fn write_open(&self, t: Tree) -> BTreeSet<String> {
        self.handles
            .values()
            .filter(|h| !h.waiting && h.target == Target::Tree(t))
            .map(|h| h.path.clone())
            .collect()
    }

    pub fn alive(&self, t: Tree) -> bool {
        match t {
            Tree::Main => true,
            Tree::Fork(f) => self.forks[&f].life != Life::Dropped,
        }
    }

    pub fn live_trees(&self) -> Vec<Tree> {
        let mut v = vec![Tree::Main];
        v.extend(
            self.forks
                .values()
                .filter(|f| f.life != Life::Dropped)
                .map(|f| Tree::Fork(f.id)),
        );
        v
    }

    /// `t` and every tree upstream of it, up to main.
    pub fn upstream(&self, t: Tree) -> Vec<Tree> {
        let mut v = vec![t];
        while let Some(Tree::Fork(f)) = v.last().copied() {
            v.push(self.forks[&f].parent);
        }
        v
    }

    fn children(&self, t: Tree) -> Vec<ForkId> {
        self.forks
            .values()
            .filter(|f| f.parent == t && f.life != Life::Dropped)
            .map(|f| f.id)
            .collect()
    }

    // ── git plumbing ─────────────────────────────────────────────────────

    /// `base`'s tree (a commit or tree id; none: empty) with `files` replaced.
    fn tree_with(&self, base: Option<&str>, files: &BTreeMap<String, Content>) -> String {
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        if let Some(base) = base {
            for line in git(&self.repo, &["ls-tree", base]).lines() {
                if let Some((_, name)) = line.split_once('\t') {
                    entries.insert(name.to_string(), line.to_string());
                }
            }
        }
        for (p, c) in files {
            let blob = git_in(
                &self.repo,
                &["hash-object", "-w", "--stdin"],
                &c.render(self.policies.layout),
            );
            entries.insert(p.clone(), format!("100644 blob {blob}\t{p}"));
        }
        let listing: String = entries.values().map(|l| format!("{l}\n")).collect();
        git_in(&self.repo, &["mktree"], &listing)
    }

    fn commit(&self, parents: &[&str], tree: &str, writer: &str, cause: &str, on: &str) -> String {
        let msg = message(&format!("{cause} by {writer}"), writer, cause, on);
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        git_in(&self.repo, &args, &msg)
    }

    /// `git update-ref`, counted so `check` re-reads history only after a
    /// ref moved.
    fn update_ref(&mut self, args: &[&str]) {
        let mut v = vec!["update-ref"];
        v.extend(args);
        git(&self.repo, &v);
        self.ref_moves += 1;
    }

    fn set_head(&mut self, t: Tree, c: &str) {
        self.update_ref(&[&Self::branch(t), c]);
        match t {
            Tree::Main => self.main_head = c.to_string(),
            Tree::Fork(f) => self.forks.get_mut(&f).expect("fork").head = c.to_string(),
        }
    }

    fn tree_of(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^{{tree}}")])
    }

    fn first_parent(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^1")])
    }

    /// Whether commit `a` is `b` or an ancestor of it in its lineage (stored
    /// history plus `Squashed`'s grafts; cached: commits never change, and a
    /// graft is made with its commit, before any read of it).
    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        let key = (a.to_string(), b.to_string());
        if let Some(r) = self.ancestry.borrow().get(&key) {
            return *r;
        }
        let r =
            a == b || try_git(&self.repo, &["merge-base", "--is-ancestor", a, b], None).code == 0;
        self.ancestry.borrow_mut().insert(key, r);
        r
    }

    /// Per slot of `p` at commit `at`, the commit that wrote its line (`git
    /// blame`). Cached: commits never change.
    fn blame(&self, at: &str, p: &str) -> Vec<String> {
        let key = format!("{at}:{p}");
        if let Some(b) = self.blames.borrow().get(&key) {
            return b.clone();
        }
        let b = blame_in(&self.repo, at, p, self.policies.layout, false);
        self.blames.borrow_mut().insert(key, b.clone());
        b
    }

    /// The lines the line `origin` wrote on slot `slot` of `p` was written
    /// over, newest first: the line its writer saw there (on `origin`'s
    /// first parent), the line that one was written over, and so on, read
    /// from git (first parents and `git blame`). Per line, not per commit:
    /// a sync merge that kept the fork's line over the parent's does not
    /// make the parent's line one the fork's next edit was written over.
    /// Cached: commits never change.
    fn written_over(&self, origin: &str, p: &str, slot: usize) -> Vec<String> {
        let key = format!("{origin}:{p}:{slot}");
        if let Some(v) = self.over.borrow().get(&key) {
            return v.clone();
        }
        let up = self.first_parent(origin);
        let base = self.file(&up, p).slot(slot).to_string();
        let v = if base == EMPTY {
            Vec::new()
        } else {
            let mut v = vec![base];
            v.extend(self.written_over(&self.blame(&up, p)[slot], p, slot));
            v
        };
        self.over.borrow_mut().insert(key, v.clone());
        v
    }

    /// The writer a commit names.
    pub fn writer_of_commit(&self, c: &str) -> String {
        self.trailer(c, "Agentfs-Writer")
    }

    pub(crate) fn trailer(&self, c: &str, key: &str) -> String {
        git(
            &self.repo,
            &[
                "log",
                "-1",
                &format!("--format=%(trailers:key={key},valueonly)"),
                c,
            ],
        )
    }

    /// Whether `c` is a plain save (not a transaction's commit, nor a
    /// landing of a fork's own fork).
    fn plain_save(&self, c: &str) -> bool {
        self.trailer(c, "Agentfs-Cause") == "Direct"
    }

    /// The slots `b` changes from `a`, with the token each now holds.
    pub fn edits(&self, a: &str, b: &str) -> Vec<Edit> {
        let mut out = Vec::new();
        for p in git(&self.repo, &["diff-tree", "-r", "--name-only", a, b]).lines() {
            let (x, y) = (self.file(a, p), self.file(b, p));
            for i in 0..SLOTS {
                if x.slot(i) != y.slot(i) && y.slot(i) != EMPTY {
                    out.push(Edit {
                        path: p.to_string(),
                        slot: i,
                        token: y.slot(i).to_string(),
                    });
                }
            }
        }
        out
    }

    /// Rule 3 at the parent's queue: each incoming line meets the line of
    /// `ours` (the parent's head) on its slot. A line written over the
    /// parent's ([`World::written_over`] holds it) replaces it; a line the
    /// parent's was written over is stale and dropped; else the two are
    /// concurrent, the parent's line stays, and the collision is reported.
    /// Per [`CollisionScope::File`], a file with a collision keeps the
    /// parent's whole version, every incoming line in it lost.
    fn resolve(&self, ours: &str, incoming: &[Incoming]) -> Merged {
        let mut by_path: BTreeMap<&str, Vec<&Incoming>> = BTreeMap::new();
        for i in incoming {
            by_path.entry(&i.path).or_default().push(i);
        }
        let mut files = BTreeMap::new();
        let mut collisions = Vec::new();
        for (p, ins) in by_path {
            let cur = self.file(ours, p);
            let mut new = cur.clone();
            let (mut hit, mut applied) = (Vec::new(), Vec::new());
            let mut token = BTreeMap::new();
            for i in ins {
                let c = cur.slot(i.slot);
                token.insert(i.slot, i.token.clone());
                if c == i.token {
                    continue;
                }
                if c == EMPTY
                    || self
                        .written_over(&i.origin, p, i.slot)
                        .iter()
                        .any(|t| t == c)
                {
                    new = new.with(i.slot, &i.token);
                    applied.push(i.slot);
                } else if !self
                    .written_over(&self.blame(ours, p)[i.slot], p, i.slot)
                    .contains(&i.token)
                {
                    hit.push(i.slot);
                }
            }
            if !hit.is_empty() && self.policies.collision == CollisionScope::File {
                new = cur.clone();
                hit.extend(applied);
                hit.sort_unstable();
            }
            if !hit.is_empty() {
                let lost = hit.iter().map(|i| token[i].clone()).collect();
                let won = hit.iter().map(|i| cur.slot(*i).to_string()).collect();
                collisions.push(Lost {
                    path: p.to_string(),
                    slots: hit,
                    lost,
                    won,
                });
            }
            if new != cur {
                files.insert(p.to_string(), new);
            }
        }
        let tree = if files.is_empty() {
            self.tree_of(ours)
        } else {
            self.tree_with(Some(ours), &files)
        };
        Merged {
            tree,
            files: files.into_keys().collect(),
            collisions,
        }
    }

    /// The lines a commit's own edit sets: its diff from `base`, each with
    /// its writing commit.
    fn edit_of(&self, base: &str, c: &str) -> Vec<Incoming> {
        self.edits(base, c)
            .into_iter()
            .map(|e| Incoming {
                origin: self.blame(c, &e.path)[e.slot].clone(),
                path: e.path,
                slot: e.slot,
                token: e.token,
            })
            .collect()
    }

    /// [`MergeRule::PerLine`]'s sync: the parent's version `up`, with each
    /// line of the fork's `ours` the parent has not decided yet (its
    /// writing commit is not in `up`'s lineage) kept over it.
    fn adopt(&self, ours: &str, up: &str) -> String {
        let mut files = BTreeMap::new();
        for p in git(&self.repo, &["diff-tree", "-r", "--name-only", ours, up]).lines() {
            let (mine, theirs) = (self.file(ours, p), self.file(up, p));
            let origin = self.blame(ours, p);
            let mut new = theirs.clone();
            for (i, o) in origin.iter().enumerate() {
                if mine.slot(i) != theirs.slot(i) && !self.is_ancestor(o, up) {
                    new = new.with(i, mine.slot(i));
                }
            }
            if new != theirs {
                files.insert(p.to_string(), new);
            }
        }
        if files.is_empty() {
            self.tree_of(up)
        } else {
            self.tree_with(Some(up), &files)
        }
    }

    /// [`MergeRule::GitMergeBase`]: git's own three-way merge of `up` into
    /// `ours`, on git's merge base, a conflicting hunk keeping the fork's
    /// side.
    fn git_merge(&self, ours: &str, up: &str) -> String {
        let o = try_git(
            &self.repo,
            &[
                "merge-tree",
                "--write-tree",
                "--no-messages",
                "-Xours",
                ours,
                up,
            ],
            None,
        );
        // Only content conflicts arise, which `-X` resolves: deletes and
        // renames are left out of the model (MODEL.md).
        assert_eq!(o.code, 0, "merge-tree -Xours resolves: {}", o.stderr);
        o.stdout.lines().next().expect("a tree").to_string()
    }

    /// Records a landing's collisions in tree `t`, each keeping the losing
    /// incoming commit reachable under `refs/collisions/<n>`.
    fn record(
        &mut self,
        t: Tree,
        dir: Direction,
        m: &Merged,
        winner: &str,
        loser: &str,
        result: &str,
    ) {
        for Lost {
            path,
            slots,
            lost,
            won,
        } in &m.collisions
        {
            let reference = format!("refs/collisions/{}", self.collisions.len() + 1);
            self.update_ref(&[&reference, loser]);
            self.note(format!(
                "collision in {t:?}:{path}{slots:?} ({dir:?}): incoming {lost:?} lost to {won:?}"
            ));
            self.collisions.push(Collision {
                tree: t,
                direction: dir,
                path: path.clone(),
                slots: slots.clone(),
                lost: lost.clone(),
                won: won.clone(),
                winner: winner.to_string(),
                loser: loser.to_string(),
                result: result.to_string(),
                reference,
            });
            self.replication_due = true;
        }
    }

    /// Replication's push ([`Step::Replicate`]): main, the grafts and every
    /// collision ref to the shared remote in one atomic push (owner,
    /// 2026-11-03: "yes, \"losing\" patches should still be part of the
    /// history. i want to know who changed what when"). Replication is the
    /// remote's one pusher; no referee step pushes. Never forced: main only
    /// moves forward, and collision and graft refs never move.
    fn replicate(&mut self) {
        self.replication_due = false;
        let remote = self.remote.to_string_lossy().into_owned();
        git(
            &self.repo,
            &[
                "push",
                "-q",
                "--atomic",
                &remote,
                "refs/heads/main:refs/heads/main",
                "refs/collisions/*:refs/collisions/*",
                "refs/replace/*:refs/replace/*",
            ],
        );
    }

    /// `t`'s head moved: a fork's new commit queues for its parent (`up`),
    /// and the new content queues for every child (rule 1: both ways).
    fn moved(&mut self, t: Tree, up: bool) {
        let head = self.head(t);
        if up && let Tree::Fork(f) = t {
            let parent = self.forks[&f].parent;
            self.queues
                .entry(parent)
                .or_default()
                .push_back(Entry::Landing {
                    from: f,
                    commit: head.clone(),
                });
        }
        for c in self.children(t) {
            self.queues
                .entry(Tree::Fork(c))
                .or_default()
                .push_back(Entry::Sync);
        }
    }

    // ── clients ──────────────────────────────────────────────────────────

    /// A launcher creates a fork over `parent`, a branch at its head.
    pub fn create_fork(&mut self, parent: Tree) -> ForkId {
        let id = self.next_fork;
        self.next_fork += 1;
        let head = self.head(parent);
        self.forks.insert(
            id,
            Fork {
                id,
                parent,
                writer: format!("c{id}"),
                life: Life::Open,
                head: head.clone(),
                txn: None,
            },
        );
        self.set_head(Tree::Fork(id), &head);
        self.note(format!("create f{id} over {parent:?}"));
        id
    }

    fn token(&mut self, writer: &str, txn: Option<TxnId>) -> String {
        let n = self.counters.entry(writer.to_string()).or_default();
        *n += 1;
        match txn {
            Some(t) => format!("{writer}.{n}~t{t}"),
            None => format!("{writer}.{n}"),
        }
    }

    /// Rule 1: open `p` for writing, write slot `slot`, close: the token
    /// written. The open must not wait (no take holds `p`); see
    /// [`World::open_write`] for one that may.
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let h = self.open_write(c, p);
        assert!(
            !self.handles[&h].waiting,
            "save: a take holds {p}; open_write waits for it"
        );
        let token = self
            .write(h, Write::At(slot))
            .expect("a write at an offset")
            .expect("a new line");
        self.close(h);
        token
    }

    /// The writer a client's lines name, and its open transaction.
    fn who(&self, c: &Client) -> (String, Option<TxnId>) {
        match c {
            Client::Main(w) => (w.clone(), None),
            Client::Fork(f) => {
                let fork = &self.forks[f];
                (fork.writer.clone(), fork.txn.as_ref().map(|t| t.id))
            }
        }
    }

    fn target_head(&self, t: Target) -> String {
        match t {
            Target::Tree(t) => self.head(t),
            Target::Txn(f, _) => self.forks[&f].txn.as_ref().expect("open").head.clone(),
        }
    }

    /// Rule 9: open `p` for writing, in the client's open transaction if it
    /// has one, else in its tree. Granted at once unless a take holds `p`
    /// in that tree: then the open waits, and the take's release grants it
    /// (an event, never a timer). A transaction's branch is frozen, so an
    /// open there never waits.
    pub fn open_write(&mut self, c: &Client, p: &str) -> HandleId {
        assert!(
            !matches!(c, Client::Main(n) if self.ended_main.contains(n)),
            "open_write by {c:?}, which ended"
        );
        let (_, txn) = self.who(c);
        let target = match (c, txn) {
            (Client::Fork(f), Some(t)) => Target::Txn(*f, t),
            (Client::Fork(f), None) => Target::Tree(Tree::Fork(*f)),
            (Client::Main(_), _) => Target::Tree(Tree::Main),
        };
        let waiting = matches!(target, Target::Tree(t)
            if self.holds.get(&t).is_some_and(|h| h.files.contains(p)));
        let id = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            id,
            Handle {
                id,
                client: c.clone(),
                target,
                path: p.to_string(),
                waiting,
                calls: Vec::new(),
            },
        );
        self.note(format!(
            "open h{id} {c:?} {target:?}:{p}{}",
            if waiting { ": waits for the hold" } else { "" }
        ));
        id
    }

    /// Rule 8: one write call on a granted open. Returns the new line an
    /// append or a write at an offset wrote; a rewrite's lines are made at
    /// close, where they differ from the tree. The open keeps its calls;
    /// close applies them. An append is refused ([`ModelError::FileFull`])
    /// when the model file has no room left for it after this open's calls
    /// and every other open's pending appends to the file.
    pub fn write(&mut self, h: HandleId, call: Write) -> Result<Option<String>, ModelError> {
        let handle = &self.handles[&h];
        assert!(!handle.waiting, "a write needs a granted open");
        let (writer, txn) = self.handle_writer(handle);
        let call = match call {
            Write::Rewrite(content) => {
                self.handles
                    .get_mut(&h)
                    .expect("open")
                    .calls
                    .push(Call::Rewrite(content));
                self.note(format!("write h{h}: rewrite"));
                return Ok(None);
            }
            Write::Append => {
                let cur = self.file(&self.target_head(handle.target), &handle.path);
                let mut calls = handle.calls.clone();
                calls.push(Call::Append(String::new()));
                let others: usize = self
                    .handles
                    .values()
                    .filter(|o| o.id != h && o.target == handle.target && o.path == handle.path)
                    .map(|o| {
                        o.calls
                            .iter()
                            .filter(|c| matches!(c, Call::Append(_)))
                            .count()
                    })
                    .sum();
                if !replay(&cur, &calls).is_some_and(|m| end(&cur, &m) + others <= SLOTS) {
                    return Err(ModelError::FileFull);
                }
                Write::Append
            }
            at => at,
        };
        let token = self.token(&writer, txn);
        let kept = match &call {
            Write::Append => {
                self.appended.insert(token.clone());
                Call::Append(token.clone())
            }
            Write::At(i) => Call::At(*i, token.clone()),
            Write::Rewrite(_) => unreachable!("returned above"),
        };
        self.handles.get_mut(&h).expect("open").calls.push(kept);
        self.note(format!("write h{h}: {call:?} {token}"));
        Ok(Some(token))
    }

    /// Rule 8, the save at close: the write calls replayed, in order, on
    /// the head of the open's tree (or transaction) now ([`replay`]): an
    /// append fills the slot after the file's end as it is then, an offset
    /// write its slot, a rewrite every slot. A written slot that differs
    /// from the head is a new line written over the head's (for a slot a
    /// rewrite copied, a new token holding the copied bytes); every other
    /// slot keeps what the head holds, a line a sync brought or another
    /// writer's append included. One commit, first parent that head, at
    /// once; none if nothing differs. Then a take waiting for this close is
    /// woken.
    pub fn close(&mut self, h: HandleId) {
        let handle = self.handles.remove(&h).expect("an open file");
        assert!(!handle.waiting, "close a granted open");
        let target = handle.target;
        let tokens = self.save_handle(&handle);
        self.note(format!("close h{h}: saved {tokens:?}"));
        if let Target::Tree(t) = target {
            self.wake(t);
        }
    }

    /// The writer an open's lines name, and the transaction it writes in:
    /// fixed by its target at open, whatever the client began since.
    fn handle_writer(&self, h: &Handle) -> (String, Option<TxnId>) {
        let txn = match h.target {
            Target::Txn(_, t) => Some(t),
            Target::Tree(_) => None,
        };
        (self.who(&h.client).0, txn)
    }

    fn save_handle(&mut self, handle: &Handle) -> Vec<String> {
        let (writer, txn) = self.handle_writer(handle);
        let p = handle.path.as_str();
        let at = self.target_head(handle.target);
        let cur = self.file(&at, p);
        // `write` refuses an append with no room, and a file open for
        // writing receives no take (rule 9): only another writer's offset
        // write past the end in the same tree can fill it meanwhile.
        let marks = replay(&cur, &handle.calls)
            .unwrap_or_else(|| panic!("model file {p} full at close: raise SLOTS"));
        let mut new = cur.clone();
        let mut set = Vec::new();
        for (i, mark) in marks {
            let token = match mark {
                Mark::Own(t) => t,
                Mark::Copied(b) if b == cur.slot(i) => continue,
                Mark::Copied(b) => format!("{}={}", self.token(&writer, txn), bytes(&b)),
            };
            self.lineage.wrote(&token, cur.slot(i));
            new = new.with(i, &token);
            set.push(Edit {
                path: p.to_string(),
                slot: i,
                token,
            });
        }
        if set.is_empty() {
            return Vec::new();
        }
        let files = BTreeMap::from([(p.to_string(), new)]);
        let t = self.tree_with(Some(&at), &files);
        let tokens = set.iter().map(|e| e.token.clone()).collect();
        match handle.target {
            Target::Txn(f, id) => {
                let on = format!("txn/{id}");
                let commit = self.commit(&[&at], &t, &writer, "Direct", &on);
                self.update_ref(&[&format!("refs/heads/{on}"), &commit]);
                let txn = self.forks.get_mut(&f).expect("fork").txn.as_mut();
                let txn = txn.expect("open");
                txn.head = commit;
                txn.staged.extend(set);
            }
            Target::Tree(tree) => {
                let c = self.commit(&[&at], &t, &writer, "Direct", &Self::name(tree));
                self.set_head(tree, &c);
                self.entered.extend(set.into_iter().map(|e| (tree, e)));
                self.moved(tree, true);
            }
        }
        tokens
    }

    /// A close or a client's death in `t`: a queue head waiting for write-
    /// opens there is woken once none of its files is open for writing.
    fn wake(&mut self, t: Tree) {
        if let Some(files) = self.waits.get(&t) {
            let open = self.write_open(t);
            if files.iter().all(|p| !open.contains(p)) {
                self.waits.remove(&t);
                self.note(format!("wake {t:?}'s queue"));
            }
        }
    }

    // ── transactions (rule 5) ────────────────────────────────────────────

    /// Begin: a branch frozen at the fork's head. The fork keeps receiving
    /// syncs; the program reads its own staged edits on the snapshot. A
    /// file open before begin keeps writing to the fork (its target is
    /// fixed at open); only opens after begin write in the transaction.
    pub fn begin(&mut self, f: ForkId) -> Result<TxnId, ModelError> {
        let fork = &self.forks[&f];
        if fork.life != Life::Open {
            return Err(ModelError::Ended);
        }
        if fork.txn.is_some() {
            return Err(ModelError::InTransaction);
        }
        let id = self.next_txn;
        self.next_txn += 1;
        let start = fork.head.clone();
        self.update_ref(&[&format!("refs/heads/txn/{id}"), &start]);
        self.forks.get_mut(&f).expect("fork").txn = Some(Txn {
            id,
            start: start.clone(),
            head: start,
            staged: Vec::new(),
        });
        self.note(format!("begin t{id} on f{f}"));
        Ok(id)
    }

    /// Commit (a design choice, MODEL.md rule 5): the transaction's branch
    /// lands in the fork's current head as a child's change lands in its
    /// parent, as one merge commit named for the client, which then lands
    /// like any fork commit. Never refused. The fork is the transaction's
    /// parent, so a fork line concurrent with a staged line (one a sync
    /// brought in since begin) wins; the staged lines that do not collide
    /// apply; the answer names each staged line that lost. The fork's one
    /// actor finishes a take in progress first (rule 9). Refused only while
    /// the program itself holds open for writing a file of the transaction,
    /// or a file of the fork (opened before begin) the commit would write
    /// ([`ModelError::OpenFiles`], a choice): those are the commit's own
    /// caller's locks, so a wait would wait on the caller. Nothing else is
    /// open in a fork: it has one client.
    pub fn commit_txn(&mut self, f: ForkId) -> Result<TxnOutcome, ModelError> {
        let Some(txn) = &self.forks[&f].txn else {
            return Err(ModelError::NoTransaction);
        };
        let id = txn.id;
        if self
            .handles
            .values()
            .any(|h| h.target == Target::Txn(f, id))
        {
            return Err(ModelError::OpenFiles);
        }
        if self.holds.contains_key(&Tree::Fork(f)) {
            self.finish(Tree::Fork(f));
        }
        let fork = &self.forks[&f];
        let ours = fork.head.clone();
        let txn = fork.txn.as_ref().expect("open");
        let (start, head) = (txn.start.clone(), txn.head.clone());
        let planned = if head == start {
            None
        } else {
            let edit = self.edit_of(&start, &head);
            let m = self.resolve(&ours, &edit);
            let open = self.write_open(Tree::Fork(f));
            if m.files.iter().any(|p| open.contains(p)) {
                return Err(ModelError::OpenFiles);
            }
            Some((edit, m))
        };
        let txn = self.take_txn(f)?;
        let mut result = None;
        let outcome = if let Some((edit, m)) = planned {
            let writer = self.forks[&f].writer.clone();
            let t = Tree::Fork(f);
            let c = self.commit(
                &[&ours, &txn.head],
                &m.tree,
                &writer,
                &format!("Transaction t{}", txn.id),
                &Self::name(t),
            );
            self.record(t, Direction::Transaction, &m, &ours, &txn.head, &c);
            self.set_head(t, &c);
            for e in edit {
                self.entered.push((
                    t,
                    Edit {
                        path: e.path,
                        slot: e.slot,
                        token: e.token,
                    },
                ));
            }
            self.moved(t, true);
            result = Some(c);
            let overwritten = m
                .collisions
                .iter()
                .flat_map(|l| {
                    l.slots
                        .iter()
                        .zip(l.lost.iter().zip(&l.won))
                        .map(|(s, (lost, won))| Overwritten {
                            path: l.path.clone(),
                            slot: *s,
                            staged: lost.clone(),
                            kept: won.clone(),
                        })
                })
                .collect();
            TxnOutcome::Committed { overwritten }
        } else {
            TxnOutcome::Empty
        };
        self.note(format!("commit t{} on f{f}: {outcome:?}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Commit(outcome.clone()),
            result,
        });
        Ok(outcome)
    }

    /// Rollback (abandon): drop the branch, and its files still open,
    /// unsaved.
    pub fn rollback(&mut self, f: ForkId) -> Result<(), ModelError> {
        let txn = self.take_txn(f)?;
        self.handles
            .retain(|_, h| !matches!(h.target, Target::Txn(g, t) if g == f && t == txn.id));
        self.discard(&txn);
        self.note(format!("rollback t{} on f{f}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Rollback,
            result: None,
        });
        Ok(())
    }

    fn discard(&mut self, txn: &Txn) {
        self.discarded
            .extend(txn.staged.iter().map(|e| e.token.clone()));
    }

    fn take_txn(&mut self, f: ForkId) -> Result<Txn, ModelError> {
        let txn = self
            .forks
            .get_mut(&f)
            .expect("fork")
            .txn
            .take()
            .ok_or(ModelError::NoTransaction)?;
        self.update_ref(&["-d", &format!("refs/heads/txn/{}", txn.id)]);
        Ok(txn)
    }

    /// A fork's client leaves or dies (its end event): an open transaction
    /// is dropped with its open files, and every lock it held is released
    /// ([`World::release`]). The fork's commits still land; it drops once
    /// that is done.
    pub fn end_client(&mut self, f: ForkId) {
        if self.forks[&f].txn.is_some() {
            let _ = self.rollback(f);
        }
        self.release(&Client::Fork(f));
        self.forks.get_mut(&f).expect("fork").life = Life::Ended;
        self.wake(Tree::Fork(f));
        self.note(format!("end f{f}"));
    }

    /// One of main's clients exits or dies (its end event): every lock it
    /// held is released ([`World::release`]), so main's queue, waiting for
    /// its open file, is woken. It opens nothing after.
    pub fn end_main_client(&mut self, writer: &str) {
        self.release(&Client::Main(writer.to_string()));
        self.ended_main.insert(writer.to_string());
        self.wake(Tree::Main);
        self.note(format!("end main client {writer}"));
    }

    /// A client's end event releases its locks: its open files close as the
    /// kernel closes them on exit, each saving its write calls (a choice),
    /// which wakes a take waiting for them; its waiting opens are dropped.
    fn release(&mut self, c: &Client) {
        let mine: Vec<HandleId> = self
            .handles
            .values()
            .filter(|h| h.client == *c)
            .map(|h| h.id)
            .collect();
        for h in mine {
            if self.handles[&h].waiting {
                self.handles.remove(&h);
            } else {
                self.close(h);
            }
        }
    }

    // ── steps ────────────────────────────────────────────────────────────

    /// Every step that can act now: the head of each live tree's queue
    /// that neither waits nor has a take in progress, each take in
    /// progress, and each fork ready to drop.
    pub fn pending(&self) -> Vec<Step> {
        let mut out: Vec<Step> = self
            .queues
            .iter()
            .filter(|(t, q)| {
                !q.is_empty()
                    && self.alive(**t)
                    && !self.holds.contains_key(t)
                    && !self.waits.contains_key(t)
            })
            .map(|(t, _)| Step::Next(*t))
            .collect();
        out.extend(self.holds.keys().map(|t| Step::Finish(*t)));
        out.extend(
            self.forks
                .keys()
                .filter(|f| self.droppable(**f))
                .map(|f| Step::Drop(*f)),
        );
        if self.replication_due {
            out.push(Step::Replicate);
        }
        out
    }

    /// Takes one step and returns how many queue entries it took: a
    /// [`Step::Next`] takes its queue's head, plus under `Squashed` the
    /// same fork's plain saves it carries with it, or none when it waits.
    /// Nothing is held by a collision.
    pub fn step(&mut self, s: &Step) -> usize {
        match s {
            Step::Next(t) => self.acquire(*t),
            Step::Finish(t) => {
                self.finish(*t);
                0
            }
            Step::Drop(f) => {
                self.drop_fork(*f);
                0
            }
            Step::Replicate => {
                self.replicate();
                0
            }
        }
    }

    /// Takes steps until none is left.
    pub fn settle(&mut self) {
        for _ in 0..100_000 {
            let Some(s) = self.pending().into_iter().next() else {
                return;
            };
            self.note_step(&s);
            self.step(&s);
        }
        panic!("the queues never drained");
    }

    /// Rule 9, a take begins: plan the queue head's change into `t` and the
    /// files it will overwrite there. If a client of `t` has one of them
    /// open for writing, the head waits for those to close (`waits`) and
    /// nothing is taken; else it leaves the queue and holds its files.
    fn acquire(&mut self, t: Tree) -> usize {
        let q = self.queue(t);
        let head = q.first().cloned().expect("a queued entry");
        let ours = self.head(t);
        let (take, files, keep) = match head {
            Entry::Sync => {
                let Tree::Fork(f) = t else {
                    unreachable!("main has no parent")
                };
                let up = self.head(self.forks[&f].parent);
                let tree = self.sync_tree(&ours, &up);
                let files = tree
                    .as_ref()
                    .map(|tr| self.changed(&ours, tr))
                    .unwrap_or_default();
                let keep: VecDeque<Entry> = q[1..].iter().cloned().collect();
                let landing = self.landing_pending(self.forks[&f].parent, f);
                (Take::Sync { up, tree, landing }, files, keep)
            }
            Entry::Landing { from, commit } => {
                let (run, keep) = self.run_from(from, &commit, &q[1..]);
                let merged = self.resolve(&ours, &self.incoming(&run));
                let files = merged.files.clone();
                (Take::Landing { from, run, merged }, files, keep)
            }
        };
        let open: BTreeSet<String> = self.write_open(t).intersection(&files).cloned().collect();
        if !open.is_empty() {
            self.note(format!("{t:?}'s queue waits for {open:?} to close"));
            self.waits.insert(t, open);
            return 0;
        }
        let taken = q.len() - keep.len();
        self.queues.insert(t, keep);
        self.note(format!("{t:?} holds {files:?}"));
        self.holds.insert(
            t,
            Hold {
                files,
                take,
                ours,
                entered: self.entered.len(),
            },
        );
        taken
    }

    /// Rule 9, a take ends: write the held change, then release the files:
    /// every open waiting for them is granted (an event). If a save of
    /// another file moved the tree's head since the take began, the change
    /// is planned again on the new head; it must still write only files it
    /// holds (a save never writes a held file: no write-open of one is
    /// granted), and the model stops loudly if not.
    fn finish(&mut self, t: Tree) {
        let hold = self.holds.remove(&t).expect("a take in progress");
        let head = self.head(t);
        let moved = head != hold.ours;
        match hold.take {
            Take::Sync { up, tree, landing } => {
                let tree = if moved {
                    self.sync_tree(&head, &up)
                } else {
                    tree
                };
                let files = tree
                    .as_ref()
                    .map(|tr| self.changed(&head, tr))
                    .unwrap_or_default();
                held(t, &files, &hold.files);
                self.sync(t, &up, tree, landing, hold.entered);
            }
            Take::Landing { from, run, merged } => {
                let merged = if moved {
                    self.resolve(&head, &self.incoming(&run))
                } else {
                    merged
                };
                held(t, &merged.files, &hold.files);
                self.land(t, from, &run, merged);
            }
        }
        let waiting: Vec<HandleId> = self
            .handles
            .values()
            .filter(|h| h.waiting && h.target == Target::Tree(t))
            .map(|h| h.id)
            .collect();
        for h in waiting {
            self.handles.get_mut(&h).expect("open").waiting = false;
            self.note(format!("grant h{h}"));
        }
    }

    /// The files tree `tree` changes from commit `ours`.
    fn changed(&self, ours: &str, tree: &str) -> BTreeSet<String> {
        git(&self.repo, &["diff-tree", "-r", "--name-only", ours, tree])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The fork commits a landing of `first` carries, and the queue left
    /// behind (`rest`, the entries after it): `first` alone, or under
    /// `Squashed`, when `first` is a plain save, every later plain save of
    /// the same fork in `rest` up to that fork's next other entry (a
    /// transaction's commit or a nested landing, which lands on its own).
    fn run_from(
        &self,
        from: ForkId,
        first: &str,
        rest: &[Entry],
    ) -> (Vec<String>, VecDeque<Entry>) {
        let mut run = vec![first.to_string()];
        let mut keep = VecDeque::new();
        let squash = self.policies.history == HistoryPolicy::Squashed && self.plain_save(first);
        let mut open = squash;
        for e in rest {
            if open
                && let Entry::Landing { from: g, commit } = e
                && *g == from
            {
                if self.plain_save(commit) {
                    run.push(commit.clone());
                    continue;
                }
                open = false;
            }
            keep.push_back(e.clone());
        }
        (run, keep)
    }

    /// The lines a run of fork commits sets, the last write on each slot.
    fn incoming(&self, run: &[String]) -> Vec<Incoming> {
        let mut lines: BTreeMap<(String, usize), Incoming> = BTreeMap::new();
        for c in run {
            for i in self.edit_of(&self.first_parent(c), c) {
                lines.insert((i.path.clone(), i.slot), i);
            }
        }
        lines.into_values().collect()
    }

    /// Rule 3, a fork's change into its parent `x`, the one place a
    /// collision between them is decided. A fast-forward when the commit's
    /// base is the target's head (`FullLineage`), else each carried line
    /// meets the parent's ([`World::resolve`]): the parent's line wins a
    /// collision. `m`: the merge planned on `x`'s current head.
    fn land(&mut self, x: Tree, from: ForkId, run: &[String], m: Merged) {
        let tip = run.last().expect("a commit").clone();
        let ours = self.head(x);
        let incoming = self.incoming(run);
        let carried: Vec<&str> = incoming.iter().map(|i| i.token.as_str()).collect();
        self.note(format!(
            "landing f{from}'s {} commit(s) to {tip:.7} into {x:?} carries {carried:?}",
            run.len()
        ));
        for i in &incoming {
            self.entered.push((
                x,
                Edit {
                    path: i.path.clone(),
                    slot: i.slot,
                    token: i.token.clone(),
                },
            ));
        }
        for c in run {
            self.landings.push((x, c.clone()));
        }
        if run.len() > 1 {
            self.squashed_runs += 1;
        }
        let full = self.policies.history == HistoryPolicy::FullLineage;
        if full && self.first_parent(&tip) == ours {
            // Made on the parent's current version: every line replaces
            // the parent's, so nothing collides.
            assert!(
                m.collisions.is_empty() && m.tree == self.tree_of(&tip),
                "a fast-forward is a plain replacement"
            );
            self.set_head(x, &tip);
            self.fast_forwards += 1;
            self.note(format!(
                "landed f{from}'s {tip:.7} into {x:?}: fast-forward"
            ));
            self.moved(x, true);
            return;
        }
        let writer = self.writer_of_commit(&tip);
        let new = if full {
            self.commit(&[&ours, &tip], &m.tree, &writer, "Landing", &Self::name(x))
        } else {
            // Rule 7, `Squashed`: the stored commit has one parent; its
            // lineage, the last fork commit it carried, is a graft beside
            // history, made before any read of `new`. Made even when
            // every line lost, so the parent's lineage shows the decision.
            let new = self.commit(&[&ours], &m.tree, &writer, "Landing", &Self::name(x));
            git(&self.repo, &["replace", "--graft", &new, &ours, &tip]);
            self.ref_moves += 1;
            new
        };
        self.record(x, Direction::Landing, &m, &ours, &tip, &new);
        self.note(format!("landed f{from}'s {tip:.7} into {x:?}"));
        self.set_head(x, &new);
        self.moved(x, true);
    }

    /// The tree a sync of the parent's `up` into fork `x` (at `ours`)
    /// makes, per [`MergeRule`]; none if the fork already has `up`.
    fn sync_tree(&self, ours: &str, up: &str) -> Option<String> {
        if self.is_ancestor(up, ours) {
            return None;
        }
        Some(if self.is_ancestor(ours, up) {
            self.tree_of(up)
        } else {
            match self.policies.merge {
                MergeRule::PerLine => self.adopt(ours, up),
                MergeRule::GitMergeBase => self.git_merge(ours, up),
            }
        })
    }

    /// Rule 3, parent into fork: the parent's decided version `up`, read
    /// when the take began (never the one current when the sync was
    /// queued). It decides nothing: each fork line the parent's queue has
    /// not taken yet stays until that queue decides it. Nothing if the
    /// fork has the parent's version; the parent's head itself (a
    /// fast-forward: the fork adopts the parent's decisions) when no fork
    /// line or commit is still waiting for the parent; else a merge commit
    /// labelled `Sync` with the parent's head as second parent.
    /// `planned`: the tree planned on the fork's current head (none: it has
    /// `up`); `landing`: a fork commit was on its way to the parent when the
    /// take began, so `up` may lack it.
    fn sync(&mut self, x: Tree, up: &str, planned: Option<String>, landing: bool, entered: usize) {
        let Tree::Fork(f) = x else {
            unreachable!("main has no parent")
        };
        let parent = self.forks[&f].parent;
        let ours = self.head(x);
        let Some(tree) = planned else {
            return;
        };
        for e in self.edits(&ours, &tree) {
            self.entered.push((x, e));
        }
        let waiting = landing || self.landing_pending(parent, f);
        let new = if tree == self.tree_of(up) && !waiting {
            self.note(format!("synced {up:.7} into f{f}: fast-forward"));
            up.to_string()
        } else {
            self.note(format!("synced {up:.7} into f{f}"));
            self.commit(&[&ours, up], &tree, "agentfs", "Sync", &Self::name(x))
        };
        self.set_head(x, &new);
        self.syncs.push((x, up.to_string(), new, entered));
        self.moved(x, false);
    }

    fn droppable(&self, f: ForkId) -> bool {
        let fork = &self.forks[&f];
        let t = Tree::Fork(f);
        fork.life == Life::Ended
            && self.children(t).is_empty()
            && !self.holds.contains_key(&t)
            && !self.waits.contains_key(&t)
            && !self.landing_pending(fork.parent, f)
    }

    /// Whether a commit of fork `f` waits in `parent`'s queue or is being
    /// taken there.
    fn landing_pending(&self, parent: Tree, f: ForkId) -> bool {
        self.queue(parent)
            .iter()
            .any(|e| matches!(e, Entry::Landing { from, .. } if *from == f))
            || matches!(self.holds.get(&parent),
                Some(Hold { take: Take::Landing { from, .. }, .. }) if *from == f)
    }

    fn drop_fork(&mut self, f: ForkId) {
        assert!(self.droppable(f), "drop f{f} when ready");
        self.update_ref(&["-d", &Self::branch(Tree::Fork(f))]);
        self.queues.remove(&Tree::Fork(f));
        self.forks.get_mut(&f).expect("fork").life = Life::Dropped;
        self.note(format!("drop f{f}"));
    }

    /// What a run decided, without commit ids (they carry dates): each
    /// branch's and collision ref's tree, and each collision's tree, side,
    /// file, slots and tokens. The same calls give the same fingerprint
    /// whatever the commits' dates (`r2_no_clock_commit_dates_decide_nothing`).
    /// Graft refs are named by commit id, so they are left out; their
    /// effect shows in the trees.
    pub fn fingerprint(&self) -> String {
        let mut s = git(
            &self.repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(tree)",
                "refs/heads",
                "refs/collisions",
            ],
        );
        for c in &self.collisions {
            s.push_str(&format!(
                "\n{:?} {:?} {} {:?} {:?} {:?}",
                c.tree, c.direction, c.path, c.slots, c.lost, c.won
            ));
        }
        s
    }

    /// Every ref's commits as stored (grafts' own commits included), for
    /// history checks.
    pub fn all_commits(&self) -> Vec<String> {
        git(&self.repo, &["--no-replace-objects", "rev-list", "--all"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Whether commit `a` is in `b`'s stored history: what is pushed, with
    /// `Squashed`'s grafts ignored (rule 7).
    pub fn in_history(&self, a: &str, b: &str) -> bool {
        a == b
            || try_git(
                &self.repo,
                &["--no-replace-objects", "merge-base", "--is-ancestor", a, b],
                None,
            )
            .code
                == 0
    }
}

/// Rule 9: a take writes only files it holds.
fn held(t: Tree, files: &BTreeSet<String>, holds: &BTreeSet<String>) {
    assert!(
        files.is_subset(holds),
        "a take into {t:?} would write {files:?}, holding only {holds:?}"
    );
}

/// Rule 8: an open's write calls replayed, in order, on `cur` (the head at
/// close): what each slot it wrote ends up holding. An append fills the
/// slot after the file's end as the calls before it leave it, never a
/// written slot; an offset write its slot; a rewrite every slot. None when
/// an append finds no room (a model file has [`SLOTS`] lines).
fn replay(cur: &Content, calls: &[Call]) -> Option<BTreeMap<usize, Mark>> {
    let mut marks = BTreeMap::new();
    for call in calls {
        match call {
            Call::Append(t) => {
                let i = end(cur, &marks);
                if i == SLOTS {
                    return None;
                }
                marks.insert(i, Mark::Own(t.clone()));
            }
            Call::At(i, t) => {
                marks.insert(*i, Mark::Own(t.clone()));
            }
            Call::Rewrite(c) => {
                for i in 0..SLOTS {
                    marks.insert(i, Mark::Copied(c.slot(i).to_string()));
                }
            }
        }
    }
    Some(marks)
}

/// The slot after the last written one of `cur` under `marks`. A rewrite
/// never shortens a model file: a slot it leaves blank over a written one
/// holds a new blank line, so it counts as written.
fn end(cur: &Content, marks: &BTreeMap<usize, Mark>) -> usize {
    (0..SLOTS)
        .rev()
        .find(|i| match marks.get(i) {
            Some(Mark::Own(_)) => true,
            Some(Mark::Copied(b)) => b != EMPTY || cur.slot(*i) != EMPTY,
            None => cur.slot(*i) != EMPTY,
        })
        .map_or(0, |i| i + 1)
}

/// Per slot of `p` at commit `at` in `repo`, the commit that wrote its
/// line (`git blame --porcelain`); `stored`: as stored, grafts ignored.
pub(crate) fn blame_in(
    repo: &Path,
    at: &str,
    p: &str,
    layout: Layout,
    stored: bool,
) -> Vec<String> {
    let mut args = vec!["blame", "--porcelain", at, "--", p];
    if stored {
        args.insert(0, "--no-replace-objects");
    }
    let mut by_line = BTreeMap::new();
    for line in git(repo, &args).lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() >= 3
            && f[0].len() == 40
            && f[0].bytes().all(|b| b.is_ascii_hexdigit())
            && let (Ok(_), Ok(n)) = (f[1].parse::<usize>(), f[2].parse::<usize>())
        {
            by_line.insert(n, f[0].to_string());
        }
    }
    (0..SLOTS)
        .map(|i| by_line[&Content::line_of(i, layout)].clone())
        .collect()
}

impl Drop for World {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
