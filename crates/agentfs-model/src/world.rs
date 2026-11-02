//! The model: main and each fork as a git branch, each save a commit naming
//! one writer, and the owner's 2026-11-02 rules on top (MODEL.md).
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | [`World::save`] never fails; a save is a commit on the writer's tree at once |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`]s per tree; a [`Step::Next`] takes its head, always |
//! | 3. Fast-forward, else line merge, upstream wins | `land` (a fork commit into its parent: `git merge-tree --merge-base=<commit^1> -X ours`) and `sync` (the parent into a fork: each parent commit the fork lacks, `git merge-tree --merge-base=<commit^1> -X theirs`, [`SyncBase`]) |
//! | 4. Nothing waits behind a collision | there is no held state: a collision is recorded as a [`Collision`], its loser kept under `refs/collisions/<n>`, and the queue moves on |
//! | 5. Transactions | [`World::begin`]: a branch `txn/<n>` frozen at the fork's head; [`World::commit_txn`] merges it or rejects it; [`World::rollback`] drops it |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a sync is a merge with the parent's commit as second parent |
//! | 7. History setting | [`HistoryPolicy`]: a landing is a merge with the fork's commit as second parent (or a fast-forward), or one single-parent commit |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git ancestry.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, EMPTY, Lineage, PATHS, SLOTS};
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
    /// A landing is one single-parent commit: main's ancestry holds only
    /// main's own commits.
    Squashed,
}

/// The design choice under rule 3: how much of the losing side a
/// same-line collision costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionScope {
    /// Only the colliding hunks: the losing edit's clean hunks still land
    /// (`git merge -X ours`). The recommendation.
    Hunk,
    /// The whole file: the upstream's version of a colliding file wins
    /// (`git checkout --ours <file>` after a conflicted merge).
    File,
}

/// The design choice under rules 2 and 3: the merge base of a sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncBase {
    /// Each parent commit the fork lacks is applied in order on its own
    /// base (its first parent), as each fork commit lands on its own (rule
    /// 2: "each edit's base"); the parent's landing of a commit the fork
    /// already holds is skipped. The model's choice.
    EachEdit,
    /// git's own merge base (`git merge`). Under `FullLineage` a fork's sync
    /// and its parent's landing of the fork's commit cross; git merges the
    /// several bases leaving conflict markers in the base, which
    /// manufactures collisions between edits that saw each other
    /// (coherence finding 2).
    Git,
}

#[derive(Clone, Copy, Debug)]
pub struct Policies {
    pub history: HistoryPolicy,
    pub collision: CollisionScope,
    pub sync_base: SyncBase,
}

impl Policies {
    /// The model's choices under `history` and `collision`.
    pub fn new(history: HistoryPolicy, collision: CollisionScope) -> Self {
        Self {
            history,
            collision,
            sync_base: SyncBase::EachEdit,
        }
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
    /// The tokens it staged.
    pub tokens: Vec<String>,
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
    /// The parent's commit, to merge into this fork.
    Sync { commit: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A fork's commit into its parent.
    Landing,
    /// A parent's commit into a fork.
    Sync,
}

/// Rule 3's event: a same-line collision, which the upstream side won.
#[derive(Clone, Debug)]
pub struct Collision {
    /// The tree whose new content it decided.
    pub tree: Tree,
    pub direction: Direction,
    pub path: String,
    /// The slots the losing side lost.
    pub slots: Vec<usize>,
    /// The losing side's tokens on those slots.
    pub lost: Vec<String>,
    /// The upstream side's commit.
    pub winner: String,
    /// The losing side's commit, kept reachable by `reference`.
    pub loser: String,
    /// The tree's commit after the merge.
    pub result: String,
    pub reference: String,
}

/// What a transaction's commit tells the program (rule 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxnOutcome {
    Committed,
    /// A file it touched has a same-line collision with the fork's head:
    /// the branch is dropped.
    Rejected,
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

/// One merge's same-line collisions, per file.
struct Merged {
    tree: String,
    collisions: Vec<(String, Vec<usize>, Vec<String>)>,
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
    /// by a landing (whether it won or lost there).
    pub entered: Vec<(Tree, Edit)>,
    /// Every landing taken: (target, the commit landed).
    pub landings: Vec<(Tree, String)>,
    /// Landings that were fast-forwards.
    pub fast_forwards: usize,
    /// Tokens of rolled-back and rejected transactions.
    pub discarded: BTreeSet<String>,
    pub outcomes: Vec<(ForkId, TxnId, TxnOutcome)>,
    pub log: Vec<String>,
    objects: RefCell<BTreeMap<String, Content>>,
    /// `check`'s state: its last view of each tree and caches over
    /// immutable git objects.
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
    pub(crate) checked_commits: BTreeSet<String>,
    pub(crate) reach: BTreeMap<String, BTreeSet<String>>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) collisions_checked: usize,
    pub(crate) main_checked: Option<String>,
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
            landings: Vec::new(),
            fast_forwards: 0,
            discarded: BTreeSet::new(),
            outcomes: Vec::new(),
            log: Vec::new(),
            objects: RefCell::new(BTreeMap::new()),
            views: BTreeMap::new(),
            checked_commits: BTreeSet::new(),
            reach: BTreeMap::new(),
            grepped: (0, BTreeSet::new()),
            collisions_checked: 0,
            main_checked: None,
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
            let blob = git_in(&self.repo, &["hash-object", "-w", "--stdin"], &c.render());
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

    pub fn is_ancestor(&self, a: &str, b: &str) -> bool {
        try_git(&self.repo, &["merge-base", "--is-ancestor", a, b], None).code == 0
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

    /// A three-way merge of `ours` and `theirs` (commits); `base`: an
    /// explicit merge base, else git's own (ancestry). `upstream_ours`:
    /// which side is upstream and wins a same-line collision. Per
    /// [`CollisionScope`], the losing side loses the colliding hunks or the
    /// whole file; each file's loss is reported.
    fn merge(&self, ours: &str, theirs: &str, base: Option<&str>, upstream_ours: bool) -> Merged {
        let run = |x: Option<&str>| {
            let mut args = vec![
                "merge-tree".to_string(),
                "--write-tree".into(),
                "--name-only".into(),
                "--no-messages".into(),
            ];
            if let Some(b) = base {
                args.push(format!("--merge-base={b}"));
            }
            if let Some(x) = x {
                args.push(format!("-X{x}"));
            }
            args.extend([ours.to_string(), theirs.to_string()]);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
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
        let (win, lose) = if upstream_ours {
            ("ours", "theirs")
        } else {
            ("theirs", "ours")
        };
        let resolved = |x| {
            let o = run(Some(x));
            assert_eq!(o.code, 0, "merge-tree -X{x} resolves: {}", o.stderr);
            o.stdout.lines().next().expect("a tree").to_string()
        };
        let (won, lost_tree) = (resolved(win), resolved(lose));
        let upstream = if upstream_ours { ours } else { theirs };
        let mut whole = BTreeMap::new();
        let mut collisions = Vec::new();
        for p in &paths {
            let final_ = match self.policies.collision {
                CollisionScope::Hunk => self.file(&won, p),
                CollisionScope::File => {
                    let up = self.file(upstream, p);
                    whole.insert(p.clone(), up.clone());
                    up
                }
            };
            let l = self.file(&lost_tree, p);
            let slots: Vec<usize> = (0..SLOTS)
                .filter(|i| final_.slot(*i) != l.slot(*i))
                .collect();
            let lost = slots
                .iter()
                .map(|i| l.slot(*i).to_string())
                .filter(|t| t != EMPTY)
                .collect();
            collisions.push((p.clone(), slots, lost));
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
        for (path, slots, lost) in &m.collisions {
            let reference = format!("refs/collisions/{}", self.collisions.len() + 1);
            self.update_ref(&[&reference, loser]);
            self.note(format!(
                "collision in {t:?}:{path}{slots:?} ({dir:?}): {lost:?} lost to upstream"
            ));
            self.collisions.push(Collision {
                tree: t,
                direction: dir,
                path: path.clone(),
                slots: slots.clone(),
                lost: lost.clone(),
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
                .push_back(Entry::Sync {
                    commit: head.clone(),
                });
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
        self.lineage.wrote(&token, cur.slot(slot));
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
                txn.tokens.push(token.clone());
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
            tokens: Vec::new(),
        });
        self.note(format!("begin t{id} on f{f}"));
        Ok(id)
    }

    /// Commit: merge the branch onto the fork's current head as one merge
    /// commit named for the client, which then lands like any fork commit.
    /// A same-line collision in any file it touched rejects it: the branch
    /// is dropped and the program reads the fork's head.
    pub fn commit_txn(&mut self, f: ForkId) -> Result<TxnOutcome, ModelError> {
        let txn = self.take_txn(f)?;
        let ours = self.forks[&f].head.clone();
        let outcome = if txn.head == txn.start {
            TxnOutcome::Empty
        } else {
            let m = self.merge(&ours, &txn.head, None, true);
            if m.collisions.is_empty() {
                let writer = self.forks[&f].writer.clone();
                let c = self.commit(
                    &[&ours, &txn.head],
                    &m.tree,
                    &writer,
                    &format!("Transaction t{}", txn.id),
                    &Self::name(Tree::Fork(f)),
                );
                self.set_head(Tree::Fork(f), &c);
                for e in self.edits(&ours, &c) {
                    self.entered.push((Tree::Fork(f), e));
                }
                self.moved(Tree::Fork(f), true);
                TxnOutcome::Committed
            } else {
                self.discarded.extend(txn.tokens.iter().cloned());
                TxnOutcome::Rejected
            }
        };
        self.note(format!("commit t{} on f{f}: {outcome:?}", txn.id));
        self.outcomes.push((f, txn.id, outcome));
        Ok(outcome)
    }

    /// Rollback (abandon): drop the branch.
    pub fn rollback(&mut self, f: ForkId) -> Result<(), ModelError> {
        let txn = self.take_txn(f)?;
        self.discarded.extend(txn.tokens.iter().cloned());
        self.note(format!("rollback t{} on f{f}", txn.id));
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
                    Entry::Sync { commit } => self.sync(*t, &commit),
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
    /// the target's head, else a line merge of the commit's own change
    /// (merge base: its first parent) in which the target wins collisions.
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
        if full && base == ours {
            self.set_head(x, c);
            self.fast_forwards += 1;
            self.note(format!("landed f{from}'s {c:.7} into {x:?}: fast-forward"));
            self.moved(x, true);
            return;
        }
        let m = self.merge(&ours, c, Some(&base), true);
        let new = if full {
            self.commit(&[&ours, c], &m.tree, &writer, "Landing", &Self::name(x))
        } else if m.tree == self.tree_of(&ours) {
            ours.clone()
        } else {
            self.commit(&[&ours], &m.tree, &writer, "Landing", &Self::name(x))
        };
        self.record(x, Direction::Landing, &m, &ours, c, &new);
        self.note(format!("landed f{from}'s {c:.7} into {x:?}"));
        if new != ours {
            self.set_head(x, &new);
            self.moved(x, true);
        }
    }

    /// Rule 3, parent into fork: nothing if the fork has it, a
    /// fast-forward if the fork has nothing of its own, else a merge
    /// commit labelled `Sync` with the parent's commit as second parent,
    /// in which the parent wins collisions. Merge base: per [`SyncBase`].
    fn sync(&mut self, x: Tree, c: &str) {
        let Tree::Fork(f) = x else {
            unreachable!("main has no parent")
        };
        let ours = self.head(x);
        if self.is_ancestor(c, &ours) {
            return;
        }
        if self.is_ancestor(&ours, c) {
            self.set_head(x, c);
            self.note(format!("synced {c:.7} into f{f}: fast-forward"));
            self.moved(x, false);
            return;
        }
        let merges = match self.policies.sync_base {
            SyncBase::EachEdit => self.replay(&ours, c),
            SyncBase::Git => vec![self.merge(&ours, c, None, false)],
        };
        let tree = match merges.last() {
            Some(m) => m.tree.clone(),
            None => self.tree_of(&ours),
        };
        let new = self.commit(&[&ours, c], &tree, "agentfs", "Sync", &Self::name(x));
        for m in &merges {
            self.record(x, Direction::Sync, m, c, &ours, &new);
        }
        self.set_head(x, &new);
        self.note(format!("synced {c:.7} into f{f}"));
        self.moved(x, false);
    }

    /// [`SyncBase::EachEdit`]: the parent's first-parent commits from
    /// `up` back to what the fork holds, oldest first, each merged onto the
    /// fork's tree on its own base (its first parent), the parent winning
    /// collisions. A merge whose second parent the fork already holds (the
    /// parent's landing of the fork's own commit) brings nothing new: what
    /// that landing kept of the parent's came in an earlier commit here.
    fn replay(&self, ours: &str, up: &str) -> Vec<Merged> {
        let list = git(
            &self.repo,
            &["rev-list", "--reverse", "--first-parent", up, "--not", ours],
        );
        let mut acc = ours.to_string();
        let mut out = Vec::new();
        for k in list.lines() {
            let parents = git(&self.repo, &["rev-list", "--parents", "-n1", k]);
            let parents: Vec<&str> = parents.split(' ').skip(1).collect();
            if let [_, landed] = parents[..]
                && self.is_ancestor(landed, ours)
            {
                continue;
            }
            let m = self.merge(&acc, k, Some(parents[0]), false);
            acc = m.tree.clone();
            out.push(m);
        }
        out
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

    /// Every ref's commits, for history checks.
    pub fn all_commits(&self) -> Vec<String> {
        git(&self.repo, &["rev-list", "--all"])
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if std::env::var_os("AGENTFS_MODEL_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
