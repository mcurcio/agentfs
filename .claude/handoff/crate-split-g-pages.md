# Crate split, PR G: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row G, as the skeleton rewrote it; section 1;
section 4's items routed to G). Branch `claude/bold-hopper-72ixu2`, PR #47, base of this run 0aea0f8. Two slices,
G1 then G2, in order: G2 builds on the line values G1 moves into agentfs-lines and on G1's two cuts, and both edit
the engine. Owner's words (OWNER-GUIDANCE.md "Performance", general guidance): "Just two? Can we break it down more
along domain lines?" (2026-11-24); carrying D to G on PR #47: "Yes" (2026-11-25). Counts are `git grep` at 0aea0f8.
A to F idioms hold: ranks, agentfs-repo's checks, no shims or second paths, docs in the same commit, commits naming
their paths. No design review ran in this step (the skill: the code architect self-checks; Decisions, "Self-check").
**The skeleton** (this file's commit): `crates/agentfs-referee/{Cargo.toml,src/lib.rs}` (rank `domain`, no
dependencies, module doc and `#![cfg_attr(not(test), no_std)]` only); the member; the rank `domain`'s uses gain
`line-identity` (manifest, agentfs-repo's `ranks()` fixture, architecture.md's Ranks table); the front's `pub use
agentfs_referee as referee;`, its dependency and its doc sentence; `Cargo.lock`; the Dockerfile's stubs;
architecture.md's Crates row and module-map section; crate-split.md row G and section 4 (the decisions below).

## Page G1

**Title.** The line values live in agentfs-lines; the referee's two upward edges are cut; the library gives each
item one path (checked); the engine's open items closed.
**Line values** (tree.rs:22-205 → `crates/agentfs-lines/src/version.rs`, private module; lib.rs gains `pub use
version::{EventNumber, LineVersion, Lines, NotALineVersion, Versions};`). Bodies move unchanged; visibility widens
from `pub(super)` to `pub` because the engine and G2's referee both name them.
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)] #[serde(transparent)]
pub struct EventNumber(u64);
impl EventNumber { #[inline] pub const fn new(seq: u64) -> Self; #[inline] pub const fn get(self) -> u64;
                   #[inline] pub const fn object(self) -> u64; }          // + Display, as today
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)] #[serde(into = "String", try_from = "String")]
pub struct LineVersion { event: EventNumber, index: u64 }                 // fields stay private
impl LineVersion { #[inline] pub const fn event(self) -> EventNumber; }
// Id (at, writer, index), Ord and PartialOrd (the texts' byte order), Display, From<LineVersion> for String:
// as today, each method #[inline], and the private `text` and `digits` #[inline].
impl FromStr for LineVersion { type Err = NotALineVersion; }  impl TryFrom<String> for LineVersion { type Error = NotALineVersion; }
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a line version: `<event>:<index>`, decimal, no sign or leading zero, index from 1")]
pub struct NotALineVersion(pub String);
pub struct Versions { event: EventNumber, last: u64 }
impl Versions { #[inline] pub const fn of(event: EventNumber) -> Self; #[inline] pub fn take(&mut self, n: u64) -> LineVersion;
                #[cfg(test)] pub fn next(&mut self) -> LineVersion; }
pub type Lines = LineMap<LineVersion>;
```
`NotALineVersion` replaces the engine's `Error::Format` here; it is read only through serde (a `serde_json::Error`, so
the engine's `Error::Json` as before) and by tests. **Performance:** the values cross a crate boundary without release
`lto`, and `LineVersion`'s `Ord` is hot (the builder's and the referee's maps of versions): hence the `#[inline]`s;
image.yml's release figures for the final SHA are read against 0aea0f8's (AGENTS.md: a miss but `create`'s warns).
**Cuts.** (1) `queue::held` (queue.rs:50) → `impl Shape { pub(super) fn held(&self, open_objects: impl
IntoIterator<Item = u64>) -> BTreeSet<queue::Key> }` in tree.rs, beside `keys_of`, whose body it repeats (each
named open object's `keys_of`); takes.rs:278 calls `t.held(..)`; queue.rs no longer imports `Shape`. (2)
`Upstreams::merges` (upstreams.rs:66) → refs.rs, the ref layout's home: `pub(in crate::tree_actor) fn
merges(upstreams: &Upstreams) -> BTreeMap<String, String>`, over a new `pub(in crate::tree_actor) fn pairs(&self) ->
impl Iterator<Item = (TreeId, TreeId)> + '_` (each fork and its upstream, by fork); builder.rs's two calls (1774,
1849); upstreams.rs no longer imports `refs`.
**One path.** tree_actor/mod.rs:45 `pub use agentfs_contracts::{NotATree, TreeId};` becomes a private `use`; every
import of either through the engine names `agentfs_contracts::{TreeId, NotATree}` (the front's doctest's form):
engine src about 30 lines in 25 files (`super::TreeId`, `crate::tree_actor::{.., TreeId}`), engine tests 19 files,
agentfs-cli 3 (launcher/host.rs, launcher/mod.rs, tests/cli/cli_verify.rs), agentfs-conformance 3 (engine_names.rs,
mounted.rs, library.rs), agentfs-fuse 2 (tree_view.rs, save_worker.rs), docs/library/engine.md:186. New check
(agentfs-repo `crates.rs`): `the_library_gives_each_item_one_path`: no production source (`production`, the spawn
check's helper) of a crate the front re-exports (its normal dependencies) has a line matching `^\s*pub use
(::)?agentfs_`; `fn reexport_problems(name: &str, source: &str) -> Vec<String>` with a fixture test
`a_second_path_is_found` (`pub use agentfs_contracts::TreeId;` found; `pub(crate) use` and one inside `#[cfg(test)]
mod t` not). Not read: an alias of a crate (`use agentfs_contracts as c; pub use c::TreeId;`) and the leaves
(`agentfs_lines` re-exports align's `Hunk`, `agentfs_git` `GIT_PATH`, by design).
**Engine items.** `Error::Reconcile(#[from] agentfs_lines::Error)` → `Error::Lines(#[from] agentfs_lines::Error)`,
errno arm as before (mod.rs:183, :279). JC6 and JC8 join `journal/compaction.rs`'s module doc, from their citations
and latency-upkeep.md's C6 and C8 (git show 1e45703^): **JC6.** an unlinked segment a holder still reads stays
readable through its `Arc` and counts in `Durability::len` until the last holder lets go, never in `len()` here;
**JC8.** every field of the checkpoint has one named source at `built` (the table on `Checkpoint`): History's state
read in one hold, and the fold of the records at or below it. Test-only `pub` items, by F2's rule (no production
caller: `#[cfg(any(test, feature = "test-util"))] pub`): `History::wait_compacted` (and the field `compacted`, its init
and its write, which only it reads), `History::wait_compactor`, `stages::holds_on_this_thread`. Found by a scan of every
`pub` item of the engine's production text whose name no other production text names; a `pub` method sharing its name
with a production item elsewhere is not separated by the scan (stated in engine.md, "Test builds").
**Tests.** tree.rs's `line_versions_order_as_their_text`, `only_canonical_text_parses`, `versions_count_from_one` and
`v()` → version.rs; queue.rs's `held_keys_are_an_open_objects_identity_and_names_and_none_unnamed` → tree.rs's tests
(calls `shape.held`); upstreams.rs's `merges` assertion → refs.rs's `merges_names_each_forks_upstream_branch`.
**Docs.** architecture.md: lines' `version.rs` row; engine rows `tree.rs`, `queue.rs`, `history/refs.rs`,
`history/upstreams.rs`. engine.md: example's `TreeId`, the compaction table's `wait_compacted` and "Test builds".
agentfs.md "Not re-exported": `Error::Lines`. development.md: the new check in agentfs-repo's list. module-cut.md row 5.1's
`tree.rs` path. CHANGELOG.
**Acceptance.** `git grep -nE '(struct|type) (EventNumber|LineVersion|Versions|Lines)\b' -- crates/agentfs-engine` and
`git grep -n 'Reconcile(' -- crates/agentfs-engine` are empty; `git grep -nE 'fn (held|merges)' --
crates/agentfs-engine/src/tree_actor/queue.rs crates/agentfs-engine/src/tree_actor/history/upstreams.rs` is empty;
`git grep -nE '^\s*pub use agentfs_contracts' -- crates/agentfs-engine` is empty. Gates: fmt; clippy `--all-targets -D
warnings` and nextest of -lines, -engine (also `--features failpoints`), -fuse, -cli (also failpoints), -conformance,
-service, -repo; CI's two library steps (`cargo clippy --locked -p agentfs-engine -p agentfs-service -p agentfs --lib --
-D warnings`, `cargo test --doc --locked -p agentfs`); `cargo build -p agentfs-conformance --bin phase1-replay`.
**Probes.** (1) LineVersion's `Ord` derived (numeric) → `line_versions_order_as_their_text`. (2) the engine's `pub
use agentfs_contracts::TreeId;` restored → `the_library_gives_each_item_one_path`. (3) `Shape::held` keeps only its
first object → the moved held test. (4) `refs::merges` maps to `refs::head()` → the moved merges test. (5)
`holds_on_this_thread()` called in agentfs-service's `src` → CI's library clippy step (E0425).
**Files.** `crates/agentfs-lines/src/{lib.rs,version.rs}`; `crates/agentfs-engine/**`; the import lines named in
-cli, -conformance, -fuse; `crates/agentfs-repo/tests/repo/crates.rs`; the docs named.

## Page G2

**Title.** agentfs-referee holds the landing rules, the queue, the fork shape and their values, `no_std` outside its
tests; its purity is checked three ways; the crate split's page keeps only what remains.
**The crate** (`crates/agentfs-referee`, rank `domain`). Deps: agentfs-contracts (`TreeId`, `FileOwner`), agentfs-lines
(the line maps and values), `serde` (derive), `thiserror`; dev `serde_json`. No dev-dependency on agentfs-git: the
tests' spawner is `std::process::Command::spawn` (tests have std). lib.rs: `#![cfg_attr(not(test), no_std)]`, `extern
crate alloc;`, private modules, `pub mod queue;`, the rest re-exported at the root (the engine's idiom: one path each).
```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the history the landing rules read is malformed")] Format,   // every `Error::Format` of referee/ today
    #[error(transparent)] Ancestry(#[from] Broken),                       // Broken: Cycle | Missing, as today
    #[error(transparent)] Lines(#[from] agentfs_lines::Error),            // Lines::from_expanded and the maps
}
pub type Result<T, E = Error> = core::result::Result<T, E>;
/// History's answers; its own failure arrives as the caller's error, the rules' converted into it.
pub trait Landed { type Error: From<Error>;
    fn landed(&mut self, save: EventNumber) -> Result<bool, Self::Error>;
    fn record_landed(&mut self, record: Record<'_>) -> Result<bool, Self::Error>; }
pub fn undecided<L: Landed + ?Sized>(fork: &Snapshot, upstream: &Snapshot, landed: &mut L) -> Result<Undecided, L::Error>;
pub fn land(upstream: &Snapshot, base: &Snapshot, incoming: &Snapshot, prior: &PriorDecisions) -> Result<Decision>;
pub fn sync(upstream: &Snapshot, fork: &Snapshot, undecided: &Undecided,
            last_lines: &BTreeMap<MergeFileIdentity, Lines>) -> Result<Synced>;
pub fn touches(before: &Snapshot, after: &Snapshot) -> BTreeSet<queue::Key>;
pub fn consulted(base: &Snapshot, incoming: &Snapshot) -> Result<BTreeSet<queue::Key>>;
pub fn ended_files(upstream: &Snapshot, undecided: &Undecided) -> Result<BTreeSet<MergeFileIdentity>>;
pub fn with_directories(keys: impl IntoIterator<Item = queue::Key>) -> BTreeSet<queue::Key>;
pub fn parent_of(path: &str) -> &str;      // tree.rs:787; tree.rs imports it
```
`pub` at the root, shapes unchanged (fields `pub` as today): `Snapshot<L = Lines>`, `File<L = Lines>`, `Decision<L =
Lines>`, `Version`, `CollisionKind`, `Collision`, `PriorDecisions`, `Synced`, `Cause`, `Record<'a>`, `Undecided`, `Broken`;
`MergeFileIdentity(pub u64)`, `NameHistory { pub head, pub versions }`, `NameVersions`, `NameVersion { pub path, pub
over, pub replaced_by }` and their methods (`pub`, the small accessors `#[inline]`); `Kind`, `Perm`, `Attrs`, `Meta`
(as meta.rs today; `Perm: TryFrom<String, Error = NotAPerm>`, `pub struct NotAPerm(pub String)`, `#[error("{0:?} is not
a permission: four octal digits")]`); `Upstreams` (its methods `pub`, with G1's `pairs`). `queue`: `Key`, `Plan`,
`eligible`, `Cut` and its methods, `cut`, `pub`. Crate-private: `ByLine`, `Expanded`, `by_line`, `stored`, `Verdict`,
`ancestry`'s `Graph`, `Ancestors`, `descends`, `union`; `test_bytes` and `TEST_FILE` under `#[cfg(test)]`.
**Engine.** `impl From<agentfs_referee::Error> for Error`: `Format | Ancestry(_)` → `Format`, `Lines(e)` → `Lines(e)`
(today's mapping); `From<referee::Broken>` goes; `impl Landed for LandedInto { type Error = Error; }`; unit test
`referee_errors_map_to_the_engines` (mod.rs). `mod referee; mod queue;` and `history/upstreams.rs` go; meta.rs keeps
`Times`, `SetTime` and its own `#[cfg(test)] TEST_FILE` (`Perm::from_mode(0o644)`); `pub use meta::{SetTime, Times};`.
**Move map.** `tree_actor/referee/mod.rs` → `src/lib.rs`; `referee/{ancestry,entries,names,placing,undecided,verdicts,
whole}.rs` → `src/`; `tree_actor/queue.rs` → `src/queue.rs`; `history/upstreams.rs` → `src/upstreams.rs`; meta.rs's
`Kind`, `Perm`, `Attrs`, `Meta` → `src/meta.rs`; tree.rs's `MergeFileIdentity`, `NameHistory`, `NameVersions`,
`NameVersion`, `parent_of` → `src/values.rs`. Each test module with its code; tree.rs's `name_versions_keep_event_order`,
`the_stored_text_is_unchanged`, `combine_keeps_..`, `a_name_is_ended_..` and meta.rs's `meta_is_its_stored_json_..`,
`only_four_octal_digits_..` move; `a_save_keeps_the_atime` stays.
**Consumers.** engine src: `referee::X` → `agentfs_referee::X` (about 55 lines), `use super::queue` →
`agentfs_referee::queue` (6 files), the meta values (about 20 lines in 18 files), the values from tree.rs (14 files);
engine tests 9 files; agentfs-fuse `tree_view.rs`, `untracked.rs` (+ normal dep); agentfs-conformance `library.rs` (+ dep);
agentfs-cli `tests/cli/cli_verify.rs`, agentfs-service `tests/volume.rs` (+ dev-deps); the front's doctest
`agentfs::referee::{Attrs, Perm}`; docs/library/engine.md:186.
**Checks.** (1) `no_std`: production code naming `std` fails to compile. (2) agentfs-repo `crates.rs`:
`the_pure_crates_are_no_std` over `const PURE: &[&str] = &["agentfs-referee"]`: its `src/lib.rs` holds
`#![cfg_attr(not(test), no_std)]` and no source's production text says `extern crate std`; `fn purity_problems(name:
&str, lib_rs: &str, sources: &[(String, String)]) -> Vec<String>`, fixture test `a_pure_crate_that_reaches_std_is_found`.
(3) the spawn check reads `agentfs-referee/src` too (a spawner closure handed to lines compiles under `no_std`). (4)
`ci/check-deps.sh`: `ok   referee`, failing when `cargo tree --locked -p agentfs-referee -e normal` has `git2`,
`libgit2-sys`, `libc`, `rustix` or `opentelemetry`; header comment says so. Not read: I/O through a dependency's API
other than a spawn (lines' `FileImage::open` with an inferred `&Path`).
**Docs.** New `docs/library/referee.md` (what it decides, its calls, the `Landed` port, errors, purity and its checks,
tests in seconds) in SUMMARY.md and docs/README.md; agentfs.md's module row and "Not re-exported" (the line values in
the referee's API); architecture.md: the referee's map (lib, ancestry, entries, meta, names, placing, queue, undecided,
upstreams, values, verdicts, whole), the engine loses those rows, `meta.rs` and `tree.rs` rows; AGENTS.md Layout row;
development.md: the checks, `ok   referee`, and "Build settings" `cargo nextest run -p agentfs-referee` cold and warm
[measured]; CHANGELOG. crate-split.md keeps only what remains (Decision 8): section 1, M4, M9, "Later", what is not
delivered; the Traps' "not read" lines move to development.md beside each check; references at check-deps.sh:10,
crates.rs:438, the front's Cargo.toml:15 and target-architecture/README.md:61 follow.
**Acceptance.** `crates/agentfs-engine/src/tree_actor/{referee,queue.rs,history/upstreams.rs}` gone; `git grep -n
'git2\|agentfs-git' -- crates/agentfs-referee/Cargo.toml` empty; the Traps prose grep, every hit read. Gates: fmt;
clippy `--all-targets -D warnings` and nextest of -referee, -lines, -engine (also failpoints), -fuse, -cli (also
failpoints), -conformance, -service, -repo; CI's two library steps; `bash ci/check-deps.sh`; phase1-replay builds.
**Probes.** (1) `let _ = std::fs::read("x");` in a referee function → `cargo build -p agentfs-referee` E0433. (2) the
`no_std` line deleted → `the_pure_crates_are_no_std`. (3) `git2 = { version = "0.21.0", default-features = false }` in its
dependencies → check-deps `referee links git2 ..`. (4) `agentfs_lines::histogram_with(a, b, |c| c.spawn())` in a
referee function → the spawn check. (5) the engine's From maps `Lines(_)` to `Format` → `referee_errors_map_to_the_engines`.
**Files.** `crates/agentfs-referee/**`; `crates/agentfs-engine/**`; Cargo.toml of -fuse, -conformance, -cli, -service;
the consumer files named; `crates/agentfs/src/lib.rs`; `crates/agentfs-repo/tests/repo/crates.rs`; `ci/check-deps.sh`;
`Cargo.lock`; the docs named.

## Decisions

**Slice order.** G1 first, package-depth first: agentfs-lines gains the values the referee's signatures carry, and the
engine's two upward edges are cut while the code is still in one crate, so G2 is moves and imports. Neither slice can
fan out: both edit the engine.

**Design choices.**
1. **Rank `domain`**, whose uses gain `line-identity`. The domain rank already holds "the tree views' rules"
   (agentfs-rules); the referee needs contracts (`TreeId`, `FileOwner`) and lines, and the front must re-export it,
   since the engine's calls take `Attrs`, `Kind`, `Meta`, `Perm`. A rank of its own was weighed: one more row, and
   `umbrella` would use it anyway. Cost: `only_the_service_composes_the_domains` derives its domains from the
   service's dependencies, so the service may never depend on the referee or rules without that check refusing the
   engine's and fuse's edges (stated in development.md beside the check).
2. **Deviation from row G: `no_std`, not a `clippy.toml`.** Verified on this toolchain (clippy 0.1.100, scratch
   workspace): a crate's own `clippy.toml` replaces the workspace's (the root's 66 bans, `Minter::new` among them, stop
   applying), and clippy bans no module ("expected a type, found a module"), so "std::fs banned" would be some 40
   entries that a new std function slips past, plus a copy of the root's that drifts. `#![cfg_attr(not(test),
   no_std)]` is compile-time and covers fs, process, net, env, io and threads at once; tests keep std. Verified: a
   `no_std` crate depends on std crates (lines), and `std::fs::read` in it fails E0433. Its one hole, a spawner closure
   handed to lines, is the spawn check's.
3. **What moves.** Row G's values plus what they need: the queue (owner's 2026-11-08 cut keeps "queue and locks" its
   own module: `pub mod queue`), `Upstreams` (the earlier course put it with the queue; it decides which queue a
   landing joins), `Kind`, `Perm`, `Attrs` with `Meta` (`Meta::new` takes `Attrs`), `NameVersions`, `NameVersion` with
   `NameHistory`, `parent_of` (two sites, tree.rs and entries.rs). `Times` and `SetTime` stay: each tree's own times,
   never in history.
4. **`held` goes into `Shape`**, not takes.rs: up out of the queue as row G says, beside `keys_of`, which it repeated.
   **`merges` goes up into refs.rs**, the ref layout's one home; ref naming does not move down (the referee knows no
   git refs).
5. **Errors.** The referee's own enum; the engine's `From` keeps today's mapping exactly. `Landed` has an associated
   error (serde's `Serializer::Error` idiom), so the engine's git failures cross the port intact, no boxing.
   `Error::Reconcile` keeps `agentfs_lines::Error`, renamed `Lines` for its crate: E's policy holds (a leaf's types
   that reach the API are named by depending on the leaf, agentfs.md), and the referee's API now carries lines' values
   anyway; re-exporting the leaves from the front would make `umbrella` use `line-identity` and re-export align too.
   `NotALineVersion` and `NotAPerm` follow contracts' `NotATree` idiom.
6. **One path, checked.** The re-export goes and agentfs-repo refuses its class in every crate the front re-exports.
7. **finding-51's trace: closed, not re-recorded.** VERIFYING.md re-records "when an unrelated engine change makes the
   second test fail"; it passes, and G does not touch take.rs's guard. Recording needs the pinned git through
   `ci/model-docker.sh`; this container has no Docker daemon and the rails forbid starting one in the background. The
   open question (the re-cut hunk ran past 750 s) has its home in the patch's header, which says so.
8. **crate-split.md after G**: kept, shrunk to what remains (M4's two crates, M9's deletion and the front's `fuse`,
   "Later", what is not delivered), because M4 and M9 build on its end state; the built behaviour (the Traps) moves to
   development.md beside the checks.
9. Pages G1 and G2 run to about 80 lines each, past the 60 asked: their signatures and move maps are the pages.

**Errata C to F, each disposed.** C: finding-51 (Decision 7); the dev-edge rule and domain docs (built in C, E); a
test-name filter is unchecked: closed, a stale filter in a script fails when run (nextest exits 4, "no tests to run"),
one in markdown is the unchecked class development.md names. D: built in D; the prose grep runs in G2's acceptance. E:
the rank, doctest and front (built in E, F); JC6 and JC8 (G1). F: (a), (b), (d), (e), (f) built; (c) the
`allow-invalid` bans stay unchecked, now said in development.md (G2); F2's hand-read list (G1's scan).

**Self-check (goal-oriented).** Row G's ends and the slice that delivers each: purity checked (G2 checks 1-4); rule
tests in seconds (G2's measured row); error enum with `From` (G2); line values (G1); values public (G2); `held` (G1);
merges (G1); `Error::Reconcile` (G1, Decision 5); `TreeId` path (G1); JC6, JC8 (G1); finding-51 (Decision 7); test-only
`pub` items (G1); release figures (G1's `#[inline]`s, read in image.yml); crate-split.md (G2). No goal is unowned.

**New terms.** `pure crate`: one in agentfs-repo's `PURE`, `no_std` outside its tests. `Landed::Error`: the port's own
failure type. `NotALineVersion`, `NotAPerm`: text that is not one, as `NotATree` is for trees.
