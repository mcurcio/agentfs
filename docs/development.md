# Building and testing

How to build agentfs, what CI runs on every change, and how to run the
end-to-end tests yourself.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### The Rust toolchain

`rust-toolchain.toml` at the root pins one Rust release, with rustfmt and
clippy. rustup reads it for every cargo command in the checkout and installs
that release on first use. CI's workflows install it with
`rustup toolchain install`, so a new stable release cannot change a build
until the pin moves. The [Dockerfile](../Dockerfile)'s Rust image is the
same release, pinned by digest, and `ci/dev-image.sh` builds the development
image from it. A test (`rust_toolchain_is_pinned_once`) fails when a
workflow names its own toolchain or the Dockerfile's image is another
release. To move the pin, change the file and the Dockerfile's
`RUST_IMAGE` (tag and digest) together, fix what the new release's lints
find, and rebuild the development image.

### What the build needs

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
- **e2e telemetry** ([`ci/e2e-telemetry.sh`](../ci/e2e-telemetry.sh)): a
  tree volume whose operator settings name a remote and a patch source,
  mounted with `OTEL_*` set: stderr carries a `not built:` line for each,
  and a real OpenTelemetry Collector receives `agentfs.not_built` points
  for `capability=push` and `capability=patches`, with the
  `agentfs.volume` resource attribute.
  Then [`ci/e2e-otlp-transports.sh`](../ci/e2e-otlp-transports.sh) runs
  `agentfs mount ROOT -- true` over http/protobuf, http/json and gRPC,
  plain, over TLS with a CA bundle and a bearer token, and over mutual TLS
  (certificates from [`ci/otel-certs.sh`](../ci/otel-certs.sh)), each
  delivering `agentfs.not_built`, and checks that a wrong token, a missing
  client certificate and an unknown CA get the collector nothing while the
  mount still succeeds.
- **e2e FUSE mount** ([`ci/fuse-tests.sh`](../ci/fuse-tests.sh)): real
  mounts as root running the `#[ignore]`d tests in
  [`crates/agentfs/tests/launcher_mount.rs`](../crates/agentfs/tests/launcher_mount.rs):
  `agentfs mount -- COMMAND` (the command's write recorded, its exit status
  the mount's), the operator settings' `not built:` lines, `probe` on a
  live and a stale view, and a restart after `kill -9` under each
  `--on-stale-mount` policy; and the unit tests that need root (creating
  as the caller, tree views stacked over a stale mount).
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
  front and `patch apply` at their failpoints. The `test (all)` job runs
  both as well.
- **e2e csi-sanity** ([`ci/csi-sanity.sh`](../ci/csi-sanity.sh)): the
  Kubernetes CSI conformance suite csi-test (`csi-sanity` v5.6.0, pinned)
  against `agentfs-csi` with real history services and FUSE
  fronts: every controller and node call, error codes, idempotency and
  cleanup, then no mount, record or volume directory left behind.
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
  unit tests of the example alert rules, and the rules against a tree
  mount's metrics in a real Prometheus, `AgentfsNotBuilt` active for
  settings that name a remote ([`ci/e2e-alerts.sh`](../ci/e2e-alerts.sh),
  [deployment.md](deployment.md#alerts)).

[`.github/workflows/docs.yml`](../.github/workflows/docs.yml) builds this
documentation as a book with [mdBook](https://rust-lang.github.io/mdBook/)
on every push and pull request ([`book.toml`](../book.toml),
`docs/SUMMARY.md`), deploys it to GitHub Pages from `main`
(<https://mcurcio.github.io/agentfs/>), and runs the README's Docker quick
start as written against the published image
([`ci/readme-quickstart.sh`](../ci/readme-quickstart.sh) with `docker`).
Build the book locally with `mdbook build` (output in `target/book`) or
`mdbook serve`.

The e2e scripts run locally too: run the collector with
[`ci/otelcol.yaml`](../ci/otelcol.yaml) for the telemetry scripts, or run
`ci/fuse-tests.sh` and the tree scripts on any Linux host with FUSE and
passwordless sudo. `ci/crash-tests.sh` needs only git without FUSE (it
then skips the mount crash tests). `ci/e2e-kind.sh` and `ci/e2e-csi.sh`
run on any Linux host with docker, kind, kubectl, nginx and fcgiwrap;
their headers list the commands. `ci/csi-sanity.sh` needs FUSE,
passwordless sudo and `csi-sanity`.
`cargo bench -p agentfs-core --bench record_step` needs neither.

The recording engine runs only under the CSI driver
([csi.md](csi.md)), so no shell script drives it through the standalone
`agentfs` launcher: its pushes, patch queue, LFS, several pods and state
areas on NFS are tested by the Rust tests named below and by the CSI jobs
on kind, and tree volumes do not build them yet
([trees](trees.md#operator-settings)).

## What each promise is tested by

Each promise in [guarantees](guarantees.md) has a test. This table maps
them to where they run. The CI jobs are described above.

| Promise | Where it is tested |
|---|---|
| Every change boundary is recorded: editor saves, appends, `truncate`, directory moves, `rm -r`, hard links, mode changes, symlinks, unlink and rename of open files, log rotation | the tree views' e2e scripts (`ci/e2e-trees.sh`, `ci/e2e-trees-ordering.sh`) through real mounts |
| A change is recorded before the call returns: `kill -9` right after `close`, then remount | the `fuse` job; the engine matrix of the `crash` job |
| Nothing is lost or duplicated at any step of record, commit, ref update and push | the `crash` job (`ci/crash-tests.sh`: the engine matrix, the process tests and the mount tests) with one oracle; power loss of tree views in the `phase-1` workflow (`ci/power-loss-trees.sh`) |
| Any user: root and uid 10000 write with normal permission semantics | the `fuse` job; the kind jobs, in a pod |
| The history is invisible: no history, journal, state or credential is reachable through the mount | the `fuse` job; the kind jobs |
| Rules: ignores apply only to new paths, a tracked file stays recorded, tracked rule files cannot turn recording off | the `fuse` job and the unit tests of `rules.rs` |
| Nested repositories: a clone, a bare clone and `git init` at the root behave as documented, and nesting deletes nothing from history | the `fuse` job (`nothing_is_deleted_by_nesting`), through a mount and a front |
| Remotes: interval pushes reach two remotes, and an unreachable remote, a refused push and a non-fast-forward each raise an alarm while the filesystem keeps working | the unit tests of `push.rs`, `cli_push.rs`, the `crash` job's diverged remote, and pushes from CSI volumes in `ci/e2e-csi.sh` and the `f8` workflow |
| Pre-commit checks: a path git's fsck or a remote limit would refuse is not recorded, and raises an alarm | the unit tests of `checks.rs` and the `fuse` job |
| Shared repositories: two volumes push on their own refs without contending, a rival writer is detected as divergence, identities follow the templates | the unit tests of `push.rs`, `crates/agentfs/tests/cli_identity.rs` |
| Recovery: a file and a whole commit are restored, and the restore is recorded | `crates/agentfs/tests/cli_restore.rs` |
| Telemetry: the metrics are exported and move as expected | `cli_telemetry.rs` without a collector; `ci/e2e-telemetry.sh` (a tree mount's `agentfs.not_built`) against a real collector. The export of these counters is not tested, though the behaviour each counts is: `agentfs.history_service.superseded` and `.resyncs`, `agentfs.fuse.create_as_caller_fallbacks` and `.racy_rechecks`, `agentfs.reconcile.records` and `.failures`, `agentfs.nested.gitdir_heuristic` |
| Performance of tree views is recorded for the many-file create and the other release workloads on XFS and ext4 | `ci/e2e-trees-tracer.sh --figures` in the `image` workflow ([performance.md](performance.md#tree-views-release-build)) |
| LFS content is readable through `git clone` plus `git lfs pull` | `crates/agentfs/tests/cli_lfs.rs` |
| The patch queue: a conflicting patch is set aside and the rest go on, the volume mounts, and a crash and replay never applies a patch twice | the unit tests of `agentfs_core::patch`, `cli_patch.rs` |
| Several pods on one node: each pod's changes carry its own committer identity and history keeps every boundary | `history_service.rs`, `ci/e2e-csi.sh` on kind |
| A `ReadWriteMany` claim is refused | `ci/e2e-csi.sh` and `crates/agentfs-csi/tests/grpc.rs` |
| A volume continues on another node as a new volume started from its branch | the `f8` workflow (`ci/e2e-csi-move.sh`, two-worker kind cluster) |
| Forks: a fork publishes one change create-only, a refused publish stays refused, a forked claim in use clones | `fork/tests.rs`, `cli_fork.rs`, `cli_fork_deliver.rs`, `ci/e2e-csi-forks.sh`, `ci/e2e-csi-fork-of-fork.sh` |

Not tested: the CSI driver on hardened node images (Talos) and across node
reboots; two nodes writing one volume, which is not supported; power loss
under the recording engine (power loss is tested through tree views);
the recording engine's pushes over SSH, its LFS transfers against a real
LFS server, its state areas on a real NFS export, and SQLite through a
mount; and reordered writes within one flush epoch, because the replay is
in write order.

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
- `e2e-trees-tracer.sh --figures FS` (workflow `image`, on ext4 and XFS):
  the release figures in
  [performance.md](performance.md#tree-views-release-build), each workload
  against its own deadline in `ci/trees-budget.json`, reporting what it
  reached. A missed bar is a warning, never a failure.
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
- **Moves.** A move to another node is tested on a two-worker kind
  cluster.
- **Runbooks never `git push` a branch.** A test
  (`runbooks_push_only_the_namespace`) holds every `git push` in a runbook
  to refs under `refs/agentfs/`. The one way a branch is replaced is
  `agentfs push --force-with-lease`, by an operator
  ([history-rewrite](runbooks/history-rewrite.md)).
