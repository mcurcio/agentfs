# Changelog

All notable changes to agentfs are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions will
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once
there are releases.

## [Unreleased]

Nothing has been released yet. `main` has the following, built since the
design was reviewed on 2026-10-12.

### Changed

- **The CLI's package is `agentfs-cli`.** Its directory is `crates/agentfs-cli`
  and its binary is still `agentfs`; build it from source with
  `cargo build --release -p agentfs-cli`. No package is named `agentfs`.
- **`agentfs init` and `mount` run the tree engine.** `agentfs trees` is gone:
  `init --volume NAME [--include FILE]... [--owner UID:GID]` creates or finds a
  tree volume (`<name> <id> created|found`), `mount ROOT` serves it (with every
  former `trees mount` flag, plus `--on-stale-mount refuse|hold|recover`,
  `--seal-mountpoint` and a trailing `-- COMMAND` that runs once every view is
  up and drains the mount when it exits), and `fork end --fork-key KEY` ends a
  keyed fork. Each mount logs, at every boot, every operator setting whose
  capability tree volumes do not build yet (`not built: remote.origin.url
  (push): ...`) and counts them in the `agentfs.not_built` gauge, and refuses
  an include under a view. A view restarted after a crash stacks over the stale
  mount its predecessor left. `init`, `mount` and `fork end` refuse `--root`
  (`AGENTFS_ROOT`) and a recording state area (move it with
  `docs/runbooks/upgrading-the-sidecar.md`); every recording command (`status`,
  `push`, `restore`, `run` and the rest) refuses a tree volume, saying it is not
  built yet; the check reads the tree format's marker, refuses when it cannot
  read it, and covers `fork create --source-state`. `mount` refuses a mount
  point inside the state area, an include under a view at any depth, and
  every other refusal before it creates or seals a mount point; it logs an
  `includeIf` line as not built, logs not-built settings on a held boot too,
  stacks only over a view's own stale mount (never a new unkeyed fork over a
  crashed one's), and detaches dead layers left under `<state>/stack/` at
  start.
- **The sidecar, its chart, the Docker examples and Hermes run the tree
  engine.** The sidecar's init container runs `agentfs init --include
  /etc/agentfs/config`, and the sidecar mounts `/agentfs/workspace` with
  `--on-stale-mount recover` and `--seal-mountpoint`; the rule files are
  `history.git/info/exclude` and `info/attributes`, and there is no backing
  tree. The Docker examples create the volume with `init --include --owner`
  (no chown), and Hermes' migration Job copies the files in through `mount
  -- COMMAND`. History is `/data/state/history.git`, branch `root`. A remote
  or patch source in their settings is logged as not built at every boot.
  `docs/runbooks/upgrading-the-sidecar.md` moves a recording volume onto a
  tree volume, archiving the old state area.
- **The CSI driver runs the tree engine.** CreateVolume runs `agentfs init
  --volume <name> --include <profile>.config` and installs the profile's rule
  files; the node plugin runs one `agentfs host` per volume in use on its node,
  driving it over the child's standard input (a Unix socket only the plugin
  holds; JSON lines, protocol v2: `open`, `close`, `reconcile`, `fork_create`,
  `fork_end`, each request and reply in an envelope with an id, answered in
  any order, so a close that waits for a held file stalls only the requests
  about its mount, client or fork). A plugin refuses a host of another
  protocol version (`FAILED_PRECONDITION`). EOF, SIGTERM or SIGINT drains a host with no view open; a host
  with views open exits 3 at once and leaves their mounts stale, as a crash
  does, so a plugin restart keeps every pod's view and a fork claim's one
  writer. A drain logs `tree DrainBlocked` naming the clients of other
  launchers it waits for. Views mount as type `fuse.agentfs`. NodePublish opens a view directly at the
  kubelet's target with the pod's context (`Agentfs-Pod`, `Agentfs-Namespace`,
  `Agentfs-Pod-Uid`, `Agentfs-Node` trailers); there is no NodeStage. A crashed
  host, or a restarted plugin, opens every published view again, stacked over
  the stale mounts, rebinding each client. One pod per claim: a second is
  refused (`several pods on one volume are not built`). DeleteVolume of a root
  claim drains the volume through a host and keeps its directory
  (`csi.retained.json`); a static PV mounts a kept volume again. A **fork
  claim** (a fork class, `fork: "true"`, `landing: automatic`, with a
  `dataSource` claim on the same node) is an in-volume fork of its source,
  owned by Trees under the claim's PV name, served by the source's host and
  landing in the source automatically; DeleteVolume ends it. Refused as not
  built: `onDelete: delete`, `startFrom`, `workloadMounts`, `link: unlinked`
  and `remotes`, `landing: approval` and forks of forks (both refused by
  Trees). Each host logs push, patch and LFS settings as `not built:` and
  counts them in `agentfs.not_built` through the `OTEL_*` variables the plugin
  passes on. A recording-format volume is never served or deleted: NodePublish
  and DeleteVolume answer `FAILED_PRECONDITION` naming the directory and
  `docs/runbooks/moving.md`, which archives it and moves its files onto a tree
  volume. `agentfs host --launcher ID --volume NAME` is new, and
  `agentfs_contracts::host` holds its messages.

### Removed

- The recording engine's standalone mount (`agentfs mount BACKING MOUNTPOINT`,
  with `--passthrough`, `--threads`, `--unmount-grace`, `--push-grace`,
  `--workload-mount` and the sandbox overlay's `--sandbox-overlay` and
  `--sandbox-owner`; the fronts keep their own), `probe --sandbox-overlay`, and
  the recording `init` (`--branch`).
- `agentfs-csi`'s recording-engine flags: `--mount-dir`, `--passthrough`,
  `--front-threads`, `--stage-wait`, `--publish-wait`, `--unpublish-grace`,
  `--unstage-grace`, `--history-wait` and `--release-scan-interval`
  (`AGENTFS_CSI_RELEASE_SCAN_INTERVAL`), with the release scan, the push and
  fork finishing it ran, NodeStage and NodeUnstage, and start classes.
- The sandbox overlay's manifests: `deploy/examples/sidecar-sandbox-overlay`,
  the sidecar chart's `sandboxOverlay` values and the overlay runbook. No
  launcher offers the overlay.
- The CI jobs that drove the standalone recording launcher: remotes over
  HTTP, HTTPS and SSH, shared and failing remotes, LFS, patches, several
  pods, NFS, SQLite, recovery, the runbooks, mergiraf, the Docker
  restart-and-patch job, and the `bench.yml` and `power-loss.yml`
  workflows. The recording engine is covered by its Rust tests.

### Added

- **`AgentfsNotBuilt`.** An example alert rule, a warning while
  `agentfs_not_built` counts a configured operator setting whose capability
  tree volumes do not build yet, labelled by capability. The telemetry,
  OTLP-transport and alerts checks run it against a tree mount.

- **Tree views: launcher support.** A new tree volume records its root
  directory with the owner its launcher names (`VolumeSpec.root_owner`,
  `UID:GID`), else the creating process's. A tree volume's operator settings
  are `<state>/config`, git-config syntax, holding `include.path` lines that
  libgit2 follows; every configured key tree volumes do not build yet (push,
  patches, LFS, other) is classified, and the `agentfs.not_built` gauge counts
  them by capability. Tree views mount as `agentfs:<volume>` with subtype
  `agentfs`, so `probe` and stale detection see them, and a view can stack over
  a stale mount of its volume left by a view host that died, detaching the
  stale layers when it ends. The stale-mount helpers are one module,
  `agentfs_fuse::stale`, shared with the recording mount.
- **Tree views: operator rules and the untracked area.** A tree volume reads
  `history.git/info/exclude` and `history.git/info/attributes` when it opens.
  A new name they leave out (`-agentfs`, an `info/exclude` pattern), a `.git`
  name, anything inside a nested repository and anything under an untracked
  directory lives in the tree's untracked area: an ordinary Linux file the view
  merges into the tree, never journaled, gated, landed or in history, so
  secrets, SQLite databases, sessions and logs work through the mount and stay
  out of history. A tracked entry wins its name, and rules decide only for new
  names. A new fork starts with a copy of its upstream's untracked area, taken
  at one instant while the upstream's untracked writes are held; a dropped
  fork's area is removed. A rule tree volumes do not build (`agentfs.<name>`,
  `filter=lfs`) refuses the volume, naming its line. The Rules domain is the
  new pure crate `agentfs-rules`.
- **Tree views: a whole-file replace, and `-agentfs` on tracked paths.**
  `TreeActor::replace_file` makes the atomic save a program makes by renaming
  a finished file over a tracked one in one step: the file keeps its identity
  and its unchanged lines keep theirs, and history never shows the temporary
  name. A tracked file or link that `-agentfs` marks leaves history when the
  volume opens: its bytes, mode and owner move to the tree's untracked area and
  one `Unlink by agentfs` commit records it. The recording engine's operator
  rules use the Rules domain's matcher.
- **Tree views: moves between the tree and the untracked area.** A rename
  from an excluded temporary file onto a recorded name (Python's
  `NamedTemporaryFile` plus `os.replace`, an editor's atomic save) records one
  whole-file save that keeps the file's and its unchanged lines' identities,
  and history never shows the temporary name; a descriptor still open on the
  temporary file answers `ESTALE` to writes. A tracked file or link renamed
  onto an excluded name moves out: its removal is recorded and the area holds
  its bytes. Directories across the two answer `EXDEV`, so `mv` copies. A hard
  link within the untracked area works; `mkfifo` and a unix socket's `bind`
  work and are never recorded; `O_TMPFILE` in an untracked directory answers
  `EOPNOTSUPP`.
- **Tree views: M2 fixes.** A directory's record in history is named by its
  path's blob id, with the path inside, so a clone or checkout of history
  works however deep a directory is. `fsync` and close of a descriptor make
  every record so far durable, a `chmod` or `futimens` of the file included.
  A save no longer moves a file's atime. A sync that would show an upstream
  entry under a directory the fork's undecided change removed fails and waits
  for the upstream's decision instead of hiding the entry. A take adds at most
  2^20 directories.
- **Tree views: forks land directories, metadata and links by the model's
  rules.** A fork's change lands in its upstream, and a sync brings the
  upstream's decisions back, by rules 13 to 15: an entry's kind, mode, owner
  and group are one value, which lands over the value its writer saw; a
  directory is its path, so two trees making one directory agree, and a
  directory's rename moves everything under it as one change; a link is a file
  whose bytes are its target. A change that loses by these rules loses whole,
  and its landing's `Agentfs-Collisions` names the cause and the entry:
  `metadata`, `directory-not-empty` or `directory-gone`. The conformance replay
  runs the rules' scenarios on the library and on real mounts, comparing each
  tree's directories, kinds and metadata and each loss's cause, and reads
  invariant D1 (every entry's parent is a directory) from history.
- **Tree views: owners, links and times.** A view presents each entry's
  recorded owner and group; root can `chown`, an owner can `chgrp`, and the
  kernel lets each user do what the recorded modes and owners allow
  (`default_permissions`). A set-group-ID directory gives new entries its
  group, and new subdirectories keep the bit. `symlink` and `readlink` are
  served, links rename and unlink like files, and opening a link answers
  `ELOOP`; a regular file's `mknod` is a create, any other `EPERM`. `touch`
  and `utimensat` set times that survive a restart and build no commit. The
  mount root starts owned by the process that ran `trees init`, mode 0755: a
  chown through the mount gives it to a workload. `trees mount --owner`
  (`AGENTFS_OWNER`) and the grant's presented owner are removed.
- **Tree views: directories, modes and links.** A tree view serves
  directories: `mkdir`, `rmdir`, files in any directory, and renames across
  directories, a directory moving with everything under it, with POSIX's errors.
  Each entry's kind, permission bits and owner are recorded (`lines/` is
  `[identity, line map, meta]`, each directory a `dirs/` record, the root's
  `dirs/root` in the Root commit), files keep git's modes in history (`100755`
  for an executable, `120000` for a link), and `chmod` through a view is
  recorded. The view shows each entry's recorded mode and times. The library also records symbolic links, owners
  (`set_meta`) and explicit times (`set_times`, no commit); times are each
  tree's own, kept by the journal, never in history. Commits are
  `Agentfs-Format: envelope-2` and the state format `agentfs-trees-11`:
  earlier tree volumes do not open.
- **Tree views: recovery converges after a lost event.** An unkeyed fork with no
  client attached ends at the next judgement (boot, a last detach, a landing),
  so a crash between a client's end and the fork's end, or between a fork's
  create and its first attach, no longer leaves the fork open for good; the
  boot's reconcile releases the writer claim of a fork that dropped before its
  claim was released. The Stop drain ends every client of its launcher, and a
  reconcile that cannot record says `tree ReconcileFailed`. Every event a
  launcher's call causes carries the launcher's pod trailers, and every event by
  `agentfs` the serving process's. The tree e2e scripts bound each wait on a
  mount (a drain that never releases fails within two minutes).
- **Tree views: the volume's lifecycle.** `trees mount` stages the volume
  (`VolumeStaged` on `refs/agentfs/volume`) before it serves. SIGTERM, SIGINT or
  unmounting every view drains it (`VolumeDraining Stop`): no client attaches,
  each client is detached with its open files saved, each view unmounts lazily,
  and once no client is attached and every queue is empty the volume is released
  (`VolumeReleased Keep`) and the process exits 0. A failed builder holds the
  release (the volume stays draining, recording goes on); a stopped actor exits 1.
  Each move is one journal step with its event commit, durable before it returns.
  The recording-failures runbook's section 7 covers a tree mount that will not
  start or stop; the domains' invariants are in docs/library/core.md.
- **Tree views: fork keys, end and drop.** `trees mount --fork-mount DIR` mounts
  a new fork, which ends when its view is unmounted, or with `--fork-key KEY` the
  open fork under that key: a second create under a key returns its fork and
  writes nothing. `trees end --fork-key KEY` ends a keyed fork (exit 75 while a
  mount holds the volume). An ended fork drops once its upstream's queue holds
  nothing of it, no file is open in it and no fork names it: one journal step
  (`Change::Drop`) with its `ForkDropped` row, after which its branch, queue
  entries and checkout are gone while its row chain, collision refs and landed
  saves stay; its writer claim is released (`ClaimReleased`). Ends and drops are
  commits on the fork's chain (`ForkEnded`, `ForkDropped`), judged from the
  journal at boot, after each landing and at each end, so a failing builder holds
  none back (docs/trees.md, "Forks: keys, end and the drop"). `trees fork`,
  `--sandbox`, `--fork`, `--sandbox-writer` and `--sandbox-read-only` are gone:
  use `--fork-mount`, `--fork-key`, `--fork-writer` and `--fork-read-only`.
- **Tree views: clients and sessions.** Each view of `trees mount` is a client of
  its tree, attached through the Clients domain, and every save names it:
  `Agentfs-Writer: <label>#<n>@<launcher>/<tree>` (`root-client#5@cli/root`), its
  number the attach's journal step. A view reaches its tree only through its
  client's session (`agentfs_core::session`): a read-only client's writes answer
  `EROFS` (`--read-only`, `--fork-read-only`), and a session reaches only its
  own open files. A fork has one writer for life: a second read-write mount of it,
  live or after a clean unmount, exits 1. After a kill the next mount rebinds the
  client (same id, the new pod context); a clean unmount detaches it, its open
  files saved and durable first. Attach, rebind and end are commits on
  `refs/agentfs/clients/<tree>` (`Attached`, `Rebound`, `AttachmentEnded`,
  `LastDetached`), durable before they return, never waiting for history. The
  view host is `agentfs_fuse::tree_view::ViewHost` (docs/trees.md, "Clients").
  `--writer` and `--fork-writer` are now client labels: safe names of at most
  64 bytes.
- **Tree views: volume events on record as commits.** A fork's row is a commit
  on its own chain, `refs/agentfs/trees/fork/<n>`, beside the fork: its tree is
  `row.json` (the row with its audit stamps) and its message the event
  (`ForkCreated`, with `Agentfs-Kind`, `Agentfs-By`, `Agentfs-Event` and the
  volume and pod trailers, `Agentfs-Format: row-1`). The fork's branch and its row
  are one journal record, durable before the call returns; the event commit is
  built behind it. A mount's new fork is created through
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
  PersistentVolumeClaims for unprivileged pods on one node, with operator
  profiles per StorageClass and crashed volume hosts restarted. Several
  nodes writing one volume are not supported, by decision.
- **Forked volumes**: a fork of a volume for a sandbox, created, finished
  and published as one change, and delivered by inbox, branch, pull
  request head, command or webhook.
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
  `--no-allow-other`.
- **Testing**: end-to-end CI against real FUSE mounts, git servers over
  HTTP, HTTPS and SSH, LFS servers, an OpenTelemetry collector, NFS, kind
  clusters and csi-sanity; crash injection and power-loss replay on XFS
  and ext4; benchmarks against a budget.
- **Project**: dual MIT and Apache-2.0 licence files, contributing guide,
  security policy, code of conduct, issue and pull request templates, and
  the pixel-art logo in `docs/assets/`.

[Unreleased]: https://github.com/mcurcio/agentfs/commits/main
