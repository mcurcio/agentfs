# agentfs documentation

agentfs is a FUSE filesystem that records every change to a directory in
git history its users cannot see, pushes that history to git remotes, and
applies patches queued from outside. This is the index of its
documentation, also published as a book at
<https://mcurcio.github.io/agentfs/>. New here? Start with the
[README's quick start](https://github.com/mcurcio/agentfs#quick-start).

Section numbers (§) across these pages refer to [design.md](design.md).

## Using agentfs

| Page | For |
|---|---|
| [Operator guide](operator-guide.md) | Running `agentfs mount`: requirements, layout, flags, settings, permissions, what is recorded when, lifecycle, the state-area lock, crashes, recovery, LFS, databases, troubleshooting, several mounts of one volume |
| [Patches](patches.md) | Changing a volume's files from outside it through the patch queue |
| [Forked volumes](forks.md) | A sandbox's copy of a volume that publishes one change for review, and delivering that change |
| [Telemetry](telemetry.md) | Turning on OpenTelemetry export, what is exported, and the example alerts |
| [Configuration reference](configuration.md) | Every git config key, operator attribute, flag and environment variable (checked against the code in CI) |
| [Supported configurations](supported-configurations.md) | The dimensions a configuration varies on (integration point, sharing, remotes, knobs), built or designed, and named example configurations to check a design against |

## Deploying

| Page | For |
|---|---|
| [Docker](docker.md) | The agentfs container and workload containers, with `docker run` and Compose |
| [Kubernetes sidecar](deployment.md) | The container image and the privileged native sidecar, with the alert rules and the sandbox overlay |
| [CSI driver](csi.md) | Volumes as claims for unprivileged pods, several pods per volume on one node, forks |
| [Helm charts](helm.md) | The CSI driver chart and the sidecar library chart |

## Runbooks

One procedure per failure mode, each linked from the alarms and alerts
that point to it.

| Runbook | When |
|---|---|
| [Recovery and undo](runbooks/recovery-and-undo.md) | Finding a change and bringing files back, or undoing a commit |
| [Recording failures](runbooks/recording-failures.md) | `close` fails with `EIO`, `RecordFailed` or `CommitFailed` |
| [Pushes that keep failing](runbooks/stuck-pushes.md) | `PushFailed` again and again, a growing push backlog |
| [A diverged remote](runbooks/divergence.md) | Someone else moved the volume's branch (`Diverged`) |
| [Removing a recorded secret](runbooks/history-rewrite.md) | A secret reached history: rewrite it, the one documented force-push |
| [The patch queue](runbooks/patch-queue.md) | A held patch or a blocked queue (`PatchHeld`, `PatchBlocked`, `PatchRuleFailed`), patches held after an unclean stop (`PatchQueueDeferred`), a notice that failed (`NotifyFailed`), a refused push to `serve-git`, a fork of a fork whose parent finished first (`ForkParentFinished`), a fork that finished with patches its change does not carry (`ForkInboxNotCarried`) |
| [LFS storage](runbooks/lfs-storage.md) | The LFS cache's size, pruning and fetching objects |
| [A crashed sidecar](runbooks/crashed-sidecar.md) | The daemon died in a pod |
| [Upgrading the sidecar](runbooks/upgrading-the-sidecar.md) | Rolling out a new image for the phase 1 sidecar |
| [The history service is down](runbooks/history-service-down.md) | Fronts failing with `EIO` (`HistoryServiceUnavailable`) |
| [Recovering CSI volumes](runbooks/csi-recovery.md) | Incidents under the CSI driver |
| [Moving a volume to another node](runbooks/moving.md) | A node lost or retired: a new volume started from the old branch |
| [The start branch moved](runbooks/start-branch-moved.md) | `StartBranchMoved`: the old branch received commits after a move |
| [A fork's publish is refused](runbooks/fork-refused.md) | `ForkRefused`, by reason |
| [A moved directory could not be reconciled](runbooks/reconcile-failed.md) | `ReconcileFailed` |
| [Worktree paths found by the heuristic](runbooks/worktree-heuristic.md) | `agentfs.nested.gitdir_heuristic` counting |
| [The sandbox overlay](runbooks/sandbox-overlay.md) | The overlay refuses to start, a sandbox cannot save, tmp files gone, both mounts stopped, sandboxes still on a dead mount (`agentfs.sandbox_overlay.refusals`) |

## How it works

| Page | For |
|---|---|
| [Design](design.md) | The specification: what is recorded when, the guarantees, the decisions (§19.1) and the open questions (§19.2) |
| [Architecture](architecture.md) | The crates, every module, the data flow of a change and the crash model (checked against the code in CI) |
| [Performance](performance.md) | Measured costs, the benchmark and the performance budget |
| [Building and testing](development.md) | Building, the features, what each CI workflow runs, and running the end-to-end tests yourself |
| [Adversarial review, 2026-10-12](reviews/2026-10-12-adversarial.md) | The review of the first design draft and what changed because of it |

## The project

Contributing, the Code of Conduct, security reports and the licences are
in the repository:
[CONTRIBUTING.md](https://github.com/mcurcio/agentfs/blob/main/CONTRIBUTING.md),
[CODE_OF_CONDUCT.md](https://github.com/mcurcio/agentfs/blob/main/CODE_OF_CONDUCT.md),
[SECURITY.md](https://github.com/mcurcio/agentfs/blob/main/SECURITY.md),
[LICENSE-APACHE](https://github.com/mcurcio/agentfs/blob/main/LICENSE-APACHE) and
[LICENSE-MIT](https://github.com/mcurcio/agentfs/blob/main/LICENSE-MIT).
