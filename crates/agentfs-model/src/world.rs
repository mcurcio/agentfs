//! The model: main and each fork as a git branch, each save a commit naming
//! one writer, and the owner's 2026-11-02 rules on top (MODEL.md).
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | [`World::save`] never fails; a save is a commit on the writer's tree at once |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`]s per tree; a [`Step::Next`] takes its head, always |
//! | 3. Fast-forward, else line merge, the incoming change wins | `land` (a fork commit's own edit into its parent) and `sync` (the parent's current version into a fork), both through `resolve`: per line, the newer of two values by git ancestry, else the incoming one ([`MergeRule`]) |
//! | 4. Nothing waits behind a collision | there is no held state: a collision is recorded as a [`Collision`] naming what won, its loser kept under `refs/collisions/<n>`, and the queue moves on |
//! | 5. Transactions | [`World::begin`]: a branch `txn/<n>` frozen at the fork's head; [`World::commit_txn`] merges it onto the fork's head, its lines winning (it is the incoming change); [`World::rollback`] drops it |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a sync is a merge with the parent's commit as second parent |
//! | 7. History setting | [`HistoryPolicy`]: a landing is a merge with the fork's commit as second parent (or a fast-forward), or one single-parent commit whose carried fork commit is kept beside history, as a `git replace` graft |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git ancestry.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, EMPTY, Layout, Lineage, PATHS, SLOTS};
use crate::git::{git, git_in, try_git};

pub type ForkId = u32;
pub type TxnId = u32;

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
    /// main's own commits. The fork commit it carried is recorded beside
    /// history, as a graft (`git replace --graft <commit> <parent> <fork
    /// commit>` under `refs/replace/`), which git's ancestry and blame read
    /// and `--no-replace-objects` (main's history as stored and pushed)
    /// does not. Without it a sync cannot tell a fork's own landed line
    /// from a concurrent one (coherence finding 2, resolved).
    Squashed,
}

/// The design choice under rule 3: how much of the losing side a
/// same-line collision costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionScope {
    /// Only the colliding lines: the tree's other lines in that file stay.
    /// The recommendation. Narrower than git's hunk: `git merge -X theirs`
    /// takes a whole conflicting hunk, and neighbouring lines share one
    /// (coherence finding 6).
    Line,
    /// The whole file: the incoming version of a colliding file replaces
    /// the tree's (`git checkout --theirs <file>` after a conflicted merge).
    File,
}

/// The design choice under rules 2 and 3: how a merge tells a newer line
/// from a concurrent one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRule {
    /// Per line, from git alone. A landing carries the fork commit's own
    /// edit (its diff from its first parent). A sync carries the parent's
    /// current version, less each line whose last change on the parent's
    /// first-parent chain the fork already holds (`git blame
    /// --first-parent`). Each carried line then meets the tree's line: the
    /// one whose writing commit (`git blame`) descends from the other's
    /// wins; if neither does, they are concurrent and the incoming one
    /// wins. The model's choice.
    PerLine,
    /// git's own three-way merge of the parent's current version into the
    /// fork (`git merge-tree -X theirs`, git's merge base). Crossing syncs
    /// and landings give two merge bases; git merges them leaving conflict
    /// markers in the base, so a fork's newer line "collides" with its own
    /// older one and loses (coherence finding 1).
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
    /// The parent moved: merge its current version into this fork (rule
    /// 3: a sync carries the parent's current version, never a stale one).
    Sync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A fork's commit into its parent.
    Landing,
    /// A parent's commit into a fork.
    Sync,
    /// A transaction's branch into its fork, at commit.
    Transaction,
}

/// Rule 3's event: a same-line collision, which the incoming change won.
#[derive(Clone, Debug)]
pub struct Collision {
    /// The tree whose new content it decided.
    pub tree: Tree,
    pub direction: Direction,
    pub path: String,
    /// The slots the tree's lines lost.
    pub slots: Vec<usize>,
    /// The tree's tokens on those slots, which lost.
    pub lost: Vec<String>,
    /// The incoming tokens that overwrote them, slot for slot.
    pub won: Vec<String>,
    /// The incoming commit: the fork's commit (landing), the parent's
    /// head (sync) or the transaction's branch (commit).
    pub winner: String,
    /// The tree's head before, kept reachable by `reference`.
    pub loser: String,
    /// The tree's commit after the merge.
    pub result: String,
    pub reference: String,
}

/// How a transaction ended (rule 5), with what it staged: `check` holds
/// every staged edit to its end (in the fork at commit, or discarded).
#[derive(Clone, Debug)]
pub struct Ended {
    pub fork: ForkId,
    pub txn: TxnId,
    pub staged: Vec<Edit>,
    pub how: TxnEnd,
    /// The fork's commit that carries it, when committed.
    pub result: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnEnd {
    Commit(TxnOutcome),
    /// Rollback, abandon, or the client's death.
    Rollback,
}

/// What a transaction's commit tells the program (rule 5). A commit is
/// never refused: it is the incoming change into its fork, so it wins a
/// same-line collision there (the owner's correction of 2026-11-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    Committed,
    /// It staged nothing.
    Empty,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The fork has one open already.
    InTransaction,
    NoTransaction,
    Ended,
}

/// A step the scheduler can take: every one always completes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// Take the head of a tree's queue.
    Next(Tree),
    Drop(ForkId),
}

/// One slot a commit changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub path: String,
    pub slot: usize,
    pub token: String,
}

/// One merge's result and its same-line collisions.
struct Merged {
    tree: String,
    collisions: Vec<Lost>,
}

/// One file's lines lost in a merge: the slots, the tree's tokens there,
/// and the incoming tokens that won them, slot for slot.
struct Lost {
    path: String,
    slots: Vec<usize>,
    lost: Vec<String>,
    won: Vec<String>,
}

/// One line an incoming change sets ([`MergeRule::PerLine`]).
struct Incoming {
    path: String,
    slot: usize,
    token: String,
    /// The line this edit was made on, for a commit's own edit (a landing,
    /// a transaction); a sync carries a version, not an edit.
    base: Option<String>,
    /// The commit that wrote `token` (`git blame`).
    origin: String,
    /// For a sync, the commit on the parent's first-parent chain that set
    /// this line there (`git blame --first-parent`): the parent's own
    /// decision. A tree line that decision already saw lost in the parent,
    /// so it is an update here, not a new collision.
    decided: Option<String>,
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

pub struct World {
    pub policies: Policies,
    root: PathBuf,
    repo: PathBuf,
    pub main_head: String,
    pub forks: BTreeMap<ForkId, Fork>,
    next_fork: ForkId,
    next_txn: TxnId,
    queues: BTreeMap<Tree, VecDeque<Entry>>,
    pub lineage: Lineage,
    counters: BTreeMap<String, u32>,
    pub collisions: Vec<Collision>,
    /// Every token that entered a tree: written in it, or carried into it
    /// by a landing, a sync or a transaction (whether it won or lost
    /// there).
    pub entered: Vec<(Tree, Edit)>,
    /// Every sync that moved a fork: (the fork, the parent's head it
    /// carried, the fork's head after).
    pub syncs: Vec<(Tree, String, String)>,
    /// Every landing taken: (target, the commit landed).
    pub landings: Vec<(Tree, String)>,
    /// Landings that were fast-forwards.
    pub fast_forwards: usize,
    /// Tokens of rolled-back transactions.
    pub discarded: BTreeSet<String>,
    pub ended: Vec<Ended>,
    pub log: Vec<String>,
    objects: RefCell<BTreeMap<String, Content>>,
    blames: RefCell<BTreeMap<String, Vec<String>>>,
    ancestry: RefCell<BTreeMap<(String, String), bool>>,
    /// The test oracle's record of what each commit's writer could have
    /// seen: every token in its tree or in any commit it merged or carried
    /// (a squashed landing's carried commit included), kept apart from
    /// git and its grafts. [`Lineage`] takes each write's view from it.
    known: RefCell<BTreeMap<String, BTreeSet<String>>>,
    /// `check`'s state: its last view of each tree and caches over
    /// immutable git objects.
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
    pub(crate) checked_commits: BTreeSet<String>,
    /// Per head: the commits its stored history reaches, and those its
    /// lineage (history plus grafts) reaches.
    pub(crate) reach: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    /// Main's stored history, checked for `Squashed`.
    pub(crate) main_history: BTreeSet<String>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) collisions_checked: usize,
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
        std::fs::create_dir_all(&repo).expect("create the repository");
        git(&repo, &["init", "-q", "-b", "main"]);
        let mut w = Self {
            policies,
            root,
            repo,
            main_head: String::new(),
            forks: BTreeMap::new(),
            next_fork: 1,
            next_txn: 1,
            queues: BTreeMap::new(),
            lineage: Lineage::default(),
            counters: BTreeMap::new(),
            collisions: Vec::new(),
            entered: Vec::new(),
            syncs: Vec::new(),
            landings: Vec::new(),
            fast_forwards: 0,
            discarded: BTreeSet::new(),
            ended: Vec::new(),
            log: Vec::new(),
            objects: RefCell::new(BTreeMap::new()),
            blames: RefCell::new(BTreeMap::new()),
            ancestry: RefCell::new(BTreeMap::new()),
            known: RefCell::new(BTreeMap::new()),
            views: BTreeMap::new(),
            checked_commits: BTreeSet::new(),
            reach: BTreeMap::new(),
            grepped: (0, BTreeSet::new()),
            collisions_checked: 0,
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

    fn name(t: Tree) -> String {
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
        let c = git_in(&self.repo, &args, &msg);
        let mut k = self.known(parents);
        for p in PATHS {
            k.extend(self.file(tree, p).tokens().map(str::to_string));
        }
        self.known.borrow_mut().insert(c.clone(), k);
        c
    }

    /// The oracle's view: every token known at any of `commits`.
    fn known(&self, commits: &[&str]) -> BTreeSet<String> {
        let known = self.known.borrow();
        commits
            .iter()
            .flat_map(|c| known.get(*c).into_iter().flatten().cloned())
            .collect()
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

    /// Per slot of `p` at commit `at`, the commit that set its line (`git
    /// blame`): the commit that wrote it, or with `first_parent` the
    /// commit on `at`'s first-parent chain that last changed it (the
    /// tree's own decision). Cached: commits never change.
    fn blame(&self, at: &str, p: &str, first_parent: bool) -> Vec<String> {
        let key = format!("{at}:{p}:{first_parent}");
        if let Some(b) = self.blames.borrow().get(&key) {
            return b.clone();
        }
        let mut args = vec!["blame", "--porcelain"];
        if first_parent {
            args.push("--first-parent");
        }
        args.extend([at, "--", p]);
        let mut by_line = BTreeMap::new();
        for line in git(&self.repo, &args).lines() {
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() >= 3
                && f[0].len() == 40
                && f[0].bytes().all(|b| b.is_ascii_hexdigit())
                && let (Ok(_), Ok(n)) = (f[1].parse::<usize>(), f[2].parse::<usize>())
            {
                by_line.insert(n, f[0].to_string());
            }
        }
        let b: Vec<String> = (0..SLOTS)
            .map(|i| by_line[&Content::line_of(i, self.policies.layout)].clone())
            .collect();
        self.blames.borrow_mut().insert(key, b.clone());
        b
    }

    /// The writer a commit names.
    pub fn writer_of_commit(&self, c: &str) -> String {
        git(
            &self.repo,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Agentfs-Writer,valueonly)",
                c,
            ],
        )
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

    /// Rule 3 under [`MergeRule::PerLine`]: each incoming line meets the
    /// line of `ours` (a commit) on its slot. An incoming line `ours`
    /// already superseded (its writing commit is an ancestor of the one
    /// that wrote `ours`'s line) is stale and dropped, unless it is a
    /// commit's own edit made on the line `ours` still has. Every other
    /// incoming line replaces the tree's; when the tree's line was not
    /// written before it (neither writing commit descends from the other)
    /// the two are concurrent, and the collision is reported: also when a
    /// fork's commit carries a collision it decided itself (a nested
    /// landing), since it is one in this tree too; not when the parent's
    /// decision a sync carries already saw the tree's line (it lost there,
    /// and is recorded there). Per
    /// [`CollisionScope::File`], a file with a collision takes `whole`'s
    /// version of the file instead.
    fn resolve(&self, ours: &str, incoming: &[Incoming], whole: &str) -> Merged {
        let mut by_path: BTreeMap<&str, Vec<&Incoming>> = BTreeMap::new();
        for i in incoming {
            by_path.entry(&i.path).or_default().push(i);
        }
        let mut files = BTreeMap::new();
        let mut collisions = Vec::new();
        for (p, ins) in by_path {
            let cur = self.file(ours, p);
            let mut new = cur.clone();
            let mut hit = Vec::new();
            for i in ins {
                let c = cur.slot(i.slot);
                if c == i.token {
                    continue;
                }
                let mine = &self.blame(ours, p, false)[i.slot];
                if i.base.as_deref() == Some(c) || !self.is_ancestor(&i.origin, mine) {
                    new = new.with(i.slot, &i.token);
                    let decided_upstream = i
                        .decided
                        .as_ref()
                        .is_some_and(|d| self.is_ancestor(mine, d));
                    if !self.is_ancestor(mine, &i.origin) && !decided_upstream {
                        hit.push(i.slot);
                    }
                }
            }
            if !hit.is_empty() && self.policies.collision == CollisionScope::File {
                new = self.file(whole, p);
                hit = (0..SLOTS)
                    .filter(|i| cur.slot(*i) != EMPTY && cur.slot(*i) != new.slot(*i))
                    .collect();
            }
            if !hit.is_empty() {
                let lost = hit.iter().map(|i| cur.slot(*i).to_string()).collect();
                let won = hit.iter().map(|i| new.slot(*i).to_string()).collect();
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
        Merged { tree, collisions }
    }

    /// The lines a commit's own edit sets: its diff from `base`, each with
    /// the line it was made on and its writing commit.
    fn edit_of(&self, base: &str, c: &str) -> Vec<Incoming> {
        self.edits(base, c)
            .into_iter()
            .map(|e| Incoming {
                base: Some(self.file(base, &e.path).slot(e.slot).to_string()),
                origin: self.blame(c, &e.path, false)[e.slot].clone(),
                decided: None,
                path: e.path,
                slot: e.slot,
                token: e.token,
            })
            .collect()
    }

    /// The lines a sync carries: each line of the parent's current version
    /// `up` that differs from the fork's `ours`, less each whose last
    /// change on the parent's first-parent chain `ours` already holds (the
    /// fork saw that decision and has moved on since).
    fn version_of(&self, ours: &str, up: &str) -> Vec<Incoming> {
        let mut out = Vec::new();
        for p in git(&self.repo, &["diff-tree", "-r", "--name-only", ours, up]).lines() {
            let (mine, theirs) = (self.file(ours, p), self.file(up, p));
            let decided = self.blame(up, p, true);
            let origin = self.blame(up, p, false);
            for i in 0..SLOTS {
                if mine.slot(i) != theirs.slot(i) && !self.is_ancestor(&decided[i], ours) {
                    out.push(Incoming {
                        path: p.to_string(),
                        slot: i,
                        token: theirs.slot(i).to_string(),
                        base: None,
                        origin: origin[i].clone(),
                        decided: Some(decided[i].clone()),
                    });
                }
            }
        }
        out
    }

    /// [`MergeRule::GitMergeBase`]: git's own three-way merge of `theirs`
    /// into `ours`, on git's merge base, the incoming side (`theirs`)
    /// winning each collision (per [`CollisionScope`]).
    fn git_merge(&self, ours: &str, theirs: &str) -> Merged {
        let run = |x: Option<&str>| {
            let mut args = vec!["merge-tree", "--write-tree", "--name-only", "--no-messages"];
            let x = x.map(|x| format!("-X{x}"));
            if let Some(x) = &x {
                args.push(x);
            }
            args.extend([ours, theirs]);
            try_git(&self.repo, &args, None)
        };
        let plain = run(None);
        let mut lines = plain.stdout.lines();
        let tree = lines.next().expect("merge-tree prints a tree").to_string();
        match plain.code {
            0 => {
                return Merged {
                    tree,
                    collisions: Vec::new(),
                };
            }
            1 => {}
            c => panic!("git merge-tree failed ({c}): {}", plain.stderr),
        }
        let paths: BTreeSet<String> = lines
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let resolved = |x| {
            let o = run(Some(x));
            // Only content conflicts arise: deletes and renames, whose
            // modify/delete and rename conflicts `-X` does not resolve, are
            // left out of the model (MODEL.md).
            assert_eq!(o.code, 0, "merge-tree -X{x} resolves: {}", o.stderr);
            o.stdout.lines().next().expect("a tree").to_string()
        };
        let (won, kept) = (resolved("theirs"), resolved("ours"));
        let mut whole = BTreeMap::new();
        let mut collisions = Vec::new();
        for p in &paths {
            let final_ = match self.policies.collision {
                CollisionScope::Line => self.file(&won, p),
                CollisionScope::File => {
                    let f = self.file(theirs, p);
                    whole.insert(p.clone(), f.clone());
                    f
                }
            };
            let l = self.file(&kept, p);
            let slots: Vec<usize> = (0..SLOTS)
                .filter(|i| final_.slot(*i) != l.slot(*i) && l.slot(*i) != EMPTY)
                .collect();
            let lost = slots.iter().map(|i| l.slot(*i).to_string()).collect();
            let wins = slots.iter().map(|i| final_.slot(*i).to_string()).collect();
            collisions.push(Lost {
                path: p.clone(),
                slots,
                lost,
                won: wins,
            });
        }
        let tree = if whole.is_empty() {
            won
        } else {
            self.tree_with(Some(&won), &whole)
        };
        Merged { tree, collisions }
    }

    /// Records a merge's collisions in tree `t`, each keeping its loser
    /// reachable under `refs/collisions/<n>`.
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
                "collision in {t:?}:{path}{slots:?} ({dir:?}): {lost:?} lost to incoming {won:?}"
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
        }
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

    /// Rule 1: open, write slot `slot` of `p`, close. Never refused: one
    /// commit at once on the client's tree (on its transaction's branch
    /// while one is open).
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let (tree, writer, txn) = match c {
            Client::Main(w) => (Tree::Main, w.clone(), None),
            Client::Fork(f) => {
                let fork = &self.forks[f];
                (
                    Tree::Fork(*f),
                    fork.writer.clone(),
                    fork.txn.as_ref().map(|t| (t.id, t.head.clone())),
                )
            }
        };
        let at = match &txn {
            Some((_, h)) => h.clone(),
            None => self.head(tree),
        };
        let cur = self.file(&at, p);
        let token = self.token(&writer, txn.as_ref().map(|t| t.0));
        self.lineage.wrote(&token, self.known(&[&at]));
        let files = BTreeMap::from([(p.to_string(), cur.with(slot, &token))]);
        let t = self.tree_with(Some(&at), &files);
        self.note(format!("save {token} into {tree:?}:{p}[{slot}]"));
        match txn {
            Some((id, _)) => {
                let on = format!("txn/{id}");
                let commit = self.commit(&[&at], &t, &writer, "Direct", &on);
                self.update_ref(&[&format!("refs/heads/{on}"), &commit]);
                let Client::Fork(f) = c else {
                    unreachable!("only a fork's client has a transaction")
                };
                let fork = self.forks.get_mut(f).expect("fork");
                let txn = fork.txn.as_mut().expect("open");
                txn.head = commit;
                txn.staged.push(Edit {
                    path: p.to_string(),
                    slot,
                    token: token.clone(),
                });
            }
            None => {
                let c = self.commit(&[&at], &t, &writer, "Direct", &Self::name(tree));
                self.set_head(tree, &c);
                self.entered.push((
                    tree,
                    Edit {
                        path: p.to_string(),
                        slot,
                        token: token.clone(),
                    },
                ));
                self.moved(tree, true);
            }
        }
        token
    }

    // ── transactions (rule 5) ────────────────────────────────────────────

    /// Begin: a branch frozen at the fork's head. The fork keeps receiving
    /// syncs; the program reads its own staged edits on the snapshot.
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

    /// Commit: merge the branch onto the fork's current head as one merge
    /// commit named for the client, which then lands like any fork commit.
    /// Never refused: the transaction is the incoming change into its fork,
    /// so its line wins a same-line collision with a line a sync brought in
    /// since begin (rule 3), recorded like any other. So the outcome does
    /// not depend on whether that sync arrived before or after the commit.
    pub fn commit_txn(&mut self, f: ForkId) -> Result<TxnOutcome, ModelError> {
        let txn = self.take_txn(f)?;
        let ours = self.forks[&f].head.clone();
        let mut result = None;
        let outcome = if txn.head == txn.start {
            TxnOutcome::Empty
        } else {
            let edit = self.edit_of(&txn.start, &txn.head);
            let m = self.resolve(&ours, &edit, &txn.head);
            let writer = self.forks[&f].writer.clone();
            let t = Tree::Fork(f);
            let c = self.commit(
                &[&ours, &txn.head],
                &m.tree,
                &writer,
                &format!("Transaction t{}", txn.id),
                &Self::name(t),
            );
            self.record(t, Direction::Transaction, &m, &txn.head, &ours, &c);
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
            TxnOutcome::Committed
        };
        self.note(format!("commit t{} on f{f}: {outcome:?}", txn.id));
        self.ended.push(Ended {
            fork: f,
            txn: txn.id,
            staged: txn.staged,
            how: TxnEnd::Commit(outcome),
            result,
        });
        Ok(outcome)
    }

    /// Rollback (abandon): drop the branch.
    pub fn rollback(&mut self, f: ForkId) -> Result<(), ModelError> {
        let txn = self.take_txn(f)?;
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

    /// The client leaves or dies: an open transaction is dropped; the
    /// fork's commits still land; it drops once that is done.
    pub fn end_client(&mut self, f: ForkId) {
        if self.forks[&f].txn.is_some() {
            let _ = self.rollback(f);
        }
        self.forks.get_mut(&f).expect("fork").life = Life::Ended;
        self.note(format!("end f{f}"));
    }

    // ── steps ────────────────────────────────────────────────────────────

    /// Every step that can act now: the head of each live tree's queue,
    /// and each fork ready to drop.
    pub fn pending(&self) -> Vec<Step> {
        let mut out: Vec<Step> = self
            .queues
            .iter()
            .filter(|(t, q)| !q.is_empty() && self.alive(**t))
            .map(|(t, _)| Step::Next(*t))
            .collect();
        out.extend(
            self.forks
                .keys()
                .filter(|f| self.droppable(**f))
                .map(|f| Step::Drop(*f)),
        );
        out
    }

    /// Takes one step. It always completes: nothing is held.
    pub fn step(&mut self, s: &Step) {
        match s {
            Step::Next(t) => {
                let e = self
                    .queues
                    .get_mut(t)
                    .and_then(VecDeque::pop_front)
                    .expect("a queued entry");
                match e {
                    Entry::Landing { from, commit } => self.land(*t, from, &commit),
                    Entry::Sync => self.sync(*t),
                }
            }
            Step::Drop(f) => self.drop_fork(*f),
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

    /// Rule 3, fork into parent: a fast-forward when the commit's base is
    /// the target's head, else the commit's own edit (its diff from its
    /// first parent) merged line by line, the incoming line winning a
    /// collision.
    fn land(&mut self, x: Tree, from: ForkId, c: &str) {
        let base = self.first_parent(c);
        let ours = self.head(x);
        let writer = self.writer_of_commit(c);
        let edits = self.edits(&base, c);
        let carried: Vec<&str> = edits.iter().map(|e| e.token.as_str()).collect();
        self.note(format!(
            "landing f{from}'s {c:.7} into {x:?} carries {carried:?}"
        ));
        for e in edits {
            self.entered.push((x, e));
        }
        self.landings.push((x, c.to_string()));
        let full = self.policies.history == HistoryPolicy::FullLineage;
        let edit = self.edit_of(&base, c);
        let m = self.resolve(&ours, &edit, c);
        if full && base == ours {
            // A fork's commit can carry a collision the fork decided (a
            // nested landing): it is one in this tree too.
            self.record(x, Direction::Landing, &m, c, &ours, c);
            self.set_head(x, c);
            self.fast_forwards += 1;
            self.note(format!("landed f{from}'s {c:.7} into {x:?}: fast-forward"));
            self.moved(x, true);
            return;
        }
        let new = if full {
            self.commit(&[&ours, c], &m.tree, &writer, "Landing", &Self::name(x))
        } else if m.tree == self.tree_of(&ours) {
            ours.clone()
        } else {
            let new = self.commit(&[&ours], &m.tree, &writer, "Landing", &Self::name(x));
            // Rule 7, `Squashed`: the stored commit has one parent; its
            // lineage, the fork commit it carried, is a graft beside
            // history, made before any read of `new`.
            git(&self.repo, &["replace", "--graft", &new, &ours, c]);
            self.ref_moves += 1;
            let k = self.known(&[&new, c]);
            self.known.borrow_mut().insert(new.clone(), k);
            new
        };
        self.record(x, Direction::Landing, &m, c, &ours, &new);
        self.note(format!("landed f{from}'s {c:.7} into {x:?}"));
        if new != ours {
            self.set_head(x, &new);
            self.moved(x, true);
        }
    }

    /// Rule 3, parent into fork: the parent's current version (never the
    /// one current when the sync was queued). Nothing if the fork has it,
    /// a fast-forward if the fork has nothing of its own, else a merge
    /// commit labelled `Sync` with the parent's head as second parent,
    /// built per [`MergeRule`], the incoming line winning a collision.
    fn sync(&mut self, x: Tree) {
        let Tree::Fork(f) = x else {
            unreachable!("main has no parent")
        };
        let up = self.head(self.forks[&f].parent);
        let ours = self.head(x);
        if self.is_ancestor(&up, &ours) {
            return;
        }
        if self.is_ancestor(&ours, &up) {
            for e in self.edits(&ours, &up) {
                self.entered.push((x, e));
            }
            self.set_head(x, &up);
            self.syncs.push((x, up.clone(), up.clone()));
            self.note(format!("synced {up:.7} into f{f}: fast-forward"));
            self.moved(x, false);
            return;
        }
        let m = match self.policies.merge {
            MergeRule::PerLine => {
                let version = self.version_of(&ours, &up);
                for i in &version {
                    let e = Edit {
                        path: i.path.clone(),
                        slot: i.slot,
                        token: i.token.clone(),
                    };
                    self.entered.push((x, e));
                }
                self.resolve(&ours, &version, &up)
            }
            MergeRule::GitMergeBase => self.git_merge(&ours, &up),
        };
        let new = self.commit(&[&ours, &up], &m.tree, "agentfs", "Sync", &Self::name(x));
        self.record(x, Direction::Sync, &m, &up, &ours, &new);
        self.set_head(x, &new);
        self.syncs.push((x, up.clone(), new.clone()));
        self.note(format!("synced {up:.7} into f{f}"));
        self.moved(x, false);
    }

    fn droppable(&self, f: ForkId) -> bool {
        let fork = &self.forks[&f];
        fork.life == Life::Ended
            && self.children(Tree::Fork(f)).is_empty()
            && !self
                .queue(fork.parent)
                .iter()
                .any(|e| matches!(e, Entry::Landing { from, .. } if *from == f))
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
    /// file, slots and lost tokens. The same calls give the same
    /// fingerprint whatever the commits' dates
    /// (`r2_no_clock_commit_dates_decide_nothing`). Graft refs are named by
    /// commit id, so they are left out; their effect shows in the trees.
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

impl Drop for World {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
