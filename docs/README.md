# agentfs documentation

agentfs is a FUSE filesystem that records every change to a directory in
git history its users cannot see, pushes that history to git remotes, and
applies patches queued from outside. This is the index of its
documentation, also published as a book at
<https://mcurcio.github.io/agentfs/>. New here? Start with the
[README's quick start](https://github.com/mcurcio/agentfs#quick-start).

These pages describe what agentfs does now. The code and the tests are the
reference: tests check [configuration](configuration.md) and
[architecture](architecture.md) against it. Designs for work that is not built
are under [Future designs](#future-designs).

## Using agentfs

| Page | For |
|---|---|
| [Guarantees and limits](guarantees.md) | What agentfs promises, what it does not, and which test backs each claim |
| [How agentfs records changes](recording.md) | What is recorded and when, and how records become commits |
| [Remotes and pushing](remotes.md) | Configuring remotes, when pushes run, what a failed push does, several volumes on one repository |
| [Tree views](trees.md) | Mounts of git-backed trees: upstreams and forks, how saves flow, running them |
| [Operator guide](operator-guide.md) | Running `agentfs mount`: requirements, layout, flags, settings, permissions, what is recorded when, lifecycle, the state-area lock, crashes, recovery, LFS, databases, troubleshooting, several mounts of one volume |
| [Patches](patches.md) | Changing a volume's files from outside it through the patch queue |
| [Forked volumes](forks.md) | A sandbox's copy of a volume that publishes one change for review, and delivering that change |
| [Telemetry](telemetry.md) | Turning on OpenTelemetry export, what is exported, and the example alerts |
| [Configuration reference](configuration.md) | Every git config key, operator attribute, flag and environment variable (checked against the code in CI) |
| [Supported configurations](supported-configurations.md) | The dimensions a configuration varies on (integration point, sharing, remotes, knobs), built or designed, and named example configurations to check a design against |

## Library

| Page | For |
|---|---|
| [The library: `agentfs-core`](library/core.md) | The engine: recording, history, pushing, restore, patches, forks and the history service, for building another front end or tooling |
| [The filesystem: `agentfs-fuse` and `agentfs`](library/filesystem.md) | The FUSE operations, the binary's commands, exit codes and control socket |
| [The Kubernetes adapter: `agentfs-csi`](library/kubernetes.md) | The CSI driver as the adapter layer, and how to build on it |

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
| [Upgrading the sidecar](runbooks/upgrading-the-sidecar.md) | Rolling out a new image for the sidecar |
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
| [Architecture](architecture.md) | The crates, every module, the data flow of a change and the crash model (checked against the code in CI) |
| [Performance](performance.md) | Measured costs, the benchmark and the performance budget |
| [Building and testing](development.md) | Building, the features, what each CI workflow runs, and running the end-to-end tests yourself |
| [agentfs in git terms](target-architecture/git-model.md) | Every tree, save, lock, landing and sync of the tree model as git branches, commits and refs. The baseline the model and its tests prove |
| [The rules ledger](target-architecture/rules-ledger.md) | Every rule of that model in plain words, with the tests that check it |
| [The fork and upstream rules in plain git](target-architecture/demo/walkthrough.md) | The rules run with a shell script that uses only `git` and coreutils, run in CI |

## Future designs

[Overview](target-architecture/README.md). The designs of slices that are not
built are under `target-architecture/`; the baseline pages are listed above. When
a slice is built, its page is deleted and the behaviour is in the pages above.

## The project

Contributing, the Code of Conduct, security reports and the licences are
in the repository:
[CONTRIBUTING.md](https://github.com/mcurcio/agentfs/blob/main/CONTRIBUTING.md),
[CODE_OF_CONDUCT.md](https://github.com/mcurcio/agentfs/blob/main/CODE_OF_CONDUCT.md),
[SECURITY.md](https://github.com/mcurcio/agentfs/blob/main/SECURITY.md),
[OWNER-GUIDANCE.md](https://github.com/mcurcio/agentfs/blob/main/OWNER-GUIDANCE.md) (the owner's standing answers),
[LICENSE-APACHE](https://github.com/mcurcio/agentfs/blob/main/LICENSE-APACHE) and
[LICENSE-MIT](https://github.com/mcurcio/agentfs/blob/main/LICENSE-MIT).
