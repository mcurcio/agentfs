# Crate split, PRs A and B: frozen pages (revision 2)

Design: `docs/target-architecture/future/crate-split.md` (section 3 rows A and B). Branch `claude/bold-hopper-72ixu2`,
from main 264a494. Slices run in order A, B1, B2, never in parallel: each changes `Cargo.toml` and `Cargo.lock`.
Counts are lines naming the item (`git grep -nE`, comment lines dropped) at the skeleton commit. The skeleton holds:
the five crates (manifest, `lib.rs` doc comment only), the workspace members, every package's rank, the ranks table
in the root manifest, the Dockerfile stubs, AGENTS.md's Layout rows, and docs/architecture.md's crate rows, "Ranks"
section and empty module sections. Owner's words binding this: OWNER-GUIDANCE.md "Performance" (2026-11-24) and
2026-11-25 ("A and B can be built in the same PR"). A *rank* is a new term: see Decisions, "New terms".

## Page A

**Crate.** `agentfs-repo`, `crates/agentfs-repo`, rank `tests`, `publish = false`. Deps `serde` (derive), `serde_json`;
dev (added in B1) `agentfs-failpoints`. Its library depends on no agentfs crate. Why a crate: a change to any page but
the five listed under "CI and docs" builds serde and serde_json (5.0 s cold [measured: a scratch crate with just those
two, 4 CPUs]), not core (88 s).
**Public surface** (`src/lib.rs`: `pub mod doc_tables; pub mod workspace;`).
- `doc_tables.rs`: `git mv` of `crates/agentfs/src/doc_tables.rs`; `#![allow(dead_code)]` and the `#[path]` paragraph
  go. Unchanged: `Section`, `Table`, `Table::column`, `Section::rows`, `heading`, `sections`, `split_row`, `code_spans`,
  `sole_code_span`, `section`. Changed: `pub fn read_doc(name: &str) -> String` (`docs/<name>` under
  `workspace::root()`, panics naming the path; `manifest_dir` goes). New, from `docs_reference.rs`:
  `pub fn key_cells(section: &Section) -> std::collections::BTreeSet<String>` (core and repo use it).
- `workspace.rs`, new: `pub fn root() -> std::path::PathBuf`; `pub struct Workspace { pub ranks: Vec<Rank>, pub members:
  Vec<Member> }`; `impl Workspace { pub fn load() -> Self; pub fn member(&self, name: &str) -> Option<&Member>; pub fn
  rank(&self, name: &str) -> Option<&Rank>; }` (`load` runs `env!("CARGO") metadata --format-version 1 --no-deps
  --offline --manifest-path <root>/Cargo.toml`, reads `metadata.agentfs.ranks`, panics on a cargo failure);
  `pub struct Rank { pub name: String, pub uses: Vec<String> }`; `pub struct Member { pub name: String, pub dir: PathBuf
  /* relative */, pub rank: Option<String>, pub dependencies: Vec<Dependency>, pub features: BTreeMap<String,
  Vec<String>>, pub targets: Vec<Target> }`; `pub struct Dependency { pub name: String, pub kind: DependencyKind }`;
  `pub enum DependencyKind { Normal, Build, Dev }`; `pub struct Target { pub name: String, pub kinds: Vec<String> }`.
  All `Debug, Clone`. Each check below is a pure function over a `Workspace` or a line, so fixtures build one by hand.
**Test moves.** One program `tests/repo/main.rs` (modules `docs.rs`, `ci.rs`, `crates.rs`) takes core's
`docs_reference.rs` but its 2 checks of `config::KEYS` and `rules::OPERATOR_ATTRIBUTES` (17 checks, with `modules`,
`files_under`, `shell_words`); those 2 stay as core's `tests/config_reference.rs`. agentfs's `reference_tests.rs` and
csi's `main.rs` (its `#[path]` module goes) read through `agentfs_repo::doc_tables`, a dev-dep of core, agentfs, csi.
**Changed checks.** Module map: every member, no exclusion (the page says "every module of every crate"); A writes
`### agentfs-align` (1 row) and `### agentfs-model` (22 rows). Dockerfile stubs: members from `Workspace`.
**New checks** (fixture test beside each):
1. `dependencies_follow_the_ranks`: rank names unique; each rank's `uses` names itself or an earlier rank; every member
   names a rank; each Normal or Build dependency on a member is in a rank its own rank uses. Dev is exempt (agentfs's
   tests drive agentfs-csi). Fixtures: lines → git, trees → volume, contracts → os, core → a second `engine` package.
2. `every_failpoints_feature_is_forwarded`: a member declaring `failpoints` lists `<dep>/failpoints` or
   `<dep>?/failpoints` for each Normal dependency on a member that declares it. Code gated on the feature in a crate
   without it is refused by rustc's `unexpected_cfgs` under `-D warnings`. Fixture: fuse without core's.
3. `cargo_commands_name_what_the_workspace_has`. Failure class: a cargo command selecting a package, feature or binary
   the workspace does not have; chiefly a reference meant for the binary once D renames its package (and E gives the
   name `agentfs` to the library front). Files: `.github/workflows/*`, `ci/**`, `Dockerfile`, `docs/**/*.md`,
   `.claude/skills/**/*.md`, `crates/*/*.md`, `AGENTS.md`, `README.md`, `CONTRIBUTING.md`, `examples/**`, `deploy/**`.
   Lexing: join `\`-continued lines; words as sh splits them (quotes group and are removed); `` ` ``, `|`, `;`, `&`,
   `(`, `)` are separator tokens; no comment rule, so comments and markdown cells are read. A command starts at every
   word `cargo` (after a YAML `- run:`, prose or a cell's backtick alike) and runs to the next separator; a word that
   holds whitespace (a quoted string) is lexed again as a line. In a command: `-p X`, `-pX`, `-p=X`, `--package X`,
   `--package=X` name a member; `--features V`, `--features=V`, `-F V`: V split at `,` and blanks, each `pkg/feat`
   names a member and a feature it declares, each bare `feat` is declared by at least one selected package (cargo's
   rule) when every selection is literal; `--bin B` names a bin target of a selected package (of any member when none);
   `cargo run` with a selection needs a bin target. Values outside `[A-Za-z0-9_-]+` (`<crate>`, `$pkg`) are skipped.
   Prototype at the skeleton: 147 commands, 28 select `agentfs`, 0 refused; without package `agentfs` (D's state), 36
   refused. Fixtures, today's lines: ci.yml `- run: cargo build --locked -p agentfs --features otlp-tls`;
   configuration.md `` `cargo build -p agentfs --features ssh` ``; development.md `` | `cargo test -p agentfs`, its
   build and run | ``; csi-sanity.sh `fail "$bin is missing: cargo build --release -p agentfs -p agentfs-csi"`;
   Dockerfile `--features agentfs/https,agentfs/ssh,agentfs/otlp-tls`. Each yields its selection and features, and is
   refused on a fixture with no `agentfs`; on a fixture where `agentfs` is a library with no features (E's state) the
   feature lines, `--bin agentfs` and `cargo run -p agentfs` are refused and `cargo test --doc -p agentfs` passes.
   Not covered: after E, a bare `cargo build -p agentfs` meant for the binary is, in form, a correct use of the front.
4. `the_ranks_are_documented`: architecture.md's "Ranks" table: column 1's code spans are the ranks in order; column
   2's code spans are each rank's `uses`.
**CI and docs.** ci.yml docs job: first `cargo nextest run --locked -p agentfs-repo`, then core `--test
config_reference`, the other three steps kept; `doc_tables` out of agentfs's filter; the job comment. AGENTS.md "Local
gates": a doc change runs `cargo nextest run -p agentfs-repo`; docs/configuration.md also `-p agentfs-core --test
config_reference`, `-p agentfs --bin agentfs reference_tests plan_reads_only_env_vars` and `-p agentfs-csi --bin
agentfs-csi reference_tests`; docs/runbooks/fork-refused.md or patch-queue.md also `-p agentfs --bin agentfs
fork_cmd::`; rules-ledger.md or git-model.md also `-p agentfs-model --test ledger`. Every other name of
`docs_reference`: ci.yml:58, AGENTS.md:130, CONTRIBUTING.md:52, ci/lib.sh:29, core config.rs:192 and :1935, rules.rs:210,
VERIFYING.md:108, architecture.md module-map intro, configuration.md:13, development.md:121 and :459,
target-architecture/future/root-branch-name.md:71. architecture.md: the agentfs `doc_tables.rs` row out; repo rows.
**Acceptance.** `git grep -n docs_reference -- ':!.claude'` is empty; `git grep -n '#\[path' crates` shows no
`doc_tables`; `crates/agentfs/src/doc_tables.rs` is gone. Gates: `cargo clippy -p agentfs-repo -p agentfs-core -p
agentfs -p agentfs-csi --all-targets -- -D warnings`; `cargo nextest run -p agentfs-repo`; core `--test
config_reference`; agentfs's and csi's `reference_tests`. Measured and recorded in development.md "Build settings":
`cargo nextest run -p agentfs-repo` cold (fresh target dir) and warm.
**Probes.** (1) agentfs-lines gets `agentfs-git` in `[dependencies]` → check 1 names `agentfs-lines -> agentfs-git
(line-identity does not use leaf)`; agentfs-trees gets agentfs-volume → `domain does not use domain`. (2) drop
`"agentfs-core/failpoints"` from fuse → check 2. (3) `cargo build -p agentfs-cli` in ci/fuse-tests.sh, and
`--features agentfs/tls` anywhere → check 3. (4) swap two rows of "Ranks" → check 4. (5) delete `### agentfs-align`
→ the module map check.
**Files.** `crates/agentfs-repo/**`; agentfs's `Cargo.toml`, `src/{doc_tables,main,reference_tests}.rs`; csi's
`Cargo.toml`, `src/main.rs`; core's `Cargo.toml`, `tests/{docs,config}_reference.rs`, `src/{config,rules}.rs`;
`Cargo.lock`; the CI and docs files named above.

## Page B1

**Crates**, all rank `leaf`: what knows no domain value and is used outside the recording engine too, so M9 can delete
core whole. Users outside it: allocator (csi, binary), confined (tree_actor, fuse tree_view), file_lock, fsutil
(tree_actor, csi), listening_socket, shared_fs (csi), mountinfo (fuse stale.rs), failpoints, git_cli, init (tree_actor).
Two modules of the brief fail the test and stay in core: `tree_git` (a tree volume's ref layout and notes, used by the
tree engine alone: C moves it with the engine; its bans, tests and counters unchanged) and `local_socket` (the history
service's and the binary's control socket, recording engine only: M9 deletes it).
- `agentfs-failpoints`. Deps `libc`, `thiserror`; dev `tempfile`, `rustix` (fs). Feature `failpoints = []`. `src/lib.rs`
  = today's `failpoints.rs`. Surface kept: `ENV_VAR`, `POINTS`, `hit`, `owner`, `adopt`; with the feature
  `FailpointError`, `Guard`, `configure`, `hits`, `trace`, `TakeGate`, `take_gate`, `FifoGate`. Changes: FifoGate
  implements nothing of core and gains `pub fn park(&self, place: &str) -> std::io::Result<()>` (today's `at` body with
  `gap.name()` → `place`: write `place` to `at`, read a line of `go`: `go` → Ok, `fail` → EIO, empty or EOF ends the
  gate); its doc is "a process parked at named places by a test script". `COMPACT_GATE_VAR` leaves for core's
  `tree_actor::COMPACT_GATE_VAR` (defined in `journal/compaction.rs` beside the impl, re-exported as `CompactGate` is;
  users: agentfs `launcher/mod.rs:489`, `reference_tests.rs:163`, conformance `mounted.rs:249`).
  `FailpointError::UnknownPoint`'s text names `agentfs_failpoints::POINTS`. `POINTS` stays one list, grouped by engine
  in comments; why not tikv's `fail`, see Decisions F6.
- `agentfs-os`. Deps `agentfs-failpoints` (fsutil hits `state.json.before_rename`), `libc`, `rustix`, `serde`,
  `serde_json`, `tokio` (net), `tokio-stream`; dev `tempfile`, `tokio` (rt). Feature `test-util = []`. Modules moved
  whole: `allocator`, `confined`, `file_lock`, `fsutil`, `listening_socket`, `mountinfo`, `shared_fs`. Shape changes:
  `confined::Dir::sha256_at` → core `lfs::pointer::sha256_at(tree: &agentfs_os::confined::Dir, rel: &str) ->
  io::Result<(LfsOid, u64)>`; `confined::{Current, FileState}` and `Dir::current_entry` (git blob ids, `RepoPath`) →
  `restore.rs`, private: `enum Current`, `struct FileState`, `fn current_entry(tree: &Dir, path: &RepoPath) ->
  io::Result<Current>`; `confined::RESOLVE` widens to `pub const RESOLVE: rustix::fs::ResolveFlags` and fuse's
  `backing.rs` takes `agentfs_os::confined::{RESOLVE, proc_path}`, dropping its copies (19 lines name `proc_path`);
  confined's paragraph naming `agentfs_fuse::backing` goes, and backing.rs's doc says what it still keeps apart and
  that M9 ends it; `file_lock::takes_while_children_spawn` → `#[cfg(any(test, feature = "test-util"))] pub fn
  takes_while_children_spawn(take: impl FnMut() -> bool) -> (usize, usize)` (4 core test users, via core's
  dev-dependency with `features = ["test-util"]`). Doc links to core items become plain text.
- `agentfs-git`. Deps `agentfs-align` (`GIT_PATH`), `git2` and `libgit2-sys`, both `default-features = false`; dev
  `tempfile`. Modules: `git_cli` (moved whole; gains `pub const PACK_CONFIG: [&str; 12]` from core's `repack.rs`;
  `counted` and `children` widen `pub(crate)` → `pub`: the tree engine's stages and builder call them from core) and
  `setup` (private, items at the root): `pub fn init()` (today's `init_libgit2` without SSH: fsync, the four search
  paths, the home directory, both timeouts, in one `Once`), `pub const SERVER_CONNECT_TIMEOUT_MS: std::ffi::c_int =
  30_000`, `pub const SERVER_IO_TIMEOUT_MS: std::ffi::c_int = 300_000`.
**Cycle cuts.** (1) `failpoints.rs:141`: `#[cfg(feature = "failpoints")] impl CompactGate for agentfs_failpoints::FifoGate
{ fn at(&self, gap: Gap) -> io::Result<()> }` (parks at `gap.name()`) in `tree_actor/journal/compaction.rs`. (2) The
libgit2 setup: core's `history::init_libgit2` goes; core gets `pub(crate) fn init_git()` in `history.rs`:
`agentfs_git::init()`, then, with `ssh`, `ssh_transport::register()` inside its own `Once`. The recording engine's
calls rename one for one; the tree engine's 4 (`tree_actor/mod.rs:688`, `:803`, `tree_git.rs:575`,
`history/verify.rs:686`) call `agentfs_git::init()`. Ordering invariant (git2: `transport::register` must not run while
this process creates a transport), restated in `ssh_transport::register`'s SAFETY: every recording-engine path that
opens a remote calls `init_git` first, whose `Once` finishes the registration before any call returns; nothing else
opens a remote: the tree engine is refused one by B2's standing check, and fuse, agentfs and csi name no git2 remote
API today (grep; not checked). (3) `tree_actor/history/repack.rs:98` and core's `repack.rs` import
`agentfs_git::git_cli::PACK_CONFIG`.
**Checks.** clippy.toml: the `FileLock` and `Listener` reasons name `agentfs_os::...`; new bans with `allow-invalid =
true`, reason "libgit2 is set up once, by agentfs_git::init": `libgit2_sys::init`, `libgit2_sys::git_libgit2_opts`
and every setter of `git2::opts` (`set_search_path`, `reset_search_path`, `enable_caching`, `set_cache_object_limit`,
`set_cache_max_size`, `strict_object_creation`, `strict_hash_verification`, `set_extensions`,
`set_verify_owner_validation`, `set_ssl_cert_file`, `set_ssl_cert_dir`, `set_mwindow_size`, `set_mwindow_mapped_limit`,
`set_mwindow_file_limit`, `set_server_connect_timeout_in_milliseconds`, `set_server_timeout_in_milliseconds`; getters
are free), each call in `setup.rs` under `#[expect(clippy::disallowed_methods)]`; `git2::transport::register`, reason
"transports are registered by agentfs_core::ssh_transport::register", expected there only. Core drops `libgit2-sys`.
Features: core `failpoints = ["agentfs-failpoints/failpoints"]`; fuse and agentfs add `"agentfs-failpoints/failpoints"`.
Moved to agentfs-repo `crates.rs`: `every_hit_is_listed` (the src of every `Workspace` member; each `hit("` outside
agentfs-failpoints' own src is written `agentfs_failpoints::hit("`; the names equal `POINTS`, none twice) and core's
`tree_stages.rs::one_spawn_site` as `the_tree_engine_starts_git_only_through_counted` (production text, up to the first
`#[cfg(test)]`, of core's `src/tree_actor/**` but `history/repack.rs`, `src/tree_git.rs`, `src/tree_reconcile.rs`:
none of `.spawn()`, `.output()`, `.status()`, `Command::spawn`, `Command::output`, `Command::status`; agentfs-git's
`git_cli.rs` has one `.spawn()`).
**Consumers** (no re-exports from core). core: failpoints 115, os modules 46, git 43 (incl. `init_libgit2`); fuse:
failpoints 7, os 3, plus backing's 19; agentfs: failpoints 8, os 7, git 1; csi: os 4; conformance: `COMPACT_GATE_VAR` 1.
**Test moves.** Unit tests move with their files. History's `libgit2_home_directory_is_cleared`,
`libgit2_server_timeouts_are_set`, `init_libgit2_clears_config_search_paths` → `setup.rs`, over `init()`. FifoGate's
test → over `park`. confined's `current_entry` tests → `restore/tests.rs`.
**Docs.** architecture.md (core rows of the 9 moved files out, rows for the new crates' modules, the crate rows,
:305, :311, :578, Technology choices' libgit2 setup), library/core.md (module table, "Locks and sockets",
`repack::PACK_CONFIG`, `confined::Dir` at :397), configuration.md :1141, :1146, csi.md :578,
target-architecture/future/state-on-nfs.md :11, the root `Cargo.toml` comment naming `agentfs_core::file_lock`.
**Acceptance.** None of core's `src/{allocator,confined,file_lock,fsutil,listening_socket,mountinfo,shared_fs,
failpoints,git_cli}.rs` exists; `git grep -nE "agentfs_core::(allocator|confined|file_lock|fsutil|listening_socket|
mountinfo|shared_fs|failpoints|git_cli)|init_libgit2|repack::PACK_CONFIG" -- ':!.claude'` is empty. Gates, one call
each: `cargo clippy -p agentfs-failpoints -p agentfs-os -p agentfs-git -p agentfs-core -p agentfs-fuse -p agentfs -p
agentfs-csi -p agentfs-conformance -p agentfs-repo --all-targets -- -D warnings`, the same with `--features
failpoints` (cargo needs one selected package to declare it), and `cargo nextest run` with both; `ci/check-deps.sh`.
**Probes.** (1) `git2::opts::set_mwindow_size` in core → clippy. (2) `agentfs_failpoints::hit("os.x")` in agentfs-os →
`every_hit_is_listed`; `use agentfs_failpoints::hit; hit("tree.x")` → the same check. (3) drop fuse's
`"agentfs-failpoints/failpoints"` → A's check 2. (4) `git2 = "0.21"` with default features in agentfs-git →
`ci/check-deps.sh` (default build links OpenSSL). (5) delete the `CompactGate` impl → `cargo clippy -p agentfs
--features failpoints` fails to compile. (6) `Command::spawn` passed as a function in `tree_actor/stages.rs` →
`the_tree_engine_starts_git_only_through_counted`.
**Files.** `crates/agentfs-{failpoints,os,git}/**`; core's 9 moved files, `history.rs`, `ssh_transport.rs` and its
tests, `repack.rs`, `restore.rs`, `restore/tests.rs`, `lfs/pointer.rs`, `local_socket.rs`, `tree_git.rs`,
`tree_actor/**`, `lib.rs`, `Cargo.toml`, `tests/**`; fuse (incl. `backing.rs`), agentfs, csi, conformance sources and
manifests; `crates/agentfs-repo/tests/**`, its `Cargo.toml`; `clippy.toml`; root `Cargo.toml`; `Cargo.lock`; the docs.

## Page B2

**Crate.** `agentfs-lines`, `crates/agentfs-lines`, rank `line-identity`. Deps `agentfs-align`, `serde` (derive),
`serde_json`, `thiserror`; no git2, so G's referee can build on it, and A's check 1 refuses any edge to a `leaf`.
`src/lib.rs` = today's `tree_reconcile.rs` (`pub use agentfs_align::Hunk` stays: it is in the signatures). Surface
kept (`Id`, `Image`, `FileImage`, `Window`, `LineIndex`, `Run`, `Over`, `Line`, `Expanded`, `LineMap`, `Error`,
`Piece`, `pieces`, `render`), but the cut below.
**Cycle cut.** `tree_reconcile.rs:1174`, `:1189` call `crate::git_cli::counted`. Lines starts no process; its three
diff entry points take the spawner and are named, as `agentfs_align::histogram_with` is, with `_with`:
`pub fn histogram_with(old: &[u8], new: &[u8], spawn: impl FnOnce(&mut std::process::Command) ->
std::io::Result<std::process::Child>) -> Result<Vec<Hunk>, Error>`; `pub fn histogram_files_with(old: &Path, new: &Path,
spawn: impl FnOnce(&mut Command) -> io::Result<Child>) -> Result<Vec<Hunk>, Error>`; `impl<V: Id> LineMap<V> { pub fn
align_bytes_with(&self, old: &[u8], new: &[u8], spawn: impl FnOnce(&mut Command) -> io::Result<Child>, next: impl
FnMut(u64) -> V) -> Result<Self, Error> }`. For the same rule align's `histogram_files` is renamed
`histogram_files_with` (one caller). The one production caller, `builder.rs:1429`, passes
`agentfs_git::git_cli::counted`. Cost: the count stays exact only while every tree-engine caller passes `counted`; the
standing check holds that: `the_tree_engine_starts_git_only_through_counted` now reads `agentfs-lines/src/**` for the
old `tree_reconcile.rs` and also refuses `agentfs_align::histogram(` (align's uncounted call) in the files it reads.
**Contracts.** `agentfs_contracts::paths`: today's `paths.rs` whole (`RepoPath`, `PathError`, `is_dotgit`,
`SpecialFile`, `special_file`, `must_not_be_symlink`; serde and thiserror, which contracts has) with its tests.
`agentfs_contracts::units` (new): `pub fn parse_duration(value: &str) -> Option<std::time::Duration>` and `pub fn
parse_size(value: &str) -> Option<u64>` from `config.rs:1887`, `:1904` with their tests; and the one clap wrapper,
`pub fn duration_arg(value: &str) -> Result<std::time::Duration, NotADuration>` with `#[derive(Debug, Clone,
PartialEq, Eq, thiserror::Error)] #[error("{0:?} is not a duration (use e.g. 500ms, 5s, 30m, 24h)")] pub struct
NotADuration(pub String);`, replacing csi `main.rs:26` `duration`, agentfs `main.rs:366` `parse_grace` and
`lfs_cmd.rs:67` `parse_age` (their `value_parser =` sites name `duration_arg`).
**csi.** `layout.rs:85`, `:90`: `agentfs_contracts::name::is_safe_name(x)`, refusal → the same `LayoutError`. Then csi
uses core only for `operator_settings` and `tree_actor::inspect`, which C moves.
**New standing check** in agentfs-repo `crates.rs`, `the_tree_engine_names_no_recording_engine_module`: in the
production text of core's `src/tree_actor/**`, `tree_git.rs`, `session.rs`, `store.rs`, `state_area.rs`,
`operator_settings.rs` (C's list), every `crate::<m>` path, inside `use crate::{...}` too, names `tree_actor`,
`tree_git`, `session`, `store`, `state_area` or `operator_settings`; and none of `find_remote`, `remote_anonymous`,
`remote_with_fetch`, `git2::Remote`, `git2::transport` appears (B1's ordering invariant). `super::` chains that reach
the crate root are not read (none today). C deletes it: the crate boundary replaces it.
**Consumers.** core: `paths::` 44, `tree_reconcile::` 18, `parse_*` 20 (most inside config.rs); fuse: `paths::` 9;
agentfs: `paths::` 6, `parse_*` 4; csi: `validate_name` 3, `parse_duration` 2. core and fuse already depend on contracts.
**Test moves.** `tests/it/tree_reconcile.rs` → `crates/agentfs-lines/tests/line_maps.rs` (its calls pass
`Command::spawn`); `referee/mod.rs:426`'s test calls `align_bytes_with` with `agentfs_git::git_cli::counted`.
**Docs.** architecture.md (core rows `paths.rs`, `tree_reconcile.rs` out; contracts rows `paths.rs`, `units.rs`; lines
`lib.rs`; contracts' crate row), library/core.md (module table rows, the `RepoPath` example's import), AGENTS.md's
contracts row.
**Acceptance.** Section 2's four edges are gone and stay gone: the standing check passes, and `git grep -nE
"init_libgit2|repack::PACK_CONFIG|config::parse_size|crate::paths" -- crates/agentfs-core/src/tree_actor
crates/agentfs-core/src/operator_settings.rs` is empty; `git grep -n "agentfs_core::config" -- crates/agentfs-csi` is
empty; `git grep -nE "agentfs_core::(paths|tree_reconcile)|config::parse_(size|duration)" -- ':!.claude'` is empty.
Gates: `cargo clippy -p agentfs-align -p agentfs-contracts -p agentfs-lines -p agentfs-core -p agentfs-fuse -p agentfs
-p agentfs-csi -p agentfs-model -p agentfs-repo --all-targets -- -D warnings`, again with `--features failpoints`;
`cargo nextest run` with the same packages, both ways.
**Probes.** (1) `builder.rs` passes `std::process::Command::spawn` → the spawn check names builder.rs. (2)
`crate::history::init_git()` in `tree_actor/mod.rs` → the recording-module check. (3) agentfs-lines depends on
agentfs-git → A's check 1. (4) a `find_remote` call in `tree_git.rs` → the recording-module check.
**Files.** `crates/agentfs-lines/**`; `crates/agentfs-align/src/lib.rs`; `crates/agentfs-contracts/{Cargo.toml,
src/lib.rs,src/paths.rs,src/units.rs}`; core's `paths.rs`, `tree_reconcile.rs`, `config.rs`, `fsync_class.rs`,
`operator_settings.rs`, `lib.rs`, `tree_actor/**`, `benches/record_step.rs`, `tests/**`, `Cargo.toml`; fuse, agentfs,
csi sources and manifests; `crates/agentfs-repo/tests/**`; `Cargo.lock`; the docs.

## Decisions

**Slice order.** A, B1, B2, serial (each changes the manifest and lockfile). A first, so its checks guard B's new
crates; B1 before B2, because B2's builder passes `agentfs_git::git_cli::counted` and B2's standing check passes only
once B1 and B2 have both cut their edges.

**Design choices.**
1. One home for the markdown reader: agentfs-repo's library, a dev-dependency of core, agentfs and csi; it replaces
   two `#[path]` includes; its library depends on no agentfs crate, so no cycle.
2. Ranks in Cargo's own metadata tables, read with `cargo metadata --no-deps` through serde_json (the `cargo_metadata`
   crate adds three packages for six fields). The model is ArchUnit's layered architecture (`mayOnlyAccessLayers`, each
   layer names the layers it may use) with import-linter's independent siblings (a rank's packages do not depend on one
   another unless the rank uses itself). Today's manifests pass; lines → git, trees → volume, contracts → os, rules →
   os and fuse → csi are refused [checked with a prototype over `cargo metadata`].
3. The `cargo` check reads references, not intent: it refuses what names nothing (D's rename) and binary-only uses of
   the front's name (features, `--bin`, `run`); a bare `-p agentfs` after E is the stated gap.
4. Failpoints forwarding over manifests; rustc's `unexpected_cfgs` covers gated code without a feature.
5. Deviation: confined loses its git part as well as its LFS part (`current_entry` hashes blobs, takes `RepoPath`): to
   `restore.rs`, its one consumer.
6. Deviation: the SSH transport stays in core; `agentfs_git::init` sets every libgit2 option, enforced by clippy bans
   on every setter; core's `init_git` adds the transport, with the ordering invariant of page B1.
7. Deviation: lines takes the spawner (`_with` names); a git2 dependency in lines would reach G's referee.
8. `test-util` feature on agentfs-os for one shared test helper (tokio's idiom); the alternative was a copy in core.
9. `PACK_CONFIG` in `git_cli`: the `git` program's configuration. `COMPACT_GATE_VAR` moves beside the compaction
   gate's impl in the tree engine; `TakeGate` stays generic in the leaf.
10. Deviation: `tree_git` and `local_socket` stay in core (page B1's leaf test); crate-split.md's section 1 diagram
    and row C now say so.
11. Core's remaining doc test is renamed `config_reference`, so no two programs share a name.
12. `duration_arg` and `NotADuration` in `agentfs_contracts::units`: the one home of the clap wrapper, one message.

**New terms.**
| Term | Definition | Why needed | Existing term it could be |
|---|---|---|---|
| rank | A group of packages that may depend on the same ranks (`[workspace.metadata.agentfs] ranks`); each of the owner's three layers is made of ranks | The owner's three layers are too coarse to refuse sibling edges (two domains, two engines, lines on git) | "layer": not used, so the owner's word keeps his meaning (principle 10) |
| `shared-kernel` (rank) | Replaces the skeleton's `kernel`: DDD's shared kernel, already AGENTS.md's term for `agentfs-contracts` | `kernel` read as the Linux kernel in a FUSE project | none |
| `line-identity` (rank) | git-model §7's term: align and lines | lines must not depend on the `leaf` rank | none |

**Review findings, one line each** (revision 2).
- F1 (P1, siblings unchecked): applied. Ranks with `uses` lists replace the strict ranking; the three sibling rules
  and kernel/domain purity are refused, with probes lines → git and trees → volume (check 1).
- F2 (P1, `-p agentfs` check): applied. The class is restated (references to what the workspace lacks); the
  binary-exists clause is gone, so E's correct uses pass; the lexer reads YAML `run:` lines, code spans, quoted strings
  and `pkg/feat`; five fixtures from today's lines; the post-E gap is stated. Prototype: 0 false refusals, 36 at D.
- F3 (P2, uncounted diffs): applied (cost stated). `_with` names; the spawn check moves to agentfs-repo, refuses the
  path forms and align's uncounted `histogram(`, and reads lines' src; probe is the path form. The impossible-by-
  construction option (the counter in agentfs-align) was weighed and rejected: align would own every git child's count.
- F4 (P2, tree_git not a leaf): applied. tree_git stays in core and moves with the engine in C; the leaf is now defined
  by a test each module passes; the same test keeps `local_socket` in core (a deviation the review did not raise).
- F5 (P2, libgit2 setup and SSH order): applied. Invariant and guarantors restated; all 16 `git2::opts` setters, both
  libgit2-sys calls and `transport::register` are banned outside their homes; the tree engine's remote ban is a
  standing check (B2); fuse, agentfs and csi are named as unchecked.
- F6 (P2, failpoints prior art): applied, central list kept with reason. tikv's `fail` declares points where they are
  called and accepts any configured name, so a misspelt spec passes by never firing, the class this registry refuses
  at parse time; refusing it needs every point's name in every process at its first hit. Link-time collection
  (`linkme`, `inventory`) would give that without a central list, at the cost of a proc-macro dependency and a
  declaration at all 50 call sites; not worth it for 50 strings. Cost: a new point edits the leaf; the check forces it.
  The compaction concept leaves the leaf (`COMPACT_GATE_VAR`, FifoGate's doc).
- F7 (P2, docs-only goal): applied. The pages a doc gate lists beyond agentfs-repo are named (configuration.md, two
  runbooks, the ledger pages); AGENTS.md's gate lists every check that reads configuration.md; A measures the repo
  run cold and warm; serde derive's cost is in the 5.0 s figure. CI's docs job still compiles core for its other steps.
- F8 (P2, repo-wide scans in two homes): applied. `every_hit_is_listed` and the spawn check move to agentfs-repo; the
  one-time grep becomes the standing `the_tree_engine_names_no_recording_engine_module`.
- F9 (P2, confinement in two homes): applied in part. fuse's `backing.rs` takes `RESOLVE` and `proc_path` from
  agentfs-os in B1; the rest of `Backing` is the recording engine's FUSE half, which M9 deletes, listed under
  crate-split.md's "Not delivered".
- F10 (P2, missed references, false claims): applied. A and B1 list every reference found by `git grep` (the five
  the review named and CONTRIBUTING.md, development.md:121 and :459, state-on-nfs.md); acceptance greps cover the
  whole repo; the hand-written dependency chain is deleted from architecture.md (the ranks are the one list); the
  align and lines rows are corrected.
- F11 (P2, "layer" renamed): applied. "rank" for the check's unit, mapped to the owner's three layers in the
  architecture.md table; `kernel` → `shared-kernel`.
- F12 (nit, failpoints gate): applied. Gates are single multi-package invocations (cargo accepts `--features
  failpoints` when one selected package declares it [checked with `cargo tree`]).
- F13 (nit, module map exclusion): applied. No exclusion; align and model get sections.
- F14 (nit, three duration wrappers): applied. One `duration_arg` in `agentfs_contracts::units`.

**Design review outcome.** Section 2's file:line items re-read at the skeleton: still accurate. Goals and their
steps: "a docs-only change builds in seconds" → A (for every page but the five with crate-level readers, which still
build core or the model); "makes C and M9 possible" → B1 and B2 (no leaf the recording engine alone uses, no module the
tree engine alone uses, outside core); "the four edges gone" → B2's standing check. Untested until the slices land:
every check above; the rank prototype and the `cargo` lexer prototype ran in a scratch script only.
