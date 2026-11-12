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
```

The end state: one engine, the tree views, behind every launcher. Nobody runs `agentfs trees init` by hand: each
launcher builds a volume's spec from its configuration. The owner's guidance behind this plan is in
`OWNER-GUIDANCE.md` ("One engine").

## Goals and the step that delivers each

Every end goal has a delivering step. Steps 6, 5b, 5 and 7 are the [module cut](module-cut.md)'s; M1 to M9 are this
page's.

| End goal | Delivered by |
|---|---|
| One engine | M1 (volumes from configuration), M7 and M8 (the launchers move), M9 (the recording engine is deleted) |
| Nobody runs `trees init` by hand | M1 (`ensure` from configuration), M7 (`agentfs trees` folds into `init` and `mount`) |
| History knows the root by identity; no hardcoded `main` | module cut 5b |
| Consumers configure the name the root is published under, and may change it | [Root branch name](root-branch-name.md), built in M4 (the publisher and its key) |
| The trailer records meaningful information | 5b (which tree, by identity); M1 (where: the volume and the pod) |
| Configuration: iac and the agent both edit it (`AGENTS.md`, "What agentfs is for") | M6 |
| Audit; recording is the baseline (principle 1) | built (a write is journaled before it returns); M1 (where); M2 (every kind of change recorded or refused loudly); M3 (only operator rules skip a path) |
| Invisible and untouchable (principle 2) | built (state outside the mount); M3 (the untracked area); M4 (credentials never in a view) |
| Git idioms (principle 3) | M1 (operator files as a git-config include); M3 (`info/exclude`, `info/attributes`); M4 (the published name's key) |
| The right technology, recorded (principle 4) | M4 (the reused transports in architecture.md's "Technology choices"); M9 |
| Keep operating (principle 5) | M4 (retry, divergence saved), M5 (alarms, liveness, restart in place) |
| Any remote, or none (principle 6) | module cut 6 (the mirror leaves the model's world); M1 (zero remotes is a valid volume); M4 |
| History is the product (principle 7) | 5b (identity); M1 (where); M4 (never forced; a divergence is saved) |
| Recovery is manual but supported (principle 8); merging is an extra (principle 9) | M6 (restore, revert; patches at mount only) |
| Three layers; OpenTelemetry for everything observable (principle 10) | each milestone's library page; M5 (telemetry, and the recorder's engine source) |
| The model stays the baseline proof | module cut 6 and 7 |
| Old state formats are not supported | 5b (tree format 3); M7 and M8 (a recording state area is refused at boot) |

## The shape

No new domain. The launchers compose the domains of [the system](../system.md); the recording engine's own concepts
(frozen copies and `pending/`, the front, history service v2, forked volumes, checkpoints, the fsync class, the
overlay's tmp layer) leave, and each returns only as a slice of its own.

**One volume lifecycle for every launcher.** The sidecar, Docker, the CLI and CSI each build a `VolumeSpec` from
configuration and call the same idempotent `ensure`, then `stage` ([Volume](../domains/volume.md)). Release is judged
only by Volume. Operator files (rules, remotes, the published name) stay out of the spec: iac places them as a
git-config include, and each owner reads its own at boot, so an iac edit is never a spec mismatch.

```rust
pub struct VolumeSpec { name: VolumeName, start: StartPoint, on_release: OnRelease }

pub trait Volume {
    /// Idempotent. No state: create, with a new volume id. Same name and start: recover, taking `on_release`
    /// from this spec. Another name or start: `SpecMismatch`, before any write.
    fn ensure(&mut self, spec: VolumeSpec, by: &LaunchCap) -> Result<VolumeState, VolumeError>;
    fn id(&self) -> VolumeId;   // a UUID, assigned at create, never changed
    // stage, unstage, release_verdict: as on the Volume page
}
```

**Identities and names.**

| Thing | Identity (what history records) | Name (configuration at the edge) |
|---|---|---|
| volume | a UUID assigned at create | its `VolumeName`, from the launcher's configuration |
| tree | `root`, or `fork/<n>`, a short number within its volume (5b) | the root's published name, the publisher's setting (M4), changeable at any boot |

Each volume publishes its refs under a prefix of its own (M4), so several volumes share one repository without their
`root` and `fork/<n>` refs meeting.

**The trailer.** Every tree-view commit records which tree (its identity, 5b), the writer and the cause, as
[agentfs in git terms](../git-model.md) §5 has them, and never the published name. From M1 it also records where
the commit was made, as trailers: the volume's name and UUID, and the namespace, pod, pod UID and node the launcher
runs in (the recording engine's `Agentfs-Namespace`, `Agentfs-Pod`, `Agentfs-Pod-Uid` and `Agentfs-Node`). Pod and
node names reach every remote, as they do today.

## Forks and landing approval

Whether a fork's change needs an approval before it lands in its source is a setting of that fork. A fork configured
to need approval cannot be created until [landing approvals](landing-approvals.md) is built; creating one fails with
a clear error meanwhile. Every other fork lands in its source as the tree views do today. On the CSI driver the
setting belongs to the fork's class; M8's architect picks its key.

The user-facing word for these is "fork". M7 renames "sandbox" in the binary (the trial CLI's `--sandbox`) and M8 in
the CSI driver, its chart and [supported configurations](../../supported-configurations.md) (example 5's "sandbox
claim" becomes a fork claim). Each milestone's gate decides the new spellings.

## Milestones

Each is its own PR series from `main`, gated by an architect, an implementer, one review, one fix round, and green
ci and image runs, plus what its row says.

**M1. Volumes from configuration.** Every launcher gets `ensure(VolumeSpec)`; a volume gets its UUID at create; zero
remotes is a valid volume. Tree-view commits gain the "where" trailers, written through module cut step 5's one
message function. A tracer CI job starts (below). Its gate decides where the spec is stored and audited, the trailer
keys, and how a launcher passes its pod context. Gate: one `phase-1.yml` dispatch; the tracer.

**M2. Beyond flat files.** Directories, symlinks, non-UTF-8 names and more links
([beyond flat files](tree-views-beyond-flat-files.md)), and file metadata: modes, the presented owner and times, in
place of today's `0600`, the daemon's uid and epoch times. Its architect writes the missing file-metadata page. The
model gains rules, so it runs the full model gate. Gate: the full model gate; one dispatch.

**M3. Rules and the untracked area.** Operator rules decide what is recorded ([rules](../domains/rules.md),
git-model §3), and unrecorded files live in the untracked area. Its gate decides whether untracked files get
ordinary Linux open semantics and what a fork sees of its source's. Gate: one dispatch.

**M4. Mirrors and the published name.** Mirrors on the tree model ([replication](../domains/replication.md)), the
publisher, each volume's refs under its own prefix, and history a mirror's reader can read
([line map and renames](line-map-and-renames.md)). The SSH and HTTP transports, credentials and the fast-forward
claim are reused, re-homed under Replication, and recorded in architecture.md's "Technology choices". Its gate
decides the published name's key, what a rename does to the old remote ref, and the prefix's form
([root branch name](root-branch-name.md), which M4 deletes). Gate: one dispatch.

**M5. Keep operating.** Telemetry, alarms, liveness and restart in place
([tree-view telemetry](tree-view-telemetry.md), [health](../domains/health.md)), and the recorder's engine source:
agentfs-core's OpenTelemetry events feed the model's recorder (`calls::Sink`) through an adapter in
agentfs-conformance, with content hashes, not bytes. The export never blocks a save and so may drop data; the
recorder needs a lossless, unsampled, in-process listener beside it. Squaring the two is M5's first design question.
Gate: one dispatch; a regression found on a mount replays in the model from the engine's own events.

**M6. Patches, restore and revert on trees.** iac's patches applied at mount, and `restore` and `revert` on trees
([integrator patches](integrator-patches.md)). It needs M4's remotes and nothing later: it may land before or after
M7 and M8. Gate: one dispatch.

**M7. The `agentfs` binary moves.** The sidecar, the Docker examples and the CLI share one launcher, the binary's
`init` and `mount`, so they move together. `agentfs trees` folds into `init` and `mount` and goes. Between M7 and M9
the binary ships both engines, with the CSI driver's path still on the old one; each path runs exactly one engine,
and nothing selects. Gate: a release-build run of the tracer against `ci/bench-budget.json`; a miss holds the move
and pulls [save and take speed](save-and-take-speed.md) in front of it.

**M8. The CSI driver moves.** CreateVolume calls `ensure`; publish attaches a view host; DeleteVolume asks Volume's
release verdict. A fork claim makes an in-volume fork; a fork class configured to need approval is refused until
landing approvals is built. Its architect writes the missing CSI-on-trees page. Gate: as M7.

**M9. The recording engine is deleted.** Its domain logic and the passthrough FUSE layer go; LFS and `serve-git` go,
each extra's slice bringing back what it needs from history. Adapters the tree engine uses stay where M4 and M5
re-homed them. Recording-only docs, runbooks and alert rules go. This page goes. Gate: ci and image.

## Order

```text
 6 → 5b → 5.1 → 5.2 → 5.3 ─┬─▶ M1 ─┬─▶ M2 → M3 → M4 → M5 → M7 → M8 → M9
                           └─▶ 7 ──┘              │
                                                  └─▶ M6 (any time after M4)
```

- Step 7 (the model's checker reads git) and M1 run side by side; they touch disjoint crates. M2 adds model rules,
  which the checker then judges, so it waits for both.
- Step 5 precedes M1 because M2, M3, M4 and M6 each extend a module step 5 makes.
- **Goals may lapse during the move.** agentfs is before its 1.0 release, so a launcher moves without waiting for
  every goal its users rely on. M6 is off M7's path: from M7 until M6 lands, a moved launcher applies no iac patches
  at mount and has no `restore` or `revert`, and example 3 of [supported configurations](../../supported-configurations.md)
  is marked "not built". Extras (several pods on one CSI volume, moving a volume, LFS, nested forks) are marked "not
  built" from the move until their own slices.

## Existing recording volumes

A recording state area is refused at boot by the format check (`FormatsOf`, [the system](../system.md)); there is no
converter. To move one, create a volume from configuration and copy the files in through its mount, as
[deployment](../../deployment.md)'s migration Job does. The old history stays on its remotes under
`refs/heads/agentfs/<volume>`. If the new root is published under that same name on that remote, its first push is
not a fast-forward: Replication saves it as a divergence and raises an alarm, never overwrites. The upgrading runbook
says to pick another name.

## Tracer

From M1, a CI job on the shipped image, in the Docker example's shape (an agentfs container and an unprivileged
workload container), on the `trees` commands driven from configuration:

1. a configuration file, then `ensure`, then mount; a second `ensure` recovers; another name is refused;
2. the workload writes `a.txt`: saved in the root's history before `close` returns, with its "where" trailers;
3. agentfs is killed mid-write and restarted in place: every answered write is saved.

Each milestone adds its step: M2 a directory and an executable bit; M3 a database file in the untracked area, never
in history; M4 a refusing remote (the filesystem keeps working), then a push under a configured name; M5 the alarm at
a collector; M6 a producer's patch at the next mount and a `restore`; M7 the same steps through the sidecar on kind;
M8 a CSI volume and a fork claim. Not covered: several pods, LFS, database groups, nested forks, transactions,
upgrades.

## Design rules (design choices)

1. **Each launcher path runs one engine.** A launcher's move replaces its path in one PR; no flag, setting or
   fallback selects an engine.
2. **One volume lifecycle for every launcher.** Every launcher builds a `VolumeSpec` and calls the same `ensure` and
   `stage`; release is judged only by Volume; operator files are read only by their owners.
3. **Identity in history, names at the edge.** History records the volume's UUID and the tree's identity; a name a
   consumer configures is applied by the publisher and may change without rewriting anything.

## Costs

- Nine milestones after the module cut, touching every crate, doc, chart and workflow.
- Goals and extras go dark for a while: iac's patches, `restore` and `revert` from M7 until M6; several pods, moving,
  LFS and nested forks from a launcher's move until their slices.
- Existing volumes' history does not continue inside new ones; a reused published name diverges, loudly.
- The history format changes at 5b (the root's identity) and again at M1 (the "where" trailers).
- Two pages to write: file metadata (M2) and CSI on trees (M8). M2's new model rules run the costliest gate.
- **CI.** Measured: a `phase-1.yml` dispatch takes 73 min (run 43146523186); a full model gate about 41 min on a
  4-CPU container. Before M1: four dispatches (6, 5b, 5.1, 5.3; about 4.9 h of runner time) and four full model
  gates (step 6 three, 5b one; about 2.7 h), plus ci and image (5 to 9 min each, side by side) on every pushed
  commit; step 7 adds one or two model gates beside M1. M1 to M9: about one dispatch per engine milestone (about
  9.7 h), M2's model gate, the tracer job (not yet measured) and both engines' jobs in ci until M9. The counts are
  estimates.
- Not planned: a converter, an engine flag, mirror-back (pull requests from a forge into the root).

**Prior art.** Strangler fig (Fowler) and parallel change (Sato) for moving one launcher at a time; branch by
abstraction rejected, because it keeps a dual path; tracer bullet (Hunt, Thomas); idempotent create (CSI
`CreateVolume`, `git init`); git's `include.path`; entity identity apart from its attributes (Evans, *Domain-Driven
Design*; Kubernetes' `uid` beside `name`).
