# Crate split, PR G: frozen pages (revision 2, after the design review)

Design: `docs/target-architecture/future/crate-split.md` (section 3 row G, as this revision's skeleton rewrote it;
section 1; section 4). Branch `claude/bold-hopper-72ixu2`, PR #47, base of this run 0aea0f8. Two slices, G1 then G2,
in order: G2's referee names the metadata values G1 moves to the shared kernel and uses G1's `Shape::held` cut, and
both edit the engine. Owner's words (OWNER-GUIDANCE.md "Performance", general guidance): "Just two? Can we break it
down more along domain lines?" (2026-11-24); carrying D to G on PR #47: "Yes" (2026-11-25). Counts are `grep` at
0aea0f8. A to F idioms hold: ranks, agentfs-repo's checks, no shims or second paths, docs in the same commit, commits
naming their paths. The design review's twelve findings and their outcome are under Decisions, "Review".
**The skeleton** (this revision's commit): `crates/agentfs-referee/{Cargo.toml,src/lib.rs}` in a new rank
`landing-rules` (uses `line-identity`, `shared-kernel`; `engine` and `tests` use it), no dependencies, module doc and
`#![cfg_attr(not(test), no_std)]` only; `domain` back to `uses = ["shared-kernel"]`; the front's re-export of the
referee, its dependency and its doc sentence removed (the front is 0aea0f8's again); the manifest, agentfs-repo's
`ranks()` fixture, architecture.md's Crates row, Ranks table and module-map section; the Dockerfile's stubs;
`Cargo.lock`; crate-split.md row G, section 1 and section 4.

## Page G1

**Title.** Entry metadata and `parent_of` go to the shared kernel; the engine gives each item one path (checked);
`queue::held` becomes `Shape::held`; the engine's open items close.
**Metadata** (meta.rs:12-103 → `crates/agentfs-contracts/src/meta.rs`, `pub mod meta;`, doc: an entry's metadata as
history records it, docs/trees.md "What a commit holds"). Bodies move; `Perm`'s parse error becomes its own:
```rust
pub enum Kind { File, Symlink, Dir }                       // derives and serde as today
pub struct Perm(u32);                                      // private field; serde into/try_from String
impl Perm { pub const fn from_mode(mode: u32) -> Self; pub const fn bits(self) -> u32; }
impl TryFrom<String> for Perm { type Error = NotAPerm; }   // Display, From<Perm> for String as today
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a permission: four octal digits")]
pub struct NotAPerm(pub String);
pub struct Attrs { pub perm: Perm, pub owner: crate::clients::FileOwner }
pub struct Meta { pub kind: Kind, pub mode: Perm, pub uid: u32, pub gid: u32 }
impl Meta { pub const fn new(kind: Kind, attrs: Attrs) -> Self; pub const fn attrs(&self) -> Attrs; }
```
`Meta::git_mode` stays in the engine as `pub(in crate::tree_actor) const fn git_mode(meta: &Meta) -> u32` in
`history/format.rs`, its one caller (format.rs:165): git's storage mode is the history writer's. The engine's meta.rs
keeps `Times`, `SetTime` and `#[cfg(test)] TEST_FILE` (now `Meta { kind: Kind::File, mode: Perm::from_mode(0o644),
uid: 1000, gid: 1000 }`); `pub use meta::{SetTime, Times};`.
**`parent_of`** (tree.rs:786-789) → `agentfs_contracts::paths::parent_of(path: &str) -> &str`, beside `is_dotgit`;
fuse's identical `parent_dir` (tree_view.rs:714-717) is deleted and its caller (:1240) uses it.
**One path.** mod.rs:45 `pub use agentfs_contracts::{NotATree, TreeId};` becomes a private `use`; `Attrs, Kind,
Meta, Perm` leave mod.rs's `pub use meta::..`. Every import names `agentfs_contracts::{TreeId, NotATree}` or
`agentfs_contracts::meta::{..}`: engine src about 30 lines in 25 files (TreeId) and 14 files (metadata); engine tests
19 files; agentfs-cli 3 (launcher/host.rs, launcher/mod.rs, tests/cli/cli_verify.rs); agentfs-conformance 3
(engine_names.rs, mounted.rs, library.rs); agentfs-fuse 3 (tree_view.rs, save_worker.rs, untracked.rs);
agentfs-service tests/volume.rs; the front's doctest (`agentfs::contracts::meta::{Attrs, Perm}`); engine.md:25, :186.
New check (agentfs-repo `crates.rs`) `the_library_gives_each_item_one_path`: in every source of a crate the front
re-exports (its normal dependencies), whole text, no statement (up to its `;`) that starts `pub use` or `pub type`
(not `pub(..)`) names a path `agentfs_<name>::` or `::agentfs_`; `fn reexport_problems(source: &str) -> Vec<String>`,
fixture test `a_second_path_is_found`: `pub use agentfs_contracts::TreeId;`, `pub use {agentfs_contracts::TreeId};`,
`pub type T = agentfs_contracts::TreeId;` and one inside `mod tests` found; `pub(crate) use ..`, `use ..` and
`agentfs_layers` (a name, fuse lib.rs:52) not. **Not read** (development.md, beside the check): a private import
re-exported through `self::`/`crate::` (`use agentfs_contracts::TreeId; pub use self::TreeId as T;`), a crate alias
(`use agentfs_contracts as c; pub use c::TreeId;`), a macro that expands to one; rustdoc JSON, which resolves items,
is nightly-only. The leaves are out of scope by design (lines' `Hunk`, git's `GIT_PATH`).
**Cut.** `queue::held` (queue.rs:50) → `impl Shape { pub(super) fn held(&self, open_objects: impl IntoIterator<Item =
u64>) -> BTreeSet<queue::Key> }` in tree.rs, beside `keys_of` (tree.rs:1210), whose body it repeats; takes.rs:278
calls it; queue.rs no longer imports `Shape`. `Upstreams` and its `merges` do not move (Decisions 3).
**Engine items.** `Error::Reconcile(#[from] agentfs_lines::Error)` → `Error::Lines(..)`, errno arm as before (mod.rs:183,
:279). JC6 and JC8 join compaction.rs's invariant list (cited at :68, :987, built.rs:732, compactor.rs:190,
mod.rs:2613; text from latency-upkeep.md's C6, C8 at 1e45703^): **JC6.** an unlinked segment a holder still reads stays
readable through its `Arc` and counts in `Durability::len` until the last holder lets go; **JC8.** every field of the
checkpoint has one named source at `built`: History's state read in one hold, and the fold of the records at or below
it. Test-only `pub` (F2's rule, `#[cfg(any(test, feature = "test-util"))]`; every caller is a test, read with grep):
`History::wait_checkpointed` (built.rs:424; callers mod.rs:1962 in `test_support`, :2609, compactor/tests.rs:478, :611,
it/tree_actor.rs:1396, support/trees.rs:115, tree_stages.rs:393), `wait_compacted` with the field `compacted` and its
write, `wait_compactor`, `stages::holds_on_this_thread` with `HOLDS_HERE` and its increment in `Held::start`
(stages.rs:596, :611). engine.md's two compaction rows move under "Test builds".
**CI.** `ci/fuse-tests.sh:47`'s `creds::` run gains `case "$OUT" in *"ok. 0 passed"*) fail "no creds test ran" ;; esac`
(the one direct libtest run with a name filter and no count guard; :52 has one); development.md says a filter given to
a libtest binary run directly is not otherwise checked: libtest exits 0 when it matches nothing.
**Docs.** architecture.md: contracts' `meta.rs` row, `paths.rs` row; engine's `meta.rs`, `tree.rs`, `queue.rs`,
`history/format.rs` rows. domains.md: the metadata values. engine.md, agentfs.md (doctest route, "Not re-exported":
`Error::Lines`). development.md: the new check and its Not read. module-cut.md row 5.1's `tree.rs` path. CHANGELOG.
**Acceptance.** `grep -rnE 'pub use agentfs_contracts|struct (Perm|Attrs|Meta)\b|enum Kind\b|fn parent_(of|dir)\b'
crates/agentfs-engine/src crates/agentfs-fuse/src` and `grep -rn 'Reconcile(' crates/agentfs-engine` are empty; `grep -n
'fn held' crates/agentfs-engine/src/tree_actor/queue.rs` is empty. Gates: fmt; clippy `--all-targets -D warnings` and
nextest of -contracts, -engine (also `--features failpoints`), -fuse, -cli (also failpoints), -conformance, -service,
-repo; `cargo test --doc -p agentfs`; CI's two library steps (`cargo clippy --locked -p agentfs-engine -p
agentfs-service -p agentfs --lib -- -D warnings`, `cargo test --doc --locked -p agentfs`); `cargo build -p
agentfs-conformance --bin phase1-replay`; `bash ci/check-deps.sh`.
**Probes.** (1) `pub use agentfs_contracts::TreeId;` restored in mod.rs → `the_library_gives_each_item_one_path`. (2)
`Shape::held` keeps only its first object → the moved held test. (3) `holds_on_this_thread()` called in
agentfs-service's `src` → CI's library clippy step (E0425). (4) `Perm::try_from("644")` accepted → contracts'
`only_four_octal_digits_parse_as_a_mode`. (5) `creds::` renamed `cred::` in the script → its new guard (on a runner).
**Tests.** meta.rs's `meta_is_its_stored_json_..` splits: JSON and `attrs` half → contracts, git modes half →
format.rs's `git_modes_follow_the_owners_execute_bit`; `only_four_octal_digits_..` → contracts; `a_save_keeps_the_atime`
stays. queue.rs's `held_keys_are_an_open_objects_identity_and_names_and_none_unnamed` → tree.rs's tests.
**Files.** `crates/agentfs-contracts/src/{lib.rs,meta.rs,paths.rs}`; `crates/agentfs-engine/**`; the import lines named
in -cli, -conformance, -fuse, -service; `crates/agentfs/src/lib.rs`; `crates/agentfs-repo/tests/repo/crates.rs`; `ci/fuse-tests.sh`; the docs.

## Page G2

**Title.** agentfs-referee holds the landing rules, the queue and their values, `no_std` outside its tests; purity is
read from the ranks and checked at compile time; the crate split's page keeps only what remains.
**The crate** (`crates/agentfs-referee`, rank `landing-rules`). Deps: agentfs-contracts (`TreeId`, `meta::Meta`),
`agentfs-lines = { .., default-features = false }`, `serde` (derive), `thiserror`; dev: `serde_json`, `agentfs-lines`
with `features = ["io"]` (the tests' `align_bytes_with(.., Command::spawn, ..)`). lib.rs: the skeleton's lines, `extern
crate alloc;`, private modules, `pub mod queue;`, the rest re-exported at the root (one path each).
```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the history the landing rules read is malformed")] Format,   // every `Error::Format` of referee/ today
    #[error(transparent)] Ancestry(#[from] Broken),                       // Broken: Cycle | Missing, as today
    #[error(transparent)] Lines(#[from] agentfs_lines::Error),
}
pub type Result<T, E = Error> = core::result::Result<T, E>;
pub trait Landed { type Error: From<Error>;                               // the caller's failure crosses intact
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
```
**Values** (tree.rs:22-205, `src/version.rs`): `EventNumber` (`new`, `get`, `object` `#[inline] pub const`; Display),
`LineVersion` (fields private; `event`; `Id`, `Ord`/`PartialOrd` as its text, Display, `FromStr`/`TryFrom<String>` with
`type Err = NotALineVersion`, each `#[inline]`), `pub struct NotALineVersion(pub String)` (`#[error("{0:?} is not a
line version: `<event>:<index>`, decimal, no sign or leading zero, index from 1")]`), `Versions` (`of`, `take`;
`#[cfg(test)] next`), `pub type Lines = agentfs_lines::LineMap<LineVersion>`. tree.rs's `MergeFileIdentity(pub u64)`,
`NameHistory { pub head, pub versions }`, `NameVersions`, `NameVersion { pub path, pub over, pub replaced_by }` →
`src/values.rs`. The engine's `UNSEALED` (tree.rs:303) becomes `EventNumber::new(u64::MAX)`.
`pub` at the root, shapes unchanged: `Snapshot<L = Lines>`, `File<L>`, `Decision<L>`, `Version`, `CollisionKind`,
`Collision`, `PriorDecisions`, `Synced`, `Cause`, `Record<'a>`, `Undecided`, `Broken`. `queue`: `Key`, `Plan`,
`eligible`, `Cut` and its methods, `cut`. Crate-private: `ByLine`, `Expanded`, `by_line`, `stored`, `Verdict`,
ancestry's `Graph`, `Ancestors`, `descends`, `union`; `#[cfg(test)]` `test_bytes`, `TEST_FILE` (Decisions 6).
**Engine.** `impl From<agentfs_referee::Error> for Error`: `Format | Ancestry(_)` → `Format`, `Lines(e)` → `Lines(e)`;
`From<referee::Broken>` goes; `impl Landed for LandedInto { type Error = Error; }`; unit test
`referee_errors_map_to_the_engines`. `mod referee; mod queue;` go. Consumers: engine src `referee::X` → `agentfs_referee::X`
(about 55 lines), `queue` (6 files), the values (17 files name `EventNumber`); engine tests 9 files. No crate outside
the engine names the referee.
**agentfs-lines `io` feature** (`default = ["io"]`): `FileImage` and its impls, `LineMap::align_bytes_with`,
`histogram_with`, `histogram_files_with` and their `std::fs`/`os`/`path`/`process` imports under `#[cfg(feature =
"io")]`; the engine, its only other user, keeps the default. Verified (scratch workspace, resolver 3): a dev-dependency's
feature is not on in `cargo check -p <crate> --lib`, so a referee call to `FileImage::open` fails there (E0425).
**Purity, checked.** (1) `no_std`: production code naming `std` fails to compile (verified with thiserror and serde
derives). (2) From the ranks: `[workspace.metadata.agentfs] ranks` gains `no_std = true` on `domain` and
`landing-rules`; `Rank` gains `pub no_std: bool` (`#[serde(default)]`); architecture.md's Ranks table gains a `no_std`
column, read by `ranks_table_problems`. `the_pure_ranks_are_no_std` (crates.rs): each library of such a rank has
`#![cfg_attr(not(test), no_std)]` in `src/lib.rs` and no `extern crate std` anywhere in `src` (whole text, tests
included: they have std already); `fn purity_problems(name: &str, lib_rs: &str, sources: &[(String, String)]) ->
Vec<String>`, fixture `a_pure_crate_that_reaches_std_is_found`. agentfs-volume, -clients, -trees and -rules become
`no_std` (a trial compile: about 45 sites in 10 files, `String`/`Vec`/`format!` from `alloc`, `std::` → `core::`).
`shared-kernel` is not marked: contracts' host.rs and views.rs carry std's `PathBuf` (stated in the table). (3) The
spawn check reads `agentfs-referee/src` too. (4) `ci/check-deps.sh`: `cargo check --locked -p agentfs-referee --lib`
(alone, so lines' `io` is off) → `ok   referee alone`; `cargo tree --locked -p agentfs-referee -e normal,features`
refusing `git2`, `libgit2-sys`, `libc`, `rustix`, `opentelemetry*` and `agentfs-lines feature "io"` → `ok   referee`.
**Not read** (development.md): I/O through a dependency's API other than lines' `io` half; an `extern crate` of another
std-linked crate's re-export of std (none in the graph today). Concern for the implementer: a local `cargo clippy -p
agentfs-referee --all-targets` turns `io` on through the dev-dependency; only check-deps' `--lib` build refuses its use.
**Docs.** New `docs/library/referee.md` (what it decides, its calls, `Landed`, errors, purity and its checks, tests in
seconds) in SUMMARY.md and docs/README.md; architecture.md: the referee's map (lib, ancestry, entries, names, placing,
queue, undecided, values, verdicts, version, whole), the engine loses those rows; AGENTS.md Layout row; development.md:
the checks, both `ok` lines, "Build settings" `cargo nextest run -p agentfs-referee` cold and warm [measured]; VERIFYING.md's
finding-51 paragraph (Decisions 7); CHANGELOG. crate-split.md keeps only what remains (Decisions 8): section 1, M4,
M9, "Later", what is not delivered; the Traps' "not read" lines move to development.md beside each check; references
at check-deps.sh:10, crates.rs:438, the front's Cargo.toml:15 and target-architecture/README.md:61 follow.
**Acceptance.** `crates/agentfs-engine/src/tree_actor/{referee,queue.rs}` gone; `grep -n 'git2\|agentfs-git'
crates/agentfs-referee/Cargo.toml` empty; the Traps prose grep, every hit read. Gates: fmt; clippy `--all-targets -D
warnings` and nextest of -referee, -lines, -volume, -clients, -trees, -rules, -engine (also failpoints), -fuse, -cli
(also failpoints), -conformance, -service, -repo; CI's two library steps; `bash ci/check-deps.sh`; phase1-replay builds.
Then, on the final pushed SHA: ci and image green; **phase-1.yml green with 0 mismatched traces** (no `MISMATCH` in its
log); image.yml's release figures read against 0aea0f8's (a miss on any bar but `create` is a warning, reported).
**Probes.** (1) `let _ = std::fs::read("x");` in a referee function → E0433. (2) the `no_std` line deleted from
agentfs-trees → `the_pure_ranks_are_no_std`. (3) `agentfs_lines::FileImage::open("x".as_ref())` in a referee function
→ check-deps `referee alone` (E0425). (4) `features = ["io"]` on its normal dependency → `ok   referee` fails. (5)
`git2 = { version = "0.21.0", default-features = false }` in its dependencies → `referee links git2`. (6) `|c|
c.spawn()` in a referee function → the spawn check. (7) the engine's From maps `Lines(_)` to `Format` →
`referee_errors_map_to_the_engines`.
**Files.** `crates/agentfs-referee/**`; `crates/agentfs-lines/src/lib.rs`; `crates/agentfs-{volume,clients,trees,rules}/src/**`;
`crates/agentfs-engine/**`; `Cargo.toml`; `crates/agentfs-repo/{src/workspace.rs,tests/repo/crates.rs,tests/repo/docs.rs}`;
`ci/check-deps.sh`; `Cargo.lock`; `crates/agentfs-model/VERIFYING.md`; the docs named.

## Decisions

**Slice order.** G1 first: the metadata values go down to the shared kernel and the engine's one-path change rewrites
the same import lines, while the referee's code is still in the engine; G2 then moves the referee onto them. Neither
slice can fan out: both edit the engine and `crates.rs`.

**Design choices.**
1. **Rank `landing-rules` of its own** (uses `line-identity`, `shared-kernel`; used by `engine` and `tests`). With the
   metadata values in the shared kernel, nothing outside the engine names the referee: fuse, the binary, the service
   and consumers do not. So `umbrella` does not use the rank, the front does not re-export it (agentfs-repo's front
   check requires exactly the libraries of the ranks `umbrella` uses), and its `pub` is engine-facing, not consumer API.
   `domain` stays `uses = ["shared-kernel"]`, so the pure domains still cannot depend on align or lines.
2. **`no_std`, not a crate `clippy.toml`** (deviation from row G's first text, kept from revision 1): on clippy 0.1.100 a
   crate's own `clippy.toml` replaces the workspace's 66 bans, and clippy bans no module.
3. **What moves.** Row G's values; the queue (owner's 2026-11-08 cut keeps "queue and locks" its own module: `pub mod
   queue`); `NameVersions`, `NameVersion` with `NameHistory`; the line values. Not: `Upstreams` (no rule or queue reads
   it; its readers are fold.rs, builder.rs, mod.rs), so `merges` keeps its `refs` import inside the engine and no ref
   naming moves; the metadata values and `parent_of` (shared kernel, G1); `Times`, `SetTime` (each tree's own times).
4. **`held` goes into `Shape`**, beside `keys_of`, which it repeated.
5. **Errors.** The referee's own enum; the engine's `From` keeps today's mapping. `Landed` has an associated error
   (serde's `Serializer::Error` idiom). `Error::Reconcile` keeps `agentfs_lines::Error`, renamed `Lines` (E's policy:
   a leaf's types that reach the API are named by depending on the leaf). `NotAPerm`, `NotALineVersion` follow
   `NotATree`.
6. **Two `TEST_FILE`s** (the engine's tests, the referee's): a one-line fixture; one home would need a `test-util`
   feature on the shared kernel or the referee for a constant.
7. **finding-51.** Not re-recorded in G: it needs Docker (`ci/model-docker.sh`), and this container has no daemon. G2
   writes into VERIFYING.md's finding-51 paragraph, present tense, that the patch's hunk was re-cut against
   `shows_the_same` and is not yet re-recorded, so that it still reproduces the regression on today's engine is
   unverified, and that the re-record is that page's "Re-record" step. The orchestrator should open a bead for it
   (this agent's tracker access is read-only).
8. **crate-split.md after G**: kept, shrunk to what remains (M4's two crates, M9's deletion and the front's `fuse`,
   "Later", what is not delivered), because M4 and M9 build on its end state.
9. Pages run to about 75 (G1) and 85 (G2) lines, past the 60 asked: their signatures and move maps are the pages.

**Review (revision 2): one line per finding.**
1. P1 metadata in the referee: **applied.** `Kind`, `Perm`, `Attrs`, `Meta` go to `agentfs_contracts::meta` (G1);
   `git_mode` stays in format.rs; the front does not re-export the referee.
2. P1 `domain` widened: **applied.** Rank `landing-rules` of its own; `domain` reverted (skeleton).
3. P1 `Upstreams` moved: **applied.** It stays in the engine with `merges`; G1's second cut and `pairs` are gone.
4. P2 hand-kept `PURE`: **applied.** `no_std = true` on the ranks, the four domains made `no_std`, whole-text
   `extern crate std` scan; lines' I/O half behind `io`, refused by check-deps' alone build (G2). The shared kernel is
   unmarked, with its reason.
5. P2 line values' home: **applied.** They go to the referee (G2), as section 1 already drew.
6. P2 `wait_checkpointed`, `HOLDS_HERE`: **applied** (G1, with every caller listed). No scan is described in engine.md:
   which `pub` items only tests call stays unchecked, said in development.md.
7. P2 libtest filters: **applied.** Not closed: G1 adds a `0 passed` refusal after `ci/fuse-tests.sh`'s `creds::` run
   (the one direct run with a name filter not guarded; :52 has its count), and development.md states that a filter
   given to a libtest binary run directly is not checked, since libtest exits 0 when it matches nothing.
8. P2 finding-51 orphan: **applied** (Decisions 7): VERIFYING.md owns it; a bead is asked of the orchestrator.
9. P2 gate missing from acceptance: **applied** (G2's acceptance: phase-1 0 mismatched, ci, image, release figures).
10. P2 one-path regex holes: **applied.** Whole text, statements, `pub type` and braces read; the rest listed as Not
   read; rustdoc JSON rejected (nightly-only).
11. nit `parent_of` twice: **applied.** `agentfs_contracts::paths::parent_of`; fuse's copy deleted (G1).
12. nit `TEST_FILE`, `git_mode`: **applied** for `git_mode` (format.rs); two `TEST_FILE`s accepted (Decisions 6).

**Errata C to F, each disposed.** C: finding-51 (Decisions 7); the dev-edge rule and domain docs (built in C, E); the
test-name filter (Review 7). D: built in D; the prose grep runs in G2's acceptance. E: the rank, doctest and front
(built in E, F); JC6 and JC8 (G1). F: (a), (b), (d), (e), (f) built; (c) the `allow-invalid` bans stay unchecked, said
in development.md (G2); F2's hand-read list (G1 adds `wait_checkpointed` and the counter; still unchecked).

**Self-check (goal-oriented).** Row G's ends and the slice for each: purity checked (G2 1-4); rule tests in seconds
(G2's measured row); error enum with `From` (G2); line values (G2); values public (G2); `held` (G1); merges resolved
(stays, Decisions 3); `Error::Reconcile` (G1); `TreeId` path (G1, checked); JC6, JC8 (G1); finding-51 (Decisions 7);
test-only `pub` (G1); release figures and phase-1 (G2's acceptance); crate-split.md (G2). No goal is unowned.

**New terms.** `landing-rules`: the rank of the referee. `no_std` rank: one the manifest marks `no_std = true`, whose
libraries are `no_std` outside their tests. `io` (agentfs-lines): the feature holding its file and process calls.
`Landed::Error`: the port's own failure type. `NotALineVersion`, `NotAPerm`: text that is not one, as `NotATree`.
