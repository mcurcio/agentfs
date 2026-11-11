//! The model: main and each fork as a git branch, each save a commit naming
//! one writer, and the owner's rules on top (MODEL.md).
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | a close ([`World::close`]) is never refused; its save is a commit on the writer's tree at once, its first parent the tree's head at that moment |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`](crate::queue::Entry)s per tree (`queue`); a [`Step::Next`] takes its head, or records that it waits (rule 9) |
//! | 3. The upstream's queue decides | `land` (a fork's commit into its upstream) is the only place a collision with the upstream is decided, through `judge` and the referee's land (`referee`, called by `resolve`): per line, a line written over the upstream's replaces it, a stale one is dropped, and of two concurrent lines the upstream's wins; then each change (a save, a transaction's commit) is judged whole: one lost line and none of it applies (rule 3e). `sync` decides nothing: it takes the upstream's version and keeps only the fork's lines the upstream's queue has not taken yet (the referee's sync, called by `adopt`) |
//! | 4. Nothing waits behind a collision | there is no held state: a collision is recorded as a [`Collision`] and in git (a trailer of the take's result commit, [`COLLISION`]), the fork's losing commit kept under `refs/collisions/<n>`, which Replication later pushes to the shared remote with main ([`Step::Replicate`], due on every move of main and every collision), and the queue moves on |
//! | 5. Transactions | [`World::begin`]: a branch `txn/<n>` frozen at the fork's head; [`World::commit_txn`] lands it in the fork like a child's commit, whole or not at all (the fork is its upstream, so the fork's lines win), and answers whether it lost, with every staged line that did not apply; [`World::rollback`] drops it. While one is open, other I/O on its files in the fork is accepted and applies in order after it ends ([`World::deferred`], kept by `queue`, R5.3) |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a sync is a merge with the upstream's commit as second parent |
//! | 7. Full lineage | a landing is a merge with the fork's commit as second parent (or a fast-forward), so a tree's history reaches every change it took, the losers included; each fork commit lands on its own |
//! | 8. A save records the write calls | [`World::write`] keeps each byte call ([`Write`](crate::recording::Write): an `O_APPEND` write, `pwrite`, `ftruncate`; [`World::open_trunc`] for `O_TRUNC`) on the open file; [`World::close`] applies them to the tree's head then: an append's bytes go after the file's last byte, finishing a partial last line (a version of it) and then making new lines; other calls leave bytes the save aligns with the head's lines (rule 10). [`World::edit`] makes a test's line edit as a program's byte calls (`program`) |
//! | 9. A take locks the files it overwrites | [`World::open_write`]; a [`Step::Next`] whose sync or landing would overwrite a file open for writing in that tree waits ([`World::waits`]) until the close; else it takes its entry, locks its files and writes them in that one step ([`World::last_take`]), so no call runs while a take holds a file (the owner, 2026-11-08, finding 49: "B"); a client's death ([`World::end_client`], [`World::end_main_client`]) closes its files |
//! | 10. Line identity | a save derives identities from the bytes at close: it aligns them with the head's lines once, by git's histogram diff (`align`), and records the result: each new line an identity, each changed line a new version written over the version it replaced; the referee and a sync then compare versions of one identity, never bytes or positions (the referee: `referee/placing.rs` places them); a line a landing would put after the upstream's unfinished last line collides with it (R10.4), and so does an unfinished line a landing would put in front of a line the upstream keeps (R10.8); until the upstream decides, a sync hides the upstream's unfinished last line in front of an undecided line (R10.6) and the upstream's lines after an undecided unfinished line (R10.9) |
//! | 9, progress (git-model.md §6) | P1: a [`Step::Next`] takes the first entry no earlier waiting entry shares a file or a source with (`queue`'s scan), so several may wait ([`World::waits`]); no call ever waits on a waiting entry (P3 is gone, the owner's decision of 2026-11-04): a new open of a file a waiting entry waits for is granted; P2, the prefix sync: a sync delivers the upstream's commits in order up to the first whose delivery would overwrite a file open for writing in the fork, and waits for the rest (`queue`'s prefix, its `Cut`) |
//! | 11. Deletes and renames | a file's name line: [`World::rename`] writes a new version, [`World::unlink`] a deleted one; the referee's land applies the per-line rule to it and couples a file's existence with its content. [`World::create`] makes a file (an unborn identity); an open holds an object ([`Handle::object`](crate::recording::Handle::object)), so a close saves to the file its object is named by ([`World::named`]) or, for an orphan, nothing ([`Orphan`](crate::recording::Orphan), kept by `recording`); a rename-over from a new file is an atomic save, a rewrite of the replaced file |
//! | 12. The journal (git-model.md §3) | an open's calls are its journal; [`World::crash`] saves each open from it once (the `Agentfs-Open` trailer, [`World::saved`]); a take is one step, so none is in progress at a crash |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git history.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::calls::{
    CallError, Caller, Client, HandleId, Recorded, RecordedCall, Shown, ShownCollision, Took,
    TreeCalls, TreeId, TxnId, gather_collisions, out_of_scope,
};
use crate::content::{Content, Edit, Layout, Line, PATHS, UNBORN};
use crate::git::{git, git_in, try_git};
use crate::program;
pub use crate::program::LineEdit;
use crate::queue::{
    self, Choice, Cut, Entry, Keys, Plans, Queues, Take, TakeRecord, Waiting, name_key,
};
use crate::recording::{self, Files, Handle, Kind, Recording, Save, Saved, Target, Write};
use crate::referee::{self, Incoming, Lost};

/// The design choice under rules 2 and 3: how a sync builds the fork's
/// new version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRule {
    /// Per line, from git alone. A sync takes the upstream's current version
    /// and keeps each fork line whose writing commit (`git blame`) the
    /// upstream's lineage does not reach yet: a line still waiting for the
    /// upstream's queue to decide it. The model's choice.
    PerLine,
    /// git's own three-way merge of the upstream's current version into the
    /// fork (`git merge-tree -X ours`, git's merge base): a conflicting
    /// hunk keeps the fork's side, since a fork never decides a collision.
    /// Crossing syncs and landings give two merge bases, so the fork keeps
    /// a line the upstream already decided against (coherence finding 1); a
    /// hunk spans neighbouring lines (coherence finding 6).
    GitMergeBase,
}

#[derive(Clone, Copy, Debug)]
pub struct Policies {
    pub merge: MergeRule,
    pub layout: Layout,
}

impl Default for Policies {
    /// The model's choices: per-line syncs, slots on separate hunks.
    fn default() -> Self {
        Self {
            merge: MergeRule::PerLine,
            layout: Layout::Separated,
        }
    }
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
    pub id: TreeId,
    pub upstream: TreeId,
    /// Its one client's writer name.
    pub writer: String,
    pub life: Life,
    pub head: String,
    pub txn: Option<Txn>,
}

/// Where a collision was decided: always an upstream's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A fork's commit into its upstream.
    Landing,
    /// A transaction's branch into its fork, at commit: the fork is the
    /// transaction's upstream.
    Transaction,
}

/// Why a version lost (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// A same-line collision: the version and the upstream's were each
    /// written without seeing the other. Or a line placed after the
    /// upstream's unfinished last line, unseen by its writer (R10.4).
    Line,
    /// A file rule (rule 11): an unlink the upstream's unseen line defeated,
    /// an edit of a file the upstream unlinked, half of a rename-over, a
    /// path clash. Its lost and won versions may be of different lines.
    File,
    /// A line that did not collide itself, of a change that lost whole
    /// (rule 3e: a save, or a transaction, lands whole or loses whole): its
    /// `won` is the upstream's version that decided the change's loss, named
    /// by a `Line` or `File` collision of the same change.
    Whole,
}

/// Rule 3's event: a change into an upstream lost, whole (rule 3e). One
/// record per file and cause ([`By`]); every record of one change names
/// the same `loser` and `result`.
#[derive(Clone, Debug)]
pub struct Collision {
    /// The tree that decided it: the upstream the incoming change landed in.
    pub tree: TreeId,
    pub direction: Direction,
    /// The file (its identity) the lost versions are of.
    pub path: String,
    /// The line identities the incoming change lost (a file's name line
    /// is the file's identity).
    pub lines: Vec<String>,
    /// The incoming versions of those lines, which lost.
    pub lost: Vec<String>,
    /// The tree's versions that won (it kept them), one for each lost: of
    /// the same line, or, per [`By`] or R10.4, the line that decided the loss.
    pub won: Vec<String>,
    pub by: By,
    /// The tree's head before the take: the upstream's version, which won.
    pub winner: String,
    /// The change that lost, whole: a save, a transaction's commit or (a
    /// nested fork's) landing commit, or the transaction's branch at
    /// its commit. Kept reachable by `reference`.
    pub loser: String,
    /// The tree's commit after the take (or the transaction's commit).
    pub result: String,
    pub reference: String,
}

/// How a transaction ended (rule 5), with what it staged: `check` holds
/// every staged edit to its end (in the fork at commit, all named in the
/// answer of a commit that lost, or discarded).
#[derive(Clone, Debug)]
pub struct Ended {
    pub fork: TreeId,
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

/// What a transaction's commit tells the program (rule 5). Nothing refuses
/// a commit. It lands in the fork as a child's change
/// does in its upstream, whole or not at all (rule 3e): the fork's own
/// concurrent line wins a collision, and then none of the transaction's
/// lines apply; the answer names every staged line that did not, so the
/// program is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    /// Every staged line applied.
    Committed,
    /// A staged line collided and the fork's line won, so the whole
    /// transaction lost: none of its lines applied. Each staged line that
    /// did not, with the fork's version that won it, or for a line that
    /// did not collide itself the one that decided the loss.
    Lost { lines: Vec<Overwritten> },
    /// It staged nothing.
    Empty,
}

/// A staged line a transaction's commit did not apply: the fork's
/// concurrent version `kept` won it, or decided the transaction's loss
/// ([`By::Whole`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overwritten {
    pub path: String,
    /// The staged version's line identity.
    pub line: String,
    pub staged: String,
    pub kept: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The fork has one open already.
    InTransaction,
    NoTransaction,
    Ended,
}

/// A step the scheduler can take: each completes at once (a [`Step::Next`]
/// may record that its head waits).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// A take, one atomic step (the owner, 2026-11-08, finding 49: "B"):
    /// choose the first entry of a tree's queue P1 lets go, lock the files it
    /// overwrites and write them, with no call in between; or, if a client of
    /// that tree has one of them open for writing, record that it waits for
    /// those to close (rule 9).
    Next(TreeId),
    Drop(TreeId),
    /// Replication pushes to the shared remote: due once main moved or a
    /// collision was recorded since its last push (the events it waits
    /// for), taken in any order with the queue steps, so the push is
    /// eventual and never part of a referee's step.
    Replicate,
}

/// One take of a change into a tree (a landing, or a transaction's commit
/// into its fork), as `check` reads it: the change with the base its own
/// edit is read against, and whether it won.
#[derive(Clone, Debug)]
pub struct Landed {
    pub tree: TreeId,
    pub direction: Direction,
    /// The tree's head before the take, and its commit after.
    pub before: String,
    pub result: String,
    /// A commit, or a transaction's branch head.
    pub change: String,
    /// The change's first parent, or the commit the transaction began on.
    pub base: String,
    pub won: bool,
}

/// One take's result: the tree after the change, if it won, and its
/// collisions, if it lost (rule 3e).
#[derive(Clone, Debug)]
pub struct Merged {
    tree: String,
    /// The files the result changes.
    files: BTreeSet<String>,
    collisions: Vec<Lost>,
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

/// The trailer of a take's result commit naming one of its collision records
/// (R5.1, R9.9): the decision lives on that commit, in its tree's history. A
/// losing commit cannot be edited after it is made, so it holds none.
pub const COLLISION: &str = "Agentfs-Collision";

/// One collision record as its result commit's trailer holds it: the deciding
/// tree's branch name, the file, the cause, the losing change and its ref, the
/// lines, and the lost and winning versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub tree: String,
    pub path: String,
    pub by: String,
    pub loser: String,
    pub reference: String,
    pub lines: Vec<String>,
    pub lost: Vec<String>,
    pub won: Vec<String>,
}

impl Decision {
    pub fn of(c: &Collision) -> Self {
        Self {
            tree: World::name(c.tree),
            path: c.path.clone(),
            by: format!("{:?}", c.by),
            loser: c.loser.clone(),
            reference: c.reference.clone(),
            lines: c.lines.clone(),
            lost: c.lost.clone(),
            won: c.won.clone(),
        }
    }

    fn render(&self) -> String {
        format!(
            "{}: {} {} {} {} {} {} {} {}\n",
            COLLISION,
            self.tree,
            self.path,
            self.by,
            self.loser,
            self.reference,
            self.lines.join(","),
            self.lost.join(","),
            self.won.join(",")
        )
    }

    fn parse(s: &str) -> Option<Self> {
        let f: Vec<&str> = s.split(' ').collect();
        let list = |x: &str| x.split(',').map(str::to_string).collect();
        let [tree, path, by, loser, reference, lines, lost, won] = f[..] else {
            return None;
        };
        Some(Self {
            tree: tree.into(),
            path: path.into(),
            by: by.into(),
            loser: loser.into(),
            reference: reference.into(),
            lines: list(lines),
            lost: list(lost),
            won: list(won),
        })
    }
}

/// Every collision record in the commits `rev` lists (`git log`'s arguments)
/// in `repo`, with the commit that holds it: read from git alone.
pub fn decisions_in(repo: &Path, rev: &[&str]) -> Vec<(String, Decision)> {
    let format =
        format!("--format=%H%x1f%(trailers:key={COLLISION},valueonly,unfold,separator=%x1f)");
    let mut args = vec!["log", format.as_str()];
    args.extend(rev);
    let mut out = Vec::new();
    for line in git(repo, &args).lines() {
        let mut fields = line.split('\x1f');
        let on = fields.next().expect("a commit");
        for rec in fields.map(str::trim).filter(|r| !r.is_empty()) {
            out.push((
                on.to_string(),
                Decision::parse(rec).expect("a collision record in a commit's trailers"),
            ));
        }
    }
    out
}

pub struct World {
    pub policies: Policies,
    root: PathBuf,
    repo: PathBuf,
    /// The shared remote (a bare repository): Replication pushes main and
    /// every collision ref there.
    remote: PathBuf,
    root_head: String,
    pub forks: BTreeMap<TreeId, Fork>,
    next_fork: u32,
    next_txn: TxnId,
    /// Each tree's queue, its waiting entries and its deferred calls (rules 2 and 9, R5.3).
    queues: Queues,
    /// Opens, their objects and calls, and what their saves recorded (rules 8, 10, 11).
    recording: Recording,
    /// The last queue step's take, if it took: for the checks.
    last_take: Option<(TreeId, TakeRecord<Merged>)>,
    /// The files each landing commit's own changes touch, cached.
    touches: RefCell<BTreeMap<String, BTreeSet<String>>>,
    pub collisions: Vec<Collision>,
    /// Every token that entered a tree: written in it, carried into it by a
    /// landing or a transaction (whether it won or lost there), or taken
    /// from the upstream by a sync.
    pub entered: Vec<(TreeId, Edit)>,
    /// Every sync that moved a fork: (the fork, the upstream's head it
    /// carried, the fork's head after, `entered.len()` when it began).
    pub syncs: Vec<(TreeId, String, String, usize)>,
    /// Every take of changes into a tree, a transaction's commit included.
    pub landings: Vec<Landed>,
    /// Landings that were fast-forwards.
    pub fast_forwards: usize,
    /// Tokens of rolled-back transactions.
    pub discarded: BTreeSet<String>,
    pub ended: Vec<Ended>,
    pub log: Vec<String>,
    /// The repository's files, through the one object cache.
    objects: GitFiles,
    /// Each version's writing commit, and each commit's parents.
    origins: RefCell<BTreeMap<String, String>>,
    parents: RefCell<BTreeMap<String, Vec<String>>>,
    ancestry: RefCell<BTreeMap<(String, String), bool>>,
    over: RefCell<BTreeMap<String, Vec<String>>>,
    /// Main moved, or a collision was recorded, since Replication last
    /// pushed.
    replication_due: bool,
    /// Syncs that delivered only a prefix of the upstream's commits (P2),
    /// counted so a run can show it met them.
    pub prefix_syncs: usize,
    /// The calls made, as [`crate::calls`] records them, when recording.
    trace: Option<Vec<Recorded>>,
    /// Inside a call that records itself as one: nothing it does records.
    trace_mute: bool,
    /// What tells this world's trace file from another's (a seed).
    trace_label: Option<String>,
}

/// The trailer naming the open a save came from (git-model.md §3).
pub const OPEN: &str = "Agentfs-Open";

/// Recording's port over the repository: each file at a commit or tree, read
/// once with `git cat-file` and cached (objects never change). The world's one
/// object cache.
struct GitFiles {
    repo: PathBuf,
    objects: RefCell<BTreeMap<String, Content>>,
}

impl Files for GitFiles {
    fn file(&self, at: &str, p: &str) -> Content {
        let spec = format!("{at}:{p}");
        if let Some(c) = self.objects.borrow().get(&spec) {
            return c.clone();
        }
        let c = Content::parse(&git(&self.repo, &["cat-file", "-p", &spec]))
            .expect("a model file in git");
        self.objects.borrow_mut().insert(spec, c.clone());
        c
    }
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
        let objects = GitFiles {
            repo: repo.clone(),
            objects: RefCell::new(BTreeMap::new()),
        };
        let mut w = Self {
            policies,
            root,
            repo,
            remote,
            root_head: String::new(),
            forks: BTreeMap::new(),
            next_fork: 1,
            next_txn: 1,
            queues: Queues::default(),
            recording: Recording::new(),
            last_take: None,
            touches: RefCell::new(BTreeMap::new()),
            collisions: Vec::new(),
            entered: Vec::new(),
            syncs: Vec::new(),
            landings: Vec::new(),
            fast_forwards: 0,
            discarded: BTreeSet::new(),
            ended: Vec::new(),
            log: Vec::new(),
            objects,
            origins: RefCell::new(BTreeMap::new()),
            parents: RefCell::new(BTreeMap::new()),
            ancestry: RefCell::new(BTreeMap::new()),
            over: RefCell::new(BTreeMap::new()),
            replication_due: false,
            prefix_syncs: 0,
            trace: std::env::var_os(TRACE_DIR).map(|_| Vec::new()),
            trace_mute: false,
            trace_label: None,
        };
        let files = PATHS
            .iter()
            .map(|p| (p.to_string(), Content::root(p)))
            .collect();
        let tree = w.tree_with(None, &files);
        let c = w.commit(&[], &tree, "agentfs", "Root", "main");
        w.set_head(TreeId::ROOT, &c);
        if !matches!(policies.merge, MergeRule::PerLine) || policies.layout != Layout::Separated {
            w.trace_call(out_of_scope("policy"));
        }
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

    /// Tree `t`'s head. Panics on a tree never made.
    pub fn head(&self, t: TreeId) -> String {
        if t == TreeId::ROOT {
            self.root_head.clone()
        } else {
            self.forks[&t].head.clone()
        }
    }

    /// The tree `t` syncs from and lands into; none for the root. Panics on a
    /// tree never made.
    pub fn upstream_of(&self, t: TreeId) -> Option<TreeId> {
        (t != TreeId::ROOT).then(|| self.forks[&t].upstream)
    }

    /// The ref holding `t`'s head.
    pub fn branch(t: TreeId) -> String {
        format!("refs/heads/{}", Self::name(t))
    }

    pub(crate) fn name(t: TreeId) -> String {
        if t == TreeId::ROOT {
            "main".into()
        } else {
            format!("fork/{}", t.number())
        }
    }

    /// Transaction `id`'s name, as its commits' `Agentfs-Tree` gives it. With
    /// [`World::name`] and [`World::branch`] the one place that names a tree.
    pub(crate) fn txn_name(id: TxnId) -> String {
        format!("txn/{id}")
    }

    /// The ref holding transaction `id`'s head.
    pub(crate) fn txn_branch(id: TxnId) -> String {
        format!("refs/heads/{}", Self::txn_name(id))
    }

    /// How many transactions began in this run (each has its own id, from 1).
    pub fn txns_begun(&self) -> TxnId {
        self.next_txn - 1
    }

    /// What tree `t` shows at path `p`: the file named `p` there.
    pub fn read(&self, t: TreeId, p: &str) -> Content {
        self.read_at(&self.head(t), p)
    }

    fn read_at(&self, at: &str, p: &str) -> Content {
        let f = self
            .lookup(at, p)
            .unwrap_or_else(|| panic!("no file named {p} at {at:.7}"));
        self.file(at, &f)
    }

    /// The identity of the file named `p` at commit `at`, if one is.
    pub fn lookup(&self, at: &str, p: &str) -> Option<String> {
        recording::lookup(&self.objects, at, p)
    }

    /// The path file `f` has in tree `t`; none if unlinked there.
    pub fn path_of(&self, t: TreeId, f: &str) -> Option<String> {
        self.file(&self.head(t), f).path().map(str::to_string)
    }

    /// The file at path `p` where client `c` reads, if any.
    fn client_lookup(&self, c: &Client, p: &str) -> Option<String> {
        self.lookup(&self.target_head(self.target_of(c)), p)
    }

    /// The path file `f` has where client `c` reads (its open
    /// transaction's branch, else its tree); none if unlinked there.
    pub fn client_path(&self, c: &Client, f: &str) -> Option<String> {
        let at = self.target_head(self.target_of(c));
        self.file(&at, f).path().map(str::to_string)
    }

    /// What a client's program reads at path `p`: its open transaction's
    /// branch (its own staged edits on the frozen snapshot), else its tree.
    pub fn client_reads(&self, c: &Client, p: &str) -> Content {
        self.read_at(&self.target_head(self.target_of(c)), p)
    }

    /// File `p` (its identity) at a commit or tree id, cached: objects
    /// never change.
    pub fn file(&self, at: &str, p: &str) -> Content {
        self.objects.file(at, p)
    }

    pub fn queue_len(&self, t: TreeId) -> usize {
        self.queues.len(t)
    }

    pub fn queue(&self, t: TreeId) -> Vec<Entry> {
        self.queues.entries(t)
    }

    /// The queue with each entry's number.
    pub fn queue_numbered(&self, t: TreeId) -> Vec<(u64, Entry)> {
        self.queues.numbered(t)
    }

    /// Every open file, and every deferred path call (R5.3).
    pub fn handles(&self) -> impl Iterator<Item = &Handle> {
        self.recording.handles()
    }

    pub fn handle(&self, h: HandleId) -> &Handle {
        self.recording.handle(h)
    }

    /// Every `OrphanDropped` event.
    pub fn orphans(&self) -> &[recording::Orphan] {
        self.recording.orphans()
    }

    /// Each save's write calls, as the program made them, by the save's
    /// commit: `check` reads rule 8's `O_APPEND` from them and the blobs,
    /// not from the replay.
    pub fn save_calls(&self) -> &BTreeMap<String, Vec<Write>> {
        self.recording.save_calls()
    }

    /// Main's clients that ended (rule 9: their end event released their
    /// locks).
    pub fn ended_main(&self) -> &BTreeSet<String> {
        self.recording.ended_main()
    }

    /// The take the last queue step made, with its tree, if it took one: what
    /// it locked and wrote (rule 9). None after a step that only waited.
    pub fn last_take(&self) -> Option<&(TreeId, TakeRecord<Merged>)> {
        self.last_take.as_ref()
    }

    /// Each tree whose queue head waits, with the files it waits for.
    pub fn waits(&self) -> &BTreeMap<TreeId, BTreeMap<u64, Waiting>> {
        self.queues.waits()
    }

    /// The files tree `t`'s waiting entries wait for: those of their files
    /// open for writing there.
    pub fn waiting_for(&self, t: TreeId) -> BTreeSet<String> {
        self.queues.waiting_for(t, &self.write_open(t))
    }

    /// R5.3: the calls deferred behind fork `f`'s transaction, in the order made.
    pub fn deferred(&self, f: TreeId) -> Vec<HandleId> {
        self.queues.deferred(f).collect()
    }

    /// What a client of `t` has open for writing, and what its
    /// deferred calls (R5.3) will save: each file and the name it has now (R9.1,
    /// [`queue::NAME_KEY`]). An open whose object no path names (an orphan) has no file open.
    pub fn write_open(&self, t: TreeId) -> BTreeSet<String> {
        let at = self.head(t);
        self.recording
            .open_files(t, &at, &self.objects)
            .into_iter()
            .flat_map(|f| [name_key(&self.file(&at, &f)), Some(f)])
            .flatten()
            .collect()
    }

    /// The file open `h`'s object is in its target now: the live file
    /// whose object it is; none if no path names it (an orphan: its file
    /// was unlinked, or replaced by a rename-over or an atomic save, there).
    pub fn named(&self, h: &Handle) -> Option<String> {
        let at = self.target_head(h.target);
        self.recording.named(h, &at, &self.objects)
    }

    pub fn alive(&self, t: TreeId) -> bool {
        match self.upstream_of(t) {
            None => true,
            Some(_) => self.forks[&t].life != Life::Dropped,
        }
    }

    /// Every tree made in this run, dropped ones included: the root, then the
    /// forks in the order made.
    pub fn trees(&self) -> Vec<TreeId> {
        std::iter::once(TreeId::ROOT)
            .chain(self.forks.keys().copied())
            .collect()
    }

    pub fn live_trees(&self) -> Vec<TreeId> {
        let mut v = vec![TreeId::ROOT];
        v.extend(
            self.forks
                .values()
                .filter(|f| f.life != Life::Dropped)
                .map(|f| f.id),
        );
        v
    }

    /// `t` and every tree upstream of it, up to main.
    pub fn upstream(&self, t: TreeId) -> Vec<TreeId> {
        let mut v = vec![t];
        while let Some(u) = v.last().and_then(|t| self.upstream_of(*t)) {
            v.push(u);
        }
        v
    }

    fn children(&self, t: TreeId) -> Vec<TreeId> {
        self.forks
            .values()
            .filter(|f| f.upstream == t && f.life != Life::Dropped)
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
        self.commit_by(parents, tree, writer, cause, on, None)
    }

    /// [`World::commit`]; a save from an open names it in one more trailer,
    /// `Agentfs-Open: <n>` (git-model.md §3), so a crash's replay of the
    /// journal never saves an open twice.
    fn commit_by(
        &self,
        parents: &[&str],
        tree: &str,
        writer: &str,
        cause: &str,
        on: &str,
        open: Option<HandleId>,
    ) -> String {
        let mut msg = message(&format!("{cause} by {writer}"), writer, cause, on);
        if let Some(h) = open {
            msg.push_str(&format!("{OPEN}: {h}\n"));
        }
        self.commit_message(parents, tree, &msg)
    }

    /// A take's result commit, its decisions in its trailers (R5.1, R9.9).
    fn commit_deciding(
        &self,
        parents: &[&str],
        tree: &str,
        writer: &str,
        cause: &str,
        on: &str,
        decisions: &[Decision],
    ) -> String {
        let mut msg = message(&format!("{cause} by {writer}"), writer, cause, on);
        msg.extend(decisions.iter().map(Decision::render));
        self.commit_message(parents, tree, &msg)
    }

    fn commit_message(&self, parents: &[&str], tree: &str, msg: &str) -> String {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        git_in(&self.repo, &args, msg)
    }

    /// `git update-ref`.
    fn update_ref(&mut self, args: &[&str]) {
        let mut v = vec!["update-ref"];
        v.extend(args);
        git(&self.repo, &v);
    }

    fn set_head(&mut self, t: TreeId, c: &str) {
        self.update_ref(&[&Self::branch(t), c]);
        match self.upstream_of(t) {
            None => {
                // Replication pushes main once it moved: the event its push
                // waits for.
                self.replication_due = true;
                self.root_head = c.to_string();
            }
            Some(_) => self.forks.get_mut(&t).expect("fork").head = c.to_string(),
        }
    }

    fn tree_of(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^{{tree}}")])
    }

    fn first_parent(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^1")])
    }

    /// Whether commit `a` is `b` or an ancestor of it (cached: commits never
    /// change).
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

    /// The commit that wrote version `id` of file `p`, from commit `at`,
    /// which holds it ([`origin_in`]). Cached: a version is written once.
    fn origin_of(&self, at: &str, p: &str, id: &str) -> String {
        if let Some(o) = self.origins.borrow().get(id) {
            return o.clone();
        }
        let parents = |c: &str| {
            if let Some(v) = self.parents.borrow().get(c) {
                return v.clone();
            }
            let v = commit_parents(&self.repo, c);
            self.parents.borrow_mut().insert(c.to_string(), v.clone());
            v
        };
        let o = origin_in(at, parents, |c| self.file(c, p).holds(id));
        self.origins.borrow_mut().insert(id.to_string(), o.clone());
        o
    }

    /// The versions version `v` of file `p` (written by commit `origin`)
    /// was written over, newest first: the one its writer saw (on
    /// `origin`'s first parent), the one that one was written over, and so
    /// on, read from git (the records, first parents and `git blame`). Never
    /// from which commits a version merged: a sync merge that kept the fork's
    /// version over the upstream's showed the fork's writer no upstream's version
    /// (R3c.2: it could not read it), so the fork's next edit is not written
    /// over that. Cached: versions never change.
    fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String> {
        if let Some(c) = self.over.borrow().get(&v.id) {
            return c.clone();
        }
        let chain = match &v.over {
            None => Vec::new(),
            Some(o) => {
                let up = self.first_parent(origin);
                let prev = self.file(&up, p);
                let rec = prev
                    .records()
                    .find(|l| l.id == *o)
                    .unwrap_or_else(|| {
                        panic!("{} was written over {o}, which {up:.7}:{p} lacks", v.id)
                    })
                    .clone();
                let mut chain = vec![o.clone()];
                chain.extend(self.written_over(&self.origin_of(&up, p, o), p, &rec));
                chain
            }
        };
        self.over.borrow_mut().insert(v.id.clone(), chain.clone());
        chain
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

    /// The line versions `b` holds that `a` does not. A file created and
    /// ended between them (unborn in `a`, ended in `b`, as a transaction's
    /// atomic save leaves its new file) changed nothing anyone can see:
    /// none of its versions.
    pub fn edits(&self, a: &str, b: &str) -> Vec<Edit> {
        let mut out = Vec::new();
        for p in self.changed(a, b) {
            let (x, y) = (self.file(a, &p), self.file(b, &p));
            if !y.name.live() && x.name.id == p && UNBORN.contains(&p.as_str()) {
                continue;
            }
            for r in y.records() {
                if x.member(&r.line).is_none_or(|l| l.id != r.id) {
                    out.push(Edit {
                        path: p.clone(),
                        line: r.line.clone(),
                        token: r.id.clone(),
                        gone: !r.live(),
                    });
                }
            }
        }
        out
    }

    /// Rules 3, 10 and 11 at the upstream's queue: the referee's plan
    /// ([`referee::land`]) for one change into the upstream's head `ours`, its
    /// tree built here.
    fn resolve(&self, dir: Direction, ours: &str, incoming: &[Incoming], tip: &str) -> Merged {
        let referee::Plan { files, collisions } = referee::land(self, dir, ours, incoming, tip);
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

    /// The versions a commit's own edit sets: its diff from `base`, each
    /// with its writing commit.
    fn edit_of(&self, base: &str, c: &str) -> Vec<Incoming> {
        self.edits(base, c)
            .into_iter()
            .map(|e| Incoming {
                origin: self.origin_of(c, &e.path, &e.token),
                version: self
                    .file(c, &e.path)
                    .member(&e.line)
                    .expect("the version it set")
                    .clone(),
                path: e.path,
            })
            .collect()
    }

    /// [`MergeRule::PerLine`]'s sync: the referee's plan ([`referee::sync`]),
    /// its tree built here over `up`.
    fn adopt(&self, ours: &str, up: &str) -> String {
        let files = referee::sync(self, ours, up, self.changed(ours, up));
        let tree = if files.is_empty() {
            self.tree_of(up)
        } else {
            self.tree_with(Some(up), &files)
        };
        self.no_clash(&tree);
        tree
    }

    /// Two live files never share a path (the referee's names keep
    /// it so in a sync); the model stops loudly if one would.
    fn no_clash(&self, tree: &str) {
        let mut seen = BTreeSet::new();
        for f in PATHS {
            if let Some(p) = self.file(tree, f).path() {
                assert!(
                    seen.insert(p.to_string()),
                    "two files named {p} in {tree:.7}: two renames to one path (not modelled)"
                );
            }
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
        // Only content conflicts arise, which `-X` resolves: a model file
        // is never deleted or renamed in git (rule 11 keeps both in its
        // name line).
        assert_eq!(o.code, 0, "merge-tree -Xours resolves: {}", o.stderr);
        o.stdout.lines().next().expect("a tree").to_string()
    }

    /// A take's decisions in tree `t`, before its result commit, which holds
    /// them: one decision per change that lost (rule 3a), kept reachable under
    /// one `refs/collisions/<n>`, with one record under that ref for each file
    /// and cause it lost by.
    fn decide(&mut self, t: TreeId, m: &Merged) -> Vec<Decision> {
        let mut refs: BTreeMap<&str, String> = BTreeMap::new();
        let mut out = Vec::new();
        for l in &m.collisions {
            let reference = match refs.get(l.loser.as_str()) {
                Some(r) => r.clone(),
                None => {
                    let made: BTreeSet<&str> = self
                        .collisions
                        .iter()
                        .map(|c| c.reference.as_str())
                        .collect();
                    let r = format!("refs/collisions/{}", made.len() + 1);
                    self.update_ref(&[&r, &l.loser]);
                    refs.insert(&l.loser, r.clone());
                    r
                }
            };
            out.push(Decision {
                tree: Self::name(t),
                path: l.path.clone(),
                by: format!("{:?}", l.by),
                loser: l.loser.clone(),
                reference,
                lines: l.lines.clone(),
                lost: l.lost.clone(),
                won: l.won.clone(),
            });
        }
        out
    }

    /// Records a take's collisions in tree `t`, its decisions being in its
    /// result commit.
    fn record(
        &mut self,
        t: TreeId,
        dir: Direction,
        m: &Merged,
        decisions: &[Decision],
        winner: &str,
        result: &str,
    ) {
        for (l, d) in m.collisions.iter().zip(decisions) {
            let Lost {
                path,
                lines,
                lost,
                won,
                by,
                loser,
            } = l;
            self.note(format!(
                "collision in {t:?}:{path}{lines:?} ({dir:?}, {by:?}): {loser:.7}'s {lost:?} lost to {won:?}"
            ));
            self.collisions.push(Collision {
                tree: t,
                direction: dir,
                path: path.clone(),
                lines: lines.clone(),
                lost: lost.clone(),
                won: won.clone(),
                by: *by,
                winner: winner.to_string(),
                loser: loser.clone(),
                result: result.to_string(),
                reference: d.reference.clone(),
            });
            self.replication_due = true;
        }
    }

    /// Replication's push ([`Step::Replicate`]): main, whose history holds
    /// every take's decisions, and every collision ref to the shared remote in one
    /// atomic push (owner,
    /// 2026-11-03: "yes, \"losing\" patches should still be part of the
    /// history. i want to know who changed what when"). Replication is the
    /// remote's one pusher; no referee step pushes. Never forced: main only
    /// moves forward, and collision refs never move.
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
            ],
        );
    }

    /// `t`'s head moved: a fork's new commit queues for its upstream (`up`),
    /// and the new content queues for every child (rule 1: both ways).
    fn moved(&mut self, t: TreeId, up: bool) {
        let head = self.head(t);
        let children = self.children(t);
        self.queues
            .moved(t, &head, self.upstream_of(t), &children, up);
    }

    // ── clients ──────────────────────────────────────────────────────────

    /// A launcher creates a fork over `upstream`, a branch at its head.
    pub fn create_fork(&mut self, upstream: TreeId) -> TreeId {
        let id = TreeId::new(self.next_fork);
        self.next_fork += 1;
        let head = self.head(upstream);
        self.forks.insert(
            id,
            Fork {
                id,
                upstream,
                writer: format!("c{id}"),
                life: Life::Open,
                head: head.clone(),
                txn: None,
            },
        );
        self.set_head(id, &head);
        self.note(format!("create f{id} over {upstream:?}"));
        self.trace_call(match self.upstream_of(upstream) {
            None => RecordedCall::Fork { id },
            Some(_) => out_of_scope("nested-fork"),
        });
        id
    }

    /// Rule 1: open `p` for writing, write line `slot` (a program's byte
    /// calls, [`World::edit`]), close: the version written.
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let h = self.open_write(c, p);
        let token = self.edit(h, LineEdit::At(slot)).expect("a new version");
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
            Client::Outside(f) => (self.forks[f].writer.clone(), None),
        }
    }

    /// Where a client's calls go: its open transaction, else its tree.
    fn target_of(&self, c: &Client) -> Target {
        match c {
            Client::Main(_) => Target::Tree(TreeId::ROOT),
            Client::Fork(f) => match &self.forks[f].txn {
                Some(t) => Target::Txn(*f, t.id),
                None => Target::Tree(*f),
            },
            Client::Outside(f) => Target::Tree(*f),
        }
    }

    /// The head a write-open's save applies to: its tree's, or its
    /// transaction's.
    pub fn target_head(&self, t: Target) -> String {
        match t {
            Target::Tree(t) => self.head(t),
            Target::Txn(f, _) => self.forks[&f].txn.as_ref().expect("open").head.clone(),
        }
    }

    /// An `O_TRUNC` open: [`World::open_write`] whose first call truncates the
    /// file to nothing (applied at close, with the rest: close-to-open).
    pub fn open_trunc(&mut self, c: &Client, p: &str) -> HandleId {
        let h = self.open_handle(c, p);
        self.recording.truncate_at_open(h);
        self.note(format!("h{h}: O_TRUNC"));
        self.record_open(h);
        h
    }

    /// Rule 9: open `p` for writing, in the client's open transaction if it
    /// has one, else in its tree, at once: a take is one step, so none holds
    /// the file between calls (the owner, 2026-11-08, finding 49: "B"), and
    /// no open waits on a waiting take (R9.2). The open names the file by its
    /// identity, so it follows the file through a rename.
    pub fn open_write(&mut self, c: &Client, p: &str) -> HandleId {
        let h = self.open_handle(c, p);
        self.record_open(h);
        h
    }

    fn open_handle(&mut self, c: &Client, p: &str) -> HandleId {
        let target = self.target_of(c);
        let file = self
            .lookup(&self.target_head(target), p)
            .unwrap_or_else(|| {
                panic!("open_write of {p}: no such file ([`World::create`] makes one)")
            });
        let id = self.recording.open(c, target, file, p, Kind::Write);
        self.note(format!("open h{id} {c:?} {target:?}:{p}"));
        id
    }

    /// Rule 8: one byte call on an open. The open keeps its calls; close
    /// applies them.
    pub fn write(&mut self, h: HandleId, call: Write) {
        self.note(format!("write h{h}: {call:?}"));
        let at = u64::from(h);
        let recorded = match &call {
            Write::Append(b) => RecordedCall::Append {
                h: at,
                bytes: b.clone().into_bytes(),
            },
            Write::Pwrite { offset, bytes } => RecordedCall::Pwrite {
                h: at,
                offset: *offset as u64,
                bytes: bytes.clone().into_bytes(),
            },
            Write::Truncate(n) => RecordedCall::Truncate {
                h: at,
                size: *n as u64,
            },
        };
        self.recording.write(h, call);
        self.trace_call(recorded);
    }

    /// What the program reads through open `h`: its file at the open's
    /// target head now, with its own calls applied (close-to-open: no
    /// other writer's unsaved calls).
    pub fn view(&self, h: HandleId) -> String {
        let at = self.target_head(self.recording.handle(h).target);
        self.recording.view(h, &at, &self.objects)
    }

    /// A program's line edit on open `h`, made as the byte calls a program
    /// makes ([`program::calls`] on [`World::view`]); a new line's text is
    /// a version id reserved now, returned, which the save gives the
    /// version holding it.
    pub fn edit(&mut self, h: HandleId, edit: LineEdit) -> Option<String> {
        let makes = matches!(
            edit,
            LineEdit::Append | LineEdit::At(_) | LineEdit::Insert(_)
        );
        let token = makes.then(|| {
            let (writer, txn) = self.handle_writer(self.recording.handle(h));
            self.recording.reserve(h, &writer, txn)
        });
        let text = token.clone().unwrap_or_default();
        for c in program::calls(&self.view(h), &edit, &text) {
            self.write(h, c);
        }
        token
    }

    /// [`World::edit`], the new line holding `text`: bytes that may repeat
    /// other lines', as blank lines and braces do.
    pub fn edit_text(&mut self, h: HandleId, edit: LineEdit, text: &str) {
        for c in program::calls(&self.view(h), &edit, text) {
            self.write(h, c);
        }
    }

    /// Rule 8, the save at close: the write calls applied, in order, to
    /// the head of the open's tree (or transaction) now (recording's replay):
    /// every line the program's bytes leave unchanged keeps its version, a line a sync brought
    /// or another writer's append included. One commit, first parent that
    /// head, at once; none if nothing changed, or if the file was unlinked
    /// meanwhile (as on Linux, its write calls reach no path). Then a take
    /// waiting for this close is woken. A close of an open transaction's
    /// file returns at once and saves after it ends (R5.3).
    pub fn close(&mut self, h: HandleId) {
        let handle = self.recording.handle(h);
        assert!(
            !handle.deferred && handle.kind == Kind::Write,
            "close an open"
        );
        if self.defer(h) {
            return;
        }
        let handle = self.recording.remove(h).expect("an open file");
        let target = handle.target;
        let tokens = self.save_handle(&handle);
        self.note(format!("close h{h}: saved {tokens:?}"));
        if let Target::Tree(t) = target {
            self.wake(t);
        }
        self.trace_call(RecordedCall::Close { h: u64::from(h) });
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

    /// Recording's save of `handle` at close, committed (rule 1); its new
    /// versions.
    fn save_handle(&mut self, handle: &Handle) -> Vec<String> {
        let (writer, txn) = self.handle_writer(handle);
        let at = self.target_head(handle.target);
        match self
            .recording
            .save(handle, &writer, txn, &at, &self.objects)
        {
            Saved::Orphaned(bytes) => {
                if let Some(bytes) = bytes {
                    self.note(format!(
                        "close h{}: orphan, {bytes} bytes dropped",
                        handle.id
                    ));
                }
                Vec::new()
            }
            Saved::NoChange => Vec::new(),
            Saved::Save(save) => {
                let tokens = save.set.iter().map(|e| e.token.clone()).collect();
                let c = self.commit_save(save);
                self.recording.record_calls(c, handle.calls().to_vec());
                tokens
            }
        }
    }

    /// A save's commit: on the target tree, first parent its head `at`,
    /// moving its branch and queueing the move both ways (rule 1); or on
    /// the open transaction's branch, staged.
    fn commit_save(&mut self, save: Save) -> String {
        let Save {
            target,
            writer,
            at,
            files,
            set,
            open,
        } = save;
        let (writer, at) = (writer.as_str(), at.as_str());
        let t = self.tree_with(Some(at), &files);
        match target {
            Target::Txn(f, id) => {
                let on = Self::txn_name(id);
                let commit = self.commit_by(&[at], &t, writer, "Direct", &on, open);
                self.update_ref(&[&Self::txn_branch(id), &commit]);
                let txn = self.forks.get_mut(&f).expect("fork").txn.as_mut();
                let txn = txn.expect("open");
                txn.head = commit.clone();
                txn.staged.extend(set);
                commit
            }
            Target::Tree(tree) => {
                let c = self.commit_by(&[at], &t, writer, "Direct", &Self::name(tree), open);
                self.set_head(tree, &c);
                self.entered.extend(set.into_iter().map(|e| (tree, e)));
                self.moved(tree, true);
                c
            }
        }
    }

    /// Rule 11: unlink the file at `p`, in the client's open transaction if
    /// it has one, else in its tree: one save writing a deleted version of
    /// its name line. One call, done at once, or deferred behind a
    /// transaction that has the file (R5.3). R9.7's wait for a take holding
    /// the file is the order itself: a take is one step. The client's own
    /// opens of the file stay open: as on Linux, their write calls then reach
    /// no path and save nothing.
    pub fn unlink(&mut self, c: &Client, p: &str) {
        self.path_call(c, p, Kind::Unlink);
    }

    /// Rule 11: rename the file at `from` to `to`: one save writing a new
    /// version of its name line. If a file is named `to`, it is a
    /// rename-over: the same save unlinks that file (its name line deleted,
    /// naming the file that replaced it). Done at once, as [`World::unlink`].
    /// The file keeps its identity, so its opens and lines follow it.
    pub fn rename(&mut self, c: &Client, from: &str, to: &str) {
        let at = self.target_head(self.target_of(c));
        let over = self.lookup(&at, to);
        let kind = Kind::Rename {
            to: to.to_string(),
            over,
        };
        self.path_call(c, from, kind);
    }

    /// A path call runs, or returns at once and runs after the transaction
    /// that has its file ends (R5.3).
    fn path_call(&mut self, c: &Client, p: &str, kind: Kind) {
        let target = self.target_of(c);
        let file = self
            .lookup(&self.target_head(target), p)
            .unwrap_or_else(|| panic!("{kind:?} of {p}: no such file"));
        let id = self.recording.open(c, target, file, p, kind);
        if !self.defer(id) {
            let h = self.recording.remove(id).expect("a path call");
            self.run_path_call(&h);
        }
    }

    fn run_path_call(&mut self, h: &Handle) {
        let (writer, txn) = self.handle_writer(h);
        let at = self.target_head(h.target);
        let Some((save, from)) = self
            .recording
            .path_save(h, &writer, txn, &at, &self.objects)
        else {
            return;
        };
        let tokens: Vec<&String> = save.set.iter().map(|e| &e.token).collect();
        self.note(format!("{:?} {:?}: saved {tokens:?}", h.client, h.kind));
        self.commit_save(save);
        // An open of a file this call unlinked or replaced holds an object
        // no path names now, so no file of it is open: a take waiting for
        // that close is woken.
        if let Target::Tree(t) = h.target {
            self.wake(t);
        }
        let caller = self.caller_of(&h.client);
        self.trace_call(match &h.kind {
            Kind::Rename { to, .. } => RecordedCall::Rename {
                caller,
                from,
                to: to.clone(),
            },
            _ => RecordedCall::Unlink { caller, path: from },
        });
    }

    /// Rule 11: `open(p, O_CREAT | O_EXCL | O_WRONLY)` of a path no file
    /// has: a new file, its identity the next of [`UNBORN`] (each taken once
    /// in the volume), made at once by one save of its name line, written
    /// over the unborn version, with no lines; and a granted open on it. The
    /// only way the model makes a file. None once every unborn identity is
    /// taken.
    pub fn create(&mut self, c: &Client, p: &str) -> Option<HandleId> {
        let target = self.target_of(c);
        let at = self.target_head(target);
        let writer = self.who(c).0;
        let txn = match target {
            Target::Txn(_, t) => Some(t),
            Target::Tree(_) => None,
        };
        let save = self
            .recording
            .create(p, &writer, txn, target, &at, &self.objects)?;
        let (f, id) = (save.set[0].path.clone(), save.set[0].token.clone());
        self.note(format!(
            "create {c:?} {target:?}:{p} as {f}: saved [{id:?}]"
        ));
        self.commit_save(save);
        let h = self.recording.open(c, target, f, p, Kind::Write);
        let caller = self.caller_of(c);
        self.trace_call(RecordedCall::Create {
            caller,
            path: p.to_string(),
            h: u64::from(h),
        });
        Some(h)
    }

    /// A close or a client's death in `t`: each waiting entry there whose
    /// `until` files are all closed stops waiting; the next queue step on
    /// `t` takes it, or finds it must wait again (P1). Nothing waits behind
    /// it meanwhile (P3 is gone).
    fn wake(&mut self, t: TreeId) {
        let open = self.write_open(t);
        for n in self.queues.wake(t, &open) {
            self.note(format!("wake {t:?}'s entry {n}"));
        }
    }

    // ── transactions (rule 5) ────────────────────────────────────────────

    /// Begin: a branch frozen at the fork's head. The fork keeps receiving
    /// syncs; the program reads its own staged edits on the snapshot. A
    /// file open before begin keeps writing to the fork (its target is
    /// fixed at open); only the client's opens after begin write in the
    /// transaction.
    pub fn begin(&mut self, f: TreeId) -> Result<TxnId, ModelError> {
        self.trace_call(out_of_scope("transaction"));
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
        self.update_ref(&[&Self::txn_branch(id), &start]);
        self.forks.get_mut(&f).expect("fork").txn = Some(Txn {
            id,
            start: start.clone(),
            head: start,
            staged: Vec::new(),
        });
        self.note(format!("begin t{id} on f{f}"));
        Ok(id)
    }

    /// The files open transaction `f` holds (R5.3): those it has saved
    /// (its branch differs from its start in them), those it has open for
    /// writing, and those of the calls already deferred, so a file's calls
    /// keep their order.
    fn txn_files(&self, f: TreeId) -> BTreeSet<String> {
        let fork = &self.forks[&f];
        let txn = fork.txn.as_ref().expect("open");
        let mut files = self.changed(&txn.start, &txn.head);
        files.extend(
            self.recording
                .handles()
                .filter(|h| h.target == Target::Txn(f, txn.id))
                .filter_map(|h| self.named(h)),
        );
        for h in self.queues.deferred(f) {
            files.extend(self.deferred_files(self.recording.handle(h)));
        }
        files
    }

    /// The files call `h` acts on: an open's, the file its object is named
    /// by; a path call's, its file and a rename's target.
    fn deferred_files(&self, h: &Handle) -> Vec<String> {
        match h.kind {
            Kind::Write => self.named(h).into_iter().collect(),
            _ => h.files(),
        }
    }

    /// R5.3 (the owner, 2026-11-05: "deferred, obviously"): call `h`, a close
    /// or a path call in a fork's tree, acts on a file of the fork's open
    /// transaction: it returns at once and goes on its fork's deferred list ([`World::deferred`]), true.
    /// Nothing waits on a program.
    fn defer(&mut self, h: HandleId) -> bool {
        let handle = self.recording.handle(h);
        let Target::Tree(f) = handle.target else {
            return false;
        };
        if self.upstream_of(f).is_none() || self.forks[&f].txn.is_none() {
            return false;
        }
        let mine = self.txn_files(f);
        if !self.deferred_files(handle).iter().any(|x| mine.contains(x)) {
            return false;
        }
        self.note(format!(
            "h{h} {:?} of {}: deferred behind the transaction",
            handle.kind, handle.file
        ));
        self.recording.set_deferred(h);
        self.queues.defer(f, h);
        true
    }

    /// R5.3: the next deferred call of fork `f` applies, its transaction
    /// having ended: a save as any later close saves, on the tree's head
    /// now, a path call as it would have run. None when none is left.
    pub fn apply_deferred(&mut self, f: TreeId) -> Option<HandleId> {
        assert!(
            self.forks[&f].txn.is_none(),
            "a deferred call during its transaction"
        );
        let id = self.queues.next_deferred(f)?;
        let h = self.recording.remove(id).expect("a deferred call");
        if h.kind == Kind::Write {
            let tokens = self.save_handle(&h);
            self.note(format!("deferred close h{id}: saved {tokens:?}"));
        } else {
            self.note(format!("deferred h{id}: it runs"));
            self.run_path_call(&h);
        }
        self.wake(f);
        Some(id)
    }

    /// Commit (MODEL.md rule 5), then the calls it deferred (R5.3).
    pub fn commit_txn(&mut self, f: TreeId) -> Result<TxnOutcome, ModelError> {
        let outcome = self.commit_txn_only(f)?;
        while self.apply_deferred(f).is_some() {}
        Ok(outcome)
    }

    /// The commit alone: the transaction's branch lands in the fork's
    /// current head as a child's change lands in its upstream, as one merge
    /// commit named for the client, which then lands like any fork commit.
    /// Never refused. The fork is the transaction's upstream, so a fork line
    /// concurrent with a staged line (one a sync brought in since begin)
    /// wins, and the transaction loses whole (rule 3e): none of its lines
    /// apply, the merge keeps the fork's tree (so history shows the decision)
    /// and the answer names every staged line. It goes ahead while programs hold
    /// files open (the owner, 2026-11-05: R5.3): those files stay open, a
    /// transaction's opens now write in the fork, and each saves at its close
    /// as an ordinary later save.
    pub fn commit_txn_only(&mut self, f: TreeId) -> Result<TxnOutcome, ModelError> {
        if self.forks[&f].txn.is_none() {
            return Err(ModelError::NoTransaction);
        }
        let fork = &self.forks[&f];
        let ours = fork.head.clone();
        let txn = fork.txn.as_ref().expect("open");
        let (id, start, head) = (txn.id, txn.start.clone(), txn.head.clone());
        let planned = (head != start).then(|| {
            let edit = self.edit_of(&start, &head);
            let m = self.resolve(Direction::Transaction, &ours, &edit, &head);
            (edit, m)
        });
        let stay_open: Vec<(HandleId, Option<String>)> = self
            .recording
            .handles()
            .filter(|h| h.target == Target::Txn(f, id))
            .map(|h| (h.id, self.named(h)))
            .collect();
        let txn = self.take_txn(f)?;
        let mut result = None;
        let outcome = if let Some((edit, m)) = planned {
            let writer = self.forks[&f].writer.clone();
            let t = f;
            let decisions = self.decide(t, &m);
            let c = self.commit_deciding(
                &[&ours, &txn.head],
                &m.tree,
                &writer,
                &format!("Transaction t{}", txn.id),
                &Self::name(t),
                &decisions,
            );
            self.record(t, Direction::Transaction, &m, &decisions, &ours, &c);
            self.landings.push(Landed {
                tree: t,
                direction: Direction::Transaction,
                before: ours.clone(),
                result: c.clone(),
                change: txn.head.clone(),
                base: txn.start.clone(),
                won: m.collisions.is_empty(),
            });
            self.set_head(t, &c);
            for e in edit {
                self.entered.push((t, e.edit()));
            }
            self.moved(t, true);
            result = Some(c);
            let lines: Vec<Overwritten> = m
                .collisions
                .iter()
                .flat_map(|l| {
                    l.lines
                        .iter()
                        .zip(l.lost.iter().zip(&l.won))
                        .map(|(line, (lost, won))| Overwritten {
                            path: l.path.clone(),
                            line: line.clone(),
                            staged: lost.clone(),
                            kept: won.clone(),
                        })
                })
                .collect();
            if lines.is_empty() {
                TxnOutcome::Committed
            } else {
                TxnOutcome::Lost { lines }
            }
        } else {
            TxnOutcome::Empty
        };
        for (h, file) in stay_open {
            self.recording.retarget(h, Target::Tree(f), file);
        }
        self.note(format!("commit t{} on f{f}: {outcome:?}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Commit(outcome.clone()),
            result,
        });
        self.wake(f);
        Ok(outcome)
    }

    /// Rollback (abandon), then the calls it deferred (R5.3).
    pub fn rollback(&mut self, f: TreeId) -> Result<(), ModelError> {
        self.rollback_only(f)?;
        while self.apply_deferred(f).is_some() {}
        Ok(())
    }

    /// The rollback alone: drop the branch, and its files still open, unsaved.
    pub fn rollback_only(&mut self, f: TreeId) -> Result<(), ModelError> {
        let txn = self.take_txn(f)?;
        self.recording.drop_opens_of(Target::Txn(f, txn.id));
        self.discard(&txn);
        self.note(format!("rollback t{} on f{f}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Rollback,
            result: None,
        });
        self.wake(f);
        Ok(())
    }

    fn discard(&mut self, txn: &Txn) {
        self.discarded
            .extend(txn.staged.iter().map(|e| e.token.clone()));
    }

    fn take_txn(&mut self, f: TreeId) -> Result<Txn, ModelError> {
        let txn = self
            .forks
            .get_mut(&f)
            .expect("fork")
            .txn
            .take()
            .ok_or(ModelError::NoTransaction)?;
        self.update_ref(&["-d", &Self::txn_branch(txn.id)]);
        Ok(txn)
    }

    /// A fork's client leaves or dies (its end event): an open transaction
    /// is dropped with its open files, and every lock it held is released
    /// ([`World::release`]). The fork's commits still land; it drops once
    /// that is done.
    pub fn end_client(&mut self, f: TreeId) {
        let muted = std::mem::replace(&mut self.trace_mute, true);
        if self.forks[&f].txn.is_some() {
            let _ = self.rollback(f);
        }
        self.release(&[Client::Fork(f), Client::Outside(f)]);
        self.forks.get_mut(&f).expect("fork").life = Life::Ended;
        self.wake(f);
        self.note(format!("end f{f}"));
        self.trace_mute = muted;
        self.trace_call(RecordedCall::EndClient {
            caller: Caller::Fork(f),
        });
    }

    /// One of main's clients exits or dies (its end event): every lock it
    /// held is released ([`World::release`]), so main's queue, waiting for
    /// its open file, is woken. It opens nothing after.
    pub fn end_main_client(&mut self, writer: &str) {
        let muted = std::mem::replace(&mut self.trace_mute, true);
        self.release(&[Client::Main(writer.to_string())]);
        self.recording.end_main_client(writer);
        self.wake(TreeId::ROOT);
        self.note(format!("end main client {writer}"));
        self.trace_mute = muted;
        self.trace_call(RecordedCall::EndClient {
            caller: Caller::Main(writer.to_string()),
        });
    }

    /// A client's end event releases its locks: its open files close as the
    /// kernel closes them on exit, each saving its write calls (a choice),
    /// which wakes a take waiting for them.
    fn release(&mut self, clients: &[Client]) {
        let mine: Vec<HandleId> = self
            .recording
            .handles()
            .filter(|h| clients.contains(&h.client))
            .map(|h| h.id)
            .collect();
        for h in mine {
            self.close(h);
        }
    }

    // ── crash and the journal (git-model.md §3) ──────────────────────────

    /// Every agentfs process dies at once (a view host, the history
    /// service, the node). Each write call was in its tree's journal before
    /// it returned (git-model.md §3; the model keeps an open's journaled
    /// calls on its handle), so at restart, before any tree is served
    /// again, each open is saved from the journal as its close would have
    /// saved it, unless a save already names it (`Agentfs-Open`), and the
    /// opens end: their descriptors died with the mount. An open no path
    /// names raises its `OrphanDropped` event again (no save records it in
    /// git: events are at least once). A call a transaction deferred did
    /// return, so it stays in the journal behind that transaction, which
    /// survives; an open whose close would defer defers (R5.3). A take is one
    /// step (finding 49), so none is in progress; waits are dropped and found
    /// again at the next take. Transactions survive: their branches are refs.
    pub fn crash(&mut self) {
        for h in self.crash_down() {
            self.restart_open(h);
        }
    }

    /// [`World::crash`]'s first part: every process is down; waits are
    /// dropped. Returns the opens the restart replays from the journal, in
    /// order.
    pub fn crash_down(&mut self) -> Vec<HandleId> {
        self.note("crash: every agentfs process dies; restart replays the journal".into());
        self.queues.clear_waits();
        let replay = self
            .recording
            .handles()
            .filter(|h| !h.deferred)
            .map(|h| h.id)
            .collect();
        self.trace_call(RecordedCall::Crash);
        replay
    }

    /// [`World::crash`]'s restart, for one open: saved from the journal as
    /// its close would save it, unless a save already names it; it ends.
    pub fn restart_open(&mut self, id: HandleId) {
        self.restart_open_unshown(id);
        self.reshow();
    }

    fn restart_open_unshown(&mut self, id: HandleId) {
        if self.saved(id) {
            self.recording.remove(id);
            self.note(format!("crash: h{id} already saved: skipped"));
            return;
        }
        if self.defer(id) {
            return;
        }
        let h = self.recording.replayed(id);
        let tokens = self.save_handle(&h);
        self.note(format!(
            "crash: h{id} replayed from the journal: saved {tokens:?}"
        ));
    }

    /// A crash during open `h`'s close: its save is committed (its branch
    /// moved), but its journaled calls are not yet dropped; then
    /// [`World::crash`], whose replay finds the save naming `h` and skips it.
    pub fn close_then_crash(&mut self, h: HandleId) {
        self.close_keeping_journal(h);
        self.crash();
    }

    /// An open's close whose save is committed but whose journal records
    /// are not dropped yet: the open stays, for a crash to replay.
    pub fn close_keeping_journal(&mut self, h: HandleId) {
        let handle = self.recording.handle(h).clone();
        assert!(
            !handle.deferred && handle.kind == Kind::Write,
            "close an open"
        );
        if self.defer(h) {
            return;
        }
        let tokens = self.save_handle(&handle);
        self.note(format!(
            "close h{h}: saved {tokens:?}; crash before its journal drops it"
        ));
        self.trace_call(RecordedCall::Close { h: u64::from(h) });
    }

    /// Whether a save names open `h` (`Agentfs-Open`), read from git.
    pub fn saved(&self, h: HandleId) -> bool {
        let grep = format!("--grep=^{OPEN}: {h}$");
        !git(&self.repo, &["rev-list", "--all", &grep]).is_empty()
    }

    // ── steps ────────────────────────────────────────────────────────────

    /// Every step that can act now: a take in each live tree whose queue
    /// has an entry P1 lets go, and each fork ready to drop.
    pub fn pending(&self) -> Vec<Step> {
        let mut out: Vec<Step> = self
            .queues
            .queued_trees()
            .into_iter()
            .filter(|t| self.alive(*t) && self.queues.scan(*t, self).is_some())
            .map(Step::Next)
            .collect();
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
    /// [`Step::Next`] takes the first its queue lets go and writes it, or
    /// none when it waits (a prefix sync writes and leaves its entry
    /// queued). Nothing is held by a collision.
    pub fn step(&mut self, s: &Step) -> usize {
        match s {
            Step::Next(t) => {
                self.last_take = None;
                let choice = self.queues.choose(*t, self);
                self.note_choice(*t, &choice);
                self.queues.apply(*t, &choice);
                let take = match choice {
                    Choice::Wait { .. } => None,
                    Choice::Prefix { take, .. } => {
                        self.prefix_syncs += 1;
                        Some(take)
                    }
                    Choice::Take { take, .. } => Some(take),
                };
                let mut taken = 0;
                if let Some(take) = take {
                    taken = usize::from(take.seq.is_some());
                    self.finish(*t, &take);
                    self.last_take = Some((*t, take));
                }
                self.record_take(*t);
                taken
            }
            Step::Drop(f) => {
                self.trace_call(out_of_scope("drop"));
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

    /// Logs a take's choice in tree `t`.
    fn note_choice(&mut self, t: TreeId, choice: &Choice<Merged>) {
        let s = match choice {
            Choice::Wait { seq, waiting } => {
                format!("{t:?}'s entry {seq} waits for {:?} to close", waiting.until)
            }
            Choice::Prefix { seq, waiting, take } => {
                let Take::Sync { cut, head, .. } = &take.take else {
                    unreachable!("only a sync delivers a prefix")
                };
                format!(
                    "{t:?}'s entry {seq} delivers {:.7} of {head:.7}, holds {:?}, and waits for {:?} to close for the rest",
                    cut.up(),
                    take.files,
                    waiting.until
                )
            }
            Choice::Take { take, .. } => format!(
                "{t:?} takes entry {}, locking {:?}",
                take.seq.unwrap_or_default(),
                take.files
            ),
        };
        self.note(s);
    }

    /// The files a fork commit's own changes touch (each change against its
    /// first parent) and their names on both sides (R9.1), cached: commits never change.
    fn touches_of(&self, c: &str) -> BTreeSet<String> {
        if let Some(x) = self.touches.borrow().get(c) {
            return x.clone();
        }
        let files = self.changed_keys(&self.first_parent(c), c);
        self.touches
            .borrow_mut()
            .insert(c.to_string(), files.clone());
        files
    }

    /// Rule 9, the same step's write: the planned change, on the head it was
    /// planned on (nothing ran in between), writing only files it locked (the
    /// model stops loudly if not); then an entry waiting in `t` whose files
    /// closed meanwhile is woken (a prefix sync's rest).
    fn finish(&mut self, t: TreeId, take: &TakeRecord<Merged>) {
        let entered = self.entered.len();
        match &take.take {
            Take::Sync { cut, landing, .. } => {
                let files = cut
                    .tree()
                    .map(|tr| self.changed(&self.head(t), tr))
                    .unwrap_or_default();
                queue::held(t, &files, &take.files);
                self.sync(t, cut, *landing, entered);
            }
            Take::Landing { from, commit, plan } => {
                queue::held(t, &plan.files, &take.files);
                self.land(t, *from, commit, plan.clone());
            }
        }
        self.wake(t);
    }

    /// The files tree `tree` changes from commit `ours`.
    fn changed(&self, ours: &str, tree: &str) -> BTreeSet<String> {
        git(&self.repo, &["diff-tree", "-r", "--name-only", ours, tree])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// What going from `ours` to `tree` locks (R9, R9.1): each file it changes, and
    /// that file's name on each side, because two files can compete for a name.
    fn changed_keys(&self, ours: &str, tree: &str) -> BTreeSet<String> {
        self.keys_of(self.changed(ours, tree), ours, tree)
    }

    /// `files` as lock keys: each file, and its name in `ours` and in `tree`.
    fn keys_of(&self, files: BTreeSet<String>, ours: &str, tree: &str) -> BTreeSet<String> {
        let names: Vec<String> = files
            .iter()
            .flat_map(|f| [name_key(&self.file(ours, f)), name_key(&self.file(tree, f))])
            .flatten()
            .collect();
        files.into_iter().chain(names).collect()
    }

    /// Rule 3e, one take of commit `c` into the tree at `ours`: the change
    /// judged whole ([`World::resolve`]).
    fn judge(&self, ours: &str, c: &str) -> Merged {
        self.resolve(
            Direction::Landing,
            ours,
            &self.edit_of(&self.first_parent(c), c),
            c,
        )
    }

    /// Rule 3, a fork's change into its upstream `x`, the one place a
    /// collision between them is decided. A fast-forward when the commit's
    /// base is the target's head, else the change is judged whole
    /// ([`World::judge`]): the upstream's line wins a collision, and the
    /// change that lost it applies nothing. `m`: the take planned on `x`'s
    /// current head.
    fn land(&mut self, x: TreeId, from: TreeId, tip: &str, m: Merged) {
        let ours = self.head(x);
        let base = self.first_parent(tip);
        let incoming = self.edit_of(&base, tip);
        let carried: Vec<&str> = incoming.iter().map(|i| i.version.id.as_str()).collect();
        let won = m.collisions.is_empty();
        self.note(format!(
            "landing f{from}'s {tip:.7} into {x:?} carries {carried:?}; won: {won}"
        ));
        for i in &incoming {
            self.entered.push((x, i.edit()));
        }
        if base == ours {
            // Made on the upstream's current version: every line replaces
            // the upstream's, so nothing collides.
            assert!(
                won && m.tree == self.tree_of(tip),
                "a fast-forward is a plain replacement"
            );
            self.set_head(x, tip);
            self.fast_forwards += 1;
            self.landed(x, &ours, tip, tip, won);
            self.note(format!(
                "landed f{from}'s {tip:.7} into {x:?}: fast-forward"
            ));
            self.moved(x, true);
            return;
        }
        let writer = self.writer_of_commit(tip);
        let decisions = self.decide(x, &m);
        let new = self.commit_deciding(
            &[&ours, tip],
            &m.tree,
            &writer,
            "Landing",
            &Self::name(x),
            &decisions,
        );
        self.record(x, Direction::Landing, &m, &decisions, &ours, &new);
        self.landed(x, &ours, &new, tip, won);
        self.note(format!("landed f{from}'s {tip:.7} into {x:?}"));
        self.set_head(x, &new);
        self.moved(x, true);
    }

    /// Records a take's change and whether it won, for `check`.
    fn landed(&mut self, x: TreeId, before: &str, result: &str, change: &str, won: bool) {
        self.landings.push(Landed {
            tree: x,
            direction: Direction::Landing,
            before: before.to_string(),
            result: result.to_string(),
            change: change.to_string(),
            base: self.first_parent(change),
            won,
        });
    }

    /// The files in which the upstream's commit `c` wrote a line version the
    /// fork at `ours` has never had, so its line map changes at this sync
    /// even if the fork's file goes on reading the same (finding 38).
    fn new_lines_of(&self, ours: &str, c: &str) -> BTreeSet<String> {
        let before = self.first_parent(c);
        self.changed(&before, c)
            .into_iter()
            .filter(|p| {
                let (was, now) = (self.file(&before, p), self.file(c, p));
                now.lines.iter().any(|l| {
                    !was.holds(&l.id) && !self.is_ancestor(&self.origin_of(c, p, &l.id), ours)
                })
            })
            .collect()
    }

    /// The lock keys delivering the upstream's commit `c` into a fork at
    /// `ours` writes, `tree` the sync's tree (none: the fork has `c`): the
    /// files that tree changes, and those whose line map `c` changes there
    /// (finding 38), each with its names (R9.1).
    fn delivery_keys(&self, ours: &str, c: &str, tree: Option<&str>) -> BTreeSet<String> {
        let mut now: BTreeSet<String> = tree
            .map(|tr| self.changed_keys(ours, tr))
            .unwrap_or_default();
        let after = tree.unwrap_or(ours);
        now.extend(self.keys_of(self.new_lines_of(ours, c), ours, after));
        now
    }

    /// The tree a sync of the upstream's `up` into fork `x` (at `ours`)
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

    /// Rule 3, upstream into fork: the upstream's decided version `up`, read
    /// when the take began (never the one current when the sync was
    /// queued). It decides nothing: each fork line the upstream's queue has
    /// not taken yet stays until that queue decides it. Nothing if the
    /// fork has the upstream's version; the upstream's head itself (a
    /// fast-forward: the fork adopts the upstream's decisions) when no fork
    /// line or commit is still waiting for the upstream; else a merge commit
    /// labelled `Sync` with the upstream's head as second parent.
    /// `planned`: the tree planned on the fork's current head (none: it has
    /// `up`); `landing`: a fork commit was on its way to the upstream when the
    /// take began, so `up` may lack it.
    fn sync(&mut self, x: TreeId, cut: &Cut, landing: bool, entered: usize) {
        let (up, planned) = (cut.up(), cut.tree().map(str::to_string));
        let f = x;
        let Some(upstream) = self.upstream_of(f) else {
            unreachable!("the root has no upstream")
        };
        let ours = self.head(x);
        let Some(tree) = planned else {
            return;
        };
        for e in self.edits(&ours, &tree) {
            self.entered.push((x, e));
        }
        let waiting = landing || self.queues.landing_pending(upstream, f);
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

    fn droppable(&self, f: TreeId) -> bool {
        let fork = &self.forks[&f];
        fork.life == Life::Ended
            && self.children(f).is_empty()
            && !self.queues.waits().contains_key(&f)
            && !self.queues.landing_pending(fork.upstream, f)
    }

    fn drop_fork(&mut self, f: TreeId) {
        assert!(self.droppable(f), "drop f{f} when ready");
        self.update_ref(&["-d", &Self::branch(f)]);
        self.queues.forget(f);
        self.forks.get_mut(&f).expect("fork").life = Life::Dropped;
        self.note(format!("drop f{f}"));
    }

    // ── recording (crate::calls) ─────────────────────────────────────────

    /// A world that records its calls whatever the environment says.
    pub fn recording(policies: Policies) -> Self {
        let mut w = Self::new(policies);
        w.trace.get_or_insert_with(Vec::new);
        w
    }

    /// The calls recorded so far: none unless recording.
    pub fn trace(&self) -> &[Recorded] {
        self.trace.as_deref().unwrap_or_default()
    }

    /// Names this world's trace file apart from others of its test.
    pub fn label_trace(&mut self, label: String) {
        self.trace_label = Some(label);
    }

    fn trace_call(&mut self, call: RecordedCall) {
        if self.trace.is_none() || self.trace_mute {
            return;
        }
        // Every call, a take included, is whole when it returns: the trace is
        // the order the calls ran in (finding 49).
        let shown = Some(self.shown());
        if let Some(trace) = &mut self.trace {
            trace.push(Recorded {
                call,
                shown,
                head: None,
            });
        }
    }

    /// What the last call shows now, for a call the model makes in parts.
    fn reshow(&mut self) {
        if self.trace.is_none() || self.trace_mute {
            return;
        }
        let shown = Some(self.shown());
        if let Some(last) = self.trace.as_mut().and_then(|t| t.last_mut()) {
            last.shown = shown;
        }
    }

    /// A take, recorded after it wrote. A queue step that took nothing while
    /// another entry is free is not a call: it only marks a wait the real
    /// system finds again by itself, and a real `take_next` goes on to the
    /// free entry in one step.
    fn record_take(&mut self, t: TreeId) {
        let took = if self.last_take.is_some() {
            Took::Taken
        } else if self.queues.scan(t, self).is_some() {
            return;
        } else {
            Took::Waiting
        };
        let moved = (took == Took::Taken).then(|| self.head_shape(t));
        self.trace_call(RecordedCall::Take { tree: t, took });
        if let (Some(head), false, Some(last)) = (
            moved,
            self.trace_mute,
            self.trace.as_mut().and_then(|t| t.last_mut()),
        ) {
            last.head = Some(head);
        }
    }

    /// The `Agentfs-Cause` of tree `t`'s head and its number of parents.
    fn head_shape(&self, t: TreeId) -> (String, usize) {
        let head = self.head(t);
        let body = git(&self.repo, &["log", "-1", "--format=%B", &head]);
        let cause = body
            .lines()
            .find_map(|l| l.strip_prefix("Agentfs-Cause: "))
            .unwrap_or_default()
            .to_owned();
        let parents = git(&self.repo, &["rev-list", "--parents", "-n", "1", &head]);
        (cause, parents.split_whitespace().count().saturating_sub(1))
    }

    /// An open, recorded under the name its file has in its tree.
    fn record_open(&mut self, h: HandleId) {
        if self.trace.is_none() || self.trace_mute {
            return;
        }
        let x = self.recording.handle(h).clone();
        if x.kind != Kind::Write {
            return;
        }
        let at = u64::from(h);
        let Some(path) = self
            .named(&x)
            .and_then(|f| self.path_of(tree_of(x.target), &f))
        else {
            self.trace_call(RecordedCall::OpenGone { h: at });
            return;
        };
        let caller = self.caller_of(&x.client);
        self.trace_call(if x.calls().is_empty() {
            RecordedCall::OpenWrite {
                caller,
                path,
                h: at,
            }
        } else {
            RecordedCall::OpenTrunc {
                caller,
                path,
                h: at,
            }
        });
    }

    fn caller_of(&mut self, c: &Client) -> Caller {
        match c {
            Client::Main(n) => Caller::Main(n.clone()),
            Client::Fork(f) => Caller::Fork(*f),
            Client::Outside(f) => {
                self.trace_call(out_of_scope("outside"));
                Caller::Fork(*f)
            }
        }
    }

    /// What every live tree shows by path, and every collision so far.
    pub fn shown(&self) -> Shown {
        let trees = self
            .live_trees()
            .into_iter()
            .map(|t| {
                let head = self.head(t);
                let files = PATHS
                    .iter()
                    .filter_map(|f| {
                        let c = self.file(&head, f);
                        c.path().map(|p| (p.to_string(), c.bytes().into_bytes()))
                    })
                    .collect();
                (t, files)
            })
            .collect();
        let collisions = gather_collisions(self.collisions.iter().map(|c| {
            let lost = self.file(&c.loser, &c.path);
            let path = self
                .file(&c.result, &c.path)
                .path()
                .or(lost.path())
                .unwrap_or_default()
                .to_string();
            ShownCollision {
                tree: c.tree,
                path,
                lost_bytes: c
                    .lost
                    .iter()
                    .filter_map(|t| lost.lines.iter().find(|l| l.id == *t))
                    .filter_map(|l| l.text())
                    .map(|t| t.as_bytes().to_vec())
                    .collect(),
                whole: c.by == By::Whole,
            }
        }));
        Shown { trees, collisions }
    }

    /// Every ref's commits, for history checks.
    pub fn all_commits(&self) -> Vec<String> {
        git(&self.repo, &["rev-list", "--all"])
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// The commit that wrote version `id`, reached from commit `at`, which
/// holds it: from `at`, step to a parent that holds it (`holds`) until none
/// does. Exact whatever the bytes repeat or the lines moved, since a
/// version is unique and written once; where the line maps hold one
/// unique version per git line, `git blame --porcelain` answers the same.
/// The model's blobs repeat separator lines, which let git's diff (and so
/// blame) show a record whose neighbours moved as rewritten, so it walks.
pub(crate) fn origin_in(
    at: &str,
    parents: impl Fn(&str) -> Vec<String>,
    holds: impl Fn(&str) -> bool,
) -> String {
    let mut c = at.to_string();
    while let Some(q) = parents(&c).into_iter().find(|q| holds(q)) {
        c = q;
    }
    c
}

/// A commit's parents in `repo`.
pub(crate) fn commit_parents(repo: &Path, c: &str) -> Vec<String> {
    git(repo, &["rev-list", "--parents", "-n1", c])
        .split(' ')
        .skip(1)
        .map(str::to_string)
        .collect()
}

fn tree_of(t: Target) -> TreeId {
    match t {
        Target::Tree(t) => t,
        Target::Txn(f, _) => f,
    }
}

fn client_of(c: &Caller) -> Client {
    match c {
        Caller::Main(n) => Client::Main(n.clone()),
        Caller::Fork(f) => Client::Fork(*f),
    }
}

fn no_such(what: &str, path: &str) -> CallError {
    CallError {
        errno: Some(libc_enoent()),
        message: format!("{what} of {path}: no such file"),
    }
}

/// `ENOENT`: this crate has no dependencies, so not from a libc binding.
const fn libc_enoent() -> i32 {
    2
}

fn text(bytes: &[u8]) -> Result<String, CallError> {
    String::from_utf8(bytes.to_vec()).map_err(|e| CallError::new(e.to_string()))
}

/// The model through the shared calls: the same replay that drives the real
/// system drives a fresh model (`phase1-replay`'s own check of the recording).
impl TreeCalls for World {
    type Open = HandleId;

    fn fork(&mut self) -> Result<TreeId, CallError> {
        Ok(self.create_fork(TreeId::ROOT))
    }

    fn open_write(&mut self, c: &Caller, path: &str) -> Result<HandleId, CallError> {
        let client = client_of(c);
        if self.client_lookup(&client, path).is_none() {
            return Err(no_such("open", path));
        }
        Ok(World::open_write(self, &client, path))
    }

    fn open_trunc(&mut self, c: &Caller, path: &str) -> Result<HandleId, CallError> {
        let client = client_of(c);
        if self.client_lookup(&client, path).is_none() {
            return Err(no_such("open", path));
        }
        Ok(World::open_trunc(self, &client, path))
    }

    fn create(&mut self, c: &Caller, path: &str) -> Result<HandleId, CallError> {
        World::create(self, &client_of(c), path)
            .ok_or_else(|| CallError::new("every unborn identity is taken"))
    }

    fn pwrite(&mut self, h: HandleId, offset: u64, bytes: &[u8]) -> Result<(), CallError> {
        self.write(
            h,
            Write::Pwrite {
                offset: offset as usize,
                bytes: text(bytes)?,
            },
        );
        Ok(())
    }

    fn append(&mut self, h: HandleId, bytes: &[u8]) -> Result<(), CallError> {
        self.write(h, Write::Append(text(bytes)?));
        Ok(())
    }

    fn truncate(&mut self, h: HandleId, size: u64) -> Result<(), CallError> {
        self.write(h, Write::Truncate(size as usize));
        Ok(())
    }

    fn close(&mut self, h: HandleId) -> Result<(), CallError> {
        World::close(self, h);
        Ok(())
    }

    fn unlink(&mut self, c: &Caller, path: &str) -> Result<(), CallError> {
        let client = client_of(c);
        if self.client_lookup(&client, path).is_none() {
            return Err(no_such("unlink", path));
        }
        World::unlink(self, &client, path);
        Ok(())
    }

    fn rename(&mut self, c: &Caller, from: &str, to: &str) -> Result<(), CallError> {
        let client = client_of(c);
        if self.client_lookup(&client, from).is_none() {
            return Err(no_such("rename", from));
        }
        World::rename(self, &client, from, to);
        Ok(())
    }

    fn end_client(&mut self, c: &Caller) -> Result<(), CallError> {
        match c {
            Caller::Main(n) => self.end_main_client(n),
            Caller::Fork(f) => World::end_client(self, *f),
        }
        Ok(())
    }

    fn take_next(&mut self, t: TreeId) -> Result<Took, CallError> {
        if self.queue_len(t) == 0 {
            self.trace_call(RecordedCall::Take {
                tree: t,
                took: Took::Empty,
            });
            return Ok(Took::Empty);
        }
        let mut stepped = false;
        while self.queues.scan(t, self).is_some() {
            stepped = true;
            self.step(&Step::Next(t));
            if self.last_take.is_some() {
                return Ok(Took::Taken);
            }
        }
        // The last step recorded its wait; none ran if a wait was already marked.
        if !stepped {
            self.trace_call(RecordedCall::Take {
                tree: t,
                took: Took::Waiting,
            });
        }
        Ok(Took::Waiting)
    }

    fn crash(&mut self) -> Result<(), CallError> {
        World::crash(self);
        Ok(())
    }

    fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
        let head = self.head(t);
        Ok(self
            .lookup(&head, path)
            .map(|f| self.file(&head, &f).bytes().into_bytes()))
    }

    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError> {
        let head = self.head(t);
        let mut paths: Vec<String> = PATHS
            .iter()
            .filter_map(|f| self.file(&head, f).path().map(str::to_string))
            .collect();
        paths.sort();
        Ok(paths)
    }
}

/// Where `World::drop` writes a recorded world's trace.
const TRACE_DIR: &str = "AGENTFS_MODEL_TRACE_DIR";

/// `<thread name>[.<label>][.<n>]`, a world's name for its trace and digest
/// files: the n-th world of that name in this process counts from 1 past the
/// first. A world takes one name, whichever of the two it writes.
fn run_name(label: Option<&str>) -> String {
    static NAMES: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());
    let thread = std::thread::current();
    let mut name = thread.name().unwrap_or("unnamed").replace("::", "-");
    if let Some(l) = label {
        name = format!("{name}.{l}");
    }
    let n = {
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        let n = names.entry(name.clone()).or_insert(0);
        *n += 1;
        *n
    };
    if n > 1 {
        name = format!("{name}.{n}");
    }
    name
}

impl Drop for World {
    fn drop(&mut self) {
        let trace_dir = std::env::var_os(TRACE_DIR).filter(|_| self.trace.is_some());
        let digest_dir = std::env::var_os(crate::digest::DIGEST_DIR);
        if !std::thread::panicking() && (trace_dir.is_some() || digest_dir.is_some()) {
            let name = run_name(self.trace_label.as_deref());
            if let (Some(dir), Some(trace)) = (trace_dir, &self.trace) {
                let file = Path::new(&dir).join(format!("{name}.trace"));
                // A trace that cannot be written must fail the job, not a floor later.
                std::fs::create_dir_all(&dir)
                    .and_then(|()| std::fs::write(&file, crate::calls::render(trace)))
                    .unwrap_or_else(|e| panic!("write {}: {e}", file.display()));
            }
            if let Some(dir) = digest_dir {
                crate::digest::write(self, Path::new(&dir), &name);
            }
        }
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl referee::Lineage for World {
    fn file(&self, at: &str, p: &str) -> Content {
        World::file(self, at, p)
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        World::is_ancestor(self, a, b)
    }

    fn origin_of(&self, at: &str, p: &str, id: &str) -> String {
        World::origin_of(self, at, p, id)
    }

    fn written_over(&self, origin: &str, p: &str, v: &Line) -> Vec<String> {
        World::written_over(self, origin, p, v)
    }
}

/// Queue and locks reads the world through this port.
impl Keys for World {
    fn head(&self, t: TreeId) -> String {
        World::head(self, t)
    }

    fn upstream_of(&self, t: TreeId) -> Option<TreeId> {
        World::upstream_of(self, t)
    }

    fn write_open(&self, t: TreeId) -> BTreeSet<String> {
        World::write_open(self, t)
    }

    fn touches_of(&self, c: &str) -> BTreeSet<String> {
        World::touches_of(self, c)
    }

    fn changed_keys(&self, ours: &str, tree: &str) -> BTreeSet<String> {
        World::changed_keys(self, ours, tree)
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        World::is_ancestor(self, a, b)
    }

    fn first_parent(&self, c: &str) -> String {
        World::first_parent(self, c)
    }

    fn delivery_keys(&self, ours: &str, c: &str, tree: Option<&str>) -> BTreeSet<String> {
        World::delivery_keys(self, ours, c, tree)
    }
}

impl Plans for World {
    type Plan = Merged;

    fn sync_tree(&self, ours: &str, up: &str) -> Option<String> {
        World::sync_tree(self, ours, up)
    }

    /// Rule 3e's plan of landing `c` on `ours` ([`World::judge`]) and the files it writes.
    fn plan(&self, ours: &str, c: &str) -> (Merged, BTreeSet<String>) {
        let m = self.judge(ours, c);
        let files = m.files.clone();
        (m, files)
    }
}
