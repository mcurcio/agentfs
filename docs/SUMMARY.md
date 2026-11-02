# Summary

[Introduction](README.md)

# Using agentfs

- [Operator guide](operator-guide.md)
- [Patches](patches.md)
- [Forked volumes](forks.md)
- [Telemetry](telemetry.md)
- [Configuration reference](configuration.md)
- [Supported configurations](supported-configurations.md)

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

- [Design](design.md)
- [Architecture](architecture.md)
- [Performance](performance.md)
- [Building and testing](development.md)
- [Adversarial review, 2026-10-12](reviews/2026-10-12-adversarial.md)

# Target architecture

- [Overview](target-architecture/README.md)
  - [The system](target-architecture/system.md)
  - [Rules](target-architecture/domains/rules.md)
  - [Clients](target-architecture/domains/clients.md)
  - [Views](target-architecture/domains/views.md)
  - [Trees](target-architecture/domains/trees.md)
  - [Forks](target-architecture/domains/forks.md)
  - [Journal](target-architecture/domains/journal.md)
  - [Landing](target-architecture/domains/landing.md)
  - [History](target-architecture/domains/history.md)
  - [Replication](target-architecture/domains/replication.md)
  - [Volume](target-architecture/domains/volume.md)
  - [Health](target-architecture/domains/health.md)
  - [Telemetry](target-architecture/domains/telemetry.md)
