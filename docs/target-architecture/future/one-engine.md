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
| Recovery is manual but supported (principle 8); merging is an extra (principle 9) | M6 (restore, revert; patches at mount only, and `serve-git` as a patch source); M6 lands before M9 |
| Three layers; OpenTelemetry for everything observable (principle 10) | each milestone's library page; M5 (telemetry, and the recorder's engine source) |
| The model stays the baseline proof | module cut 6 and 7 |
| Old state formats are not supported | 5b (tree format 3); M7 and M8 (a recording state area is refused at boot) |
| Extras that ship today: LFS; forks of forks; several pods on one CSI volume; moving a volume between nodes; the fork overlay of example 12; `fork deliver`; unlinked forks | Open: [question 1](#open-questions-for-the-owner) asks which M9 drops and which get a step before M9 |

## The shape

The launchers compose the domains of [the system](../system.md). None of them is built: M1 builds the four a
launcher composes (Volume, Clients, Views, Trees), and the rest arrive where they are needed (Rules in M3,
Replication in M4, Health and Telemetry in M5). The recording engine's own concepts (frozen copies and `pending/`,
the front, history service v2, forked volumes, checkpoints, the fsync class, the overlay's tmp layer) leave, and each
returns only as a slice of its own.

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
slice is built. Every other fork lands in its source as the tree views do today. Trees holds each fork's settings
and is the one place that refuses ([Trees](../domains/trees.md)); a launcher only passes them on. M8's architect
picks the CSI class's key for the setting. The default when the setting is absent, and what today's fork classes
become, are [question 2](#open-questions-for-the-owner).

A CSI fork claim's fork has one lifecycle owner, Trees: CreateVolume creates it with the claim's handle as an
idempotency key, so a repeated CreateVolume finds the same fork, and DeleteVolume ends it by that key. The launcher
keeps no map of its own. This is a domain change proposed for the owner's review ([Trees](../domains/trees.md)).

The user-facing word for these is "fork". M7 renames "sandbox" in the binary (the trial CLI's `--sandbox`) and M8 in
the CSI driver, its chart and [supported configurations](../../supported-configurations.md) (example 5's "sandbox
claim" becomes a fork claim). Each milestone's gate decides the new spellings.

## Milestones

Each is its own PR series from `main`, gated by an architect, an implementer, one review, one fix round, and green
ci and image runs, plus what its row says. A "full run" of `phase-1.yml` includes the 500-seed shards; a "short run"
does not. Only a milestone that changes save or land logic needs the full run.

**M1. Volumes and their lifecycle.** The four domains a launcher composes are built: Volume (`ensure`, `stage`,
`unstage`, the drain and the release verdict), Clients (attach and detach), Views (view hosts) and Trees (the fork
lifecycle: create with settings and an idempotency key, end, drop). `ensure` takes a `VolumeSpec` value; the trial
`trees` CLI builds it from the same flags `agentfs init` takes, so no new configuration format appears. A volume
gets its UUID at create; zero remotes is a valid volume. Other users reach the tree mount, as they reach the
recording mount today; the tracer's workload runs as an unprivileged user. Tree-view commits gain the "where"
trailers, written through module cut step 5's one message function. A release build on a many-file workload, in
the shape of the recording engine's end-to-end budgets (`ci/bench-budget.json`: many files created, a large file
closed, a clone), gives the first release figures and sets tree-view budgets. A tracer CI job starts (below). Its
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
later: it may land before or after M7 and M8, and lands before M9, which deletes the recording engine's patch queue.
Its gate names its commands, so that no command serves both engines. Gate: a full run.

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
landing and never fails, so it cannot hold the move); a miss holds the move.

**M8. The CSI driver moves.** CreateVolume calls `ensure`; publish attaches a view host; DeleteVolume asks Volume's
release verdict, or for a fork claim ends its fork by key. A fork claim makes an in-volume fork, owned by Trees under the claim's handle (above); a fork whose
settings need approval is refused by Trees until landing approvals is built. Its architect writes the missing
CSI-on-trees page, which also carries several pods on one volume and moving a volume between nodes, as question 1's
answer places them. Existing recording-format volumes move by the runbook below; a recording-format volume is never
staged, and its DeleteVolume keeps its directory, with an alarm naming it. Gate: as M7.

**M9. The recording engine is deleted.** Its domain logic and the passthrough FUSE layer go, with its patch queue and
`serve-git` (rebuilt on trees by M6), its CSI entry points and one-shot commands, and its LFS and the other extras
as question 1's answer says. Adapters the tree engine uses stay where M4 and M5 re-homed them. Recording-only docs,
runbooks and alert rules go, as M5's list says. This page goes. Gate: ci and image.

## Order

```text
 6 → 5b → 5.1 → 5.2 → 5.3 ─┬─▶ M1 ─┬─▶ speed → M2 → M3 → M4 → M5 → M7 → M8 ─┬─▶ M9
                           └─▶ 7 ──┘                       │    │             │
                                                           │    └─▶ M5's recorder series (off the path)
                                                           └─▶ M6 ────────────┘
                                                   (M6: any time after M4, before M9)
```

- Step 7 (the model's checker reads git) and M1 run side by side; they touch disjoint crates. M2 adds model rules,
  which the checker then judges, so it waits for both.
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
  [supported configurations](../../supported-configurations.md) is marked "not built". M6 must land before M9, which
  deletes the recording engine's patch queue. The extras in the goals table are marked "not built" from the move
  until question 1's answer gives each its step, or its removal.
- **Links to deleted pages.** The commit that deletes a plan page repoints every link to it: step 7's commit,
  which deletes the module cut, rewrites the references to its steps here and in
  [root branch name](root-branch-name.md) as the built behaviour's library pages; M4's commit, which deletes root
  branch name, does the same for its links here, in the domain pages and in the baseline pages; M9's, which deletes
  this page, for every link to it. The docs workflow's link check fails otherwise.

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
- Goals and extras go dark for a while: iac's patches (alarmed), `restore` and `revert` from M7 until M6; the extras
  from a launcher's move until question 1's answer gives each its step.
- Existing volumes' history does not continue inside new ones. Whether a published name that a recording volume
  used diverges, fails loudly or meets nothing depends on M4's prefix.
- A volume re-created at a location whose state was lost (an `emptyDir`, a deleted Docker volume) gets a new UUID.
  Under a prefix built from the UUID it publishes a parallel history beside the old one, with no alarm.
- Old state areas are archived, not converted: an operator keeps them, and their disk, as long as the audit trail
  is wanted.
- The history format changes at 5b (the root's identity) and again at M1 (the "where" trailers).
- M7 repoints or deletes about two dozen `ci/` scripts and renames the CSI driver's two recording entry points.
- Two pages to write: file metadata (M2) and CSI on trees (M8). M2's new model rules run the costliest gate.
- **CI.** Wall time and runner time differ, because jobs run side by side. Measured on run 43146523186: a full
  `phase-1.yml` run takes about 73 min wall and about 300 runner-min; without the 500-seed shards, about 20 min wall
  and about 41 runner-min. A full model gate takes about 41 min on a 4-CPU container, outside CI. Measured on the
  2026-11-12 pushes: ci 9 to 13 min and image 5 to 10 min wall, side by side; with helm, f8 and docs, about 25 to
  30 runner-min per pushed commit. Before M1: four full runs (6, 5b, 5.1, 5.3; each moves or changes save and land
  code), about 4.9 h wall and 20 h runner time, and four full model gates (step 6 three, 5b one; about 2.7 h);
  step 7 adds one or two model gates beside M1. From M1 to M8: full runs where save or land logic changes (expected:
  M1's series, speed, M2, M3, M6) and short runs elsewhere (M4, M5, M7, M8), M2's model gate, the tracer job (not
  yet measured) and both engines' jobs in ci until M9. Today a dispatch always runs the seed shards; M1 adds a way
  to dispatch without them. The counts are estimates.
- Not planned: a converter, an engine flag, mirror-back (pull requests from a forge into the root).

## Open questions for the owner

Each is asked labelled "general guidance" or "absolute requirement". A later commit folds the answers in.

1. **Shipped extras at M9** (absolute requirement). Background: today these ship on the recording engine, and M9
   deletes it. The tree views have none of them, and each has a design page or none: LFS
   ([large and binary files](large-and-binary-files.md)), forks of forks ([nested forks](nested-forks.md)), several
   pods on one CSI volume and moving a volume between nodes (M8's CSI-on-trees page), the fork overlay of example 12
   (`--sandbox-overlay`; [the system](../system.md) has it become a fork), `fork deliver` (a fork's change delivered
   as an inbox patch, a branch, a command or a webhook, example 8) and unlinked forks (forks that never deliver).
   Question: for each, does M9 wait for a step that rebuilds it on the tree views, or may M9 drop it?
2. **The approval default and today's fork classes** (general guidance). Background: approval is a setting of each
   fork. Today a CSI fork class makes a linked fork, which delivers its change on finish for the receiver's
   acceptance rule (`agentfs.patchAccept`, default `cleanMerge`), or an unlinked fork, which never delivers. On the
   tree views a fork lands as it saves, unless its setting needs approval, and such a fork cannot be created until
   landing approvals is built. Question: when a fork's setting is absent, does it need approval (refused until then)
   or not (it lands as it saves)? And what does each of today's classes become?
3. **Tree-engine CI on every PR** (general guidance). Background: `phase-1.yml` runs today only on a push to
   `docs/target-architecture` and on a dispatch, so the tree engine's conformance and power-loss jobs never run on
   PRs or on `main`. Without the seed shards a run costs about 20 min wall and 41 runner-min. Question: from M1,
   does it run on every PR and every push to `main`?

**Prior art.** Strangler fig (Fowler) and parallel change (Sato) for moving one launcher at a time; branch by
abstraction rejected, because it keeps a dual path; tracer bullet (Hunt, Thomas); idempotent create (CSI
`CreateVolume`, `git init`); git's `include.path`; entity identity apart from its attributes (Evans, *Domain-Driven
Design*; Kubernetes' `uid` beside `name`); an idempotency key on a create request (CSI's volume name).
