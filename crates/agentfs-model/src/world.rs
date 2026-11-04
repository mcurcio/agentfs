//! The model: main and each fork as a git branch, each save a commit naming
//! one writer, and the owner's rules on top (MODEL.md).
//!
//! | Rule (MODEL.md) | Here |
//! |---|---|
//! | 1. Like a Linux filesystem | a close ([`World::close`]) is never refused; its save is a commit on the writer's tree at once, its first parent the tree's head at that moment |
//! | 2. One landing queue per tree | [`World::queue_len`]: a FIFO of [`Entry`]s per tree; a [`Step::Next`] takes its head, or records that it waits (rule 9) |
//! | 3. The parent's queue decides | `land` (a fork's commit into its parent) is the only place a collision with the parent is decided, through `judge` and `resolve`: per line, a line written over the parent's replaces it, a stale one is dropped, and of two concurrent lines the parent's wins; then each change (a save, a transaction's commit) is judged whole: one lost line and none of it applies (rule 3e). `sync` decides nothing: it takes the parent's version and keeps only the fork's lines the parent's queue has not taken yet |
//! | 4. Nothing waits behind a collision | there is no held state: a collision is recorded as a [`Collision`] and in git (a note on the take's result commit under [`NOTES`]), the fork's losing commit kept under `refs/collisions/<n>`, which Replication later pushes to the shared remote with main, the notes and the grafts ([`Step::Replicate`], due on every move of main and every collision), and the queue moves on |
//! | 5. Transactions | [`World::begin`]: a branch `txn/<n>` frozen at the fork's head; [`World::commit_txn`] lands it in the fork like a child's commit, whole or not at all (the fork is its parent, so the fork's lines win), and answers whether it lost, with every staged line that did not apply; [`World::rollback`] drops it |
//! | 6. Attribution | every commit names one writer in an `Agentfs-Writer` trailer; a sync is a merge with the parent's commit as second parent |
//! | 7. History setting | [`HistoryPolicy`]: a landing is a merge with the fork's commit as second parent (or a fast-forward), or one single-parent commit carrying the saves of a fork's pending run that won, each judged on its own first, as one changeset, whose winning saves are kept beside history as a `git replace` graft; a losing save stays its own commit, kept by its collision ref |
//! | 8. A save records the write calls | [`World::write`] keeps each byte call ([`Write`]: an `O_APPEND` write, `pwrite`, `ftruncate`; [`World::open_trunc`] for `O_TRUNC`) on the open file; [`World::close`] applies them to the tree's head then: an append's bytes go after the file's last byte, finishing a partial last line (a version of it) and then making new lines; other calls leave bytes the save aligns with the head's lines (rule 10). [`World::edit`] makes a test's line edit as a program's byte calls (`program`) |
//! | 9. A take locks the files it overwrites | [`World::open_write`]; a [`Step::Next`] whose sync or landing would overwrite a file open for writing in that tree waits ([`World::waits`]) until the close; then it holds its files ([`World::holds`]) until [`Step::Finish`] writes them; a write-open, [`World::unlink`] or [`World::rename`] of a held file waits until then; a client's death ([`World::end_client`], [`World::end_main_client`]) closes its files |
//! | 10. Line identity | a save derives identities from the bytes at close: it aligns them with the head's lines once, by git's histogram diff (`align`), and records the result: each new line an identity, each changed line a new version written over the version it replaced; the referee and a sync then compare versions of one identity, never bytes or positions (`resolve`, `adopt`, `splice`); a partial line that a merge put another line after reads as terminated (`Content::read_lines`) |
//! | 9, progress (git-model.md §6) | P1: a [`Step::Next`] takes the first entry no earlier waiting entry shares a file or a source with (`scan`), so several may wait ([`World::waits`]); no call ever waits on a waiting entry (P3 is gone, the owner's decision of 2026-11-04): a new open of a file a waiting entry waits for is granted and raises a [`Delayed`] event; P2, the prefix sync: a sync delivers the parent's commits in order up to the first whose delivery would overwrite a file open for writing in the fork, and waits for the rest (`prefix`) |
//! | 11. Deletes and renames | a file's name line: [`World::rename`] writes a new version, [`World::unlink`] a deleted one; `resolve` applies the per-line rule to it and couples a file's existence with its content. [`World::create`] makes a file (an unborn identity); an open holds an object ([`Handle::object`]), so a close saves to the file its object is named by ([`World::named`]) or, for an orphan, nothing ([`Orphan`]); a rename-over from a new file is an atomic save, a rewrite of the replaced file |
//! | 12. The journal (git-model.md §3) | an open's calls are its journal; [`World::crash`] saves each open from it once (the `Agentfs-Open` trailer, [`World::saved`]), drops calls that never returned and puts takes back in their queues |
//!
//! No clock: commits carry a fixed date (`git.rs`) and order comes only
//! from the queues and from git history.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::content::{Content, Layout, Line, Lineage, PATHS, State, UNBORN, split_lines};
use crate::git::{git, git_in, try_git};
use crate::program;
pub use crate::program::LineEdit;

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
    /// main's own commits. A fork's plain saves sitting directly behind
    /// each other in the parent's queue are taken together, each judged
    /// whole on its own (rule 3e), and those that won land as one
    /// changeset; a transaction's commit (or a nested landing) lands on its
    /// own. The saves that won are recorded beside history, as a graft
    /// (`git replace --graft <commit> <parent> <save>...` under
    /// `refs/replace/`), which git's ancestry and blame read and
    /// `--no-replace-objects` (main's history as stored and pushed) does
    /// not. Without it a sync cannot tell that the parent's queue has
    /// taken a fork's line (coherence finding 2, resolved). A save that
    /// lost is in no graft: its collision ref keeps it.
    Squashed,
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
    pub merge: MergeRule,
    pub layout: Layout,
}

impl Policies {
    /// The model's choices under `history`, slots on separate hunks.
    pub fn new(history: HistoryPolicy) -> Self {
        Self {
            history,
            merge: MergeRule::PerLine,
            layout: Layout::Separated,
        }
    }
}

impl Default for Policies {
    fn default() -> Self {
        Self::new(HistoryPolicy::FullLineage)
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

/// Why a version lost (rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// A same-line collision: the version and the parent's were each
    /// written without seeing the other.
    Line,
    /// A file rule (rule 11): an unlink the parent's unseen line defeated,
    /// an edit of a file the parent unlinked, half of a rename-over, a
    /// path clash. Its lost and won versions may be of different lines.
    File,
    /// A line that did not collide itself, of a change that lost whole
    /// (rule 3e: a save, or a transaction, lands whole or loses whole): its
    /// `won` is the parent's version that decided the change's loss, named
    /// by a `Line` or `File` collision of the same change.
    Whole,
}

/// Rule 3's event: a change into a parent lost, whole (rule 3e). One
/// record per file and cause ([`By`]); every record of one change names
/// the same `loser` and `result`.
#[derive(Clone, Debug)]
pub struct Collision {
    /// The tree that decided it: the parent the incoming change landed in.
    pub tree: Tree,
    pub direction: Direction,
    /// The file (its identity) the lost versions are of.
    pub path: String,
    /// The line identities the incoming change lost (a file's name line
    /// is the file's identity).
    pub lines: Vec<String>,
    /// The incoming versions of those lines, which lost.
    pub lost: Vec<String>,
    /// The tree's versions that won (it kept them), one for each lost: of
    /// the same line, or, per [`By`], the line that decided the loss.
    pub won: Vec<String>,
    pub by: By,
    /// The tree's head before the take: the parent's version, which won.
    pub winner: String,
    /// The change that lost, whole: a save, a transaction's commit or (a
    /// nested sandbox's) landing commit, or the transaction's branch at
    /// its commit. Under `Squashed`, one save of the take, never the
    /// squash (rule 7). Kept reachable by `reference`.
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
/// [`ModelError::OpenFiles`]). It lands in the fork as a child's change
/// does in its parent, whole or not at all (rule 3e): the fork's own
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
    /// Commit while the program holds open for writing a file of the
    /// transaction, or one of the fork the commit would write (a choice).
    OpenFiles,
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
    /// Replication pushes to the shared remote: due once main moved or a
    /// collision was recorded since its last push (the events it waits
    /// for), taken in any order with the queue steps, so the push is
    /// eventual and never part of a referee's step.
    Replicate,
}

/// One line version a commit set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The file's identity.
    pub path: String,
    /// The line's identity.
    pub line: String,
    /// The version.
    pub token: String,
    /// The version is a delete (of a line, or an unlink).
    pub gone: bool,
}

/// Rule 8: one write call on an open file, as POSIX gives it: bytes, never
/// lines. Every byte is ASCII in the model, so an offset is a byte offset
/// and a char boundary at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// `write` on an `O_APPEND` open: the kernel seeks to the end and writes
    /// in one step, so these bytes go after the file's last byte as it is
    /// at the save, another writer's concurrent append included. The save
    /// knows it (rule 8).
    Append(String),
    /// `pwrite(fd, bytes, offset)`, or `lseek` and `write`: these bytes
    /// from `offset` on, overwriting what is there and extending the file
    /// past its end; a gap past the end reads as NUL bytes (a hole).
    Pwrite { offset: usize, bytes: String },
    /// `ftruncate(fd, len)`, and an `O_TRUNC` open (`Truncate(0)` at open):
    /// the file cut to `len` bytes, or extended to it with NUL bytes.
    Truncate(usize),
}

impl Write {
    /// The calls applied to `bytes`, in order: what the file holds after
    /// them. The model's own account (the harness keeps another).
    pub fn apply_all(bytes: &str, calls: &[Write]) -> String {
        let mut b = bytes.as_bytes().to_vec();
        for c in calls {
            c.apply(&mut b);
        }
        String::from_utf8(b).expect("ASCII bytes")
    }

    fn apply(&self, b: &mut Vec<u8>) {
        match self {
            Write::Append(x) => b.extend(x.bytes()),
            Write::Pwrite { offset, bytes } => {
                let end = offset + bytes.len();
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[*offset..end].copy_from_slice(bytes.as_bytes());
            }
            Write::Truncate(n) => b.resize(*n, 0),
        }
    }
}

/// One line of a save's replay: a head line kept, or a new version (of a
/// head line when `line` is some, else a new line), whose id is given at
/// close.
#[derive(Clone, Debug)]
enum Replayed {
    Kept(Line),
    New {
        line: Option<String>,
        over: Option<String>,
        state: State,
    },
}

impl Replayed {
    /// This line written again: a new version holding `state`.
    fn version(self, state: State) -> Self {
        match self {
            Replayed::Kept(l) => Replayed::New {
                line: Some(l.line),
                over: Some(l.id),
                state,
            },
            Replayed::New { line, over, .. } => Replayed::New { line, over, state },
        }
    }

    fn live(&self) -> bool {
        match self {
            Replayed::Kept(l) => l.live(),
            Replayed::New { state, .. } => matches!(state, State::Live(_)),
        }
    }

    fn text(&self) -> String {
        match self {
            Replayed::Kept(Line {
                state: State::Live(t),
                ..
            })
            | Replayed::New {
                state: State::Live(t),
                ..
            } => t.clone(),
            _ => String::new(),
        }
    }

    /// This line deleted: a deleted version of a head line; nothing for a
    /// line new in this save.
    fn deleted(self) -> Option<Self> {
        match self {
            Replayed::New { line: None, .. } => None,
            r => Some(r.version(State::Gone)),
        }
    }
}

/// What a handle is: an open file, or a path call (rule 11) waiting while
/// a take holds its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Write,
    Unlink,
    /// To that path; `over`: the file there when called (rename-over).
    Rename {
        to: String,
        over: Option<String>,
    },
}

/// Where a write-open writes: its tree, or its client's open transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Tree(Tree),
    Txn(ForkId, TxnId),
}

/// A file a client has open for writing (rule 8), or waits to (rule 9),
/// or a path call waiting (rule 11). Its write calls stay private to it
/// until close (close-to-open: the owner's decision, git-model.md §3).
#[derive(Clone, Debug)]
pub struct Handle {
    pub id: HandleId,
    pub client: Client,
    pub target: Target,
    /// The file's identity: an open follows its file through a rename.
    /// After an atomic save over the file it was opened on, the file its
    /// object is now named by ([`World::named`] says which, at close).
    pub file: String,
    /// The object it holds (the inode, on Linux; git-model.md §7): fixed
    /// at open, never changed. Its close saves to the file whose object
    /// this is in its target then; if no path names it, nothing.
    pub object: u64,
    /// The path it was opened by.
    pub path: String,
    /// The open waits for a take's hold on the file to be released: only
    /// while a take is writing it, never behind a waiting take (the owner,
    /// 2026-11-04: P3 deleted).
    pub waiting: bool,
    pub kind: Kind,
    /// Its write calls, in order, applied to the head at close.
    calls: Vec<Write>,
    /// Version ids its [`World::edit`]s reserved, each the text of the line
    /// it wrote: a new version holding exactly that text takes the id, so a
    /// test can name it. A label only: which line is new or changed comes
    /// from the bytes alone.
    reserved: Vec<String>,
}

impl Handle {
    /// The files it locks or waits for: its file, and a rename's target.
    pub fn files(&self) -> Vec<String> {
        let mut v = vec![self.file.clone()];
        if let Kind::Rename { over: Some(g), .. } = &self.kind {
            v.push(g.clone());
        }
        v
    }
}

/// An `OrphanDropped` event (git-model.md §7, a choice): an open whose
/// object no path named at its close (its file unlinked, or replaced by a
/// rename-over or an atomic save, in its target). Its write calls were
/// recorded as made (the journal, git-model.md §3); they are dropped, never
/// committed, and this event names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    pub handle: HandleId,
    pub writer: String,
    /// The path it was opened by.
    pub path: String,
    /// The bytes its write calls carried.
    pub bytes: usize,
}

/// What a tree's take holds its files for (rule 9).
#[derive(Clone, Debug)]
pub enum Take {
    /// The parent's head when the take began, the tree it plans, whether
    /// a commit of the fork was then on its way to the parent, and the
    /// collisions recorded then: the notes ref's commit (none: no
    /// collision yet).
    Sync {
        up: String,
        tree: Option<String>,
        landing: bool,
        known: Option<String>,
        /// The parent's head when the take began: `up` is it, or under P2
        /// the last of its first parents the take may deliver.
        head: String,
        /// The parent's commits after `up` it holds back: their collision
        /// notes are not delivered either.
        held_back: BTreeSet<String>,
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
    /// The numbers of the queue entries it took.
    pub seqs: Vec<u64>,
    /// The tree's head its plan was made on.
    ours: String,
    /// `entered.len()` when it began: what the parent had decided then.
    entered: usize,
}

/// Progress rule P1: a queue entry that waits for write-opens of its files
/// to close (rule 9). Entries behind it go ahead unless they share a file
/// or a source with it (git-model.md §6). No call waits behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The files its take locks (a sync: every file it may write).
    pub files: BTreeSet<String>,
    /// The files whose writers it waits for: it is woken once none is open
    /// for writing. A landing's are all its `files`; a sync's, the files
    /// the first commit it holds back adds to its write (P2, the prefix
    /// sync).
    pub until: BTreeSet<String>,
}

/// The alarm on a waiting take (a choice; the owner, 2026-11-04: a landing
/// "indefinitely delayed while an application is keeping the fs busy with
/// real work" is low risk): a write-open granted on a file a waiting entry
/// waits for, which keeps it waiting. One event per such open and entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delayed {
    pub tree: Tree,
    /// The queue entry it keeps waiting (its number).
    pub entry: u64,
    pub handle: HandleId,
    pub file: String,
}

/// Where a queue entry comes from: its order is kept per source (P1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A sync: the tree's parent.
    Parent,
    /// A landing of a child fork's commit.
    Child(ForkId),
}

impl Entry {
    pub fn source(&self) -> Source {
        match self {
            Entry::Landing { from, .. } => Source::Child(*from),
            Entry::Sync => Source::Parent,
        }
    }
}

/// What a prefix sync (P2) delivers now: the parent's commit it goes up
/// to, the tree it makes (none: the fork has it) and the commits it holds
/// back; and the open files whose close it waits for to deliver the rest.
struct Prefix {
    deliver: Option<(String, Option<String>, BTreeSet<String>)>,
    until: Option<BTreeSet<String>>,
}

/// One take of changes into a tree (a landing, or a transaction's commit
/// into its fork), as `check` reads it: each change with the base its own
/// edit is read against, and which of them won.
#[derive(Clone, Debug)]
pub struct Landed {
    pub tree: Tree,
    pub direction: Direction,
    /// The tree's head before the take, and its commit after.
    pub before: String,
    pub result: String,
    /// (change, base): a commit and its first parent, or a transaction's
    /// branch head and the commit it began on.
    pub units: Vec<(String, String)>,
    pub winners: Vec<String>,
}

/// One take's result: the tree after every change that won, and the
/// collisions of every change that lost (rule 3e).
#[derive(Clone, Debug)]
pub struct Merged {
    tree: String,
    /// The files the result changes.
    files: BTreeSet<String>,
    collisions: Vec<Lost>,
    /// The changes the take judged, in order, each whole: a save, a
    /// transaction's commit or a landing commit (rule 7: a squash judges
    /// each save on its own, never the changeset).
    units: Vec<String>,
    /// Those that won (every one without a collision).
    winners: Vec<String>,
}

/// One file's incoming versions of one change lost in a landing, for one
/// cause: their line identities, the versions, and the tree's versions
/// that won, one for each.
#[derive(Clone, Debug)]
struct Lost {
    path: String,
    lines: Vec<String>,
    lost: Vec<String>,
    won: Vec<String>,
    by: By,
    /// The change that lost.
    loser: String,
}

/// One line version a landing carries.
#[derive(Clone, Debug)]
struct Incoming {
    path: String,
    version: Line,
    /// The commit that wrote it (`git blame`).
    origin: String,
}

impl Incoming {
    fn edit(&self) -> Edit {
        Edit {
            path: self.path.clone(),
            line: self.version.line.clone(),
            token: self.version.id.clone(),
            gone: !self.version.live(),
        }
    }
}

/// The referee's verdict on one incoming version (rule 3).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    Apply,
    /// The same version, a delete the parent's delete agrees with, or a
    /// stale one.
    Drop,
    /// The parent's version (or the line that decided a whole loss) wins.
    Lose(String),
}

static WORLDS: AtomicU32 = AtomicU32::new(0);

/// The notes ref holding each take's collision records, on its result
/// commit (git-model.md §4).
pub const NOTES: &str = "refs/notes/agentfs/collisions";

/// One collision record as the notes ref holds it: the deciding tree's
/// branch name, the file, the cause, the losing change and its ref, the
/// lines, and the lost and winning versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Noted {
    pub tree: String,
    pub path: String,
    pub by: String,
    pub loser: String,
    pub reference: String,
    pub lines: Vec<String>,
    pub lost: Vec<String>,
    pub won: Vec<String>,
}

impl Noted {
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
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
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
        let f: Vec<&str> = s.split('\t').collect();
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

/// Every record of the notes ref at commit `at` in `repo`, with the commit
/// each note is on: read from git alone (a notes tree names each note by
/// the commit it annotates, perhaps fanned out into directories).
pub fn read_notes_on(repo: &Path, at: &str) -> Vec<(String, Noted)> {
    let mut out = Vec::new();
    for line in git(repo, &["ls-tree", "-r", at]).lines() {
        let Some((meta, name)) = line.split_once('\t') else {
            continue;
        };
        let blob = meta.split(' ').nth(2).expect("an object");
        let on = name.replace('/', "");
        for rec in git(repo, &["cat-file", "-p", blob]).lines() {
            out.push((
                on.clone(),
                Noted::parse(rec).expect("a collision record in the notes ref"),
            ));
        }
    }
    out
}

fn read_notes(repo: &Path, at: &str) -> Vec<Noted> {
    read_notes_on(repo, at)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

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
    /// Each tree's queue: its entries, each numbered when queued.
    queues: BTreeMap<Tree, VecDeque<(u64, Entry)>>,
    next_seq: u64,
    handles: BTreeMap<HandleId, Handle>,
    next_handle: HandleId,
    /// Each file's object in each target (the inode an open holds,
    /// git-model.md §7); none once its file is unlinked or replaced there.
    /// Not in git: a view's state, as an inode table is.
    inodes: BTreeMap<(Target, String), u64>,
    next_object: u64,
    /// How many of [`UNBORN`] creates have taken.
    next_unborn: usize,
    /// Every `OrphanDropped` event.
    pub orphans: Vec<Orphan>,
    holds: BTreeMap<Tree, Hold>,
    /// Each tree's queue entries that wait for write-opens of their files
    /// to close (P1: several may wait, each for its own files).
    waits: BTreeMap<Tree, BTreeMap<u64, Waiting>>,
    /// Every alarm on a waiting take ([`Delayed`]).
    pub delayed: Vec<Delayed>,
    /// The files each landing commit's own changes touch, cached.
    touches: RefCell<BTreeMap<String, BTreeSet<String>>>,
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
    /// Every take of changes into a tree, a transaction's commit included.
    pub landings: Vec<Landed>,
    /// Landings that were fast-forwards.
    pub fast_forwards: usize,
    /// Landings whose one commit carried more than one save that won (a
    /// squashed changeset).
    pub squashed_runs: usize,
    /// Tokens of rolled-back transactions.
    pub discarded: BTreeSet<String>,
    /// Each save's write calls, as the program made them, by the save's
    /// commit: `check` reads rule 8's `O_APPEND` from them and the blobs,
    /// not from the replay.
    pub save_calls: BTreeMap<String, Vec<Write>>,
    /// Main's clients that ended (rule 9: their end event released their
    /// locks).
    pub ended_main: BTreeSet<String>,
    pub ended: Vec<Ended>,
    pub log: Vec<String>,
    objects: RefCell<BTreeMap<String, Content>>,
    /// Each version's writing commit, and each commit's parents (grafts
    /// applied).
    origins: RefCell<BTreeMap<String, String>>,
    parents: RefCell<BTreeMap<String, Vec<String>>>,
    ancestry: RefCell<BTreeMap<(String, String), bool>>,
    over: RefCell<BTreeMap<String, Vec<String>>>,
    notes: RefCell<BTreeMap<String, Vec<Noted>>>,
    notes_on: RefCell<BTreeMap<String, Vec<(String, Noted)>>>,
    /// `check`'s state: its last view of each tree and caches over
    /// immutable git objects.
    pub(crate) views: BTreeMap<Tree, BTreeMap<String, Content>>,
    /// Every (tree, file, line identity, version) a tree has shown and
    /// then replaced.
    pub(crate) left: BTreeSet<(Tree, String, String, String)>,
    /// The head the last take's write left, before the opens and path
    /// calls its release granted ran.
    pub(crate) taken: Option<String>,
    pub(crate) checked_commits: BTreeSet<String>,
    /// Each open a save names (`Agentfs-Open`), with that save: `check`'s.
    pub(crate) opens_saved: BTreeMap<String, String>,
    /// Each rename-over's unlink, read from its save's commit: the replaced
    /// file's deleted name version, to the renamed file's identity and the
    /// name version that same save gave it (`check`: one call, whole).
    pub(crate) rename_overs: BTreeMap<String, (String, String)>,
    /// Per head: the commits its stored history reaches, and those its
    /// lineage (history plus grafts) reaches.
    pub(crate) reach: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>,
    /// Main's stored history, checked for `Squashed`.
    pub(crate) main_history: BTreeSet<String>,
    pub(crate) grepped: (usize, BTreeSet<String>),
    pub(crate) collisions_checked: usize,
    pub(crate) landings_checked: usize,
    /// Main moved, or a collision was recorded, since Replication last
    /// pushed.
    replication_due: bool,
    pub(crate) syncs_checked: usize,
    pub(crate) ended_checked: usize,
    pub(crate) ref_moves: u64,
    pub(crate) refs_checked: Option<u64>,
    /// Counted by `check` (rules 10 and 11 met in a take, so a run can show
    /// it exercised them): a new line placed beside a parent's concurrent
    /// new line at its place, and a change that edited a file the parent
    /// had unlinked.
    pub inserts_met: usize,
    /// Syncs that delivered only a prefix of the parent's commits (P2),
    /// counted so a run can show it met them.
    pub prefix_syncs: usize,
    pub unlinked_edits: usize,
    pub(crate) placements_checked: usize,
}

/// The trailer naming the open a save came from (git-model.md §3).
pub const OPEN: &str = "Agentfs-Open";

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
            next_seq: 0,
            handles: BTreeMap::new(),
            next_handle: 1,
            inodes: BTreeMap::new(),
            next_object: 0,
            next_unborn: 0,
            orphans: Vec::new(),
            holds: BTreeMap::new(),
            waits: BTreeMap::new(),
            delayed: Vec::new(),
            touches: RefCell::new(BTreeMap::new()),
            lineage: Lineage::default(),
            counters: BTreeMap::new(),
            collisions: Vec::new(),
            entered: Vec::new(),
            syncs: Vec::new(),
            landings: Vec::new(),
            fast_forwards: 0,
            squashed_runs: 0,
            discarded: BTreeSet::new(),
            save_calls: BTreeMap::new(),
            ended_main: BTreeSet::new(),
            ended: Vec::new(),
            log: Vec::new(),
            objects: RefCell::new(BTreeMap::new()),
            origins: RefCell::new(BTreeMap::new()),
            parents: RefCell::new(BTreeMap::new()),
            ancestry: RefCell::new(BTreeMap::new()),
            over: RefCell::new(BTreeMap::new()),
            notes_on: RefCell::new(BTreeMap::new()),
            notes: RefCell::new(BTreeMap::new()),
            views: BTreeMap::new(),
            left: BTreeSet::new(),
            taken: None,
            checked_commits: BTreeSet::new(),
            opens_saved: BTreeMap::new(),
            rename_overs: BTreeMap::new(),
            reach: BTreeMap::new(),
            grepped: (0, BTreeSet::new()),
            collisions_checked: 0,
            landings_checked: 0,
            replication_due: false,
            syncs_checked: 0,
            ended_checked: 0,
            main_history: BTreeSet::new(),
            ref_moves: 0,
            refs_checked: None,
            inserts_met: 0,
            prefix_syncs: 0,
            unlinked_edits: 0,
            placements_checked: 0,
        };
        let files = PATHS
            .iter()
            .map(|p| (p.to_string(), Content::root(p)))
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

    /// What tree `t` shows at path `p`: the file named `p` there.
    pub fn read(&self, t: Tree, p: &str) -> Content {
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
        PATHS
            .iter()
            .find(|f| self.file(at, f).path() == Some(p))
            .map(|f| f.to_string())
    }

    /// The path file `f` has in tree `t`; none if unlinked there.
    pub fn path_of(&self, t: Tree, f: &str) -> Option<String> {
        self.file(&self.head(t), f).path().map(str::to_string)
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
            .map(|q| q.iter().map(|(_, e)| e.clone()).collect())
            .unwrap_or_default()
    }

    /// The queue with each entry's number.
    pub fn queue_numbered(&self, t: Tree) -> Vec<(u64, Entry)> {
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

    /// The head the last take's write left, before the opens and path
    /// calls its release granted ran.
    pub fn taken_head(&self) -> Option<&str> {
        self.taken.as_deref()
    }

    /// Each tree whose queue head waits, with the files it waits for.
    pub fn waits(&self) -> &BTreeMap<Tree, BTreeMap<u64, Waiting>> {
        &self.waits
    }

    /// The files tree `t`'s waiting entries wait for: those of their files
    /// open for writing there.
    pub fn waiting_for(&self, t: Tree) -> BTreeSet<String> {
        let open = self.write_open(t);
        self.waits
            .get(&t)
            .into_iter()
            .flat_map(|m| m.values())
            .flat_map(|x| x.until.iter())
            .filter(|p| open.contains(*p))
            .cloned()
            .collect()
    }

    /// The files a client of `t` has open for writing (granted opens).
    /// An open whose object no path names (an orphan) has no file open.
    pub fn write_open(&self, t: Tree) -> BTreeSet<String> {
        self.handles
            .values()
            .filter(|h| !h.waiting && h.target == Target::Tree(t))
            .filter_map(|h| self.named(h))
            .collect()
    }

    /// The file open `h`'s object is in its target now: the live file
    /// whose object it is; none if no path names it (an orphan: its file
    /// was unlinked, or replaced by a rename-over or an atomic save, there).
    pub fn named(&self, h: &Handle) -> Option<String> {
        let at = self.target_head(h.target);
        self.inodes
            .iter()
            .find(|((t, f), o)| *t == h.target && **o == h.object && self.file(&at, f).name.live())
            .map(|((_, f), _)| f.clone())
    }

    /// File `f`'s object in target `t`, a new one if it has none.
    fn inode(&mut self, t: Target, f: &str) -> u64 {
        if let Some(o) = self.inodes.get(&(t, f.to_string())) {
            return *o;
        }
        self.next_object += 1;
        self.inodes.insert((t, f.to_string()), self.next_object);
        self.next_object
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
            Tree::Main => {
                // Replication pushes main (and the graft made with it) once
                // it moved: the event its push waits for.
                self.replication_due = true;
                self.main_head = c.to_string();
            }
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
            let v = commit_parents(&self.repo, c, false);
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
    /// on, read from git (the records, first parents and `git blame`). Per
    /// line, not per commit: a sync merge that kept the fork's version over
    /// the parent's does not make the parent's version one the fork's next
    /// edit was written over. Cached: versions never change.
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

    /// Whether `c` is a plain save (not a transaction's commit, nor a
    /// landing of a fork's own fork).
    fn plain_save(&self, c: &str) -> bool {
        self.trailer(c, "Agentfs-Cause") == "Direct"
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

    /// Rules 3, 10 and 11 at the parent's queue. Each incoming version
    /// meets the version `ours` (the parent's head) holds of the same line
    /// identity ([`World::verdict`]). Then a file's existence couples with
    /// its content (rule 11): an edit of a file the parent unlinked loses,
    /// all of it; an unlink loses whole if the parent has a live line its
    /// writer did not see ([`World::seen`]); a rename onto a path another
    /// file holds in the result loses.
    /// Then the change is judged whole (rule 3e): if any version lost, none
    /// applies. The applied versions are placed by [`splice`], in the order
    /// `tip` (the incoming change) has them.
    fn resolve(&self, ours: &str, incoming: &[Incoming], tip: &str) -> Merged {
        let mut by_path: BTreeMap<&str, Vec<&Incoming>> = BTreeMap::new();
        for i in incoming {
            by_path.entry(&i.path).or_default().push(i);
        }
        let key = |i: &Incoming| (i.path.clone(), i.version.line.clone());
        let mut verdicts: BTreeMap<(String, String), Verdict> = BTreeMap::new();
        let mut by_file: BTreeSet<String> = BTreeSet::new();
        for (p, ins) in &by_path {
            let cur = self.file(ours, p);
            for i in ins {
                verdicts.insert(key(i), self.verdict(ours, &cur, i));
            }
            let unlink = ins
                .iter()
                .find(|i| i.version.line == cur.name.line && !i.version.live());
            // A create: a live name version written over the parent's
            // deleted one, which only an unborn file's create can be (a
            // program cannot rename a file no path names).
            let creates = ins.iter().any(|i| {
                i.version.line == cur.name.line
                    && i.version.live()
                    && verdicts[&key(i)] == Verdict::Apply
            });
            if !cur.name.live() && unlink.is_none() && !creates {
                // An edit of a file the parent unlinked: the unlink wins.
                for i in ins {
                    if verdicts[&key(i)] != Verdict::Drop {
                        verdicts.insert(key(i), Verdict::Lose(cur.name.id.clone()));
                        by_file.insert(p.to_string());
                    }
                }
            } else if let Some(u) = unlink
                && verdicts[&key(u)] == Verdict::Apply
            {
                // An unlink loses whole to a line its writer did not see.
                let t = self.file(tip, p);
                if let Some(q) = cur.live().find(|q| !self.seen(tip, &t, p, q)) {
                    for i in ins {
                        verdicts.insert(key(i), Verdict::Lose(q.id.clone()));
                    }
                    by_file.insert(p.to_string());
                }
            }
        }
        // A rename-over (one call: both name lines, in one save) needs no
        // rule of its own: if either half loses, the save loses whole
        // (rule 3e, below). So the path its unlink half would have freed is
        // no claim against its rename half (finding 25): a file this change
        // itself replaces keeps its name only because the change lost, and
        // the rename loses with it (`By::Whole`), not to that name.
        let replaced: BTreeSet<&str> = incoming
            .iter()
            .filter(|i| i.version.line == i.path && matches!(i.version.state, State::Replaced(_)))
            .map(|i| i.path.as_str())
            .collect();
        loop {
            let mut changed = false;
            // Two files claiming one path: the parent's keeps it.
            let names: Vec<(String, Line)> = PATHS
                .iter()
                .map(|f| {
                    let k = (f.to_string(), f.to_string());
                    let name = match verdicts.get(&k) {
                        Some(Verdict::Apply) => incoming
                            .iter()
                            .find(|i| key(i) == k)
                            .map(|i| i.version.clone()),
                        _ => None,
                    };
                    (
                        f.to_string(),
                        name.unwrap_or_else(|| self.file(ours, f).name),
                    )
                })
                .collect();
            for (f, n) in &names {
                let k = (f.clone(), f.clone());
                if verdicts.get(&k) != Some(&Verdict::Apply) || n.text().is_none() {
                    continue;
                }
                if let Some((_, g)) = names
                    .iter()
                    .find(|(g, m)| g != f && m.text() == n.text() && !replaced.contains(g.as_str()))
                {
                    verdicts.insert(k, Verdict::Lose(g.id.clone()));
                    by_file.insert(f.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // Rule 3e: the change lands whole or loses whole. Any lost version
        // loses the change: its other versions lose too, named for the
        // first loss's winner, and nothing of it applies.
        let decider = by_path
            .values()
            .flatten()
            .find_map(|i| match &verdicts[&key(i)] {
                Verdict::Lose(won) => Some(won.clone()),
                _ => None,
            });
        let mut files = BTreeMap::new();
        let mut collisions = Vec::new();
        for (p, ins) in &by_path {
            let cur = self.file(ours, p);
            let lost = |by| Lost {
                path: p.to_string(),
                lines: Vec::new(),
                lost: Vec::new(),
                won: Vec::new(),
                by,
                loser: tip.to_string(),
            };
            let mut real = lost(if by_file.contains(*p) {
                By::File
            } else {
                By::Line
            });
            let mut whole = lost(By::Whole);
            let mut applied = Vec::new();
            for i in ins {
                let (into, won) = match (&verdicts[&key(i)], &decider) {
                    (Verdict::Drop, _) => continue,
                    (Verdict::Apply, None) => {
                        applied.push(i.version.clone());
                        continue;
                    }
                    (Verdict::Apply, Some(d)) => (&mut whole, d.clone()),
                    (Verdict::Lose(won), _) => (&mut real, won.clone()),
                };
                into.lines.push(i.version.line.clone());
                into.lost.push(i.version.id.clone());
                into.won.push(won);
            }
            let new = splice(&cur, &applied, &self.file(tip, p));
            if new != cur {
                files.insert(p.to_string(), new);
            }
            collisions.extend([real, whole].into_iter().filter(|l| !l.lost.is_empty()));
        }
        let tree = if files.is_empty() {
            self.tree_of(ours)
        } else {
            self.tree_with(Some(ours), &files)
        };
        let won = collisions.is_empty();
        Merged {
            tree,
            files: files.into_keys().collect(),
            collisions,
            units: vec![tip.to_string()],
            winners: if won {
                vec![tip.to_string()]
            } else {
                Vec::new()
            },
        }
    }

    /// Rule 3, per line identity: an incoming version against the version
    /// `cur` (the parent's, at `ours`) holds of the same line. None there
    /// (a line new to the parent): it applies. The same version, or two
    /// deletes: nothing. Written over the parent's: it replaces it. The
    /// parent's written over it: stale, dropped. Else the two are
    /// concurrent, and the parent's wins.
    fn verdict(&self, ours: &str, cur: &Content, i: &Incoming) -> Verdict {
        let Some(p) = cur.member(&i.version.line) else {
            return Verdict::Apply;
        };
        if p.id == i.version.id || (!p.live() && !i.version.live()) {
            Verdict::Drop
        } else if self
            .written_over(&i.origin, &i.path, &i.version)
            .contains(&p.id)
        {
            Verdict::Apply
        } else if self
            .written_over(&self.origin_of(ours, &i.path, &p.id), &i.path, p)
            .contains(&i.version.id)
        {
            Verdict::Drop
        } else {
            Verdict::Lose(p.id.clone())
        }
    }

    /// Whether the writer of commit `tip` (its file `p` is `t`) saw the
    /// parent's version `q`: `tip` holds it, or a version written over it.
    fn seen(&self, tip: &str, t: &Content, p: &str, q: &Line) -> bool {
        t.member(&q.line).is_some_and(|v| {
            v.id == q.id
                || self
                    .written_over(&self.origin_of(tip, p, &v.id), p, v)
                    .contains(&q.id)
        })
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

    /// Whether `parent`'s queue had decided fork commit `c` when a sync
    /// read the parent's head `up`: `up`'s lineage reaches it (it won
    /// there, or under `FullLineage` lost), or a collision recorded then
    /// (read from git: the notes ref as it was, `known`), decided in
    /// `parent` or a tree upstream of it, names a change that reaches it.
    /// Under `Squashed` a save that lost is in no graft (rule 7), so its
    /// collision record is the only record of that decision. The fork's
    /// branch is in queue order, so a commit a decided change reaches was
    /// queued before it and is decided too.
    /// A prefix sync (P2) delivers no decision of a commit it holds back
    /// (`held_back`): a note on one of them is not read, nor a note from a
    /// tree further up whose losing change only a held-back commit reaches
    /// (the parent's commit that carried it upstream is held back, so that
    /// decision is not delivered yet: causal order, a decision never
    /// arrives before the change it decided).
    fn decided(
        &self,
        parent: Tree,
        c: &str,
        up: &str,
        known: Option<&str>,
        held_back: &BTreeSet<String>,
    ) -> bool {
        let trees: Vec<String> = self.upstream(parent).into_iter().map(Self::name).collect();
        self.is_ancestor(c, up)
            || self.noted_on(known).iter().any(|(on, x)| {
                !held_back.contains(on)
                    && trees.contains(&x.tree)
                    && self.is_ancestor(c, &x.loser)
                    && (self.is_ancestor(&x.loser, up)
                        || !held_back.iter().any(|h| self.is_ancestor(&x.loser, h)))
            })
    }

    /// [`World::noted`], with the commit each record is on.
    fn noted_on(&self, at: Option<&str>) -> Vec<(String, Noted)> {
        let Some(at) = at else {
            return Vec::new();
        };
        if let Some(v) = self.notes_on.borrow().get(at) {
            return v.clone();
        }
        let v = read_notes_on(&self.repo, at);
        self.notes_on.borrow_mut().insert(at.to_string(), v.clone());
        v
    }

    /// The notes ref's commit now (none: no collision recorded yet).
    pub fn notes_head(&self) -> Option<String> {
        let o = try_git(&self.repo, &["rev-parse", "--verify", "-q", NOTES], None);
        (o.code == 0).then(|| o.stdout.trim().to_string())
    }

    /// Every collision record in the notes ref at commit `at` (none: no
    /// note yet), read from git. Cached: a notes commit never changes.
    pub fn noted(&self, at: Option<&str>) -> Vec<Noted> {
        let Some(at) = at else {
            return Vec::new();
        };
        if let Some(v) = self.notes.borrow().get(at) {
            return v.clone();
        }
        let v = read_notes(&self.repo, at);
        self.notes.borrow_mut().insert(at.to_string(), v.clone());
        v
    }

    /// [`MergeRule::PerLine`]'s sync: the parent's version `up`, with each
    /// version of the fork's `ours` the parent has not decided yet
    /// ([`World::decided`]) kept over the parent's version of that line,
    /// placed in the fork's order ([`splice`]).
    fn adopt(
        &self,
        parent: Tree,
        ours: &str,
        up: &str,
        known: Option<&str>,
        held_back: &BTreeSet<String>,
    ) -> String {
        let mut files = BTreeMap::new();
        for p in self.changed(ours, up) {
            let (mine, theirs) = (self.file(ours, &p), self.file(up, &p));
            let keep: Vec<Line> = mine
                .records()
                .filter(|r| {
                    theirs.member(&r.line).is_none_or(|u| u.id != r.id)
                        && !self.decided(
                            parent,
                            &self.origin_of(ours, &p, &r.id),
                            up,
                            known,
                            held_back,
                        )
                })
                .cloned()
                .collect();
            let new = splice(&theirs, &keep, &mine);
            if new != theirs {
                files.insert(p, new);
            }
        }
        self.keep_own_names(ours, up, &mut files);
        let tree = if files.is_empty() {
            self.tree_of(up)
        } else {
            self.tree_with(Some(up), &files)
        };
        self.no_clash(&tree);
        tree
    }

    /// Finding 11, two renames to one path before the parent decides (a
    /// choice, recommended on 2026-11-04 without objection): a sync keeps
    /// the fork's undecided name; had the parent meanwhile given that path
    /// to another file (a rename, a rename-over, a create), the fork shows
    /// the parent's file under the name it has in the fork (its previous
    /// name; none, so not shown, for a file the fork has never had) until
    /// the parent's queue decides the fork's rename. A chain (the parent
    /// then gave that previous name to a third file) is the same rule
    /// again: each file whose parent's name clashes with a name the sync
    /// shows is shown under its fork's name, until no two clash; the fork's
    /// own tree has no clash, so this ends. `files`: the sync's changed
    /// files, by identity, over `up`.
    fn keep_own_names(&self, ours: &str, up: &str, files: &mut BTreeMap<String, Content>) {
        let now = |files: &BTreeMap<String, Content>, f: &str| {
            files.get(f).cloned().unwrap_or_else(|| self.file(up, f))
        };
        loop {
            let mut at: BTreeMap<String, Vec<&str>> = BTreeMap::new();
            for f in PATHS {
                if let Some(p) = now(files, f).path() {
                    at.entry(p.to_string()).or_default().push(f);
                }
            }
            // A file in a clash showing a name other than its fork's.
            let back = at
                .values()
                .filter(|v| v.len() > 1)
                .flatten()
                .find(|f| now(files, f).name.id != self.file(ours, f).name.id);
            let Some(f) = back.map(|f| f.to_string()) else {
                return;
            };
            let mut c = now(files, &f);
            c.name = self.file(ours, &f).name.clone();
            files.insert(f, c);
        }
    }

    /// Two live files never share a path ([`World::keep_own_names`] keeps
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

    /// Rule 10's alignment: git's histogram diff of the old lines' bytes
    /// against the new (as text, whatever bytes they hold), as (old start, old count,
    /// new start, new count) per changed region, 0-based.
    fn hunks(&self, old: &[String], new: &[String]) -> Vec<(usize, usize, usize, usize)> {
        let blob = |v: &[String]| {
            let text: String = v.concat();
            git_in(&self.repo, &["hash-object", "-w", "--stdin"], &text)
        };
        let (a, b) = (blob(old), blob(new));
        if a == b {
            return Vec::new();
        }
        git(
            &self.repo,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--histogram",
                "--text",
                "-U0",
                &a,
                &b,
            ],
        )
        .lines()
        .filter_map(parse_hunk)
        .collect()
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

    /// Records a take's collisions in tree `t`: one decision per change
    /// that lost (rule 3a), kept reachable under one `refs/collisions/<n>`,
    /// with one record under that ref for each file and cause it lost by.
    fn record(&mut self, t: Tree, dir: Direction, m: &Merged, winner: &str, result: &str) {
        let first = self.collisions.len();
        let mut refs: BTreeMap<&str, String> = BTreeMap::new();
        for Lost {
            path,
            lines,
            lost,
            won,
            by,
            loser,
        } in &m.collisions
        {
            let reference = match refs.get(loser.as_str()) {
                Some(r) => r.clone(),
                None => {
                    let made: BTreeSet<&str> = self
                        .collisions
                        .iter()
                        .map(|c| c.reference.as_str())
                        .collect();
                    let r = format!("refs/collisions/{}", made.len() + 1);
                    self.update_ref(&[&r, loser]);
                    refs.insert(loser, r.clone());
                    r
                }
            };
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
                reference,
            });
            self.replication_due = true;
        }
        if self.collisions.len() > first {
            // The take's decision, beside history: one note on its result
            // commit, a line per record (git-model.md §4).
            let note: String = self.collisions[first..]
                .iter()
                .map(|c| Noted::of(c).render())
                .collect();
            git_in(
                &self.repo,
                &["notes", "--ref", NOTES, "add", "-f", "-F", "-", result],
                &note,
            );
            self.ref_moves += 1;
        }
    }

    /// Replication's push ([`Step::Replicate`]): main, the grafts, the
    /// collision notes and every collision ref to the shared remote in one
    /// atomic push (owner,
    /// 2026-11-03: "yes, \"losing\" patches should still be part of the
    /// history. i want to know who changed what when"). Replication is the
    /// remote's one pusher; no referee step pushes. Never forced: main only
    /// moves forward, and collision and graft refs never move.
    fn replicate(&mut self) {
        self.replication_due = false;
        let remote = self.remote.to_string_lossy().into_owned();
        // `refs/notes/*`, a pattern: the notes ref exists only once a
        // collision has been recorded.
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
                "refs/notes/*:refs/notes/*",
            ],
        );
    }

    /// `t`'s head moved: a fork's new commit queues for its parent (`up`),
    /// and the new content queues for every child (rule 1: both ways).
    fn moved(&mut self, t: Tree, up: bool) {
        let head = self.head(t);
        if up && let Tree::Fork(f) = t {
            let parent = self.forks[&f].parent;
            self.next_seq += 1;
            let n = self.next_seq;
            self.queues.entry(parent).or_default().push_back((
                n,
                Entry::Landing {
                    from: f,
                    commit: head.clone(),
                },
            ));
        }
        for c in self.children(t) {
            self.next_seq += 1;
            let n = self.next_seq;
            self.queues
                .entry(Tree::Fork(c))
                .or_default()
                .push_back((n, Entry::Sync));
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

    /// Rule 1: open `p` for writing, write line `slot` (a program's byte
    /// calls, [`World::edit`]), close: the version written. The open must not
    /// wait (no take holds `p`); see [`World::open_write`] for one that may.
    pub fn save(&mut self, c: &Client, p: &str, slot: usize) -> String {
        let h = self.open_write(c, p);
        assert!(
            !self.handles[&h].waiting,
            "save: a take holds {p}; open_write waits for it"
        );
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
        }
    }

    /// Where a client's calls go: its open transaction, else its tree.
    fn target_of(&self, c: &Client) -> Target {
        match c {
            Client::Main(_) => Target::Tree(Tree::Main),
            Client::Fork(f) => match &self.forks[f].txn {
                Some(t) => Target::Txn(*f, t.id),
                None => Target::Tree(Tree::Fork(*f)),
            },
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

    /// A new handle on `file` at `target`, waiting if a take holds one of
    /// its files in that tree (a transaction's branch is frozen: never).
    fn add_handle(
        &mut self,
        c: &Client,
        target: Target,
        file: String,
        path: &str,
        kind: Kind,
    ) -> HandleId {
        assert!(
            !matches!(c, Client::Main(n) if self.ended_main.contains(n)),
            "a call by {c:?}, which ended"
        );
        let id = self.next_handle;
        self.next_handle += 1;
        let object = self.inode(target, &file);
        let mut h = Handle {
            id,
            client: c.clone(),
            target,
            file,
            object,
            path: path.to_string(),
            waiting: false,
            kind,
            calls: Vec::new(),
            reserved: Vec::new(),
        };
        if let Target::Tree(t) = target {
            h.waiting = self
                .holds
                .get(&t)
                .is_some_and(|x| h.files().iter().any(|f| x.files.contains(f)));
            if !h.waiting && h.kind == Kind::Write {
                self.delay(t, &h);
            }
        }
        self.handles.insert(id, h);
        id
    }

    /// The alarm on a long wait (a choice, the owner's decision of
    /// 2026-11-04): an open granted on a file a waiting entry of `t` waits
    /// for keeps that entry waiting, so it raises a [`Delayed`] event per
    /// such entry. Raised by the open, never by a timer; only an open
    /// granted at once can be due one ([`World::grant`] shows why).
    fn delay(&mut self, t: Tree, h: &Handle) {
        let hit: Vec<(u64, String)> = self
            .waits
            .get(&t)
            .into_iter()
            .flatten()
            .filter_map(|(n, x)| {
                h.files()
                    .into_iter()
                    .find(|f| x.until.contains(f))
                    .map(|f| (*n, f))
            })
            .collect();
        for (entry, file) in hit {
            self.note(format!(
                "alarm: h{} keeps {t:?}'s entry {entry} waiting for {file}",
                h.id
            ));
            self.delayed.push(Delayed {
                tree: t,
                entry,
                handle: h.id,
                file,
            });
        }
    }

    /// An `O_TRUNC` open: [`World::open_write`] whose first call truncates the
    /// file to nothing (applied at close, with the rest: close-to-open).
    pub fn open_trunc(&mut self, c: &Client, p: &str) -> HandleId {
        let h = self.open_write(c, p);
        self.handles
            .get_mut(&h)
            .expect("open")
            .calls
            .push(Write::Truncate(0));
        self.note(format!("h{h}: O_TRUNC"));
        h
    }

    /// Rule 9: open `p` for writing, in the client's open transaction if it
    /// has one, else in its tree. Granted at once unless a take holds the
    /// file in that tree: then the open waits, and the take's release
    /// grants it (an event, never a timer). A transaction's branch is
    /// frozen, so an open there never waits. The open names the file by
    /// its identity, so it follows the file through a rename.
    pub fn open_write(&mut self, c: &Client, p: &str) -> HandleId {
        let target = self.target_of(c);
        let file = self
            .lookup(&self.target_head(target), p)
            .unwrap_or_else(|| {
                panic!("open_write of {p}: no such file ([`World::create`] makes one)")
            });
        let id = self.add_handle(c, target, file, p, Kind::Write);
        let waiting = self.handles[&id].waiting;
        self.note(format!(
            "open h{id} {c:?} {target:?}:{p}{}",
            if waiting { ": waits for the hold" } else { "" }
        ));
        id
    }

    /// Rule 8: one byte call on a granted open. The open keeps its calls;
    /// close applies them.
    pub fn write(&mut self, h: HandleId, call: Write) {
        let handle = &self.handles[&h];
        assert!(
            !handle.waiting && handle.kind == Kind::Write,
            "a write needs a granted open"
        );
        let bytes = match &call {
            Write::Append(b) | Write::Pwrite { bytes: b, .. } => b.as_str(),
            Write::Truncate(_) => "",
        };
        assert!(bytes.is_ascii(), "the model's bytes are ASCII");
        self.note(format!("write h{h}: {call:?}"));
        self.handles.get_mut(&h).expect("open").calls.push(call);
    }

    /// What the program reads through open `h`: its file at the open's
    /// target head now, with its own calls applied (close-to-open: no
    /// other writer's unsaved calls).
    pub fn view(&self, h: HandleId) -> String {
        let handle = &self.handles[&h];
        let cur = self.file(&self.target_head(handle.target), &handle.file);
        Write::apply_all(&cur.bytes(), &handle.calls)
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
            let (writer, txn) = self.handle_writer(&self.handles[&h]);
            self.token(&writer, txn)
        });
        let text = token.clone().unwrap_or_default();
        if let Some(t) = &token {
            self.handles
                .get_mut(&h)
                .expect("open")
                .reserved
                .push(t.clone());
        }
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
    /// the head of the open's tree (or transaction) now ([`World::replay`]):
    /// every line the program's bytes leave unchanged keeps its version, a line a sync brought
    /// or another writer's append included. One commit, first parent that
    /// head, at once; none if nothing changed, or if the file was unlinked
    /// meanwhile (as on Linux, its write calls reach no path). Then a take
    /// waiting for this close is woken.
    pub fn close(&mut self, h: HandleId) {
        let handle = self.handles.remove(&h).expect("an open file");
        assert!(
            !handle.waiting && handle.kind == Kind::Write,
            "close a granted open"
        );
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
        let Some(p) = self.named(handle) else {
            self.orphan(handle, &writer);
            return Vec::new();
        };
        let at = self.target_head(handle.target);
        let cur = self.file(&at, &p);
        let mut reserved = handle.reserved.clone();
        let (lines, set) = self.rewrite(&cur, &p, &handle.calls, &writer, txn, &mut reserved);
        if set.is_empty() {
            return Vec::new();
        }
        let tokens = set.iter().map(|e| e.token.clone()).collect();
        let new = Content {
            name: cur.name.clone(),
            lines,
        };
        let files = BTreeMap::from([(p, new)]);
        let c = self.commit_save(handle.target, &writer, &at, files, set, Some(handle.id));
        self.save_calls.insert(c, handle.calls.clone());
        tokens
    }

    /// An open no path names at its close: its calls are dropped, and an
    /// `OrphanDropped` event names them (none if it made no call).
    fn orphan(&mut self, h: &Handle, writer: &str) {
        if h.calls.is_empty() {
            return;
        }
        let bytes = h
            .calls
            .iter()
            .map(|c| match c {
                Write::Append(b) | Write::Pwrite { bytes: b, .. } => b.len(),
                Write::Truncate(_) => 0,
            })
            .sum();
        self.note(format!("close h{}: orphan, {bytes} bytes dropped", h.id));
        self.orphans.push(Orphan {
            handle: h.id,
            writer: writer.to_string(),
            path: h.path.clone(),
            bytes,
        });
    }

    /// File `p`'s lines `cur` with byte calls replayed on them (rules 8 and
    /// 10): the new lines, and the versions they set. A new version holding
    /// exactly the text of a `reserved` id takes that id.
    fn rewrite(
        &mut self,
        cur: &Content,
        p: &str,
        calls: &[Write],
        writer: &str,
        txn: Option<TxnId>,
        reserved: &mut Vec<String>,
    ) -> (Vec<Line>, Vec<Edit>) {
        let mut lines = Vec::new();
        let mut set = Vec::new();
        for r in self.replay(cur, calls) {
            match r {
                Replayed::Kept(l) => lines.push(l),
                Replayed::New { line, over, state } => {
                    let text = match &state {
                        State::Live(t) => t.strip_suffix('\n').unwrap_or(t),
                        _ => "",
                    };
                    let id = match reserved.iter().position(|t| t == text) {
                        Some(i) => reserved.remove(i),
                        None => self.token(writer, txn),
                    };
                    let line = line.unwrap_or_else(|| id.clone());
                    self.lineage.wrote(&id, over.as_deref());
                    set.push(Edit {
                        path: p.to_string(),
                        line: line.clone(),
                        token: id.clone(),
                        gone: !matches!(state, State::Live(_)),
                    });
                    lines.push(Line {
                        id,
                        line,
                        over,
                        state,
                    });
                }
            }
        }
        (lines, set)
    }

    /// Rules 8 and 10: an open's byte calls on `cur` (the head at close),
    /// line identities derived from the bytes. An `O_APPEND` write is exact:
    /// the kernel, and so the save, knows its bytes go after the end. If the
    /// file then ends in a partial last line (no final newline), the
    /// appended bytes up to and including their first newline finish that
    /// line: a new version of it, written over it, not a new line ("foo"
    /// and an append of "bar" is one line, "foobar"). Every further line the
    /// append wrote is a new line after the file's last. Every other call
    /// changes the bytes (`image`), which the save aligns with the lines
    /// once ([`World::align`]), before the next append and at close. So a
    /// program's insert, delete or move comes out as the alignment reads
    /// its bytes, and a move as a delete plus an insert.
    fn replay(&self, cur: &Content, calls: &[Write]) -> Vec<Replayed> {
        let mut seq: Vec<Replayed> = cur.lines.iter().cloned().map(Replayed::Kept).collect();
        let mut image = cur.bytes().into_bytes();
        let mut dirty = false;
        for call in calls {
            let Write::Append(x) = call else {
                call.apply(&mut image);
                dirty = true;
                continue;
            };
            if dirty {
                seq = self.align(seq, &String::from_utf8(image.clone()).expect("ASCII"));
                dirty = false;
            }
            let mut rest = x.as_str();
            if let Some(k) = seq.iter().rposition(Replayed::live)
                && !seq[k].text().ends_with('\n')
                && !rest.is_empty()
            {
                let cut = rest.find('\n').map_or(rest.len(), |i| i + 1);
                let t = format!("{}{}", seq[k].text(), &rest[..cut]);
                seq[k] = seq[k].clone().version(State::Live(t));
                rest = &rest[cut..];
            }
            for l in split_lines(rest) {
                seq.push(Replayed::New {
                    line: None,
                    over: None,
                    state: State::Live(l),
                });
            }
            image.extend(x.bytes());
        }
        if dirty {
            seq = self.align(seq, &String::from_utf8(image).expect("ASCII"));
        }
        seq
    }

    /// Rule 10's one alignment: the live lines of `seq` aligned with the
    /// program's bytes `image` (its lines, [`split_lines`]) by git's
    /// histogram diff ([`World::hunks`]): unchanged lines keep their
    /// versions, a changed region pairs old and new lines in order (each new
    /// one a version of its old one), the extra new ones are new lines and
    /// the extra old ones deleted (lines at a region's ends whose bytes did
    /// not change keep their versions); deleted versions stay where they
    /// were. A line's bytes include its newline, so adding or removing the
    /// final newline changes the last line.
    fn align(&self, seq: Vec<Replayed>, image: &str) -> Vec<Replayed> {
        let texts = split_lines(image);
        let lives: Vec<usize> = (0..seq.len()).filter(|k| seq[*k].live()).collect();
        // Each line as the program read it: a followed partial line reads
        // terminated (`Content::read_lines`).
        let old: Vec<String> = lives
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let t = seq[*k].text();
                if i + 1 < lives.len() && !t.ends_with('\n') {
                    format!("{t}\n")
                } else {
                    t
                }
            })
            .collect();
        let mut instead: Vec<Vec<Replayed>> = lives.iter().map(|k| vec![seq[*k].clone()]).collect();
        let mut before: Vec<Vec<Replayed>> = vec![Vec::new(); old.len() + 1];
        for (os, on, ns, nn) in self.hunks(&old, &texts) {
            // git may put lines whose bytes did not change at either end of
            // a region (among repeated lines): they keep their versions.
            let same = |i: usize, j: usize| old[i] == texts[j];
            let mut head = 0;
            while head < on.min(nn) && same(os + head, ns + head) {
                head += 1;
            }
            let mut tail = 0;
            while tail < (on - head).min(nn - head) && same(os + on - 1 - tail, ns + nn - 1 - tail)
            {
                tail += 1;
            }
            let (os, on, ns, nn) = (os + head, on - head - tail, ns + head, nn - head - tail);
            let n = on.min(nn);
            for j in 0..on {
                let r = seq[lives[os + j]].clone();
                instead[os + j] = if j < n {
                    vec![r.version(State::Live(texts[ns + j].clone()))]
                } else {
                    r.deleted().into_iter().collect()
                };
            }
            for t in &texts[ns + n..ns + nn] {
                before[os + on].push(Replayed::New {
                    line: None,
                    over: None,
                    state: State::Live(t.clone()),
                });
            }
        }
        let mut out = Vec::new();
        let mut k = 0;
        for r in seq {
            if r.live() {
                out.append(&mut before[k]);
                out.append(&mut instead[k]);
                k += 1;
            } else {
                out.push(r);
            }
        }
        out.append(&mut before[k]);
        // A kept line a merge had made read as terminated, now the last:
        // the program's bytes say what it ends with, so it takes them, as a
        // new version, if they differ from what its version holds.
        let last = out.iter().rposition(Replayed::live);
        if let (Some(k), Some(t)) = (last, texts.last())
            && matches!(out[k], Replayed::Kept(_))
            && out[k].text() != *t
        {
            out[k] = out[k].clone().version(State::Live(t.clone()));
        }
        out
    }

    /// A save's commit: on the target tree, first parent its head `at`,
    /// moving its branch and queueing the move both ways (rule 1); or on
    /// the open transaction's branch, staged.
    fn commit_save(
        &mut self,
        target: Target,
        writer: &str,
        at: &str,
        files: BTreeMap<String, Content>,
        set: Vec<Edit>,
        open: Option<HandleId>,
    ) -> String {
        let t = self.tree_with(Some(at), &files);
        match target {
            Target::Txn(f, id) => {
                let on = format!("txn/{id}");
                let commit = self.commit_by(&[at], &t, writer, "Direct", &on, open);
                self.update_ref(&[&format!("refs/heads/{on}"), &commit]);
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
    /// its name line. One call, done at once (none returned) unless a take
    /// holds the file in that tree: then it waits like a write-open, and
    /// the take's release runs it (the returned handle until then). The
    /// client's own opens of the file stay open: as on Linux, their write
    /// calls then reach no path and save nothing.
    pub fn unlink(&mut self, c: &Client, p: &str) -> Option<HandleId> {
        self.path_call(c, p, Kind::Unlink)
    }

    /// Rule 11: rename the file at `from` to `to`: one save writing a new
    /// version of its name line. If a file is named `to`, it is a
    /// rename-over: the same save unlinks that file (its name line deleted,
    /// naming the file that replaced it). Waits like [`World::unlink`]
    /// while a take holds either file. The file keeps its identity, so its
    /// opens and lines follow it.
    pub fn rename(&mut self, c: &Client, from: &str, to: &str) -> Option<HandleId> {
        let at = self.target_head(self.target_of(c));
        let over = self.lookup(&at, to);
        let kind = Kind::Rename {
            to: to.to_string(),
            over,
        };
        self.path_call(c, from, kind)
    }

    fn path_call(&mut self, c: &Client, p: &str, kind: Kind) -> Option<HandleId> {
        let target = self.target_of(c);
        let file = self
            .lookup(&self.target_head(target), p)
            .unwrap_or_else(|| panic!("{kind:?} of {p}: no such file"));
        let id = self.add_handle(c, target, file, p, kind);
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
        let cur = self.file(&at, &h.file);
        if !cur.name.live() {
            return;
        }
        let mut files = BTreeMap::new();
        let mut set = Vec::new();
        let name = |w: &mut Self, c: &Content, state: State| {
            let id = w.token(&writer, txn);
            w.lineage.wrote(&id, Some(&c.name.id));
            let e = Edit {
                path: c.name.line.clone(),
                line: c.name.line.clone(),
                token: id.clone(),
                gone: !matches!(state, State::Live(_)),
            };
            let mut n = c.clone();
            n.name = Line {
                id,
                line: c.name.line.clone(),
                over: Some(c.name.id.clone()),
                state,
            };
            (n, e)
        };
        fn put(
            files: &mut BTreeMap<String, Content>,
            set: &mut Vec<Edit>,
            (n, e): (Content, Edit),
        ) {
            files.insert(n.name.line.clone(), n);
            set.push(e);
        }
        match &h.kind {
            Kind::Unlink => {
                put(&mut files, &mut set, name(self, &cur, State::Gone));
                self.inodes.remove(&(h.target, h.file.clone()));
            }
            Kind::Rename { to, .. } => {
                if cur.path() == Some(to.as_str()) {
                    return;
                }
                match self.lookup(&at, to) {
                    Some(g) if fresh(&cur) => {
                        // The atomic save (git-model.md §7, a choice): a
                        // rename-over from a file created and never renamed
                        // since is a rewrite of the replaced file with the
                        // new file's bytes: `g` keeps its identity, its
                        // lines aligned with those bytes (rule 10), and the
                        // new file ends. The new file's object is now `g`'s:
                        // its opens follow it; `g`'s old object no path names.
                        let gc = self.file(&at, &g);
                        let bytes = cur.bytes();
                        let mut calls = vec![Write::Truncate(0)];
                        if !bytes.is_empty() {
                            calls.push(Write::Pwrite { offset: 0, bytes });
                        }
                        let (lines, edits) =
                            self.rewrite(&gc, &g, &calls, &writer, txn, &mut Vec::new());
                        if !edits.is_empty() {
                            files.insert(
                                g.clone(),
                                Content {
                                    name: gc.name.clone(),
                                    lines,
                                },
                            );
                            set.extend(edits);
                        }
                        put(&mut files, &mut set, name(self, &cur, State::Gone));
                        let o = self.inodes.remove(&(h.target, h.file.clone()));
                        self.inodes.remove(&(h.target, g.clone()));
                        if let Some(o) = o {
                            self.inodes.insert((h.target, g.clone()), o);
                        }
                        for x in self.handles.values_mut() {
                            if x.target == h.target && Some(x.object) == o {
                                x.file = g.clone();
                            }
                        }
                    }
                    over => {
                        put(
                            &mut files,
                            &mut set,
                            name(self, &cur, State::Live(to.clone())),
                        );
                        if let Some(g) = over {
                            let gc = self.file(&at, &g);
                            put(
                                &mut files,
                                &mut set,
                                name(self, &gc, State::Replaced(h.file.clone())),
                            );
                            self.inodes.remove(&(h.target, g));
                        }
                    }
                }
            }
            Kind::Write => unreachable!("an open is not a path call"),
        }
        let tokens: Vec<&String> = set.iter().map(|e| &e.token).collect();
        self.note(format!("{:?} {:?}: saved {tokens:?}", h.client, h.kind));
        self.commit_save(h.target, &writer, &at, files, set, None);
        // An open of a file this call unlinked or replaced holds an object
        // no path names now, so no file of it is open: a take waiting for
        // that close is woken.
        if let Target::Tree(t) = h.target {
            self.wake(t);
        }
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
        assert!(self.lookup(&at, p).is_none(), "create of {p}: it exists");
        let f = UNBORN.get(self.next_unborn)?.to_string();
        self.next_unborn += 1;
        let writer = self.who(c).0;
        let txn = match target {
            Target::Txn(_, t) => Some(t),
            Target::Tree(_) => None,
        };
        let cur = self.file(&at, &f);
        let id = self.token(&writer, txn);
        self.lineage.wrote(&id, Some(&cur.name.id));
        let mut n = cur.clone();
        n.name = Line {
            id: id.clone(),
            line: f.clone(),
            over: Some(cur.name.id.clone()),
            state: State::Live(p.to_string()),
        };
        let set = vec![Edit {
            path: f.clone(),
            line: f.clone(),
            token: id.clone(),
            gone: false,
        }];
        self.note(format!(
            "create {c:?} {target:?}:{p} as {f}: saved [{id:?}]"
        ));
        self.commit_save(
            target,
            &writer,
            &at,
            BTreeMap::from([(f.clone(), n)]),
            set,
            None,
        );
        let h = self.add_handle(c, target, f, p, Kind::Write);
        assert!(!self.handles[&h].waiting, "a new file is held by no take");
        Some(h)
    }

    /// A close or a client's death in `t`: each waiting entry there whose
    /// `until` files are all closed stops waiting; the next queue step on
    /// `t` takes it, or finds it must wait again (P1). Nothing waits behind
    /// it meanwhile (P3 is gone).
    fn wake(&mut self, t: Tree) {
        let open = self.write_open(t);
        if let Some(m) = self.waits.get_mut(&t) {
            let woken: Vec<u64> = m
                .iter()
                .filter(|(_, x)| x.until.is_disjoint(&open))
                .map(|(n, _)| *n)
                .collect();
            for n in &woken {
                m.remove(n);
            }
            if m.is_empty() {
                self.waits.remove(&t);
            }
            for n in woken {
                self.note(format!("wake {t:?}'s entry {n}"));
            }
        }
    }

    /// A waiting call goes ahead: an open is granted, a path call runs.
    fn grant(&mut self, h: HandleId) {
        if self.handles[&h].kind == Kind::Write {
            let x = self.handles.get_mut(&h).expect("open");
            x.waiting = false;
            self.note(format!("grant h{h}"));
            let x = self.handles[&h].clone();
            // No alarm can be due here (review of `d552b3d`, P1-3): P1 lets a
            // take go only when no earlier waiting entry shares a file with
            // it, no entry starts waiting while a take holds the tree, and a
            // prefix sync's rest waits only for files its delivered prefix
            // does not write; so no waiting entry waits for a file a
            // release grants. The model stops loudly if one would.
            if let Target::Tree(t) = x.target {
                assert!(
                    self.waits
                        .get(&t)
                        .into_iter()
                        .flatten()
                        .all(|(_, w)| { x.files().iter().all(|f| !w.until.contains(f)) }),
                    "h{h}, granted at a release in {t:?}, is a file a waiting entry waits for"
                );
            }
        } else {
            let call = self.handles.remove(&h).expect("a waiting call");
            self.note(format!("grant h{h}: it runs"));
            self.run_path_call(&call);
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

    /// Commit (MODEL.md rule 5): the transaction's branch lands in the
    /// fork's current head as a child's change lands in its parent, as one
    /// merge commit named for the client, which then lands like any fork
    /// commit. Never refused. The fork is the transaction's parent, so a
    /// fork line concurrent with a staged line (one a sync brought in since
    /// begin) wins, and the transaction loses whole (rule 3e): none of its
    /// lines apply, the merge keeps the fork's tree (so history shows the
    /// decision) and the answer names every staged line. The fork's one
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
            let m = self.resolve(&ours, &edit, &head);
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
            self.record(t, Direction::Transaction, &m, &ours, &c);
            self.landings.push(Landed {
                tree: t,
                direction: Direction::Transaction,
                before: ours.clone(),
                result: c.clone(),
                units: vec![(txn.head.clone(), txn.start.clone())],
                winners: m.winners.clone(),
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

    // ── crash and the journal (git-model.md §3) ──────────────────────────

    /// Every agentfs process dies at once (a view host, the history
    /// service, the node). Each write call was in its tree's journal before
    /// it returned (git-model.md §3; the model keeps an open's journaled
    /// calls on its handle), so at restart, before any tree is served
    /// again, each open is saved from the journal as its close would have
    /// saved it, unless a save already names it (`Agentfs-Open`), and the
    /// opens end: their descriptors died with the mount. An open no path
    /// names raises its `OrphanDropped` event again (no save records it in
    /// git: events are at least once). Waiting opens and path calls never
    /// returned: dropped, nothing recorded. A take in progress wrote nothing
    /// (its write is one ref transaction): its entries go back to the front
    /// of its queue; waits are dropped and found again at the next take.
    /// Transactions survive: their branches are refs.
    pub fn crash(&mut self) {
        for h in self.crash_down() {
            self.restart_open(h);
        }
    }

    /// [`World::crash`]'s first part: every process is down; takes go back
    /// to their queues, waits and calls that never returned are dropped.
    /// Returns the opens the restart replays from the journal, in order.
    pub fn crash_down(&mut self) -> Vec<HandleId> {
        self.note("crash: every agentfs process dies; restart replays the journal".into());
        let holds = std::mem::take(&mut self.holds);
        for (t, hold) in holds {
            let entries: Vec<Entry> = match hold.take {
                Take::Sync { .. } => vec![Entry::Sync],
                Take::Landing { from, run, .. } => run
                    .into_iter()
                    .map(|commit| Entry::Landing { from, commit })
                    .collect(),
            };
            // Back in queue order: each entry where its number puts it.
            let q = self.queues.entry(t).or_default();
            for (n, e) in hold.seqs.into_iter().zip(entries) {
                let at = q.iter().position(|(m, _)| *m > n).unwrap_or(q.len());
                q.insert(at, (n, e));
            }
        }
        self.waits.clear();
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        let mut replay = Vec::new();
        for id in ids {
            let h = &self.handles[&id];
            if h.waiting || h.kind != Kind::Write {
                self.handles.remove(&id);
                self.note(format!("crash: h{id} never returned: dropped"));
            } else {
                replay.push(id);
            }
        }
        replay
    }

    /// [`World::crash`]'s restart, for one open: saved from the journal as
    /// its close would save it, unless a save already names it; it ends.
    pub fn restart_open(&mut self, id: HandleId) {
        let h = self.handles.remove(&id).expect("an open to replay");
        if self.saved(id) {
            self.note(format!("crash: h{id} already saved: skipped"));
            return;
        }
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
        let handle = self.handles[&h].clone();
        assert!(
            !handle.waiting && handle.kind == Kind::Write,
            "close a granted open"
        );
        let tokens = self.save_handle(&handle);
        self.note(format!(
            "close h{h}: saved {tokens:?}; crash before its journal drops it"
        ));
    }

    /// Whether a save names open `h` (`Agentfs-Open`), read from git.
    pub fn saved(&self, h: HandleId) -> bool {
        let grep = format!("--grep=^{OPEN}: {h}$");
        !git(&self.repo, &["rev-list", "--all", &grep]).is_empty()
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
                    && self.eligible(**t).is_some()
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
    /// same fork's plain saves directly behind it, or none when it waits.
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

    /// Progress rule P1 (git-model.md §6): the first entry of `t`'s queue
    /// that may be taken now, and the files of the entries ahead of it that
    /// wait (or wait behind a waiting one). An entry waits behind every
    /// earlier waiting entry it shares a file or a source with; so does
    /// every entry behind one that waits behind, so one source's entries
    /// keep their order and no two entries sharing a file pass each other.
    /// Which files an entry not yet planned touches is read from git: a
    /// landing's own changes (each against its first parent), a sync's
    /// every file its tree and the parent's head differ in (all it may
    /// write).
    fn scan(&self, t: Tree) -> (Option<usize>, BTreeSet<String>) {
        let waiting = self.waits.get(&t);
        let mut files = BTreeSet::new();
        let mut sources = BTreeSet::new();
        for (i, (n, e)) in self.queues.get(&t).into_iter().flatten().enumerate() {
            if let Some(x) = waiting.and_then(|m| m.get(n)) {
                files.extend(x.files.iter().cloned());
                sources.insert(e.source());
                continue;
            }
            let mine = self.hint(t, e);
            if sources.contains(&e.source()) || !mine.is_disjoint(&files) {
                files.extend(mine);
                sources.insert(e.source());
                continue;
            }
            return (Some(i), files);
        }
        (None, files)
    }

    fn eligible(&self, t: Tree) -> Option<usize> {
        self.scan(t).0
    }

    /// The files queue entry `e` of `t` may touch, before it is planned.
    fn hint(&self, t: Tree, e: &Entry) -> BTreeSet<String> {
        match e {
            Entry::Landing { commit, .. } => self.touches_of(commit),
            Entry::Sync => match t {
                Tree::Fork(f) => self.changed(&self.head(t), &self.head(self.forks[&f].parent)),
                Tree::Main => BTreeSet::new(),
            },
        }
    }

    /// The files a fork commit's own changes touch (each change against its
    /// first parent), cached: commits never change.
    fn touches_of(&self, c: &str) -> BTreeSet<String> {
        if let Some(x) = self.touches.borrow().get(c) {
            return x.clone();
        }
        let mut files = BTreeSet::new();
        for u in self.units(c) {
            files.extend(self.changed(&self.first_parent(&u), &u));
        }
        self.touches
            .borrow_mut()
            .insert(c.to_string(), files.clone());
        files
    }

    /// Rule 9, a take begins: plan the first entry P1 lets go
    /// ([`World::scan`]) into `t` and the files it locks there (a landing:
    /// every file its changes touch, [`World::touched`]). If a client of `t`
    /// has one of them open for writing, the entry waits for those to close
    /// (`waits`) and nothing is taken; a new open of one of its files is
    /// granted and raises an alarm ([`Delayed`]: no call waits on a waiting
    /// take, the owner's decision of 2026-11-04); else it leaves the queue
    /// and holds its files. A squash carries only the saves directly behind its first
    /// that share no file with an entry ahead of it that waits and touch no
    /// file open for writing in `t`; a first that must wait waits alone.
    fn acquire(&mut self, t: Tree) -> usize {
        let q = self.queue_numbered(t);
        let (i, blocked) = match self.scan(t) {
            (Some(i), b) => (i, b),
            (None, _) => panic!("{t:?}: no entry can be taken"),
        };
        let (seq, head) = q[i].clone();
        let rest: Vec<Entry> = q[i + 1..].iter().map(|(_, e)| e.clone()).collect();
        let ours = self.head(t);
        let (take, files, n) = match head {
            Entry::Sync => {
                let Tree::Fork(f) = t else {
                    unreachable!("main has no parent")
                };
                let parent = self.forks[&f].parent;
                let whole = self.head(parent);
                let known = self.notes_head();
                let landing = self.landing_pending(self.forks[&f].parent, f);
                let p = self.prefix(t, parent, &ours, &whole, known.as_deref());
                let Some((up, tree, held_back)) = p.deliver else {
                    // Its first commit would overwrite an open file: it
                    // waits for those to close, taking nothing.
                    let until = p.until.expect("a sync with nothing to deliver waits");
                    self.note(format!(
                        "{t:?}'s entry {seq} (a sync) waits for {until:?} to close"
                    ));
                    let mut files = self.hint(t, &Entry::Sync);
                    files.extend(until.iter().cloned());
                    self.waits
                        .entry(t)
                        .or_default()
                        .insert(seq, Waiting { files, until });
                    return 0;
                };
                let files = tree
                    .as_ref()
                    .map(|tr| self.changed(&ours, tr))
                    .unwrap_or_default();
                if let Some(until) = p.until {
                    // P2: it delivers the prefix now; the entry stays queued
                    // and waits for the rest.
                    self.prefix_syncs += 1;
                    self.note(format!(
                        "{t:?}'s entry {seq} delivers {up:.7} of {whole:.7}, holds {files:?}, and waits for {until:?} to close for the rest"
                    ));
                    let mut all = self.hint(t, &Entry::Sync);
                    all.extend(until.iter().cloned());
                    all.extend(files.iter().cloned());
                    self.waits
                        .entry(t)
                        .or_default()
                        .insert(seq, Waiting { files: all, until });
                    self.holds.insert(
                        t,
                        Hold {
                            files,
                            take: Take::Sync {
                                up,
                                tree,
                                landing,
                                known,
                                head: whole,
                                held_back,
                            },
                            seqs: Vec::new(),
                            ours,
                            entered: self.entered.len(),
                        },
                    );
                    return 0;
                }
                (
                    Take::Sync {
                        up,
                        tree,
                        landing,
                        known,
                        head: whole,
                        held_back,
                    },
                    files,
                    1,
                )
            }
            Entry::Landing { from, commit } => {
                let (mut run, _) = self.run_from(from, &commit, &rest);
                // A squash carries only saves that could land now on their
                // own: none sharing a file with a waiting entry ahead (P1),
                // none touching a file open for writing here; and if its
                // first must wait, it waits alone. So a long-held file holds
                // back only the saves that touch it, under either history
                // setting (P1).
                let open = self.write_open(t);
                let k = if self.touches_of(&commit).is_disjoint(&open) {
                    run.iter()
                        .skip(1)
                        .position(|c| {
                            let mine = self.touches_of(c);
                            !mine.is_disjoint(&blocked) || !mine.is_disjoint(&open)
                        })
                        .map(|k| k + 1)
                } else {
                    Some(1)
                };
                if let Some(k) = k {
                    run.truncate(k);
                }
                let merged = self.judge(&ours, &run);
                let files = self.touched(&merged);
                let n = run.len();
                (Take::Landing { from, run, merged }, files, n)
            }
        };
        let open: BTreeSet<String> = self.write_open(t).intersection(&files).cloned().collect();
        if !open.is_empty() {
            // A landing whose files are open waits for them to close; a
            // sync never gets here (`prefix` delivers none of an open file).
            self.note(format!("{t:?}'s entry {seq} waits for {open:?} to close"));
            self.waits.entry(t).or_default().insert(
                seq,
                Waiting {
                    until: files.clone(),
                    files,
                },
            );
            return 0;
        }
        let mut qd = self.queues.remove(&t).unwrap_or_default();
        let seqs: Vec<u64> = qd.drain(i..i + n).map(|(m, _)| m).collect();
        self.queues.insert(t, qd);
        self.note(format!("{t:?} holds {files:?} (entry {seq})"));
        self.holds.insert(
            t,
            Hold {
                files,
                take,
                seqs,
                ours,
                entered: self.entered.len(),
            },
        );
        n
    }

    /// Rule 9, a take ends: write the held change, then release the files:
    /// every open waiting for them is granted (an event). If a save of
    /// another file moved the tree's head since the take began, the change
    /// is planned again on the new head; it must still write only files it
    /// holds, and the model stops loudly if not. It does: a landing holds
    /// every file its changes touch, losing ones included, so a replan can
    /// flip a change from lost to won only through a save of a file it
    /// holds, and no write-open of a held file is granted.
    fn finish(&mut self, t: Tree) {
        let hold = self.holds.remove(&t).expect("a take in progress");
        let head = self.head(t);
        let moved = head != hold.ours;
        match hold.take {
            Take::Sync {
                up,
                tree,
                landing,
                known,
                held_back,
                ..
            } => {
                let tree = if moved {
                    let Tree::Fork(f) = t else {
                        unreachable!("main has no parent")
                    };
                    self.sync_tree(
                        self.forks[&f].parent,
                        &head,
                        &up,
                        known.as_deref(),
                        &held_back,
                    )
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
                    self.judge(&head, &run)
                } else {
                    merged
                };
                held(t, &merged.files, &hold.files);
                self.land(t, from, &run, merged);
            }
        }
        self.taken = Some(self.head(t));
        // The release: every call waiting for the hold goes ahead (none
        // waits for anything else), then an entry waiting in `t` whose
        // files closed meanwhile is woken (a prefix sync's rest).
        let waiting: Vec<HandleId> = self
            .handles
            .values()
            .filter(|h| h.waiting && h.target == Target::Tree(t))
            .map(|h| h.id)
            .collect();
        for h in waiting {
            self.grant(h);
        }
        self.wake(t);
    }

    /// Rule 9, the files a landing locks, fixed when it begins: every file
    /// any of its changes touches (read from git, each change against its
    /// own first parent), whether the plan says it wins or loses, and every
    /// file the plan writes. Locking only what the plan writes would leave
    /// a losing change's file open to a write-open of the parent's, whose
    /// save could flip that change to a win at the replan in `finish`.
    fn touched(&self, m: &Merged) -> BTreeSet<String> {
        let mut files = m.files.clone();
        for u in &m.units {
            files.extend(self.changed(&self.first_parent(u), u));
        }
        files
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
    /// `Squashed`, when `first` is a plain save, the plain saves of the
    /// same fork directly behind it in the queue, up to the first other
    /// entry (another fork's, a sync, a transaction's commit or a nested
    /// landing). A squash never takes an entry ahead of one queued before
    /// it, so the history setting decides no collision (rule 7): the take
    /// judges the same changes in the same order as one take each would.
    fn run_from(
        &self,
        from: ForkId,
        first: &str,
        rest: &[Entry],
    ) -> (Vec<String>, VecDeque<Entry>) {
        let mut run = vec![first.to_string()];
        let mut open = self.policies.history == HistoryPolicy::Squashed && self.plain_save(first);
        let mut keep = VecDeque::new();
        for e in rest {
            if open
                && let Entry::Landing { from: g, commit } = e
                && *g == from
                && self.plain_save(commit)
            {
                run.push(commit.clone());
                continue;
            }
            open = false;
            keep.push_back(e.clone());
        }
        (run, keep)
    }

    /// The changes a fork commit brings, each judged whole (rule 3e): the
    /// commit itself (a save, a transaction's commit, or under
    /// `FullLineage` a landing of its own child's change), or under
    /// `Squashed` for a landing commit the saves its graft names, which
    /// won in that fork: a squashed changeset is history only, never a
    /// change judged whole (rule 7).
    fn units(&self, c: &str) -> Vec<String> {
        if self.policies.history == HistoryPolicy::Squashed
            && self.trailer(c, "Agentfs-Cause") == "Landing"
        {
            return commit_parents(&self.repo, c, false)[1..]
                .iter()
                .flat_map(|u| self.units(u))
                .collect();
        }
        vec![c.to_string()]
    }

    /// Rules 3e and 7, one take of `run` into the tree at `ours`: each
    /// change judged on its own, in order, whole ([`World::resolve`]) on
    /// the result of those before it; the result holds exactly the changes
    /// that won. Between two changes the result so far is a commit no ref
    /// names, its second parent the change that won, so the next change's
    /// judgement reads that change's lines from git like any other.
    fn judge(&self, ours: &str, run: &[String]) -> Merged {
        let units: Vec<String> = run.iter().flat_map(|c| self.units(c)).collect();
        let mut at = ours.to_string();
        let mut collisions = Vec::new();
        let mut winners = Vec::new();
        for u in &units {
            let m = self.resolve(&at, &self.edit_of(&self.first_parent(u), u), u);
            if m.collisions.is_empty() {
                if m.tree != self.tree_of(&at) {
                    at = self.commit(&[&at, u], &m.tree, "agentfs", "Judged", "none");
                }
                winners.push(u.clone());
            } else {
                collisions.extend(m.collisions);
            }
        }
        let tree = self.tree_of(&at);
        Merged {
            files: self.changed(ours, &tree),
            tree,
            collisions,
            units,
            winners,
        }
    }

    /// The versions each of a take's changes sets.
    fn incoming(&self, units: &[String]) -> Vec<Incoming> {
        units
            .iter()
            .flat_map(|u| self.edit_of(&self.first_parent(u), u))
            .collect()
    }

    /// Rule 3, a fork's change into its parent `x`, the one place a
    /// collision between them is decided. A fast-forward when the commit's
    /// base is the target's head (`FullLineage`), else each change the take
    /// carries is judged whole ([`World::judge`]): the parent's line wins a
    /// collision, and the change that lost it applies nothing. `m`: the
    /// take planned on `x`'s current head.
    fn land(&mut self, x: Tree, from: ForkId, run: &[String], m: Merged) {
        let tip = run.last().expect("a commit").clone();
        let ours = self.head(x);
        let incoming = self.incoming(&m.units);
        let carried: Vec<&str> = incoming.iter().map(|i| i.version.id.as_str()).collect();
        self.note(format!(
            "landing f{from}'s {} commit(s) to {tip:.7} into {x:?} carries {carried:?}; won: {:?}",
            run.len(),
            m.winners
        ));
        for i in &incoming {
            self.entered.push((x, i.edit()));
        }
        if m.winners.len() > 1 {
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
            self.landed(x, &ours, &tip, &m);
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
            // lineage, the saves that won, is a graft beside history, made
            // before any read of `new`. A save that lost is not in it: its
            // collision ref keeps it. Made even when every change lost, so
            // the parent's history shows the decision and its forks sync.
            let new = self.commit(&[&ours], &m.tree, &writer, "Landing", &Self::name(x));
            if !m.winners.is_empty() {
                let mut graft = vec!["replace", "--graft", &new, &ours];
                graft.extend(m.winners.iter().map(String::as_str));
                git(&self.repo, &graft);
                self.ref_moves += 1;
            }
            new
        };
        self.record(x, Direction::Landing, &m, &ours, &new);
        self.landed(x, &ours, &new, &m);
        self.note(format!("landed f{from}'s {tip:.7} into {x:?}"));
        self.set_head(x, &new);
        self.moved(x, true);
    }

    /// Records a take's changes and which won, for `check`.
    fn landed(&mut self, x: Tree, before: &str, result: &str, m: &Merged) {
        self.landings.push(Landed {
            tree: x,
            direction: Direction::Landing,
            before: before.to_string(),
            result: result.to_string(),
            units: m
                .units
                .iter()
                .map(|u| (u.clone(), self.first_parent(u)))
                .collect(),
            winners: m.winners.clone(),
        });
    }

    /// Progress rule P2, the prefix sync (the owner, 2026-11-04: "agreed",
    /// on "a sync delivers the parent's changes in order up to the first
    /// change that touches a file the sandbox has open for writing"): the
    /// parent's commits fork `t` (at `ours`) lacks are `whole`'s first
    /// parents back to the first the fork has, each one change as the
    /// parent took it (a save, a transaction's commit, a landing; a
    /// squash's saves are in it as the parent took them). It delivers them
    /// in order up to, not including, the first whose delivery would
    /// overwrite a file open for writing in `t` (the sync's own write, so
    /// a commit that leaves the fork's file as it is touches nothing), and
    /// holds back that one and every later one, their collision notes
    /// included, until those files close.
    fn prefix(
        &self,
        t: Tree,
        parent: Tree,
        ours: &str,
        whole: &str,
        known: Option<&str>,
    ) -> Prefix {
        let open = self.write_open(t);
        let none = BTreeSet::new();
        let tree = self.sync_tree(parent, ours, whole, known, &none);
        let writes = |tr: &Option<String>| -> BTreeSet<String> {
            tr.as_ref()
                .map(|tr| self.changed(ours, tr))
                .unwrap_or_default()
        };
        if writes(&tree).is_disjoint(&open) {
            return Prefix {
                deliver: Some((whole.to_string(), tree, none)),
                until: None,
            };
        }
        let mut chain = Vec::new();
        let mut c = whole.to_string();
        while !self.is_ancestor(&c, ours) {
            chain.push(c.clone());
            c = self.first_parent(&c);
        }
        chain.reverse();
        let mut deliver = None;
        let mut before = BTreeSet::new();
        for (k, c) in chain.iter().enumerate() {
            let held_back: BTreeSet<String> = chain[k + 1..].iter().cloned().collect();
            let tree = self.sync_tree(parent, ours, c, known, &held_back);
            let now = writes(&tree);
            if !now.is_disjoint(&open) {
                // It waits until no file this commit adds to the sync's
                // write is open (its own change's files, as for a landing).
                return Prefix {
                    deliver,
                    until: Some(now.difference(&before).cloned().collect()),
                };
            }
            deliver = Some((c.clone(), tree, held_back));
            before = now;
        }
        unreachable!("the whole sync overwrites an open file, so one of its commits does")
    }

    /// The tree a sync of the parent's `up` into fork `x` (at `ours`)
    /// makes, per [`MergeRule`]; none if the fork already has `up`.
    fn sync_tree(
        &self,
        parent: Tree,
        ours: &str,
        up: &str,
        known: Option<&str>,
        held_back: &BTreeSet<String>,
    ) -> Option<String> {
        if self.is_ancestor(up, ours) {
            return None;
        }
        Some(if self.is_ancestor(ours, up) {
            self.tree_of(up)
        } else {
            match self.policies.merge {
                MergeRule::PerLine => self.adopt(parent, ours, up, known, held_back),
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
                "\n{:?} {:?} {} {:?} {:?} {:?} {:?}",
                c.tree, c.direction, c.path, c.lines, c.lost, c.won, c.by
            ));
        }
        s
    }

    /// Every ref's commits as stored (grafts' own commits included), for
    /// history checks.
    pub fn all_commits(&self) -> Vec<String> {
        let notes = format!("--exclude={NOTES}");
        git(
            &self.repo,
            &["--no-replace-objects", "rev-list", &notes, "--all"],
        )
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
/// Whether file `c` is new since its create: an [`UNBORN`] identity whose
/// name version is still the one its create wrote (never renamed since).
/// A rename-over from such a file is an atomic save (git-model.md §7).
/// Read from git alone, so it is the same whenever the save runs and
/// whether or not the parent has taken the create yet.
pub fn fresh(c: &Content) -> bool {
    UNBORN.contains(&c.name.line.as_str()) && c.name.over.as_deref() == Some(c.name.line.as_str())
}

fn held(t: Tree, files: &BTreeSet<String>, holds: &BTreeSet<String>) {
    assert!(
        files.is_subset(holds),
        "a take into {t:?} would write {files:?}, holding only {holds:?}"
    );
}

/// Rules 3 and 10: `base` with each version of `applied` set, its line's
/// old version (if any) leaving its place, each placed in the order `src`
/// has the lines (design choice, as a sequence CRDT's left origin): after
/// the nearest line before it in `src` that the result holds, then past
/// any lines there that `src` lacks (lines `base` added that `src` never
/// saw), so of two inserts at one place the base's comes first. The
/// referee calls it with the parent's file as `base` and the incoming one
/// as `src`; a sync with the parent's file as `base` and the fork's as
/// `src`. A version of the name line replaces the name.
fn splice(base: &Content, applied: &[Line], src: &Content) -> Content {
    let by_line: BTreeMap<&str, &Line> = applied.iter().map(|l| (l.line.as_str(), l)).collect();
    let in_src: BTreeSet<&str> = src.lines.iter().map(|l| l.line.as_str()).collect();
    let mut lines: Vec<Line> = base
        .lines
        .iter()
        .filter(|l| !by_line.contains_key(l.line.as_str()))
        .cloned()
        .collect();
    for (k, s) in src.lines.iter().enumerate() {
        let Some(v) = by_line.get(s.line.as_str()) else {
            continue;
        };
        let anchor = src.lines[..k]
            .iter()
            .rev()
            .find_map(|l| lines.iter().position(|o| o.line == l.line));
        let mut at = anchor.map_or(0, |a| a + 1);
        while at < lines.len() && !in_src.contains(lines[at].line.as_str()) {
            at += 1;
        }
        lines.insert(at, (*v).clone());
    }
    let name = by_line
        .get(base.name.line.as_str())
        .map_or_else(|| base.name.clone(), |v| (*v).clone());
    Content { name, lines }
}

/// One `@@ -a,b +c,d @@` header of a `-U0` diff as 0-based (old start, old
/// count, new start, new count); a count of 0 starts after the line named.
fn parse_hunk(line: &str) -> Option<(usize, usize, usize, usize)> {
    let h = line.strip_prefix("@@ -")?;
    let (old, rest) = h.split_once(" +")?;
    let new = rest.split_once(" @@")?.0;
    let range = |r: &str| -> Option<(usize, usize)> {
        let (s, n) = match r.split_once(',') {
            Some((s, n)) => (s.parse::<usize>().ok()?, n.parse::<usize>().ok()?),
            None => (r.parse::<usize>().ok()?, 1),
        };
        Some((if n == 0 { s } else { s - 1 }, n))
    };
    let ((os, on), (ns, nn)) = (range(old)?, range(new)?);
    Some((os, on, ns, nn))
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
