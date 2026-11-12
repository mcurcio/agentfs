# Module cut, step 5, round 2: the engine's rules once, a typed version, then the split (rev. 5, reconciled)

Layer: interfaces and implementation, inside `module-cut.md`. Built on `step5b-design-r3.md` (rev. 4; `-r2.md` is
superseded). Lines and probes are 22d55f9's; probes 1-5 are re-run after 5b merges. No rule, model file, conformance
check or public `Volume` signature changes.

**Decision first.** Order (`plan.md`): 6, the plan PR, 5b, **5**, one-engine M1 (why: §8). Three PRs, four slices,
each a pushed commit green alone. PR 5.1: (1) typed versions; (2) each rule once, proved equal to the old code by a
differential test. PR 5.2: (3) verbatim moves. PR 5.3: (4) the modules take their own state. **Round 2 changed slice 4
only:** a tree is a `TreeId` in every module; `history/refs.rs` homes the ref layout and `history/message.rs` every
trailer, written and read; `Message` is typed. **No owner question.**

## 1. Requirements, guidance, proposed

| # | Statement | Kind | Source |
|---|---|---|---|
| R1 | "a design doc exists only until its slice is built, and the commit that lands the slice deletes it" | **Requirement** (asked as absolute: "Yes, this is a good idea to record") | OWNER-GUIDANCE, 2026-11-09 |
| G1 | Six modules, the actor only composing ("B"); "I want each phase of the work to start with an architect" (gate: 0 mismatched traces) | Guidance | 2026-11-08; 2026-11-09 |
| G2 | "the goal is to have one engine, ostensibly the new tree view" | Guidance | 2026-11-12 |
| G3 | "more important for an _instance_ of agentfs to know that it is "the root" than to have a specially named branch"; "the trailer should be addressed so that it records meaningful information" | Guidance | 2026-11-12 |
| G4 | "drop support. we are pre-production"; "i would like for the git history to remain compatible across minor versions" | Guidance | 2026-11-06; 2026-11-02 |
| G5 | "'goal oriented' is a good rubric for any workflow process to consider" (§2); step 6 Q2, a transaction commit lands under every landing rule: "A" | Guidance | 2026-11-12 |
| P | All else. R1 deletes no page here (`module-cut.md` serves step 7, which deletes it) | **Proposed** | this page |

## 2. Goals and what step 5 delivers

| End goal | Step 5's part |
|---|---|
| One engine (G2) | On its path (5b, 5, M1): what M1-M6 extend has one home first (§6), not a 2,427-line `mod.rs` |
| Recording is the baseline | Design rule 2: the commit point stays in the actor; no step of the save path changes |
| Invisible; git idioms; any remote | Nothing reachable through the mount; the ref layout (git-model §4) in one home, which M4's publisher reads |
| History is the product; a meaningful trailer (G3) | 5b records identity. Step 5 types and single-homes every trailer, golden-equal to post-5b bytes, so M1's "where" fields (one-engine Q3) are one edit |

## 3. Canonical shape

**Concepts touched** (existing terms): versions, event number, ancestry, verdict, whole (R3e), undecided, the `Cut`,
lock keys, root, upstream, fork, trailers, refs. **No domain change.** Line versions and event numbers get types.

| Rule or record | Copies today (22d55f9) |
|---|---|
| Ancestry walk | `tree_reconcile::verdict::descends`, `referee::line_descends`, `referee::name_descends` |
| Rule-3 verdict | `tree_reconcile::verdict` (lines), `referee::name_verdict` (names): the same five cases |
| Name combine | `referee::combine_names` and `sync::combine`, byte for byte |
| Line-graph union | `combined_graph`, inline in `unseen_parent_line`, an unchecked `extend` in `judge` |
| R3e | `land_save` loses a file whole; `judge` the save, calling `verdict` again per line |
| P2 prefix; version allocator | `queue::prefix` and `plan_sync`'s loop; `format!("{seq}:{index}")` in `build_close`, `build_rename` |
| Trailers written; read | `mod.rs` 675, 1489, 1670, 2188; read at 1200, 2245 (`-Format`), 1445 (`-Collisions`), 1633, 1766 (`-Tree`), `tree_git.rs:208` (`-Cause`, `-Event`) |
| Ref layout | `mod.rs` 114, 709, 821, 832, 1284, 1302, 1389, 1513, 1629, 1747, 1755, 2230; `tree_git.rs` 53 (`HEAD`), 135 (`merge`) |

**Patterns.** Newtype (Rust API Guidelines C-NEWTYPE); parse, don't validate (King, 2019); functional core,
imperative shell (Bernhardt); ports and adapters (Cockburn): `Landed`, a layout-free `TreeGit`; unit of work over a
write-ahead log (Fowler, *PoEAA*); smart constructor: `Cut`; differential testing (McKeeman, 1998); Extract Class.

## 4. Design rules (design choices)

1. **One implementation per rule, one home per record.** A grep outside `#[cfg(test)]` is the deletion test:
   `line_descends`, `name_descends`, `name_verdict`, `combine_names`, `combined_graph`, `land_save`, `split_once(':')`,
   `queue.rs`'s `fn prefix` and `direct_events` find nothing. In `tree_actor/` and `tree_git.rs`, `"Agentfs-` is
   found only in `history/message.rs`, `"refs/` only in `history/refs.rs`, `"main"` nowhere (5b's gate, kept).
2. **The commit point stays in the actor, by construction.** Only `Volume` holds the `Journal`; `run_step` and
   `journal_record` are the only appends. Modules prepare (pure or object writes) and install (cannot fail).
   Failpoints, their order, stage timers and a step's `RefUpdate` order (branch, queue entry, collision ref, object
   table, enqueues) stay.
3. **Decision order is a key, not a push order.** `whole` sorts candidates by `Order` (names before lines, then file
   identity, then position in the incoming line map) before R3e reads the first loss: today's push order (probe 4),
   written into `Agentfs-Collisions`. So the units run in the frame's order without changing a byte.
4. **A tree is a `TreeId` above the journal** (5b's rule 1, extended to the modules). Only `journal/`, a DTO on disk,
   holds tree strings; its fold parses them.

## 5. Interfaces

### Slice 1: typed versions (unchanged from rev. 3)

```rust
// tree_reconcile.rs (public): generic over the caller's version; tests keep String ids ("r1", "e1").
pub trait Id: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned {}
impl<T: Clone + Ord + fmt::Debug + fmt::Display + Serialize + DeserializeOwned> Id for T {}
pub struct Line<V> { pub identity: V, pub version: V, pub bytes: Option<Vec<u8>> }
pub struct LineMap<V> { pub lines: Vec<Line<V>>, pub ancestors: BTreeMap<V, Option<V>> }
impl<V: Id> LineMap<V> {
    pub fn append(&self, suffix: &[u8], next: impl FnMut() -> V) -> Result<Self, Error>;
    pub fn align(&self, image: &[u8], next: impl FnMut() -> V) -> Result<Self, Error>;
    // bytes, encode, decode, validate: same bodies, generic
}
pub enum Error { /* ... */ DuplicateId(String) /* the version's Display */ }

// tree_actor/tree.rs (crate-private; journal numbering stays out of the line-map library)
/// The journal number of the step that wrote a version; name versions are event numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct EventNumber(u64);
impl EventNumber {
    pub(super) const fn new(seq: u64) -> Self;          // once per step, in `run_step`
    pub(super) const fn get(self) -> u64;
    pub(super) const fn object(self) -> u64;   // the one event-to-object step: creates (2051, 2065), R11.3 (sync.rs:131)
}
/// "<event>:<index>", index from 1. JSON is the same string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub(super) struct LineVersion { event: EventNumber, index: u64 }
impl LineVersion { pub(super) const fn event(self) -> EventNumber; }
impl fmt::Display for LineVersion; impl FromStr for LineVersion { type Err = Error; }   // Error::Format
/// One save's line versions in order: <event>:1, <event>:2, ...
pub(super) struct Versions { event: EventNumber, last: u64 }
impl Versions { pub(super) const fn of(event: EventNumber) -> Self; pub(super) fn next(&mut self) -> LineVersion; }
pub(super) type Lines = LineMap<LineVersion>;
```

`NameHistory`, `NameVersion::over` and `owners` take `EventNumber`. **Still `u64`:** object and handle ids, `Record::seq`,
page writes, `MergeFileIdentity`. JSON values unchanged (probe 1); new `ancestors` keys sort numerically (G4). ~300 lines.

### Slice 2: rules once (`tree_actor/referee/`, crate-private; unchanged from rev. 3)

```rust
// referee/ancestry.rs: the port the other units ask
pub(super) trait Graph { type Version: Copy + Ord;   // over: Ok(None) for a first version; Missing if not held
    fn over(&self, v: Self::Version) -> Result<Option<Self::Version>, Broken>; }
impl Graph for BTreeMap<LineVersion, Option<LineVersion>> { type Version = LineVersion; }
impl Graph for NameHistory { type Version = EventNumber; }
#[derive(Debug, thiserror::Error)] pub(super) enum Broken { #[error("cyclic ancestry")] Cycle, #[error("missing ancestor")] Missing }
impl From<Broken> for Error;     // Error::Format, EIO; lines were Reconcile(InvalidMap), also EIO
pub(super) fn descends<G: Graph + ?Sized>(g: &G, v: G::Version, ancestor: G::Version) -> Result<bool, Broken>;
pub(super) fn union(a: &Ancestors, b: &Ancestors) -> Result<Ancestors>;   // Format if they disagree on an `over`
pub(super) type Ancestors = BTreeMap<LineVersion, Option<LineVersion>>;
pub(super) trait Landed { fn landed(&mut self, save: EventNumber) -> Result<bool>; }  // history's one port
// tree.rs: Format if the two differ on a version; keeps self.head
impl NameHistory { pub(super) fn combine(&self, other: &NameHistory) -> Result<NameHistory>; }
// referee/verdicts.rs: rule 3 for names and lines; the unseen-line and ended-file rules
pub(super) struct Held<V> { pub version: V, pub live: bool }
pub(super) enum Verdict<V> { Apply, Drop, Lose { winner: V } }
pub(super) fn verdict<G: Graph + ?Sized>(g: &G, upstream: Option<Held<G::Version>>, incoming: Held<G::Version>)
    -> Result<Verdict<G::Version>, Broken>;
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]   // a candidate's place (rule 3): names, then lines
pub(super) struct Order { pub line: bool, pub file: MergeFileIdentity, pub at: usize }
pub(super) struct Candidate { pub order: Order, pub lost: Version, pub loss: Option<(Version, CollisionKind)> }
pub(super) fn verdicts(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, prior: &PriorDecisions)
    -> Result<Vec<Candidate>>;
// referee/names.rs: rename-over displacement (R11.2), to a fixed point; the sync's clash fallback
pub(super) fn displace(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, c: &mut [Candidate]) -> Result<()>;
// referee/placing.rs: one file's applied lines placed, and its joins (R10.4, R10.8); sync's placing (R10.6, R10.9)
/// `verdicts` come from verdicts() (none computed twice); `land` folds each join (lost, winner) into the candidates.
pub(super) fn place(upstream: &Lines, base: &Lines, incoming: &Lines,
                    verdicts: &BTreeMap<LineVersion, Verdict<LineVersion>>) -> Result<Placed>;
pub(super) struct Placed { pub joins: Vec<(LineVersion, LineVersion)>, pub lines: Lines }
pub(super) fn sync_file(upstream: &Lines, fork: &Lines, undecided: &BTreeSet<LineVersion>) -> Result<Lines>;
// referee/whole.rs: R3e, its one home
/// Sorts by Order; a loss anywhere makes every candidate a collision (Whole, named for the first loss's
/// winner, where it did not lose on its own) and the snapshot is the upstream's; else `applied`.
pub(super) fn whole(upstream: &Snapshot, applied: Snapshot, candidates: Vec<Candidate>) -> Decision;
// referee/mod.rs
pub(super) struct Undecided { pub lines: BTreeSet<LineVersion>, pub names: BTreeSet<(MergeFileIdentity, EventNumber)> }
pub(super) type PriorDecisions = BTreeMap<Version, Version>;
pub(super) fn land(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, prior: &PriorDecisions) -> Result<Decision>;  // verdicts, displace, place, whole
pub(super) fn undecided<L: Landed + ?Sized>(fork: &Snapshot, landed: &mut L) -> Result<Undecided>;
pub(super) fn ended_files(upstream: &Snapshot, undecided: &Undecided) -> Result<BTreeSet<MergeFileIdentity>>;
/// undecided (given), placing over every file either side holds, then names to a fixed point over the placed
/// files; a live name left with no placed file is Error::Format, as today's `(None, None)`.
pub(super) fn sync(upstream: &Snapshot, fork: &Snapshot, undecided: &Undecided,
                   last_lines: &BTreeMap<MergeFileIdentity, Lines>) -> Result<Snapshot>;
pub(super) fn touches(before: &Snapshot, after: &Snapshot) -> BTreeSet<queue::Key>;   // a step's lock keys (R9.3)
```

`tree_reconcile` loses its public `verdict`, `Verdict`, `land_save`, `Landing`, `Collision`, `CollisionKind`, `sync`
(G4); their nine tests move to `referee/`, same cases. **Proof: a differential test.** `#[cfg(test)] referee/oracle.rs`
keeps slice 1's `judge`, `sync::sync` and every callee slice 2 deletes (~900 lines), verbatim.
`land_and_sync_equal_the_oracle` draws 20,000 seeded cases (splitmix64): 1-4 files through `align` and `Versions`, then
an upstream and an incoming save of 0-3 edits (edit, insert, delete, create, rename, unlink, rename-over), sometimes
over an earlier losing save; equal `Decision`s (collision order included) and sync snapshots, the failing seed printed,
and **fails unless Apply, Drop, Lose, each `CollisionKind`, a join, a rename-over and an ended-file edit each occur 50
times**. PR 5.1's last commit deletes both after their green run (a second copy on `main` is a dual path). Step 6's
"A" (G5) removes the model's landing-only gate in `joins`, so the model's placing and `place` take one shape.

### Slice 4: the modules take their own state (changed in round 2)

```rust
// `pub(super)` in a nested file below means `pub(in crate::tree_actor)`: the actor composes every module.
// queue.rs: `mod queue` (was `pub mod`; nothing outside tree_actor names it). Plan.source: TreeId (5b).
pub(super) enum Key { File(MergeFileIdentity), Name(String) }   // was File(u64): same order
pub(super) fn eligible(plans: &[Plan], open: &BTreeSet<Key>) -> Option<usize>;      // P1, unchanged
/// Fields private: only `cut` makes one, and only a Cut builds a sync (R9.3, R9.9).
pub(super) struct Cut<T> { taken: Option<T>, remaining: bool, keys: BTreeSet<Key> }
impl<T> Cut<T> { pub fn taken(&self) -> Option<&T>; pub fn remaining(&self) -> bool;
                 pub fn keys(&self) -> &BTreeSet<Key>; pub fn into_taken(self) -> Option<T>; }
/// Pulls `steps` oldest first, none after the first whose keys meet `open`. The caller's `scan` over
/// `History::ahead_of` carries the previous snapshot for `referee::touches(prev, next)`.
pub(super) fn cut<T, E>(steps: impl IntoIterator<Item = Result<(T, BTreeSet<Key>), E>>, open: &BTreeSet<Key>)
    -> Result<Cut<T>, E>;
pub(super) fn held(tree: &Tree, open_objects: impl IntoIterator<Item = u64>) -> BTreeSet<Key>;

// history/upstreams.rs: 5b's upstream map and open checks, out of mod.rs
/// Each tree's upstream, any depth; Root has none. One level is a limit of `Volume::fork()`, not of this type.
pub(super) struct Upstreams(BTreeMap<TreeId, TreeId>);
impl Upstreams {
    /// Error::Format unless `trees` holds Root, Root has no upstream, and every upstream is in `trees`.
    pub(super) fn recovered(upstreams: BTreeMap<TreeId, TreeId>, trees: &BTreeSet<TreeId>) -> Result<Self>;
    pub(super) fn upstream_of(&self, tree: TreeId) -> Option<TreeId>;
    pub(super) fn forks_of(&self, tree: TreeId) -> impl Iterator<Item = TreeId> + '_;
    pub(super) fn add(&mut self, fork: TreeId, upstream: TreeId);   // the journal's fork record: fold and install
    pub(super) fn iter(&self) -> impl Iterator<Item = (TreeId, TreeId)> + '_;   // checkpoint
}

// history/refs.rs: git-model.md §4's layout, its one home. Prefixes end in '/': `fork/7/` never matches `fork/70/`.
pub(super) const ALL: &str = "refs/";                       // check_objects
pub(super) const QUEUES: &str = "refs/queue/";              // the queued metric
pub(super) fn branch(tree: TreeId) -> String;               // refs/heads/<tree>
/// Every branch's tree and head; Error::Format for a `refs/heads/` name that is no TreeId (5b's fold check).
pub(super) fn branches(pointers: &Pointers) -> Result<BTreeMap<TreeId, Oid>>;
pub(super) fn queue_ref_prefix(tree: TreeId) -> String;     // refs/queue/<tree>/
pub(super) fn queue_entry(tree: TreeId, event: EventNumber) -> String;   // <prefix><event:020>
pub(super) fn collision(tree: TreeId, loser: Oid) -> String;            // refs/collisions/<tree>/<loser>
pub(super) fn object_table(tree: TreeId) -> String;                     // refs/agentfs/objects/<tree>
/// Branches, the root's queue (only it holds landings: the other one-level assumption), collision refs.
pub(super) fn owner_tips(pointers: &Pointers) -> Vec<Oid>;

// history/message.rs: every message the tree actor writes, and every trailer it reads
/// A direct save's subject word (probe 5). M2's directories and symlinks add variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action { Create, Rename, Unlink, Direct }
pub(super) enum Message<'a> {
    Root,   // tree `root`, event 0, NO Agentfs-Format (its absence marks the Root); no tree field (G3)
    /// "{action} by {writer}"; Agentfs-Cause: Direct; Agentfs-Open when the save closed an open.
    Direct { action: Action, writer: &'a str, tree: TreeId, event: EventNumber, open: Option<u64> },
    Landing { tree: TreeId, event: EventNumber, collisions: &'a [Collision] },
    Sync { tree: TreeId, event: EventNumber },   // `tree` is a fork: the root has no upstream
}
impl Message<'_> {
    /// Today's order (Writer, Cause, Tree, Event, Format, then Open or Collisions) from one private function every
    /// variant shares: a trailer every commit gains (M1's "where") is one edit there. Err: JSON only.
    pub(super) fn render(&self) -> Result<String>;
}
/// `Agentfs-Tree`: None if absent; Error::Format if it names no tree (5b's `tree_trailer`, moved).
pub(super) fn tree_of(message: &[u8]) -> Result<Option<TreeId>>;
/// The event of an `Agentfs-Cause: Direct` commit; None for any other, as `direct_events` skips them today.
pub(super) fn direct_event(message: &[u8]) -> Option<EventNumber>;
/// Every `Agentfs-Collisions` line's entries in order; empty if none; a parse failure is today's error.
pub(super) fn collisions(message: &[u8]) -> Result<Vec<Collision>>;
pub(super) fn is_envelope(message: &[u8]) -> bool;                    // snapshot_at and checkout

// tree_git.rs (public; callers: tree_actor, tests/tree_git.rs): git children and files, no layout, no trailer
impl TreeGit {
    /// Replaces `config` and `HEAD` atomically, as today. `head`: the full ref; `merges`: section → full ref.
    pub fn write_config(&self, head: &str, merges: &BTreeMap<String, String>) -> Result<()>;
    /// Each commit reachable from `tips` with its raw message, in one git child (was `direct_events`).
    pub fn commit_messages(&self, tips: &[Oid]) -> Result<Vec<(Oid, Vec<u8>)>>;
}

// history/mod.rs
pub(super) struct History { repo: TreeGit, owners: BTreeMap<EventNumber, Oid>, upstreams: Upstreams }
impl History {
    pub(super) fn landed_into(&self, upstream: Oid) -> impl Landed + '_;   // cached per commit
    pub(super) fn prior_decisions(&self, head: Oid) -> Result<PriorDecisions>;
    pub(super) fn last_lines(&self, at: Oid, wanted: BTreeSet<MergeFileIdentity>) -> Result<BTreeMap<MergeFileIdentity, Lines>>;
    pub(super) fn ahead_of(&self, upstream_head: Oid, fork_head: Oid) -> Result<Vec<Oid>>;  // oldest first
    /// R6.1: a save of `fork` waits in its upstream's queue. Finds upstream, prefix and each entry's tree itself,
    /// so finding 51's class (a wrong prefix lets a sync fast-forward) is one function to test.
    pub(super) fn pending_from(&self, pointers: &Pointers, fork: TreeId) -> Result<bool>;
    pub(super) fn rebuild_owners(&mut self, pointers: &Pointers) -> Result<()>;   // owner_tips, direct_event
    pub(super) fn write_config(&self) -> Result<()>;   // HEAD → branch(Root); each fork merges branch(upstream)
    pub(super) fn saved(&mut self, event: EventNumber, head: Oid);          // install, cannot fail
    pub(super) fn upstreams(&self) -> &Upstreams;
}

// journal/{mod,fold,pointers}.rs: a DTO of strings on disk; the fold parses trees (rule 4).
//   `Folded { upstreams: BTreeMap<TreeId, TreeId>, .. }`; opens, page_writes, saved_through as `FoldedRecording`.

// recording/{mod,pages}.rs: holds no Journal (design rule 2); every tree a TreeId (rule 4)
pub(super) struct Recording { handles: BTreeMap<u64, Handle>, pending_closes: Vec<u64>,
                              pages: BTreeMap<(TreeId, u64), ObjectPages>, changed: Vec<(TreeId, u64)> }
impl Recording {
    /// Recovery (785-813): opens and page writes before their closes run; Format for an unknown tree.
    pub(super) fn recovered(folded: FoldedRecording, trees: &mut BTreeMap<TreeId, Tree>) -> Result<Self>;
    pub(super) fn checkpoint(&self, trees: &BTreeMap<TreeId, Tree>) -> (Vec<OpenRecord>, Vec<(TreeId, u64, u64)>);
    pub(super) fn retains(&self, entry: &Entry, seq: u64) -> bool;   // compaction (1994-2028), with checkpoint
    pub(super) fn note_changed(&mut self, tree: TreeId, before: &Bytes, after: &Bytes);   // after a take installs
    pub(super) fn drain_changed(&mut self) -> Vec<(TreeId, u64)>;
    pub(super) fn prepare_call(&self, handle: u64, call: &Call, saved: &[u8]) -> Result<(CallHead, Vec<u8>)>; // no change
    pub(super) fn called(&mut self, handle: u64, seq: u64, call: Call, view: Vec<u8>);  // journalled as seq; infallible
    pub(super) fn open_objects(&self, tree: TreeId) -> impl Iterator<Item = u64> + '_;
    // Edits on a copy of a Tree, each giving the Draft its tree: create, close (Changed | Unchanged | Orphan |
    // Anonymous), rename, unlink, first_link; each takes `versions: &mut Versions`.
}
```

`Volume` keeps its post-5b API; a step builds a `Draft` (an edit or a decision, plus history's commit and refs),
records it, installs it. **Owners:** `tree_reconcile` line maps; `tree.rs` shared and version types; `recording/`
close-to-open, calls, orphans, R1.4; `queue.rs` P1, `Cut`, held keys; `referee/` rule 3, R3a-e, R10, R11, touches;
`history/` commits, every trailer, R6.1, upstreams, refs; `journal/` commit point, fold, compaction; `mod.rs` composes.

**Crossings.** R9.3: history walks, referee `touches`, queue cuts. R11.3: `EventNumber::object`, recording takes it.
Undecided: `Landed`. Recovery: journal folds and parses, `refs::branches` and `Upstreams::recovered` check, history and
recording take their parts, the actor closes abandoned handles. Takes: `take` wraps `step`, then `note_changed`.

## 6. Where the one-engine milestones land (G2)

| Milestone | Lands in |
|---|---|
| M1 volume from configuration (`ensure`) | `mod.rs` (`Volume`), `journal/` (what the state area stores); the "where" trailers (one-engine Q3) in `Message`'s shared function |
| M2 directories, symlinks, metadata | `recording/`, `referee/names.rs`, `Action`, `history/envelope` |
| M3 rules, untracked area | `recording/` (which paths are recorded) |
| M4 mirrors, readable history (with step 6) | `history/` (envelope reader, `is_envelope`); the publisher reads `refs::branch(TreeId::Root)` |
| M5 telemetry; recorder source B (module-cut's engine-source row, homed here) | emit at `install` (each `Draft`), `History::saved`, and `run_step`'s and `prepare_call`'s errors (` !<errno>`) |
| M6 patches, restore, revert | `history/` (commits through `Message`), `referee/` (a producer's landing) |
| M9 deletes the recording engine | none of these: they are the engine that stays |

## 7. Risks

| Risk | Met by |
|---|---|
| Recording (principle 1): an append moves after a memory change | Rule 2; byte calls keep check, journal, memory (1066-1080) as `prepare_call`, `journal_record`, `called`; crash, recording-fault and power-loss jobs in the dispatch |
| R3e's winner or order changes; conformance skips whole winners (`invariants.rs:433`) | Rule 3; probe 4; the differential test compares `Decision`s |
| A message or blob changes bytes (G4); the Root stops reading as Root | Probe 1; `Message::Root` has no `Agentfs-Format`; goldens per variant and blobs (`metadata.json`, `namespace.json`) from a **post-5b** build, never a state area |
| A moved trailer reader reads differently | One `message` function each, tested on the goldens; 5b's `pending_from` test, now through `History` |
| Frame order places a file sync then hides; `Held::live` read before `Drop` | Equal results (differential test); only corrupt data fails earlier, EIO either way |
| `TreeGit`'s two public signatures change | Callers: `tree_actor`, three calls in `tests/tree_git.rs` |
| `Cut` reads more; `one_spawn_site` on a subdirectory (probe 3); finding-51's header | Counting-iterator test; slice 2 recurses; slice 3 drops the clause |
| `tree_actor::recording` misread as "the recording engine" until M9 | Its doc comment says so |

## 8. Slices, PRs and gates

5b first gives `TreeId`, so no literal `main` can be re-created unseen. Each PR starts from main after the last merges.

| PR | # | Slice (one commit each, pushed; ci and image green on each SHA) | Size (estimate) |
|---|---|---|---|
| 5.1 | 1 | **Tracer: typed versions** through JSON, `tree_reconcile`, referee, sync, actor; `Versions`; golden format test | ~300 lines |
| 5.1 | 2 | Rules once, `referee/` units in frame order; `sync.rs` folds in; nine tests moved; oracle and differential test, then a commit deleting both; `one_spawn_site` recurses; rows (`docs_reference`) | ~800, net fewer |
| 5.2 | 3 | Verbatim moves to `recording/`, `history/`, `journal/`, `tree.rs` (visibility may widen); rows; finding-51 header | ~2,400 moved |
| 5.3 | 4 | `Cut`, `held`, `Upstreams`, `refs`, `Message` and its readers, `TreeGit`'s two signatures, `Folded`, recording crossings, `mod queue` | ~950 lines |

Each slice: fmt, `clippy -D warnings`, `cargo test --workspace`; ci and image. **PR gates.** 5.1 and 5.3 each get one
`phase-1.yml` dispatch on the PR head (replay, mounts, seeds 1-500, recording faults, power loss, 0 mismatched; step
4's run 43146523186 took 73 min): 5.1's proves the rewrite, 5.3's is the step gate (G1). 5.2: ci, image, and
`git diff --color-moved=dimmed-zebra` showing nothing beyond `use`, `mod`, visibility. **Model gate: none.**

**Why before M1.** This design is done against post-5b code; M1's is not, so M1's architect designs on the six
modules while 5 is built, and M2, M3, M4, M6 extend modules 5 makes (§6). **Costs.** Three PRs, two dispatches (~146
min), three merges delay M1's build; one ~4,500-line PR saves a dispatch, loses 5.2's "moves only" check.

## 9. Docs

`architecture.md` rows (`tree_reconcile.rs` generic over the caller's version; `tree_git.rs` no layout); `library/core.md`
(`tree_reconcile` is the public line-map library); `trees.md` "The library"; rules-ledger R12; `shared-mappings.md`;
`module-cut.md` row 5 built, "a landing's collision order is a key". No page is deleted: step 7 deletes module-cut.md.

## 10. Probes (real output on 22d55f9; patches in `next/probe-5/`)

(1) Typed `LineMap` reread `align`'s bytes: values equal, keys `{"9:1",..,"10:1",..}`; `r1` refused. (2) Rebuilt candidates equal `judge`'s; generic `verdict` equals `name_verdict` (28 files, 19 name verdicts).
(3) `one_spawn_site` panics on a subdirectory. (4) `judge`'s candidates sorted by `Order` on every call
(`probe4-key-order.diff`; 93 tests; 38 decisions, 48 candidates, 16 with a loss). (5) Root (675) lacks
`Agentfs-Format`; 1489, 1670, 2188 have it; Direct's subject is `{cause} by {writer}` for Create, Rename, Unlink,
Direct.

## 11. Owner questions

None; every choice is decided and revisable. What "where" a commit records is one-engine Q3, built at M1; step 5 takes
any answer.

| New term | Means | Why needed | Existing term it could be |
|---|---|---|---|
| `EventNumber` | A step's journal number; numbers versions and objects | One `u64`, five roles | "event number" |
| `LineVersion`, `Versions` | One line version; a save's allocator | Strings parsed late; two closures | "line version"; `next_id` |
| `Graph`, `descends`, `Broken` | Ancestry, its walk, its failures | Three walks | "ancestry" |
| `Held`, `Verdict<V>`, `Candidate`, `Order` | Rule 3's input and answer; a version, its loss, its place | Two verdicts; R3e twice | judge's `candidates` |
| `Landed` | The referee's one read of history | Pure referee | the model's `Lineage` |
| `Upstreams` | Each tree's upstream, by `TreeId` | 5b's map and checks need a home | the journal's `upstreams` |
| `refs` | The ref layout's one home | Fourteen literals in two files | git-model §4 |
| `Message`, `Action` | Every commit message; a direct save's subject word | Four writers, six readers | the trailers; "cause" |
| oracle | Old code in one commit's tests, compared with the new | Slice 2's proof | none |

**NOT RUN:** the differential test (unwritten; 20,000-seed runtime unmeasured); probes 1-5 after 5b (unbuilt); any
compile of these signatures (forbidden on this machine during the model gate); any model run; conformance,
`phase1-replay`; FUSE, mounts, crash, power-loss; clippy, fmt on probes; git 2.43, not 2.47.3.

**Reconcile (`plan.md`, F2, F4, F5, F7, F9).** "Where" is M1's (one-engine Q3); one order, 5 before M1 (§8); source
B homed at M5; step 7 deletes module-cut.md; the "-r2" box is gone.
