# Building and testing

How to build agentfs, what CI runs on every change, and how to run the
end-to-end tests yourself.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

libgit2 is built from source, so a C compiler is needed. Local, `file://`
and plain `http://` remotes work in the default build. Two features add
transports: `https` links OpenSSL (`libssl-dev` and `pkg-config` on Debian
and Ubuntu); `ssh` is agentfs's own transport on russh with ring, with no
OpenSSL or libssh2:

```sh
cargo build -p agentfs --features https      # https:// remotes and https LFS servers
cargo build -p agentfs --features ssh        # ssh:// and scp-like remotes (russh)
cargo build -p agentfs --features https,ssh  # both
```

A third, `otlp-tls`, exports telemetry to `https://` collectors (rustls on
ring: no OpenSSL; `ci/check-deps.sh` checks what each feature set links).

A binary built without `ssh` reports SSH remotes as misconfigured and
pushes its other remotes. SSH remotes need `agentfsKnownHosts` (an OpenSSH known_hosts file with the
server's host keys, or `@cert-authority` lines for host certificates) and
either `agentfsSshKey` (a private key, with `agentfsSshKeyPassphrase` if it
is encrypted) or `agentfsSshAgent` (an ssh-agent socket); see the
[configuration reference](configuration.md#ssh-remotes).

The FUSE layer uses fuser's pure-Rust mount code, so building needs no
libfuse or pkg-config; running a mount needs `/dev/fuse` (and `fusermount3`
from the `fuse3` package for a non-root daemon).

Tests only: `cargo test --workspace --features agentfs/failpoints` adds
the crash-injection failpoints (`AGENTFS_FAILPOINTS`
in the [configuration reference](configuration.md)). Never ship such a
build: its `--version` ends in `+failpoints`.

The container image:

```sh
docker build -t agentfs:dev .                 # https, ssh and otlp-tls features, Debian trixie
```

## CI

Every push runs [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

- **lint:** rustfmt, clippy with default and all features, and
  [`ci/check-deps.sh`](../ci/check-deps.sh): what C and crypto each feature
  set links.
- **test:** the workspace tests, with default and all features. They
  include `restore` and `revert` without a mount
  ([`cli_restore.rs`](../crates/agentfs/tests/cli_restore.rs)), and the
  `.gitmodules` checks against the runner's `git` (the parser against
  `git config --list`, the checks against `git fsck`), Git LFS through the
  CLI with local and `file://` remotes and `git lfs pull` of a clone
  ([`cli_lfs.rs`](../crates/agentfs/tests/cli_lfs.rs)), and the managed
  `.gitattributes` block against `git check-attr`.
- **e2e remotes** ([`ci/e2e-remotes.sh`](../ci/e2e-remotes.sh)): pushes to a real
  smart-HTTP server (nginx and `git http-backend`, [`ci/git-http-server.sh`](../ci/git-http-server.sh))
  over http and https with basic auth. Covers credential files, auth failures
  that do not block other remotes, credential rotation picked up by a
  running `agentfs run`, divergence, and TLS certificate verification (a
  certificate for another host fails, and a self-signed one fails as
  `untrusted_host`).
- **e2e shared and failing remotes**
  ([`ci/e2e-shared-remote.sh`](../ci/e2e-shared-remote.sh),
  [`ci/e2e-remote-failures.sh`](../ci/e2e-remote-failures.sh)), through real
  FUSE mounts. Two volumes push every second, and in ten rounds of
  `agentfs push` started together, to one smart-HTTP repository (as
  different users, a slow hook making their pushes overlap, as the server's
  access log shows) and to one local bare repository: no push fails, each
  branch ends at its volume's head, nothing is saved as diverged, and both
  repositories pass `git fsck --strict`. Then one mount with a good remote
  and six failing ones (nothing listening, a server that never answers
  ([`ci/hung-server.sh`](../ci/hung-server.sh)), a refusing hook, a wrong
  password, a diverged branch, an untrusted certificate) keeps serving: the
  close latency stays within max(3x a healthy control's, 50 ms) at p99
  (the control: a volume with only the good remote, mounted alongside,
  its 2,000 closes interleaved with the failing mount's so the runner's
  noise falls on both alike),
  renames, directories and a `git clone` work, everything reaches history
  and the good remote, `agentfs status` and the alarms name each failure's
  kind, the fixed remotes catch up, and SIGTERM ends the mount within its
  push grace, abandoning the hung push.
- **e2e patches** ([`ci/e2e-patches.sh`](../ci/e2e-patches.sh)): the patch
  queue against the same smart-HTTP server and a real FUSE mount, following
  the [runbook](runbooks/patch-queue.md): two producers racing
  `agentfs patch submit` for a number, a conflicting patch held whole
  while the later ones land and the volume mounts, the queue push (applied
  refs, inbox refs deleted, the status ref), a crash between the commit
  and the ref move, and a replaced patch landing, with the producer as
  author.
- **e2e ssh remotes** ([`ci/e2e-ssh.sh`](../ci/e2e-ssh.sh)): pushes to a real
  OpenSSH server ([`ci/git-ssh-server.sh`](../ci/git-ssh-server.sh): its own
  sshd on localhost:2222, key-only authentication, a `git-shell` user,
  host certificates from a CA, and two more sshd with an expired
  certificate and one for another name),
  with a binary built with `--features ssh` (agentfs's russh transport).
  Covers Ed25519 OpenSSH and RSA
  PEM keys, plain and hashed known_hosts, a wrong key that fails only its
  own remote, unknown and changed host keys refused before the key is
  offered (checked in the sshd log), a key rotated under a running
  `agentfs run`, a clean SIGTERM, divergence over SSH, a patch-queue
  inbox fetch (`agentfs patch apply`), host certificates (a CA-only
  known_hosts; an unknown or revoked CA, an expired certificate and a
  wrong principal refused before the key is offered), ssh-agent
  identities (a second identity accepted, an agent of refused keys and a
  dead agent failing as `auth`), and passphrase-protected OpenSSH, PKCS#8
  and RSA PEM keys (missing and wrong passphrases never offering the key,
  and a passphrase rotated under `agentfs run`).
- **e2e Git LFS** ([`ci/e2e-lfs.sh`](../ci/e2e-lfs.sh)): git-lfs's reference
  `lfs-test-server` (pinned, [`ci/lfs-server.sh`](../ci/lfs-server.sh)) behind
  the same nginx, with a separate LFS host that has its own users, and the
  real git-lfs client. Covers pointers and the managed `.gitattributes`
  lines, every upload finishing before the git push (from the nginx log),
  each host seeing only its own credentials, `git clone` plus
  `git lfs pull` giving back the exact content, wrong and missing LFS
  credentials failing at stage `lfs` with no ref pushed, the endpoint
  derived from the git remote, `file://` remotes, https and an untrusted
  certificate, and restores from the cache and from a server. Then
  [`ci/e2e-lfs-transfers.sh`](../ci/e2e-lfs-transfers.sh), with nginx rules
  that throttle or fail chosen objects: concurrent transfers, uploads
  resuming by object and downloads by byte (`Range`, after a `kill -9`),
  the per-remote object size limit, `agentfs lfs fetch` and `prune`, and
  proxies through tinyproxy (https through `CONNECT`).
- **e2e Git LFS over SSH** ([`ci/e2e-lfs-ssh.sh`](../ci/e2e-lfs-ssh.sh)):
  `git-lfs-authenticate` as a git-shell custom command on OpenSSH, with
  the real git-lfs pulling what agentfs pushed, answers that expire per
  batch, refused headers, a server without the command, and an untrusted
  host key with no key offered.
- **e2e telemetry** ([`ci/e2e-telemetry.sh`](../ci/e2e-telemetry.sh)): the
  record, commit, repack, push, precheck, remote, patch, state and journal
  metrics, the record, commit and push spans and the alarm logs must reach
  a real OpenTelemetry Collector,
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
  Then [`ci/e2e-otlp-transports.sh`](../ci/e2e-otlp-transports.sh) exports
  over http/protobuf, http/json and gRPC, plain, over TLS with a CA bundle
  and a bearer token, and over mutual TLS (certificates from
  [`ci/otel-certs.sh`](../ci/otel-certs.sh)), and checks that a wrong token,
  a missing client certificate and an unknown CA get the collector nothing
  while the command succeeds.
- **e2e FUSE mount** ([`ci/fuse-tests.sh`](../ci/fuse-tests.sh)), with
  passthrough `auto`, `on` and `off`: real mounts as root running the `#[ignore]`d
  tests in [`crates/agentfs/tests/mount.rs`](../crates/agentfs/tests/mount.rs):
  every operation of the no-gaps promise (editor saves, appends, truncate,
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
  [`ci/e2e-recovery.sh`](../ci/e2e-recovery.sh), the recovery runbook end to
  end, from finding a change to pushing the restore commits. It also
  runs SQLite databases in the `fsync` class: the `mount.rs` tests
  restore every recorded version of WAL and DELETE databases with its
  `-wal` or `-journal` and check it, and
  [`ci/e2e-sqlite.sh`](../ci/e2e-sqlite.sh) (with
  [`sqlite-writer.py`](../ci/sqlite-writer.py) and
  [`sqlite-check.py`](../ci/sqlite-check.py)) checks that history catches up
  within one interval while a WAL writer stays open, that captures are
  at least one interval apart (a DELETE-mode writer does not capture
  per transaction), that every version passes `PRAGMA integrity_check`
  with whole transactions, and the database restore runbook, live and
  direct, restoring one member and getting its whole group.
- **e2e multi-pod** ([`ci/e2e-multipod.sh`](../ci/e2e-multipod.sh)), with
  passthrough `auto` and `off`: two real FUSE front mounts of one backing
  tree as pods `pod-a` and `pod-b`, feeding one `agentfs serve-history`.
  Covers files from each pod, a file created through one and renamed and
  removed through the other, a directory `mv`, a shared file written in
  turn, every commit's committer equal to its `Agentfs-Pod` trailer, `EIO`
  and the alarm while the service is down and the catch-up after it
  restarts, a `kill -9` of the service right after a close returned, and
  the push. It then measures the per-close cost through a single-process
  mount and through a front ([`boundary-latency.py`](../ci/boundary-latency.py))
  and the socket round trip, into the job summary. The `fuse` job also
  runs `two_fronts_one_service` from `mount.rs`.
- **e2e crash injection** ([`ci/crash-tests.sh`](../ci/crash-tests.sh)): a build with the `failpoints` feature (test builds
  only; `AGENTFS_FAILPOINTS` names where to kill or fail) runs
  [`crash_points.rs`](../crates/agentfs-core/tests/crash_points.rs), a
  workload of writes, a directory move, LFS content, commits and pushes to
  a plain and a diverged remote, killed and failed at the 1st and 3rd hit
  of every engine failpoint (and failed then killed at the next commit),
  each recovered and checked: nothing promised lost, nothing committed
  twice or out of order, no empty commit, journal and `pending/` empty,
  the remote at the head, one saved diverged head at most, and
  `git fsck --full --strict` on every repository.
  [`crash_daemon.rs`](../crates/agentfs/tests/crash_daemon.rs) kills
  `record`, `push`, `run`, `serve-history`, a `record --history-service`
  front and `patch apply` at their failpoints, and
  [`crash_mount.rs`](../crates/agentfs/tests/crash_mount.rs) a real FUSE
  mount after a recorded close and while saving its repair queue. The
  `test (all)` job runs the first two as well.
- **e2e csi-sanity** ([`ci/csi-sanity.sh`](../ci/csi-sanity.sh)): the
  Kubernetes CSI conformance suite csi-test (`csi-sanity` v5.6.0, pinned)
  against `agentfs-csi` with real history services and FUSE
  fronts: every controller and node call, error codes, idempotency and
  cleanup, then no mount, record or volume directory left behind.
- **e2e runbooks** ([`ci/e2e-runbooks.sh`](../ci/e2e-runbooks.sh)): runs the
  shell blocks of the [divergence](runbooks/divergence.md),
  [history-rewrite](runbooks/history-rewrite.md) and
  [LFS storage](runbooks/lfs-storage.md) runbooks exactly as written
  (with `git filter-repo`), against a `file://` remote, and checks the
  secret is gone locally and remotely and pushes are fast-forwards again;
  a second step runs the history rewrite with forks.
- **e2e NFS** ([`ci/e2e-nfs.sh`](../ci/e2e-nfs.sh)), with passthrough
  `auto` and `off`: one NFS export mounted by two NFSv4 clients on the
  runner. The state lock and its holder line hold across both clients,
  NFS 4.0 is refused, a front over the export records mmap writes and
  SQLite commits, `.nfs` names never reach history, and a CSI volume
  moves between two node plugins, refused on the second while staged on
  the first.
- **docs:** checks the documentation against the code: every git config
  key agentfs reads (`config::KEYS`, filled by a test that records every
  read), every operator attribute, every flag and environment variable of
  `agentfs` and `agentfs-csi` (walking their clap definitions) and every
  OpenTelemetry variable is documented in
  [configuration.md](configuration.md), and nothing documented is
  gone; every module is in [architecture.md](architecture.md)'s map;
  and every relative link and anchor in README.md and docs/ resolves.
- **README quick start** (in the `fuse` job,
  [`ci/readme-quickstart.sh`](../ci/readme-quickstart.sh)): the README's
  "From source" block, run as written, as root; the remote must hold the
  final commit after the mount stops.

[`.github/workflows/image.yml`](../.github/workflows/image.yml) builds and
tests the container image on every push:

- **build:** the [`Dockerfile`](../Dockerfile) (with the GitHub Actions layer
  cache), a smoke test of the image (`agentfs --version`, `fusermount3`,
  `git`, `agentfs-csi --version`, `tini --version`), and a render of the
  kustomizations in `deploy/sidecar`, `deploy/csi`, `deploy/csi/example`,
  `ci/k8s` and `ci/k8s-csi`. The image is kept as an artifact for the next jobs.
- **e2e kind cluster** ([`ci/e2e-kind.sh`](../ci/e2e-kind.sh)): loads that
  image into a kind cluster (kind, kubectl and the node image pinned by
  checksum and digest, [`ci/install-kind.sh`](../ci/install-kind.sh)),
  validates `deploy/sidecar` with `kubectl apply --dry-run=server`, and
  deploys its CI overlay [`ci/k8s`](../ci/k8s) against the runner's git
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
- **e2e CSI driver** ([`ci/e2e-csi.sh`](../ci/e2e-csi.sh)): the same image in
  another kind cluster, with [`deploy/csi`](../deploy/csi) through its CI
  overlay [`ci/k8s-csi`](../ci/k8s-csi). It validates the manifests, checks
  the driver's registration and its RBAC inventory with `kubectl auth
  can-i` and the provisioner admission policy, runs two restricted pods (uid 10000 and 10001, primary groups
  other than their shared `fsGroup`) on one claim, has them create the
  same new names at once, checks their distinct committers, trailers and
  authors on the runner's git server, kills one pod's front (the other
  pod unaffected, the front back in its slot and live), the history
  service right after a write, and the node plugin pod, each recovered,
  checks that inline volumes and `ReadWriteMany` are refused, and tears
  down through DeleteVolume.
- **e2e forked volumes** ([`ci/e2e-csi-forks.sh`](../ci/e2e-csi-forks.sh)):
  forks on the CSI driver in kind: a sandbox's claim cloned from a claim
  in use, its pushes reaching only the fork remote, the admission policy,
  publishing after an outage and a node plugin restart, a refused
  publish, and a change submitted twice applied once
  ([csi.md](csi.md#14-forks)).
- **publish** (main only, after the three kind tests): pushes the tested image to
  `ghcr.io/mcurcio/agentfs` as `<commit sha>` and `latest`.

Four more workflows test what ships beside the binary:

- [`.github/workflows/docker.yml`](../.github/workflows/docker.yml), on
  pushes that touch the image, `examples/docker`, `ci/hermes` or
  `deploy/examples/hermes`: each Docker example in its own job
  ([docker.md](docker.md)), and upstream Hermes on the compose example
  ([deployment.md](deployment.md#how-it-is-tested)).
- [`.github/workflows/hermes.yml`](../.github/workflows/hermes.yml), on
  pushes that touch the image, the Hermes suite, `deploy/sidecar`,
  `deploy/csi` or `docs/deployment.md`: upstream Hermes on kind, as the
  sidecar pod, its read-only fence alternative and on a CSI volume, one
  job each ([deployment.md](deployment.md#how-it-is-tested)).
- [`.github/workflows/helm.yml`](../.github/workflows/helm.yml), on every
  push: the charts linted and checked against `deploy/`, the CSI chart
  installed on kind and put through `ci/e2e-csi.sh`, and the charts
  published from `main` ([helm.md](helm.md#6-how-it-is-tested)).
- [`.github/workflows/alerts.yml`](../.github/workflows/alerts.yml), on
  pushes that touch the rules or the telemetry code: promtool checks and
  unit tests of the example alert rules, and the rules against the
  binary's metrics in a real Prometheus
  ([deployment.md](deployment.md#alerts)).

[`.github/workflows/docs.yml`](../.github/workflows/docs.yml) builds this
documentation as a book with [mdBook](https://rust-lang.github.io/mdBook/)
on every push and pull request ([`book.toml`](../book.toml),
`docs/SUMMARY.md`), deploys it to GitHub Pages from `main`
(<https://mcurcio.github.io/agentfs/>), and runs the README's Docker quick
start as written against the published image
([`ci/readme-quickstart.sh`](../ci/readme-quickstart.sh) with `docker`).
Build the book locally with `mdbook build` (output in `target/book`) or
`mdbook serve`.

[`.github/workflows/bench.yml`](../.github/workflows/bench.yml) runs the
benchmark ([`ci/bench.sh`](../ci/bench.sh)) on every
push to main that changes code, and on demand, once on a fresh XFS
(reflink) and once on a fresh ext4 (copy) loop filesystem: the
synchronous record step without FUSE
([`record_step`](../crates/agentfs-core/benches/record_step.rs)), then
workloads through a real mount beside a plain-directory baseline (4 KiB
closes, a `git clone` with `nested=working-tree`, npm-style small files
into an excluded `node_modules/`, creating 100,000 files, renaming their
directory (one record) and `rm -rf` of them,
closing a 1 GiB file), each checked against the history afterwards.
Each filesystem runs as two parallel shards (the 100,000-file workloads,
and the rest), reported together in one summary; their outputs are
`bench-<fs>-<shard>` artifacts kept 90 days. The workflow runs after a
change lands and does not hold it;
[`ci/bench-budget.json`](../ci/bench-budget.json) adds warnings, never
failures. See [docs/performance.md](performance.md).

[`.github/workflows/power-loss.yml`](../.github/workflows/power-loss.yml)
tests power loss and fsync ordering (see [guarantees](guarantees.md)) on every
push to main that changes code, and on demand, once on ext4 and once on
XFS: [`ci/power-loss.sh`](../ci/power-loss.sh) runs `agentfs record` and
`agentfs push` on a filesystem over dm-log-writes, which logs every write,
flush and FUA, then replays the log with xfstests' `replay-log` (pinned,
[`ci/install-replay-log.sh`](../ci/install-replay-log.sh)) and, at every FUA
(every flush with the `exhaustive` input), checks the state a power cut
there leaves on a snapshot ([`ci/power-loss-check.sh`](../ci/power-loss-check.sh)):
the filesystem mounts, `agentfs flush` recovers with no backlog, git fsck
passes, sequence numbers increase with no empty commit, and every record
and push whose fsynced promise is on disk is in history and on the
remote. It needs root, loop devices and the `dm-log-writes` and
`dm-snapshot` modules.

The e2e scripts run locally too: start the server with
`ci/git-http-server.sh start <dir>` and source its output, run the
collector with [`ci/otelcol.yaml`](../ci/otelcol.yaml), or run
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

## What each promise is tested by

Each promise in [guarantees](guarantees.md) has a test. This table maps
them to where they run. The CI jobs are described above.

| Promise | Where it is tested |
|---|---|
| Every change boundary is recorded: editor saves, appends, `truncate`, directory moves, `rm -r`, hard links, mode changes, symlinks, unlink and rename of open files, log rotation | the `fuse` job, through real mounts (`crates/agentfs/tests/mount.rs`, as root, with passthrough `auto`, `on` and `off`) |
| A change is recorded before the call returns: `kill -9` right after `close`, then remount | the `fuse` job; the engine matrix of the `crash` job |
| Nothing is lost or duplicated at any step of record, commit, ref update and push | the `crash` job (`ci/crash-tests.sh`: the engine matrix, the process tests and the mount tests) with one oracle; power loss in the `power-loss` workflow |
| Any user: root and uid 10000 write with normal permission semantics | the `fuse` job; the kind jobs, in a pod |
| The history is invisible: no history, journal, state or credential is reachable through the mount | the `fuse` job; the kind jobs |
| Rules: ignores apply only to new paths, a tracked file stays recorded, tracked rule files cannot turn recording off | the `fuse` job and the unit tests of `rules.rs` |
| Nested repositories: a clone, a bare clone and `git init` at the root behave as documented, and nesting deletes nothing from history | the `fuse` job (`nothing_is_deleted_by_nesting`), through a mount and a front |
| Remotes: interval pushes reach two remotes, and an unreachable remote, a refused push and a non-fast-forward each raise an alarm while the filesystem keeps working | `e2e-remotes` (smart HTTP and HTTPS), `e2e-ssh`, and `e2e-shared-remotes` (`ci/e2e-remote-failures.sh`) through a live mount |
| Pre-commit checks: a path git's fsck or a remote limit would refuse is not recorded, and raises an alarm | the unit tests of `checks.rs` and the `fuse` job |
| Shared repositories: two volumes push on their own refs without contending, a rival writer is detected as divergence, identities follow the templates | `ci/e2e-shared-remote.sh`, the unit tests of `push.rs`, `crates/agentfs/tests/cli_identity.rs` |
| Recovery: a file and a whole commit are restored, and the restore is recorded | `ci/e2e-recovery.sh`, `crates/agentfs/tests/cli_restore.rs`, the `e2e-runbooks` job |
| Telemetry: the metrics are exported and move as expected | `ci/e2e-telemetry.sh` against a real collector and `cli_telemetry.rs` without one. The export of these counters is not tested, though the behaviour each counts is: `agentfs.history_service.superseded` and `.resyncs`, `agentfs.fuse.create_as_caller_fallbacks` and `.racy_rechecks`, `agentfs.reconcile.records` and `.failures`, `agentfs.nested.gitdir_heuristic` |
| Performance is recorded for a `git clone`, 30,000 small files into an excluded path, `rm -rf` of 100,000 files and closing a 1 GiB file on XFS and ext4 | the `bench` workflow ([performance.md](performance.md)) |
| Databases: SQLite in WAL and DELETE mode restores every version and passes `PRAGMA integrity_check`; LFS content is readable through `git clone` plus `git lfs pull` | the `fuse` job, `ci/e2e-sqlite.sh`, the `e2e-lfs` jobs |
| The patch queue: a conflicting patch is set aside and the rest go on, the volume mounts, and a crash and replay never applies a patch twice | the unit tests of `agentfs_core::patch`, `cli_patch.rs`, `ci/e2e-patches.sh` |
| Several pods on one node: each pod's changes carry its own committer identity and history keeps every boundary | `ci/e2e-multipod.sh`, `history_service.rs`, `ci/e2e-csi.sh` on kind |
| A `ReadWriteMany` claim is refused | `ci/e2e-csi.sh` and `crates/agentfs-csi/tests/grpc.rs` |
| The state lock, its holder line and the mount-option check hold across two NFS clients of one export | `ci/e2e-nfs.sh` |
| A volume continues on another node as a new volume started from its branch | the `f8` workflow (`ci/e2e-csi-move.sh`, two-worker kind cluster) |
| Forks: a fork publishes one change create-only, a refused publish stays refused, a forked claim in use clones | `fork/tests.rs`, `cli_fork.rs`, `cli_fork_deliver.rs`, `ci/e2e-csi-forks.sh`, `ci/e2e-csi-fork-of-fork.sh` |

Not tested: the CSI driver on hardened node images (Talos) and across node
reboots; two nodes writing one volume, which is not supported; a power cut
through a FUSE mount (power loss is tested through the CLI's `record`);
and reordered writes within one flush epoch, because the replay is in
write order.

The `fuse` job also runs the README's from-source quick start as written,
as root (`ci/readme-quickstart.sh`): its remote log after `kill %1; wait`
must show the edited file, which only the final push at shutdown puts
there. The `docs` workflow runs the README's Docker quick start as written
against the published image. The README's Kubernetes quick starts are not
run as written; the manifests and charts they use are tested by the
`image` and `helm` workflows.

## Tree-view tests

Tests of tree views ([trees](trees.md)) use real repositories but local
`tree_actor`, `tree_git` and replay tests are not FUSE or power-loss
evidence. CI scripts judge the build:

- `e2e-trees.sh` and `e2e-trees-ordering.sh` (workflow `ci`): recording,
  objects, the anonymous first link, immediate reopen.
- `e2e-tree-conformance.sh`: fixed model cases and byte calls against a
  plain Linux file.
- `phase1-replay` and `e2e-tree-replay.sh`: the model's in-scope scenarios
  and seeds replayed on a fresh model, the library and real mounts,
  comparing each tree, each collision's lost lines and each take's head,
  with invariants read from git. `--record` writes what the system did with
  each trace, a trace the model replays: each mount's recording replays on a
  fresh model, and a recorded regression is a fixture in
  `crates/agentfs-conformance/tests/regressions`
  ([VERIFYING.md](../crates/agentfs-model/VERIFYING.md)).
- `e2e-tree-pagecache.sh`: shared mappings in 21 cases, `trees-mmap`, 64
  closes reopened.
- `e2e-tree-restart.sh`, `e2e-tree-recording-faults.sh` and
  `e2e-tree-enospc.sh`: view-host death, kills at each failpoint, state
  exhaustion.
- `power-loss-trees.sh`: dm-log-writes replay on ext4 and XFS, on the
  upstream and a fork, each checkpoint also rebuilding the refs from the
  journal alone.
- `measure-trees.sh`: the measurements in [performance.md](performance.md#tree-views),
  which are not budgets. A pushed commit message containing
  `[phase1-measure]` runs them all, `[phase1-held]`, `[phase1-calls]` and
  `[phase1-pilot]` one mode each, and `[phase1-seeds]` the library at
  seeds 1 to 500.

## Runners, images and runbooks

- **Runners are pinned.** Every CI job runs on GitHub's runner image named
  `ubuntu-24.04`, never `ubuntu-latest`, so the kernel, FUSE, kind and nginx
  the tests rely on change only when the pin does.
  `crates/agentfs-core/tests/docs_reference.rs` fails on any other runner.
- **The image is published after the kind jobs pass.** The `image`
  workflow publishes to `ghcr.io/mcurcio/agentfs` as the commit sha and
  `latest`, only from main and only after both kind end-to-end jobs
  pass with that image.
- **Kind jobs run a real git server.** The example git server runs as a
  pod, so pushes from pods reach a real remote.
- **LFS is tested against a real server.** The `e2e-lfs` jobs run git-lfs's
  `lfs-test-server` behind nginx with the real git-lfs client.
- **NFS and moves.** `ci/e2e-nfs.sh` mounts one NFS export on two clients
  to check the state lock, the mount-option check, silly-rename names and
  a CSI volume refused on a second node plugin while staged on the first.
  A move to another node is tested on a two-worker kind cluster.
- **Runbooks never `git push` a branch.** A test
  (`runbooks_push_only_the_namespace`) holds every `git push` in a runbook
  to refs under `refs/agentfs/`. The one way a branch is replaced is
  `agentfs push --force-with-lease`, by an operator
  ([history-rewrite](runbooks/history-rewrite.md)).
