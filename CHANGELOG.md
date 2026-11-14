# Changelog

All notable changes to agentfs are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once
there are releases.

## [Unreleased]

Nothing has been released yet. `main` has the following, built since the
design was reviewed on 2026-10-12.

### Added

- **Tree views: clients and sessions.** Each view of `trees mount` is a client of
  its tree, attached through the Clients domain, and every save names it:
  `Agentfs-Writer: <label>#<n>@<launcher>/<tree>` (`root-client#5@cli/root`), its
  number the attach's journal step. A view reaches its tree only through its
  client's session (`agentfs_core::session`): a read-only client's writes answer
  `EROFS` (`--read-only`, `--sandbox-read-only`), and a session reaches only its
  own open files. A fork has one writer for life: a second read-write mount of it,
  live or after a clean unmount, exits 1. After a kill the next mount rebinds the
  client (same id, the new pod context); a clean unmount detaches it, its open
  files saved and durable first. Attach, rebind and end are commits on
  `refs/agentfs/clients/<tree>` (`Attached`, `Rebound`, `AttachmentEnded`,
  `LastDetached`), durable before they return, never waiting for history. The
  view host is `agentfs_fuse::tree_view::ViewHost` (docs/trees.md, "Clients").
  `--writer` and `--sandbox-writer` are now client labels: safe names of at most
  64 bytes.
- **Tree views: volume events on record as commits.** A fork's row is a commit
  on its own chain, `refs/agentfs/trees/fork/<n>`, beside the fork: its tree is
  `row.json` (the row with its audit stamps) and its message the event
  (`ForkCreated`, with `Agentfs-Kind`, `Agentfs-By`, `Agentfs-Event` and the
  volume and pod trailers, `Agentfs-Format: row-1`). The fork's branch and its row
  are one journal record, durable before the call returns; the event commit is
  built behind it. `trees fork` and a mount's new sandbox create forks through
  the Trees domain on the new store adapter (`agentfs_core::store::TreeStore`).
  Rows and queue facts are read from the journal, never Git, so they answer while
  the builder lags or fails (docs/trees.md, "Rows and event commits"). The tree
  state format is `agentfs-trees-10`; a state area of any other format is
  refused, never converted.
- **Tree views: a large file costs its save, not memory.** A line map holds each
  line's identity and version as runs, never its bytes: bytes have one home, the
  file. A save's alignment reads both versions a line at a time (a new file and an
  append run no `git diff`), git streams blobs over 8 MiB, and a take's merged
  files are written from their sources into chunk records of at most 16 MiB, so a
  take of any size records. A 1 GiB save builds in about 40 s with the builder at
  17 MiB (one local run); the figures add `history_1g` and `memory_1g`. Opening a
  volume never waits for history: it serves the journal's records replayed on the
  trees as built, and a failed build raises its alarm without refusing the open.
  No record after a checkpoint names a step the checkpoint covers
  (docs/trees.md, "What a commit holds", "File maps"). The tree state format is
  `agentfs-trees-9`; a state area of any other format is refused, never
  converted.
- **Tree views: file maps and journal segments.** No file's bytes are held in
  memory: a file is a map of journal frames and its checkout as built, so a write
  costs its own bytes at any file size and a close compares only the bytes the
  open touched. The journal is a directory of 64 MiB segments (`journal/`); a
  compaction writes a checkpoint segment and drops the segments behind it, and a
  file held open pins only its own frames. Checkouts write each file by rename, so
  a reader keeps the version it opened. A save record carries a digest of the
  file's pieces in place of a checksum. A map holds no descriptor: a checked-out
  base is a hard link in `bases/`, read through a cache of 256 open files. The
  figures add `close_1g` and `write_1g` (docs/trees.md, "File maps").
- **Tree views: a save costs its change, not the tree.** A commit keeps each
  file's bytes, line map and name history under two levels of hashed folders
  (`files/<aa>/<bb>/<name>`, `lines/...`, `names/<cc>/<dd>/<id>`), and the builder
  rewrites only the changed files' folders. `metadata.json`, `namespace.json` and
  the object table refs (`refs/agentfs/objects/<tree>`) are gone: object numbers
  live in the journal's checkpoint. A new file's first save runs no `git diff`.
  Checkouts mirror `files/` (docs/trees.md, "What a commit holds").
- **Tree views: history is built behind the calls.** A step's journal record is the
  change itself; the call path runs no git. A builder thread makes the commits with
  `git fast-import` behind it, and Git's refs, commits and checkouts show only what
  is built (`Recording::settle` waits for it), recorded in `built.log`. A failed
  build is retried and never fails a call.
- **Tree views: takes are decided off the actor.** A queue take is decided on
  built history with the actor unlocked, so calls go on while it judges, and lands
  only if nothing it read changed since; otherwise it is decided again
  (docs/trees.md, "Takes").
- **Tree views: a call is journalled without an fsync.** A write, create, rename
  or unlink is in the journal before it returns and survives a crash of agentfs;
  close, fsync, an `O_SYNC` write or an fsync of the view's directory make it
  power-cut safe, with waits sharing one fsync. The journal's frames are `AFJ3`.
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
- **Tree views identify their volume and writers** ([trees](docs/trees.md)):
  `agentfs trees init --volume NAME` (required) gives a volume a random id,
  finds it again under the same name with nothing written, and refuses
  another name. Every commit carries `Agentfs-Event`, `Agentfs-Volume`,
  `Agentfs-Volume-Id` and the writer's pod context (`--namespace`, `--pod`,
  `--pod-uid`, `--node`); a landing merge names the writer it lands. `trees
  mount` never creates a volume, mounts with FUSE `allow_other` unless
  `--no-allow-other`, and presents its files as `--owner UID:GID`.
- **Testing**: end-to-end CI against real FUSE mounts, git servers over
  HTTP, HTTPS and SSH, LFS servers, an OpenTelemetry collector, NFS, kind
  clusters and csi-sanity; crash injection and power-loss replay on XFS
  and ext4; benchmarks against a budget.
- **Project**: dual MIT and Apache-2.0 licence files, contributing guide,
  security policy, code of conduct, issue and pull request templates, and
  the pixel-art logo in `docs/assets/`.

[Unreleased]: https://github.com/mcurcio/agentfs/commits/main
