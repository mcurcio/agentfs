# One engine: the tree views run the sidecar, CSI and CLI

**Proposed, not built.** This page plans how agentfs goes from two engines to one: the tree views take over every
launcher, and the recording engine is deleted. It is the system-level plan; each milestone below starts with its
own architect and design gate, one layer down. The commit that lands the last milestone (M9) deletes this page and
moves the behaviour into the library and operator docs.

## Today

Two engines ship. Every consumer runs the recording engine; the tree views are a trial that no consumer reaches.

```text
 sidecar (deploy/sidecar, chart)  ─┐  agentfs init + mount     ┌─ agentfs trees init | fork | mount   (trial CLI,
 Docker examples                  ─┤  (one launcher: the      │    hand-run, flat files, one host, no remote)
 CLI: push, patch, restore ...    ─┘   agentfs binary)        │            │
 CSI driver (agentfs-csi)  ──────────▶ RECORDING ENGINE        └─▶ TREE ENGINE  (git model, conformance, model)
   (spawns the same binary: `mount --history-service`, `serve-history`)
```

The end state: one engine, the tree views, behind every launcher. Nobody runs `agentfs trees init` by hand: each
launcher builds a volume's spec from its configuration. The owner's guidance behind this plan is in
`OWNER-GUIDANCE.md` ("One engine").

## Goals and the step that delivers each

Every end goal has a delivering step. Steps 6, 5b, 5 and 7 are the [module cut](module-cut.md)'s; M1 to M9 are this
page's; "speed" is [save and take speed](save-and-take-speed.md).

| End goal | Delivered by |
|---|---|
| A baseline build verified with a target application | [the baseline](#the-baseline), the plan's first acceptance point: M1 to M7, with M6 |
| One engine | M1 (volumes and their lifecycle), M7 and M8 (the launchers move), M9 (the recording engine is deleted) |
| Nobody runs `trees init` by hand | M1 (`ensure` takes a `VolumeSpec` value; the trial CLI builds it from `init`'s flags), M7 and M8 (each launcher calls `ensure`; `agentfs trees` folds into `init` and `mount`) |
| One volume that root and non-root processes both use (the problem ArtifactFS stalled on) | M1 (other users reach the tree mount; today it admits only its owner); M2 (modes and owners decide what each may do) |
| The launchers' speed holds | M1 (a release build on a many-file workload sets tree-view budgets); speed (before M2, below); M7's gate |
| History knows the root by identity; no hardcoded `main` | module cut 5b |
| Consumers configure the name the root is published under, and may change it | [Root branch name](root-branch-name.md), built in M4 (the publisher and its key) |
| The trailer records meaningful information | 5b (which tree, by identity); M1 (where: the volume and the pod) |
| Configuration: iac and the agent both edit it (`AGENTS.md`, "What agentfs is for") | M6; until it lands, a configured patch source raises a standing alarm (M7) |
| Audit; recording is the baseline (principle 1) | built (a write is journaled before it returns; power loss replayed by `phase-1.yml`); M1 (where); M2 (every kind of change recorded or refused loudly); M3 (only operator rules skip a path) |
| Invisible and untouchable (principle 2) | built (state outside the mount); M3 (the untracked area); M4 (credentials never in a view) |
| Git idioms (principle 3) | M1 (operator files as a git-config include); M3 (`info/exclude`, `info/attributes`); M4 (the published name's key) |
| The right technology, recorded (principle 4) | M4 (the reused transports in architecture.md's "Technology choices"); M9 |
| Keep operating (principle 5) | M4 (retry, divergence saved), M5 (alarms, liveness, restart in place, each of today's alerts and runbooks ported or dropped) |
| Any remote, or none (principle 6) | module cut 6 (the mirror leaves the model's world); M1 (zero remotes is a valid volume); M4 |
| History is the product (principle 7) | 5b (identity); M1 (where); M4 (never forced; a divergence is saved); M7 and M8 (old state areas archived, never deleted) |
| Recovery is manual but supported (principle 8); merging is an extra (principle 9) | M6 (restore, revert; patches at mount only, and `serve-git` as a patch source); M6 lands before the baseline |
| Three layers; OpenTelemetry for everything observable (principle 10) | each milestone's library page; M5 (telemetry, and the recorder's engine source) |
| The model stays the baseline proof | module cut 6 and 7; from M1, the tree engine's core tests on every PR ([CI tiers](#ci-tiers)) |
| Old state formats are not supported | 5b (tree format 3); M7 and M8 (a recording state area is refused at boot) |
| Extras that ship today (LFS, forks of forks, several pods on one volume, moving a volume, the fork overlay, `fork deliver`, unlinked forks and the rest) | Deferred: each is recorded with its home in [deferred extras](#deferred-extras), dropped at M9 until its slice lands |

## The shape

The launchers compose the domains of [the system](../system.md). None of them is built: M1 builds the four a
launcher composes (Volume, Clients, Views, Trees), and the rest arrive where they are needed (Rules in M3,
Replication in M4, Health and Telemetry in M5). The recording engine's own concepts (frozen copies and `pending/`,
the front, history service v2, forked volumes, checkpoints, the fsync class, the overlay's tmp layer) leave, and each
returns only as a slice of its own ([deferred extras](#deferred-extras)).

**One volume lifecycle for every launcher.** The sidecar, Docker, the CLI and CSI each build a `VolumeSpec` from
configuration and call the same idempotent `ensure`, then `stage` ([Volume](../domains/volume.md)). Release is judged
only by Volume. Operator settings (rules, remotes, the published name, what happens on release) stay out of the
spec: iac places them as a git-config include, and each owner reads its own at boot, so an iac edit is never a spec
mismatch.

```rust
pub struct VolumeSpec { name: VolumeName, start: StartPoint }   // no id: `ensure` assigns it

pub trait Volume {
    /// Idempotent. Acts on the state area at the volume's location (a path, or CSI's volume handle).
    /// None there: create, with a new volume id. Same name and start: recover. Another name or start:
    /// `SpecMismatch`, before any write.
    fn ensure(&mut self, spec: VolumeSpec, by: &LaunchCap) -> Result<Ensured, VolumeError>;
    // stage, unstage, release_verdict: as on the Volume page
}
pub struct Ensured { pub id: VolumeId, pub state: VolumeState }   // the id exists only once `ensure` has run
```

**Identities and names.**

| Thing | Identity (what history records) | Name (configuration at the edge) |
|---|---|---|
| volume | a UUID assigned at create | its `VolumeName`, from the launcher's configuration |
| tree | `root`, or `fork/<n>`, a short number within its volume (5b) | the root's published name, the publisher's setting (M4), changeable at any boot |

The UUID is not in `VolumeSpec`: `ensure` assigns it at create and stores it in the state area. A launcher that had
to carry it would need state of its own before the volume exists, and a first boot could only mismatch.

A volume's state is found by its location, not its name: the sidecar's and Docker's state path, or the CSI volume's
handle (today's `agentfs init` and CSI CreateVolume work the same way). The name is a label stored with the volume,
and a guard: a repeated `ensure` with another name at the same location is refused, as `agentfs init` refuses it
today, because a launcher whose configured name changed while it points at the same state is more likely
misconfigured than renamed. Whether the name may later change depends on M4's prefix question: under a prefix built
from the name, a rename moves the volume's published refs; under one built from the UUID, it moves nothing. M4's
gate settles both. History does not depend on the name: the trailer records it beside the UUID, which identifies
the volume.

Each volume publishes its refs under a prefix of its own (M4), so several volumes share one repository without their
`root` and `fork/<n>` refs meeting. Its form (the name, the UUID or both) is M4's question.

**The trailer.** Every tree-view commit records which tree (its identity, 5b), the writer and the cause, as
[agentfs in git terms](../git-model.md) §5 has them, and never the published name. From M1 it also records where
the commit was made, as trailers: the volume's name and UUID, and the namespace, pod, pod UID and node the launcher
runs in (the recording engine's `Agentfs-Namespace`, `Agentfs-Pod`, `Agentfs-Pod-Uid` and `Agentfs-Node`). Pod and
node names reach every remote, as they do today.

## Forks and landing approval

Whether a fork's change needs an approval before it lands in its source is a setting of that fork, as
[landing approvals](landing-approvals.md) has it; a fork configured to need approval cannot be created until that
slice is built. Landing is automated by default: a fork with no approval setting lands in its source as it saves,
as the tree views do today, and approval is opt-in per fork. Trees holds each fork's settings and is the one place
that refuses ([Trees](../domains/trees.md)); a launcher only passes them on. M8's architect picks the CSI class's
key for the setting. Today's CSI fork classes have no such key, so on the tree views their forks land automatically
unless a class is configured to need approval; the receiver's acceptance rule of today (`agentfs.patchAccept`) no
longer gates them. A class with `link: unlinked` makes an unlinked fork, which is
[deferred](#deferred-extras): M8 refuses such a claim, with a clear error, until its slice lands.

A CSI fork claim's fork has one lifecycle owner, Trees: CreateVolume creates it with the claim's handle as an
idempotency key, so a repeated CreateVolume finds the same fork, and DeleteVolume ends it by that key. The launcher
keeps no map of its own. This is a domain change proposed for the owner's review ([Trees](../domains/trees.md)).

The user-facing word for these is "fork". M7 renames "sandbox" in the binary (the trial CLI's `--sandbox`) and M8 in
the CSI driver, its chart and [supported configurations](../../supported-configurations.md) (example 5's "sandbox
claim" becomes a fork claim). Each milestone's gate decides the new spellings.

## Milestones

Each is its own PR series from `main`, gated by an architect, an implementer, one review, one fix round, and green
ci and image runs, plus what its row says. A "short run" of `phase-1.yml` is its core tests, without the 500-seed
shards; from M1 it runs on every PR and push to `main` ([CI tiers](#ci-tiers)). A "full run" adds the shards, on a
manual trigger. Only a milestone that changes save or land logic needs the full run.

**M1. Volumes and their lifecycle.** The four domains a launcher composes are built: Volume (`ensure`, `stage`,
`unstage`, the drain and the release verdict), Clients (attach and detach), Views (view hosts) and Trees (the fork
lifecycle: create with settings and an idempotency key, end, drop). `ensure` takes a `VolumeSpec` value; the trial
`trees` CLI builds it from the same flags `agentfs init` takes, so no new configuration format appears. A volume
gets its UUID at create; zero remotes is a valid volume; its start point is empty (a start from another
volume's branch is [deferred](#deferred-extras)). Other users reach the tree mount, as they reach the
recording mount today; the tracer's workload runs as an unprivileged user. Tree-view commits gain the "where"
trailers, written through module cut step 5's one message function. A release build on a many-file workload, in
the shape of the recording engine's end-to-end budgets (`ci/bench-budget.json`: many files created, a large file
closed, a clone), gives the first release figures and sets tree-view budgets. A tracer CI job starts (below), and
`phase-1.yml` gains its PR and `main` triggers ([CI tiers](#ci-tiers)). Its
gate cuts M1 into PR series, and decides where the spec and the settings are stored and audited, the trailer keys,
how a launcher passes its pod context, and the budgets. Gate: a full run per PR series; the tracer.

**Speed.** [Save and take speed](save-and-take-speed.md) comes before M2 when M1's release figures miss the
budgets, and debug-build figures say they will (about 96 ms a save against the recording engine's 6 ms a file;
[performance](../../performance.md)). It comes first because M2, M3 and M4 extend the save and take path it
rewrites. Gate: a full run; the budgets met.

**M2. Beyond flat files.** Directories, symlinks, non-UTF-8 names and more links
([beyond flat files](tree-views-beyond-flat-files.md)), and file metadata: modes, the presented owner and times, in
place of today's `0600`, the daemon's uid and epoch times; permission checks follow them, so each user may do what
the modes allow. Its architect writes the missing file-metadata page. The model gains rules, so it runs the full
model gate. Gate: the full model gate; a full run.

**M3. Rules and the untracked area.** Operator rules decide what is recorded ([rules](../domains/rules.md),
git-model §3), and unrecorded files live in the untracked area. Its gate decides whether untracked files get
ordinary Linux open semantics and what a fork sees of its source's. Gate: a full run.

**M4. Mirrors and the published name.** Mirrors on the tree model ([replication](../domains/replication.md)), the
publisher, each volume's refs under its own prefix, and history a mirror's reader can read
([line map and renames](line-map-and-renames.md)). The SSH and HTTP transports, credentials and the fast-forward
claim are reused, re-homed under Replication, and recorded in architecture.md's "Technology choices". Its gate
decides the published name's key, what a rename does to the old remote ref, the prefix's form, and whether a
volume's name may change ([root branch name](root-branch-name.md), which M4 deletes; git-model's
`refs/agentfs/<volume>/` stands for that prefix until then). Gate: a short run.

**M5. Keep operating.** Telemetry, alarms, liveness and restart in place
([tree-view telemetry](tree-view-telemetry.md), [health](../domains/health.md)). Its gate lists each of today's 12
alert rules (`deploy/alerts/agentfs.rules.yml`: `AgentfsRecordFailed`, `AgentfsCommitsFailing`,
`AgentfsRepackFailed`, `AgentfsReconcileFailed`, `AgentfsHistoryServiceUnreachable`, `AgentfsPushBacklogOld`,
`AgentfsPushBacklogStale`, `AgentfsPushDiverged`, `AgentfsRemoteMisconfigured`, `AgentfsPatchQueueBlocked`,
`AgentfsWorktreeHeuristic`, `AgentfsTelemetryExportFailing`) and 17 runbooks (`docs/runbooks/`: crashed sidecar,
CSI recovery, divergence, fork refused, history rewrite, history service down, LFS storage, moving, patch queue,
reconcile failed, recording failures, recovery and undo, sandbox overlay, start branch moved, stuck pushes,
upgrading the sidecar, worktree heuristic) as ported or dropped, with the reason; a ported alert fires against the
tree engine's own metrics in the alerts workflow's real Prometheus. A second PR series, which nothing later waits
for, builds the recorder's engine source: agentfs-core's OpenTelemetry events feed the model's recorder
(`calls::Sink`) through an adapter in agentfs-conformance, with content hashes, not bytes. The export never blocks a
save and so may drop data; the recorder needs a lossless, unsampled, in-process listener beside it. Squaring the two
is that series' first design question. Gate: a short run; a regression found on a mount replays in the model from
the engine's own events.

**M6. Patches, restore and revert on trees.** iac's patches applied at mount, `serve-git` as a patch source, and
`restore` and `revert` on trees ([integrator patches](integrator-patches.md)). It needs M4's remotes and nothing
later: it may land before or after M7, and lands before the baseline, whose every Hermes job lands a queued patch.
Its gate names its commands, among them the producer's `patch submit`, so that no command serves both engines.
Gate: a full run.

**M7. The `agentfs` binary moves.** The sidecar, the Docker examples and the CLI share one launcher, the binary's
`init` and `mount`, so they move together. `agentfs trees` folds into `init` and `mount` and goes. The CSI driver
spawns the same binary (`mount --history-service` and `serve-history`), so M7 cuts by entry point: the recording
engine's two CSI entry points move to subcommands of their own, which only the CSI driver calls, and M7 edits the
CSI launcher's call sites to match. Each entry point runs exactly one engine, and no flag selects one:

| Entry point | Before M7 | M7 to M8 | M8 to M9 |
|---|---|---|---|
| `init`, `mount` | recording | tree | tree |
| the CSI front (today `mount --history-service`) and `serve-history` | recording | recording, under their own names | not called; deleted at M9 |
| the recording one-shot commands (`record`, `flush`, `push`, `checkpoint`, `run`, `patch`, `fork`, `lfs`, `start accept`, `restore`, `revert`) | recording | recording, on a recording state area only | the same; deleted at M9 |

The `ci/` scripts that call `init` or `mount` (about two dozen) are repointed or deleted in M7. Until M6 lands, a moved
launcher whose configuration names a patch source (`agentfs.patchRemote` or any other `agentfs.patch*` key) starts
and keeps the filesystem working (principle 5), logs the gap at every boot, and raises a standing alarm while it
runs: patches are never silently not applied. M7 adds the migration runbook (below). Gate: the tracer's
release-build many-file step within M1's tree budgets, run in CI before the merge (`bench.yml` runs only after
landing and never fails, so it cannot hold the move); a miss holds the move. M7 also rewrites the migration Job of
[deployment](../../deployment.md#10-hermes) (today `record` and `push`) to copy the files in through the mount.

### The baseline

The plan's first acceptance point: a baseline build, verified integrated with a target application, before anything
else is moved or deleted. The applications are the ones agentfs already tests end to end: the generic sidecar
example (`deploy/sidecar`) and upstream Hermes ([deployment](../../deployment.md#10-hermes)), each on the tree
engine. The baseline builds nothing of its own. It is reached when M6 and M7 have both landed and its gate passes;
a failure is fixed in the milestone that owns the gap, before M8 starts.

What those applications need, and the milestone that gives it:

| Need | Milestone |
|---|---|
| A volume created, or recovered, from the launcher's configuration | M1 (`ensure`), M7 (the sidecar and Docker launchers call it) |
| The mount at the application's data path, used by root, by other users and by a docker-in-docker tools container | M1 (other users reach the mount), M2 (directories, symlinks, modes and owners) |
| Rule files: what is recorded, and untracked paths (secrets, databases, sessions, logs) never in history | M3 |
| Every change pushed to a remote | M4 |
| Health and alarms, and a sidecar-only restart onto a new mount | M5's first series |
| iac's patches landing at the next start | M6 |
| The start-up speed the probes allow | M1's budgets; speed, if M1's figures miss them |

Only these come before it. M5's recorder series, M8, M9 and every [deferred extra](#deferred-extras) come after,
or never block it.

Gate: these CI jobs green with the launchers on the tree engine: image's `e2e (kind cluster)` (`ci/e2e-kind.sh`,
the generic sidecar), hermes' `e2e (Hermes on kind, sidecar)` and `e2e (Hermes on kind, fence)`
(`ci/e2e-hermes-kind.sh`), and docker's `e2e (docker compose, upstream Hermes)` (`ci/e2e-hermes.sh`). Each lands a
queued patch, records a tool container's edit and keeps secrets and databases out of history. Hermes' third job,
`e2e (Hermes on kind, csi)`, runs on the CSI driver and moves at M8.

**M8. The CSI driver moves.** CreateVolume calls `ensure`; publish attaches a view host; DeleteVolume asks Volume's
release verdict, or for a fork claim ends its fork by key. A fork claim makes an in-volume fork, owned by Trees
under the claim's handle (above); it lands automatically unless its class needs approval, and a fork whose settings
need approval is refused by Trees until landing approvals is built. Its architect writes the missing CSI-on-trees
page for one pod per volume; several pods on one volume, moving a volume between nodes and the other CSI extras are
[deferred](#deferred-extras), and M8 refuses a claim that asks for one (a second pod's publish, `startFrom`,
`link: unlinked`) with a clear error. Existing recording-format volumes move by the runbook below; a
recording-format volume is never staged, and its DeleteVolume keeps its directory, with an alarm naming it. Gate:
as M7, and hermes' `csi` job green on the tree engine.

**M9. The recording engine is deleted.** Its domain logic and the passthrough FUSE layer go, with its patch queue and
`serve-git` (rebuilt on trees by M6), its CSI entry points and one-shot commands, and every
[deferred extra](#deferred-extras) it still serves. Adapters the tree engine uses stay where M4 and M5 re-homed
them. Recording-only docs, runbooks, alert rules and CI jobs go, as M5's list and the deferred table say. This page
goes; its deferred table moves to [the target architecture's index](../README.md) first. Gate: ci and image.

## Order

```text
 6 → 5b → 5.1 → 5.2 → 5.3 → M1 → speed → M2 → M3 → M4 → M5 → M7 ─┬─▶ BASELINE ─┬─▶ M8 → M9 → deferred extras
                                                        │    │        │             └─▶ 7 (the model's checker)
                                                        │    └─▶ M5's recorder series (off the path)
                                                        └─▶ M6 ───────┘
                                           (M6: any time after M4, before the baseline)
```

The order serves the baseline first: every milestone before it is one a target application needs, and nothing
after it is.

- Step 6 is built before the plan's first milestone: its first slice moves the mirror out of `World`, which M4's
  publisher is checked against, and its transaction rules ride along because they were already in flight.
- Step 5b precedes M1 because M1's "where" trailers and M4's publisher both name trees by identity, which 5b gives.
- Step 7 (the model's checker reads git) is not on the baseline's path: the checker judges M2's new model rules as it
  is today, reading some facts through `World`. It runs after the baseline, beside M8.
- Step 5 precedes M1 because M2, M3, M4 and M6 each extend a module step 5 makes.
- Each serial edge has a reason. Speed precedes M2 because M2 to M4 extend the path it rewrites. M2 precedes M3
  because rules classify directories and symlinks, which M2 adds. M3 precedes M4 because a mirror publishes what
  rules record, and a path published before rules exclude it stays in history, which is never rewritten
  (principle 7). M4 precedes M5 because the push and divergence alarms need the publisher. M5 precedes M7 because a
  moved launcher without alarms or liveness would not keep operating (principle 5); only M5's first series is on the
  path.
- **Goals may lapse during the move.** agentfs is before its 1.0 release, so a launcher moves without waiting for
  every goal its users rely on. M6 is off M7's path: from M7 until M6 lands, a moved launcher applies no iac patches
  at mount, never silently (M7's alarm), and has no `restore` or `revert`, and example 3 of
  [supported configurations](../../supported-configurations.md) is marked "not built". M6 must land before the
  baseline, whose Hermes jobs land a patch. Each [deferred extra](#deferred-extras) is marked "not built" in
  supported configurations from its launcher's move (M7 or M8) until its slice lands.
- **Links to deleted pages.** The commit that deletes a plan page repoints every link to it: step 7's commit,
  which deletes the module cut, rewrites the references to its steps here and in
  [root branch name](root-branch-name.md) as the built behaviour's library pages; M4's commit, which deletes root
  branch name, does the same for its links here, in the domain pages and in the baseline pages; M9's, which deletes
  this page, for every link to it. The docs workflow's link check fails otherwise.

## Deferred extras

Every capability the recording engine ships that no milestone above rebuilds. None is needed for
[the baseline](#the-baseline). Each goes dark when its launcher moves (M7 for the binary and the Docker examples,
M8 for the CSI driver), is marked "not built" in [supported configurations](../../supported-configurations.md) from
then, and is deleted with the recording engine at M9. Each returns as a slice of its own, from its home below; M9's
commit moves this table to [the target architecture's index](../README.md), so no row is lost with this page.

| Capability | What it is | Who uses it today | Home | Until then |
|---|---|---|---|---|
| Git LFS | content over a threshold stored as LFS objects, uploaded beside each push | example 11; ci's `e2e-lfs` and `e2e-lfs-ssh` | [Large and binary files](large-and-binary-files.md) | dropped at M9 until its slice lands |
| Forks of forks | a fork of a linked fork, whose change rolls up through its parent | example 6; f8's `e2e-csi-fork-of-fork` | [Nested forks](nested-forks.md) | dropped at M9 until its slice lands |
| Several pods on one volume | one history service per CSI volume, one front per pod | example 4; ci's `e2e-multipod` | [The system](../system.md) (the "history service with view hosts" composition), [Views](../domains/views.md) | dropped at M9 until its slice lands; M8 refuses a second pod |
| Moving a volume between nodes | a new volume on another node, started from the old one's branch, by the moving runbook | example 10; f8's `e2e-csi-move` | [Volume](../domains/volume.md) (the start point) | dropped at M9 until its slice lands |
| `startFrom` and `start accept` | a CSI start class: a volume whose history begins at another volume's commit; `StartBranchMoved` while that branch moves | example 10; f8's `e2e-csi-move` | [Volume](../domains/volume.md) (`StartPoint::Line`, the start record) | dropped at M9 until its slice lands; M1 builds only the empty start, and M8 refuses a start class |
| The fork overlay | `mount --sandbox-overlay`: a second mount that shows a fork's workload only recordable files and keeps the rest in a private tmp layer | example 12; `crates/agentfs/tests/mount.rs`, the sidecar chart's overlay values | [The system](../system.md) (the overlay becomes a fork), [Overlay gaps](sandbox-overlay-gaps.md) | dropped at M9 until its slice lands |
| `fork deliver` | a finished fork's change handed to a receiver as an inbox patch, a branch, a command or a webhook | example 8; `crates/agentfs/tests/cli_fork_deliver.rs` | [Fork delivery](fork-delivery.md) | dropped at M9 until its slice lands |
| Unlinked forks | a fork that never delivers to its source and pushes only to its own remotes | example 5's `link: unlinked` class; `crates/agentfs/tests/cli_fork.rs`, `crates/agentfs-csi/tests/grpc.rs` | [Fork delivery](fork-delivery.md) | dropped at M9 until its slice lands; M8 refuses an unlinked class |
| Recording without a mount | `agentfs run` (the commit and push loops with no mount), `record` and `flush` (named paths of the backing tree) | the Hermes migration Job; ci's `e2e-remotes`, `e2e-ssh`, `e2e-telemetry`; alerts' `ci/e2e-alerts.sh` | [Recording without a mount](recording-without-a-mount.md) | dropped at M9 until its slice lands; M7 rewrites the migration Job and repoints the scripts |
| Checkpoint and the change line | an integrator cuts one commit per change intent on `refs/agentfs/<volume>/change-line`; a remote may get only that line | `crates/agentfs/tests/cli_checkpoint.rs` | [Checkpoints and patch notices](integrator-hooks.md) | dropped at M9 until its slice lands |
| Patch notices | the `patch-notify` hook and the `agentfs.patchNotifyUrl` webhook, sent when the queue's verdicts change | ci's `e2e-patches` | [Checkpoints and patch notices](integrator-hooks.md) | dropped at M9 until its slice lands |
| Database groups | `agentfs.commit=fsync` and `agentfs.group`: a database and its journal recorded whole at `fsync` | ci's `fuse` job (`ci/e2e-sqlite.sh`) | [Open files and groups](files-open-groups.md) | dropped at M9 until its slice lands |
| State areas on NFS | a CLI or sidecar state area on an NFS 4.1 export, its lock reaching other clients | ci's `e2e-nfs` | [State on NFS](state-on-nfs.md) | dropped at M9 until its slice lands |
| Hidden fork history | `agentfs.forkRefs = hidden`: a fork's history outside `refs/heads/`; whether a forge runs CI on it is not verified | supported configurations, "Remote topology" | [Forge CI on hidden refs](forge-ci-on-hidden-refs.md) | dropped at M9 until its slice lands |
| A fork of a claim in use | a fork claim whose source a pod is writing; Kubernetes may one day refuse such a clone | image's `e2e-csi-forks` | [Clone in use](clone-in-use.md) | not dropped: M8's fork claims are clones too, so the page stays and applies to them |

Each row's CI job or test goes with the recording engine at M9 and returns with its slice.

## CI tiers

From M1, `phase-1.yml` runs in two tiers:

- **Core, on every PR and every push to `main`:** mounted conformance, power loss (ext4 and XFS), journal and state
  exhaustion, the two-writer page cache and the measurement harness. About 20 min wall and 41 runner-min a run
  (run 43146523186, without the shards).
- **Randomized, on a manual trigger:** the 500-seed replay shards, about 73 min wall and about 300 runner-min with
  the core jobs. They still run in CI, only when someone asks: a dispatch, or `[phase1-seeds]` in the pushed commit
  message. A milestone that changes save or land logic asks for them before it merges (its "full run").

Today `phase-1.yml` runs only on a push to `docs/target-architecture` and on a dispatch, so the tree engine's tests
never run on PRs or `main`. M1 adds the PR and `main` triggers and keeps the shards behind the manual trigger.

## Existing recording volumes

A recording state area is refused at boot by the format check (`FormatsOf`, [the system](../system.md)); there is no
converter. M7 adds the move to the upgrading runbook ([upgrading the sidecar](../../runbooks/upgrading-the-sidecar.md)),
and every step is required:

1. **Push.** With the old binary, push the backlog and check that every remote holds the head. A volume with no
   remote skips this.
2. **Archive the old state area.** Keep it, moved aside or copied out, never deleted. Its git directory stays
   readable with plain git (`git --git-dir <archive>/git log`), also after M9, when agentfs can no longer open it.
   For a volume with no remote, or with history its remotes lack, the archive is the only copy of its audit trail.
3. **Copy the files in.** Create the volume from configuration and copy the files in through its mount, as
   [deployment](../../deployment.md)'s migration Job does, so the copy is the new root's first save.

The old history stays on its remotes under `refs/heads/agentfs/<volume>`. What the new root's first push meets
there depends on M4's prefix. Under a prefix that publishes the root at that same ref, the push is not a
fast-forward: Replication saves it as a divergence and raises an alarm, never overwrites. Under a prefix that nests
refs beneath it (`agentfs/<volume>/…`), git refuses the push as a ref name conflict: an alarmed, retried push
failure, which M4's gate checks. Under a prefix built from the UUID, nothing meets the old ref, and both histories
stand side by side without an alarm. The runbook advises a published name that meets nothing.

**CSI volumes.** Before the driver is upgraded to M8, each recording-format volume is pushed and archived (steps 1
and 2, in its node's directory). After the upgrade, a new claim on a tree-engine class gets the files from the
archive (`git archive` of the old branch's head, unpacked through the new mount). A recording-format volume is never
staged by M8's driver; deleting its claim keeps its directory, whatever `onDelete` says, and raises an alarm naming
it, so the operator archives it before removing it by hand.

## Tracer

From M1, a CI job on the shipped image, in the sidecar's and the Docker example's shape: an init step and a mount
step in an agentfs container, from the flags `init` and `mount` take, and an unprivileged workload container. Until
M7 the two steps are `agentfs trees init` and `agentfs trees mount`; M7 changes only their names.

1. the flags, then `ensure`, then mount; a second `ensure` recovers; another name at the same location is refused;
2. the workload, as another user, writes `a.txt`: saved in the root's history before `close` returns, with its
   "where" trailers;
3. agentfs is killed mid-write and restarted in place: every answered write is saved;
4. a many-file workload on a release build, measured against M1's tree budgets.

Each milestone adds its step: M2 a directory and an executable bit; M3 a database file in the untracked area, never
in history; M4 a refusing remote (the filesystem keeps working), then a push under a configured name; M5 the alarm at
a collector; M6 a producer's patch at the next mount and a `restore`; M7 the same steps through the sidecar on kind,
and the patch-source alarm while M6 is not built; M8 a CSI volume and a fork claim. Not covered: several pods, LFS,
database groups, nested forks, transactions, upgrades.

## Design rules (design choices)

1. **Each entry point runs one engine.** A launcher's move replaces its entry points in one PR; no flag, setting or
   fallback selects an engine.
2. **One volume lifecycle for every launcher.** Every launcher builds a `VolumeSpec` and calls the same `ensure` and
   `stage`; release is judged only by Volume; operator settings are read only by their owners.
3. **Identity in history, names at the edge.** History records the volume's UUID and the tree's identity; a name a
   consumer configures is applied by the publisher and may change without rewriting anything.

## Costs

- Nine milestones and the speed slice after the module cut, touching every crate, doc, chart and workflow. M1 is
  the largest: it builds four domains before any launcher moves.
- Goals and extras go dark for a while: iac's patches (alarmed), `restore` and `revert` from M7 until M6; each
  [deferred extra](#deferred-extras) from its launcher's move until its own slice lands, which no milestone here
  schedules. A user who needs one stays on a release before its launcher's move until then.
- Existing volumes' history does not continue inside new ones. Whether a published name that a recording volume
  used diverges, fails loudly or meets nothing depends on M4's prefix.
- A volume re-created at a location whose state was lost (an `emptyDir`, a deleted Docker volume) gets a new UUID.
  Under a prefix built from the UUID it publishes a parallel history beside the old one, with no alarm.
- Old state areas are archived, not converted: an operator keeps them, and their disk, as long as the audit trail
  is wanted.
- The history format changes at 5b (the root's identity) and again at M1 (the "where" trailers).
- M7 repoints or deletes about two dozen `ci/` scripts and renames the CSI driver's two recording entry points.
- Two pages to write: file metadata (M2) and CSI on trees (M8). M2's new model rules run the costliest gate.
- The baseline's gate adds no job: it is four existing jobs, run against the tree engine.
- **CI.** Wall time and runner time differ, because jobs run side by side. Measured on run 43146523186: a full
  `phase-1.yml` run takes about 73 min wall and about 300 runner-min; without the 500-seed shards, about 20 min wall
  and about 41 runner-min. A full model gate takes about 41 min on a 4-CPU container, outside CI. Measured on the
  2026-11-12 pushes: ci 9 to 13 min and image 5 to 10 min wall, side by side; with helm, f8 and docs, about 25 to
  30 runner-min per pushed commit. Before M1: four full runs (6, 5b, 5.1, 5.3; each moves or changes save and land
  code), about 4.9 h wall and 20 h runner time, and four full model gates (step 6 three, 5b one; about 2.7 h);
  step 7 adds one or two model gates after the baseline. From M1 to M8: full runs where save or land logic changes (expected:
  M1's series, speed, M2, M3, M6) and short runs elsewhere (M4, M5, M7, M8), M2's model gate, the tracer job (not
  yet measured) and both engines' jobs in ci until M9. From M1 the core tier ([CI tiers](#ci-tiers)) adds about
  20 min wall, side by side with ci, and about 41 runner-min to every PR push and every push to `main`; the
  randomized shards cost only when triggered. The counts are estimates.
- Not planned: a converter, an engine flag, mirror-back (pull requests from a forge into the root).

## Prior art

Strangler fig (Fowler) and parallel change (Sato) for moving one launcher at a time; branch by
abstraction rejected, because it keeps a dual path; tracer bullet (Hunt, Thomas); idempotent create (CSI
`CreateVolume`, `git init`); git's `include.path`; entity identity apart from its attributes (Evans, *Domain-Driven
Design*; Kubernetes' `uid` beside `name`); an idempotency key on a create request (CSI's volume name).
