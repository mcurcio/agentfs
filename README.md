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

Phase 1 in progress. See [docs/design.md](docs/design.md) and the
[review record](docs/reviews/2026-10-12-adversarial.md).

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
- **Deployment:** a container image (`ghcr.io/mcurcio/agentfs`) and
  example Kubernetes manifests for the phase 1 sidecar, tested end to end
  in a kind cluster. See
  [Deploying on Kubernetes](#deploying-on-kubernetes).

The `agentfs` command has `init`, `status`, `mount`, `probe`, `flush`, `record`,
`push`, `run`, `restore`, `revert` and `patch`. `mount` mounts a backing tree and runs the commit and
push loops in the same process. `record` records paths of a backing tree
directly, without a mount, and reports the paths the rules skip or the
checks refuse, with the reason; a database in the `fsync` class is
recorded with its whole group. It needs `--root` (or `AGENTFS_ROOT`),
since a path missing from the backing tree is recorded as removed. `push`
pushes once; `run` runs the commit and push loops until SIGINT or SIGTERM.
`restore` and `revert` write content from history back into the tree
([Recovery](#recovery)). `init` is safe to re-run: for the same volume it
exits 0 and changes nothing, for another volume it refuses; `--include
FILE` keeps settings and remotes in an operator-owned config fragment
(`include.path`, for example a ConfigMap; see the
[configuration reference](docs/configuration.md#where-settings-live)).
Alarms are printed to stderr, and exported as telemetry when it is enabled.

Only one process uses a state area at a time. `mount`, `run`, `record`,
`flush`, `push`, and `restore` and `revert` when no mount runs, take an
exclusive lock on `<state>/meta/lock` and exit
with status 75 if another process holds it, naming the holder;
`--wait-lock 30s` (or `AGENTFS_WAIT_LOCK`) waits instead. `status` takes no
lock and changes nothing, so it works beside a running daemon, and shows
who holds the lock.

Not built yet: the CSI driver (phase 4), asking a running mount to push now, and
restarting a pod automatically after its sidecar crashes. SSH remotes do
not support ssh-agent, passphrase-protected keys or `@cert-authority`
host certificates. LFS has no SSH (`git-lfs-authenticate`), concurrent or
resumable transfers, proxy settings or pruning. Restoring a database needs
its workload stopped
([runbook](docs/runbooks/recovery-and-undo.md#7-restoring-a-database)).

## Mounting

```sh
agentfs --state /data/state init --volume demo
agentfs --state /data/state mount /data/backing /workspace
```

The backing tree, the state area and the mount point must be separate
directories, none inside another, and no remote's credentials, SSH key or
known_hosts file may be under the backing tree or the mount point; `mount` refuses to start otherwise. It runs in the
foreground until SIGINT, SIGTERM or an unmount, then unmounts (lazily if
files are still open), records what is still open, makes a final commit
and exits. Paths whose record failed and could not be repaired by then are
saved in `<state>/meta/repairs` and recorded by the next mount before it
serves anything. A missing mount point is created. Flags: `--passthrough auto|on|off`, `--threads`,
`--no-allow-other`, `--unmount-grace`, `--push-grace`,
`--on-stale-mount refuse|hold` (a restarted Kubernetes sidecar holds
instead of remounting over a dead mount). `agentfs probe MOUNTPOINT`
exits 0 only for a live agentfs mount, for Kubernetes probes.

It needs Linux 5.6+ and `/dev/fuse`, and should run as root for multi-user
volumes (new files are given to their creator: `CAP_CHOWN` and
`CAP_FOWNER`) and for passthrough (`CAP_SYS_ADMIN` outside any user
namespace). The
[operator guide](docs/operator-guide.md) covers requirements, layout,
flags, settings, permissions, what is recorded when, shutdown and crashes.

## Deploying on Kubernetes

The image `ghcr.io/mcurcio/agentfs` (tags: the commit sha, and `latest`
for `main`) has `agentfs` built with the `https` and `ssh` features, plus
`fuse3`, `git` and the CA certificates. [`deploy/sidecar`](deploy/sidecar)
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
unrecorded), the pod turns NotReady, and deleting the pod recovers.
[docs/deployment.md](docs/deployment.md) explains the layout, the
configuration, operations, the security trade-offs of the privileged
sidecar and the Hermes example ([`deploy/examples/hermes`](deploy/examples/hermes));
[runbooks/crashed-sidecar.md](docs/runbooks/crashed-sidecar.md) covers a
crash.

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
[pushes that keep failing](docs/runbooks/stuck-pushes.md), and
[the patch queue](docs/runbooks/patch-queue.md).

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

`mount`, `flush`, `record`, `push` and `run` export OpenTelemetry metrics, traces and
logs over OTLP, configured with the standard `OTEL_*` environment variables.
With no endpoint set, telemetry is off and costs nothing.

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
agentfs run
```

- **Endpoints:** `OTEL_EXPORTER_OTLP_ENDPOINT` enables every signal, at
  `/v1/traces`, `/v1/metrics` and `/v1/logs` under it. A signal-specific
  `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT` (a full URL) enables
  only that signal. `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` turns one off;
  `OTEL_SDK_DISABLED=true` turns everything off.
- **Transport:** this build speaks OTLP **http/protobuf over plain http**
  only, which suits an in-cluster collector. gRPC, `http/json` and `https`
  endpoints are not built: they are reported with a `warning: telemetry:`
  line on stderr and that signal stays off. Telemetry problems never stop a
  command.
- **Failed exports** (collector down, an HTTP error) print
  `warning: telemetry: <signal> export failed: ...` on stderr, at once and
  then at most once a minute while they keep failing, and a
  `telemetry: <signal> export recovered` line when they work again. Each
  failure is also counted in `agentfs.telemetry.export_failures`.
- **Other settings**, read by the OpenTelemetry SDK:
  `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_EXPORTER_OTLP_TIMEOUT` (and their
  per-signal forms), `OTEL_SERVICE_NAME` (default `agentfs`),
  `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_METRIC_EXPORT_INTERVAL` (default 60s),
  and the batch settings `OTEL_BSP_*` and `OTEL_BLRP_*`.
- **Sampling:** every record makes an `agentfs.record` span. On a busy
  volume, sample: `OTEL_TRACES_SAMPLER=parentbased_traceidratio` with
  `OTEL_TRACES_SAMPLER_ARG=0.01`. Metrics and alarm logs are not sampled.
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
| `agentfs.push.failures` | counter | `remote`, `reason`, `stage` (`git`, or `lfs` for LFS uploads) |
| `agentfs.push.diverged` | gauge, 0 or 1 | `remote` |
| `agentfs.remote.misconfigured` | gauge | `remote` |
| `agentfs.precheck.rejected_paths` | counter | `stage`, `reason` |
| `agentfs.state.bytes` | gauge, bytes | `part` |
| `agentfs.telemetry.export_failures` | counter | `signal` (`traces`, `metrics`, `logs`) |

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
  `agentfs.telemetry.export_failures` over metrics.
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
- **e2e runbooks** ([`ci/e2e-runbooks.sh`](ci/e2e-runbooks.sh)): runs the
  shell blocks of the [divergence](docs/runbooks/divergence.md) and
  [history-rewrite](docs/runbooks/history-rewrite.md) runbooks exactly as
  written (with `git filter-repo`), against a `file://` remote, and checks
  the secret is gone locally and remotely and pushes are fast-forwards
  again.

[`.github/workflows/image.yml`](.github/workflows/image.yml) builds and
tests the container image on every push:

- **build:** the [`Dockerfile`](Dockerfile) (with the GitHub Actions layer
  cache), a smoke test of the image (`agentfs --version`, `fusermount3`,
  `git`), and a render of the kustomizations in `deploy/sidecar` and
  `ci/k8s`. The image is kept as an artifact for the next jobs.
- **e2e kind cluster** ([`ci/e2e-kind.sh`](ci/e2e-kind.sh)): loads that
  image into a kind cluster (kind, kubectl and the node image pinned by
  checksum and digest, [`ci/install-kind.sh`](ci/install-kind.sh)),
  validates `deploy/sidecar` with `kubectl apply --dry-run=server`, and
  deploys its CI overlay [`ci/k8s`](ci/k8s) against the runner's git
  server. It checks root and uid 10000 writers (one through a `subPath` of
  the FUSE mount), operator rules, the invisible state area, authors and
  pod, namespace and node trailers in commits pushed over HTTP with the
  credential Secret, a restore through `kubectl exec`, the final commit
  when the pod is deleted, and recovery after `kill -9` of the daemon:
  `ENOTCONN` for the workload, a holding sidecar and a NotReady pod,
  `kubectl delete pod` completing, and the new pod pushing what was
  written before the kill.
- **publish** (main only, after the kind test): pushes the tested image to
  `ghcr.io/mcurcio/agentfs` as `<commit sha>` and `latest`.

The e2e scripts run locally too: start the server with
`ci/git-http-server.sh start <dir>` and source its output, run the
collector with [`ci/otelcol.yaml`](ci/otelcol.yaml), or run
`ci/git-ssh-server.sh start <dir>` (it needs passwordless sudo and
`openssh-server`: it creates a system user and runs a root sshd) and
`ci/e2e-ssh.sh` with its output in the environment, or run
`ci/fuse-tests.sh`, `ci/e2e-recovery.sh` and (with the HTTP server)
`ci/e2e-patches.sh` on any Linux host with FUSE
and passwordless sudo. `ci/e2e-runbooks.sh` needs only `git-filter-repo`.
`ci/e2e-kind.sh` runs on any Linux host with docker, kind, kubectl, nginx
and fcgiwrap; its header lists the commands.
