# Crate split, PR F: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row F, section 1, section 2's F item, section 4's
items routed to F). Branch `claude/bold-hopper-72ixu2`, PR #47, base of this run 806e0a5. Two slices, F1 then F2, in
order (F2 edits files F1 leaves). Owner's words (OWNER-GUIDANCE.md "Performance", general guidance): the library hands
consumers a working volume, its domains wired onto one store, from a crate the binary also uses: "Q1 A" (2026-11-24);
carrying D to G on PR #47: "Yes" (2026-11-25). Counts are `git grep` at the skeleton. A to E idioms hold: ranks,
agentfs-repo's checks, no shims, docs in the same commit, clippy bans in `clippy.toml`.
**The skeleton** (the commit carrying this file): `crates/agentfs-service/{Cargo.toml,src/lib.rs}` (rank `service`, no
dependencies, module doc only); the member (after `agentfs-core`); the rank `{ name = "service", uses = ["shared-kernel",
"domain", "engine"] }` listed after `engine`, and `service` added to the uses of `filesystem`, `umbrella` and `tests`
(manifest, agentfs-repo's `ranks()` fixture, architecture.md's Ranks table); the front's `pub use agentfs_service as
service;` and its dependency (agentfs-repo's front check requires both once `umbrella` uses `service`); `Cargo.lock`; the
Dockerfile's stubs; architecture.md's Crates row and module-map section `agentfs-service` (`lib.rs` only); crate-split.md
row F's words on `identity::Context` (Decision 6).

## Page F1

**Title.** `agentfs-service` composes one volume; `agentfs init`, `mount`, `host` and `fork end` use it; the front's
doctest serves a volume through it.
**The crate** (`crates/agentfs-service`, rank `service`, the library). Deps: agentfs-{clients,contracts,engine,trees,
volume} (path, `0.1.0`), `thiserror = "2.0.21"`; dev `tempfile = "3.27.0"`. A crate because the library is an
integration point (principle 10) and only the binary wired a volume; both now share one composition. Modules private,
items at the root, one path each: `pub use self::{domains::Domains, error::ServiceError, launch::{Launcher, Recovered,
ensure}};`
```rust
#[derive(Debug, Clone)]
pub struct Launcher { pub id: LauncherId, pub may: LaunchScope, pub at: PodContext, pub upkeep: Upkeep }
impl Launcher { pub fn root(id: LauncherId, at: PodContext) -> Self; }   // LaunchScope::Root, Upkeep::default()
pub struct Recovered { area: TreeStateArea }                              // the state area's lock, until dropped
/// `ensure` as `launcher`, then the recovery every start makes.
pub fn ensure(state: &Path, spec: VolumeSpec, launcher: &Launcher) -> Result<(Ensured, Recovered), ServiceError>;
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)] Volume(#[from] VolumeError),
    #[error("recovering the volume: {0}")] Recovering(VolumeError),
    #[error("reading the operator settings: {0}")] Settings(#[from] SettingsError),
    #[error("the staged volume is not served")] NotServed,
    #[error("reading the forks' rows: {0}")] Forks(StoreError),
    #[error("reading the clients' rows: {0}")] Clients(StoreError),
}
impl Domains {   // !Send: one thread, as today
    /// Operator settings read first; Volume staged (recovers, never creates); overlay limit set; Trees, Clients restored.
    pub fn stage(state: &Path, launcher: &Launcher) -> Result<Self, ServiceError>;
    /// `StateArea::open`, then Trees and Clients restored, no Volume: judges no release (`fork end`).
    pub fn open(state: &Path, launcher: &Launcher) -> Result<Self, ServiceError>;
    pub fn sessions(&self) -> &Sessions;
    pub fn bind(&mut self, tree: TreeId, label: ClientLabel, access: Access, at: PodContext) -> Result<Binding, ClientError>;
    pub fn detach_all(&mut self, ids: &[ClientId]);
    pub fn reconcile(&mut self, live: &ClientSet) -> u32;
    pub fn settle(&mut self);
    pub fn fork(&mut self, upstream: TreeId, key: Option<ForkKey>, settings: ForkSettings) -> Result<TreeId, TreeError>;
    pub fn end_keyed(&mut self, key: &ForkKey) -> Result<(), TreeError>;
    pub fn keyed(&self, key: &ForkKey) -> Option<(TreeId, ForkPhase)>;
    pub fn unstage(&mut self);
    pub fn drain_begun(&mut self) -> bool;         // `VolumeDraining` consumed since the last ask; true once
    pub fn drain(&mut self, ids: &[ClientId]);     // ids detached, the rest of this launcher reconciled, others logged
    pub fn released(&self) -> bool;
    pub fn stopped(&self) -> bool;
}
```
`TreeId` is `agentfs_contracts::TreeId`. Private: `restore`, `dispatch`, `rejudge`, `settle_trees`, `clients`,
`log_foreign_clients`, the aliases, `NoClients`, and `fn mint(&Launcher) -> (Minter, LaunchCap)`, the crate's one
`#[expect(clippy::disallowed_methods)]`. Bodies, log lines and dispatch order move unchanged.
**Move map.** cli `launcher/domains.rs` → `src/domains.rs`; `src/shared.rs` → `src/shared.rs` and `src/domain_events.rs`
→ `src/events.rs` (both `pub(crate)`, unit tests with them); `NoClients` and init's ensure-and-recover → `src/launch.rs`;
serving.rs's restore half of `stage` and `run`'s drain branch → `stage`, `drain_begun`, `drain`. The binary keeps flags,
`upkeep()`, mount points, `at_boot` (still calls `overlay_limit()` for the early refusal; `BootSettings` drops the
field), signals (installed before `Domains::stage`, so none is lost while it boots), the view host, `Driver`, exit codes.
**Binary.** main.rs loses `Minter::new` and `mod shared`, `mod domain_events`; `launcher::{init,mount,host,end_fork}`
lose `minter`; `root_cap` becomes `Launcher { upkeep: upkeep(), ..Launcher::root(LauncherId::parse("cli")?, at) }`.
Sites: launcher/mod.rs (about 12), serving.rs (8), host.rs (2). agentfs-cli depends on agentfs-service.
**Front.** Doctest: `ensure`, `drop(recovered)`, `Domains::stage`, `bind(TreeId::Root, .., Access::ReadWrite, ..)`, the
session from `sessions().session(binding.session)`, `write_open`/`write` on its guard, `close`, `read` back,
`detach_all`, `unstage`, `assert!(volume.released())`; only `agentfs::` names, `tempfile` hidden.
**Tests.** `crates/agentfs-service/tests/volume.rs`, no FUSE: `a_bound_client_writes_through_its_session`,
`a_stop_drains_and_releases`, `a_keyed_fork_is_found_again_then_ends`, `a_second_stage_is_busy`
(`ServiceError::Volume(VolumeError::Busy)`), `open_refuses_an_unknown_key` (`TreeError::KeyUnknown`).
**Docs.** New `docs/library/service.md` (Launcher, ensure, stage and open, the calls, dispatch order, the drain, errors,
one thread, capabilities minted inside) in SUMMARY.md and docs/README.md. agentfs.md: module row; the consumer API names
the service route alone; `Minter::new` and the `StateArea` port's `create` move to "Outside". domains.md: the root's
queue → service.md; C1's enforcement "outside agentfs-service". filesystem.md's dispatcher line. architecture.md: Crates
row link, the service's map (lib, domains, launch, error, shared, events), cli's map loses three rows. AGENTS.md
principle 10 and Layout; clippy.toml's `Minter::new` reason; caps.rs doc; CHANGELOG.
**Acceptance.** `git grep -n 'Minter::new' -- 'crates/*/src' ':!crates/agentfs-contracts'` hits only agentfs-service
and test modules; the Traps prose grep, every hit read. Gates: fmt; clippy `--all-targets -D warnings` and nextest of
agentfs-service and agentfs-cli (also `--features failpoints`); nextest -p agentfs-repo; `cargo test --doc -p agentfs`;
`bash ci/check-deps.sh`.
**Probes.** (1) agentfs-fuse in the service's deps → `dependencies_follow_the_ranks`. (2) the front's `service` line
deleted → `the_front_only_reexports_the_ranks_it_uses`. (3) `dispatch()` dropped from `unstage` →
`a_stop_drains_and_releases`.
**Files.** `crates/agentfs-service/**`; agentfs-cli `Cargo.toml`, `src/{main.rs,shared.rs,domain_events.rs}`,
`src/launcher/{mod.rs,domains.rs,serving.rs,host.rs,settings.rs}`; `crates/agentfs/src/lib.rs`; `Cargo.lock`;
clippy.toml; `crates/agentfs-contracts/src/caps.rs`; the docs named.

## Page F2

**Title.** Items `pub` only for the workspace's tests are out of a consumer's reach, the rest of agentfs.md's list is
documented outside the supported route (the service), and the tree launch reads nothing of `agentfs-core`.
**Mechanism.** agentfs-engine: `[features] test-util = ["dep:visibility"]`, `visibility = { version = "0.1.1", optional
= true }` (prior art: tokio's `test-util`, agentfs-os's; the `visibility` crate is the idiom for a visibility a feature
widens; verified on an `impl` method), and `[dev-dependencies] agentfs-engine = { path = ".", features = ["test-util"] }`
so its own `tests/` see them. Rule per item: with a production caller in the engine,
`#[cfg_attr(feature = "test-util", visibility::make(pub))] pub(crate)`; with none, `#[cfg(any(test, feature =
"test-util"))] pub`.
| Item | Becomes |
|---|---|
| `TreeActor::record_rows`, `recording::…::close_writer` | `pub(crate)` (engine only) |
| `TreeActor::{fork_with, drop_fork, create, create_with}`, `Recording::{create, create_with}` | test-util |
| `TreeGit::{open, open_with_lease, init, init_with_lease}` | test-util |
| `COMPACT_GATE_VAR`, `CompactGate`, `Gap` (re-export), `ChannelGate`, `Upkeep::gated` | test-util; `Upkeep.gate` `pub(crate)` |
| `Minter::new`; the `StateArea` port's `create`; the guard's named-writer calls (19) | documented outside (below) |
`Recording::{create, create_with}` are added: the same hand-made-id route as `TreeActor::create`. The clippy bans stay:
they guard the engine's own modules.
**Consumers** (sites): engine `tests/` (about 55, by the self dev-dependency, no edit); agentfs-fuse dev-dep test-util
(tree_view.rs tests, 1); agentfs-cli dev-dep test-util (cli_verify.rs, reference_tests.rs) and `failpoints` adds
`"agentfs-engine/test-util"` (`upkeep()`); agentfs-conformance's engine dependency `features = ["test-util"]` (14).
**Not narrowed, and why.** `Minter::new`: agentfs-service mints in production, and a feature it enabled would reach every
consumer (features unify); Rust has nothing narrower than `pub` across crates. The `StateArea` port's `create`: a trait
method the domain calls. The guard's 19 named-writer calls: the engine's own call surface under sessions, used by the
replay and its tests. Each is in agentfs.md's "Outside the supported route", with the service's call to use instead.
**The launch off core** (section 2's F item). `otel::init(volume: &str, replica: &str, pod: &PodArgs) -> Otel`, and
`resource` alike; `fn scope() -> InstrumentationScope` and `const TELEMETRY_EXPORT_FAILURES: &str` move into otel.rs;
core loses `telemetry::scope` and `names::TELEMETRY_EXPORT_FAILURES` (its test harness takes a local scope, as the
engine's tests do). `PodArgs::replica(&self) -> String` (the pod name, else `crate::hostname()`) is that rule's one home;
main.rs's `Pod::replica` answers `--replica`, else it. launcher's `identity()` goes. One observable change, to
telemetry.md's words ("`k8s.*` from the pod flags"): a recording command's `k8s.pod.uid` is absent without `--pod-uid`
(it was the replica): CHANGELOG Fixed. Acceptance: `git grep -n agentfs_core -- crates/agentfs-cli/src/launcher
crates/agentfs-service` empty; otel.rs reads core's `Telemetry`, `OtelAlarmLog`, `AlarmLog` only. No check: M9's
deletion of core is the compile-time one.
**Check.** `ci/check-deps.sh`: `ok   library features`, failing when `cargo tree --locked -p agentfs -e
normal,features --prefix none --format '{p}'` has a line `^agentfs-[a-z-]+ feature "(test-util|failpoints)"`.
**Docs.** agentfs.md's outside list rewritten (narrowed items gone; three documented); engine.md: the `test-util` feature;
development.md: test-util beside failpoints; crate-split.md row F "(built)", section 2's F item and section 4's F items
resolved (the `Error::Reconcile` leaf type stays G's); CHANGELOG.
**Acceptance.** `cargo build -p agentfs-engine` and `cargo clippy -p agentfs-engine -- -D warnings` (no feature, lib only)
warning-free; the library line in check-deps passes. Gates: fmt; clippy `--all-targets -D warnings` and nextest of
agentfs-engine (and `--features failpoints`), -fuse, -cli (and `--features failpoints`), -core, -conformance, -service; `cargo test --doc -p agentfs`;
nextest -p agentfs-repo; `cargo build -p agentfs-conformance --bin phase1-replay`.
**Probes.** (1) `features = ["test-util"]` on agentfs-service's engine dependency → check-deps "library features".
(2) call `TreeActor::create` from agentfs-service's `src` → `cargo build -p agentfs-service` fails (E0624 or E0599).
**Files.** agentfs-engine `Cargo.toml`, `tree_actor/{mod.rs,call_path.rs,recording/mod.rs,journal/compaction.rs}`,
`tree_git.rs`; Cargo.toml of -fuse, -cli, -conformance; agentfs-cli `src/{otel.rs,main.rs,pod_args.rs}`,
`src/launcher/{mod.rs,host.rs}`; agentfs-core `src/telemetry.rs`; `Cargo.lock`; `ci/check-deps.sh`; the docs named.

## Decisions

**Slice order.** F1 first: the service, the binary on it and the doctest form one green unit (the route must exist before
the surface narrows to it). F2 then narrows around that route and takes the launch off core; it edits launcher/mod.rs
and host.rs after F1, so the two run in order.

**Design choices.**
1. Rank `service`, listed after `engine`, used by `filesystem`, `umbrella` and `tests`; `kubernetes` does not (csi
   spawns the binary). Rows E and F of crate-split.md.
2. The composition's names are kept: `Domains`, its calls, its log lines. New: `Launcher` (who opens a volume and how;
   mirrors `LaunchCap`'s two fields plus the pod context and upkeep), `Recovered`, `ServiceError`, `drain_begun`, `drain`.
3. The service mints. With the service as the one composition of a volume, no consumer needs a capability, so `Minter::new`
   leaves the consumer API (E had it in until F).
4. `stage` reads the operator settings itself, so no consumer serves a volume without the operator's
   `agentfs.overlayLimit`; the binary's boot check keeps the early refusal through the same `overlay_limit()`.
5. `open` goes through `TreeStateArea::open`, one open path for `fork end` too (its busy is `VolumeError::Busy`).
6. **Deviation from crate-split.md row F:** `identity::Context` does not move to contracts. Contracts already holds the
   pod context (`PodContext`); a second pod type there would be two homes. The tree launch's only use was the binary's
   telemetry resource, whose home is the binary (`otel::init` takes the pod flags and the replica). Core keeps `Context`
   for its commit templates until M9.
7. Narrowing by a `test-util` feature plus `pub(crate)`, not a sealed trait (a cross-crate seal does not exist) and not
   docs alone; check-deps guards that no library build enables it. Cost: one optional proc-macro dependency
   (`visibility`, about 80 lines, on syn 2, already in the graph), a self dev-dependency in the engine, and in a
   workspace-wide test build the feature is on everywhere (features unify), so inside the workspace the clippy bans,
   not visibility, keep production code off these items; a plain `cargo build -p <crate>` refuses them.
8. Page F1 runs to about 80 lines: its signatures are the page.

**Review round.** Not yet run.

**New terms.** `service`: the rank of `agentfs-service`. `Launcher`: who opens a volume (id, scope, pod context, upkeep).
`test-util`: the engine feature that makes the test-only surface `pub`.
