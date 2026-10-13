# agentfs

A filesystem that records every change an agent makes.

agentfs sits in front of an ordinary directory as a FUSE filesystem. Any
process, root or not, uses the directory with normal file calls and never sees
git. Each time a file is closed after a change, or renamed, deleted or linked,
agentfs durably records the new state before the call returns. It then commits
it to a git branch and pushes at regular intervals to one or more git remotes.
The history lives outside the mount, where the agent cannot see or reach it.

Path rules use git's own files (`.gitignore`, `.gitattributes`, `.lfsconfig`,
`$GIT_DIR/info/*`). Nested git repositories are excluded by default.
Telemetry is exported over OpenTelemetry.

It is built in two layers:

1. **Core and daemon**: a Rust library and `agentfs` binary that run anywhere
   FUSE does, with no Kubernetes dependency. The first deployment is a sidecar.
2. **CSI driver**: gives pods agentfs volumes, including volumes several pods
   mount at once.

## Status

| Phase | Scope ([design §1.3](docs/design.md#13-baseline-extras-phases)) | State |
|---|---|---|
| 1 | The recorder as a sidecar for one pod: recording, commits, interval push to any remote, path rules, push checks, OpenTelemetry, recovery | Built; tested end to end in CI, including a kind cluster |
| 2 | Git LFS (§7) and the `fsync` class for databases (§8) | Built |
| 3 | The ordered patch queue for iac (§12) | Built |
| 4 | The CSI driver, with volumes several pods mount at once (§14) | Built for one node per volume; several nodes per volume open (§19.2) |

Performance is benchmarked in CI on XFS and ext4 against a budget; see
[docs/performance.md](docs/performance.md). Not built yet: see
[the end of Features](#not-built-yet). The design was reviewed
adversarially ([review record](docs/reviews/2026-10-12-adversarial.md)).

## Quick start

On Linux 5.6+ with `/dev/fuse`, as root (see [Building](#building)):

```sh
cargo build --release -p agentfs
export PATH=$PWD/target/release:$PATH
mkdir -p /data/backing /workspace
agentfs --state /data/state init --volume demo
git init --bare /data/remote.git
git --git-dir /data/state/git config remote.origin.url /data/remote.git
agentfs --state /data/state mount /data/backing /workspace &
until agentfs probe /workspace >/dev/null; do sleep 0.2; done   # mounted

echo hello > /workspace/notes.txt              # recorded before close returns
sleep 1                                         # commits follow within commitWindow (200ms)
agentfs --state /data/state status              # head, committed seq, backlog
git --git-dir /data/state/git log --stat refs/heads/agentfs/demo
kill %1; wait                                   # final commit and push, unmount
git --git-dir /data/remote.git log --oneline agentfs/demo
```

Stopping the mount prints `final commit:` and a `final push: origin: ...`
line: the last commit is pushed before the mount exits, within
`--push-grace` (5s), so the remote's log shows it straight away. Remotes
are otherwise pushed every `agentfs.pushInterval` (60s), or at once with
`agentfs --state /data/state push`, which asks the running mount to
commit and push now.

Then [Mounting](#mounting) and the [operator guide](docs/operator-guide.md)
for a real deployment, or [Deploying on Kubernetes](#deploying-on-kubernetes).

## Documentation

| Document | For |
|---|---|
| [design.md](docs/design.md) | The specification: what is recorded when, guarantees, decisions and open questions |
| [architecture.md](docs/architecture.md) | Crates, every module, the data flow of a change, the crash model |
| [operator-guide.md](docs/operator-guide.md) | Requirements, layout, flags, settings, permissions, shutdown and crashes |
| [configuration.md](docs/configuration.md) | Every git config key, operator attribute, flag and environment variable (checked against the code in CI) |
| [performance.md](docs/performance.md) | Measured costs, the benchmark and the performance budget |
| [deployment.md](docs/deployment.md) | The container image and the Kubernetes sidecar |
| [csi.md](docs/csi.md) | The CSI driver |
| [runbooks/](docs/runbooks/) | Recovery and undo, divergence, stuck pushes, history rewrite, patch queue, crashed sidecar, history service down, CSI recovery, a failed directory-move reconciliation, worktree paths found by the heuristic |

## Features

Built so far, in `agentfs-core`: path checks, the journal (group commit,
crash recovery), frozen copies (reflink, else copy), the commit builder,
identity templates and settings, and path rules: operator and (optionally)
tree ignore and attribute rules with git's tracked/untracked behaviour, and
nested repository detection. Also:

- **Pre-commit checks:** git's fsck rules and each remote's
  `agentfsMaxBlobSize`, checked at record time and again at commit time.
  A refused path is skipped and raises an alarm; the rest of its record is
  kept. `.gitmodules` content is checked as `git fsck` checks it (submodule
  names, URLs, paths, `update` commands, parse errors). Content scans of
  special files over 1 MiB are left to commit time, so recording stays
  fast whatever their size.
- **Remotes:** several at once, from `[remote "<name>"]` in the history
  repository's own config. Each has its own push state and backoff.
  Local paths and `file://` need nothing; `http://` and `https://` remotes
  authenticate with a token file; SSH remotes with a private key file,
  after their host key is verified against a known_hosts file (unknown or
  changed host keys are refused, never learned). The files are re-read
  when rotated. See the
  [configuration reference](docs/configuration.md).
- **Interval push:** only the volume's own refs, fast-forward only, never
  forced. A divergence saves the local head under
  `refs/agentfs/<volume>/diverged/<timestamp>` and keeps alarming.
- **The service:** the commit loop and one push loop per remote, with clean
  shutdown.
- **Alarm events** (`agentfs_core::events`).
- **Telemetry:** OpenTelemetry metrics, traces and logs (see below).
- **The FUSE layer** (`agentfs-fuse`): a passthrough filesystem over a real
  backing directory that records every change boundary before the call
  returns, with kernel passthrough (Linux 6.9+) when available. See
  [Mounting](#mounting).
- **Recovery:** `agentfs restore` and `agentfs revert`, through a running
  mount or directly, each recorded as one commit, and operator runbooks.
  See [Recovery](#recovery).
- **Git LFS** (phase 2): large files stored as LFS objects, with pointers
  and a maintained `filter=lfs` block in history's `.gitattributes`, so
  `git clone` plus `git lfs pull` gives the content back. Off by default:

  ```sh
  git --git-dir $STATE/git config agentfs.lfsThreshold 8m        # files over 8 MiB
  git --git-dir $STATE/git config remote.origin.lfsurl https://lfs.example.com/acme/r   # optional
  ```

  Objects are uploaded before the commits that reference them are pushed,
  to the endpoint derived from the remote (`<url>.git/info/lfs`) or the
  one configured, with credentials bound to that host. See
  [Git LFS](docs/configuration.md#git-lfs).
- **Databases: the `fsync` class** (phase 2): files changed in place, such
  as SQLite databases, are captured whole (the database with its `-wal` or
  `-journal`, never `-shm`) at `fsync`, at most once per interval, by a
  trailing timer, at the group's last close and at unmount, while writes
  to them are held for the length of the copy (and renames anywhere on the
  mount wait for it). Each capture is one point in time, as a crash would
  leave the files, which SQLite recovers from; CI opens every recorded
  version and runs `PRAGMA integrity_check`. A change is in history at
  most one interval late; if the daemon crashes, the changes since a
  group's last capture are recorded only when the database next changes.
  `restore` and `revert` bring a database's whole group. Opt in from the
  operator's `info/attributes`:

  ```text
  *.db* agentfs.commit=fsync agentfs.group=sqlite
  ```

  The history is repacked with `git repack --geometric=2` every hour
  (`agentfs.repackInterval`, about 256 MiB of memory while it runs), and
  `agentfs.maxSize` caps a path's size.
  See [Databases](docs/operator-guide.md#14-databases-the-fsync-class).
- **The patch queue** (phase 3): producers outside the volume (iac, an
  operator, CI) queue single-commit patches at
  `refs/agentfs/<volume>/inbox/<n>` on the volume's remote, with
  `agentfs patch submit` or plain git. Each mount applies them in order
  before anything sees the tree, each as one commit with the producer as
  author; the first that cannot be applied blocks the queue and raises
  `PatchBlocked`, and the volume mounts anyway. See
  [Patches](#patches).
- **Several mounts of one volume** (phase 4 building blocks, single node):
  `agentfs serve-history` is the volume's history service, the only
  writer of its history, and `agentfs mount --history-service` mounts are
  fronts that hand each change boundary to it over a Unix socket in the
  state area and return once it is durable. Each pod's changes are
  committed with that pod's committer and trailers. See
  [Several mounts of one volume](#several-mounts-of-one-volume).
- **CSI driver** (phase 4, one node per volume): `agentfs-csi` gives pods
  agentfs volumes as PersistentVolumeClaims. The node plugin runs a history
  service per volume and a front per pod, each pod committing as itself;
  settings, rules and credentials are the operator's (StorageClass
  profiles); workloads need no privilege; crashed fronts and services are
  restarted and their mounts recovered. See [docs/csi.md](docs/csi.md).
- **Deployment:** a container image (`ghcr.io/mcurcio/agentfs`), example
  Kubernetes manifests for the phase 1 sidecar and for the CSI driver,
  both tested end to end in a kind cluster. See
  [Deploying on Kubernetes](#deploying-on-kubernetes).

The `agentfs` command has `init`, `status`, `mount`, `probe`, `flush`, `record`,
`push`, `run`, `restore`, `revert`, `patch` and `serve-history`. `mount` mounts a backing tree and runs the commit and
push loops in the same process. `record` records paths of a backing tree
directly, without a mount, and reports the paths the rules skip or the
checks refuse, with the reason; a database in the `fsync` class is
recorded with its whole group. It needs `--root` (or `AGENTFS_ROOT`),
since a path missing from the backing tree is recorded as removed. `push`
pushes once, or, with `mount`, `run` or `serve-history` running, asks it
over its control socket to commit and push now (`--timeout`, 120s); `run`
runs the commit and push loops until SIGINT or SIGTERM.
`restore` and `revert` write content from history back into the tree
([Recovery](#recovery)). `init` is safe to re-run: for the same volume it
exits 0 and changes nothing, for another volume it refuses; `--include
FILE` keeps settings and remotes in an operator-owned config fragment
(`include.path`, for example a ConfigMap; see the
[configuration reference](docs/configuration.md#where-settings-live)).
Alarms are printed to stderr, and exported as telemetry when it is enabled.

Only one process uses a state area at a time. `mount`, `run`,
`serve-history`, `record`, `flush`, and `push`, `restore` and `revert`
when no daemon answers on the control socket, take an
exclusive lock on `<state>/meta/lock` and exit
with status 75 if another process holds it, naming the holder;
`--wait-lock 30s` (or `AGENTFS_WAIT_LOCK`) waits instead. `status` takes no
lock and changes nothing, so it works beside a running daemon, and shows
who holds the lock.

### Not built yet

Several mounts of one volume across nodes. SSH remotes do
not support ssh-agent, passphrase-protected keys or `@cert-authority`
host certificates. LFS has no SSH (`git-lfs-authenticate`), concurrent or
resumable transfers, proxy settings or pruning. Restoring a database needs
its workload stopped
([runbook](docs/runbooks/recovery-and-undo.md#7-restoring-a-database)).
A file written and closed at once is recorded once: when its last
reference goes, the racy-timestamp rule compares it with the copy its
close froze rather than recording it again
([known costs](docs/performance.md#known-costs)).


## Mounting

```sh
agentfs --state /data/state init --volume demo
agentfs --state /data/state mount /data/backing /workspace
```

The backing tree, the state area and the mount point must be separate
directories, none inside another, and no remote's credentials, SSH key or
known_hosts file may be under the backing tree or the mount point; `mount` refuses to start otherwise. It runs in the
foreground until SIGINT, SIGTERM or an unmount, then unmounts (lazily if
files are still open), records what is still open, makes a final commit,
pushes it once more within `--push-grace` and exits. Paths whose record failed and could not be repaired by then are
saved in `<state>/meta/repairs` and recorded by the next mount before it
serves anything. A missing mount point is created. Flags: `--passthrough auto|on|off`, `--threads`,
`--no-allow-other`, `--unmount-grace`, `--push-grace`,
`--on-stale-mount refuse|hold|recover` (a restarted Kubernetes sidecar
mounts over the dead mount its predecessor left, so the workload carries
on without a pod restart; `hold` waits for one instead; a dead mount of
another volume is refused), `--seal-mountpoint` (make the bare mount
point root's with mode 0000 before mounting, so nothing non-root writes
there unrecorded between mounts). `agentfs probe MOUNTPOINT`
exits 0 only for a live agentfs mount, for Kubernetes probes.

It needs Linux 5.6+ and `/dev/fuse`, and should run as root for multi-user
volumes (new files are given to their creator: `CAP_CHOWN` and
`CAP_FOWNER`) and for passthrough (`CAP_SYS_ADMIN` outside any user
namespace). The
[operator guide](docs/operator-guide.md) covers requirements, layout,
flags, settings, permissions, what is recorded when, shutdown and crashes.

## Several mounts of one volume

One volume can be mounted several times on one node, for example by two
pods sharing a backing tree, with one history (design §14). Start the
history service first, then a front per mount:

```sh
agentfs --state /data/state init --volume demo
agentfs --state /data/state --root /data/backing serve-history &
AGENTFS_POD=pod-a agentfs --state /data/state mount \
  --history-service /data/state/meta/history/history.sock /data/backing /mnt/a &
AGENTFS_POD=pod-b agentfs --state /data/state mount \
  --history-service /data/state/meta/history/history.sock /data/backing /mnt/b &
```

The service holds the state-area lock, commits and pushes. A front does
the rules, checks and freeze for its own mount, sends the record over
`meta/history/history.sock`, and returns from `close` once the service has
journaled it. Every commit holds one pod's changes, with that pod's
committer (the identity templates rendered with its pod flags) and
`Agentfs-Pod` trailers; history is in the order the service received
the boundaries, and the tree is last-writer-wins. `agentfs status` shows
the service and every front.

If the service is down, a change through a front fails with `EIO` and
the `HistoryServiceUnavailable` alarm; once it is back the fronts
reconnect and their repairs and sweep record the final state
([runbook](docs/runbooks/history-service-down.md)). On Kubernetes the CSI
driver runs all of this for you, per volume and per pod
([docs/csi.md](docs/csi.md)). Limits: one node only;
`restore` and `revert` run through the service and reach every front at
once; locks are not shared
between mounts, so a database must be written from one of them; patches
are applied only when the service starts with no fronts; the service
serves 1024 connections, about 60 fronts busy at once. A file open in
one mount that another mount renames or removes is recorded under its
new name, or not at all. Without
`--history-service`, `mount` is the single-process mount above. See the
[operator guide](docs/operator-guide.md#16-several-mounts-of-one-volume-history-service).

## Deploying on Kubernetes

The image `ghcr.io/mcurcio/agentfs` (tags: the commit sha, and `latest`
for `main`) has `agentfs` built with the `https` and `ssh` features and
the CSI driver `agentfs-csi`, plus `fuse3`, `git`, `tini` and the CA
certificates. [`deploy/sidecar`](deploy/sidecar)
is a kustomize base that runs it as a privileged native sidecar next to a
workload (phase 1, design §17.1):

```sh
kubectl apply -k deploy/sidecar   # after editing deploy/sidecar/config/ and the Secret
```

The workload sees the volume at `/agentfs/workspace`; the backing tree
and the state area live on a `ReadWriteOncePod` PVC it never sees.
Settings and remotes come from a ConfigMap (`agentfs init --include`),
rules from the same ConfigMap, credentials from a Secret. If the daemon
crashes, the workload's calls fail with `ENOTCONN` (nothing goes
unrecorded) until the kubelet restarts the sidecar, which mounts over the
dead mount and carries on with no manual step; the workload mounts the
`emptyDir` read-only, so writes while no daemon is mounted fail instead of
going unrecorded, and the sidecar seals the bare mount point (root's,
mode 0000) for containers that bind the mount itself through `subPath`.
[docs/deployment.md](docs/deployment.md) explains the layout, the
configuration, operations, the security trade-offs of the privileged
sidecar and the Hermes example ([`deploy/examples/hermes`](deploy/examples/hermes));
[runbooks/crashed-sidecar.md](docs/runbooks/crashed-sidecar.md) covers a
crash.

**Or the CSI driver** (phase 4, design §14.2): [`deploy/csi`](deploy/csi)
installs `agentfs-csi`, and workloads mount an ordinary claim of an agentfs
StorageClass, with no privilege and no agentfs configuration of their own
([`deploy/csi/example`](deploy/csi/example): two replicas on one claim):

```sh
kubectl apply -k deploy/csi           # after editing deploy/csi/profiles and the credentials
kubectl apply -k deploy/csi/example
```

Several pods on one node share a volume, each committing as itself; one
node per volume at a time. Each claim gets its own branch,
`agentfs/<namespace>.<claim>.<pv>`; the service pushes the last pod's
final commit as the volume is unstaged, and `onDelete: delete` never
deletes history a remote lacks. [docs/csi.md](docs/csi.md) covers the
architecture, parameters, recovery, RBAC and Pod Security;
[runbooks/csi-recovery.md](docs/runbooks/csi-recovery.md) the incidents.

## Recovery

Bring files back as they were in a commit, or undo a commit; either way
the change is recorded as one commit (`Agentfs-Trigger: restore`, with
`Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>`), authored by
whoever ran it:

```sh
git --git-dir /data/state/git log --follow --oneline agentfs/demo -- notes/plan.md
agentfs --state /data/state restore notes/plan.md <commit>   # a file, a directory, or `.`
agentfs --state /data/state revert <commit>
```

- **With the volume mounted** the command asks the running mount, over
  an owner-only socket in the state area, to write through the mount: the
  workload sees the files at once. **With nothing running** it takes the
  state-area lock and writes the backing tree (`--root`) itself.
- `restore` follows `git restore --source`: files the commit has under
  the path are written back, files history has there but the commit did
  not are removed, untracked files are never touched. `revert` undoes a
  commit's diff against its parent.
- Every check runs before anything is written. Content history does not
  have yet is never overwritten (not even with `--force`), and paths the
  rules would not record are refused. `--force` only overrides what loses
  nothing: a revert of paths changed again later, and files the workload
  holds open. `--dry-run` prints the plan. Pause the workload for files
  it may be writing: a write it makes in the moment between a path's last
  check and the restore's write is replaced before it is recorded.
- A restore that fails or is killed part way is finished by running it
  again: what the tree already holds is recorded, the rest written.
- Exit status: 0 done or already matching, 1 refused (with the reason and
  a hint) or failed, 75 busy.

Runbooks: [recovery and undo](docs/runbooks/recovery-and-undo.md),
[a diverged remote](docs/runbooks/divergence.md),
[removing a recorded secret](docs/runbooks/history-rewrite.md) (history
rewrite with `git filter-repo`, the one documented force-push), and
[pushes that keep failing](docs/runbooks/stuck-pushes.md),
[the patch queue](docs/runbooks/patch-queue.md),
[the history service is down](docs/runbooks/history-service-down.md),
[a moved directory that could not be reconciled](docs/runbooks/reconcile-failed.md),
and [worktree paths found by the heuristic](docs/runbooks/worktree-heuristic.md).

## Patches

Change a volume's files from outside it: queue a patch, and the next mount
applies it before the workload sees the tree.

```sh
# In the producer's git repository (its remote and credentials are used):
agentfs patch submit --volume demo --put config/app.yaml=./app.yaml -m "Raise the limit"
agentfs patch submit --volume demo HEAD          # a commit whose parent is on agentfs/demo

# On the volume:
agentfs --state /data/state status               # patches:   last applied 3, 0 queued, ok (from origin)
agentfs --state /data/state patch status         # the queue, a block and why
agentfs --state /data/state --root /data/backing patch apply   # with the volume stopped
```

- A patch is one commit with one parent on the volume's branch, at
  `refs/agentfs/<volume>/inbox/<n>` (ten digits). Submitters that race
  for a number take the next one: the ref is created with
  `git push --force-with-lease=<ref>:`.
- The mount fetches the inbox (`agentfs.patchFetchTimeout`, default 30s),
  cherry-picks each patch onto the head through the same checks as
  `restore`, and records it as one commit (`Agentfs-Trigger: patch`,
  `Agentfs-Patch`, `Agentfs-Inbox`). A crash at any point applies nothing
  twice: the branch's trailers decide what is applied.
- The first patch that conflicts (or is otherwise refused) blocks the
  queue; the mount carries on. Fix it with `patch submit --replace N` or
  `--skip N` (both also fill a missing number), then restart.
- The push to the patch remote (`agentfs.patchRemote`) moves
  `applied/<n>`, deletes `inbox/<n>` and updates the status ref
  (`refs/agentfs/<volume>/status`, holding `status.json`).

Runbook: [the patch queue](docs/runbooks/patch-queue.md).

## Telemetry

`mount`, `run`, `record`, `flush`, `push`, `serve-history`, `patch apply`, and
`restore` and `revert` without a mount export OpenTelemetry metrics, traces
and logs over OTLP, configured with the standard `OTEL_*` environment variables.
With no endpoint set, telemetry is off and costs nothing.

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
agentfs run
```

- **Variables:** an endpoint enables export, per signal or for all;
  `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` or `OTEL_SDK_DISABLED=true`
  turn it off. This build speaks OTLP **http/protobuf over plain http**
  only, which suits an in-cluster collector; anything else is reported
  with a `warning: telemetry:` line on stderr and that signal stays off.
  Telemetry problems never stop a command. Every variable agentfs and the
  OpenTelemetry SDK read, including sampling (`OTEL_TRACES_SAMPLER`: on a
  busy volume, sample), is in the
  [configuration reference](docs/configuration.md#environment-variables).
- **Failed exports** (collector down, an HTTP error) print
  `warning: telemetry: <signal> export failed: ...` on stderr, at once and
  then at most once a minute while they keep failing, and a
  `telemetry: <signal> export recovered` line when they work again. Each
  failure is also counted in `agentfs.telemetry.export_failures`.
- **Resource:** `service.name`, `service.version`, `service.instance.id`
  (the replica), `process.pid`, `agentfs.volume`, and `k8s.*` from the pod
  flags (`AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`,
  `AGENTFS_NODE`).
- **Shutdown** flushes telemetry, waiting up to 5s, so a collector that is
  down can add up to 5s to a short command.

Metrics (design §13):

| Metric | Type | Attributes |
|---|---|---|
| `agentfs.record.duration` | histogram, seconds | `trigger` |
| `agentfs.record.failures` | counter | `trigger` |
| `agentfs.journal.backlog` | gauge | |
| `agentfs.commit.count` | counter | |
| `agentfs.commit.failures` | counter | |
| `agentfs.repack.failures` | counter | |
| `agentfs.push.unpushed_commits` | gauge | `remote` |
| `agentfs.push.oldest_unpushed_age` | gauge, seconds | `remote` |
| `agentfs.push.failures` | counter | `remote`, `reason`, `stage` (`git`, `lfs` for LFS uploads, or `queue`) |
| `agentfs.push.diverged` | gauge, 0 or 1 | `remote` |
| `agentfs.remote.misconfigured` | gauge | `remote` |
| `agentfs.precheck.rejected_paths` | counter | `stage`, `reason` |
| `agentfs.patch.queue_depth` | gauge | |
| `agentfs.patch.blocked` | gauge, 0 or 1 | |
| `agentfs.state.bytes` | gauge, bytes | `part` |
| `agentfs.telemetry.export_failures` | counter | `signal` (`traces`, `metrics`, `logs`) |
| `agentfs.front.round_trip` | histogram, seconds | `request` |
| `agentfs.front.service_failures` | counter | `stage` |
| `agentfs.history_service.fronts` | gauge | |
| `agentfs.history_service.superseded` | counter | |
| `agentfs.history_service.resyncs` | counter | |
| `agentfs.fuse.create_as_caller_fallbacks` | counter | |
| `agentfs.fuse.racy_rechecks` | counter | `outcome` |
| `agentfs.reconcile.records` | counter | |
| `agentfs.reconcile.failures` | counter | |
| `agentfs.nested.gitdir_heuristic` | counter | |

Spans are `agentfs.record`, `agentfs.commit` and `agentfs.push`, carrying
journal sequence numbers and commit ids. Every alarm is also a log record
(`agentfs.path_refused`, `agentfs.push_failed`, ...) with its details as
`agentfs.*` attributes and the trace of the operation that raised it.
Alarms are still printed to stderr as well.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

libgit2 is built from source, so a C compiler is needed. Local, `file://`
and plain `http://` remotes work in the default build. Two features add
transports; each links OpenSSL (`libssl-dev` and `pkg-config` on Debian
and Ubuntu):

```sh
cargo build -p agentfs --features https      # https:// remotes and https LFS servers
cargo build -p agentfs --features ssh        # ssh:// and scp-like remotes (libssh2)
cargo build -p agentfs --features https,ssh  # both
```

A binary built without `ssh` reports SSH remotes as misconfigured and
pushes its other remotes. SSH remotes need `agentfsSshKey` (an unencrypted
private key) and `agentfsKnownHosts` (an OpenSSH known_hosts file listing
every host key type the server has); see the
[configuration reference](docs/configuration.md#ssh-remotes).

The FUSE layer uses fuser's pure-Rust mount code, so building needs no
libfuse or pkg-config; running a mount needs `/dev/fuse` (and `fusermount3`
from the `fuse3` package for a non-root daemon).

The container image:

```sh
docker build -t agentfs:dev .                 # https and ssh features, Debian trixie
```

## CI

Every push runs [`.github/workflows/ci.yml`](.github/workflows/ci.yml):

- **lint:** rustfmt, and clippy with default and all features.
- **test:** the workspace tests, with default and all features. They
  include `restore` and `revert` without a mount
  ([`cli_restore.rs`](crates/agentfs/tests/cli_restore.rs)), and the
  `.gitmodules` checks against the runner's `git` (the parser against
  `git config --list`, the checks against `git fsck`), Git LFS through the
  CLI with local and `file://` remotes and `git lfs pull` of a clone
  ([`cli_lfs.rs`](crates/agentfs/tests/cli_lfs.rs)), and the managed
  `.gitattributes` block against `git check-attr`.
- **e2e remotes** ([`ci/e2e-remotes.sh`](ci/e2e-remotes.sh)): pushes to a real
  smart-HTTP server (nginx and `git http-backend`, [`ci/git-http-server.sh`](ci/git-http-server.sh))
  over http and https with basic auth. Covers credential files, auth failures
  that do not block other remotes, credential rotation picked up by a
  running `agentfs run`, divergence, and TLS certificate verification (a
  certificate for another host fails, and a self-signed one fails as
  `untrusted_host`).
- **e2e shared and failing remotes**
  ([`ci/e2e-shared-remote.sh`](ci/e2e-shared-remote.sh),
  [`ci/e2e-remote-failures.sh`](ci/e2e-remote-failures.sh)), through real
  FUSE mounts. Two volumes push every second, and in ten rounds of
  `agentfs push` started together, to one smart-HTTP repository (as
  different users, a slow hook making their pushes overlap, as the server's
  access log shows) and to one local bare repository: no push fails, each
  branch ends at its volume's head, nothing is saved as diverged, and both
  repositories pass `git fsck --strict`. Then one mount with a good remote
  and six failing ones (nothing listening, a server that never answers
  ([`ci/hung-server.sh`](ci/hung-server.sh)), a refusing hook, a wrong
  password, a diverged branch, an untrusted certificate) keeps serving: the
  close latency stays within max(3x a healthy baseline, 50 ms) at p99,
  renames, directories and a `git clone` work, everything reaches history
  and the good remote, `agentfs status` and the alarms name each failure's
  kind, the fixed remotes catch up, and SIGTERM ends the mount within its
  push grace, abandoning the hung push.
- **e2e patches** ([`ci/e2e-patches.sh`](ci/e2e-patches.sh)): the patch
  queue against the same smart-HTTP server and a real FUSE mount, following
  the [runbook](docs/runbooks/patch-queue.md): two producers racing
  `agentfs patch submit` for a number, a conflicting patch that blocks
  the queue while the volume mounts, the queue push (applied refs, inbox
  refs deleted, the status ref), a crash between the commit and the ref
  move, and a replaced patch unblocking it, with the producer as author.
- **e2e ssh remotes** ([`ci/e2e-ssh.sh`](ci/e2e-ssh.sh)): pushes to a real
  OpenSSH server ([`ci/git-ssh-server.sh`](ci/git-ssh-server.sh): its own
  sshd on 127.0.0.1:2222, key-only authentication, a `git-shell` user),
  with a binary built with `--features ssh`. Covers Ed25519 OpenSSH and RSA
  PEM keys, plain and hashed known_hosts, a wrong key that fails only its
  own remote, unknown and changed host keys refused before the key is
  offered (checked in the sshd log), a key rotated under a running
  `agentfs run`, a clean SIGTERM, and divergence over SSH.
- **e2e Git LFS** ([`ci/e2e-lfs.sh`](ci/e2e-lfs.sh)): git-lfs's reference
  `lfs-test-server` (pinned, [`ci/lfs-server.sh`](ci/lfs-server.sh)) behind
  the same nginx, with a separate LFS host that has its own users, and the
  real git-lfs client. Covers pointers and the managed `.gitattributes`
  lines, every upload finishing before the git push (from the nginx log),
  each host seeing only its own credentials, `git clone` plus
  `git lfs pull` giving back the exact content, wrong and missing LFS
  credentials failing at stage `lfs` with no ref pushed, the endpoint
  derived from the git remote, `file://` remotes, https and an untrusted
  certificate, and restores from the cache and from a server.
- **e2e telemetry** ([`ci/e2e-telemetry.sh`](ci/e2e-telemetry.sh)): every
  metric, span and alarm log above must reach a real OpenTelemetry Collector,
  and a dead traces endpoint must produce the stderr warning and
  `agentfs.telemetry.export_failures` over metrics. The failure counters
  come from real failures: an unreadable file (`agentfs.record.failures`),
  an unwritable object store (`agentfs.commit.failures`, the record
  committed later) and a pack directory the repack cannot write
  (`agentfs.repack.failures`). A real `serve-history` with FUSE front
  mounts shows `agentfs.history_service.fronts` going 0, 1, 2, 1, a
  front's `agentfs.front.round_trip`, and its
  `agentfs.front.service_failures` for a hung service (`timeout`) and a
  stopped one (`connect`).
- **e2e FUSE mount** ([`ci/fuse-tests.sh`](ci/fuse-tests.sh)), with
  passthrough `auto`, `on` and `off`: real mounts as root running the `#[ignore]`d
  tests in [`crates/agentfs/tests/mount.rs`](crates/agentfs/tests/mount.rs):
  every operation of design §18 test 1 (editor saves, appends, truncate,
  directory renames, `rm -r`, hard links, `chmod +x`, symlinks, unlink and
  rename of open files, log rotation), `kill -9` right after a close, root
  and uid 10000 writers, invisibility of the state area, rules and nested
  repositories through the mount, SQLite (WAL and DELETE) and git on the
  mount, extended attributes, a `.git` that is no repository, shutdown with
  files open, external unmounts, the lock, and an unprivileged daemon
  (refused under `on`). The job log shows the passthrough mode the runner's
  kernel negotiated, and every mount asserts that no open fell back from
  passthrough to the daemon. The same job runs restores and reverts through a real mount (the
  `mount.rs` recovery tests: in-place rewrites, removals, owners, files
  open for writing, direct mode with the workload stopped) and
  [`ci/e2e-recovery.sh`](ci/e2e-recovery.sh), the recovery runbook end to
  end, from finding a change to pushing the restore commits. It also
  runs SQLite databases in the `fsync` class: the `mount.rs` tests
  restore every recorded version of WAL and DELETE databases with its
  `-wal` or `-journal` and check it, and
  [`ci/e2e-sqlite.sh`](ci/e2e-sqlite.sh) (with
  [`sqlite-writer.py`](ci/sqlite-writer.py) and
  [`sqlite-check.py`](ci/sqlite-check.py)) checks that history catches up
  within one interval while a WAL writer stays open, that captures are
  at least one interval apart (a DELETE-mode writer does not capture
  per transaction), that every version passes `PRAGMA integrity_check`
  with whole transactions, and the database restore runbook, live and
  direct, restoring one member and getting its whole group.
- **e2e multi-pod** ([`ci/e2e-multipod.sh`](ci/e2e-multipod.sh)), with
  passthrough `auto` and `off`: two real FUSE front mounts of one backing
  tree as pods `pod-a` and `pod-b`, feeding one `agentfs serve-history`.
  Covers files from each pod, a file created through one and renamed and
  removed through the other, a directory `mv`, a shared file written in
  turn, every commit's committer equal to its `Agentfs-Pod` trailer, `EIO`
  and the alarm while the service is down and the catch-up after it
  restarts, a `kill -9` of the service right after a close returned, and
  the push. It then measures the per-close cost through a single-process
  mount and through a front ([`boundary-latency.py`](ci/boundary-latency.py))
  and the socket round trip, into the job summary. The `fuse` job also
  runs `two_fronts_one_service` from `mount.rs`.
- **e2e crash injection** ([`ci/crash-tests.sh`](ci/crash-tests.sh),
  design §18 test 3): a build with the `failpoints` feature (test builds
  only; `AGENTFS_FAILPOINTS` names where to kill or fail) runs
  [`crash_points.rs`](crates/agentfs-core/tests/crash_points.rs), a
  workload of writes, a directory move, LFS content, commits and pushes to
  a plain and a diverged remote, killed and failed at the 1st and 3rd hit
  of every engine failpoint (and failed then killed at the next commit),
  each recovered and checked: nothing promised lost, nothing committed
  twice or out of order, no empty commit, journal and `pending/` empty,
  the remote at the head, one saved diverged head at most, and
  `git fsck --full --strict` on every repository.
  [`crash_daemon.rs`](crates/agentfs/tests/crash_daemon.rs) kills
  `record`, `push`, `run`, `serve-history`, a `record --history-service`
  front and `patch apply` at their failpoints, and
  [`crash_mount.rs`](crates/agentfs/tests/crash_mount.rs) a real FUSE
  mount after a recorded close and while saving its repair queue. The
  `test (all)` job runs the first two as well.
- **e2e csi-sanity** ([`ci/csi-sanity.sh`](ci/csi-sanity.sh)): the
  Kubernetes CSI conformance suite csi-test (`csi-sanity` v5.6.0, pinned)
  against `agentfs-csi --mode all` with real history services and FUSE
  fronts: every controller and node call, error codes, idempotency and
  cleanup, then no mount, record or volume directory left behind.
- **e2e runbooks** ([`ci/e2e-runbooks.sh`](ci/e2e-runbooks.sh)): runs the
  shell blocks of the [divergence](docs/runbooks/divergence.md) and
  [history-rewrite](docs/runbooks/history-rewrite.md) runbooks exactly as
  written (with `git filter-repo`), against a `file://` remote, and checks
  the secret is gone locally and remotely and pushes are fast-forwards
  again.
- **docs:** checks the documentation against the code: every git config
  key agentfs reads (`config::KEYS`, filled by a test that records every
  read), every operator attribute, every flag and environment variable of
  `agentfs` and `agentfs-csi` (walking their clap definitions) and every
  OpenTelemetry variable is documented in
  [configuration.md](docs/configuration.md), and nothing documented is
  gone; every module is in [architecture.md](docs/architecture.md)'s map;
  and every relative link and anchor in README.md and docs/ resolves.

[`.github/workflows/image.yml`](.github/workflows/image.yml) builds and
tests the container image on every push:

- **build:** the [`Dockerfile`](Dockerfile) (with the GitHub Actions layer
  cache), a smoke test of the image (`agentfs --version`, `fusermount3`,
  `git`, `agentfs-csi --version`, `tini --version`), and a render of the
  kustomizations in `deploy/sidecar`, `deploy/csi`, `deploy/csi/example`,
  `ci/k8s` and `ci/k8s-csi`. The image is kept as an artifact for the next jobs.
- **e2e kind cluster** ([`ci/e2e-kind.sh`](ci/e2e-kind.sh)): loads that
  image into a kind cluster (kind, kubectl and the node image pinned by
  checksum and digest, [`ci/install-kind.sh`](ci/install-kind.sh)),
  validates `deploy/sidecar` with `kubectl apply --dry-run=server`, and
  deploys its CI overlay [`ci/k8s`](ci/k8s) against the runner's git
  server. It checks root and uid 10000 writers (one through a `subPath` of
  the FUSE mount), operator rules, the invisible state area, authors and
  pod, namespace and node trailers in commits pushed over HTTP with the
  credential Secret, a restore through `kubectl exec`, the final commit
  and final push when the pod is deleted, in-place recovery after `kill
  -9` of the daemon (`ENOTCONN` for the workload, the kubelet restarting
  the sidecar, which stacks over the dead mount, both writers carrying on
  without a restart), `agentfs push` through the running daemon, the
  sealed mount point, the clean-exit fence (`EROFS` for the read-only
  `emptyDir`, `EACCES` for a `subPath` writer restarted in the gap), and a
  pod deletion leaving nothing mounted on the node.
- **e2e CSI driver** ([`ci/e2e-csi.sh`](ci/e2e-csi.sh)): the same image in
  another kind cluster, with [`deploy/csi`](deploy/csi) through its CI
  overlay [`ci/k8s-csi`](ci/k8s-csi). It validates the manifests, checks
  the driver's registration and its RBAC inventory with `kubectl auth
  can-i`, runs two restricted pods (uid 10000 and 10001, primary groups
  other than their shared `fsGroup`) on one claim, has them create the
  same new names at once, checks their distinct committers, trailers and
  authors on the runner's git server, kills one pod's front (the other
  pod unaffected, the front back in its slot and live), the history
  service right after a write, and the node plugin pod, each recovered,
  checks that inline volumes and `ReadWriteMany` are refused, and tears
  down through DeleteVolume.
- **publish** (main only, after both kind tests): pushes the tested image to
  `ghcr.io/mcurcio/agentfs` as `<commit sha>` and `latest`.

[`.github/workflows/bench.yml`](.github/workflows/bench.yml) runs the
benchmark ([`ci/bench.sh`](ci/bench.sh), design §18 test 13) on every
push to main that changes code, and on demand, once on a fresh XFS
(reflink) and once on a fresh ext4 (copy) loop filesystem: the
synchronous record step without FUSE
([`record_step`](crates/agentfs-core/benches/record_step.rs)), then
workloads through a real mount beside a plain-directory baseline (4 KiB
closes, a `git clone` with `nested=working-tree`, npm-style small files
into an excluded `node_modules/`, creating 100,000 files, renaming their
directory (one record) and `rm -rf` of them,
closing a 1 GiB file), each checked against the history afterwards.
Results go to the job summary and a `bench-<fs>` artifact kept 90 days;
[`ci/bench-budget.json`](ci/bench-budget.json) adds warnings, never
failures. See [docs/performance.md](docs/performance.md).

[`.github/workflows/power-loss.yml`](.github/workflows/power-loss.yml)
tests power loss and fsync ordering (design §3.3, §18 test 3) on every
push to main that changes code, and on demand, once on ext4 and once on
XFS: [`ci/power-loss.sh`](ci/power-loss.sh) runs `agentfs record` and
`agentfs push` on a filesystem over dm-log-writes, which logs every write,
flush and FUA, then replays the log with xfstests' `replay-log` (pinned,
[`ci/install-replay-log.sh`](ci/install-replay-log.sh)) and, at every FUA
(every flush with the `exhaustive` input), checks the state a power cut
there leaves on a snapshot ([`ci/power-loss-check.sh`](ci/power-loss-check.sh)):
the filesystem mounts, `agentfs flush` recovers with no backlog, git fsck
passes, sequence numbers increase with no empty commit, and every record
and push whose fsynced promise is on disk is in history and on the
remote. It needs root, loop devices and the `dm-log-writes` and
`dm-snapshot` modules.

The e2e scripts run locally too: start the server with
`ci/git-http-server.sh start <dir>` and source its output, run the
collector with [`ci/otelcol.yaml`](ci/otelcol.yaml), or run
`ci/git-ssh-server.sh start <dir>` (it needs passwordless sudo and
`openssh-server`: it creates a system user and runs a root sshd) and
`ci/e2e-ssh.sh` with its output in the environment, or run
`ci/fuse-tests.sh`, `ci/e2e-recovery.sh` and (with the HTTP server)
`ci/e2e-patches.sh` on any Linux host with FUSE
and passwordless sudo. `ci/crash-tests.sh` needs only git without FUSE
(it then skips the mount crash tests). `ci/e2e-runbooks.sh` needs only `git-filter-repo`.
`ci/e2e-kind.sh` and `ci/e2e-csi.sh` run on any Linux host with docker,
kind, kubectl, nginx and fcgiwrap; their headers list the commands.
`ci/csi-sanity.sh` needs FUSE, passwordless sudo and `csi-sanity`.
`ci/bench.sh dir PATH` benchmarks the filesystem PATH is on with FUSE and
passwordless sudo; `cargo bench -p agentfs-core --bench record_step`
needs neither.
