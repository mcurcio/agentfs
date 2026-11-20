# One engine: the tree views run the sidecar, CSI and CLI

**Proposed; M1, M2, M3, M7 and M8 are built.** This page plans how agentfs goes from two engines to one: the tree views take over every
launcher, and the recording engine is deleted. It is the system-level plan; each milestone below starts with its
own architect and design gate, one layer down. M1 (volumes, clients, forks and their lifecycle on the tree views),
M2 (directories, metadata and links), M3 (operator rules and the untracked area), M7 (the `agentfs` binary's
`init` and `mount`, so the sidecar, the Docker examples and the CLI) and M8 (the CSI driver, through `agentfs host`)
are built and described in [tree views](../../trees.md), [the CSI driver](../../csi.md) and the
[library](../../library/core.md#git-backed-tree-views). The commit that lands the last milestone (M9) deletes this page and
moves the behaviour into the library and operator docs.

## Today

Every launcher runs the tree engine. The recording engine's commands still ship, on recording state areas only, and
no launcher runs them.

```text
 sidecar (deploy/sidecar, chart)  ─┐  agentfs init, mount [-- COMMAND], fork end
 Docker examples                  ─┤  (one launcher: the agentfs binary)  ─────────▶ TREE ENGINE
 CLI                              ─┘                                                (git model, conformance, model)
 CSI driver (agentfs-csi)  ──────────▶ agentfs init, one agentfs host per volume ─▶ TREE ENGINE
 nothing  ───────────────────────────▶ serve-history, front, the one-shot commands ▶ RECORDING ENGINE (deleted at M9)
```

The end state: one engine, the tree views, behind every launcher. Nobody builds a volume's spec by hand: each
launcher builds it from its configuration, as `init` does from its flags. The owner's guidance behind this plan is in
`OWNER-GUIDANCE.md` ("One engine").

## Goals and the step that delivers each

Every end goal has a delivering step. Steps 6, 5b, 5 and 7 are the [module cut](module-cut.md)'s; M1 to M9 are this
page's; "speed" is [save and take speed](save-and-take-speed.md).

| End goal | Delivered by |
|---|---|
| A baseline build verified with a target application | [the baseline](#the-baseline), the plan's first acceptance point: the first Hermes deploy, on a CSI volume claim, after M3, M7 and M8 |
| One engine | built: volumes and their lifecycle (M1); the binary, so the sidecar, Docker and the CLI (M7, [tree views](../../trees.md#run-it)); the CSI driver (M8, [csi](../../csi.md)); M9 (the recording engine is deleted) |
| Nobody builds a volume's spec by hand | built: `ensure` takes a `VolumeSpec` value (M1); the binary's `init` builds it from its flags, and the sidecar and Docker from their configuration (M7); CreateVolume runs `agentfs init` (M8) |
| One volume that root and non-root processes both use (the problem ArtifactFS stalled on) | built: other users reach the tree mount (M1); modes and owners decide what each may do (M2) |
| The launchers' speed holds | built: release-build figures against the tree-view budgets, `ci/trees-budget.json` (M1); speed (before M2, below); M7's gate, the tracer's many-file step (`create`) gated in `ci/trees-budget.json` |
| History knows the root by identity; no hardcoded `main` | module cut 5b |
| Consumers configure the name the root is published under, and may change it | [Root branch name](root-branch-name.md), built in M4 (the publisher and its key) |
| The trailer records meaningful information | built: which tree, by identity (5b), and where, the volume and the pod (M1) |
| Configuration: iac and the agent both edit it (`AGENTS.md`, "What agentfs is for") | M6, after the first Hermes deploy; until it lands, a configured patch source raises a standing alarm and logs the gap: built for the binary (M7, [operator settings](../../trees.md#operator-settings)) and each CSI volume's host (M8) |
| Audit; recording is the baseline (principle 1) | built (a write is journaled before it returns; power loss replayed by `phase-1.yml`; where, and the volume's own events as commits: M1; every kind of change recorded or refused loudly: M2; only operator rules and named built-ins skip a path: M3) |
| Invisible and untouchable (principle 2) | built (state outside the mount; the untracked area, M3); M4 (credentials never in a view) |
| Git idioms (principle 3) | built: `info/exclude`, `info/attributes` (M3); operator settings as a git-config include (M7, [operator settings](../../trees.md#operator-settings)); M4 (the published name's key) |
| The right technology, recorded (principle 4) | M4 (the reused transports in architecture.md's "Technology choices"); M9 |
| Keep operating (principle 5) | built: a view restarted after a crash stacks over its stale mount (M7, [restarting](../../trees.md#restarting-after-a-crash)); M4 (retry, divergence saved), M5 (alarms, liveness, each of today's alerts and runbooks ported or dropped), both after the first Hermes deploy; until then what is not built is said with the `agentfs.not_built` gauge, `AgentfsNotBuilt` and a log line, never silently (built for the binary, M7, and the CSI driver, M8) |
| Any remote, or none (principle 6) | built: the mirror leaves the model's world (module cut 6), and zero remotes is a valid volume (M1); M4 |
| History is the product (principle 7) | built: identity (5b), where (M1); old state areas refused at boot, archived and never deleted, by the [upgrading runbook](../../runbooks/upgrading-the-sidecar.md) (M7); M4 (never forced; a divergence is saved); built for CSI volumes: a deleted claim's volume is kept, and a recording-format one is never served or deleted (M8) |
| Recovery is manual but supported (principle 8); merging is an extra (principle 9) | M6 (restore, revert; patches at mount only, and `serve-git` as a patch source), after the first Hermes deploy |
| Three layers; OpenTelemetry for everything observable (principle 10) | each milestone's library page; M5 (telemetry, and the recorder's engine source) |
| The model stays the baseline proof | module cut 6 and 7; built: the tree engine's core tests on every PR ([CI tiers](#ci-tiers)) |
| Old state formats are not supported | 5b (tree format 3); built: `init` and `mount` refuse a recording state area, naming the runbook (M7); the CSI driver never serves one, naming [moving](../../runbooks/moving.md) (M8) |
| Extras that ship today (LFS, forks of forks, several pods on one volume, moving a volume, the fork overlay, `fork deliver`, unlinked forks and the rest) | Deferred: each is recorded with its home in [deferred extras](#deferred-extras), marked "not built" for the sidecar, Docker and the CLI from M7 and refused by the CSI driver from M8 ([supported configurations](../../supported-configurations.md), section 1.5), dropped at M9 until its slice lands |

## The shape

The launchers compose the domains of [the system](../system.md). The four a launcher composes (Volume, Clients,
Views, Trees) are built ([tree views](../../trees.md#clients)), and the rest arrive where they are needed (Rules in
M3, Replication in M4, Health and Telemetry in M5). The recording engine's own concepts (frozen copies and `pending/`,
the front, history service v2, forked volumes, checkpoints, the fsync class, the overlay's tmp layer) leave, and each
returns only as a slice of its own ([deferred extras](#deferred-extras)).

**One volume lifecycle for every launcher.** The sidecar, Docker, the CLI and CSI each build a `VolumeSpec` from
configuration and call the same idempotent `ensure`, then `stage`, as the trial CLI does today
([volume identity](../../trees.md#volume-identity-and-trailers), [lifecycle](../../trees.md#volume-lifecycle)).
Release is judged only by Volume. Operator settings (rules, remotes, the published name, what happens on release)
stay out of the spec: iac places them as a git-config include, and each owner reads its own at boot, so an iac edit
is never a spec mismatch.

**Identities and names.**

| Thing | Identity (what history records) | Name (configuration at the edge) |
|---|---|---|
| volume | a UUID assigned at create | its `VolumeName`, from the launcher's configuration |
| tree | `root`, or `fork/<n>`, a short number within its volume (5b) | the root's published name, the publisher's setting (M4), changeable at any boot |

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

**The trailer.** Every tree-view commit records which tree, the writer, the cause and where it was made, and never
the published name ([trailers](../../trees.md#volume-identity-and-trailers)). Pod and node names reach every remote
once M4 pushes, as they do today.

## Forks and landing approval

Whether a fork's change needs an approval before it lands in its source is a setting of that fork, as
[landing approvals](landing-approvals.md) has it; a fork configured to need approval cannot be created until that
slice is built. Landing is automated by default: a fork with no approval setting lands in its source as it saves,
as the tree views do today, and approval is opt-in per fork. Trees holds each fork's settings and is the one place
that refuses ([Trees](../domains/trees.md)); a launcher only passes them on. Built (M8): the CSI fork class's key
is `landing`, `automatic` by default; `approval` is refused by Trees (`landing approvals are not built`). A class with
`link: unlinked` makes an unlinked fork, which is [deferred](#deferred-extras): the driver refuses such a claim
(`unlinked forks are not built`) until its slice lands ([csi](../../csi.md#14-forks)).

A CSI fork claim's fork has one lifecycle owner, Trees: CreateVolume creates it with the claim's handle as its key,
so a repeated CreateVolume finds the same fork, and DeleteVolume ends it by that key
([keyed forks](../../trees.md#forks-keys-end-and-the-drop), built). The launcher keeps no map of its own. Built (M8).

The user-facing word for these is "fork": the binary's tree commands (`--fork-mount`; M7), and the CSI driver, its
chart and [supported configurations](../../supported-configurations.md) (example 5's fork claim; M8).

## Milestones

Each is its own PR series from `main`, gated by an architect, an implementer, one review, one fix round, and green
ci and image runs, plus what its row says. A "short run" of `phase-1.yml` is its core tests, without the 500-seed
shards; it runs on every PR and push to `main` that touches the code ([CI tiers](#ci-tiers)). A "full run" adds the shards, on a
manual trigger. Only a milestone that changes save or land logic needs the full run.

**M1. Volumes and their lifecycle.** Built. Volume (`ensure`, `stage`, `unstage`, the Stop drain and the release
verdict), Clients (attach, rebind, detach, sessions, one writer per fork), Views (the view host) and Trees (create
with settings and a key, end, drop) are in [tree views](../../trees.md); their invariants are in the
[library](../../library/core.md#domain-invariants). A volume gets its UUID at create, zero remotes is a valid volume,
other users reach the tree mount, and every commit carries the "where" trailers. The tracer and the release figures
run in `image.yml` ([tracer](#tracer), [performance](../../performance.md#tree-views-release-build)), and
`phase-1.yml` runs on every PR. Left to later milestones: a start point from another volume's line
([deferred](#deferred-extras)), and operator settings as a git-config include, built with their first owner (M7).

**Speed.** [Save and take speed](save-and-take-speed.md) comes before M2 when M1's release figures miss the
budgets, and debug-build figures say they will (about 96 ms a save against the recording engine's 6 ms a file;
[performance](../../performance.md)). It comes first because M2, M3 and M4 extend the save and take path it
rewrites. Gate: a full run; the budgets met.

**M2. Beyond flat files.** Built. A tree holds directories (made, removed, renamed whole), symbolic links and each
entry's metadata (kind, mode bits, owner and group); each entry is presented with its recorded owner and mode, and
the kernel checks permissions by them. Times are each tree's own, never in history. Forks land directories, metadata
and links by the model's rules 13 to 15, and a change that loses by them names its cause (`metadata`,
`directory-not-empty`, `directory-gone`). [Tree views](../../trees.md) describes them and what a commit holds. Left
out: hard links past the first, names that are not UTF-8, devices, pipes and sockets, and extended attributes, each
refused as before; a line moved between files; an all-squash presented owner.

**M3. Rules and the untracked area.** Built. `history.git/info/exclude` and `-agentfs` in `info/attributes` decide
which new names a tree records (git-model §3), with the built-ins: `.git`, FIFOs and sockets, anything under an
untracked directory or inside a nested repository. A tracked path stays recorded, and one `-agentfs` marks leaves
history at the next open. Unrecorded files live in each tree's untracked area as ordinary Linux files (SQLite's
write-ahead log works), merged into the view; a new fork starts with a copy of its upstream's area; renames, links,
FIFOs and sockets across the two halves behave as on Linux or answer `EXDEV` where Linux tools expect it, and an
atomic save from an excluded temporary name records one whole-file save. [Tree views](../../trees.md#rules-and-the-untracked-area)
describes it; the pure crate is `agentfs-rules`. Left out, each refused at open, naming its line: database groups,
size and store hints, LFS and every other `agentfs.*` attribute ([rules](../domains/rules.md) keeps them, with view
profiles, rule-set versions across view hosts, tree rule files and bare-repository detection). Moved to M7: operator
settings as a git-config include, because the engine owns and rewrites `history.git/config`, and M3's rules need no
setting at their defaults (`nested = exclude` is built in). M3 needed no full run: no model rule or generator
changed, and the whole-file replace, which no model call reaches, is guarded by a test that its commit equals an
open, truncate, write and close of the same bytes; the conformance replay ran at the regression tier.

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
`AgentfsWorktreeHeuristic`, `AgentfsTelemetryExportFailing`; M7 added `AgentfsNotBuilt` for the tree mount) and 16 runbooks (`docs/runbooks/`: crashed sidecar,
CSI recovery, divergence, fork refused, history rewrite, history service down, LFS storage, moving, patch queue,
reconcile failed, recording failures, recovery and undo, start branch moved, stuck pushes,
upgrading the sidecar, worktree heuristic) as ported or dropped, with the reason; a ported alert fires against the
tree engine's own metrics in the alerts workflow's real Prometheus. A second PR series, which nothing later waits
for, builds the recorder's engine source: agentfs-core's OpenTelemetry events feed the model's recorder
(`calls::Sink`) through an adapter in agentfs-conformance, with content hashes, not bytes. The export never blocks a
save and so may drop data; the recorder needs a lossless, unsampled, in-process listener beside it. Squaring the two
is that series' first design question. Gate: a short run; a regression found on a mount replays in the model from
the engine's own events.

**M6. Patches, restore and revert on trees.** iac's patches applied at mount, `serve-git` as a patch source, and
`restore` and `revert` on trees ([integrator patches](integrator-patches.md)). It needs M4's remotes and nothing
later, and comes after the first Hermes deploy ([the baseline](#the-baseline)).
Its gate names its commands, among them the producer's `patch submit`, so that no command serves both engines.
Gate: a full run.

**M7. The `agentfs` binary moves.** Built. The sidecar, the Docker examples and the CLI share one launcher, the
binary's `init` and `mount`, on the tree engine; `agentfs trees` is gone ([tree views](../../trees.md#run-it)).
`init --volume NAME [--include FILE]... [--owner UID:GID]` creates or finds a volume, recording its root with the
owner given (`VolumeSpec.root_owner`); `mount ROOT` serves it with every former trial flag, `--on-stale-mount
refuse|hold|recover` and `--seal-mountpoint` ([restarting](../../trees.md#restarting-after-a-crash)), and a trailing
`-- COMMAND` that runs once every view is up and drains the mount when it exits
([running a command](../../trees.md#running-a-command-in-the-mount)); `fork end --fork-key KEY` ends a keyed fork.
Views mount as `agentfs:<volume>`, so `probe` sees them, and a view restarted after a crash stacks over its stale mount
(`agentfs_fuse::stale`, shared with the recording mount). Operator settings are `<state>/config`, git-config
`include.path` lines that `init --include` writes; each mount logs every key whose capability tree volumes do not
build (`not built: <key> (<capability>): ...`), counts them in the `agentfs.not_built` gauge, and `AgentfsNotBuilt`
alerts on it ([operator settings](../../trees.md#operator-settings)). The CSI driver spawns `agentfs serve-history`
and `agentfs front` (the front was `mount --history-service`). Each entry point runs one engine, and no flag selects
one: `init`, `mount` and `fork end` refuse `--root` and a recording state area, naming the
[upgrading runbook](../../runbooks/upgrading-the-sidecar.md), and every recording command refuses a tree volume as not
built. The sidecar's manifests and chart, the Docker examples and Hermes' pod ([deployment](../../deployment.md),
[docker](../../docker.md)) run the tree engine; Hermes' migration Job copies the files in through `mount --
COMMAND`. The sandbox overlay's manifests and flags are gone. The recording launcher's CI jobs are retired
([costs](#costs)). Gate: the tracer's many-file step (`create`) is gated in `ci/trees-budget.json`.

| Entry point | Before M7 | M7 to M8 (built) | M8 to M9 (built) |
|---|---|---|---|
| `init`, `mount`, `fork end` | recording (`trees init`, `trees mount`, `trees end` the trial's) | tree | tree |
| `host` | none | none | tree: the CSI driver's per-volume view host |
| the CSI front and `serve-history` | recording (`mount --history-service`, `serve-history`) | recording: `front`, `serve-history` | not called; deleted at M9 |
| the recording one-shot commands (`record`, `flush`, `push`, `checkpoint`, `run`, `patch`, `fork` but `end`, `lfs`, `start accept`, `restore`, `revert`, `status`) | recording | recording, on a recording state area only; a tree volume is refused | the same; deleted at M9 |

**M8. The CSI driver moves.** Built, and described in [the CSI driver](../../csi.md) and
[library/kubernetes](../../library/kubernetes.md). CreateVolume runs `agentfs init` (Volume's ensure). The node plugin
runs one `agentfs host` per volume in use, driven over the child's standard input
([the view host command](../../library/filesystem.md#the-view-host-command)); NodePublish opens a view at the
kubelet's target with the pod's context, and a crashed host is restarted with its views replayed and clients rebound.
DeleteVolume of a root claim drains the volume through a host (Volume releases it) and keeps the directory; of a fork
claim, it ends the fork by key. A fork claim is an in-volume fork owned by Trees under the claim's handle, landing
automatically. Refused, each with an error that says it is not built: a second pod on a claim, `startFrom`,
`workloadMounts`, `onDelete: delete`, `link: unlinked`, `landing: approval` (by Trees) and forks of forks (by Trees).
Push, patches and LFS are logged and gauged as not built at each host's start. A recording-format volume is never
served: NodePublish and DeleteVolume answer `FAILED_PRECONDITION` naming the directory and
[the moving runbook](../../runbooks/moving.md), so its PV stays Released until the operator archives the directory and
removes it by hand. Not built here, and why: the library-level coordinator (no library consumer; `mount` and `host`
share the binary's serving module), and alarms raised by the plugin itself (M5; it has no telemetry export, so the
recording-format alarm is the failing DeleteVolume's Kubernetes events). Gate: image's `e2e-csi` and `e2e-csi-forks`,
helm's chart install with `e2e-csi`, f8's three jobs, hermes' `csi`, ci's `csi-sanity`.

### The baseline

The plan's first acceptance point: the first Hermes deploy, on a CSI volume claim, so that the workload declares no
sidecar (the owner, 2026-11-19, `OWNER-GUIDANCE.md`). It is upstream Hermes
([deployment](../../deployment.md#10-hermes)) on the tree engine through the CSI driver. The baseline builds nothing
of its own. It is reached when M3, M7 and M8 have landed and its gate passes; a failure is fixed in the milestone
that owns the gap. Push, alarms and patches (M4, M5, M6) come after it, and until they land M7 and M8 say plainly,
with an alarm and a log line and never silently, what is not built yet.

What the deploy needs, and the milestone that gives it:

| Need | Milestone |
|---|---|
| A volume created, or recovered, from the launcher's configuration | built (`ensure`, M1; CreateVolume runs `agentfs init`, M8) |
| The mount at the application's data path, used by root, by other users and by a docker-in-docker tools container | built (other users reach the mount, M1; directories, symlinks, modes and owners, M2) |
| Rule files: what is recorded, and untracked paths (secrets, databases, sessions, logs) never in history | built (M3) |
| The binary the CSI driver spawns runs the tree engine | built: `init` and `mount` (M7); `init` and `host`, which the driver runs (M8) |
| A CSI volume claim on the tree engine | built (M8) |
| The start-up speed the probes allow | M1's budgets; speed, if M1's figures miss them |
| What is not built yet (push, alarms, patches) said with an alarm and a log line | built for the binary (M7: `agentfs.not_built`, `AgentfsNotBuilt`, a `not built:` line per key) and each CSI volume's host (M8) |

Only these come before it. M4, M5, M6, M9 and every [deferred extra](#deferred-extras) come after, or never block
it.

Gate: hermes' `e2e (Hermes on kind, csi)` (`ci/e2e-hermes-kind.sh`) green on the tree engine: it records a tool
container's edit, keeps secrets and databases out of history, and finds the log line and alarm for each gap. The
sidecar and Docker jobs (image's `e2e (kind cluster)`, hermes' `sidecar` and `fence`, docker's
`e2e (docker compose, upstream Hermes)`) run the tree engine since M7.

**M9. The recording engine is deleted.** Its domain logic and the passthrough FUSE layer go, with its patch queue and
`serve-git` (rebuilt on trees by M6), its CSI entry points and one-shot commands, and every
[deferred extra](#deferred-extras) it still serves. Adapters the tree engine uses stay where M4 and M5 re-homed
them. Recording-only docs, runbooks, alert rules and CI jobs go, as M5's list and the deferred table say. This page
goes; its deferred table moves to [the target architecture's index](../README.md) first. Gate: ci and image.

## Order

```text
 6 → 5b → 5.1 → 5.2 → 5.3 → M1 → speed → M2 → M3 → M7 → M8 ─▶ BASELINE ─┬─▶ M4 ─┬─▶ M5 ─┬─▶ M9 → deferred extras
                                                                         │       └─▶ M6 ─┘
                                                                         └─▶ 7 (the model's checker); M5's recorder series
```

The order serves the baseline, the first Hermes deploy (the owner, 2026-11-19: on a CSI volume claim, after M3, M7
and M8; push, alarms and patches after it). M1, M2, M3, M7 and M8 are built; the baseline, Hermes on a CSI volume
claim, is next.

- Step 6 is built before the plan's first milestone: its first slice moves the mirror out of `World`, which M4's
  publisher is checked against, and its transaction rules ride along because they were already in flight.
- Step 5b precedes M1 because M1's "where" trailers and M4's publisher both name trees by identity, which 5b gives.
- Step 7 (the model's checker reads git) is not on the baseline's path: the checker judges M2's new model rules as it
  is today, reading some facts through `World`. It runs after the baseline.
- Step 5 precedes M1 because M2, M3, M4 and M6 each extend a module step 5 makes.
- Each serial edge has a reason. Speed precedes M2 because M2 to M4 extend the path it rewrites. M2 precedes M3
  because rules classify directories and symlinks, which M2 adds. M3 precedes M4 because a mirror publishes what
  rules record, and a path published before rules exclude it stays in history, which is never rewritten
  (principle 7). M7 precedes M8 because the CSI driver spawns the binary M7 moves. M4 precedes M5 because the push
  and divergence alarms need the publisher, and M6 because patches need M4's remotes.
- **Goals may lapse during the move.** agentfs is before its 1.0 release, so a launcher moves without waiting for
  every goal its users rely on. M4, M5 and M6 are off M7's and M8's path: from each launcher's move until they land,
  it pushes to no remote, has no tree-engine alarms beyond the ones M7 and M8 raise for these gaps, applies no iac
  patches at mount, and has no `restore` or `revert`. Each gap is logged at boot and alarmed while it lasts, never
  silent, and example 3 of [supported configurations](../../supported-configurations.md) is marked "not built" (built
  for M7: its section 1.5 lists the binary's gaps). Each [deferred extra](#deferred-extras) is marked "not built" in
  supported configurations from its launcher's move (M7 or M8) until its slice lands.
- **Links to deleted pages.** The commit that deletes a plan page repoints every link to it: step 7's commit,
  which deletes the module cut, rewrites the references to its steps here and in
  [root branch name](root-branch-name.md) as the built behaviour's library pages; M4's commit, which deletes root
  branch name, does the same for its links here, in the domain pages and in the baseline pages; M9's, which deletes
  this page, for every link to it. The docs workflow's link check fails otherwise.

## Deferred extras

Every capability the recording engine ships that no milestone above rebuilds. None is needed for
[the baseline](#the-baseline). Each went dark for the binary, so for the sidecar, the Docker examples and the CLI, at
M7, and is marked "not built" for them in [supported configurations](../../supported-configurations.md) (section
1.5); it went dark for the CSI driver at M8, which refuses each with an error that says so, and is deleted with the recording engine at M9. Each returns as a slice of its own, from its home below; M9's
commit moves this table to [the target architecture's index](../README.md), so no row is lost with this page.

| Capability | What it is | Who uses it today | Home | Until then |
|---|---|---|---|---|
| Git LFS | content over a threshold stored as LFS objects, uploaded beside each push | example 11; `cli_lfs.rs` | [Large and binary files](large-and-binary-files.md) | not built on tree volumes from M7: a `filter=lfs` rule refuses the volume, and an `lfs` setting is logged and alarmed, under the CSI driver too from M8; dropped at M9 until its slice lands |
| Forks of forks | a fork of a linked fork, whose change rolls up through its parent | example 6; f8's `e2e-csi-fork-of-fork` | [Nested forks](nested-forks.md) | `fork create` refuses a tree volume from M7; refused by the CSI driver from M8 (`forks of forks are not built`); dropped at M9 until its slice lands |
| Several pods on one volume | one history service per CSI volume, one front per pod | example 4; `history_service.rs`, image's `e2e-csi` | [The system](../system.md) (the "history service with view hosts" composition), [Views](../domains/views.md) | refused by the CSI driver from M8 (`several pods on one volume are not built`); dropped at M9 until its slice lands |
| Moving a volume between nodes | a new volume on another node, started from the old one's branch, by the moving runbook | example 10; f8's `e2e-csi-move` | [Volume](../domains/volume.md) (the start point) | refused by the CSI driver from M8 (`startFrom`; the moving runbook copies files by hand); dropped at M9 until its slice lands |
| `startFrom` and `start accept` | a CSI start class: a volume whose history begins at another volume's commit; `StartBranchMoved` while that branch moves | example 10; f8's `e2e-csi-move` | [Volume](../domains/volume.md) (`StartPoint::Line`, the start record) | refused by the CSI driver from M8 (`startFrom is not built on tree volumes`); only the empty start is built; dropped at M9 until its slice lands |
| The fork overlay | the sandbox overlay (`mount --sandbox-overlay` until M7): a second mount that shows a fork's workload only recordable files and keeps the rest in a private tmp layer | example 12; `crates/agentfs-fuse/src/overlay/tests.rs` | [The system](../system.md) (the overlay becomes a fork), [Overlay gaps](sandbox-overlay-gaps.md) | offered by no launcher from M7: its flags, manifests and chart values are deleted; `agentfs-fuse` keeps it until M9; dropped at M9 until its slice lands |
| `fork deliver` | a finished fork's change handed to a receiver as an inbox patch, a branch, a command or a webhook | example 8; `crates/agentfs/tests/cli_fork_deliver.rs` | [Fork delivery](fork-delivery.md) | no launcher from M8: CSI fork claims land in their source and publish no change; dropped at M9 until its slice lands |
| Unlinked forks | a fork that never delivers to its source and pushes only to its own remotes | example 5's `link: unlinked` class; `crates/agentfs/tests/cli_fork.rs`, `crates/agentfs-csi/tests/grpc.rs` | [Fork delivery](fork-delivery.md) | `fork create` refuses a tree volume from M7; refused by the CSI driver from M8 (`unlinked forks are not built`); dropped at M9 until its slice lands |
| Recording without a mount | `agentfs run` (the commit and push loops with no mount), `record` and `flush` (named paths of the backing tree) | the recording engine's Rust tests (`cli_record.rs`, `cli_push.rs`) | [Recording without a mount](recording-without-a-mount.md) | `run`, `record` and `flush` refuse a tree volume from M7, and the migration Job copies through `mount -- COMMAND`; dropped at M9 until its slice lands |
| Checkpoint and the change line | an integrator cuts one commit per change intent on `refs/agentfs/<volume>/change-line`; a remote may get only that line | `crates/agentfs/tests/cli_checkpoint.rs` | [Checkpoints and patch notices](integrator-hooks.md) | `checkpoint` refuses a tree volume from M7; dropped at M9 until its slice lands |
| Patch notices | the `patch-notify` hook and the `agentfs.patchNotifyUrl` webhook, sent when the queue's verdicts change | `cli_patch.rs` | [Checkpoints and patch notices](integrator-hooks.md) | not built on tree volumes from M7: a patch setting is logged and alarmed; dropped at M9 until its slice lands |
| Database groups | `agentfs.commit=fsync` and `agentfs.group`: a database and its journal recorded whole at `fsync` | `cli_record.rs`, `cli_restore.rs` | [Open files and groups](files-open-groups.md) | not built on tree volumes from M7: the attributes refuse the volume, naming the line; dropped at M9 until its slice lands |
| State areas on NFS | a CLI or sidecar state area on an NFS 4.1 export, its lock reaching other clients | nothing in CI | [State on NFS](state-on-nfs.md) | not built on tree volumes from M7; dropped at M9 until its slice lands |
| Hidden fork history | `agentfs.forkRefs = hidden`: a fork's history outside `refs/heads/`; whether a forge runs CI on it is not verified | supported configurations, "Remote topology" | [Forge CI on hidden refs](forge-ci-on-hidden-refs.md) | not built on tree volumes from M7: `agentfs.forkRefs` is logged and alarmed as `push`; dropped at M9 until its slice lands |
| A fork of a claim in use | a fork claim whose source a pod is writing; Kubernetes may one day refuse such a clone | image's `e2e-csi-forks` | [Clone in use](clone-in-use.md) | not dropped: CSI fork claims (M8) are clones too, and image's `e2e-csi-forks` clones a claim in use, so the page stays and applies to them |

Each row's CI job or test goes with the recording engine at M9 and returns with its slice. The CI jobs that drove the
standalone recording launcher went at M7, because no recording `init` remains ([costs](#costs)).

## CI tiers

Built: `phase-1.yml` runs the regression tier on every PR and every push to `main` that touches the code, and the full
sweep with the 500-seed shards on a manual run or `[phase1-seeds]` in the pushed commit message
([VERIFYING.md](../../../crates/agentfs-model/VERIFYING.md), "Tiers"). A milestone that changes save or land logic asks
for the full sweep once before it merges (its "full run").

## Existing recording volumes

Built for the binary (M7): `init` and `mount` refuse a recording state area and name the
[upgrading runbook](../../runbooks/upgrading-the-sidecar.md), which pushes the backlog, archives the old state area
(never deleted) and copies the files in through the new mount. There is no converter. What the new root's first push
meets on a remote that holds the old branch depends on M4's prefix, which M4's gate checks.

**CSI volumes (M8).** Built: the driver finds a recording-format volume by its state area's format and never serves
it; NodePublish and DeleteVolume answer `FAILED_PRECONDITION` naming the directory and
[the moving runbook](../../runbooks/moving.md), and the plugin logs each such directory at start. The runbook archives
the old history (a bundle of `state/git`), unpacks the old branch's files through a new claim's mount, and has the
operator remove the old directory by hand, after which DeleteVolume succeeds.

## Tracer

Built: `image.yml`'s `trees-tracer` job runs `ci/e2e-trees-tracer.sh` on the shipped image, in the sidecar's and the
Docker example's shape: `agentfs init` and `agentfs mount` in an agentfs container and an unprivileged workload
container. Its steps are `ensure` and its refusal of another name, a save by another user with its "where" trailers, a
kill mid-write and a restart in place, and the release figures against the tree budgets, the many-file step (`create`)
gated.

Each milestone adds its step. Built for M2: a directory, an executable bit and a chown through the mount. Built for
M3: with `*.db` and `.env` marked `-agentfs`, the workload's `state.db` and `.env` work through the mount, survive a
restart, and never reach history. Built for M7: the same steps through the sidecar on kind (`ci/e2e-kind.sh`), and
the `not built:` line for the configured remote at every boot, with nothing reaching the git server (Hermes' kind
jobs check the patch source's line too; `ci/e2e-telemetry.sh` and `ci/e2e-alerts.sh` the `agentfs.not_built` gauge).
To come: M4 a refusing
remote (the filesystem keeps working), then a push under a configured name; M5 the alarm at a collector; M6 a
producer's patch at the next mount and a `restore`. Built for M8 by the CSI jobs rather than the tracer: image's
`e2e-csi` (a CSI volume, a killed host, a plugin restart) and `e2e-csi-forks` (a fork claim landing in its source).
Not covered: several pods, LFS,
database groups, nested forks, transactions, upgrades.

## Design rules (design choices)

1. **Each entry point runs one engine.** A launcher's move replaces its entry points in one PR; no flag, setting or
   fallback selects an engine.
2. **One volume lifecycle for every launcher.** Every launcher builds a `VolumeSpec` and calls the same `ensure` and
   `stage`; release is judged only by Volume; operator settings are read only by their owners.
3. **Identity in history, names at the edge.** History records the volume's UUID and the tree's identity; a name a
   consumer configures is applied by the publisher and may change without rewriting anything.

## Costs

- Eight milestones and the speed slice after M1, touching every crate, doc, chart and workflow.
- Goals and extras go dark for a while: pushes until M4, tree-engine alarms until M5, iac's patches, `restore` and
  `revert` until M6, each from its launcher's move (M7 or M8) and said with an alarm and a log line; each
  [deferred extra](#deferred-extras) from its launcher's move until its own slice lands, which no milestone here
  schedules. A user who needs one stays on a release before its launcher's move until then.
- Existing volumes' history does not continue inside new ones. Whether a published name that a recording volume
  used diverges, fails loudly or meets nothing depends on M4's prefix.
- A volume re-created at a location whose state was lost (an `emptyDir`, a deleted Docker volume) gets a new UUID.
  Under a prefix built from the UUID it publishes a parallel history beside the old one, with no alarm.
- Old state areas are archived, not converted: an operator keeps them, and their disk, as long as the audit trail
  is wanted.
- M7 retired the recording launcher's CI: the scripts and jobs that drove a standalone recording `init` and `mount`
  (remotes over HTTP, HTTPS and SSH, shared and failing remotes, LFS, patches, several pods, NFS, SQLite, recovery,
  the runbooks, mergiraf, the Docker restart-and-patch job, the recording benchmarks and power loss), because no
  recording `init` remains to create their state areas and a second one would be a dual path. Since M8 the CSI jobs
  run the tree engine too, so the recording engine is covered by its Rust tests alone: `crash_daemon.rs` and the
  one-shot commands' tests, which create their state areas through the library. Push, patches, LFS, NFS, several pods, database groups,
  recording power loss and the recording benchmarks have no shell end-to-end test until their slices return.
- A sidecar stopped alone while a `subPath` consumer (Hermes) holds the mount drains, unmounts its view and answers
  the bind's writes with `EROFS`, but exits only when it is killed (the kubelet's kill after the pod's grace, or
  `kill -9`); the consumer's bind then fails with `ENOTCONN` until its liveness probe restarts it onto the restarted
  mount ([deployment](../../deployment.md#7-a-crashed-daemon)).
  No shutdown grace or connection abort is added: either would be a timer in place of an event.
- CSI on trees is described in [the CSI driver](../../csi.md) (M8).
- M2's new model rules ran on their scenarios and on the conformance replay of those scenarios on the library and on
  mounts, at the regression tier; the random runs do not make their calls. The full model sweep was the owner's call,
  and he chose to skip it (2026-11-18).
- The baseline's gate adds no job: it is hermes' existing `csi` job, run against the tree engine.
- M3 needed no full model sweep: no model rule or generator changed, and the whole-file replace is guarded by its
  equality test instead.
- **CI.** Wall time and runner time differ, because jobs run side by side. Measured on run 43146523186: a full
  `phase-1.yml` run takes about 73 min wall and about 300 runner-min; without the 500-seed shards, about 20 min wall
  and about 41 runner-min. A full model gate takes about 41 min on a 4-CPU container, outside CI. Measured on the
  2026-11-12 pushes: ci 9 to 13 min and image 5 to 10 min wall, side by side; with helm, f8 and docs, about 25 to
  30 runner-min per pushed commit. Step 7 adds one or two model gates after the
  baseline. From M2 to M8: full runs where save or land logic changes (expected: speed, M6) and short runs elsewhere (M4, M5, M7, M8), the tracer job (about
  ten minutes in `image.yml`), and from M8 the CSI jobs on the tree engine. The core tier ([CI tiers](#ci-tiers)) adds about
  20 min wall, side by side with ci, and about 41 runner-min to every PR push and every push to `main`; the
  randomized shards cost only when triggered. The counts are estimates.
- Not planned: a converter, an engine flag, mirror-back (pull requests from a forge into the root).

## Prior art

Strangler fig (Fowler) and parallel change (Sato) for moving one launcher at a time; branch by
abstraction rejected, because it keeps a dual path; tracer bullet (Hunt, Thomas); idempotent create (CSI
`CreateVolume`, `git init`); git's `include.path`; entity identity apart from its attributes (Evans, *Domain-Driven
Design*; Kubernetes' `uid` beside `name`); an idempotency key on a create request (CSI's volume name).
