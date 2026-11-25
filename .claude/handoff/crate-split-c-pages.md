# Crate split, PR C: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row C; section 1 the end state; section 2 what is
left). Branch `claude/bold-hopper-72ixu2`, from main 2b2f6f0. Slices run in order C1, C2, never in parallel: each changes
`Cargo.toml` or `Cargo.lock`, and C2's mechanical move needs C1's cuts. The skeleton (fab306b, revised in the commit
carrying this revision) holds: `agentfs-engine` (rank `engine`) and `agentfs-test-metrics` (rank `tests`), each a
manifest and a `lib.rs` doc comment saying what it holds now (nothing); the workspace members; `[workspace.package]
version = "0.1.0"`, which every published member takes (`version.workspace = true`); `Cargo.lock`; the Dockerfile stubs;
each crate's module-map section in docs/architecture.md (`lib.rs` only, the module-map test needs it). The Crates and
Layout rows land with the code (test-metrics in C1, the engine in C2). Owner's words: OWNER-GUIDANCE.md "Performance"
(2026-11-24, 2026-11-25), and on PR C: "Go". Counts are `git grep -nE` lines at fab306b. A/B idioms hold: ranks,
agentfs-repo's checks, clippy bans with `#[expect]` at each sanctioned site, no re-exports from core.

## Page C1

**Title.** The tree engine's instruments on the binary's `Meter`; `Found` read only through the `StateArea` port, with a
read failure of its own; one in-memory metric reader. Inside existing crates, so C2 is a pure move.
**`agentfs-test-metrics`** (tests rank; deps `opentelemetry` 0.33.1 `metrics`, `opentelemetry_sdk` 0.33.1 `metrics`,
`testing`). The metric half of core's `telemetry::testing::Harness` moves here whole (`Point`, `collect`, `point`, `sum`,
`gauge_u64`, `gauge_f64`, `histogram_count`, `attrs_match`, `find_point`: telemetry.rs:1430-1600 today):
```rust
#[derive(Debug, Clone, PartialEq)] pub enum Point { U64(u64), F64(f64), I64(i64), /** a histogram's count */ Count(u64) }
pub struct MetricReader { /* SdkMeterProvider with a PeriodicReader over an InMemoryMetricExporter */ }
impl Default for MetricReader { fn default() -> Self; }
impl MetricReader {
    pub fn new() -> Self;
    pub fn meter(&self, scope: opentelemetry::InstrumentationScope) -> opentelemetry::metrics::Meter; // the #[expect]
    pub fn collect(&self) -> Option<opentelemetry_sdk::metrics::data::ResourceMetrics>;
    pub fn point(&self, name: &str, attrs: &[(&str, &str)]) -> Option<Point>; // exact attributes
    pub fn sum(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64>;      // panics on another kind,
    pub fn gauge_u64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64>; // as today
    pub fn gauge_f64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<f64>;
    pub fn histogram_count(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64>;
}
```
Core's `Harness` keeps spans and logs and gains `pub(crate) metrics: MetricReader`; its telemetry is built on
`metrics.meter(scope())`. Call sites `h.{sum,gauge_u64,gauge_f64,histogram_count,point}(` → `h.metrics.…(` (55 lines in
`telemetry.rs`, `volume.rs`, `push.rs`, `service.rs`). Core dev-depends on it; `opentelemetry_sdk` stays a core
dev-dependency for spans and logs.
**New module** `crates/agentfs-core/src/tree_telemetry.rs` (+ `tree_telemetry/tests.rs`), `pub mod` in core's `lib.rs`;
C2 moves it as `agentfs_engine::telemetry`. Doc: the tree engine's OpenTelemetry instruments, registered on a `Meter`
its caller makes (the binary); docs/telemetry.md lists them. Names `crate::tree_actor` and `crate::operator_settings`
only. Bodies move from `Telemetry` (`telemetry.rs:644-822`), `inner.meter` → `meter`, behaviour unchanged:
```rust
mod names { /* private: the 15 consts, values as today (NOT_BUILT .. REQUEST_LONGEST) */ }
/// `agentfs.not_built` and the latency figures (lock, gate, requests, checkpoints). Call once per process.
pub fn observe_process(meter: &opentelemetry::metrics::Meter, not_built: &[crate::operator_settings::NotBuilt]);
/// `agentfs.history.unverified`, read from `counts` while it lives. Call once per served volume.
pub fn observe_unverified(meter: &opentelemetry::metrics::Meter,
    counts: std::sync::Weak<dyn Fn() -> crate::tree_actor::UnverifiedCounts + Send + Sync>);
```
Telemetry off is the no-op meter, which drops callbacks: no caller branches. Core's `telemetry.rs` loses the three methods,
the 15 names, the `operator_settings` import and three tests (`not_built_settings_are_counted_by_capability`,
`the_latency_figures_are_observed_by_attribute`, `unverified_saves_are_observed_by_verdict_zeros_included`), which move to
`tree_telemetry/tests.rs` over `MetricReader`. After C1 `telemetry.rs` names no tree-engine module (section 2's lines and `:48`).
**The binary.** `otel.rs`: `Otel { telemetry, meter: Meter, providers }`; `pub fn meter(&self) -> &Meter`. One private
`fn meter_of(provider: Option<&SdkMeterProvider>) -> Meter` (the provider's meter under `telemetry::scope()`, else the
API's `NoopMeterProvider`'s) is the one `#[expect]` site; `Otel::noop`, `init` (both branches) and `shutdown` (which
drops the provider's meter before the providers, as it does the telemetry) call it. `launcher/host.rs:117-118`,
`launcher/mod.rs:416-417` → `tree_telemetry::observe_process(otel.meter(), &boot.not_built)`; `launcher/mod.rs:75` `fn
observe_unverified(meter: &Meter, domains: &Domains) -> Result<UnverifiedGauge>`; `Mount.telemetry` (`:553`, `:453`,
`:626`) → `meter: Meter`. The launcher no longer names core's `Telemetry`.
**The port.** contracts `volume.rs`: `VolumeError` gains `#[error("the volume's state area could not be read: {0}")]
Unreadable(String)` (the path and the I/O failure; nothing written). `StateArea::inspect`'s doc: "`Unreadable` for a read
that failed"; `StateArea::create`'s and `Volume::ensure`'s error lists add it. `TreeStateArea::inspect` maps `io::Error`
→ `Unreadable(format!("{}: {e}", root.display()))`, no stderr line (`volume_error` stays for the durable steps).
`tree_actor::inspect` (`mod.rs:465`) → `pub(crate)`. Callers → `TreeStateArea::at(path).inspect()`: csi `controller.rs:79`
(`map_err(|e| Error::Internal(e.to_string()))`, message keeps path and cause), `supervisor.rs:1273`; the binary's `found`
(`launcher/mod.rs:244`, `?`); agentfs `tests/csi_trees.rs:530`, `:572`. csi's `use agentfs_core::tree_actor` lines go.
Scope of the claim: what a state area holds (`Found`) is read only through the port. `tree_actor::holds_tree_format`
(main.rs:787, the recording commands' guard) stays public: it asks another question (any tree marker, identity unread),
for a recording-engine guard no domain owns; it goes with those commands at M9. `operator_settings::{install_rules,
write_includes}` stay public: they write operator policy into the git directory (principle 3), which no domain port owns.
**New checks.** (1) clippy.toml, reason "a Meter is made only by the binary's otel::meter_of and agentfs-test-metrics":
`opentelemetry::global::{meter, meter_with_scope, meter_provider, set_meter_provider}`,
`opentelemetry::metrics::MeterProvider::{meter, meter_with_scope}`, `opentelemetry::metrics::Meter::new`, all
`allow-invalid = true`; two `#[expect]` sites [checked in a scratch crate: `Meter::new` and the trait's `meter_with_scope`
resolve and fire; `#[expect]` silences]. (2) agentfs-repo `ci.rs` `workflow_path_filters_name_what_exists`: each `- entry`
under a `paths:`/`paths-ignore:` key (an `&anchor` allowed) of `.github/workflows/*.yml`, unquoted, `!` stripped: no `*`
→ the path exists; else the text before the first `*` prefixes some path under the root (`target/`, `.git/` skipped).
(3) `TREE_ENGINE` (crates.rs:1002) gains `"tree_telemetry"`. (4) `rank_problems`: a dev-dependency on another member of
its own rank is refused unless the rank uses itself ("`a -> b (dev; engine does not use engine)`"); today none exists
(cargo metadata). Cargo.toml's ranks comment and architecture.md "Ranks" ("A dev-dependency may point anywhere but at a
sibling of a rank that does not use itself"). (5) `cargo_commands_name_what_the_workspace_has`: `Selection` gains
`tests` (`--test T`, `--test=T`); with literal packages, a `T` no selected package has as a target of kind `test` is
refused ("--test T: no selected package has that test program"); fixture gains a test target.
**CI and docs.** alerts.yml paths `+ crates/agentfs-core/src/tree_telemetry.rs`. docs/telemetry.md:49 (the tree
figures through `agentfs_core::tree_telemetry`, on the binary's Meter); library/core.md "Events and telemetry", offers
row and tree section (`TreeStateArea`'s `inspect`; `VolumeError::Unreadable`); architecture.md (`tree_telemetry.rs` row;
`telemetry.rs` row loses the `agentfs.not_built` gauge; test-metrics' Crates row; `tests` rank row adds the metric
reader); AGENTS.md Layout row for `crates/agentfs-test-metrics`; csi's `Cargo.toml` comment on agentfs-core.
**Acceptance.** `git grep -nE "crate::(tree_actor|operator_settings)" crates/agentfs-core/src/telemetry.rs` empty; `git
grep -n InMemoryMetricExporter crates` only in agentfs-test-metrics; `git grep -nE "tree_actor::inspect\b" -- crates
':!crates/agentfs-core/src/tree_actor' ':!crates/agentfs-core/src/state_area.rs'` empty; no `if let Some` on a meter.
Gates, one call each: `cargo clippy -p agentfs-contracts -p agentfs-volume -p agentfs-test-metrics -p agentfs-core -p
agentfs -p agentfs-csi -p agentfs-repo --all-targets -- -D warnings`, again `--features failpoints` for the packages that
have it; `cargo nextest run` the same, both ways.
**Probes.** (1) `opentelemetry::global::meter("x")` in `tree_telemetry.rs` → clippy. (2) alerts.yml names
`crates/agentfs-core/src/nowhere.rs` → check (2). (3) `agentfs_core::tree_actor::inspect(&state)` in csi → E0603. (4)
`use crate::telemetry::Telemetry;` in `tree_telemetry.rs` → `the_tree_engine_names_no_recording_engine_module`. (5)
`agentfs-volume` in agentfs-trees' `[dev-dependencies]` → check (4). (6) development.md `-p agentfs-core --test
tree_stage` → check (5).
**Files.** `crates/agentfs-test-metrics/**`; core `src/{lib,telemetry,tree_telemetry,state_area,volume,push,service}.rs`,
`src/tree_telemetry/tests.rs`, `src/tree_actor/mod.rs`, `Cargo.toml`; contracts
`src/{volume,ports}.rs`; agentfs `src/otel.rs`, `src/launcher/{mod,host}.rs`, `tests/csi_trees.rs`; csi
`src/{controller,supervisor}.rs`, `Cargo.toml`; agentfs-repo `tests/repo/{ci,crates}.rs`; `clippy.toml`; `Cargo.toml`
(comment), `Cargo.lock`; alerts.yml; the docs named.

## Page C2

**Title.** `agentfs-engine` takes the tree engine whole. **Crate** `crates/agentfs-engine`, rank `engine` (the skeleton's).
Why a crate: the aggregate (one actor, one lock, one unit of work) apart from the recording engine, so an edit of one
builds and tests only its own (crate-split.md section 1). Deps: `agentfs-contracts`, `-failpoints`, `-git`, `-os`,
`-align` (`isolated`), `-lines`, `-rules`; `crc32fast`, `git2` (`default-features = false`), `imbl`, `libc`,
`opentelemetry` (as core's line), `rustix` (fs, process), `serde` (derive), `serde_json`, `sha2`, `thiserror`, `uuid`
(std, v4). Dev: `agentfs-volume`, `-trees`, `-clients`, `agentfs-test-metrics`, `tempfile`. Feature `failpoints =
["agentfs-failpoints/failpoints"]`. No https/ssh: the engine opens no remote (clippy's bans). No dev edge to core
(C1's check 4).
**Move map** (`git mv`, contents unchanged but paths): core `src/tree_actor/**`, `src/{tree_git, session, store,
state_area, operator_settings}.rs` → `crates/agentfs-engine/src/` same names; `src/tree_telemetry.rs`,
`src/tree_telemetry/tests.rs` → `src/telemetry.rs`, `src/telemetry/tests.rs` (the crate tells it from core's; 3 `crate::
tree_telemetry` lines change). Engine `lib.rs`: the seven `pub mod`s; its doc names docs/library/engine.md. Core `lib.rs`
drops them; its doc says it is the recording engine. No item widens: every `crate::` path in the moved files names a
moved module (A's check proves it before the move), and C1 cut the one reverse edge. The 497 `pub(in crate::tree_actor)`
and one `pub(in crate::tree_actor::journal)` compile unchanged. Core's manifest drops `agentfs-align`, `agentfs-lines`,
`imbl`, `uuid` and dev `agentfs-volume`, `-trees`, `-clients` (the build says if more go).
**Consumers** (`agentfs_core::{tree_actor,tree_git,session,store,state_area,operator_settings}` → `agentfs_engine::…`,
`agentfs_core::tree_telemetry` → `agentfs_engine::telemetry`): fuse 14 lines in 3 files (`tree_view.rs`,
`tree_view/{save_worker,untracked}.rs`); agentfs 31 in 13 (src 18: `launcher/{domains,host,mod,serving,settings,verify}.rs`,
`main.rs`, `reference_tests.rs`, plus C1's; tests 13: `cli/{cli_mount,cli_verify,deploy_examples}.rs`, `csi_trees.rs`,
`launcher_host.rs`); csi 3 in 2 (drops `agentfs-core`, gains `agentfs-engine`); conformance 9 in 4 (likewise);
clippy.toml 10 lines (`TreeGit::{open,open_with_lease,init,init_with_lease}`, `TreeActor::{fork_with,record_rows,
drop_fork,close_writer}`, reasons naming `store`, `session`). fuse and agentfs gain `agentfs-engine` and
`"agentfs-engine/failpoints"`. After C2 agentfs-core is depended on by agentfs-fuse (the recording front: `Ops`, backing,
overlay, `Telemetry`) and agentfs (the recording commands, `telemetry::scope`); csi reaches it only through fuse
(`mount_state`; crate-split.md section 1); conformance not at all.
**Test moves.** Engine `tests/it/main.rs` (from core's: `test_name!`, `qualified`, `#[path] mod trees`) with
`state_area`, `tree_actor`, `tree_clients`, `tree_dir_landing`, `tree_dirs`, `tree_git`, `tree_line_rules`,
`tree_overlay_limit`, `tree_pages`, `tree_pending_saves`, `tree_replace`, `tree_rules`, `tree_store`, `tree_untrack`,
`tree_verify` (its doc's command → `-p agentfs-engine`); core's `it/main.rs` keeps `gitmodules_git`,
`lfs_attributes_git`, `patch_fetch` and loses the macro. `tests/{tree_batch_alloc,tree_close,tree_overlay_alloc,
tree_stages}.rs`, `tests/support/trees.rs` → engine `tests/`. `crash_points.rs`'s tree half
(`a_kill_after_a_write_returned_keeps_it`, `:748-794`) → `tests/it/tree_actor.rs` under `#[cfg(feature =
"failpoints")]`, its child named by `test_name!`; core's `crash_points.rs` drops `mod trees`. Unit tests move with files.
**agentfs-repo.** Delete `TREE_ENGINE`, `tree_engine_files`, `crate_paths`, `recording_engine_uses`,
`the_tree_engine_names_no_recording_engine_module`, `a_tree_engine_use_of_the_recording_engine_is_found` (the boundary
replaces them: the rank check refuses engine → engine, normal or dev, and rustc an import of a crate not depended on);
`the_tree_engine_starts_git_only_through_counted` reads every file under agentfs-engine's `src` but
`tree_actor/history/repack.rs`, and agentfs-lines'; main.rs's doc list.
**CI, model, fixtures.** phase-1.yml:265 `-p agentfs-engine --test tree_stages`; alerts.yml
`crates/agentfs-engine/src/{telemetry,operator_settings}.rs` for core's two; ci/crash-tests.sh comment (core's matrix
is the recording engine's points; the tree kill tests are the engine's `it` program's, run by the test job);
agentfs-model `tests/ledger.rs:232` reads `crates/agentfs-engine/tests`; conformance
`regressions/finding-51-sync-fast-forward.patch` paths. phase-1.yml's path filters stay `crates/**` (Decisions 5).
**Docs.** New docs/library/engine.md ("The library: `agentfs-engine`"): core.md's offers rows for tree views, rows and
line identity; "Git-backed tree views" through "Domain invariants" (core.md:652-1138); the telemetry paragraph; its
`tree_actor::Error` row; what it does not do. core.md keeps the recording engine and links engine.md. SUMMARY.md and
docs/README.md list it. Anchors `core.md#` → `engine.md#` (runbooks/recording-failures.md, target-architecture/{system,
domains/{clients,rules,volume}, future/one-engine}.md, telemetry.md, trees.md: 13 links). `agentfs_core::` →
`agentfs_engine::` in trees.md:644, :660, library/{filesystem.md:634, :786, kubernetes.md:145},
target-architecture/domains/views.md:107, performance.md:793, telemetry.md:49. supported-configurations.md:61: a Library
row for `agentfs-engine` (tree views: `Recording`, `TreeStateArea`, sessions, stores; module map
`architecture.md#agentfs-engine`, page library/engine.md), core's row narrowed to the recording engine. README.md:253
names `agentfs-core`, `agentfs-engine`, `agentfs-fuse`, `agentfs-csi`, linking engine.md. architecture.md: the moved
module rows into the `agentfs-engine` section; Crates rows: engine (new), core ("the recording engine"), conformance
(depends on the engine). AGENTS.md: Layout rows (engine new, core narrowed), principle 10's mapping names both engines,
local gates' `--test it tree_actor::` example → `-p agentfs-engine`. development.md:98, :114, "Test programs", "Build
settings": measured `cargo nextest run -p agentfs-engine` and `-p agentfs-core`, warm after a one-file edit in each.
crate-split.md: row C built, section 2's C items out. CHANGELOG.md and OWNER-GUIDANCE.md keep their dated paths.
**Acceptance.** The moved files gone from core; `git grep -nE "agentfs_core::(tree_actor|tree_git|session|store|
state_area|operator_settings|tree_telemetry)\b" -- ':!.claude' ':!CHANGELOG.md' ':!OWNER-GUIDANCE.md'` empty; `git grep -c
"pub(in crate::tree_actor)" crates/agentfs-engine` sums to 497; `cargo tree -p agentfs-engine -e normal,dev -i
agentfs-core` and `cargo tree -p agentfs-conformance -i agentfs-core` find nothing; `git apply --check` of both
regression patches exits 0. Gates: `cargo clippy -p agentfs-engine -p agentfs-core -p agentfs-fuse -p agentfs -p
agentfs-csi -p agentfs-conformance -p agentfs-model -p agentfs-repo --all-targets -- -D warnings`, again `--features
failpoints`; `cargo nextest run` the same, both ways; `-p agentfs-model --test ledger` among them.
**Probes.** (1) `agentfs-core` in the engine's `[dependencies]` → `dependencies_follow_the_ranks` (`agentfs-engine ->
agentfs-core (engine does not use engine)`); in its `[dev-dependencies]` → the same check, `dev;` form. (2)
`cmd.output()` in engine `tree_actor/stages.rs` → the spawn check. (3) `TreeGit::open` in agentfs-fuse → clippy. (4)
alerts.yml keeps `crates/agentfs-core/src/operator_settings.rs` → C1's path-filter check. (5) ledger.rs keeps
`agentfs-core/tests` → `named_tests_exist`. (6) drop `"agentfs-engine/failpoints"` from fuse → A's check 2. (7)
development.md keeps `-p agentfs-core --test tree_stages` → C1's `--test` check.
**Files.** `crates/agentfs-engine/**`; core `src/lib.rs`, the moved files, `tests/**`, `Cargo.toml`; fuse, agentfs,
csi, conformance sources and manifests; `crates/agentfs-repo/tests/repo/{crates,main}.rs`;
`crates/agentfs-model/tests/ledger.rs`; the regression patch; `clippy.toml`; `Cargo.lock`; phase-1.yml, alerts.yml,
ci/crash-tests.sh; README.md; the docs named.

## Decisions

**Slice order.** C1 then C2. C1 is everything that is not a move (the observers out of core's `Telemetry`, the port and
its error, the shared metric reader, the checks), inside existing crates; C2 is then a move and a rename that the
compiler and the checks judge. C2 cannot come first: core's `telemetry.rs` names `crate::tree_actor`. Two slices, not
three: docs travel with the code (AGENTS.md "Git"), and the module-map test forces architecture.md into C2 anyway.

**Design choices.**
1. The engine's instruments are free functions over `&Meter` in their own module: `tree_telemetry` in core (C1, where
   `telemetry` is taken), `agentfs_engine::telemetry` after the move (C2), the name it keeps after M9. Two calls, not
   three: `observe_process` (both process-wide registrations, always made together) and `observe_unverified` (per
   served volume, later). `names` is private: nothing outside the module names a metric.
2. The binary holds the `Meter` (`Otel::meter() -> &Meter`), not core's `Telemetry`: M9 deletes core. One decider for
   "metrics off": the no-op meter, made at one site (`meter_of`); no `Option<Meter>` anywhere.
3. Deviation (stronger than the page): clippy bans the trait's `MeterProvider::{meter, meter_with_scope}` and
   `Meter::new` as well as `global::meter*`. Gap: a library could build an SDK provider; no library has the SDK outside
   dev-dependencies (not checked).
4. Deviation (wider than the page): `tree_actor::inspect` becomes `pub(crate)`, so the binary and its tests, not only
   csi, read `Found` through the port. The port's failed read gets its own error, `VolumeError::Unreadable(String)`:
   durability is not borrowed to describe a read. The recording guard's format check and the operator-settings writers
   stay public, with the reasons on page C1.
5. Deviation: phase-1.yml's path filters stay `crates/**`. Every phase-1 job builds `agentfs`, which links core, so a
   core edit can change a tree mount; filtering by crate waits for M9.
6. One in-memory metric reader, `agentfs-test-metrics` (rank `tests`): the engines' tests both need it, an edge
   between the engines is refused even for tests, and no existing crate's job fits (agentfs-repo reads docs and cargo
   metadata). Prior art: the SDK's own `InMemoryMetricExporter`; the crate adds only point lookup.
7. Checks C adds: `workflow_path_filters_name_what_exists` (C moves files a filter names; a stale filter never fires);
   the dev-edge sibling rule (an engine → engine dev edge would make one engine's tests build the other); `--test` in
   the cargo-command check (C moves test programs that docs and workflows name). Not checked: a test-name filter such
   as `tree_actor::` after `--test it`.
8. The tree crash test joins the engine's `it` program (`tree_actor` module, beside the other kill tests); the test
   job's `--all-features` run covers it.
9. No stub interfaces in the skeleton: C1's functions move with their bodies. One product version in
   `[workspace.package]`, taken by every published member: it is written into history's verification notes, the LFS
   user agent, the telemetry scope and the CSI vendor version, and the build now keeps it one value.

**New terms.** None: "the tree engine" and "the recording engine" are crate-split.md's and AGENTS.md's.

**Design review outcome.** Round 1 found 1 P1, 6 P2, 4 nits; all applied but one narrowed: P1 meter `Option` → always a
`Meter` (choice 2); P2 `holds_tree_format` → claim narrowed with reasons (choice 4, option b); P2 read as `NotDurable` →
`VolumeError::Unreadable` (choice 4); P2 dev edge between engines → rank check (choice 7); P2 `--test` unchecked → check
(choice 7); P2 duplicated reader → `agentfs-test-metrics` (choice 6); P2 integration-point table → C2 docs; nits: one
workspace version (choice 9), `Meter::new` banned, `names` private and the module named `telemetry` in the engine,
skeleton docs say what the crates hold now (Crates and Layout rows move to C1/C2). Untested until the slices land: every
check and probe above but the clippy bans' resolution, checked in a scratch crate.
