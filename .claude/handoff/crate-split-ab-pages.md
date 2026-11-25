# Crate split, PRs A and B: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 rows A and B). Branch
`claude/bold-hopper-72ixu2`, from main 264a494. Slices run in order A, B1, B2, never in
parallel: each changes `Cargo.toml` and `Cargo.lock`. Counts are lines naming the item, from
`grep -rn ... --include=*.rs` minus comment lines, at 264a494. The skeleton commit already holds:
the five crates (manifest, `lib.rs` doc comment only), the workspace members, the layer metadata of
every package, the Dockerfile stubs, AGENTS.md's Layout rows, and docs/architecture.md's crate
rows, "Layers" section and empty module sections. Owner's words binding this: OWNER-GUIDANCE.md
"Performance" (2026-11-24) and 2026-11-25 ("A and B can be built in the same PR").

## Page A

**Crate.** `agentfs-repo`, `crates/agentfs-repo`, layer `tests`, `publish = false`. Deps: `serde`
(derive), `serde_json`. No agentfs crate. A crate so a docs-only change compiles two small deps, not
core (88 s).
**Public surface** (`src/lib.rs`: `pub mod doc_tables; pub mod workspace;`).
- `doc_tables.rs`, moved from `crates/agentfs/src/doc_tables.rs` (git mv; `#![allow(dead_code)]`
  and the `#[path]` paragraph go). Unchanged: `Section`, `Table`, `Table::column`, `Section::rows`,
  `heading`, `sections`, `split_row`, `code_spans`, `sole_code_span`, `section`. Changed and new:
  `pub fn read_doc(name: &str) -> String` (docs/`name` under `workspace::root()`; panics naming
  the path; the `manifest_dir` parameter is gone) and
  `pub fn key_cells(section: &Section) -> std::collections::BTreeSet<String>` (moved from
  `docs_reference.rs`: first cells that are exactly one code span; two users, core and repo).
- `workspace.rs`, new: `pub fn root() -> std::path::PathBuf` (two above this crate's manifest);
  `pub struct Workspace { pub layers: Vec<String>, pub members: Vec<Member> }`;
  `impl Workspace { pub fn load() -> Self; pub fn member(&self, name: &str) -> Option<&Member>; }`
  (`load` runs `env!("CARGO") metadata --format-version 1 --no-deps --offline --manifest-path
  <root>/Cargo.toml`; `layers` from `metadata.agentfs.layers`; panics on a cargo failure);
  `pub struct Member { pub name: String, pub dir: PathBuf /* relative: crates/agentfs-core */,
  pub layer: Option<String>, pub dependencies: Vec<Dependency>,
  pub features: BTreeMap<String, Vec<String>>, pub targets: Vec<Target> }`;
  `pub struct Dependency { pub name: String, pub kind: DependencyKind }`;
  `pub enum DependencyKind { Normal, Build, Dev }`;
  `pub struct Target { pub name: String, pub kinds: Vec<String> }`. All `Debug, Clone`.
**Metadata (in the skeleton).** Root: `[workspace.metadata.agentfs] layers = ["leaf", "kernel",
"domain", "engine", "filesystem", "kubernetes", "tests"]`. Each package:
`[package.metadata.agentfs] layer = "<one of them>"`: leaf = align, lines, failpoints, os, git;
kernel = contracts; domain = rules, volume, clients, trees; engine = core; filesystem = fuse,
agentfs; kubernetes = csi; tests = model, conformance, repo.
**Test moves.** One merged program `crates/agentfs-repo/tests/repo/main.rs`, modules `docs.rs`,
`ci.rs`, `crates.rs`. Out of `crates/agentfs-core/tests/docs_reference.rs`: 17 checks (links and
anchors, slugs, book, module map, runbook pushes and its parser test; runners, nextest use and pin,
Docker mirror, exit traps, toolchain pin, Dockerfile stubs, executable scripts, early-exit pipes and
its test, image push retry), helpers `workspace`, `modules`, `files_under`, `shell_words` with them.
`doc_tables`' own `tables_and_spans` moves with its file. What stays in core: the 2 checks of
`config::KEYS` and `rules::OPERATOR_ATTRIBUTES`, file renamed `tests/config_reference.rs`, over
`agentfs_repo::doc_tables`. agentfs's `reference_tests.rs` (one import; `read_doc` loses its first argument) and csi's
`main.rs` (its `#[path]` module goes; one import, one `read_doc`) switch too. core, agentfs and
agentfs-csi take `agentfs-repo` as a dev-dependency.
**Changed checks.** Module map: crates = `Workspace::load().members` minus named exclusions
`agentfs-align` (one module; its crate row says so) and `agentfs-model` (its modules are MODEL.md's);
`src` from `Member::dir`. Dockerfile stubs: members from `Workspace`, not the one-line `members =`.
**New checks** (each a pure function over a `Workspace` or a line, with a fixture test beside it).
1. `every_package_has_a_layer_and_depends_down`: every member's `layer` is in `layers`; for each
   Normal or Build dependency on a member, rank(dep) <= rank(member). Dev is exempt (agentfs's
   tests drive agentfs-csi). Fixture: a leaf depending on a kernel package is reported.
2. `every_failpoints_feature_is_forwarded`: a member declaring feature `failpoints` lists
   `<dep>/failpoints` (or `<dep>?/failpoints`) for each Normal dependency on a member that declares
   one. A crate with `cfg(feature = "failpoints")` but no feature is already refused by rustc's
   `unexpected_cfgs` under clippy `-D warnings`; a crate with only `hit` calls needs none, since
   B1's registry is switched once. Fixture: fuse without `agentfs-core/failpoints` is reported.
3. `cargo_selects_only_workspace_packages`: in `.github/workflows/*`, `ci/**`, `Dockerfile`,
   `docs/**/*.md`, `.claude/skills/**/*.md`, `crates/*/*.md`, `AGENTS.md`, `README.md`,
   `examples/**`, `deploy/**`: join `\`-continued lines, split with `shell_words` at `|`, `;`, `&&`,
   `||`; in each command whose word is `cargo`, every `-p X`, `-pX`, `-p=X`, `--package X`,
   `--package=X` (X matching `[A-Za-z0-9_-]+`, so `<crate>` is skipped) names a member, and X =
   `agentfs` only while member `agentfs` has a `bin` target named `agentfs`. Today all pass; at D
   (no package `agentfs`) and at E (the front, a library) every stale `-p agentfs` fails, with no
   flag. Fixture: a workspace whose `agentfs` has only a `lib` target reports `cargo build -p agentfs`.
4. `the_layers_are_documented`: docs/architecture.md "Layers" table's code cells equal `layers`, in order.
**CI and docs.** ci.yml docs job: `cargo nextest run --locked -p agentfs-repo`, then `-p agentfs-core
--test config_reference`; drop `doc_tables` from the agentfs step's filter; the job comment. AGENTS.md
"Local gates": a doc change runs `cargo nextest run -p agentfs-repo`, and a docs/configuration.md change
also `-p agentfs-core --test config_reference`. docs/development.md (test programs paragraph, runner
pin bullet), `ci/lib.sh` comment, `crates/agentfs-model/VERIFYING.md` (links command), core's
`config.rs` and `rules.rs` doc comments (`tests/config_reference.rs`), docs/architecture.md (module map
intro, the `agentfs` row of `doc_tables.rs` out, `agentfs-repo` rows `lib.rs`, `doc_tables.rs`,
`workspace.rs`). Dockerfile: no change (stub exists).
**Acceptance.** Gates on agentfs-repo, agentfs-core (`--test config_reference`), agentfs, agentfs-csi
(clippy and nextest); `grep -rn "#\[path" crates` shows no `doc_tables`; `crates/agentfs/src/doc_tables.rs`
gone. **Probes:** (1) add `agentfs-contracts` to agentfs-os's `[dependencies]` → check 1 names
`agentfs-os -> agentfs-contracts (leaf -> kernel)`; (2) drop `"agentfs-core/failpoints"` from
agentfs-fuse → check 2; (3) write `cargo build -p agentfs-cli` in `ci/fuse-tests.sh` → check 3; (4) swap
two rows of the Layers table → check 4; (5) delete agentfs-os's module section → the module map check.
**Files.** `crates/agentfs-repo/**`, `crates/agentfs/src/{doc_tables.rs,main.rs,reference_tests.rs}`,
`crates/agentfs/Cargo.toml`, `crates/agentfs-csi/{Cargo.toml,src/main.rs}`, `crates/agentfs-core/
{Cargo.toml,tests/docs_reference.rs→tests/config_reference.rs,src/config.rs,src/rules.rs}`,
`Cargo.lock`, `.github/workflows/ci.yml`, `ci/lib.sh`, `AGENTS.md`, `docs/development.md`,
`docs/architecture.md`, `crates/agentfs-model/VERIFYING.md`.

## Page B1

**Crates** (all layer `leaf`; each a crate so the tree engine (C) and the recording engine share them
as siblings and M9 can delete core whole).
- `agentfs-failpoints` (`crates/agentfs-failpoints`). Deps `libc`, `thiserror`; dev `tempfile`,
  `rustix` (fs), `agentfs-repo`. Feature `failpoints = []`. `src/lib.rs` = today's
  `failpoints.rs` (one module crate). Surface unchanged (`ENV_VAR`, `COMPACT_GATE_VAR`, `POINTS`,
  `hit`, `owner`, `adopt`; with the feature `FailpointError`, `Guard`, `configure`, `hits`, `trace`,
  `TakeGate`, `take_gate`, `FifoGate`) except: `FifoGate` no longer implements `CompactGate`, and gains
  `pub fn park(&self, place: &str) -> std::io::Result<()>` (today's `CompactGate::at` body with
  `gap.name()` → `place`: write `place` to `at`, read `go`; `go` → Ok, `fail` → EIO, empty or EOF ends
  the gate). `every_hit_is_listed` scans `src` of every `Workspace::load()` member, not a name list.
  `POINTS` stays one list, grouped by comment per engine.
- `agentfs-os` (`crates/agentfs-os`). Deps `agentfs-failpoints`, `libc`, `rustix` (fs, process),
  `serde`, `serde_json`, `thiserror`, `tokio` (net), `tokio-stream`; dev `tempfile`, `tokio` (rt).
  Feature `test-util = []` (tokio's idiom). Modules, moved whole: `allocator`, `confined`,
  `file_lock`, `fsutil`, `listening_socket`, `local_socket`, `mountinfo`, `shared_fs`. Shape changes:
  `confined::Dir::sha256_at` leaves (to core `lfs::pointer::sha256_at(tree: &Dir, rel: &str) ->
  io::Result<(LfsOid, u64)>`); `confined::{Current, FileState}` and `Dir::current_entry` leave (git
  blob ids and `RepoPath`: to `restore.rs`, private: `enum Current`, `struct FileState`,
  `fn current_entry(tree: &Dir, path: &RepoPath) -> io::Result<Current>`; one consumer);
  `file_lock::takes_while_children_spawn` becomes `#[cfg(any(test, feature = "test-util"))] pub fn
  takes_while_children_spawn(take: impl FnMut() -> bool) -> (usize, usize)` (4 core test users).
  Doc links to core items (`crate::lock::StateLock`, ...) become plain text.
- `agentfs-git` (`crates/agentfs-git`). Deps `agentfs-align`, `agentfs-os`, `agentfs-failpoints`,
  `git2` (`default-features = false`), `libgit2-sys` (`default-features = false`), `libc`, `rustix`,
  `thiserror`; dev `tempfile`. Modules: `git_cli` (moved; gains `pub const PACK_CONFIG: [&str; 12]`
  from `repack.rs`; `counted` and `children` widen `pub(crate)` → `pub`: the tree engine's stages and
  B2's callers use them), `tree_git` (moved; `object_writes`, `object_dir_syncs` widen to `pub`:
  `tree_actor::stages`), `setup` (private; root re-export): `pub fn init()` (today's `init_libgit2`
  minus the SSH registration: fsync, search paths, home directory, timeouts, in one `Once`),
  `pub const SERVER_CONNECT_TIMEOUT_MS: std::ffi::c_int = 30_000`,
  `pub const SERVER_IO_TIMEOUT_MS: std::ffi::c_int = 300_000`.
**Cycle cuts.** `failpoints.rs:141`: `#[cfg(feature = "failpoints")] impl CompactGate for
agentfs_failpoints::FifoGate { fn at(&self, gap: Gap) -> io::Result<()> }` (parks at `gap.name()`)
in `tree_actor/journal/compaction.rs`. `init_libgit2` called core's `ssh_transport::register` inside
its `Once`: core's `history::init_libgit2` is deleted; core gets `pub(crate) fn init_git()` in
`history.rs` (`agentfs_git::init()`, then with `ssh` `ssh_transport::register()`, which gets its own
`Once`); the recording engine's ~20 calls rename to it; the tree engine's 4
(`tree_actor/mod.rs:688`, `:803`, `tree_git.rs:575`, `history/verify.rs:686`) call `agentfs_git::init()`.
`tree_actor/history/repack.rs:98` imports `agentfs_git::git_cli::PACK_CONFIG`; core's `repack.rs` too.
**Checks.** clippy.toml: the `FileLock`, `Listener` reasons name `agentfs_os::...`; the four `TreeGit`
bans become `agentfs_git::tree_git::TreeGit::{open,open_with_lease,init,init_with_lease}`; new bans
(`allow-invalid = true`), reason "libgit2 is set up once, by agentfs_git::init":
`libgit2_sys::git_libgit2_opts`, `libgit2_sys::init`, `git2::opts::set_search_path`,
`git2::opts::set_server_connect_timeout_in_milliseconds`, `git2::opts::set_server_timeout_in_milliseconds`;
`setup.rs` alone allows them. Core drops `libgit2-sys`. Features: core `failpoints =
["agentfs-failpoints/failpoints"]`; fuse adds `"agentfs-failpoints/failpoints"`; agentfs adds it too.
**Consumers** (no re-exports from core). core: failpoints 116, os modules 83, git 82; fuse: failpoints
7, os 8; agentfs: failpoints 8, os 17, git 4; csi: os 4 (allocator, fsutil, listening_socket,
shared_fs); conformance: 1 (`COMPACT_GATE_VAR`). Each adds the crates it imports.
**Test moves.** Unit tests move with their files. `tests/it/tree_git.rs` → `crates/agentfs-git/tests/
tree_git.rs`. History's `libgit2_home_directory_is_cleared`, `libgit2_server_timeouts_are_set`,
`init_libgit2_clears_config_search_paths` → `setup.rs` (over `init()`, no core fixture). `tree_git`'s
`classify_tests`: the variant stays; its `tree_actor::Error` errno mapping moves to core's tree_actor
tests. FifoGate's test is rewritten over `park("covered")` and friends. confined's `current_entry`
tests → `restore/tests.rs`. `tests/tree_stages.rs::one_spawn_site` reads `../agentfs-git/src/
{tree_git.rs,git_cli.rs}`.
**Docs.** architecture.md (core rows of the 11 files out; rows for the new crates' modules; the crate
rows; Technology choices' libgit2 setup), library/core.md (module table rows, "Locks and sockets",
`repack::PACK_CONFIG`, `confined::Dir`, `tree_git::TreeGit`), configuration.md (`TakeGate`,
`FifoGate` paths), csi.md (`shared_fs`), development.md where it names a moved module.
**Acceptance.** None of `crates/agentfs-core/src/{allocator,confined,file_lock,fsutil,listening_socket,
local_socket,mountinfo,shared_fs,failpoints,git_cli,tree_git}.rs` exists; `grep -rn "agentfs_core::
\(allocator\|confined\|file_lock\|fsutil\|listening_socket\|local_socket\|mountinfo\|shared_fs\|
failpoints\|git_cli\|tree_git\)\|init_libgit2" crates docs clippy.toml` is empty; `git2::opts::set_\|
git_libgit2_opts` only in `crates/agentfs-git/src/setup.rs`. Gates (clippy and nextest, default and
`--features failpoints`) on the five changed crates, the three new ones and conformance; agentfs-repo.
**Probes:** (1) call `git2::opts::set_search_path` in core → clippy; (2) `agentfs_failpoints::hit("os.x")`
in agentfs-os → `every_hit_is_listed`; (3) drop fuse's `"agentfs-failpoints/failpoints"` → agentfs-repo;
(4) `git2 = "0.21"` with default features in agentfs-git → `ci/check-deps.sh` (default build links
OpenSSL); (5) delete the `CompactGate` impl → `cargo clippy -p agentfs --features failpoints`.
**Files.** `crates/agentfs-{failpoints,os,git}/**`; core's 11 moved files, `history.rs`,
`ssh_transport.rs` and its tests, `repack.rs`, `restore.rs`, `restore/tests.rs`, `lfs/pointer.rs`,
`tree_actor/**`, `lib.rs`, `Cargo.toml`, `tests/**`; fuse, agentfs, csi, conformance sources and
manifests; `clippy.toml`; `Cargo.lock`; the docs above.

## Page B2

**Crate.** `agentfs-lines` (`crates/agentfs-lines`), layer `leaf`. Deps `agentfs-align`, `serde`
(derive), `serde_json`, `thiserror`. No git2, so G's pure referee can build on it. `src/lib.rs` =
today's `tree_reconcile.rs` (one module crate; `pub use agentfs_align::Hunk` stays: it is in the
signatures). Surface unchanged (`Id`, `Image`, `FileImage`, `Window`, `LineIndex`, `Run`, `Over`,
`Line`, `Expanded`, `LineMap`, `Error`, `Piece`, `pieces`, `render`) except the cut below.
**Cycle cut.** `tree_reconcile.rs:1174`, `:1189` call `crate::git_cli::counted`; lines takes the
spawner, as `agentfs_align::histogram_with` does:
`pub fn histogram(old: &[u8], new: &[u8], spawn: impl FnOnce(&mut std::process::Command) ->
std::io::Result<std::process::Child>) -> Result<Vec<Hunk>, Error>`;
`pub fn histogram_files(old: &Path, new: &Path, spawn: impl FnOnce(&mut Command) -> io::Result<Child>)
-> Result<Vec<Hunk>, Error>`; `impl<V: Id> LineMap<V> { pub fn align_bytes(&self, old: &[u8],
new: &[u8], spawn: impl FnOnce(&mut Command) -> io::Result<Child>, next: impl FnMut(u64) -> V) ->
Result<Self, Error> }`. The tree builder passes `agentfs_git::git_cli::counted`
(`builder.rs:1429`), so each operation's count of git children stays exact.
**Contracts.** `agentfs_contracts::paths`: today's `paths.rs` whole (`RepoPath`, `PathError`,
`is_dotgit`, `SpecialFile`, `special_file`, `must_not_be_symlink`; pure, serde and thiserror, which
contracts has), with its tests. `agentfs_contracts::units` (new `units.rs`): `pub fn
parse_duration(value: &str) -> Option<std::time::Duration>`, `pub fn parse_size(value: &str) ->
Option<u64>`, moved from `config.rs:1887`, `:1904`, with their tests (`config.rs` ~2750-2765).
**csi.** `layout.rs:85`, `:90`: `agentfs_contracts::name::is_safe_name(x)` → the same `LayoutError`;
`main.rs:16` `parse_duration` from `agentfs_contracts::units`. csi then uses core only for
`operator_settings` and `tree_actor::inspect` (C).
**Consumers.** core: `paths::` 44, `tree_reconcile::` 19, `parse_*` 7 (config, fsync_class,
operator_settings, the `record_step` bench); fuse: `paths::` 9; agentfs: `paths::` 6, `parse_duration`
2 (main.rs, lfs_cmd.rs); csi: 3. agentfs-core and agentfs-fuse already depend on contracts.
**Test moves.** `tests/it/tree_reconcile.rs` → `crates/agentfs-lines/tests/line_maps.rs`;
`tests/tree_stages.rs::one_spawn_site` reads `../agentfs-lines/src/lib.rs` for the old
`tree_reconcile.rs`; `referee/mod.rs:426`'s test passes a spawner.
**Docs.** architecture.md (core rows `paths.rs`, `tree_reconcile.rs` out; contracts rows `paths.rs`,
`units.rs`; lines `lib.rs`; contracts' crate row), library/core.md (module table rows 33 and 42, the
`RepoPath` example's import), AGENTS.md's contracts row if its list of values changes.
**Acceptance.** The four edges of crate-split.md section 2 are gone: `grep -rn "init_libgit2\|
repack::PACK_CONFIG\|config::parse_size\|crate::paths" crates/agentfs-core/src/tree_actor
crates/agentfs-core/src/operator_settings.rs` is empty; `grep -rn "agentfs_core::config"
crates/agentfs-csi` is empty; `grep -rn "agentfs_core::\(paths\|tree_reconcile\)\|config::parse_\(size\|
duration\)" crates docs` is empty. Gates on contracts, lines, core, fuse, agentfs, csi; agentfs-repo.
**Probe:** make `histogram` call `Command::spawn` itself → `tree_stages::one_spawn_site` fails.
**Files.** `crates/agentfs-lines/**`, `crates/agentfs-contracts/{src/lib.rs,src/paths.rs,
src/units.rs}`, core's `paths.rs`, `tree_reconcile.rs`, `config.rs`, `fsync_class.rs`,
`operator_settings.rs`, `lib.rs`, `tree_actor/**`, `benches/record_step.rs`, `tests/**`,
`Cargo.toml`; fuse, agentfs, csi sources and manifests; `Cargo.lock`; the docs above.

## Decisions

1. **One home for the markdown reader: agentfs-repo's library**, a dev-dependency of core, agentfs and
   agentfs-csi. It replaces two `#[path]` includes; repo depends on no agentfs crate, so no cycle.
2. **Layers in Cargo's own metadata tables**, `[package.metadata.agentfs] layer` and
   `[workspace.metadata.agentfs] layers`, read with `cargo metadata --no-deps` (the documented
   interface) through serde_json. The `cargo_metadata` crate would add three packages (it, `camino`,
   `cargo-platform`) to the lockfile for six fields. Same-layer edges are allowed (lines → align, agentfs → fuse); Cargo already refuses
   cycles. Cost: a domain may depend on another domain unchecked. Only layers with a member today; E/F
   insert theirs.
3. **`-p agentfs` check activates itself**: it reads whether package `agentfs` holds the `agentfs`
   binary, so it passes today and fails at D and E with no flag to flip.
4. **Failpoints forwarding** checked over manifests; rustc's `unexpected_cfgs` already covers cfg
   code without a feature.
5. **Deviation: confined loses its git part too.** `Dir::current_entry` hashes git blobs (`git2`)
   and takes `RepoPath`; agentfs-os knows no git, so it goes to its one consumer, `restore.rs`.
6. **Deviation: the SSH transport stays in core.** `init_libgit2` registered core's transport inside
   its `Once`. `agentfs_git::init` sets every libgit2 option, enforced by clippy bans and core losing
   `libgit2-sys`; core's `init_git` adds the transport. Rename cost: ~20 recording-engine calls.
7. **Deviation: lines takes the spawner.** `tree_reconcile` used `git_cli::counted`; depending on
   agentfs-git would give lines, and G's referee, a git2 dependency. Section 1 draws lines → align only.
8. **`test-util` feature on agentfs-os** for the one shared test helper (tokio's idiom); the
   alternative was copying it into core.
9. **`PACK_CONFIG` in `git_cli`**: it is the `git` program's configuration. `COMPACT_GATE_VAR` and
   `TakeGate` stay in the registry: neither names an engine type.
10. **One `POINTS` list**, grouped by comment; its scan is derived from the workspace.
11. **core's remaining doc test renamed `config_reference`**, so no two programs share `docs_reference`.

**Self-review** (no separate review, per implementation-workflow section 2). Section 2's file:line
items re-read at 264a494: all still accurate. Three unnamed edges were found (decisions 5, 6, 7),
each cut without changing the design's crate graph. Goals: "a docs-only change builds in seconds" →
A; "makes C and M9 possible" → B1 and B2 leave core no leaf both engines share; "four edges gone" →
B2's grep. Not covered here: the edges stay back-edges inside one crate until C, so between B2 and C
only the grep holds them (the clippy bans cannot scope to a module). Untested until the slices land:
every check above.
