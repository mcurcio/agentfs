# Crate split, PR C: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row C; section 1 the end state; section 2 what is
left). Branch `claude/bold-hopper-72ixu2`, from main 2b2f6f0. Slices run in order C1, C2, never in parallel: C2 changes
`Cargo.toml` and `Cargo.lock`, and C2's mechanical move needs C1's cuts. The skeleton commit holds: `agentfs-engine`
(manifest with rank `engine`, `lib.rs` doc comment only), the workspace member, `Cargo.lock`, the Dockerfile stubs, its
row in AGENTS.md's Layout and docs/architecture.md's Crates table, and its module-map section (`lib.rs` only). Owner's
words binding this: OWNER-GUIDANCE.md "Performance" (2026-11-24, 2026-11-25), and on PR C: "Go". Counts are lines
(`git grep -nE`, comment lines kept unless said) at the skeleton commit. The A/B pages' idioms hold: ranks, agentfs-repo's
checks, clippy bans with `#[expect]` at each sanctioned site, no re-exports from core.

## Page C1

**Title.** The tree engine's instruments on the binary's `Meter`; every read of a state area through the `StateArea`
port. Inside core, so C2 is a pure move. No new crate, no manifest change.
**New module** `crates/agentfs-core/src/tree_telemetry.rs` (`pub mod tree_telemetry;` in core's `lib.rs`), moved whole by
C2 with its path kept. Doc: the tree engine's OpenTelemetry instruments, registered on a `Meter` its caller makes (the
binary's composition root); docs/telemetry.md lists them. It names `crate::tree_actor` and `crate::operator_settings`
only, in its test module too (agentfs-repo does not read test modules; C2's compile does). Surface, bodies moved from
core's `Telemetry` (`telemetry.rs:644-822` today) with `inner.meter` → `meter`, behaviour unchanged:
```rust
pub mod names { pub const NOT_BUILT: &str = "agentfs.not_built"; pub const HISTORY_UNVERIFIED: &str = "agentfs.history.unverified";
    pub const JOURNAL_CHECKPOINTS: &str; LOCK_HOLD, LOCK_HOLDS, LOCK_HOLD_LONGEST, LOCK_WAIT, LOCK_WAITS, LOCK_WAIT_LONGEST,
    GATE_WAIT, GATE_WAITS, GATE_WAIT_LONGEST, REQUEST_TIME, REQUESTS, REQUEST_LONGEST /* 15 consts, values as today */ }
pub fn observe_not_built(meter: &opentelemetry::metrics::Meter, not_built: &[crate::operator_settings::NotBuilt]);
pub fn observe_latency(meter: &opentelemetry::metrics::Meter);
pub fn observe_unverified(meter: &opentelemetry::metrics::Meter,
    counts: std::sync::Weak<dyn Fn() -> crate::tree_actor::UnverifiedCounts + Send + Sync>);
```
Each: "Call once per process; telemetry that is off calls none" (no noop branch: the caller holds no meter then).
**Core's `telemetry.rs` loses** the three methods, the 15 names, the `operator_settings` import and the three tests
(`not_built_settings_are_counted_by_capability`, `the_latency_figures_are_observed_by_attribute`,
`unverified_saves_are_observed_by_verdict_zeros_included`), which move to `tree_telemetry/tests.rs` over a private
reader: an `SdkMeterProvider` with a `PeriodicReader` over `InMemoryMetricExporter`, and `point(name, attrs) ->
Option<Point>` (exact attributes). Core's `testing::Harness` stays; the reader's ~50 lines are a second copy until M9
deletes core (Decisions 6). After C1, `telemetry.rs` names no tree-engine module (section 2's `:672`, `:684`, `:801`,
`:1886`, `:1947`, and `:48`).
**The binary.** `otel.rs`: `Otel` gains `meter: Option<opentelemetry::metrics::Meter>` and `pub fn meter(&self) ->
Option<&Meter>` (`None` for `Otel::noop`; the no-op provider's meter when metrics alone are off, as `Telemetry::new` gets
today); `shutdown` drops it with the telemetry. `launcher/host.rs:117-118` and `launcher/mod.rs:416-417` become `if let
Some(meter) = otel.meter() { tree_telemetry::observe_not_built(meter, &boot.not_built); observe_latency(meter) }`;
`launcher/mod.rs:75` `fn observe_unverified(meter: Option<&Meter>, domains: &Domains) -> Result<UnverifiedGauge>`;
`Mount`'s `telemetry` field (`:553`, set `:453`, read `:626`) becomes `meter: Option<Meter>`.
**The port.** `tree_actor::inspect` (`tree_actor/mod.rs:465`) narrows to `pub(crate)`: `state_area` is its one caller,
so a reader outside the engine has only `StateArea::inspect`. Callers: csi `controller.rs:79` →
`TreeStateArea::at(&state).inspect().map_err(|e| Error::io(format!("reading {}", state.display()), e))`, csi
`supervisor.rs:1273`, the binary's `found` (`launcher/mod.rs:244`), agentfs `tests/csi_trees.rs:530`, `:572`. Cost: a
failed read answers `VolumeError::NotDurable(kind)`; its `io::Error` text goes to stderr with the path (`volume_error`),
not into csi's message. csi's `use agentfs_core::tree_actor` lines (`controller.rs:34`, `supervisor.rs:78`) go.
**New checks.** (1) clippy.toml, reason "a Meter is made only in the composition root: the binary's otel::init, and each
test that plays one": `opentelemetry::global::{meter, meter_with_scope, meter_provider, set_meter_provider}` and
`opentelemetry::metrics::MeterProvider::{meter, meter_with_scope}`, all `allow-invalid = true`; `#[expect]` at
`otel.rs:917-922` (the `meter_handle` statement), core `telemetry.rs:1470` (Harness), the new reader. [checked: clippy
flags `global::meter`, `global::meter_provider` and the trait's `meter_with_scope` called on a provider]. (2)
agentfs-repo `ci.rs`, `workflow_path_filters_name_what_exists`: every `- <entry>` under a `paths:` or `paths-ignore:` key
of `.github/workflows/*.yml`, unquoted, `!` stripped: no `*` → the path exists; else the text before the first `*` is
a prefix of some path under the root (walked, `target/` and `.git/` skipped). Fixture test over strings. (3)
`TREE_ENGINE` (crates.rs:1002) gains `"tree_telemetry"` (7 entries).
**CI and docs.** alerts.yml paths: `+ crates/agentfs-core/src/tree_telemetry.rs`. docs/telemetry.md:49 (the tree
engine's figures through `agentfs_core::tree_telemetry`, on the binary's Meter); library/core.md "Events and telemetry"
(a paragraph on `tree_telemetry`), its offers row (`TreeStateArea`'s `inspect`, not `inspect`) and the tree section's
`tree_actor::inspect(root)` sentence; architecture.md (`tree_telemetry.rs` row; `telemetry.rs` row loses "the
`agentfs.not_built` gauge among them"); csi's `Cargo.toml` comment on agentfs-core.
**Acceptance.** `git grep -nE "crate::(tree_actor|operator_settings)" crates/agentfs-core/src/telemetry.rs` empty; `git
grep -nE "tree_actor::inspect|observe_(not_built|latency|unverified)" -- crates ':!crates/agentfs-core/src/tree_actor'
':!crates/agentfs-core/src/state_area.rs' ':!crates/agentfs-core/src/tree_telemetry*' ':!crates/agentfs/src/launcher'`
empty. Gates, one call each: `cargo clippy -p agentfs-core -p agentfs -p agentfs-csi -p agentfs-repo --all-targets --
-D warnings`, again with `--features failpoints`; `cargo nextest run` the same four, both ways.
**Probes.** (1) `let _ = opentelemetry::global::meter("x");` in `tree_telemetry.rs` → clippy. (2) alerts.yml names
`crates/agentfs-core/src/nowhere.rs` → check (2). (3) `agentfs_core::tree_actor::inspect(&state)` in csi → E0603. (4)
`use crate::telemetry::Telemetry;` in `tree_telemetry.rs` production text → `the_tree_engine_names_no_recording_engine_module`.
**Files.** core `src/{lib,telemetry,tree_telemetry}.rs`, `src/tree_telemetry/tests.rs`, `src/tree_actor/mod.rs`; agentfs
`src/otel.rs`, `src/launcher/{mod,host}.rs`, `tests/csi_trees.rs`; csi `src/{controller,supervisor}.rs`, `Cargo.toml`;
`crates/agentfs-repo/tests/repo/{ci,crates}.rs`; `clippy.toml`; `.github/workflows/alerts.yml`; the docs named.

## Page C2

**Title.** `agentfs-engine` takes the tree engine whole. **Crate** `crates/agentfs-engine`, rank `engine` (the skeleton's).
Why a crate: the aggregate (one actor, one lock, one unit of work) apart from the recording engine, so an edit of one
builds and tests only its own (crate-split.md section 1). Deps: `agentfs-contracts`, `-failpoints`, `-git`, `-os`,
`-align` (`isolated`), `-lines`, `-rules`; `crc32fast`, `git2` (`default-features = false`), `imbl`, `libc`,
`opentelemetry` (as core's line), `rustix` (fs, process), `serde` (derive), `serde_json`, `sha2`, `thiserror`, `uuid`
(std, v4). Dev: `agentfs-volume`, `-trees`, `-clients`, `opentelemetry_sdk` (as core's line), `tempfile`. Feature
`failpoints = ["agentfs-failpoints/failpoints"]`. No https/ssh: the engine opens no remote (clippy's bans).
**Move map** (`git mv`, contents unchanged but the paths below): core `src/tree_actor/**`, `src/{tree_git, session,
store, state_area, operator_settings, tree_telemetry}.rs`, `src/tree_telemetry/tests.rs` → `crates/agentfs-engine/src/`
same names. Engine `lib.rs`: the seven `pub mod`s, its doc comment naming docs/library/engine.md. Core `lib.rs` drops
them; its doc says it is the recording engine. No item widens: every `crate::` path in the moved files names a moved
module (A's check proves it before the move), and C1 cut the one reverse edge. The 497 `pub(in crate::tree_actor)`
and one `pub(in crate::tree_actor::journal)` compile unchanged. Core's manifest drops `agentfs-align`, `agentfs-lines`,
`imbl`, `uuid` and dev `agentfs-volume`, `-trees`, `-clients` (none used after the move; the build says if more go).
**Consumers** (`agentfs_core::{tree_actor,tree_git,session,store,state_area,operator_settings,tree_telemetry}` →
`agentfs_engine::...`, nothing else changes): fuse 14 lines in 3 files (`tree_view.rs`, `tree_view/{save_worker,
untracked}.rs`); agentfs 31 in 13 (src 18: `launcher/{domains,host,mod,serving,settings,verify}.rs`, `main.rs`,
`reference_tests.rs`, plus C1's; tests 13: `cli/{cli_mount,cli_verify,deploy_examples}.rs`, `csi_trees.rs`,
`launcher_host.rs`); csi 3 in 2 (drops `agentfs-core`, gains `agentfs-engine`); conformance 9 in 4 (drops
`agentfs-core`, gains `agentfs-engine`); clippy.toml 10 lines (`TreeGit::{open,open_with_lease,init,init_with_lease}`,
`TreeActor::{fork_with,record_rows,drop_fork,close_writer}`, the reasons naming `store`, `session`). Manifests: fuse,
agentfs gain `agentfs-engine` and `"agentfs-engine/failpoints"` in `failpoints`. After C2 agentfs-core is depended on
by agentfs-fuse (the recording front: `Ops`, backing, overlay, `Telemetry`) and agentfs (the recording commands,
`telemetry::scope`); csi reaches it only through fuse (`mount_state`; crate-split.md section 1); conformance not at all.
**Test moves.** Engine `tests/it/main.rs` (from core's: `test_name!`, `qualified`, `#[path] mod trees`) with
`state_area`, `tree_actor`, `tree_clients`, `tree_dir_landing`, `tree_dirs`, `tree_git`, `tree_line_rules`,
`tree_overlay_limit`, `tree_pages`, `tree_pending_saves`, `tree_replace`, `tree_rules`, `tree_store`, `tree_untrack`,
`tree_verify`; core's `it/main.rs` keeps `gitmodules_git`, `lfs_attributes_git`, `patch_fetch` and loses the macro
(unused). `tests/{tree_batch_alloc,tree_close,tree_overlay_alloc,tree_stages}.rs`, `tests/support/trees.rs` → engine
`tests/`. `crash_points.rs`'s tree half (`a_kill_after_a_write_returned_keeps_it`, `crash_points.rs:748-794`) →
`tests/it/tree_actor.rs` under `#[cfg(feature = "failpoints")]`, its child named by `test_name!`; core's
`crash_points.rs` drops `mod trees`. Unit tests move with their files.
**agentfs-repo.** Delete `TREE_ENGINE`, `tree_engine_files`, `crate_paths`, `recording_engine_uses`,
`the_tree_engine_names_no_recording_engine_module`, `a_tree_engine_use_of_the_recording_engine_is_found` (the boundary
replaces them: `dependencies_follow_the_ranks` refuses engine → engine, and rustc refuses an import of a crate not
depended on); `the_tree_engine_starts_git_only_through_counted` reads every file under agentfs-engine's `src` but
`tree_actor/history/repack.rs`, and agentfs-lines'; main.rs's doc list.
**CI, model, fixtures.** phase-1.yml:265 `-p agentfs-engine --test tree_stages`; alerts.yml: `crates/agentfs-engine/
src/{tree_telemetry,operator_settings}.rs` for core's two; ci/crash-tests.sh comment (core's matrix is the recording
engine's points; the tree engine's kill tests are its `it` program's, run by the test job); agentfs-model
`tests/ledger.rs:232` reads `crates/agentfs-engine/tests`; conformance `regressions/finding-51-sync-fast-forward.patch`
paths. phase-1.yml path filters stay `crates/**` (Decisions 5). Dockerfile: done in the skeleton.
**Docs.** New docs/library/engine.md ("The library: `agentfs-engine`"): core.md's offers rows for tree views, rows and
line identity; "Git-backed tree views" through "Domain invariants" (core.md:652-1138); the `tree_telemetry` paragraph;
its `tree_actor::Error` row; what it does not do. core.md keeps the recording engine and links engine.md. SUMMARY.md
and docs/README.md list it. Anchors re-pointed (`core.md#` → `engine.md#` in runbooks/recording-failures.md,
target-architecture/{system, domains/{clients,rules,volume}, future/one-engine}.md, telemetry.md, trees.md: 13 links).
`agentfs_core::` → `agentfs_engine::` in trees.md:644, :660, library/{filesystem.md:634, :786, kubernetes.md:145},
target-architecture/domains/views.md:107, performance.md:793. architecture.md: module rows of the moved files to the
`agentfs-engine` section; Crates rows of core ("the recording engine"), conformance (depends on the engine). AGENTS.md:
Layout's core row, principle 10's mapping sentence names both engines (E rewrites it for the front), local gates'
`--test it tree_actor::` example. development.md:98, :114, "Test programs", and "Build settings": measured
`cargo nextest run -p agentfs-engine` and `-p agentfs-core`, warm after a one-file edit in each. crate-split.md: row C
built, section 2's two C items out. CHANGELOG.md and OWNER-GUIDANCE.md keep their dated paths.
**Acceptance.** `crates/agentfs-core/src/{tree_actor,tree_git.rs,...}` gone; `git grep -nE "agentfs_core::(tree_actor|
tree_git|session|store|state_area|operator_settings|tree_telemetry)\b" -- ':!.claude' ':!CHANGELOG.md'` empty; `git grep
-c "pub(in crate::tree_actor)" crates/agentfs-engine` sums to 497; `cargo tree -p agentfs-engine -e normal -i
agentfs-core` and the same for `agentfs-conformance` find nothing; `git apply --check` of both regression patches exits
0. Gates: `cargo clippy -p agentfs-engine -p agentfs-core -p agentfs-fuse -p agentfs -p agentfs-csi -p
agentfs-conformance -p agentfs-model -p agentfs-repo --all-targets -- -D warnings`, again `--features failpoints`;
`cargo nextest run` with the same, both ways; `-p agentfs-model --test ledger` is among them.
**Probes.** (1) `agentfs-core` in engine's `[dependencies]` → `dependencies_follow_the_ranks` names `agentfs-engine ->
agentfs-core (engine does not use engine)`. (2) `cmd.output()` in engine `tree_actor/stages.rs` → the spawn check. (3)
`TreeGit::open` in agentfs-fuse → clippy (a stale `agentfs_core::` path would match nothing). (4) alerts.yml keeps
`crates/agentfs-core/src/operator_settings.rs` → C1's path-filter check. (5) ledger.rs keeps `agentfs-core/tests` →
`named_tests_exist`. (6) drop `"agentfs-engine/failpoints"` from fuse → A's check 2.
**Files.** `crates/agentfs-engine/**`; core `src/lib.rs`, the moved files, `tests/**`, `Cargo.toml`; fuse, agentfs,
csi, conformance sources and manifests; `crates/agentfs-repo/tests/repo/{crates,main}.rs`;
`crates/agentfs-model/tests/ledger.rs`; the regression patch; `clippy.toml`; `Cargo.lock`; phase-1.yml, alerts.yml,
ci/crash-tests.sh; the docs named.

## Decisions

**Slice order.** C1 then C2. C1 is everything that is not a move (the observers out of core's `Telemetry`, the port,
the checks), inside crates that exist; C2 is then a move and a rename that the compiler and the checks judge. C2
cannot come first: core's `telemetry.rs` names `crate::tree_actor`, so core stops compiling the moment the engine
leaves. Two slices, not three: the docs travel with the code (AGENTS.md "Git"), and the module map's test forces
architecture.md into C2 anyway.

**Design choices.**
1. The engine's instruments live in a module of their own, `tree_telemetry`, written in core by C1 and moved with its
   path by C2, like `tree_git`; in the binary, which imports both, the prefix tells it from `agentfs_core::telemetry`.
   Three free functions over `&Meter` mirror today's three methods; no struct, nothing held.
2. The binary holds the `Meter` (`Otel::meter`), not core's `Telemetry`: M9 deletes core, and the engine's figures
   should not route through the recording engine's handle.
3. Deviation (stronger than the page): clippy bans the trait's `MeterProvider::{meter, meter_with_scope}` as well as
   `global::meter*`, with `#[expect]` at the composition root and the tests that play one, the idiom of
   `Minter::new`. The global functions alone would leave `NoopMeterProvider::new().meter(..)` open. Gap: a library could
   build an SDK provider, but no library depends on the SDK outside dev-dependencies (not checked).
4. Deviation (wider than the page): `tree_actor::inspect` becomes `pub(crate)`, so the binary and its tests, not only
   csi, read through the port; a habit becomes a compile error.
5. Deviation: phase-1.yml's path filters stay `crates/**`. Every phase-1 job builds `agentfs`, which links core (the
   recording front, `Telemetry`, `scope`), so a core edit can change a tree mount; filtering by crate waits for M9.
6. Cost stated: the in-memory metric reader exists twice in tests (core's `Harness`, the engine's), about 50 lines,
   until M9 deletes core. A shared crate for 50 test lines was weighed and rejected (right-size); no existing crate's
   responsibility fits it.
7. New check `workflow_path_filters_name_what_exists` (agentfs-repo): C moves two files a filter names, and a stale
   filter fails silently by never firing. Prefix matching, not full globs: it catches moved files and directories.
8. The tree crash test joins the engine's `it` program (`tree_actor` module, beside the other kill tests) rather than
   a one-test program; the test job's `--all-features` run covers it, as it covers core's `crash_points`.
9. The skeleton commits no stub interfaces: C1's functions move with their bodies, and a panicking stub wired into the
   binary would be worse than none. Engine version stays 0.1.0, core's: verify.rs's notes embed `CARGO_PKG_VERSION`.

**New terms.** None: "the tree engine" and "the recording engine" are crate-split.md's and AGENTS.md's.

**Design review outcome** (self-check; the owner's rules: no pre-code review of the cut). Section 2's C items re-read
at the skeleton: `telemetry.rs:672`, `:684`, `:801`, tests `:1886`, `:1947` accurate, plus `:48` (`operator_settings`),
which the page missed; csi `controller.rs:79` accurate, `supervisor.rs:1273`. The recording engine's only edges into
the tree engine are those six lines (grep of every non-tree core file). Goals and their steps: the engine crate with
the six modules and their tests → C2; instruments on the binary's Meter, no library Meter → C1; csi through the port
→ C1; core.md's tree half → C2; module paths kept, 497 held → C2's acceptance; siblings → C2 (ranks); csi and
conformance off core → C2 (csi still through fuse); CI follows → C1 (alerts.yml), C2; A's check replaced → C2; the
gain (a recording edit runs only core's tests) → C2's measurement. Untested until the slices land: every check and
probe above, but the clippy bans' resolution, checked in a scratch crate.
