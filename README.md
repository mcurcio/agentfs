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
  kept.
- **Remotes:** several at once, from `[remote "<name>"]` in the history
  repository's own config. Each has its own push state and backoff.
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

The `agentfs` command has `init`, `status`, `mount`, `flush`, `record`,
`push` and `run`. `mount` mounts a backing tree and runs the commit and
push loops in the same process. `record` records paths of a backing tree
directly, without a mount, and reports the paths the rules skip or the
checks refuse, with the reason. It needs `--root` (or `AGENTFS_ROOT`),
since a path missing from the backing tree is recorded as removed. `push`
pushes once; `run` runs the commit and push loops until SIGINT or SIGTERM.
Alarms are printed to stderr, and exported as telemetry when it is enabled.

Only one process uses a state area at a time. `mount`, `run`, `record`,
`flush` and `push` take an exclusive lock on `<state>/meta/lock` and exit
with status 75 if another process holds it, naming the holder;
`--wait-lock 30s` (or `AGENTFS_WAIT_LOCK`) waits instead. `status` takes no
lock and changes nothing, so it works beside a running daemon, and shows
who holds the lock.

Not built yet: restore and revert (§15), LFS and the `fsync` class (phase
2), the patch queue (phase 3), the CSI driver (phase 4), SSH remotes.

## Mounting

```sh
agentfs --state /data/state init --volume demo
agentfs --state /data/state mount /data/backing /workspace
```

The backing tree, the state area and the mount point must be separate
directories, none inside another, and no remote's credentials file may be
under the backing tree; `mount` refuses to start otherwise. It runs in the
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
and plain `http://` remotes work in the default build. Pushing to `https://`
remotes needs the `https` feature, which links OpenSSL:

```sh
cargo build -p agentfs --features https
```

The FUSE layer uses fuser's pure-Rust mount code, so building needs no
libfuse or pkg-config; running a mount needs `/dev/fuse` (and `fusermount3`
from the `fuse3` package for a non-root daemon).

SSH remotes are not supported yet (credentials are username/password or
token only).

## CI

Every push runs [`.github/workflows/ci.yml`](.github/workflows/ci.yml):

- **lint:** rustfmt, and clippy with default and all features.
- **test:** the workspace tests, with default and all features.
- **e2e remotes** ([`ci/e2e-remotes.sh`](ci/e2e-remotes.sh)): pushes to a real
  smart-HTTP server (nginx and `git http-backend`, [`ci/git-http-server.sh`](ci/git-http-server.sh))
  over http and https with basic auth. Covers credential files, auth failures
  that do not block other remotes, credential rotation picked up by a
  running `agentfs run`, divergence, and TLS certificate verification.
- **e2e telemetry** ([`ci/e2e-telemetry.sh`](ci/e2e-telemetry.sh)): every
  metric, span and alarm log above must reach a real OpenTelemetry Collector.
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
  passthrough to the daemon.

The e2e scripts run locally too: start the server with
`ci/git-http-server.sh start <dir>` and source its output, run the
collector with [`ci/otelcol.yaml`](ci/otelcol.yaml), or run
`ci/fuse-tests.sh` on any Linux host with FUSE and passwordless sudo.
