# Summary

[Introduction](README.md)

# Using agentfs

- [Guarantees and limits](guarantees.md)
- [How agentfs records changes](recording.md)
- [Remotes and pushing](remotes.md)
- [Operator guide](operator-guide.md)
- [Patches](patches.md)
- [Forked volumes](forks.md)
- [Tree views](trees.md)
- [Telemetry](telemetry.md)
- [Configuration reference](configuration.md)
- [Supported configurations](supported-configurations.md)

# Library

- [The library: agentfs-core](library/core.md)
- [The filesystem: agentfs-fuse and agentfs](library/filesystem.md)
- [The Kubernetes adapter: agentfs-csi](library/kubernetes.md)

# Deploying

- [Docker](docker.md)
- [Kubernetes sidecar](deployment.md)
- [CSI driver](csi.md)
- [Helm charts](helm.md)

# Runbooks

- [Recovery and undo](runbooks/recovery-and-undo.md)
- [Recording failures](runbooks/recording-failures.md)
- [Pushes that keep failing](runbooks/stuck-pushes.md)
- [A diverged remote](runbooks/divergence.md)
- [Removing a recorded secret](runbooks/history-rewrite.md)
- [The patch queue](runbooks/patch-queue.md)
- [LFS storage](runbooks/lfs-storage.md)
- [A crashed sidecar](runbooks/crashed-sidecar.md)
- [Upgrading the sidecar](runbooks/upgrading-the-sidecar.md)
- [The history service is down](runbooks/history-service-down.md)
- [Recovering CSI volumes](runbooks/csi-recovery.md)
- [Moving a volume to another node](runbooks/moving.md)
- [The start branch moved](runbooks/start-branch-moved.md)
- [A fork's publish is refused](runbooks/fork-refused.md)
- [A moved directory could not be reconciled](runbooks/reconcile-failed.md)
- [Worktree paths found by the heuristic](runbooks/worktree-heuristic.md)
- [The sandbox overlay](runbooks/sandbox-overlay.md)

# How it works

- [Architecture](architecture.md)
- [Performance](performance.md)
- [Building and testing](development.md)
- [agentfs in git terms](target-architecture/git-model.md)
- [The rules ledger](target-architecture/rules-ledger.md)
- [The fork and upstream rules in plain git](target-architecture/demo/walkthrough.md)

# Future designs

- [Overview](target-architecture/README.md)
  - [The system](target-architecture/system.md)
  - [Rules](target-architecture/domains/rules.md)
  - [Clients](target-architecture/domains/clients.md)
  - [Views](target-architecture/domains/views.md)
  - [Trees](target-architecture/domains/trees.md)
  - [Replication](target-architecture/domains/replication.md)
  - [Volume](target-architecture/domains/volume.md)
  - [Health](target-architecture/domains/health.md)
  - [Telemetry](target-architecture/domains/telemetry.md)
  - [Snapshot opens (parked)](target-architecture/snapshot-opens.md)
  - [One engine](target-architecture/future/one-engine.md)
  - [Module cut](target-architecture/future/module-cut.md)
  - [Root branch name](target-architecture/future/root-branch-name.md)
  - [Transactions](target-architecture/future/transactions.md)
  - [Nested forks](target-architecture/future/nested-forks.md)
  - [Beyond flat files](target-architecture/future/tree-views-beyond-flat-files.md)
  - [Tree-view telemetry](target-architecture/future/tree-view-telemetry.md)
  - [Landing approvals](target-architecture/future/landing-approvals.md)
  - [Integrator patches](target-architecture/future/integrator-patches.md)
  - [Open files and groups](target-architecture/future/files-open-groups.md)
  - [Large and binary files](target-architecture/future/large-and-binary-files.md)
  - [Space limits](target-architecture/future/space-limits.md)
  - [Line map and renames](target-architecture/future/line-map-and-renames.md)
  - [Shared mappings](target-architecture/future/shared-mappings.md)
  - [Save and take speed](target-architecture/future/save-and-take-speed.md)
  - [Live landing](target-architecture/future/live-landing.md)
  - [Recovery tooling](target-architecture/future/recovery-tooling.md)
  - [Overlay gaps](target-architecture/future/sandbox-overlay-gaps.md)
  - [Power-loss coverage](target-architecture/future/power-loss-coverage.md)
  - [OTLP TLS reload](target-architecture/future/otlp-tls-reload.md)
  - [Clone in use](target-architecture/future/clone-in-use.md)
  - [Forge CI on hidden refs](target-architecture/future/forge-ci-on-hidden-refs.md)
