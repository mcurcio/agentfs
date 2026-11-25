# Crate split, PR F: frozen pages

Design: `docs/target-architecture/future/crate-split.md` (section 3 row F, section 1, section 2's F item, section 4's
items routed to F). Branch `claude/bold-hopper-72ixu2`, PR #47, base of this run 806e0a5. Two slices, F1 then F2, in
order (F2 edits files F1 leaves). Owner's words (OWNER-GUIDANCE.md "Performance", general guidance): the library hands
consumers a working volume, its domains wired onto one store, from a crate the binary also uses: "Q1 A" (2026-11-24);
carrying D to G on PR #47: "Yes" (2026-11-25). Counts are `git grep` at the skeleton. A to E idioms hold: ranks,
agentfs-repo's checks, no shims, docs in the same commit, clippy bans in `clippy.toml`. Revised once after the design
review (Decisions, "Review round").
**The skeleton** (54d50ce, and the revision commit carrying this file): `crates/agentfs-service/{Cargo.toml,src/lib.rs}`
(rank `service`, no dependencies, module doc only); the member; the rank `{ name = "service", uses = ["shared-kernel",
"domain", "engine"] }` after `engine`, used by `filesystem`, `umbrella`, `tests` (manifest, agentfs-repo's `ranks()`
fixture, architecture.md's Ranks table); the front's `pub use agentfs_service as service;` and its dependency;
`Cargo.lock`; the Dockerfile's stubs; architecture.md's Crates row and module-map section; crate-split.md row F
(Decision 6), section 2 (the reads M9 cuts) and section 4 (F's decision on the front, Decision 7).

## Page F1

**Title.** `agentfs-service` composes one volume from the capabilities its caller minted; `agentfs init`, `mount`,
`host` and `fork end` use it; only it composes the domains (checked); the front's doctest serves a volume through it.
**The crate** (`crates/agentfs-service`, rank `service`). Deps: agentfs-{clients,contracts,engine,trees,volume} (path,
`0.1.0`), `thiserror = "2.0.21"`; dev `tempfile = "3.27.0"`. A crate: the library is an integration point (principle
10) and only the binary wired a volume. Modules private; `pub use self::{domains::{Domains, Unstaged},
error::ServiceError, launch::{Launcher, end_fork, init}};`. It mints nothing: the caller's `main` is the composition
root (system.md, "Authority: who mints what") and hands in its `Minter` and `LaunchCap`.
```rust
#[derive(Debug)] pub struct Launcher { minter: Minter, cap: LaunchCap, at: PodContext, upkeep: Upkeep } // private
impl Launcher {
    /// `minter` is lent to Clients alone (each read-write client's `ClientCap`); every call is made as `cap`, at `at`.
    pub fn new(minter: Minter, cap: LaunchCap, at: PodContext) -> Self;      // Upkeep::default()
    pub fn with_upkeep(self, upkeep: Upkeep) -> Self;
}
/// Volume's `ensure` as `launcher`, the recovery every start makes, then (when not empty) `includes` written as the
/// operator settings' includes under that recovery's lock, released before it returns.
pub fn init(state: &Path, spec: VolumeSpec, launcher: &Launcher, includes: &[PathBuf]) -> Result<Ensured, ServiceError>;
/// Trees and Clients restored with no Volume (judges no release); the fork under `key` ends, dropped now if unheld.
pub fn end_fork(state: &Path, launcher: Launcher, key: &ForkKey) -> Result<(), ServiceError>;
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {   // a busy state area is always Volume(VolumeError::Busy), from every call
    #[error(transparent)] Volume(#[from] VolumeError),
    #[error(transparent)] Settings(#[from] SettingsError),      // init's includes; stage's overlay limit
    #[error("reading the forks' rows: {0}")] Forks(StoreError),
    #[error("reading the clients' rows: {0}")] Clients(StoreError),
    #[error(transparent)] Fork(TreeError),                     // end_fork: KeyUnknown, ...
}
#[must_use = "a begun drain closes every view the caller serves"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Unstaged { Draining, Unchanged }
impl Domains {   // !Send: Rc-shared services and queue, one thread, as today
    /// Volume staged as the launcher (recovers, never creates); `settings`' overlay limit set; Trees, Clients restored.
    /// Holds the state area's lock until dropped.
    pub fn stage(state: &Path, launcher: Launcher, settings: &OperatorSettings) -> Result<Self, ServiceError>;
    pub fn sessions(&self) -> &Sessions;
    pub fn bind(&mut self, tree: TreeId, label: ClientLabel, access: Access, at: PodContext) -> Result<Binding, ClientError>;
    pub fn detach_all(&mut self, ids: &[ClientId]);
    pub fn reconcile(&mut self, live: &ClientSet) -> u32;
    pub fn settle(&mut self);
    pub fn fork(&mut self, upstream: TreeId, key: Option<ForkKey>, settings: ForkSettings) -> Result<TreeId, TreeError>;
    pub fn end_keyed(&mut self, key: &ForkKey) -> Result<(), TreeError>;
    pub fn keyed(&self, key: &ForkKey) -> Option<(TreeId, ForkPhase)>;
    /// Volume unstages; if that begins the drain: `views` detached (opens saved), this launcher's other clients
    /// ended, other launchers' attached clients logged (`tree DrainBlocked`), and `Draining` answered.
    pub fn unstage(&mut self, views: &[ClientId]) -> Unstaged;
    pub fn released(&self) -> bool;
    pub fn stopped(&self) -> bool;
}
```
**Contract.** `stage` and `end_fork` hold the lock while they live: a second `stage`, `init` or `end_fork` on the state
area, in any process, is `Volume(Busy)`. `init` holds none once it returns. Invariant, not an error: a staged volume
is served (`recording()` is `Some` after `open`), an `expect` naming it. Failures the caller cannot act on (dispatch,
unstage, reconcile, settle, detach) are reported as the tree library reports them: one `tree <Name>` stderr line
(`EventFailed`, `UnstageFailed`, `ReconcileFailed`, `SettleFailed`, `DetachFailed`), and the next boot re-judges.
Costs: one thread; `init` then `stage` recovers twice (the second replays nothing new); `stage` reads no file.
`OperatorSettings` loses `Default` (a private empty value inside `read`), so only `read` makes one.
**Move map.** cli `launcher/domains.rs` → `src/domains.rs` (`restore`, `dispatch`, `rejudge`, `settle_trees`,
`clients`, `log_foreign_clients`, aliases private); `src/shared.rs` → `src/shared.rs`; `src/domain_events.rs` →
`src/events.rs` (both `pub(crate)`, unit tests with them); `NoClients` and init's ensure, recovery, includes →
`src/launch.rs`; serving.rs's restore half of `stage` → `Domains::stage`; `run`'s drain branch → `unstage`.
Behaviour changes: the drain runs inside `unstage`, not at the loop's next turn; signals install right after
`Domains::stage` (today between stage and restore), so the boot window in which SIGTERM kills the process also spans
the restore of the rows (csi reads that death as `Exit::Signal(15)`, as for the window today); `init`'s recovery
failure prints `error: <e>` (not `recovering the volume: <e>`), exit 1 as before; no test reads the old words.
[Erratum F: also `init`'s recovery that finds another format prints `other_format`'s message, and `fork end`'s open
failure prints Volume's words (`opening tree state: <VolumeError>`), exit statuses unchanged.]
**Binary.** main.rs keeps `Minter::new` and mints: `LaunchCap::mint(&minter, LauncherId::parse("cli")?` (or host's
`--launcher`)`, LaunchScope::Root)`, then `Launcher::new(minter, cap, at).with_upkeep(upkeep())`. [Erratum F: main.rs
makes only the `Minter`; the launcher's `root_launcher` and `host` mint the `LaunchCap` and build the `Launcher`, as
host checks its control channel (exit 2) before it parses `--launcher`.] It loses `mod
shared`, `mod domain_events`. `settings::at_boot` returns the `OperatorSettings` it read (logs, the include check and
`overlay_limit()`'s early refusal on that one value; `BootSettings.overlay_limit` goes); stage gets that value.
`Serving::stop` passes its views' clients to `unstage` and closes every view on `Draining` or a stopped actor; every
`domains.unstage()` call goes through it. Sites: launcher/mod.rs (about 12), serving.rs (8), host.rs (3).
agentfs-cli drops its normal dependencies on agentfs-{volume,clients,trees} and gains agentfs-service.
**Check** (agentfs-repo, `crates.rs`): `only_the_service_composes_the_domains`: each `domain`-rank crate the
`service`-rank crate depends on (derived, so M4's domains join it) is a normal [erratum F: or build] dependency only of `service`,
`umbrella` and `tests` crates; with a fixture test. agentfs-conformance stays its own root (tests rank): it places a
keyed fork in a recording it already holds open, which `stage` would refuse as busy.
**Front.** Doctest: `Minter::new`, `LaunchCap::mint`, `Launcher::new`, `init`, `OperatorSettings::read`,
`Domains::stage`, `bind(TreeId::Root, .., Access::ReadWrite, ..)`, the session's guard `write_open`/`write`, `close`,
`read` back, `unstage(&[binding.client])` is `Draining`, `released()`; `agentfs::` names only, `tempfile` hidden.
**Tests** (`tests/volume.rs`, no FUSE): `a_bound_client_writes_through_its_session`, `unstage_drains_and_releases`,
`a_second_unstage_is_unchanged`, `a_keyed_fork_is_found_again_then_ends`, `a_second_stage_is_busy`,
`end_fork_while_staged_is_busy`, `init_writes_the_includes`, `end_fork_refuses_an_unknown_key`
(`ServiceError::Fork(TreeError::KeyUnknown)`).
**Docs.** New `docs/library/service.md` (Launcher, init, stage, end_fork, the calls, the contract and costs, dispatch
order, the drain, the `tree` lines) in SUMMARY.md and docs/README.md. agentfs.md: module row; the consumer API is the
service route, its caller minting (`Minter::new` stays consumer API); the `StateArea` port's `create` names `init`.
domains.md: the root's queue → service.md. filesystem.md's dispatcher line; engine.md's `OperatorSettings`.
architecture.md: the service's map (lib, domains, launch, error, shared, events), cli's loses three rows.
development.md's list of agentfs-repo checks. AGENTS.md principle 10 and Layout; CHANGELOG.
**Acceptance.** `git grep -n 'Minter::new()' -- 'crates/*/src'` hits only agentfs-cli's `main.rs`, agentfs-conformance's
`mounted.rs` (tests rank) and `#[cfg(test)]` modules; none in agentfs-service. The Traps prose grep, every hit read.
Gates: fmt; clippy `--all-targets -D warnings` and nextest of -service, -cli (also `--features failpoints`), -engine,
-repo; `cargo test --doc -p agentfs`; `bash ci/check-deps.sh`.
**Probes.** (1) agentfs-volume back in agentfs-cli's dependencies → `only_the_service_composes_the_domains`. (2) the
front's `service` line deleted → `the_front_only_reexports_the_ranks_it_uses`. (3) `dispatch()` dropped from
`unstage` → `unstage_drains_and_releases`. (4) a test's `unstage` answer ignored → clippy `-D warnings`
(`unused_must_use`).
**Files.** `crates/agentfs-service/**`; agentfs-cli `Cargo.toml`, `src/{main.rs,shared.rs,domain_events.rs}`,
`src/launcher/{mod.rs,domains.rs,serving.rs,host.rs,settings.rs}`; `crates/agentfs-engine/src/operator_settings.rs`;
`crates/agentfs-repo/tests/repo/crates.rs`; `crates/agentfs/src/lib.rs`; `Cargo.lock`; the docs named.

## Page F2

**Title.** Items `pub` only for the workspace's tests are off by default and never enabled by the library; the front's
doctest and the library's lib build run in CI without them; the tree launch's own modules name nothing of core.
**Mechanism.** agentfs-engine: `[features] test-util = ["dep:visibility"]`, `visibility = { version = "0.1.1", optional
= true }` (prior art: tokio's `test-util`; `visibility::make` verified on an `impl` method), and `[dev-dependencies]
agentfs-engine = { path = ".", features = ["test-util"] }` so its own `tests/` see them. Per item: with a production
caller in the engine, `#[cfg_attr(feature = "test-util", visibility::make(pub))] pub(crate)`; with none,
`#[cfg(any(test, feature = "test-util"))] pub`.
| Item | Becomes |
|---|---|
| `TreeActor::record_rows`, `recording::…::close_writer` | `pub(crate)` (engine only) |
| `TreeActor::{fork_with, drop_fork, create, create_with}`, `Recording::{create, create_with}` | test-util |
| `TreeGit::{open, open_with_lease, init, init_with_lease}` | test-util |
| `COMPACT_GATE_VAR`, `CompactGate`, `Gap` (re-export), `ChannelGate`, `Upkeep::gated` | test-util; `Upkeep.gate` `pub(crate)` |
| the `StateArea` port's `create`; the guard's named-writer calls (19) | documented outside the route (below) |
`Minter::new` is not in the list: a consumer's `main` is its composition root and mints (F1). The clippy bans stay.
[Erratum F: `clippy.toml`'s bans of `TreeGit::{open, init}` gain `allow-invalid = true` (the paths resolve only under
test-util); `Gap::ALL` is test-util and `Gap::name` test-util or failpoints, as nothing else calls them. The fix round
added `TreeActor::open_with`, `Recording::open_with` (make-pub) and `TreeActor::open`, `Recording::open`,
`tree_actor::wait_for_lease` (test-util), by this page's own rule.]
**Consumers** (sites): engine `tests/` (about 55, self dev-dependency, no edit); agentfs-fuse dev-dep test-util
(tree_view.rs tests, 1); agentfs-cli dev-dep test-util (cli_verify.rs, reference_tests.rs) and its `failpoints` adds
`"agentfs-engine/test-util"` (`upkeep()`); agentfs-conformance's engine dependency `features = ["test-util"]` (14).
**Not narrowed, and why.** The `StateArea` port's `create`: a trait method the Volume domain calls. The guard's 19
named-writer calls: the engine's call surface under sessions, used by the replay and its tests. Each is in agentfs.md's
"Outside the supported route" with the service call to use instead.
**CI** (ci.yml, lint job; a workspace-wide run unifies test-util on through conformance): `cargo clippy --locked -p
agentfs-engine -p agentfs-service -p agentfs --lib -- -D warnings` ("clippy (the library as a consumer builds it)") and
`cargo test --doc --locked -p agentfs`. **Check** (`ci/check-deps.sh`): `ok   library features`, failing when `cargo
tree --locked -p agentfs -e normal,features --prefix none --format '{p}'` has `^agentfs-[a-z-]+ feature
"(test-util|failpoints)"`. A consumer can still turn `agentfs-engine/test-util` on by depending on the engine by name.
**The launch's reads of core** (section 2's F item). `otel::init(volume: &str, identity: &otel::Identity) -> Otel`
with `pub struct Identity { pub namespace: String, pub pod: String, pub pod_uid: String, pub node: String, pub replica:
String }` (binary-local). The tree launch builds `Identity::of_pod(&PodArgs)` (`pod_uid` as given, as today); the
recording commands build it in main.rs from core's `Context`, keeping `--pod-uid`'s replica fallback (configuration.md,
unchanged): no observable change. `PodArgs::replica(&self) -> String` (pod name, else `crate::hostname()`) is that
rule's one home; main.rs's `Pod` answers `--replica`, else it; launcher's `identity()` goes. `fn scope() ->
InstrumentationScope` and `const TELEMETRY_EXPORT_FAILURES` move into otel.rs; core loses `telemetry::scope` and
`names::TELEMETRY_EXPORT_FAILURES` (its test harness takes a local scope, as the engine's does). Left for M9
(crate-split.md section 2): otel.rs's `Telemetry`, `OtelAlarmLog`, `AlarmLog` (every `init` builds the recording
handle, which registers `agentfs.nested.gitdir_heuristic` on a tree process's meter too) and daemon.rs's `Stop`
(`Daemon::watch`, the recording mount's).
**Docs.** agentfs.md's outside list (narrowed items gone; two documented); engine.md: `test-util`; development.md:
test-util beside failpoints, the two CI steps; crate-split.md row F "(built)", section 2's F item resolved; CHANGELOG.
**Acceptance.** `git grep -n agentfs_core -- crates/agentfs-cli/src/{launcher,otel.rs,daemon.rs,hold.rs,pod_args.rs,
mountpoint.rs} crates/agentfs-service` gives exactly two lines: otel.rs's `use agentfs_core::telemetry::{AlarmLog,
OtelAlarmLog, Telemetry}` and daemon.rs's `use agentfs_core::service::Stop`. Gates: fmt; clippy `--all-targets -D
warnings` and nextest of -engine (and `--features failpoints`), -fuse, -cli (and failpoints), -core, -conformance,
-service; the two CI steps; nextest -p agentfs-repo; `cargo build -p agentfs-conformance --bin phase1-replay`.
**Probes.** (1) `features = ["test-util"]` on agentfs-service's engine dependency → check-deps "library features".
(2) `TreeActor::create` called from agentfs-service's `src` → the new clippy step fails (E0624/E0599). (3)
`agentfs::engine::tree_actor::COMPACT_GATE_VAR` named in the front's doctest → `cargo test --doc -p agentfs` fails
while `--workspace` passes.
**Files.** agentfs-engine `Cargo.toml`, `tree_actor/{mod.rs,call_path.rs,recording/mod.rs,journal/compaction.rs}`,
`tree_git.rs`; Cargo.toml of -fuse, -cli, -conformance; agentfs-cli `src/{otel.rs,main.rs,pod_args.rs}`,
`src/launcher/{mod.rs,host.rs}`; agentfs-core `src/telemetry.rs`; `.github/workflows/ci.yml`; `Cargo.lock`;
`ci/check-deps.sh`; the docs named.

## Decisions

**Slice order.** F1 first: the service, the binary on it and the doctest are one green unit, and the route must exist
before the surface narrows to it. F2 edits launcher/mod.rs and host.rs after F1.

**Design choices.**
1. Rank `service`, after `engine`, used by `filesystem`, `umbrella`, `tests`; not `kubernetes` (csi spawns the binary).
2. Kept names: `Domains`, its calls, its log lines. New: `Launcher`, `Unstaged`, `ServiceError`, `init`, `end_fork`.
3. The caller mints. `Launcher` has private fields and takes the `Minter` and `LaunchCap` its composition root made, so
   the service is no minting point and system.md stands; `Minter::new` stays consumer API, as E documented.
4. One read of the operator settings: `stage` takes `&OperatorSettings`, which only `OperatorSettings::read` makes;
   the binary reads at boot, refuses early through `overlay_limit()` on that value, and stage applies the same value.
   Cost: nothing ties the value to `state`; a consumer could pass another volume's settings (documented).
5. One open path: every call opens through `TreeStateArea`, so `end_fork`'s busy is `VolumeError::Busy` too.
6. **Deviation from row F:** `identity::Context` does not move to contracts, which holds the pod context already
   (`PodContext`). The telemetry resource's identity is the binary's (`otel::Identity`); core keeps `Context` for its
   commit templates until M9. Row F says so (skeleton).
7. **The consumer surface, designed twice** (section 4 now records the choice). (a) Narrow the engine's items: a
   `test-util` feature and `pub(crate)` (F2). (b) Narrow the front: `umbrella` stops using `engine`, the front is
   contracts, the domains and the service, and the engine is reached by name like the leaves. (b) narrows nothing
   (a consumer that names the engine gets all of it), the service's API exposes engine types (`Sessions`, `Upkeep`,
   `OperatorSettings`), so a consumer needs two dependencies or the service re-exports them (two paths to one item),
   and it drops the engine from principle 10's "library" integration point and E's one name. agentfs-repo's front
   check would follow the rank's uses unchanged. (a) chosen: one name, items gone from default builds. Cost: one
   optional proc-macro dependency, a self dev-dependency, and a feature any consumer can still turn on.
8. Error policy: typed results where the caller decides (`bind`, `fork`, `end_keyed`, `stage`, `init`, `end_fork`);
   the rest reported as `tree <Name>` lines, the tree library's one reporting idiom (the engine's own, `tree
   SaveFailed` among them). Moving the tree library's reports to an alarm port is one change for engine and service
   together; section 4 records it as given up here.
9. Page F1 runs to about 90 lines: its signatures and contract are the page.

**Review round** (one, 14 findings):
1. P1 capability laundering: applied. `Launcher` fields private, built from the caller's `Minter` and `LaunchCap`; no
   `mint` in the service; Decision 3 rewritten; `Minter::new` stays consumer API; system.md stands.
2. P1 `Domains::open` around Volume: applied. Replaced by `end_fork(state, launcher, key)`, one call returning no
   `Domains`; only `stage` yields a type that can `bind`.
3. P2 launch still on core: applied by narrowing. F2 claims the launch's own modules; the two remaining reads are
   named, allowed by the widened grep, and routed to M9 in section 2.
4. P2 pod uid change: applied. `otel::Identity`; recording commands keep the replica fallback; no observable change.
5. P2 CI does not hold test-util: applied. Two lint-job steps without the feature; claim reworded.
6. P2 design-it-twice: applied. Decision 7 and crate-split.md section 4.
7. P2 drain flag: applied. `unstage(views) -> Unstaged`, `#[must_use]`; `drain_begun`/`drain` gone.
8. P2 `Recovered` and ordering: applied. `Recovered` gone (`init` writes the includes); contract and costs lines.
9. P2 settings read twice: applied. `stage(&OperatorSettings)`, `Default` dropped (Decision 4).
10. P2 one composition a convention: applied. cli drops the domain dependencies; `only_the_service_composes_the_domains`;
    conformance stays its own root, with the reason.
11. P2 error policy: applied as a decision (8), mechanism kept; an alarm port refuted for F (it would make the service
    report one way and the engine another).
12. nit conformance's `Minter::new`: applied (acceptance names it).
13. nit signal install: applied. Install after `Domains::stage`; the change is stated, csi's reading named.
14. nit errors and glossary: applied. `NotServed` an invariant; one `Busy`; `Recovering` folded; terms below.

**New terms.** `service`: the rank of `agentfs-service`. `Launcher`: who opens a volume, as the composition root
minted it (minter, capability, pod context, upkeep). `init` (the service's): Volume's `ensure`, then recovery, then
the includes under its lock; unlike `Volume::ensure` it recovers and writes settings. `end_fork`: `fork end` as one
call. `Unstaged`: whether an unstage began the drain. `test-util`: the engine feature that makes the test-only
surface `pub`. `otel::Identity` (erratum F): the binary-local identity the telemetry resource is built from
(namespace, pod, pod uid, node, replica as raw strings); unlike contracts' `PodContext` it carries the replica and
is not parsed; it replaces core's `identity::Context` for telemetry until M9 deletes that.
