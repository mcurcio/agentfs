//! The model: main and each fork as a git branch, each save a commit naming
//! one writer and holding the file exactly as its write calls left it, and
//! the owner's rules on top (MODEL.md). Every merge is git's own (`git
//! merge-tree`, merge-ort), always from an explicit base, so git never
//! picks a merge base and no merge has two.
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | a close ([`World::close`]) is never refused; its save is a commit on the writer's tree at once, its first parent the tree's head then |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`]s per tree; a [`Step::Next`] takes its head, or records that it waits (rule 9) |
//! | 3. The parent's queue decides | `land` is the only place a collision is decided: git's three-way merge of the landing commit's own change (its first parent as the base) into the parent's head, the parent's side winning each conflicting hunk (`-X ours`). A [`Collision`] names each such hunk. `sync` rebases the fork's commits the parent has not taken onto the parent's head, the parent's side winning a hunk there too, and records nothing |
//! | 4. Nothing waits behind a collision | no held state: the fork's losing commit is kept under `refs/collisions/<n>`, which Replication pushes ([`Step::Replicate`]) |
//! | 5. Transactions | [`World::begin`]: a branch frozen at the fork's head; [`World::commit_txn`] merges it into the fork from its start, the fork's side winning a conflicting hunk, and answers with those hunks; [`World::rollback`] drops it |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a rebased commit keeps its writer and names its original in `Agentfs-Rebased` |
//! | 7. History setting | [`HistoryPolicy`]: a landing is a fast-forward or a merge with the fork's commit second, or one single-parent commit whose carried fork commit is a `git replace` graft |
//! | 8. A save records the write calls | [`World::write`] keeps each call on the open file; [`World::close`] applies them to the tree's head then and commits the bytes |
//! | 9. A take locks the files it overwrites | as before: a take waits for write-opens of the paths it changes, then holds them until [`Step::Finish`] |
//! | 11. Deletes and renames | a path leaves the tree or moves in it; git's rename detection pairs them in a merge; a path conflict keeps the parent's side |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git history.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{File, Oracle, PATHS, root, tracked};
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
    /// second parent: main's ancestry reaches every fork commit.
    FullLineage,
    /// A landing is one single-parent commit: main's history holds only
    /// main's own commits. A fork's plain saves waiting in the parent's
    /// queue when it takes the first land as one changeset. The last fork
    /// commit it carried is a graft beside history (`refs/replace/`), so a
    /// sync can tell which fork commits the parent has taken (finding 2).
    Squashed,
}

/// How much of the losing side a conflicting hunk costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionScope {
    /// Only the conflicting hunks: git's `-X ours`. The model's choice.
    Hunk,
    /// The whole file: a file with a conflicting hunk keeps the parent's
    /// whole version (coherence finding 3).
    File,
}

#[derive(Clone, Copy, Debug)]
pub struct Policies {
    pub history: HistoryPolicy,
    pub collision: CollisionScope,
}

impl Policies {
    pub fn new(history: HistoryPolicy, collision: CollisionScope) -> Self {
        Self { history, collision }
    }
}

impl Default for Policies {
    fn default() -> Self {
        Self::new(HistoryPolicy::FullLineage, CollisionScope::Hunk)
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
    /// The versions it staged (the oracle's ids), in order.
    pub staged: Vec<String>,
}

pub struct Fork {
    pub id: ForkId,
    pub parent: Tree,
    /// Its one client's writer name.
    pub writer: String,
    pub life: Life,
    pub head: String,
    /// The parent's head it last synced to (or was created at).
    pub base: String,
    pub txn: Option<Txn>,
}

/// A queued change into a tree (rule 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A child fork's commit, to land in this tree. Always the commit the
    /// save (or landing, or transaction commit) made, never a rebased copy.
    Landing { from: ForkId, commit: String },
    /// The parent moved: rebase onto its current head.
    Sync,
}

/// Where a collision was decided: always a parent's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Landing,
    /// A transaction's branch into its fork, at commit.
    Transaction,
}

/// One conflicting hunk git resolved for the parent (rule 3), or a path
/// conflict (rule 11), or under [`CollisionScope::File`] a whole file.
#[derive(Clone, Debug)]
pub struct Collision {
    pub tree: Tree,
    pub direction: Direction,
    pub path: String,
    /// The parent's lines in the hunk, which won (none: the path was not
    /// the parent's).
    pub ours: File,
    /// The incoming lines in the hunk, which lost.
    pub theirs: File,
    /// A path conflict or a whole-file loss, not a hunk of lines.
    pub by_file: bool,
    /// The merge's base: the incoming change's first parent.
    pub base: String,
    /// The tree's head before: the parent's version, which won.
    pub winner: String,
    /// The incoming commit, which lost: kept reachable by `reference`.
    pub loser: String,
    /// The tree's commit after the landing.
    pub result: String,
    pub reference: String,
}

/// How a transaction ended (rule 5).
#[derive(Clone, Debug)]
pub struct Ended {
    pub fork: ForkId,
    pub txn: TxnId,
    pub staged: Vec<String>,
    pub how: TxnEnd,
    /// The transaction's last commit, and the fork's commit that carries it.
    pub head: String,
    pub result: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnEnd {
    Commit(TxnOutcome),
    /// Rollback, abandon, or the client's death.
    Rollback,
}

/// What a transaction's commit tells the program (rule 5). No collision
/// refuses it; the answer names every hunk the fork's side won.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    Committed {
        overwritten: Vec<Overwritten>,
    },
    /// It staged nothing.
    Empty,
}

impl TxnOutcome {
    pub fn clean() -> Self {
        Self::Committed {
            overwritten: Vec::new(),
        }
    }
}

/// A staged hunk a transaction's commit did not apply: the fork's lines
/// `kept` won it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overwritten {
    pub path: String,
    pub staged: File,
    pub kept: File,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    InTransaction,
    NoTransaction,
    Ended,
    /// Commit while the program holds open for writing a file of the
    /// transaction, or one of the fork the commit would write (a choice).
    OpenFiles,
}

/// A step the scheduler can take.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Take the head of a tree's queue: hold the files it overwrites, or
    /// wait for the write-opens of them to close (rule 9).
    Next(Tree),
    /// Write the change a tree's [`Step::Next`] holds its files for.
    Finish(Tree),
    Drop(ForkId),
    /// Replication pushes to the shared remote.
    Replicate,
}

/// Rule 8: one write call. POSIX gives byte calls only; the save sees only
/// the bytes they leave. Positions count the lines the head at close and
/// the calls before leave; one past the last line means the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// `O_APPEND`: a line after the file's last line as it is at close.
    Append,
    /// Bytes that replace that line.
    At(usize),
    /// Bytes for a new line before that line.
    Insert(usize),
    /// Bytes that remove that line.
    Delete(usize),
    /// Bytes that move line `from` to become line `to`.
    Move { from: usize, to: usize },
    /// Truncate, then write these lines.
    Rewrite(File),
}

/// What a handle is: an open file, or a path call (rule 11) waiting while
/// a take holds its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Write,
    Unlink,
    Rename { to: String },
}

/// Where a write-open writes: its tree, or its client's open transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Tree(Tree),
    Txn(ForkId, TxnId),
}

/// A file a client has open for writing, or waits to, or a path call
/// waiting. Its write calls stay private until close (close-to-open).
#[derive(Clone, Debug)]
pub struct Handle {
    pub id: HandleId,
    pub client: Client,
    pub target: Target,
    /// Its path in its target; follows a rename there; none once the file
    /// is unlinked (or renamed over) there.
    pub file: Option<String>,
    pub waiting: bool,
    pub kind: Kind,
    /// Its write calls, each with the bytes it wrote, applied at close.
    calls: Vec<(Write, Option<String>)>,
}

impl Handle {
    /// The paths it locks or waits for: its file, and a rename's target.
    pub fn files(&self) -> Vec<String> {
        let mut v: Vec<String> = self.file.iter().cloned().collect();
        if let Kind::Rename { to } = &self.kind {
            v.push(to.clone());
        }
        v
    }
}

/// What a tree's take holds its files for (rule 9).
#[derive(Clone, Debug)]
pub enum Take {
    /// The parent's head when the take began, and the rebased head planned.
    Sync {
        up: String,
        planned: Option<Rebased>,
    },
    /// A child's commits (a squashed run, or one) and the planned merge.
    Landing {
        from: ForkId,
        run: Vec<String>,
        merged: Merged,
    },
}

/// Rule 9: an incoming change holding the files it will overwrite.
#[derive(Clone, Debug)]
pub struct Hold {
    pub files: BTreeSet<String>,
    pub take: Take,
    /// The tree's head its plan was made on.
    ours: String,
}

/// A rebase of a fork's commits the parent has not taken onto `up`.
#[derive(Clone, Debug)]
pub struct Rebased {
    /// The new head: the last copy, or `up` when none is left.
    pub head: String,
    /// Each replayed commit and its copy (none: it became empty).
    pub copies: Vec<(String, Option<String>)>,
    /// Copies of `Squashed` landing commits, with the graft each keeps.
    pub grafts: Vec<(String, Vec<String>)>,
}

/// One merge's result and its conflicting hunks.
#[derive(Clone, Debug)]
pub struct Merged {
    pub tree: String,
    /// The paths the result changes from the parent's head.
    pub files: BTreeSet<String>,
    pub hunks: Vec<Hunk>,
    pub base: String,
}

/// One conflicting hunk, or a whole-file conflict.
#[derive(Clone, Debug)]
pub struct Hunk {
    pub path: String,
    pub ours: File,
    pub theirs: File,
    pub by_file: bool,
    /// The incoming commit whose change lost it, and that change's base.
    pub from: String,
    pub base: String,
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
    /// The checks' record of who wrote what over what: no decision reads it.
    pub oracle: Oracle,
    counters: BTreeMap<String, u32>,
    pub collisions: Vec<Collision>,
    /// Every sync that moved a fork: (the fork, the parent's head it
    /// carried, the fork's head before, after).
    pub syncs: Vec<(Tree, String, String, String)>,
    /// Every landing taken: (target, the commit landed), each commit of a
    /// squashed changeset included.
    pub landings: Vec<(Tree, String)>,
    pub fast_forwards: usize,
    pub squashed_runs: usize,
    /// Commits replayed by syncs, and those that became empty.
    pub rebased: usize,
    pub rebased_empty: usize,
    /// Versions of rolled-back transactions.
    pub discarded: BTreeSet<String>,
    /// Every line an append wrote, recorded at the write call.
    pub appended: BTreeSet<String>,
    pub ended_main: BTreeSet<String>,
    pub ended: Vec<Ended>,
    pub log: Vec<String>,
    /// A rebased copy to its original commit.
    pub originals: BTreeMap<String, String>,
    /// An original commit to its rebased copies.
    pub copies: BTreeMap<String, Vec<String>>,
    files_cache: RefCell<BTreeMap<String, BTreeMap<String, File>>>,
    ancestry: RefCell<BTreeMap<(String, String), bool>>,
    versions_cache: RefCell<BTreeMap<(String, String), Option<String>>>,
    /// `check`'s state.
    pub(crate) views: BTreeMap<Tree, BTreeSet<String>>,
    pub(crate) ever: BTreeMap<Tree, BTreeSet<String>>,
    pub(crate) left: BTreeSet<(Tree, String)>,
    pub(crate) taken: Option<String>,
    pub(crate) checked_commits: BTreeSet<String>,
    pub(crate) reach: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    pub(crate) main_history: BTreeSet<String>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) collisions_checked: usize,
    replication_due: bool,
    pub(crate) syncs_checked: usize,
    pub(crate) ended_checked: usize,
    pub(crate) ref_moves: u64,
    pub(crate) refs_checked: Option<u64>,
    pub(crate) originals_checked: BTreeSet<String>,
    /// Each collision's hunk read as versions (lost, won), by `check`.
    pub collision_versions: Vec<(Vec<String>, Vec<String>)>,
}

fn message(writer: &str, cause: &str, tree: &str, rebased: Option<&str>) -> String {
    let mut m = format!(
        "{cause} by {writer}\n\nAgentfs-Writer: {writer}\nAgentfs-Cause: {cause}\nAgentfs-Tree: {tree}\n"
    );
    if let Some(o) = rebased {
        m.push_str(&format!("Agentfs-Rebased: {o}\n"));
    }
    m
}

fn lines(text: &str) -> File {
    text.lines().map(str::to_string).collect()
}

impl World {
    pub fn new(policies: Policies) -> Self {
        let root_dir = std::env::temp_dir().join(format!(
            "agentfs-model-{}-{}",
            std::process::id(),
            WORLDS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root_dir);
        let repo = root_dir.join("repo");
        let remote = root_dir.join("origin.git");
        std::fs::create_dir_all(&repo).expect("create the repository");
        std::fs::create_dir_all(&remote).expect("create the remote");
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let mut w = Self {
            policies,
            root: root_dir,
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
            oracle: Oracle::default(),
            counters: BTreeMap::new(),
            collisions: Vec::new(),
            syncs: Vec::new(),
            landings: Vec::new(),
            fast_forwards: 0,
            squashed_runs: 0,
            rebased: 0,
            rebased_empty: 0,
            discarded: BTreeSet::new(),
            appended: BTreeSet::new(),
            ended_main: BTreeSet::new(),
            ended: Vec::new(),
            log: Vec::new(),
            originals: BTreeMap::new(),
            copies: BTreeMap::new(),
            files_cache: RefCell::new(BTreeMap::new()),
            ancestry: RefCell::new(BTreeMap::new()),
            versions_cache: RefCell::new(BTreeMap::new()),
            views: BTreeMap::new(),
            ever: BTreeMap::new(),
            left: BTreeSet::new(),
            taken: None,
            checked_commits: BTreeSet::new(),
            reach: BTreeMap::new(),
            main_history: BTreeSet::new(),
            grepped: (0, BTreeSet::new()),
            collisions_checked: 0,
            replication_due: false,
            syncs_checked: 0,
            ended_checked: 0,
            ref_moves: 0,
            refs_checked: None,
            originals_checked: BTreeSet::new(),
            collision_versions: Vec::new(),
        };
        let files = PATHS
            .iter()
            .map(|p| (p.to_string(), Some(root(p))))
            .collect();
        let tree = w.tree_with(None, &files);
        let c = w.commit(&[], &tree, "agentfs", "Root", "main", None);
        w.set_head(Tree::Main, &c);
        w
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

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

    /// Every file at a commit or tree, by path, cached: objects never change.
    pub fn files(&self, at: &str) -> BTreeMap<String, File> {
        if let Some(f) = self.files_cache.borrow().get(at) {
            return f.clone();
        }
        let mut out = BTreeMap::new();
        for l in git(&self.repo, &["ls-tree", at]).lines() {
            let Some((meta, path)) = l.split_once('\t') else {
                continue;
            };
            let blob = meta.split(' ').nth(2).expect("an object id");
            out.insert(path.to_string(), self.blob(blob));
        }
        self.files_cache
            .borrow_mut()
            .insert(at.to_string(), out.clone());
        out
    }

    fn blob(&self, oid: &str) -> File {
        lines(&git(&self.repo, &["cat-file", "blob", oid]))
    }

    /// File `p` at a commit, if it exists there.
    pub fn file(&self, at: &str, p: &str) -> Option<File> {
        self.files(at).remove(p)
    }

    /// What tree `t` holds at path `p`.
    pub fn read(&self, t: Tree, p: &str) -> File {
        self.file(&self.head(t), p)
            .unwrap_or_else(|| panic!("no file {p} in {t:?}"))
    }

    /// The paths a commit holds.
    pub fn paths(&self, at: &str) -> Vec<String> {
        self.files(at).into_keys().collect()
    }

    /// The paths a client's program sees: its transaction's, else its tree's.
    pub fn client_paths(&self, c: &Client) -> Vec<String> {
        self.paths(&self.target_head(self.target_of(c)))
    }

    /// What a client's program reads at `p`, if it exists.
    pub fn client_reads(&self, c: &Client, p: &str) -> Option<File> {
        self.file(&self.target_head(self.target_of(c)), p)
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

    pub fn handles(&self) -> impl Iterator<Item = &Handle> {
        self.handles.values()
    }

    pub fn handle(&self, h: HandleId) -> &Handle {
        &self.handles[&h]
    }

    pub fn holds(&self) -> &BTreeMap<Tree, Hold> {
        &self.holds
    }

    pub fn waits(&self) -> &BTreeMap<Tree, BTreeSet<String>> {
        &self.waits
    }

    /// The paths a client of `t` has open for writing (granted opens).
    pub fn write_open(&self, t: Tree) -> BTreeSet<String> {
        self.handles
            .values()
            .filter(|h| !h.waiting && h.target == Target::Tree(t) && h.kind == Kind::Write)
            .filter_map(|h| h.file.clone())
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

    /// `base`'s tree with each path set to its file, or removed (none).
    fn tree_with(&self, base: Option<&str>, files: &BTreeMap<String, Option<File>>) -> String {
        let mut entries: BTreeMap<String, String> = BTreeMap::new();
        if let Some(base) = base {
            for line in git(&self.repo, &["ls-tree", base]).lines() {
                if let Some((_, name)) = line.split_once('\t') {
                    entries.insert(name.to_string(), line.to_string());
                }
            }
        }
        for (p, f) in files {
            match f {
                Some(f) => {
                    let text: String = f.iter().map(|l| format!("{l}\n")).collect();
                    let blob = git_in(&self.repo, &["hash-object", "-w", "--stdin"], &text);
                    entries.insert(p.clone(), format!("100644 blob {blob}\t{p}"));
                }
                None => {
                    entries.remove(p);
                }
            }
        }
        let listing: String = entries.values().map(|l| format!("{l}\n")).collect();
        git_in(&self.repo, &["mktree"], &listing)
    }

    fn commit(
        &self,
        parents: &[&str],
        tree: &str,
        writer: &str,
        cause: &str,
        on: &str,
        rebased: Option<&str>,
    ) -> String {
        let msg = message(writer, cause, on, rebased);
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        git_in(&self.repo, &args, &msg)
    }

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

    pub fn tree_of(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^{{tree}}")])
    }

    pub fn first_parent(&self, c: &str) -> String {
        git(&self.repo, &["rev-parse", &format!("{c}^1")])
    }

    /// Whether commit `a` is `b` or an ancestor of it in its lineage
    /// (grafts applied; cached: commits never change).
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

    /// Whether `head` reaches commit `c` or a rebased copy of it.
    pub fn reached(&self, head: &str, c: &str) -> bool {
        self.is_ancestor(c, head)
            || self
                .copies
                .get(c)
                .is_some_and(|v| v.iter().any(|x| self.is_ancestor(x, head)))
    }

    /// The original of a commit (itself, if it is no rebased copy).
    pub fn original<'a>(&'a self, c: &'a str) -> &'a str {
        self.originals.get(c).map_or(c, String::as_str)
    }

    /// The version a commit holds as the line `bytes` (tracked bytes only):
    /// the latest version with those bytes whose writing commit (or a copy)
    /// the commit reaches. The oracle's, for the checks.
    pub fn version_in(&self, at: &str, bytes: &str) -> Option<String> {
        let vs = self.oracle.by_bytes.get(bytes)?;
        if vs.len() == 1 {
            return Some(vs[0].clone());
        }
        let key = (at.to_string(), bytes.to_string());
        if let Some(v) = self.versions_cache.borrow().get(&key) {
            return v.clone();
        }
        let v = vs
            .iter()
            .rev()
            .find(|v| self.reached(at, &self.oracle.versions[*v].commit))
            .or(vs.first())
            .cloned();
        self.versions_cache.borrow_mut().insert(key, v.clone());
        v
    }

    /// Every tracked version a commit holds.
    pub fn shown(&self, at: &str) -> BTreeSet<String> {
        self.files(at)
            .values()
            .flatten()
            .filter(|l| tracked(l))
            .filter_map(|l| self.version_in(at, l))
            .collect()
    }

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

    fn plain_save(&self, c: &str) -> bool {
        self.trailer(c, "Agentfs-Cause") == "Direct"
    }

    /// The paths that differ between two commits or trees.
    pub fn changed(&self, a: &str, b: &str) -> BTreeSet<String> {
        git(&self.repo, &["diff-tree", "-r", "--name-only", a, b])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Rule 3: git's three-way merge (merge-ort, `git merge-tree`) of
    /// `theirs` into `ours` from `base`, the side `ours` winning every
    /// conflicting hunk (`-X ours`) and every path conflict (its stage 2,
    /// else the path goes), as one hunk per conflict. Under
    /// [`CollisionScope::File`] a file with a conflict keeps `ours`' whole
    /// version.
    fn merge(&self, base: &str, ours: &str, theirs: &str) -> Merged {
        let run = |x: bool| {
            let mut args = vec!["merge-tree", "--write-tree", "--no-messages"];
            if x {
                args.push("-Xours");
            }
            let mb = format!("--merge-base={base}");
            args.extend([mb.as_str(), ours, theirs]);
            let o = try_git(&self.repo, &args, None);
            assert!(o.code == 0 || o.code == 1, "merge-tree: {}", o.stderr);
            let mut it = o.stdout.lines();
            let tree = it.next().expect("a tree").to_string();
            let mut stages: BTreeMap<String, BTreeMap<u8, String>> = BTreeMap::new();
            for l in it.take_while(|l| !l.is_empty()) {
                let (meta, path) = l.split_once('\t').expect("conflicted file info");
                let f: Vec<&str> = meta.split(' ').collect();
                stages
                    .entry(path.to_string())
                    .or_default()
                    .insert(f[2].parse().expect("a stage"), f[1].to_string());
            }
            (tree, stages)
        };
        let (written, paths) = run(true);
        let mut fix: BTreeMap<String, Option<File>> = BTreeMap::new();
        let mut hunks = Vec::new();
        for (p, s) in &paths {
            let side = |n: u8| s.get(&n).map(|o| self.blob(o));
            fix.insert(p.clone(), side(2));
            let o = side(2).unwrap_or_default();
            let t = side(3).unwrap_or_else(|| self.incoming(base, ours, theirs, p));
            if !o.is_empty() || !t.is_empty() {
                hunks.push(Hunk {
                    path: p.clone(),
                    ours: o,
                    theirs: t,
                    by_file: true,
                    from: theirs.to_string(),
                    base: base.to_string(),
                });
            }
        }
        let resolved = if fix.is_empty() {
            written
        } else {
            self.tree_with(Some(&written), &fix)
        };
        let (marked, content) = run(false);
        for p in content.keys().filter(|p| !paths.contains_key(*p)) {
            let text = self.file(&marked, p).expect("a conflicted file");
            let mut mine = Vec::new();
            let (mut o, mut t, mut side) = (Vec::new(), Vec::new(), 0u8);
            for l in text {
                if l.starts_with("<<<<<<< ") {
                    side = 1;
                } else if l == "=======" && side == 1 {
                    side = 2;
                } else if l.starts_with(">>>>>>> ") && side == 2 {
                    side = 0;
                    hunks.push(Hunk {
                        path: p.clone(),
                        ours: std::mem::take(&mut o),
                        theirs: std::mem::take(&mut t),
                        by_file: false,
                        from: theirs.to_string(),
                        base: base.to_string(),
                    });
                } else {
                    match side {
                        1 => {
                            o.push(l.clone());
                            mine.push(l);
                        }
                        2 => t.push(l),
                        _ => mine.push(l),
                    }
                }
            }
            assert_eq!(
                self.file(&resolved, p).as_ref(),
                Some(&mine),
                "-X ours took the parent's side of every marked hunk in {p}"
            );
        }
        let tree = if self.policies.collision == CollisionScope::File && !hunks.is_empty() {
            let whole: BTreeMap<String, Option<File>> = hunks
                .iter()
                .map(|h| (h.path.clone(), self.file(ours, &h.path)))
                .collect();
            let paths: BTreeSet<String> = hunks.iter().map(|h| h.path.clone()).collect();
            hunks = paths
                .into_iter()
                .map(|p| Hunk {
                    ours: self.file(ours, &p).unwrap_or_default(),
                    theirs: self.incoming(base, ours, theirs, &p),
                    path: p,
                    by_file: true,
                    from: theirs.to_string(),
                    base: base.to_string(),
                })
                .collect();
            self.tree_with(Some(&resolved), &whole)
        } else {
            resolved
        };
        Merged {
            files: self.changed(ours, &tree),
            tree,
            hunks,
            base: base.to_string(),
        }
    }

    /// The incoming side's file for merged path `p`: its own file at `p`,
    /// else the file at the path the parent's side renamed to `p` (git's
    /// rename pairing), else nothing.
    fn incoming(&self, base: &str, ours: &str, theirs: &str, p: &str) -> File {
        if let Some(f) = self.file(theirs, p) {
            return f;
        }
        git(
            &self.repo,
            &["diff-tree", "-r", "-M", "--name-status", base, ours],
        )
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() == 3 && f[0].starts_with('R') && f[2] == p).then(|| f[1].to_string())
        })
        .find_map(|from| self.file(theirs, &from))
        .unwrap_or_default()
    }

    /// Records a merge's conflicting hunks in tree `t`, each keeping the
    /// losing incoming commit reachable under `refs/collisions/<n>`.
    fn record(
        &mut self,
        t: Tree,
        dir: Direction,
        m: &Merged,
        winner: &str,
        loser: &str,
        result: &str,
    ) {
        for h in &m.hunks {
            let loser = if h.from.is_empty() { loser } else { &h.from };
            let reference = format!("refs/collisions/{}", self.collisions.len() + 1);
            self.update_ref(&[&reference, loser]);
            self.note(format!(
                "collision in {t:?}:{} ({dir:?}): {:?} lost to {:?}",
                h.path, h.theirs, h.ours
            ));
            self.collisions.push(Collision {
                tree: t,
                direction: dir,
                path: h.path.clone(),
                ours: h.ours.clone(),
                theirs: h.theirs.clone(),
                by_file: h.by_file,
                base: h.base.clone(),
                winner: winner.to_string(),
                loser: loser.to_string(),
                result: result.to_string(),
                reference,
            });
            self.replication_due = true;
        }
    }

    /// Replication's push: main, the grafts, every collision ref and every
    /// pre-rebase head, atomically, never forced.
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
                "refs/rebased/*:refs/rebased/*",
            ],
        );
    }

    /// `t`'s head moved: a fork's new commit queues for its parent (`up`),
    /// and every child queues a sync (rule 1: both ways).
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
                base: head.clone(),
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

    /// Rule 1: open `p`, write line `slot`, close: the version written.
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let h = self.open_write(c, p);
        assert!(!self.handles[&h].waiting, "save: a take holds {p}");
        let token = self.write(h, Write::At(slot)).expect("a new version");
        self.close(h);
        token
    }

    fn who(&self, c: &Client) -> String {
        match c {
            Client::Main(w) => w.clone(),
            Client::Fork(f) => self.forks[f].writer.clone(),
        }
    }

    fn target_of(&self, c: &Client) -> Target {
        match c {
            Client::Main(_) => Target::Tree(Tree::Main),
            Client::Fork(f) => match &self.forks[f].txn {
                Some(t) => Target::Txn(*f, t.id),
                None => Target::Tree(Tree::Fork(*f)),
            },
        }
    }

    pub fn target_head(&self, t: Target) -> String {
        match t {
            Target::Tree(t) => self.head(t),
            Target::Txn(f, _) => self.forks[&f].txn.as_ref().expect("open").head.clone(),
        }
    }

    fn add_handle(&mut self, c: &Client, target: Target, file: String, kind: Kind) -> HandleId {
        assert!(
            !matches!(c, Client::Main(n) if self.ended_main.contains(n)),
            "a call by {c:?}, which ended"
        );
        let id = self.next_handle;
        self.next_handle += 1;
        let mut h = Handle {
            id,
            client: c.clone(),
            target,
            file: Some(file),
            waiting: false,
            kind,
            calls: Vec::new(),
        };
        h.waiting = matches!(target, Target::Tree(t)
            if self.holds.get(&t).is_some_and(|x| h.files().iter().any(|f| x.files.contains(f))));
        self.handles.insert(id, h);
        id
    }

    /// Rule 9: open path `p` for writing, in the client's transaction if
    /// it has one, else its tree; waits while a take holds `p` there.
    pub fn open_write(&mut self, c: &Client, p: &str) -> HandleId {
        let target = self.target_of(c);
        assert!(
            self.file(&self.target_head(target), p).is_some(),
            "open_write of {p}: no such file (the model creates none)"
        );
        let id = self.add_handle(c, target, p.to_string(), Kind::Write);
        let waiting = self.handles[&id].waiting;
        self.note(format!(
            "open h{id} {c:?} {target:?}:{p}{}",
            if waiting { ": waits for the hold" } else { "" }
        ));
        id
    }

    /// Rule 8: one write call on a granted open. Returns the bytes an
    /// append, a write at an offset or an insert makes (its version id).
    pub fn write(&mut self, h: HandleId, call: Write) -> Option<String> {
        self.write_call(h, call, None)
    }

    /// [`World::write`], the new line holding `text` (repeated bytes).
    pub fn write_text(&mut self, h: HandleId, call: Write, text: &str) -> Option<String> {
        self.write_call(h, call, Some(text))
    }

    fn write_call(&mut self, h: HandleId, call: Write, text: Option<&str>) -> Option<String> {
        let handle = &self.handles[&h];
        assert!(
            !handle.waiting && handle.kind == Kind::Write,
            "a write needs a granted open"
        );
        let (writer, txn) = self.handle_writer(handle);
        let made = match call {
            Write::Append | Write::At(_) | Write::Insert(_) => Some(match text {
                Some(t) => t.to_string(),
                None => self.token(&writer, txn),
            }),
            _ => None,
        };
        if call == Write::Append
            && let Some(t) = made.as_ref().filter(|t| tracked(t))
        {
            self.appended.insert(t.clone());
        }
        let shown = match &call {
            Write::Rewrite(_) => "Rewrite".to_string(),
            c => format!("{c:?} {made:?}"),
        };
        self.handles
            .get_mut(&h)
            .expect("open")
            .calls
            .push((call, made.clone()));
        self.note(format!("write h{h}: {shown}"));
        made
    }

    /// Rule 8, the save at close: the write calls applied in order to the
    /// head of the open's tree (or transaction) now, committed as they
    /// are: one commit, first parent that head; none if nothing changed or
    /// the file was unlinked meanwhile. Then a waiting take is woken.
    pub fn close(&mut self, h: HandleId) {
        let handle = self.handles.remove(&h).expect("an open file");
        assert!(
            !handle.waiting && handle.kind == Kind::Write,
            "close a granted open"
        );
        let saved = self.save_handle(&handle);
        self.note(format!("close h{h}: saved {saved:?}"));
        if let Target::Tree(t) = handle.target {
            self.wake(t);
        }
    }

    fn handle_writer(&self, h: &Handle) -> (String, Option<TxnId>) {
        let txn = match h.target {
            Target::Txn(_, t) => Some(t),
            Target::Tree(_) => None,
        };
        (self.who(&h.client), txn)
    }

    /// The bytes `calls` leave on `head`, and for each line an `At` wrote,
    /// the bytes it wrote over.
    fn replay(
        head: &File,
        calls: &[(Write, Option<String>)],
    ) -> (File, BTreeMap<String, String>, BTreeSet<String>) {
        let mut img = head.clone();
        let mut over = BTreeMap::new();
        let mut moved = BTreeSet::new();
        for (call, made) in calls {
            let x = || made.clone().expect("a call that makes a line");
            match call {
                Write::Append => img.push(x()),
                Write::At(i) if *i < img.len() => {
                    over.insert(x(), img[*i].clone());
                    img[*i] = x();
                }
                Write::At(_) => img.push(x()),
                Write::Insert(i) => img.insert((*i).min(img.len()), x()),
                Write::Delete(i) if *i < img.len() => {
                    img.remove(*i);
                }
                Write::Move { from, to } if from != to && *from < img.len() => {
                    let l = img.remove(*from);
                    moved.insert(l.clone());
                    img.insert((*to).min(img.len()), l);
                }
                Write::Rewrite(f) => img = f.clone(),
                Write::Delete(_) | Write::Move { .. } => {}
            }
        }
        (img, over, moved)
    }

    fn save_handle(&mut self, handle: &Handle) -> Option<String> {
        let (writer, txn) = self.handle_writer(handle);
        let p = handle.file.clone()?;
        let at = self.target_head(handle.target);
        let head = self.file(&at, &p)?;
        let (new, over, moved) = Self::replay(&head, &handle.calls);
        if new == head {
            return None;
        }
        let tree = self.tree_with(Some(&at), &BTreeMap::from([(p.clone(), Some(new.clone()))]));
        let made: BTreeSet<String> = handle.calls.iter().filter_map(|(_, m)| m.clone()).collect();
        let c = self.commit(
            &[&at],
            &tree,
            &writer,
            "Direct",
            &self.on(handle.target),
            None,
        );
        // The oracle: what this save wrote, over what (no decision reads it).
        let had: BTreeSet<&String> = head.iter().filter(|l| tracked(l)).collect();
        let now: BTreeSet<&String> = new.iter().filter(|l| tracked(l)).collect();
        let mut versions = Vec::new();
        let mut covered = BTreeSet::new();
        for b in now.difference(&had) {
            let (v, o) = if made.contains(*b) {
                // Through this open's own earlier lines, back to the head's.
                let mut o = over.get(*b);
                let mut seen = BTreeSet::new();
                while let Some(x) =
                    o.filter(|x| tracked(x) && made.contains(*x) && !had.contains(x))
                {
                    if !seen.insert(x) {
                        break;
                    }
                    o = over.get(x);
                }
                let o = o
                    .filter(|x| had.contains(x))
                    .and_then(|x| self.version_in(&at, x));
                ((*b).clone(), o)
            } else {
                (self.token(&writer, txn), None)
            };
            if let Some(o) = &o {
                covered.insert(o.clone());
            }
            self.oracle.wrote(&v, Some(b), o.as_deref(), &c);
            versions.push(v);
        }
        // A moved line is, to git, a delete and an insert of its bytes: the
        // mover's new version, written over the one it moved.
        let place = |f: &File, b: &str| f.iter().position(|l| l == b);
        for b in moved.iter().filter(|b| {
            tracked(b) && had.contains(b) && now.contains(b) && place(&head, b) != place(&new, b)
        }) {
            if let Some(o) = self.version_in(&at, b) {
                let v = self.token(&writer, txn);
                self.oracle.wrote(&v, Some(b), Some(&o), &c);
                versions.push(v);
            }
        }
        for b in had.difference(&now) {
            let Some(o) = self.version_in(&at, b) else {
                continue;
            };
            if !covered.contains(&o) {
                let d = self.token(&writer, txn);
                self.oracle.wrote(&d, None, Some(&o), &c);
                versions.push(d);
            }
        }
        self.commit_save(handle.target, &c, versions);
        Some(c)
    }

    fn on(&self, t: Target) -> String {
        match t {
            Target::Tree(t) => Self::name(t),
            Target::Txn(_, id) => format!("txn/{id}"),
        }
    }

    /// A save's commit moves its target: the tree's branch (queueing the
    /// move both ways), or the open transaction's branch (staged).
    fn commit_save(&mut self, target: Target, c: &str, versions: Vec<String>) {
        match target {
            Target::Txn(f, id) => {
                self.update_ref(&[&format!("refs/heads/txn/{id}"), c]);
                let txn = self.forks.get_mut(&f).expect("fork").txn.as_mut();
                let txn = txn.expect("open");
                txn.head = c.to_string();
                txn.staged.extend(versions);
            }
            Target::Tree(tree) => {
                self.set_head(tree, c);
                self.moved(tree, true);
            }
        }
    }

    /// Rule 11: unlink path `p`: one save removing it. Waits like a
    /// write-open while a take holds `p`; the client's opens of it then
    /// save nothing.
    pub fn unlink(&mut self, c: &Client, p: &str) -> Option<HandleId> {
        self.path_call(c, p, Kind::Unlink)
    }

    /// Rule 11: rename `from` to `to` (over a file there, if any): one save
    /// moving the path. Opens of `from` follow it; opens of a replaced `to`
    /// save nothing.
    pub fn rename(&mut self, c: &Client, from: &str, to: &str) -> Option<HandleId> {
        self.path_call(c, from, Kind::Rename { to: to.to_string() })
    }

    fn path_call(&mut self, c: &Client, p: &str, kind: Kind) -> Option<HandleId> {
        let target = self.target_of(c);
        assert!(
            self.file(&self.target_head(target), p).is_some(),
            "{kind:?} of {p}: no such file"
        );
        let id = self.add_handle(c, target, p.to_string(), kind);
        if self.handles[&id].waiting {
            let kind = &self.handles[&id].kind;
            self.note(format!("h{id} {c:?} {kind:?} of {p}: waits for the hold"));
            return Some(id);
        }
        let h = self.handles.remove(&id).expect("just added");
        self.run_path_call(&h);
        None
    }

    fn run_path_call(&mut self, h: &Handle) {
        let (writer, txn) = self.handle_writer(h);
        let at = self.target_head(h.target);
        let Some(p) = h.file.clone() else { return };
        let Some(content) = self.file(&at, &p) else {
            return;
        };
        let mut change = BTreeMap::from([(p.clone(), None)]);
        let mut gone: Vec<File> = Vec::new();
        let mut carried: File = Vec::new();
        match &h.kind {
            Kind::Unlink => gone.push(content),
            Kind::Rename { to } => {
                if *to == p {
                    return;
                }
                gone.extend(self.file(&at, to));
                carried = content.clone();
                change.insert(to.clone(), Some(content));
            }
            Kind::Write => unreachable!("an open is not a path call"),
        }
        let tree = self.tree_with(Some(&at), &change);
        let c = self.commit(&[&at], &tree, &writer, "Direct", &self.on(h.target), None);
        let mut versions = Vec::new();
        for b in gone.iter().flatten().filter(|l| tracked(l)) {
            if let Some(o) = self.version_in(&at, b) {
                let d = self.token(&writer, txn);
                self.oracle.wrote(&d, None, Some(&o), &c);
                versions.push(d);
            }
        }
        for b in carried.iter().filter(|l| tracked(l)) {
            if let Some(o) = self.version_in(&at, b) {
                let m = self.token(&writer, txn);
                self.oracle.moved(&m, &o, &c);
            }
        }
        // As on Linux: opens of the path follow a rename; opens of an
        // unlinked or replaced file reach no path.
        for o in self.handles.values_mut().filter(|o| o.target == h.target) {
            match &h.kind {
                Kind::Rename { to } if o.file.as_deref() == Some(to) => o.file = None,
                Kind::Rename { to } if o.file.as_deref() == Some(&p) => o.file = Some(to.clone()),
                Kind::Unlink if o.file.as_deref() == Some(&p) => o.file = None,
                _ => {}
            }
        }
        self.note(format!("{:?} {:?} of {p}: saved {c:.7}", h.client, h.kind));
        self.commit_save(h.target, &c, versions);
        if let Target::Tree(t) = h.target {
            self.wake(t);
        }
    }

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

    /// Commit (a choice): git's merge of the transaction's branch into the
    /// fork's head from its start, the fork's side winning a conflicting
    /// hunk (the fork is the transaction's parent); one merge commit named
    /// for the client, which then lands like any fork commit. Never refused
    /// but for the program's own open files ([`ModelError::OpenFiles`]).
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
            let m = self.merge(&start, &ours, &head);
            let open = self.write_open(Tree::Fork(f));
            if m.files.iter().any(|p| open.contains(p)) {
                return Err(ModelError::OpenFiles);
            }
            Some(m)
        };
        let txn = self.take_txn(f)?;
        let mut result = None;
        let outcome = if let Some(m) = planned {
            let writer = self.forks[&f].writer.clone();
            let t = Tree::Fork(f);
            let cause = format!("Transaction t{}", txn.id);
            let c = self.commit(
                &[&ours, &txn.head],
                &m.tree,
                &writer,
                &cause,
                &Self::name(t),
                None,
            );
            self.record(t, Direction::Transaction, &m, &ours, &txn.head, &c);
            self.set_head(t, &c);
            self.moved(t, true);
            result = Some(c);
            TxnOutcome::Committed {
                overwritten: m
                    .hunks
                    .iter()
                    .map(|h| Overwritten {
                        path: h.path.clone(),
                        staged: h.theirs.clone(),
                        kept: h.ours.clone(),
                    })
                    .collect(),
            }
        } else {
            TxnOutcome::Empty
        };
        self.note(format!("commit t{} on f{f}: {outcome:?}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Commit(outcome.clone()),
            head: txn.head,
            result,
        });
        Ok(outcome)
    }

    /// Rollback (abandon): drop the branch and its open files, unsaved.
    pub fn rollback(&mut self, f: ForkId) -> Result<(), ModelError> {
        let txn = self.take_txn(f)?;
        self.handles
            .retain(|_, h| !matches!(h.target, Target::Txn(g, t) if g == f && t == txn.id));
        self.discarded.extend(txn.staged.iter().cloned());
        self.note(format!("rollback t{} on f{f}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Rollback,
            head: txn.head,
            result: None,
        });
        Ok(())
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

    /// A fork's client leaves or dies: its transaction drops, its open
    /// files close (saving, as the kernel's close on exit), its waiting
    /// opens drop.
    pub fn end_client(&mut self, f: ForkId) {
        if self.forks[&f].txn.is_some() {
            let _ = self.rollback(f);
        }
        self.release(&Client::Fork(f));
        self.forks.get_mut(&f).expect("fork").life = Life::Ended;
        self.wake(Tree::Fork(f));
        self.note(format!("end f{f}"));
    }

    pub fn end_main_client(&mut self, writer: &str) {
        self.release(&Client::Main(writer.to_string()));
        self.ended_main.insert(writer.to_string());
        self.wake(Tree::Main);
        self.note(format!("end main client {writer}"));
    }

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

    /// Takes one step; returns how many queue entries it took.
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

    /// Rule 9, a take begins: plan the head entry's change into `t`; wait
    /// if a client of `t` has a path it changes open for writing, else hold
    /// those paths.
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
                let planned = self.rebase(&ours, &self.forks[&f].base, &up);
                let files = planned
                    .as_ref()
                    .map(|r| {
                        let mut f = self.changed(&ours, &r.head);
                        f.extend(self.changed(&ours, &up));
                        for (x, _) in &r.copies {
                            f.extend(self.own_change(x));
                        }
                        f
                    })
                    .unwrap_or_default();
                let keep: VecDeque<Entry> = q[1..].iter().cloned().collect();
                (Take::Sync { up, planned }, files, keep)
            }
            Entry::Landing { from, commit } => {
                let (run, keep) = self.run_from(from, &commit, &q[1..]);
                let merged = self.plan_landing(t, &ours, &run);
                let mut files = merged.files.clone();
                for c in &run {
                    files.extend(self.own_change(c));
                }
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
        self.holds.insert(t, Hold { files, take, ours });
        taken
    }

    /// Rule 9, a take ends: write the held change (planned again if a save
    /// of another file moved the head meanwhile), then release its files.
    fn finish(&mut self, t: Tree) {
        let hold = self.holds.remove(&t).expect("a take in progress");
        let head = self.head(t);
        let moved = head != hold.ours;
        match hold.take {
            Take::Sync { up, planned } => {
                let planned = if moved {
                    let Tree::Fork(f) = t else {
                        unreachable!("main has no parent")
                    };
                    self.rebase(&head, &self.forks[&f].base, &up)
                } else {
                    planned
                };
                if let Some(r) = &planned {
                    held(t, &self.changed(&head, &r.head), &hold.files);
                }
                self.sync(t, &up, planned);
            }
            Take::Landing { from, run, merged } => {
                let merged = if moved {
                    self.plan_landing(t, &head, &run)
                } else {
                    merged
                };
                held(t, &merged.files, &hold.files);
                self.land(t, from, &run, merged);
            }
        }
        self.taken = Some(self.head(t));
        let waiting: Vec<HandleId> = self
            .handles
            .values()
            .filter(|h| h.waiting && h.target == Target::Tree(t))
            .map(|h| h.id)
            .collect();
        let at = self.head(t);
        for h in waiting {
            // The path may have left the tree meanwhile: then it reaches
            // nothing (a real open would fail).
            let file = self.handles[&h]
                .file
                .clone()
                .filter(|p| self.file(&at, p).is_some());
            let handle = self.handles.get_mut(&h).expect("open");
            handle.file = file;
            if handle.kind == Kind::Write {
                handle.waiting = false;
                self.note(format!("grant h{h}"));
            } else {
                let call = self.handles.remove(&h).expect("a waiting call");
                self.note(format!("grant h{h}: it runs"));
                self.run_path_call(&call);
            }
        }
    }

    fn run_from(
        &self,
        from: ForkId,
        first: &str,
        rest: &[Entry],
    ) -> (Vec<String>, VecDeque<Entry>) {
        let mut run = vec![first.to_string()];
        let mut keep = VecDeque::new();
        let mut open = self.policies.history == HistoryPolicy::Squashed && self.plain_save(first);
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

    /// The paths a commit's own change touches (from its first parent).
    /// A take holds these as well as the paths its plan writes: git's
    /// result in one file can change with the parent's other content (a
    /// hunk that lost may apply after the parent's line changes), so a
    /// replanned take may write any path its incoming change touches.
    fn own_change(&self, c: &str) -> BTreeSet<String> {
        self.changed(&self.first_parent(c), c)
    }

    /// Rule 3: a run of fork commits into the parent's head `ours`: git's
    /// merge of the run's own change (its first commit's first parent as the
    /// base) into `ours`, or a fast-forward under full lineage when that
    /// base is `ours`.
    fn plan_landing(&self, x: Tree, ours: &str, run: &[String]) -> Merged {
        let tip = run.last().expect("a commit");
        let base = self.first_parent(&run[0]);
        if self.fast_forward(x, ours, &base, run) {
            return Merged {
                tree: self.tree_of(tip),
                files: self.changed(ours, tip),
                hunks: Vec::new(),
                base,
            };
        }
        if run.len() == 1 {
            return self.merge(&base, ours, tip);
        }
        // A squashed run: each commit's own change in turn (as `git
        // cherry-pick` of the range), from its own first parent, so a save
        // made after a sync is read over what that sync brought; one result.
        let mut acc = ours.to_string();
        let mut hunks = Vec::new();
        let mut tree = String::new();
        for c in run {
            let m = self.merge(&self.first_parent(c), &acc, c);
            hunks.extend(m.hunks);
            tree = m.tree;
            acc = self.commit(&[&acc], &tree, "agentfs", "Squash step", "none", None);
        }
        Merged {
            files: self.changed(ours, &tree),
            tree,
            hunks,
            base,
        }
    }

    /// A landing fast-forwards only under full lineage, a single commit
    /// made on the parent's head, into main. A fork's branch is rewritten
    /// by its own syncs (rebase), so a child's commit landed in it by
    /// fast-forward would be rebased away from it: a landing into a fork is
    /// always a merge, whose rebased copies keep the child's commit as
    /// their second parent.
    fn fast_forward(&self, x: Tree, ours: &str, base: &str, run: &[String]) -> bool {
        self.policies.history == HistoryPolicy::FullLineage
            && x == Tree::Main
            && base == ours
            && run.len() == 1
    }

    /// Rule 3, a fork's change into its parent `x`, the one place a
    /// collision between them is decided.
    fn land(&mut self, x: Tree, from: ForkId, run: &[String], m: Merged) {
        let tip = run.last().expect("a commit").clone();
        let ours = self.head(x);
        self.note(format!(
            "landing f{from}'s {} commit(s) to {tip:.7} into {x:?}",
            run.len()
        ));
        for c in run {
            self.landings.push((x, c.clone()));
        }
        if run.len() > 1 {
            self.squashed_runs += 1;
        }
        let full = self.policies.history == HistoryPolicy::FullLineage;
        if self.fast_forward(x, &ours, &m.base, run) {
            self.set_head(x, &tip);
            self.fast_forwards += 1;
            self.note(format!("landed {tip:.7} into {x:?}: fast-forward"));
            self.moved(x, true);
            return;
        }
        let writer = self.writer_of_commit(&tip);
        let on = Self::name(x);
        let new = if full {
            self.commit(&[&ours, &tip], &m.tree, &writer, "Landing", &on, None)
        } else {
            // The graft names every carried commit the tip does not reach
            // (a run that spans a sync's rebase), then the tip.
            let new = self.commit(&[&ours], &m.tree, &writer, "Landing", &on, None);
            let mut args = vec!["replace", "--graft", new.as_str(), ours.as_str()];
            args.extend(
                run.iter()
                    .filter(|c| **c != tip && !self.is_ancestor(c, &tip))
                    .map(String::as_str),
            );
            args.push(&tip);
            git(&self.repo, &args);
            self.ref_moves += 1;
            new
        };
        self.record(x, Direction::Landing, &m, &ours, &tip, &new);
        self.note(format!("landed {tip:.7} into {x:?} as {new:.7}"));
        self.set_head(x, &new);
        self.moved(x, true);
    }

    /// Whether the parent's head `up` has taken fork commit `c`: it reaches
    /// `c`, its original, or a rebased copy of that original.
    fn taken_by(&self, up: &str, c: &str) -> bool {
        self.is_ancestor(c, up) || self.reached(up, self.original(c))
    }

    /// A sync's plan: the fork's commits from `ours` back to `base`, the
    /// parent's head it last synced to (`git rebase --onto up base`: a
    /// parent that rebased its own commits since is never replayed into
    /// the fork again), those `up` has not taken replayed
    /// oldest first onto `up` (`git rebase`), each as git's merge of its own
    /// change (from its first parent) with the parent's side winning a
    /// conflicting hunk; a copy keeps its writer, cause and second parent,
    /// and names its original. A copy that changes nothing is dropped,
    /// unless it is a merge (its second parent must stay reachable). None
    /// if the fork already has `up`.
    fn rebase(&self, ours: &str, base: &str, up: &str) -> Option<Rebased> {
        if self.is_ancestor(up, ours) {
            return None;
        }
        let mut chain = Vec::new();
        let mut c = ours.to_string();
        while c != base && !self.is_ancestor(&c, up) {
            chain.push(c.clone());
            c = self.first_parent(&c);
        }
        let mut head = up.to_string();
        let (mut copies, mut grafts) = (Vec::new(), Vec::new());
        for x in chain.iter().rev().filter(|x| !self.taken_by(up, x)) {
            let stored = commit_parents(&self.repo, x, true);
            let lineage = commit_parents(&self.repo, x, false);
            let m = self.merge(&lineage[0], &head, x);
            if m.files.is_empty() && lineage.len() == 1 {
                copies.push((x.clone(), None));
                continue;
            }
            let mut ps: Vec<&str> = vec![&head];
            ps.extend(stored[1..].iter().map(String::as_str));
            let copy = self.commit(
                &ps,
                &m.tree,
                &self.writer_of_commit(x),
                &self.trailer(x, "Agentfs-Cause"),
                &self.trailer(x, "Agentfs-Tree"),
                Some(self.original(x)),
            );
            if lineage.len() > stored.len() {
                let mut g = vec![head.clone()];
                g.extend(lineage[1..].iter().cloned());
                grafts.push((copy.clone(), g));
            }
            copies.push((x.clone(), Some(copy.clone())));
            head = copy;
        }
        Some(Rebased {
            head,
            copies,
            grafts,
        })
    }

    /// Rule 3, parent into fork: move the fork to the planned rebase of its
    /// own commits onto `up` (a fast-forward when none is left), keeping
    /// the pre-rebase head under `refs/rebased/<n>` so every original stays
    /// reachable. Records no collision.
    fn sync(&mut self, x: Tree, up: &str, planned: Option<Rebased>) {
        let Some(r) = planned else { return };
        let before = self.head(x);
        if !r.copies.is_empty() {
            let reference = format!("refs/rebased/{}", self.syncs.len() + 1);
            self.update_ref(&[&reference, &before]);
            self.replication_due = true;
            for (copy, parents) in &r.grafts {
                let mut args = vec!["replace", "--graft", copy.as_str()];
                args.extend(parents.iter().map(String::as_str));
                git(&self.repo, &args);
                self.ref_moves += 1;
            }
            for (orig, copy) in &r.copies {
                self.rebased += 1;
                match copy {
                    Some(c) => {
                        let o = self.original(orig).to_string();
                        self.originals.insert(c.clone(), o.clone());
                        self.copies.entry(o).or_default().push(c.clone());
                    }
                    None => self.rebased_empty += 1,
                }
            }
        }
        self.note(format!(
            "synced {up:.7} into {x:?}: {} replayed",
            r.copies.len()
        ));
        self.set_head(x, &r.head);
        if let Tree::Fork(f) = x {
            self.forks.get_mut(&f).expect("fork").base = up.to_string();
        }
        self.syncs.push((x, up.to_string(), before, r.head.clone()));
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

    /// What a run decided, without commit ids (they carry dates).
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
                "\n{:?} {:?} {} {:?} {:?} {}",
                c.tree, c.direction, c.path, c.ours, c.theirs, c.by_file
            ));
        }
        s
    }

    /// Every ref's commits as stored.
    pub fn all_commits(&self) -> Vec<String> {
        git(&self.repo, &["--no-replace-objects", "rev-list", "--all"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Whether commit `a` is in `b`'s stored history (grafts ignored).
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

/// A commit's parents in `repo`, grafts applied unless `stored`.
pub(crate) fn commit_parents(repo: &Path, c: &str, stored: bool) -> Vec<String> {
    let mut args = vec!["rev-list", "--parents", "-n1", c];
    if stored {
        args.insert(0, "--no-replace-objects");
    }
    git(repo, &args)
        .split(' ')
        .skip(1)
        .map(str::to_string)
        .collect()
}

impl Drop for World {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
