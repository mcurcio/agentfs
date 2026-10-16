# Changelog

All notable changes to agentfs are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once
there are releases.

## [Unreleased]

Nothing has been released yet. `main` has the following, built since the
design was reviewed on 2026-10-12.

### Added

- **Recording.** A FUSE filesystem (`agentfs mount`) that durably records
  every change boundary (a close after a change, a rename, a delete, a
  link) before the call returns, through a journal with group commit and
  crash recovery, then commits it to a git branch the workload cannot see.
  Kernel passthrough is used on Linux 6.9 and later.
- **Path rules in git's own files**: `.gitignore`, `.gitattributes`,
  `.lfsconfig` and `$GIT_DIR/info/*`, with nested repositories and
  worktrees excluded by default.
- **Pre-commit checks**: git's fsck rules, including `.gitmodules`, and a
  per-remote maximum blob size, at record time and at commit time.
- **Remotes**: any number per volume, or none; local, `file://`, `http://`,
  `https://` (the `https` feature) and SSH through agentfs's own russh
  transport (the `ssh` feature), with token, key, ssh-agent and
  passphrase credentials re-read when rotated, verified host keys and
  `@cert-authority` host certificates, HTTP proxies, and a per-remote
  maximum push size that pushes a backlog in steps. Pushes are
  fast-forward only; a divergence is saved and alarmed, never overwritten.
- **Git LFS**: large files stored as LFS objects, uploaded before the
  commits that reference them, with concurrent and resumable transfers,
  `agentfs lfs fetch` and `agentfs lfs prune`, and a per-remote object
  size limit.
- **The `fsync` class** for databases changed in place, such as SQLite:
  whole-group captures at `fsync`, on a timer and at close, each a
  consistent point in time; hourly geometric repacks.
- **Recovery**: `agentfs restore` and `agentfs revert`, live through a
  running mount or directly, each recorded as one commit; operator
  runbooks for each failure mode.
- **The patch queue**: producers queue single-commit patches on a volume's
  remote (`agentfs patch submit`); each mount applies them in order before
  the workload sees the tree and blocks on the first that fails, with an
  alarm.
- **Several mounts of one volume**: `agentfs serve-history`, the volume's
  only writer of history, and fronts (`agentfs mount --history-service`)
  that hand it each change over gRPC on a Unix socket; each pod commits as
  itself.
- **The CSI driver** (`agentfs-csi`): agentfs volumes as
  PersistentVolumeClaims for several pods on one node, with operator
  profiles per StorageClass and crashed fronts and services restarted.
  Several nodes writing one volume are not supported, by decision.
- **Forked volumes**: a fork of a volume for a sandbox, created, finished
  and published as one change, and delivered by inbox, branch, pull
  request head, command or webhook; CSI claims cloned into forks.
- **Telemetry**: OpenTelemetry metrics, traces and logs, exported over OTLP
  gRPC or http/json, with TLS (`otlp-tls`); alarms on stderr.
- **Deployment**: the container image `ghcr.io/mcurcio/agentfs`, example
  Kubernetes manifests for the sidecar and the CSI driver, `docker run`
  and Compose examples, Helm charts for the CSI driver and the sidecar
  (published to `oci://ghcr.io/mcurcio/charts`), and example Prometheus
  alert rules with runbooks.
- **Testing**: end-to-end CI against real FUSE mounts, git servers over
  HTTP, HTTPS and SSH, LFS servers, an OpenTelemetry collector, NFS, kind
  clusters and csi-sanity; crash injection and power-loss replay on XFS
  and ext4; benchmarks against a budget.
- **Project**: dual MIT and Apache-2.0 licence files, contributing guide,
  security policy, code of conduct, issue and pull request templates, and
  the pixel-art logo in `docs/assets/`.

[Unreleased]: https://github.com/mcurcio/agentfs/commits/main
