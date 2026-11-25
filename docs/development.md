# Building and testing

How to build agentfs, what CI runs on every change, and how to run the
end-to-end tests yourself.

## Building

```sh
cargo nextest run --workspace
cargo test --doc --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Tests run under [cargo-nextest](https://nexte.st), one process per test
and as many at once as there are CPUs; [Running tests](#running-tests)
says how.

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
cargo build -p agentfs-cli --features https      # https:// remotes and https LFS servers
cargo build -p agentfs-cli --features ssh        # ssh:// and scp-like remotes (russh)
cargo build -p agentfs-cli --features https,ssh  # both
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

Tests only: `cargo nextest run --workspace --features agentfs-cli/failpoints` adds
the crash-injection failpoints (`AGENTFS_FAILPOINTS`
in the [configuration reference](configuration.md)). Never ship such a
build: its `--version` ends in `+failpoints`.

Beside it, `agentfs-engine`'s `test-util` feature makes the engine's
test-only surface `pub` ([engine](library/engine.md#test-builds)). It is off
by default and the library crate `agentfs` never turns it on. The engine's
own tests get it through a dev-dependency on the engine itself, the FUSE
layer's and the binary's tests through their dev-dependencies, the binary's
`failpoints` build through that feature, and `agentfs-conformance` through
its dependency. A workspace-wide build therefore has it on: build the
library alone, as a consumer does, with
`cargo clippy -p agentfs-engine -p agentfs-service -p agentfs --lib` and
`cargo test --doc -p agentfs`.

The container image:

```sh
docker build -t agentfs:dev .                 # https, ssh and otlp-tls features, Debian trixie
```

### Running tests

`cargo nextest run` runs every test in a process of its own, several at
once, so a test never shares globals, an allocator or counters with
another. nextest does not run doctests: `cargo test --doc` does, for a
package with a library (`agentfs-cli` is a binary only and has none). The
library crate `agentfs` has no tests but its doctest, so its gate is
`cargo test --doc -p agentfs`: `cargo nextest run -p agentfs` fails with
exit status 4, "no tests to run". AGENTS.md
gives the pinned nextest release and how to install it.

[`.config/nextest.toml`](../.config/nextest.toml) holds two profiles. Both
retry nothing, because a failure is a broken test or a bug, and both name a
test that runs past a minute, every minute, and stop it at 15 minutes.
`default` is the one a local run uses. `ci`, which the workflows select
with `NEXTEST_PROFILE: ci`, also runs on after the first failure and prints
each failure when it happens and again at the end.

Three slow tree tests, `tree_batch_alloc` and two `tree_stages` layout
tests, build smaller volumes locally than in CI; each test's comment gives
its sizes and says what the local ones do not catch.
`AGENTFS_TEST_VOLUMES` picks the sizes: `small` or `full`. Unset, it is
`small` locally and `full` on GitHub Actions (`GITHUB_ACTIONS=true`), so
no workflow has to set it, and on Actions `small` fails the test: CI
always runs the full sizes. Any other value fails the test too. At the
full sizes `tree_batch_alloc` took 128 s to 160 s on the runners, the
slowest test of ci.yml's test job, and the two `tree_stages` tests 42 s to
78 s. To run CI's sizes locally:

```sh
AGENTFS_TEST_VOLUMES=full cargo nextest run -p agentfs-engine --test tree_batch_alloc --test tree_stages
```

`cargo test --no-run` is still used where a script builds a test program
and then runs it itself: `ci/fuse-tests.sh` runs it under sudo with
libtest's flags, and `ci/model-digest.sh` and `ci/replay-seeds.sh` run one
process per seed. Each says so in a comment.

### Test programs

Cargo links one program per file in a crate's `tests/`. To link fewer,
`agentfs-engine`, `agentfs-core` and `agentfs-cli` put the integration tests
that can share a process in one program each, a module per file:
`tests/it/` and `tests/cli/`. Run one file's tests by its module:

```sh
cargo nextest run -p agentfs-engine --test it tree_actor::
cargo nextest run -p agentfs-cli --test cli cli_fork::
```

A test file stays a program of its own when it needs the process to
itself or CI runs it by name: `tree_stages` and `tree_close` read
process-wide counters, `tree_batch_alloc` and `tree_overlay_alloc` set
their own allocator, `config_reference`, `crash_points`, `crash_daemon`,
`launcher_mount`, `launcher_host` and `csi_trees` are named by CI, and
`history_service` keeps its timings in a program of its own. Its 5 s
deadlines hold with the other tests running beside it. A
test that runs another test of its program in a child process names it with
`test_name!`, which adds the module path that `--exact` needs.

### Build settings

The workspace's `Cargo.toml` sets the development and test profile:

- **Line tables only** (`debug = "line-tables-only"`) for the workspace's
  crates, so a panic's backtrace still names file and line, and **no debug
  info** for dependencies. A debugger shows no local variables in such a
  build. Release builds are unchanged.
- **Dependencies stay unoptimised.** `opt-level = 1` for dependencies was
  measured and not taken: the cold build took 400 s instead of 232 s, and
  `agentfs-core`'s tests ran in 216 s instead of 217 s.

The first test build of a package (`cargo nextest run -p <crate>`) after a
`cargo build --workspace` compiles
some dependencies again (88 s for `agentfs-core` below): a package alone
turns on fewer features of tokio and others than the workspace does. Both
builds stay in the target directory, so this is paid once.

Each worktree has its own target directory. Don't share one build
directory (`CARGO_BUILD_BUILD_DIR`) between worktrees of this repository:
the hash cargo files a workspace crate's build under does not include the
checkout's path, so two worktrees build into the same place. One worktree's test then
runs the other's code, and `CARGO_BIN_EXE_agentfs` names the other
worktree's binary.

Measured on 2026-11-24 in a container with 4 shared CPUs and 16 GB, at
`-j 2`, one build at a time, each column from an empty target directory:

| Step | Before | Line tables only | And merged test programs |
|---|---|---|---|
| `cargo build --workspace --all-targets`, cold | 296 s | 232 s | 217 s |
| Then `cargo test -p agentfs-core --no-run` | 154 s | 97 s | 88 s |
| Target directory after these | 26 GB | 9.3 GB | 6.6 GB |
| Test programs in the workspace | 70 | 70 | 34 |
| `cargo test -p agentfs-core`, run only | 233 s | 217 s | 218 s |
| One edit in `tree_actor/recording/`, then `cargo test -p agentfs-core --no-run` | 27 s | 16 s | 9 s |
| One edit in `volume.rs`, the same | 29 s | 15 s | 9 s |
| `cargo test -p agentfs-cli`, its build and run | 143 s | 101 s | 85 s |
| Of which the run | 32 s | 25 s | 16 s |
| Target directory at the end | 33 GB | 12 GB | 8.0 GB |

The table above ran the tests with `cargo test`, one program after
another. The table below compares that with nextest, measured on
2026-11-24 in the same kind of container (4 CPUs, 15 GB), at `-j 2`, one
build at a time. Each column has its own worktree and target directory.
Its test programs were built first, so only the run is timed. "Before" is
`cargo test`, with `tree_batch_alloc` on volumes of 2,000 and 20,000
files and two `tree_stages` tests on 10,000 files. "After" is nextest,
with those volumes a tenth of the size:

| Step | Before | After |
|---|---|---|
| `agentfs-core`'s tests, run only | 196 s | 77 s |
| Of which: the tests (`cargo test` before, `cargo nextest run` after) | 196 s | 75 s |
| Of which: the doctests (`cargo test --doc`, none today) | in the above | 2 s |
| `tree_batch_alloc`, its one test | 65 s | 30 s alone, 32 s in the full run |
| `tree_stages`, its 15 tests | 57 s | 4.4 s alone |
| `agentfs-core`'s unit tests | 46 s | beside the rest |
| `agentfs-cli`'s tests, run only (no doctests: binary only) | 15 s | 17 s |
| `agentfs-core`'s test programs built from an empty target directory | 128 s | 119 s |
| `agentfs-cli`'s, the same | 86 s | 88 s |

No profile setting changed between the two columns, so the build times
differ by noise only. nextest alone, on the "Before" tests, ran
`agentfs-core` in 128 s: `tree_batch_alloc` took 81 s and each of the two
10,000-file `tree_stages` tests 42 s, beside the other tests. Smaller
volumes took the run to 75 s. Under nextest a package's run lasts at least
as long as its slowest test. For the tree engine (in `agentfs-core` then,
`agentfs-engine` now) that is `tree_batch_alloc` (about 30 s locally),
which builds a 10,000-file volume and must have its process to itself; CI
runs it at full size (above), and its run lasts about as long. `agentfs-cli`'s run gains nothing: most of its tests are in one program
(`tests/cli/`), which already ran them several at a time (14 s), and its
slowest test takes about 5 s.

The two engines are two packages, and neither depends on the other, so an
edit in one rebuilds and runs only its own tests. Measured on 2026-11-25 in
a container with 4 CPUs and 15 GB, at cargo's default jobs, with every test
program built first, after touching one file:

| Step | Build | Run | Tests |
|---|---|---|---|
| `tree_actor/recording/mod.rs`, then `cargo nextest run -p agentfs-engine` | 25 s | 64 s | 475 |
| `volume.rs`, then `cargo nextest run -p agentfs-core` | 23 s | 20 s | 761 |

The tree engine's run is bound by its four CPUs more than by its slowest
test, `tree_batch_alloc` (about 33 s).

The checks over the whole repository (`agentfs-repo`) build no engine: its
library depends only on serde and serde_json. Its checks of the crates
(`tests/repo/crates.rs`): dependencies follow the
[ranks](architecture.md#ranks); the crate `agentfs` only re-exports the
ranks it uses; only `agentfs-service`, the crate `agentfs` and the tests
depend on the domains the service composes; every `failpoints` feature is
forwarded; every cargo command the repository writes down names what the
workspace has; every failpoint hit is listed; and the tree engine starts
`git` only through `counted`. Measured on 2026-11-25 in a
container with 4 CPUs and 15 GB:

| Step | Time |
|---|---|
| `cargo nextest run -p agentfs-repo`, from an empty target directory | 6.9 s (8.8 s at `--build-jobs 2`) |
| The same again, nothing changed | 1.1 s |
| After an edit to a page under `docs/` | 1.0 s |
| Of which the run, its 29 tests | 0.6 s |

## CI

Every push runs [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):

- **lint:** rustfmt, clippy with default and all features, and
  [`ci/check-deps.sh`](../ci/check-deps.sh): what C and crypto each feature
  set of the binary links, and that the library crate `agentfs` links no
  recording engine, FUSE, OpenTelemetry SDK, TLS or SSH and turns on no
  `test-util` or `failpoints` feature. Then the library as a consumer builds
  it, without `test-util`: clippy of `agentfs-engine`, `agentfs-service`
  and `agentfs`'s libraries, and `agentfs`'s doctest.
- **test:** the workspace tests under nextest's `ci` profile, then the
  doctests with `cargo test --doc`, with default and all features. They
  include `restore` and `revert` without a mount
  ([`cli_restore.rs`](../crates/agentfs-cli/tests/cli/cli_restore.rs)), and the
  `.gitmodules` checks against the runner's `git` (the parser against
  `git config --list`, the checks against `git fsck`), Git LFS through the
  CLI with local and `file://` remotes and `git lfs pull` of a clone
  ([`cli_lfs.rs`](../crates/agentfs-cli/tests/cli/cli_lfs.rs)), and the managed
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
  [`crates/agentfs-cli/tests/launcher_mount.rs`](../crates/agentfs-cli/tests/launcher_mount.rs):
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
  [`crash_daemon.rs`](../crates/agentfs-cli/tests/crash_daemon.rs) kills
  `record`, `push`, `run`, `serve-history`, a `record --history-service`
  front and `patch apply` at their failpoints. The `test (all)` job runs
  both as well.
- **e2e csi-sanity** ([`ci/csi-sanity.sh`](../ci/csi-sanity.sh)): the
  Kubernetes CSI conformance suite csi-test (`csi-sanity` v5.6.0, pinned)
  against `agentfs-csi` with real `agentfs init` and `agentfs host`
  children and FUSE views: every controller and node call, error codes,
  idempotency and cleanup, then no agentfs mount, published record or host
  left behind, and every deleted volume kept.
- **docs:** checks the documentation against the code: every git config
  key agentfs reads (`config::KEYS`, filled by a test that records every
  read), every operator attribute, every flag and environment variable of
  `agentfs-cli` and `agentfs-csi` (walking their clap definitions) and every
  OpenTelemetry variable is documented in
  [configuration.md](configuration.md), and nothing documented is
  gone; every module is in [architecture.md](architecture.md)'s map;
  and every relative link and anchor in README.md and docs/ resolves.
- **README quick start** (in the `fuse` job,
  [`ci/readme-quickstart.sh`](../ci/readme-quickstart.sh)): the README's
  "From source" block, run as written, as root; the state area's
  `history.git` must hold `notes.txt` on the `root` branch, and nothing may
  be left mounted, after the mount's command exits.

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
  can-i` and the provisioner admission policy, runs a restricted writer
  (uid 10000, primary group other than its `fsGroup`) on a view of its
  volume's one `agentfs host`, checks its files and pod trailers in the
  volume's history and that the git server gets no ref (push is logged as
  not built), refuses a second pod, kills the host and deletes the node
  plugin pod, each recovered with a new write recorded, checks that inline
  volumes, `ReadWriteMany`, `onDelete: delete` and `startFrom` are
  refused, and tears down through DeleteVolume, which keeps the volume.
- **e2e fork claims** ([`ci/e2e-csi-forks.sh`](../ci/e2e-csi-forks.sh)):
  fork claims on the CSI driver in kind: a fork claim of a claim in use,
  served in its source's volume, its pod's write landing in the source
  and seen by the source's pod, the admission policy, `link: unlinked`
  and `landing: approval` refused, and deleting the fork claim ending the
  fork ([csi.md](csi.md#14-forks)).
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
start as written against the image built from the commit
([`ci/readme-quickstart.sh`](../ci/readme-quickstart.sh) with `docker`, and
`AGENTFS_IMAGE` set to that image), so a change to the commands the README
uses passes before an image that understands them is published. The
published image comes from the same `Dockerfile`; `image.yml` publishes it
only after its e2e jobs pass.
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

No launcher runs the recording engine any more, so no shell script
drives it: its pushes, patch queue, LFS, several pods and state areas on
NFS are tested by the Rust tests named below, and tree volumes do not
build them yet ([trees](trees.md#operator-settings)).

## What each promise is tested by

Each promise in [guarantees](guarantees.md) has a test. This table maps
them to where they run. The CI jobs are described above.

| Promise | Where it is tested |
|---|---|
| Every change boundary is recorded: editor saves, appends, `truncate`, directory moves, `rm -r`, hard links, mode changes, symlinks, unlink and rename of open files, log rotation | the tree views' e2e scripts (`ci/e2e-trees.sh`, `ci/e2e-trees-ordering.sh`) through real mounts |
| A change is recorded before the call returns: `kill -9` right after `close`, then remount | `ci/e2e-trees.sh` (SIGKILL mid-write, then remount) and step 3 of `ci/e2e-trees-tracer.sh` in the `image` workflow; for the recording engine, the engine matrix of the `crash` job |
| Nothing is lost or duplicated at any step of record, commit, ref update and push | the `crash` job (`ci/crash-tests.sh`: the engine matrix, the process tests and the mount tests) with one oracle; power loss of tree views in the `phase-1` workflow (`ci/power-loss-trees.sh`) |
| Any user: root and uid 10000 write with normal permission semantics | `ci/e2e-trees-tracer.sh` and the Docker examples (uid 10000 in a container); the kind jobs, in a pod |
| The history is invisible: no history, journal, state or credential is reachable through the mount | the kind jobs and the Docker examples, where only the agentfs container mounts the state area; `launcher_mount.rs` (a mount point inside the state area is refused) |
| Rules: ignores apply only to new paths, a tracked file stays recorded, tracked rule files cannot turn recording off | the unit tests of `agentfs-rules` and `rules.rs`; `ci/e2e-trees.sh`, with operator rules in `info/exclude` |
| Nested repositories: a clone, a bare clone and `git init` at the root behave as documented, and nesting deletes nothing from history | the unit tests of `agentfs-rules` (names inside a nested repository stay untracked); not tested through a mount or a front |
| Remotes: interval pushes reach two remotes, and an unreachable remote, a refused push and a non-fast-forward each raise an alarm while the filesystem keeps working | the unit tests of `push.rs`, `cli_push.rs`, the `crash` job's diverged remote |
| Pre-commit checks: a path git's fsck or a remote limit would refuse is not recorded, and raises an alarm | the unit tests of `checks.rs` (the recording engine) |
| Shared repositories: two volumes push on their own refs without contending, a rival writer is detected as divergence, identities follow the templates | the unit tests of `push.rs`, `crates/agentfs-cli/tests/cli/cli_identity.rs` |
| Recovery: a file and a whole commit are restored, and the restore is recorded | `crates/agentfs-cli/tests/cli/cli_restore.rs` |
| Telemetry: the metrics are exported and move as expected | `cli_telemetry.rs` without a collector; `ci/e2e-telemetry.sh` (a tree mount's `agentfs.not_built`) against a real collector. The export of these counters is not tested, though the behaviour each counts is: `agentfs.history_service.superseded` and `.resyncs`, `agentfs.fuse.create_as_caller_fallbacks` and `.racy_rechecks`, `agentfs.reconcile.records` and `.failures`, `agentfs.nested.gitdir_heuristic` |
| Performance of tree views is recorded for the many-file create and the other release workloads on XFS and ext4 | `ci/e2e-trees-tracer.sh --figures` in the `image` workflow ([performance.md](performance.md#tree-views-release-build)) |
| LFS content is readable through `git clone` plus `git lfs pull` | `crates/agentfs-cli/tests/cli/cli_lfs.rs` |
| The patch queue: a conflicting patch is set aside and the rest go on, the volume mounts, and a crash and replay never applies a patch twice | the unit tests of `agentfs_core::patch`, `cli_patch.rs` |
| Several pods on one node (the recording engine): each pod's changes carry its own committer identity and history keeps every boundary | `history_service.rs` |
| A CSI claim serves one pod, its saves carry that pod's trailers, a crashed host comes back live | `crates/agentfs-cli/tests/csi_trees.rs`, `ci/e2e-csi.sh` on kind |
| A `ReadWriteMany` claim, `onDelete: delete` and `startFrom` are refused | `ci/e2e-csi.sh`, `ci/e2e-csi-move.sh` and the unit tests of `agentfs-csi` |
| Forks: a recording fork publishes one change create-only and a refused publish stays refused; a CSI fork claim of a claim in use lands in its source, and a fork of a fork claim is refused | `fork/tests.rs`, `cli_fork.rs`, `cli_fork_deliver.rs`, `ci/e2e-csi-forks.sh`, `ci/e2e-csi-fork-of-fork.sh` |

Not tested: the CSI driver on hardened node images (Talos) and across node
reboots; two nodes writing one volume, which is not supported; power loss
under the recording engine (power loss is tested through tree views);
the recording engine's pushes over SSH, its LFS transfers against a real
LFS server, its state areas on a real NFS export, and SQLite through a
mount; and reordered writes within one flush epoch, because the replay is
in write order.

The `fuse` job also runs the README's from-source quick start as written,
as root (`ci/readme-quickstart.sh`): the `root` branch of the state area's
`history.git` must hold the edited file once the mount's command exits.
The `docs` workflow runs the README's Docker quick start as written
against the image built from the commit. The README's Kubernetes quick starts are not
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
  closes reopened, and pending saves (`trees-close-open.py`): a stat, an open
  and a rename after a slow close, and history's save then delete.
- `e2e-tree-restart.sh`, `e2e-tree-recording-faults.sh` and
  `e2e-tree-enospc.sh`: view-host death, kills at each failpoint, state
  exhaustion.
- `e2e-tree-pending-saves.sh` (job `recording-faults`): a save worker's
  decision failing, panicking or killing agentfs at failpoint
  `tree.close_decide`, on a failpoints build ([trees](trees.md#pending-saves)).
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
  `agentfs-repo`'s `workflows_pin_runners` fails on any other runner.
- **CI pulls Docker Hub through mirror.gcr.io**, Google's pull-through
  cache, because anonymous pulls from shared runners hit Docker Hub's rate
  limit. The mirror's one home is
  [`ci/docker/daemon.json`](../ci/docker/daemon.json).
  [`ci/docker-hub-mirror.sh`](../ci/docker-hub-mirror.sh) applies it to each
  daemon that pulls: the runner's dockerd and the buildx builder's BuildKit
  through the [`docker-hub-mirror`](../.github/actions/docker-hub-mirror/action.yml)
  action, every kind node's containerd through
  [`ci/kind-cluster.sh`](../ci/kind-cluster.sh), and the docker-in-docker
  daemons of `ci/hermes` by mounting the file. Images stay pinned by digest,
  and each daemon falls back to Docker Hub when the mirror fails. A test
  (`workflows_pull_docker_hub_through_the_mirror`) refuses a workflow that
  sets up buildx or creates a kind cluster around these helpers, or a job
  that pulls without the action. The examples and charts users run keep
  Docker Hub as their default.
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
- **A script's cleanup runs only in its own shell.** Every shell script
  under `ci/` registers its EXIT cleanup with `on_exit` from `ci/lib.sh`
  (`ci/tree-git-test-lib.sh` sources it), never a bare `trap ... EXIT`.
  Under bash 5.2 a forked child (a `( ... ) &` subshell, or `cmd &` before
  it execs) that gets a fatal signal before it resets its traps runs the
  EXIT trap it inherited, so a script's cleanup could run in that child and
  unmount or kill what the script still used. `on_exit` compares the exiting
  shell's `$BASHPID` with the one that registered it (`$$` is the parent's
  inside a subshell), so the cleanup runs once, in its own shell, and a
  subshell may register its own. A test
  (`ci_exit_traps_run_in_their_own_shell`) refuses a bare EXIT trap under
  `ci/`. A killed mount is killed with `tree_kill_mount`, which fails loudly
  when the daemon ended before the kill, exits other than by the kill, or
  left no mount entry, and `tree_wait_exit`'s message and status are never
  hidden.
