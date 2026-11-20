# M1 slice pages (frozen, code architect, 2026-11-13)

Design: `m1-design.md` rev. 2 with its "Gate outcome (2026-11-13)". Branch `trees/module-cut-step-6`, PR #39.
Interfaces committed as compiling code: `6c7a936` (contracts and domain crates), `6d14e2c` (Volume's one state-area
port). Adapter signatures in agentfs-core, agentfs-fuse and the binary are frozen on these pages only: they change
existing code, so each lands with its slice. Owner answers recorded: `5aeebfe` (that commit also carries the `git mv` of `tree_id.rs`, so `5aeebfe` alone
does not compile; `6c7a936` does).

An implementer reads the shared part (section 0) and its own slice's page. The frozen code is the contract: where a
page and `crates/agentfs-contracts` disagree, the code wins and the page has a bug (append to `.claude/errata.md`
and STOP). Changing a frozen signature is an erratum, never a local edit.

## 0. Shared: what every slice builds on

### 0.1 Crates (committed)

| Crate | Holds | Depends on |
|---|---|---|
| `agentfs-contracts` | values (`name`, `tree`, `pod`, `audit`), capabilities (`caps`), each domain's trait, errors and events (`volume`, `clients`, `trees`, `views`), the ports (`ports`) | serde, thiserror, uuid (std, serde) |
| `agentfs-volume` | `ensure::decide_ensure`, `release::{decide, Cleared, verdict}`, `VolumeService<A, C, E>` | contracts |
| `agentfs-clients` | `ClientsService<S, B, F, E>` | contracts |
| `agentfs-trees` | `TreesService<S, Q, E>` | contracts |
| `agentfs-core` | the adapters: `state_area` (S1), `store` (S3), `session` (S4); the tree actor | contracts (+ align, git2, ...) |
| `agentfs-fuse` | the view host (`tree_view`), implementing `Views` from S4 | core, contracts |
| `agentfs` | the composition root: makes the `Minter`, wires services to adapters, computes nothing | all |

`TreeId` and `NotATree` moved to `agentfs_contracts::tree`; `agentfs_core::tree_actor` re-exports them, so no call
site changed. `ForkId(u64)` is new beside it (`TreeId::fork()`, `From<ForkId> for TreeId`).

A domain crate never depends on another domain crate. Cross-domain questions go through one-method traits in
contracts: Clients asks `ForkLookup::facts`, Volume asks `AttachedLookup::attached_any`.

### 0.2 The parked items, resolved

| Parked item (design, "For the code architects") | Resolution |
|---|---|
| The row ref layout | One commit chain per row, beside the tree it concerns: `refs/agentfs/volume` (Volume), `refs/agentfs/trees/fork/<n>` (Trees, one per fork, kept after its drop), `refs/agentfs/clients/<tree>` (Clients, one per tree). Beside the existing `refs/agentfs/objects/<tree>`. Each commit's tree holds one blob, `row.json` (the row's `Audited<T>` as JSON); its parent is the chain's previous tip; its message is the event (format in S3). The layout's one home stays `tree_actor/history/refs.rs` |
| How a unit's rows join the actor's step | A row commit is an object written in the step's `build`, before the commit point, like a save's commit. Its ref change is one more `PointerChange` in the same `Entry::Step`. So the branch and its row are both in the journal or neither; Git's refs are copies, as for branches |
| The outbox and its dedup key | No outbox table. The event commit is the record. Delivery is in-process: a domain publishes to `EventSink` after its step returns; the composition root's queue (`VecDeque<Event>`) hands each to its consumers in order. Delivery is not durable: at boot each consumer re-judges from asked facts (Trees `settle`, Clients `reconcile`, Volume `rejudge`), and every reaction is idempotent, so no dedup key is needed. `Agentfs-Event` (the step number) orders the record |
| The Clients index rebuilt at boot | `ClientsService::restore` folds `ClientStore::load()`. In M1 a view host dies with the service, so the launcher's first call after `stage` is `reconcile(&ClientSet::new())`, which ends every restored attachment (`Reconciled`) and publishes `LastDetached` once per tree |
| `Ordinal` allocation | The journal step number of the attach (unique per volume, never reused), as fork numbers already are |
| Whether conformance maps model writers to `ClientId`s | No mapping. The new invariant compares engine commits with engine commits: a landing merge's writer and pod trailers equal its second parent's. The library harness keeps the model's labels (`c3`) as `WriterLabel`s; the model and its digests do not move |
| The `uuid` crate | `uuid` 1.x. Contracts: `std` and `serde` (parse, text). agentfs-core: `v4` too, in `state_area.rs` only. Row added to architecture.md's Technology choices (in `6c7a936`) |
| The pod flags' shared clap group | `crates/agentfs/src/pod_args.rs`: `PodArgs` (`--namespace`, `--pod`, `--pod-uid`, `--node` with their `AGENTFS_*` env). The recording engine's `Pod` flattens `PodArgs` and keeps `--replica`; `trees` flattens `PodArgs` alone |
| Reserving room for an end (clients.md) | Not built in M1: an end is one small step; if the journal append fails the actor stops (as any failed step does) and the next boot reconciles. Cost stated in S4 |
| `--branch` refused on `trees init` | No flag is added just to refuse it: clap already refuses an unknown flag |

### 0.3 Departures from the design's sketches, with their cost

| Design sketch | Frozen | Why | Cost |
|---|---|---|---|
| `Ensured { id, state }` | `Ensured { id, outcome: EnsureOutcome }` (`Created` / `Found`) | `ensure` on an existing volume reads `format` and `volume` only, without the lock, so it never opens the journal the state would be folded from; the state comes from `stage` | a launcher learns the lifecycle state at `stage`, not `ensure` |
| `MountOptions { allow_other }` with the owner from the grant | same; until S4 (no grant yet), `tree_view::Binding` carries `owner: FileOwner` from `--owner` | one home for the presented owner (the grant) from S4 | S4 removes the field |
| `Clients::attached_any` | on `AttachedLookup`, a supertrait of `Clients` | Volume asks one question of Clients | none |
| `Trees::facts` | on `ForkLookup`, a supertrait of `Trees` | Clients asks one question of Trees | none |
| `Blocker::Remote`, `Blocker::ForksLeft` | absent; `Settlement::NoRemotes` only | M4 builds settlement; the upgrade drain is deferred | M4 adds `Blocker::Remote` and grows `decide` |
| `Grant` with a `ViewProfileId` | no view profile | Rules (M3) is unbuilt; every client sees everything | M3 adds the field (a breaking change to `Grant`, accepted) |
| Volume rows "with stamps" in a database | `Audited<T>` in each row commit's `row.json`, stamped by `agentfs-core`'s `store` | the gate outcome: rows on record in git | stamps are written by convention of the one adapter (the fields are `pub`); S3's probe guards it |

### 0.4 Formats every slice shares

**Trailers of a file commit** (Root, Direct, Landing, Sync), in this order, each line ending in `\n`:

```text
<subject>

Agentfs-Writer: <writer>
Agentfs-Cause: <Root | Direct | Landing | Sync>
Agentfs-Tree: <root | fork/<n>>
Agentfs-Event: <step number, 0 for Root>
Agentfs-Volume: <volume name>
Agentfs-Volume-Id: <uuid, hyphenated lower case>
Agentfs-Namespace: <ns>          only when the writer's pod context has it
Agentfs-Pod: <pod>               likewise
Agentfs-Pod-Uid: <uid>           likewise
Agentfs-Node: <node>             likewise
Agentfs-Format: envelope-1       every commit but the Root
Agentfs-Open: <n>                a Direct save that closed an open
Agentfs-Collisions: <json>       a Landing that decided any
```

Subjects: `Root`; `<Create|Rename|Unlink|Direct> by <writer>`; `Landing by <writer>` (was `Landing`); `Sync`. The
writer and pod trailers of a Landing are its second parent's (git-model §3); of a Sync and the Root, `agentfs` and
the history service's pod context.

**Event (row) commits** (from S3), on the row chains of 0.2:

```text
<Kind> <subject detail>

Agentfs-Kind: <Kind>
Agentfs-By: <client <ClientId> | launcher <id> | agentfs>
Agentfs-Tree: <tree>             on a tree's chain only
Agentfs-Event: <step number>
Agentfs-Volume: <name>
Agentfs-Volume-Id: <uuid>
Agentfs-Namespace / -Pod / -Pod-Uid / -Node: the principal's pod context, when present
Agentfs-Format: row-1
```

Kinds: `ForkCreated`, `ForkEnded`, `ForkDropped` (Trees); `Attached`, `AttachmentEnded` (detail: client and
reason), `LastDetached`, `ClaimReleased` (Clients); `VolumeStaged`, `VolumeDraining`, `VolumeReleased` (Volume).
No event commit carries `Agentfs-Writer` or `Agentfs-Cause`, so no file-commit reader (`direct_event`, `tree_of`
in a take, the owners' rebuild) mistakes one.

**The state area, format 4** (`agentfs-trees-4\n`):

| File | Written | Holds |
|---|---|---|
| `initializing` | first, by `create`; removed last | marks a create a crash may cut short: the next `create` clears it |
| `volume` | by `create`, after the marker, before the repository | `{"name":"<name>","id":"<uuid>"}\n` (`VolumeIdentity` as JSON), fsynced, renamed into place |
| `history.git`, `calls.wal` | by `create` | as format 3, plus the trailers and journal fields above |
| `format` | last, by `create` | `agentfs-trees-4\n` |
| `actor.lock` | by the first locker | the actor's lock |

`inspect` (no lock, no write): absent dir, empty dir, a dir holding only `actor.lock`, or one with `initializing`
→ `Nothing`; `format` = format 4 and a readable `volume` → `Volume(identity)`; anything else → `Other`.

---

## S1 (tracer). Identity and pod context

**Needs:** nothing. **Run:** full (`phase-1.yml` dispatch with the shards).
**Builds:** `ensure`; the volume identity file; format 4 (write-opens and steps journaled with their pod context);
the six trailers; landing merges naming their writer; the pod flags; `trees mount` with `allow_other` and
`--owner`; `--volume` on every `trees init` caller; `phase-1.yml` on PRs and `main`; tracer steps 1 to 3.

**Owned files.** `crates/agentfs-contracts/src/name.rs`, `pod.rs`, `volume.rs` (bodies only);
`crates/agentfs-volume/src/{ensure.rs,service.rs}` (`ensure` only); `crates/agentfs-core/Cargo.toml`;
`crates/agentfs-core/src/config.rs` (`validate_name` calls `is_safe_name`); `crates/agentfs-core/src/state_area.rs`
(new) and `lib.rs`; `crates/agentfs-core/src/tree_actor/**`; `crates/agentfs-core/tests/{tree_*.rs,support/**}`,
`benches/record_step.rs` if it opens a tree actor; `crates/agentfs-fuse/src/tree_view.rs`;
`crates/agentfs/src/{trees_cmd.rs,pod_args.rs,main.rs}`; `clippy.toml`; `crates/agentfs-conformance/src/{library.rs,
mounted.rs,invariants.rs}`; `ci/*tree*` and `ci/power-loss-trees*.sh`, `ci/measure-trees.sh`, `ci/trees-restart.py`
(the `trees init --volume` calls); `ci/e2e-trees-tracer.sh` (new); `.github/workflows/{phase-1.yml,image.yml}`;
`docs/configuration.md`, `docs/trees.md`, `docs/architecture.md` (new module rows), `docs/target-architecture/git-model.md`
(one sentence in §3), `CHANGELOG.md`.

**Contracts built (bodies of committed signatures).** `name::is_safe_name` (moved from `config::validate_name`,
which then calls it), `VolumeName::parse` (safe name, ≤ 253 bytes), `Label::parse` (1..=253 bytes, ASCII graphic, no
whitespace), `WriterLabel::parse` (1..=256 bytes, no control char: today's `writer_label`), `WriterLabel::agentfs`,
`LauncherId::parse` (safe name, ≤ 64), `decide_ensure`, `VolumeService::ensure` (checks `by.may() == Root` else
`NotPermitted`; `inspect` → `decide_ensure` → `Create` calls `area.create`, `Found` answers it, `Refuse` answers the
error). `stage`, `unstage`, `release_verdict`, `rejudge` stay `todo!()` (nothing calls them until S6).

**New in agentfs-core.**

```rust
// crates/agentfs-core/src/tree_actor/mod.rs
const FORMAT: &str = "agentfs-trees-4\n";
const IDENTITY: &str = "volume";

pub struct TreeActor { /* was `Volume`; gains: */ identity: VolumeIdentity, service: PodContext, /* ... */ }

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ... as today, minus `Writer` (a `WriterLabel` is valid by construction), plus:
    #[error("no tree volume here: create it with `agentfs trees init --volume NAME`")]
    NotCreated,
}

impl TreeActor {
    /// Creates a volume in an absent or empty directory (or one a cut-short create left under its marker):
    /// the marker, `volume`, the repository and its Root commit (writer `agentfs`, `service`'s pod context),
    /// the journal, then `format`; each fsynced, the directory too. `Error::Exists` for any other directory.
    pub fn create(root: &Path, identity: &VolumeIdentity, service: &PodContext) -> Result<TreeActor>;
    /// Recovers and serves. Reads `format` and `volume` before any write: `NotCreated` for `Found::Nothing`,
    /// `Format` for `Found::Other`. Recovery closes each abandoned open as its own writer, with its journaled
    /// pod context; `service` is carried by the Sync commits this run writes.
    pub fn open(root: &Path, service: &PodContext) -> Result<TreeActor>;
    pub fn identity(&self) -> &VolumeIdentity;
    // Every method that took `writer: &str` takes `by: &WriterAt`:
    pub fn write_open(&mut self, tree: TreeId, path: &str, by: &WriterAt, create: bool) -> Result<u64>;
    pub fn write_open_truncating(&mut self, tree: TreeId, path: &str, by: &WriterAt, create: bool) -> Result<u64>;
    pub fn anonymous_open(&mut self, tree: TreeId, by: &WriterAt) -> Result<u64>;
    pub fn first_link(&mut self, tree: TreeId, object: u64, path: &str, by: &WriterAt) -> Result<()>;
    pub fn rename(&mut self, tree: TreeId, from: &str, to: &str, by: &WriterAt) -> Result<()>;
    pub fn unlink(&mut self, tree: TreeId, path: &str, by: &WriterAt) -> Result<()>;
    // `fork()` unchanged in S1 (S3 replaces it).
}

/// What a state area holds: reads `format` and `volume` only, takes no lock, writes nothing.
pub fn inspect(root: &Path) -> io::Result<Found>;   // agentfs_contracts::ports::Found

// tree_actor/journal/mod.rs (format 4; the `format` file gates it, so the frame magic stays AFJ2)
Entry::Open { tree: String, path: String, by: WriterAt, object: u64 }
Entry::AnonymousOpen { tree: String, by: WriterAt }
Entry::Step { by: WriterAt, closes: Option<u64>, opened: Option<OpenRecord>, linked: Option<u64>,
              fork: Option<(String, String)>, pointers: Vec<PointerChange> }
pub(super) struct OpenRecord { pub handle: u64, pub tree: String, pub by: WriterAt, pub object: u64, pub anonymous: bool }
// recording::Handle { tree, by: WriterAt, object, bytes, calls }; Draft::new(by: &WriterAt, tree, built)

// tree_actor/history/message.rs
pub(in crate::tree_actor) enum Message<'a> {
    Root { service: &'a PodContext },
    Direct { action: Action, by: &'a WriterAt, tree: TreeId, event: EventNumber, open: Option<u64> },
    /// `landed`: the queued commit's writer and pod context, read by `writer_at`.
    Landing { landed: &'a WriterAt, tree: TreeId, event: EventNumber, collisions: &'a [referee::Collision] },
    Sync { service: &'a PodContext, tree: TreeId, event: EventNumber },
}
impl Message<'_> {
    pub(in crate::tree_actor) fn render(&self, volume: &VolumeIdentity) -> Result<String>;
}
/// The first `Agentfs-Writer` and the pod trailers: `None` if no writer trailer or not UTF-8;
/// `Error::Format` for a writer or pod value that does not parse.
pub(in crate::tree_actor) fn writer_at(message: &[u8]) -> Result<Option<WriterAt>>;

// crates/agentfs-core/src/state_area.rs (new): the StateArea adapter
pub struct TreeStateArea { /* root: PathBuf, actor: Option<Arc<Mutex<TreeActor>>> */ }
impl TreeStateArea {
    pub fn at(root: &Path) -> Self;
    /// The actor `open` recovered; `None` before.
    pub fn actor(&self) -> Option<Arc<Mutex<TreeActor>>>;
}
impl StateArea for TreeStateArea { /* inspect → tree_actor::inspect; create → lock, inspect again, a new
    VolumeId(Uuid::new_v4()), TreeActor::create, drop it; open → TreeActor::open, keep it */ }
// VolumeStore and QueueFacts for TreeStateArea: S6 (S1 leaves them unimplemented; nothing needs them).
```

`agentfs-core` re-exports nothing new beyond `tree_actor::TreeActor`, `tree_actor::inspect`,
`state_area::TreeStateArea`. A test helper keeps the 135 call sites one line:
`crates/agentfs-core/tests/support/trees.rs`: `pub fn new_actor(dir: &Path) -> TreeActor` (creates with name `test`,
a fixed id, empty pod context) and `pub fn reopen(dir: &Path) -> TreeActor`; `pub fn writer(label: &str) -> WriterAt`.
The unit tests in `tree_actor` get the same three as `#[cfg(test)]` functions.

**agentfs-fuse (`tree_view.rs`).**

```rust
pub struct Binding { pub tree: TreeId, pub by: WriterAt, pub owner: FileOwner, pub mountpoint: PathBuf }
pub fn serve(state: &Path, actor: TreeActor, bindings: Vec<Binding>, options: MountOptions) -> io::Result<()>;
pub fn serve_with_metrics(state: &Path, actor: TreeActor, bindings: Vec<Binding>, options: MountOptions,
                          metrics_output: Option<&Path>) -> io::Result<()>;
/// Pure: the fuser config for one view. `SessionACL::All` (FUSE allow_other) when `options.allow_other`,
/// `SessionACL::Owner` otherwise; the mount options of today.
fn session_config(options: &MountOptions) -> fuser::Config;
```

Every attribute's `uid` and `gid` is the binding's `owner` (was `geteuid()`), until M2.

**CLI.**

| Command | Flags (env) | Does | Output, exit |
|---|---|---|---|
| `agentfs --state S trees init` | `--volume NAME` (`AGENTFS_VOLUME`, required), `PodArgs` | `ensure` with `LaunchCap(cli, Root)` | stdout `<name> <uuid> created\|found`; `SpecMismatch`, `OtherFormat`: message, exit 1; `Busy`: exit 75 |
| `trees fork` | `PodArgs` | as today (until S5), on `TreeActor::open` | as today |
| `trees mount ROOT` | as today, plus `--volume NAME` (`AGENTFS_VOLUME`, optional: a guard, refused before any write when the identity's name differs), `--owner UID:GID` (`AGENTFS_OWNER`, default the daemon's euid:egid), `--no-allow-other`, `PodArgs` | `TreeActor::open` (never creates: `NotCreated` → message, exit 1); every binding's `by` = its `--writer` with the mount's pod context | as today |

The composition root is the only `Minter::new` caller: `clippy.toml` gains
`{ path = "agentfs_contracts::caps::Minter::new", reason = "capabilities are minted only in the composition root" }`
and `main` carries the one `#[allow(clippy::disallowed_methods)]`.

**CI.** `phase-1.yml`: `on:` gains `pull_request:` and `push: branches: [main]` with the same `paths:`; the
`replay-seeds` job keeps its `if:` (dispatch or `[phase1-seeds]`). `image.yml`: a `trees-tracer` job running
`ci/e2e-trees-tracer.sh` on the built image: init and mount in a privileged agentfs container, the workload in a
second container as `10000:10000` sharing the mount by propagation (the Docker example's shape). Steps: (1) `trees
init --volume t` twice → one id, `created` then `found`; `--volume u` at the same state → exit 1, every file's
sha256 under the state dir unchanged; (2) the workload writes `a.txt`; the root's head, read with `git` right after
the workload's `close` returns, holds it with `Agentfs-Volume: t`, its id and the pod trailers given; (3) `kill -9`
agentfs mid-stream, `umount -l`, mount again, restart the workload: every write the workload saw answered is in
history, with its pod trailers.

**Acceptance tests.**

| Test (where) | Shows |
|---|---|
| `ensure_three_times_gives_one_id` (`agentfs-volume` unit over a fake `StateArea`; `agentfs-core/tests/state_area.rs` on a real dir) | idempotent create |
| `another_name_leaves_the_state_area_byte_identical`, `format_3_is_refused_unchanged` (core, hashes every file's bytes before and after) | refusal before any write |
| `a_fork_launcher_cannot_ensure` (volume unit) | VO6 |
| `a_recovered_open_commits_with_its_writers_pod_context` (core: open under pod A, drop the actor, `open` with service pod B, the recovery close's commit carries `Agentfs-Pod: A`) | rule 2 |
| `each_message_renders_its_literal_bytes` (message.rs, updated: all four kinds with and without pod fields) | the trailer format |
| `writer_at_reads_back_what_render_writes` (message.rs) | the landing's reader |
| `a_landing_names_its_landed_writer` (core: fork writer `w` under pod P lands; the merge's subject is `Landing by w`, its writer and pod trailers are `w`, P) | engine = git-model §3 |
| conformance `landing_names_its_writer` invariant (invariants.rs; core tier, every replay) | the same on every trace |
| `session_config_allows_other_by_default` (tree_view unit) | allow_other |
| `trees_init_needs_a_volume`, `mount_never_creates` (agentfs CLI tests) | the CLI contract |
| the tracer job | end to end |

**Mutation probes (each must turn a named test red).** `recover` (`Found`) makes a new id →
`ensure_three_times_gives_one_id`; check the name after the journal opens (write `actor.lock` or the journal first) →
`another_name_leaves_the_state_area_byte_identical`; recovery writes the restarted service's pod context →
`a_recovered_open_commits_with_its_writers_pod_context`; a landing writes `agentfs` → `a_landing_names_its_landed_writer`
and the conformance invariant; `session_config` without allow_other → `session_config_allows_other_by_default` and the
tracer's step 2.

**Docs.** configuration.md: rows for `trees init --volume`, `trees mount --volume`, `--owner`, `--no-allow-other`,
and the pod flags on `trees`; docs/trees.md: identity, the trailers, other users; git-model §3: one sentence naming
the new keys; architecture.md: `state_area.rs`, `pod_args.rs`; CHANGELOG Unreleased: tree format 4 (format 3
refused), the trailers, `trees init --volume`.

**Known costs.** Every `trees init` caller gains `--volume` (11 scripts, `trees-restart.py`, the mounted harness).
Engine tests expecting `Agentfs-Writer: agentfs` on a landing change. Model gate: light (no model change); the
conformance invariant is new, so run the replay (library and mounted) per `model-runs`.

---

## S2. Release figures

**Needs:** S1. **Run:** core.
**Builds:** tracer step 4: the design's §8 workloads on the shipped image's release build, on ext4 and XFS, as a
JSON report with each figure's guarantee. Warnings only; no gate fails on a figure.

**Owned files.** `ci/e2e-trees-tracer.sh` (step 4 behind `--figures`), `ci/trees-figures.py` (new: drives the
workloads through the mount and writes the report), `ci/trees-budget.json` (new), the `trees-tracer` job in
`.github/workflows/image.yml` (a matrix on `fs: [ext4, xfs]` over a loop device), `docs/performance.md` (a section
"Tree views, release build" with the first figures, filled from CI).

**No Rust interface changes.** The image already ships the release `agentfs`.

**The report** (`$RUNNER_TEMP/trees-figures-<fs>.json`, uploaded as an artifact; its schema is frozen here):

```json
{"build": "release", "filesystem": "ext4", "image": "<digest>",
 "workloads": [
  {"name": "close_4k", "count": 200, "p50_ms": 0.0, "p99_ms": 0.0,
   "budget": "p50 3.5 ms (record close 4k)", "warn": false,
   "guarantee": "committed before close returns"},
  {"name": "create_1000", "count": 1000, "total_s": 0.0, "extrapolated_100000_s": 0.0,
   "budget": "600 s for 100,000 (create)", "warn": false,
   "guarantee": "committed before close returns"},
  {"name": "close_64m", "bytes": 67108864, "total_s": 0.0, "extrapolated_1g_s": 0.0,
   "budget": "7-12 s for 1 GiB (bigclose)", "warn": false,
   "guarantee": "committed before close returns"}]}
```

`ci/trees-budget.json` holds the recording engine's budgets above, as numbers; `warn` is true when a figure exceeds
its budget, and the job prints a `::warning::` line for it. Extrapolations are linear from the measured count.

**Acceptance.** The report lists every workload with its figure and extrapolation, on both filesystems, from a
green `image` run. **Mutation probe:** a 50 ms sleep in the save path (`tree_actor` `run_step`, local build of the
image) sets `warn` on `close_4k` and prints the warning. **Ends with the owner's question** (the speed bar for M7's
gate) carrying the figures.

---

## S3. Store: volume events as commits beside their trees

**Needs:** S1. **Run:** full.
**Builds:** row chains in the tree actor's step; the `store` adapter; Trees' `restore`, unkeyed `create`, `facts`,
`live`; fork creation everywhere goes through `Trees::create` and writes `ForkCreated` in the same step as the branch.

**Owned files.** `crates/agentfs-core/src/tree_actor/{mod.rs,history/refs.rs,history/message.rs,history/mod.rs}`,
`crates/agentfs-core/src/store.rs` (new) and `lib.rs`; `crates/agentfs-trees/src/service.rs` (S3's methods);
`crates/agentfs/src/trees_cmd.rs` (`trees fork` and `--sandbox` creation call `Trees::create`);
`crates/agentfs-conformance/src/{library.rs,invariants.rs,git_history.rs}` (fork through the store; history walks
exclude `refs/agentfs/`); `crates/agentfs-core/tests/tree_store.rs` (new); `docs/architecture.md`, `docs/trees.md`
(where events are on record), `docs/target-architecture/git-model.md` §4 (the row refs in the table: kept locally,
pushed by Replication from M4).

**Tree actor (agentfs-core).**

```rust
// tree_actor/history/refs.rs
/// A row chain: `refs/agentfs/<domain>` or `refs/agentfs/<domain>/<tree>`. `domain` is one of
/// `ROW_DOMAINS`; never `objects`.
pub(in crate::tree_actor) fn row_chain(domain: RowDomain, tree: Option<TreeId>) -> String;
pub const ROW_DOMAINS: [&str; 3] = ["volume", "trees", "clients"];

// tree_actor/mod.rs (public: the store adapter's surface, domain-agnostic)
/// Which chain a row extends. The actor knows no domain: the store names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowDomain { Volume, Trees, Clients }
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chain { pub domain: RowDomain, pub tree: Option<TreeId> }

/// One event commit a step writes: `row` is the chain's new `row.json`, `message` its event.
pub struct RowWrite { pub chain: Chain, pub row: Vec<u8>, pub event: RowEvent }
/// The parts of an event commit's message the store decides; `Message::Row` renders it with the shared trailers.
pub struct RowEvent { pub kind: &'static str, pub detail: String, pub by: String, pub at: PodContext }

/// The step number a step is given (was internal `EventNumber`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StepNumber(pub u64);

impl TreeActor {
    /// One step: a fork of `upstream` at its saved head (number = the step's) and the rows `rows` builds
    /// for it, chained in order. Replaces `fork()`. `Error::NotFound` for an upstream that is no tree.
    pub fn fork_with<F>(&mut self, upstream: TreeId, by: &WriterAt, rows: F) -> Result<TreeId>
    where F: FnOnce(TreeId, StepNumber) -> Result<Vec<RowWrite>>;
    /// One step of rows alone (no branch moves).
    pub fn record_rows<F>(&mut self, by: &WriterAt, rows: F) -> Result<StepNumber>
    where F: FnOnce(StepNumber) -> Result<Vec<RowWrite>>;
    /// A chain's current `row.json`, from the journal's pointers (never Git's copy). `None`: no chain yet.
    pub fn row(&self, chain: &Chain) -> Result<Option<Vec<u8>>>;
    /// Every chain of `domain` with its row, in ref order.
    pub fn rows(&self, domain: RowDomain) -> Result<Vec<(Chain, Vec<u8>)>>;
    /// Every tree whose queue holds an entry; whether `fork`'s upstream's queue holds one of it (was
    /// `pending_from`); whether a fork names `fork` as upstream.
    pub fn queued(&self) -> Vec<TreeId>;
    pub fn pending_from(&self, fork: TreeId) -> Result<bool>;   // exists today
    pub fn has_child(&self, fork: TreeId) -> bool;
}
```

`Message` gains `Row { event: &'a RowEvent, tree: Option<TreeId>, step: EventNumber }`, rendered in 0.4's event
format. Each `RowWrite` becomes a commit (tree: one blob `row.json`; parent: the chain's tip from `pointers`, or
the previous `RowWrite` on the same chain in this step) and one `RefUpdate` with `expected_old` = the tip read.

**Store adapter (`crates/agentfs-core/src/store.rs`, new).**

```rust
/// The domains' rows over the volume's one tree actor: each write is one journal step.
#[derive(Clone)]
pub struct TreeStore { /* actor: Arc<Mutex<TreeActor>>, clock: fn() -> UnixNanos */ }
impl TreeStore {
    pub fn new(actor: Arc<Mutex<TreeActor>>) -> Self;
}
impl ForkStore for TreeStore { /* S3: load, create; S5: end, drop_fork */ }
impl QueueFacts for TreeStore { /* queued, pending_from, has_child: the actor's */ }
// S4: impl ClientStore; S6: impl VolumeStore (TreeStateArea delegates to its TreeStore after open).
```

Stamping, its one home (`store.rs`): a first write sets `created = changed = Stamp { at: now, by }`; every later
write copies `created` from the chain's current row and sets `changed`. Rows are `serde_json` of `Audited<T>`.
`StoreError` from `tree_actor::Error`: `Stopped` → `Stopped`; `TreeGit(NoSpace)` → `NotDurable(NoSpace)`; `Io(e)`
→ `NotDurable(Other(errno))`; `Format`, `CorruptJournal`, `MissingObject`, JSON → `Damaged`; `NotFound`, `Busy`
in a step → `Refused`.

**Trees (S3 part).** `TreesService::restore` (load every row), `facts`, `live` (phase not `Dropped`), `create` with
`key: None` (`NestedNotBuilt` for a fork upstream; `UpstreamUnknown` from `Refused`; publishes
`TreesEvent::ForkCreated`); `create` with `Some(key)` answers `todo!()` until S5 (nothing passes a key before S5).
`by` must be `Root` or the creating launcher; `created_by` = `by.launcher()`.

**The trial CLI in S3.** `trees fork` and `trees mount --sandbox` without `--fork` call `Trees::create(Root, None,
ForkSettings { landing: Automatic }, cap)` on a `TreeStore` over the opened actor.

**Conformance.** `Library::fork` goes through `TreeStore` + `TreesService` (the row is written in every replay);
`check_new_commits` and `git_history` walk `rev-list --all --exclude=refs/agentfs/*` (row chains are not file
commits).

**Acceptance tests (`crates/agentfs-core/tests/tree_store.rs`).**

| Test | Shows |
|---|---|
| `a_fork_and_its_row_are_one_step` (failpoint `tree.after_objects_durable` kill: after reopen neither the branch nor `refs/agentfs/trees/fork/<n>`; `tree.after_commit_point` kill: both) | one commit point |
| `created_survives_a_change_and_compaction` (two writes to one chain, `compact()`, reopen: `created` is the first write's, `changed` the second's) | the stamps |
| `a_row_chain_is_a_commit_chain_of_events` (git: `refs/agentfs/trees/fork/1` is a commit whose message is `ForkCreated` with 0.4's trailers and whose tree is `row.json`) | on record in git |
| `rows_are_read_from_the_journal_not_git` (delete the ref file after a step; `row()` still answers) | one source of truth |
| trees unit: `create_twice_unkeyed_makes_two_forks`, `nested_is_refused` | Trees' S3 surface |

**Mutation probes.** Write the row in its own step (two `run_step`s) → `a_fork_and_its_row_are_one_step` at the
first failpoint; restamp `created` on change → `created_survives_a_change_and_compaction`.

---

## S4. Clients and views

**Needs:** S1, S3. **Run:** full.
**Builds:** `ClientsService` (`restore`, `attach`, `detach`, `reconcile`, `attached`, `attached_any`); `ClientId` as
the `Agentfs-Writer` text; the fork writer claim; in-process sessions; the view host implementing `Views`; the
tree actor's close of an ended client's open files.

**Owned files.** `crates/agentfs-contracts/src/{clients.rs,name.rs}` (bodies: `ClientId` `Display`/`FromStr`,
`ClientLabel::parse`); `crates/agentfs-clients/src/service.rs`; `crates/agentfs-core/src/store.rs` (`ClientStore`);
`crates/agentfs-core/src/session.rs` (new) and `lib.rs`; `crates/agentfs-core/src/tree_actor/recording/mod.rs`
(`close_writer`); `crates/agentfs-fuse/src/tree_view.rs`; `crates/agentfs/src/trees_cmd.rs`; `crates/agentfs-core/
tests/tree_clients.rs` (new); `docs/configuration.md` (`--writer` becomes a client label), `docs/trees.md`.

**ClientId text.** `<label>#<n>@<launcher>/<tree>`, e.g. `root-client#5@cli/root`, `sandbox-client#9@cli/fork/3`.
`ClientLabel` and `LauncherId` are safe names (no `#`, `@`, `/`), so `FromStr` splits at the first `#`, the first `@`
after it and the first `/` after that; the rest is a `TreeId`. `WriterLabel::parse(&id.to_string())` always succeeds.

**Clients (pure, over its ports).** In one step per attach: `NotPermitted` when `by.may()` is `ForksItCreates` and the
grant's tree is the root or `forks.facts(f).created_by != by.launcher()`; `TreeUnknown` when a fork's facts are
`None` or not `Open`; `ForkTaken` when a read-write grant on a fork finds a claim; `Draining` after
`VolumeDraining` (S6 delivers it; until then never). Then `store.attach(&NewClient{..}, claim, &Principal::Launcher)`
→ `ClientId`; for a read-write grant, `sessions.bind(session, SessionAuthority::Write(ClientCap::mint(&minter, id,
tree, WriterAt { writer: id text, at })))`, else `Read(tree)`; publishes `Attached`. Detach and reconcile: one
`store.end` with every ended client and each tree left empty (`LastDetached` once per tree, C6); `sessions.unbind`;
publishes `AttachmentEnded` then `LastDetached`. Inherent consumer methods (composition root calls them; the first two are frozen here and added in S4, the third is committed):

```rust
impl<S, B, F, E> ClientsService<S, B, F, E> {
    pub fn on_volume_draining(&mut self);   // attach answers Draining from now (S6)
    pub fn on_volume_staged(&mut self);     // attach accepted again (S6)
    pub fn on_fork_dropped(&mut self, fork: ForkId) -> Result<(), StoreError>;   // committed; body S5
}
```

**Sessions (`crates/agentfs-core/src/session.rs`, new).**

```rust
/// The history service's sessions, in-process: each binds one client's authority.
#[derive(Clone)]
pub struct Sessions { /* actor: Arc<Mutex<TreeActor>>, bound: Arc<Mutex<BTreeMap<SessionId, SessionAuthority>>>, next: Arc<AtomicU64> */ }
impl Sessions {
    pub fn new(actor: Arc<Mutex<TreeActor>>) -> Self;
    /// A fresh session id (unique per process run).
    pub fn open(&self) -> SessionId;
    /// The handle a view serves through; `None` for an unknown or unbound session.
    pub fn session(&self, id: SessionId) -> Option<TreeSession>;
}
impl SessionBinder for Sessions { /* bind, unbind */ }

/// One client's handle on its own tree. Its calls name no client and no tree (C5).
#[derive(Clone)]
pub struct TreeSession { /* actor, tree: TreeId, writes_as: Option<WriterAt> */ }
impl TreeSession {
    pub fn tree(&self) -> TreeId;
    /// The actor locked for this session's calls.
    pub fn lock(&self) -> Result<SessionGuard<'_>, tree_actor::Error>;
}
pub struct SessionGuard<'a> { /* MutexGuard<'a, TreeActor>, tree, writes_as */ }
impl SessionGuard<'_> {
    pub fn read(&self, path: &str) -> Result<&[u8]>;
    pub fn files(&self) -> Result<Vec<(String, u64, usize)>>;
    pub fn is_anonymous(&self, object: u64) -> Result<bool>;
    pub fn inode_size(&self, object: u64) -> Option<u64>;
    pub fn open_object(&self, handle: u64) -> Result<u64>;
    pub fn read_open(&self, handle: u64) -> Result<&[u8]>;
    pub fn page_fill(&mut self, object: u64, offset: u64, size: u32) -> Result<Vec<u8>>;
    pub fn sync(&self) -> Result<()>;
    // Writes: `Error::ReadOnly` (new, errno EROFS) when the session has no ClientCap:
    pub fn write_open(&mut self, path: &str, create: bool) -> Result<u64>;
    pub fn write_open_truncating(&mut self, path: &str, create: bool) -> Result<u64>;
    pub fn anonymous_open(&mut self) -> Result<u64>;
    pub fn first_link(&mut self, object: u64, path: &str) -> Result<()>;
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    pub fn unlink(&mut self, path: &str) -> Result<()>;
    // Handle calls: `Error::NotFound` for a handle of another tree.
    pub fn write(&mut self, handle: u64, offset: u64, bytes: &[u8], append: bool) -> Result<()>;
    pub fn truncate(&mut self, handle: u64, size: u64) -> Result<()>;
    pub fn page_writeback(&mut self, handle: u64, offset: u64, bytes: &[u8]) -> Result<()>;
    pub fn close(&mut self, handle: u64) -> Result<Saved>;
}
```

The view host keeps `progress`, `drain_changed`, `metrics` and `is_stopped` on the shared actor (the service's,
not a client's). `TreeActor` gains:

```rust
/// AttachmentEnded's consumer: every open of `writer` in `tree` closes as a save (git-model §6), in handle order.
pub fn close_writer(&mut self, tree: TreeId, writer: &WriterLabel) -> Result<Vec<Saved>>;
```

**View host (`tree_view.rs`).** `pub struct ViewHost` implements `Views`:
`ViewHost::new(sessions: Sessions, actor: Arc<Mutex<TreeActor>>, state: &Path, metrics_output: Option<&Path>) ->
io::Result<ViewHost>`; `open` resolves `binding.session` through `Sessions::session` (`UnknownSession`), checks the
mount point as today (`MountFailed(EINVAL)`), mounts with `session_config(&options)` and presents
`binding.grant.acts_as()`; `close` unmounts; `ViewHost::wait(self) -> io::Result<()>` joins every session and the
progress worker (today's tail of `serve_with_metrics`). `tree_view::Binding` and `serve*` go.

**The trial CLI.** `trees mount`: `--writer LABEL` and `--sandbox-writer LABEL` are `ClientLabel`s (default
`root-client`, `sandbox-client`); `--read-only` attaches the root read-only. Flow: `stage`-less open (S6 adds it) →
`ClientsService::restore` → `reconcile(&ClientSet::new())` → attach each view's client (pod context = the mount's)
→ `ViewHost::open` → `wait` → `detach` each. A fork remounted after a restart has a spent claim: `ForkTaken`, the
CLI says "this fork has had its writer; mount it with --sandbox-read-only or use a new fork". `--sandbox-read-only`
attaches the sandbox read-only.

**Acceptance tests (`crates/agentfs-core/tests/tree_clients.rs` and clients unit tests).**

| Test | Shows |
|---|---|
| `a_second_writer_on_a_fork_is_fork_taken` (before and after reopen) | C4 |
| `three_clients_killed_all_end_at_boot` (attach three over two trees, drop the actor, reopen, `reconcile(empty)`: three `AttachmentEnded(Reconciled)`, `LastDetached` once per tree, rows empty) | the index rebuilt at boot; C6 |
| `a_commit_names_its_sessions_client` (a session's save: `Agentfs-Writer: root-client#<n>@cli/root`) | C5, ClientId as writer |
| `a_read_only_session_cannot_write` (`EROFS`) | C3 |
| `a_fork_launcher_reaches_only_its_forks` (clients unit with a fake `ForkLookup`) | C2 |
| `an_ended_clients_open_file_is_saved` (`close_writer`) | AttachmentEnded's consumer |
| `client_id_text_round_trips` (contracts unit) | the writer format |

**Mutation probes.** Delete the claim at detach instead of at `ForkDropped` →
`a_second_writer_on_a_fork_is_fork_taken` (after reopen); publish `LastDetached` per ended client →
`three_clients_killed_all_end_at_boot`.

**Costs.** A fork's writer cannot write again after a restart until `rebind` is built (scripts that rewrite a sandbox
after a restart, e.g. `ci/trees-restart.py`, switch to a new fork or `--sandbox-read-only`; the implementer lists
each). An end is not reserved space: a full disk that fails an end's step stops the actor, and the next boot's
reconcile ends it.

---

## S5. Fork lifecycle

**Needs:** S3, S4. **Run:** full.
**Builds:** keyed `create`, `end`, `end_keyed`, the drop (`drop_fork` in the actor and the store), `on_last_detached`
ending an unkeyed fork, `settle` after each take and at boot, Clients' `on_fork_dropped`; `--fork-key`, `trees end`
in the trial CLI, the conformance harness and the `--sandbox` scripts (power loss included).

**Owned files.** `crates/agentfs-contracts/src/name.rs` (`ForkKey::parse`); `crates/agentfs-trees/src/service.rs`;
`crates/agentfs-clients/src/service.rs` (`on_fork_dropped`); `crates/agentfs-core/src/store.rs` (`end`,
`drop_fork`, `release_claim`); `crates/agentfs-core/src/tree_actor/{mod.rs,history/**}` (`drop_fork`);
`crates/agentfs-fuse/src/tree_view.rs` (the after-take hook); `crates/agentfs/src/trees_cmd.rs`;
`crates/agentfs-conformance/src/mounted.rs`; every `ci/` script using `--sandbox`, `--fork` or `trees fork`;
`docs/configuration.md`, `docs/trees.md`, `docs/target-architecture/domains/trees.md` (deleted if this lands its
last part: see "Docs" below).

**Tree actor.**

```rust
impl TreeActor {
    /// One step: `fork`'s branch, object table and queue entries (`refs/queue/<fork>/`) deleted with `rows`;
    /// its working tree removed after the commit point (a copy). Its collision refs stay.
    /// `Error::Busy` while `pending_from(fork)`, a handle is open in it, or `has_child(fork)`; `NotFound` for the root.
    pub fn drop_fork<F>(&mut self, fork: TreeId, by: &WriterAt, rows: F) -> Result<()>
    where F: FnOnce(StepNumber) -> Result<Vec<RowWrite>>;
}
```

Recovery accepts a journal whose upstream map lost a fork with its branch (today's `check_recovered` refuses an
upstream entry with no branch; the drop removes both in one step).

**Trees.** `create(_, Some(key), ..)`: an `Open` fork under the key → that fork, nothing written; an `Ended` or
`Dropped` one → `TreeError::Ended`; none → create with the key in its row. `end(f)`: `ForkUnknown`; `NotPermitted`
unless `by.may() == Root` or `created_by == by.launcher()`; a keyed fork → `NotPermitted` (keyed forks end only by
key); `Open` → `store.end` + `ForkEnded`, then `settle`; already ended: `Ok`. `end_keyed(key)`: `KeyUnknown`; else as
`end`, for any launcher. `on_last_detached(tree)`: an `Open` unkeyed fork → end as `Principal::Agentfs`; the root, a
keyed fork: nothing. `settle()`: every `Ended` fork with `!pending_from && !has_child` → `store.drop_fork` +
`ForkDropped`; answers the dropped.

**Composition root (events).** `LastDetached{t}` → `trees.on_last_detached(t)`; `ForkDropped{f}` →
`clients.on_fork_dropped(f)`; after each take (the view host's progress worker calls an after-take hook,
`ViewHost::new(.., after_take: Box<dyn FnMut() + Send>)`, with the actor unlocked) → `trees.settle()`; at boot, after
reconcile → `trees.settle()`.

**The trial CLI.**

| Was | Now |
|---|---|
| `trees fork` | removed |
| `trees mount ROOT --sandbox DIR [--fork fork/N]` | `trees mount ROOT [--fork-mount DIR [--fork-key K] [--fork-writer LABEL \| --fork-read-only]]`: creates the keyed fork or returns it; without `--fork-key`, a new unkeyed fork that ends at its last detach |
| `--sandbox-writer`, `--sandbox-read-only` (S4) | `--fork-writer`, `--fork-read-only` |
| — | `trees end --fork-key K` (`PodArgs`): `end_keyed`; needs the state area's lock (`Busy`, exit 75, while a mount runs) |

stderr keeps `fork tree: fork/<n>` (was `sandbox tree:`) for scripts that read it.

**Acceptance tests.**

| Test | Shows |
|---|---|
| `a_second_create_under_a_key_returns_its_fork` (trees unit and core) | idempotent per key |
| `an_ended_fork_drops_only_after_its_landing_is_taken` (core: fork saves, ends, its landing queued: `settle` drops nothing; take; `settle` drops it; branch gone, `refs/collisions/...` of a losing save kept, `refs/agentfs/trees/fork/<n>` at `ForkDropped`) | the drop rule |
| `last_detached_ends_an_unkeyed_fork_not_a_keyed_one` | trees.md §2 |
| `a_dropped_forks_claim_is_released` | C4's end |
| `a_drop_and_its_row_are_one_step` (failpoints as S3) | one commit point |
| the mounted conformance replay and power-loss-trees on `--fork-mount` | the harnesses moved |

**Mutation probes.** Ignore the key (always create) → `a_second_create_under_a_key_returns_its_fork`; drop before
the queue empties (skip `pending_from`) → `an_ended_fork_drops_only_after_its_landing_is_taken`.

**Docs.** configuration.md rows; docs/trees.md (forks: keys, end, drop); docs/target-architecture/domains/trees.md:
M1 builds create/end/drop but not pinned snapshots, storage or quotas, so the page stays with those parts only; the
built parts move to docs/trees.md in this commit.

---

## S6. Volume lifecycle

**Needs:** S3, S4 (S5 for the fork reactions on the drain path). **Run:** core.
**Builds:** `stage`, `unstage`, the Stop drain, `decide` and `verdict`, `rejudge`, `VolumeStore` and `QueueFacts` on
the state area; settlement bound to `NoRemotes` until M4; SIGTERM unstages; Clients' `on_volume_draining` /
`on_volume_staged`.

**Owned files.** `crates/agentfs-volume/src/{release.rs,service.rs}`; `crates/agentfs-core/src/{state_area.rs,store.rs}`
(`VolumeStore`); `crates/agentfs-clients/src/service.rs` (the two consumers); `crates/agentfs/src/trees_cmd.rs`
(stage, SIGTERM through `signal-hook`, exit); `docs/configuration.md`, `docs/trees.md`,
`docs/target-architecture/domains/volume.md` (keeps the unbuilt parts: start lines, the upgrade drain,
`on_release`), `CHANGELOG.md`.

**Volume.** `stage(at, by)`: `NotPermitted` unless `Root`; `area.open(at)` (`NotCreated`, `OtherFormat`, `Busy`);
from the row (`None` = `Released(Keep)`): `Released` or `Draining(Stop)` → record `Staged` + `VolumeStaged`;
`Staged` (a crash while staged) → stays, nothing written. `unstage(by)`: `Staged` → record `Draining(Stop)` +
`VolumeDraining`, then `rejudge`; `Draining`/`Released` → unchanged, `Ok`. `rejudge()`: only while `Draining`; facts
= `ReleaseFacts { state, attached: clients.attached_any(), queued: area.queued(), settlement: NoRemotes }`;
`decide` → `Cleared` → record `Released(cleared.disposition())` + `VolumeReleased`. `release_verdict()` =
`verdict(&decide(&facts))`. `decide` returns blockers in `Blocker`'s order: `Staged`, `Attached`, `QueuesNotEmpty`.

The state area adapter (`TreeStateArea`) implements `VolumeStore` and `QueueFacts` by delegating to the `TreeStore`
it makes at `open` (before: `load` → `Ok(None)`, `record` → `Err(Stopped)`, `queued` → empty).

**Composition root.** `C` for `VolumeService` is a binary-local handle, `ClientsHandle(Rc<RefCell<Option<ClientsService<..>>>>)`,
implementing `AttachedLookup` (empty set while `None`); it is filled after `stage` by `ClientsService::restore`.
Events: `VolumeDraining` → `clients.on_volume_draining()`, then the CLI detaches its clients (each `AttachmentEnded`
→ `actor.close_writer`, so open files are saved) and closes the views (lazy unmount); `VolumeStaged` →
`clients.on_volume_staged()`; `AttachmentEnded`, `LastDetached`, each take → `volume.rejudge()`; `VolumeReleased`
→ the mount process exits 0. Until `Released` the progress worker keeps taking.

`trees mount`: `stage` first (replacing S1's direct open); on SIGTERM or SIGINT: `unstage`.

**Acceptance tests.**

| Test | Shows |
|---|---|
| `decide_over_every_combination` (volume unit: each state × attached empty/not × queued empty/not; exact blocker lists) | VO1, VO4 |
| `sigterm_with_a_file_open_saves_and_releases` (CLI test needing FUSE, so in `ci/e2e-trees.sh`: a 4 KiB file held open, SIGTERM: its bytes in the root's history, `refs/agentfs/volume` at `VolumeReleased`, exit 0) | the Stop drain end to end |
| `stage_during_a_stop_drain_cancels_it` (volume unit over fakes) | volume.md §5 |
| `released_needs_no_client_attached` (volume unit) | VO1 |
| `a_lifecycle_move_and_its_row_are_one_step` (core, failpoints as S3) | one commit point |

**Mutation probes.** `decide` skips `QueuesNotEmpty` → `decide_over_every_combination`; release with a client
attached (skip `Attached`) → `released_needs_no_client_attached` and the SIGTERM test.

**Docs.** configuration.md (SIGTERM behaviour of `trees mount`); docs/trees.md (lifecycle, release);
CHANGELOG Unreleased. After S6 the design's M1 slices are all built: `docs/target-architecture/system.md` and the
four domain pages lose their built parts (moved to library and human docs) in this commit; `m1-design.md`'s plan
rows in `future/one-engine.md` M1 move to "built".

---

## Slice order and why

```text
S1 (tracer) ──▶ S2 (figures)
     │
     └────────▶ S3 (store + Trees create) ──▶ S4 (Clients, sessions, views) ──▶ S5 (fork lifecycle)
                                         └──────────────────────────────────▶ S6 (volume lifecycle; after S5)
```

S1 first: the riskiest seam (identity before any write, the pod context through recovery, the landing writer) and
it changes the journal and message format every later slice writes. S2 needs only S1's tracer and may run beside S3
(disjoint files: `ci/` and `image.yml` against Rust). S3 before S4: Clients' rows and the session's writes go through
the store's step. S4 before S5: a fork's end reacts to `LastDetached`. S6 last: its drain composes every other
domain's reaction. Package depth: contracts (committed) → domain crates → agentfs-core adapters → fuse → binary,
within each slice.

## Test architecture

| Module | Tested in isolation by | Conformance boundary |
|---|---|---|
| `agentfs-contracts` | unit tests of each `parse` and text round trip | none (values) |
| `agentfs-volume`, `-clients`, `-trees` | unit tests over in-memory fakes of their ports (a `Vec`-backed store, a fixed `ForkLookup`, a recording `EventSink`) in each crate's `#[cfg(test)]`; no git, no FUSE | the ports' contracts in `agentfs-contracts::ports` doc comments |
| `agentfs-core` `state_area`, `store`, `session` | integration tests on real state areas (`tests/state_area.rs`, `tree_store.rs`, `tree_clients.rs`), failpoints for one-step claims | the tree actor's step and journal (power-loss replay) |
| the tree actor | as today (`tests/tree_actor.rs`, unit tests), plus the message literals | the model, through conformance's library and mounted replays (unchanged digests) |
| `tree_view` (`ViewHost`) | `session_config` unit test; FUSE behaviour in `ci/e2e-trees*.sh` | the mounted replay |
| the binary | CLI parse tests; the tracer job | the tracer |

## Maintenance risks

- **Row chains grow without bound** (an attach and a detach per mount; one chain per fork, kept after the drop).
  Cheap at M1's scale (tens of rows per volume); a volume with thousands of short forks accumulates refs. Packing
  refs is git's own answer; nothing else is planned.
- **`Audited<T>` fields are public**: only convention keeps stamping in `store.rs`. S3's probe guards the one adapter;
  a second adapter would need its own.
- **The composition root grows a small event loop** (S5, S6). Keep it a dispatcher: every decision stays in a domain.
- **`todo!()` bodies** in committed services panic if called early; each slice's tests call only what it built, and
  `#[expect(dead_code)]` on the service structs fails the build once the fields are read, forcing its removal.
