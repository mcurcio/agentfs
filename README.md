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

The `agentfs` command has `init`, `status`, `mount`, `flush`, `record`,
`push`, `run`, `restore` and `revert`. `mount` mounts a backing tree and runs the commit and
push loops in the same process. `record` records paths of a backing tree
directly, without a mount, and reports the paths the rules skip or the
checks refuse, with the reason. It needs `--root` (or `AGENTFS_ROOT`),
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

Not built yet: LFS and the `fsync` class (phase 2), the patch queue
(phase 3), the CSI driver (phase 4), and asking a running mount to push
now. SSH remotes do not support ssh-agent, passphrase-protected keys or
`@cert-authority` host certificates.

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
serves anything. Flags: `--passthrough auto|on|off`, `--threads`,
`--no-allow-other`, `--unmount-grace`, `--push-grace`.

It needs Linux 5.6+ and `/dev/fuse`, and should run as root for multi-user
volumes (new files are given to their creator: `CAP_CHOWN` and
`CAP_FOWNER`) and for passthrough (`CAP_SYS_ADMIN` outside any user
namespace). The
[operator guide](docs/operator-guide.md) covers requirements, layout,
flags, settings, permissions, what is recorded when, shutdown, crashes and
a Kubernetes sidecar sketch.

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
[pushes that keep failing](docs/runbooks/stuck-pushes.md).

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
| `agentfs.push.unpushed_commits` | gauge | `remote` |
| `agentfs.push.oldest_unpushed_age` | gauge, seconds | `remote` |
| `agentfs.push.failures` | counter | `remote`, `reason` |
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
cargo build -p agentfs --features https      # https:// remotes
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

## CI

Every push runs [`.github/workflows/ci.yml`](.github/workflows/ci.yml):

- **lint:** rustfmt, and clippy with default and all features.
- **test:** the workspace tests, with default and all features. They
  include `restore` and `revert` without a mount
  ([`cli_restore.rs`](crates/agentfs/tests/cli_restore.rs)), and the
  `.gitmodules` checks against the runner's `git` (the parser against
  `git config --list`, the checks against `git fsck`).
- **e2e remotes** ([`ci/e2e-remotes.sh`](ci/e2e-remotes.sh)): pushes to a real
  smart-HTTP server (nginx and `git http-backend`, [`ci/git-http-server.sh`](ci/git-http-server.sh))
  over http and https with basic auth. Covers credential files, auth failures
  that do not block other remotes, credential rotation picked up by a
  running `agentfs run`, divergence, and TLS certificate verification (a
  certificate for another host fails, and a self-signed one fails as
  `untrusted_host`).
- **e2e ssh remotes** ([`ci/e2e-ssh.sh`](ci/e2e-ssh.sh)): pushes to a real
  OpenSSH server ([`ci/git-ssh-server.sh`](ci/git-ssh-server.sh): its own
  sshd on 127.0.0.1:2222, key-only authentication, a `git-shell` user),
  with a binary built with `--features ssh`. Covers Ed25519 OpenSSH and RSA
  PEM keys, plain and hashed known_hosts, a wrong key that fails only its
  own remote, unknown and changed host keys refused before the key is
  offered (checked in the sshd log), a key rotated under a running
  `agentfs run`, a clean SIGTERM, and divergence over SSH.
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
  end, from finding a change to pushing the restore commits.
- **e2e runbooks** ([`ci/e2e-runbooks.sh`](ci/e2e-runbooks.sh)): runs the
  shell blocks of the [divergence](docs/runbooks/divergence.md) and
  [history-rewrite](docs/runbooks/history-rewrite.md) runbooks exactly as
  written (with `git filter-repo`), against a `file://` remote, and checks
  the secret is gone locally and remotely and pushes are fast-forwards
  again.

The e2e scripts run locally too: start the server with
`ci/git-http-server.sh start <dir>` and source its output, run the
collector with [`ci/otelcol.yaml`](ci/otelcol.yaml), or run
`ci/git-ssh-server.sh start <dir>` (it needs passwordless sudo and
`openssh-server`: it creates a system user and runs a root sshd) and
`ci/e2e-ssh.sh` with its output in the environment, or run
`ci/fuse-tests.sh` and `ci/e2e-recovery.sh` on any Linux host with FUSE
and passwordless sudo. `ci/e2e-runbooks.sh` needs only `git-filter-repo`.
