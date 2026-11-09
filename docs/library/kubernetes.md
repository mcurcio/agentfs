# The Kubernetes adapter: `agentfs-csi`

`agentfs-csi` is a CSI driver. It lets a cluster hand an agentfs volume to a
pod the way it hands out any other volume: a StorageClass, a claim, a mount.
It is the third integration layer. It adds no recording of its own. For every
long-running piece it starts the `agentfs` binary from the [filesystem
layer](filesystem.md), and for the volume lifecycle it uses
[`agentfs-core`](core.md).

Use this page when you build on the driver as a library, test against fakes of
its process and mount operations, or need to know exactly what each CSI call
does. Operators install and configure the driver with [CSI](../csi.md) and the
[Helm chart](../helm.md). A pod that wants the filesystem without the driver
can run the privileged sidecar of [deployment](../deployment.md) instead; that
needs no code from this layer.

## What the driver offers

The crate `agentfs-csi` is a library and a binary of the same name. The binary
is the node plugin. One runs on every node, in a DaemonSet, beside
csi-provisioner in `--node-deployment` mode, node-driver-registrar and
livenessprobe. It serves all three CSI services (Identity, Controller and
Node) on one Unix socket, with the CSI v1.13 protocol.

Each claim is a **worktree**: a directory of files and its own git branch
`agentfs/<volume>`, on the disk of the node that made it, for good. Any number
of pods on that node share it. Every multi-node access mode is refused.
Continuing a volume on another node is a new volume started from the old
branch ([moving runbook](../runbooks/moving.md)).

- There is no controller Deployment and no attach step. The driver advertises
  `VOLUME_ACCESSIBILITY_CONSTRAINTS`. NodeGetInfo and CreateVolume answer the
  node's topology, `topology.agentfs.mcurcio.github.io/node = <node>`, so the
  PV's node affinity keeps every pod of the claim on that node. Use
  `WaitForFirstConsumer` so the scheduler picks the node first.
- `CSIDriver.attachRequired` is `false`. ControllerPublish and
  ControllerUnpublish are not offered.
- The driver name is `agentfs.mcurcio.github.io` (`agentfs_csi::DRIVER_NAME`).
- It supports persistent volumes only (`volumeLifecycleModes: [Persistent]`).
  Inline ephemeral volumes are refused (`EPHEMERAL_REFUSED`), since their
  parameters would be the pod author's.
- It supports mount volumes with a single-node access mode
  (`SINGLE_NODE_WRITER`, `SINGLE_NODE_SINGLE_WRITER`,
  `SINGLE_NODE_MULTI_WRITER`, `SINGLE_NODE_READER_ONLY`), with no mount flags.
  A block volume, a mount flag or a multi-node mode is `INVALID_ARGUMENT`.
- It advertises `CLONE_VOLUME` and clones only into a fork class. Any other
  content source, and every snapshot, is `INVALID_ARGUMENT`.

| Module | Offers |
|---|---|
| `csi` | The vendored CSI v1.13 protocol, generated at build time |
| `identity` | `IdentityService` and the topology key |
| `controller` | `ControllerService`: CreateVolume and DeleteVolume on this node |
| `node` | `NodeService`: validates a call and hands it to the supervisor |
| `supervisor` | `Supervisor`: the per-volume service, the per-pod fronts, restarts, recovery |
| `release_scan` | `ReleaseScan`: pushes and finishes worktrees that no pod stages |
| `launcher` | `Launcher` and `AgentfsLauncher`: spawns the `agentfs` binary |
| `mounter` | `Mounter` and `KernelMounter`: bind mounts onto the kubelet's targets |
| `params` | StorageClass and PV parameters, `VolumeParams`, `ForkClass` |
| `layout` | `Layout`: where the driver keeps things |
| `records` | `VolumeRecord`, `StagedRecord`, `PublishedRecord`: written atomically |
| `volume` | Creating, adopting and forking worktrees; capability checks |
| `server` | `serve`: the CSI endpoint on a Unix socket, with graceful shutdown |
| `error` | `Error`, mapped onto gRPC status codes |

## How a CSI call maps onto agentfs

| CSI call | What happens |
|---|---|
| CreateVolume | Makes `<basePath>/<pv>/{backing,state,csi.json}` on this node. The request's topology must include this node (`RESOURCE_EXHAUSTED` otherwise). The state area is initialised in-process (`init_with`, or `start_from` for a start class) with the profile's config included and its rules installed. The answer carries the node's topology. A start class fetches once. A fork class clones its source locally with no network |
| NodeStage | Checks the worktree and never creates one: a missing worktree is `NOT_FOUND` naming the node. A static PV is adopted only over an existing worktree of its volume (`reopen_with`). Starts `agentfs serve-history` for the volume and waits a bounded time for it to listen (`--stage-wait`, then `UNAVAILABLE`, and the kubelet retries) |
| NodePublish | Starts a front, `agentfs mount --history-service`, with the pod's identity, mounted at a private source. Bind-mounts it onto the kubelet's target |
| NodeUnpublish | Detaches the target, then stops the front (SIGTERM, SIGKILL after `--unpublish-grace`) |
| NodeUnstage | Refused while targets are published. Then stops the service (SIGTERM, SIGKILL after `--unstage-grace`; the journal makes that safe). The service makes its final commit and pushes it once more |
| DeleteVolume | Asks the release decider and never pushes. *Delete* renames the directory to `.deleting.<handle>` under the state lock and removes it. *Keep* renames `csi.json` to `csi.retained.json`. *Wait* answers `UNAVAILABLE` and wakes the release scan |

The node plugin spawns the `agentfs` binary for every long-running piece
rather than linking FUSE into the gRPC server. A crashed front or service
cannot take the server down, and the processes, flags and logs are the ones
the single-node setup uses. The process tree is `tini`, then `agentfs-csi`,
then its children.

`podInfoOnMount` gives NodePublish the pod's name, namespace and uid. The front
passes them on as its origin, so each pod commits as itself.

## Error mapping

`error::Error` has one variant per gRPC code, and `From<Error> for
tonic::Status` maps them: `InvalidArgument`, `NotFound`, `AlreadyExists`,
`FailedPrecondition`, `Aborted`, `Unavailable`, `ResourceExhausted` and
`Internal`. A message says what to do about it. `Unavailable` means "not ready
yet": the container orchestrator retries. Little network work happens in a call:
CreateVolume of a start class fetches once, DeleteVolume lists each remote once,
and NodeUnstage waits on the service's final push, bounded by `--push-grace`.
No call pushes.

## Parameters

A StorageClass, or a static PV's volume context, is parsed by
`params::VolumeParams::from_create(name, &parameters)` and
`from_context`:

| Key | Meaning |
|---|---|
| `profile` | Required. Names a profile: `<profile>.config` (a git-config fragment), `.exclude` and `.attributes`, under `--profiles-dir`. Credentials are files of a Secret in the driver's namespace |
| `volumeName` | A template over the claim's namespace and name and the PV name. The default is `{pvcNamespace}.{pvcName}.{pvName}`. It must contain `{pvName}`, so a re-made claim is a new worktree with a new branch |
| `onDelete` | `retain` (the default) or `delete` |
| `startFrom` | `"<remote>:<branch>@<commit>"`: every claim starts as a new worktree whose history begins at that commit of another worktree's branch |
| `workloadMounts` | Comma-separated `PATH` or `PATH=DIR`, passed to every front as `--workload-mount` so a worktree's absolute `gitdir:` targets map exactly |
| `fork`, `link`, `remotes` | Fork classes only (`params::ForkClass`) |

An unknown key is refused. There is no `branch` parameter: a worktree's branch
is always `agentfs/<volume>`, named once in its identity, which no include,
static PV, stage or start can change. `VolumeParams::branch_name()` returns it.

A start point names a branch only. A `refs/...` name is refused
(`INVALID_ARGUMENT`, "not a branch"), and so is the new volume's own branch.
Start-class codes: `INVALID_ARGUMENT` (shape, a remote not in the profile, own
branch), `FAILED_PRECONDITION` (no such branch, commit not on it),
`UNAVAILABLE` (a fetch or an LFS download failed; retried), `ALREADY_EXISTS`
(the name exists with another start point).

A fork class is a StorageClass with `fork: "true"`. It must not set `profile`,
`branch` or `startFrom`. It has its source's profile, its own branch, and starts
from its source's head. Its `volumeName` must name `{pvName}` and its `onDelete`
defaults to `delete`. The source must be on the fork's node
(`FAILED_PRECONDITION` otherwise, naming the remedy: pod affinity to the
source's pods). See [forks](../forks.md).

## The supervisor

`Supervisor` is what NodeService calls. Build one from a `NodeConfig`, a
`Launcher` and a `Mounter`:

```rust
use std::sync::Arc;
use agentfs_csi::launcher::AgentfsLauncher;
use agentfs_csi::layout::Layout;
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::supervisor::{NodeConfig, Supervisor};

let layout = Layout {
    base: "/var/lib/agentfs-csi/volumes".into(),
    node_state: "/var/lib/agentfs-csi/node".into(),
    mount_dir: "/run/agentfs-csi/mounts".into(),
    profiles: "/etc/agentfs-csi/profiles".into(),
};
let config = NodeConfig::new("node-a", layout);
let supervisor = Supervisor::new(
    config,
    Arc::new(AgentfsLauncher { bin: "/usr/local/bin/agentfs".into() }),
    Arc::new(KernelMounter),
);

// Rebuild services and recover fronts from the records, then serve.
let report = supervisor.reconcile().await?;
```

`Launcher` and `Mounter` are public traits so a test injects fakes:

```rust
pub trait Launcher: Send + Sync + 'static {
    fn spawn(&self, spec: ProcessSpec) -> io::Result<Spawned>;
    fn signal(&self, pid: u32, signal: Signal) -> io::Result<()>;
}

pub trait Mounter: Send + Sync + 'static {
    fn state(&self, path: &Path) -> io::Result<MountState>;
    fn bind(&self, source: &Path, target: &Path, readonly: bool) -> io::Result<()>;
    fn detach(&self, path: &Path) -> io::Result<()>;
    fn detach_all(&self, path: &Path) -> io::Result<usize>; // provided
}
```

`AgentfsLauncher` starts the binary with a clean `AGENTFS_*` environment, forwards
its output prefixed with a label, keeps its last 20 lines for error messages,
and reports its exit.

What the supervisor guarantees:

- **Stage** checks the worktree under its lock, sets the profile's include,
  installs the profile's rules, records the volume in `<node-state>/staged/`,
  starts the service on a node-local socket and waits for it to write its
  address.
- **Publish** starts a front that FUSE-mounts the backing tree at its own source
  directory, waits until the mount is live, bind-mounts it onto the kubelet's
  target and records the target in `<node-state>/published/`. A finished fork is
  published read-only (`MS_RDONLY`) whatever the request asks, and its front
  mounts it read-only (`ro`), so the kernel answers `EROFS` through any bind.
- **A front that exits unasked** (a crash, `kill -9`, an external unmount) is
  restarted after a backoff in the same pending slot, with the same pod flags.
  Its stale source is detached and the new mount is bind-stacked over the stale
  target, without unmounting it first, so it propagates into containers that
  mount the volume with `HostToContainer`. A target whose directory is gone (the
  pod is gone) is forgotten instead.
- **A service that exits unasked** is restarted after a backoff. It recovers its
  journal and the fronts reconnect by themselves.
- **A restart that cannot spawn** (a failed fork under PID or memory pressure, a
  full disk) is tried again with backoff. The entry and the records stay, so the
  volume and its targets are never dropped for it.
- **Reconcile**, at plugin start, rebuilds services for staged volumes and
  recovers fronts for targets whose topmost mount is a stale agentfs mount. It
  returns a `ReconcileReport`.
- Each volume's calls run one at a time, behind a per-volume async lock. Waits
  happen outside it, so a slow start never delays another pod's recovery.
- A NodePublish of a published target with other flags (read-only,
  single-writer) is `ALREADY_EXISTS`, as the CSI spec asks.

Restart delays are `supervisor::Backoff`: an initial delay that doubles up to
`max` (`--backoff-max`, 30 s by default) and resets after a child has run long
enough.

## The release decider

DeleteVolume asks `agentfs_core::release::decide`, the one pure decider of what
happens to a worktree. DeleteVolume gathers its facts: whether the branch
exists, the remotes mode (`Declared::of`), what each pushed remote advertises
for the branch in a listing made for that call, how many journaled records
history lacks (`volume::inspect`, read under the state lock), the class's
`onDelete`, and a fork's state.

```rust
use agentfs_core::release::{self, Declared, Kind, OnDelete, Release, Seen, Worktree};

let seen: Vec<Seen> = /* one per pushed remote, from RemotePusher::branch_claim */;
let release = release::decide(&Worktree {
    has_history: true,
    uncommitted: 0,
    on_delete: OnDelete::Retain,
    remotes: Declared::of(&settings, &seen),
    kind: Kind::of(None, &settings),
});
match release {
    Release::Delete => { /* remove the worktree */ }
    Release::Keep(_) => { /* release the volume, keep the directory */ }
    Release::Wait(why) => { /* answer UNAVAILABLE naming `why` */ }
}
```

- **Delete.** Every push remote holds the head (or there is no history), the
  journal holds nothing history lacks, and `onDelete` is `delete`. Also: an
  unlinked fork with no remote of its own, and `onDelete: delete`, whatever its
  history holds. That is the only case where unpushed history is removed, and it
  is the user's choice.
- **Keep.** `onDelete: retain` once every remote holds the head and the journal
  is committed, or a worktree with no remote at all (nothing elsewhere can hold
  it).
- **Wait.** `UNAVAILABLE`, naming the first remote that does not hold the head
  and why (lacking, unreachable, diverged), a remote that holds the head but
  lacks a ref of a fork received into this history, every remote section
  invalid, a journal holding records history lacks, or a fork not yet published
  or abandoned.

A diverged remote branch is never removed until an operator resolves it
([divergence runbook](../runbooks/divergence.md)). DeleteVolume refuses
(`FAILED_PRECONDITION`) a worktree staged on its node, one whose state lock is
held, and one whose settings cannot be read. Nothing is deleted or released
then. Every class of the driver has `reclaimPolicy: Delete`, so DeleteVolume
always runs and the decider always answers: data safety is the decider's, not
Kubernetes'. With `onDelete: retain` the worktree stays on disk after its PV
goes.

## The release scan

`ReleaseScan::new(layout, launcher, identity, interval, backoff)` runs at start,
every `--release-scan-interval` (30 s) and whenever DeleteVolume waits
(`wake()`). For every `csi.json` or `csi.retained.json` on its node that no pod
stages, it:

1. runs `agentfs flush` while the journal holds records history lacks;
2. then runs `agentfs push` unless the last push of it exited 0 at its current
   head with the same remotes (`<name> <push URL>`: a remote added to the
   profile since is pushed to).

That memory is in process: a restarted plugin pushes each worktree once more. A
failed flush or push backs off per worktree, doubling up to `--backoff-max`;
exit 75 is retried at the next scan. Each failed push raises `PushFailed`
through the child's own alarms and telemetry, which is what alarms while
unpushed commits sit in an unstaged worktree.

A fork asked to finish (DeleteVolume of a fork records `finish_requested` in
`csi.json`) gets `agentfs fork finish --timeout 300s` instead. The scan skips a
fork that is refused or abandoned. A fork's finish and publish are the scan's
alone.

`scan()` returns a `ScanReport`: the forks being finished, the worktrees being
flushed and pushed, the children still running, the worktrees whose last push is
held, those backing off, and unreadable records.

## Where things live

| Path | Holds |
|---|---|
| `<base>/<handle>/backing` | The worktree's files |
| `<base>/<handle>/state` | Its state area |
| `<base>/<handle>/csi.json` | The `VolumeRecord`: parameters, requested capacity, creation time, whether it was adopted, whether a fork's finish was asked for |
| `<base>/<handle>/csi.retained.json` | The record of a released, kept worktree |
| `<base>/.deleting.<handle>` | A worktree being deleted |
| `<node-state>/staged/<handle>.json` | The volume's history service runs on this node |
| `<node-state>/published/<handle>/<key>.json` | A front serves this target |
| `<node-state>/run/<handle>/history.sock` | The history service's socket |
| `<mount-dir>/<handle>/<key>` | A front's own mount, before the bind onto the kubelet's target |
| `<profiles>/<profile>.{config,exclude,attributes}` | The operator's profile |

`Layout` computes these. A handle is validated: `[A-Za-z0-9][A-Za-z0-9._-]*`,
with no `..` and no `.lock` suffix. `records::store` writes a record
atomically. The base path must be on the node's own disk: `agentfs-csi` refuses
a network filesystem at start, naming its type.

## Serving

```rust
use agentfs_csi::{controller::ControllerService, identity::IdentityService,
                  node::NodeService, server};

server::serve(
    std::path::Path::new("/csi/csi.sock"),
    IdentityService::new(),
    ControllerService::new(layout, "node-a".into(), release_scan),
    NodeService::new(supervisor.clone()),
    shutdown_signal(),                 // any future
).await?;
```

`serve` binds the socket (replacing a stale one, mode 0660), serves all three
services until `shutdown` completes, then stops accepting, lets running calls
finish and removes the socket. The controller's calls are serialised and run
their blocking work off the async threads.

The `agentfs-csi` command wires these together from its flags
(`--endpoint`, `--node-id`, `--base-path`, `--node-state`, `--mount-dir`,
`--profiles-dir`, `--agentfs-bin`, `--passthrough`, `--front-threads`, the
stage, publish, unpublish and unstage waits, `--history-wait`,
`--backoff-max`, `--release-scan-interval`). The flags are in
[configuration](../configuration.md).

## Recovery

- A front crash leaves a stale mount, which the supervisor stacks a new mount
  over ([filesystem layer](filesystem.md#mount-order-and-shutdown)). The
  workload's calls fail with `ENOTCONN` meanwhile, and nothing is written where
  it would not be recorded.
- A service crash is a restart. Boundaries during the outage fail with `EIO`.
  Their final states are recorded by the fronts' repairs, release and sweep, but
  intermediate states are lost.
- A plugin restart runs `reconcile`.
- Operator procedures: [csi-recovery](../runbooks/csi-recovery.md),
  [history-service-down](../runbooks/history-service-down.md),
  [moving](../runbooks/moving.md).

## What this layer does not do

- It does not offer a multi-node volume. A volume lives on one node's disk. A
  pod on another node reads the history from the remote the volume pushes to.
- It does not fence which branch is current. A second worktree that writes a
  name another created is an unrelated history, saved as a divergence. Not
  covered: a hand-copied state area, local and `file://` remotes (libgit2
  updates them without compare-and-swap), and anyone holding the remote's
  credentials.
- It does not push from a CSI call. The history service pushes on its interval
  and at shutdown, and the release scan pushes what no pod stages.
- It does not serve the sandbox overlay. The overlay is refused on a front.
- It does not create a worktree at NodeStage. Only CreateVolume creates one.
- It does not snapshot, expand or resize volumes, and capacity is recorded but
  not enforced.
- It does not delete a worktree that holds history no remote has, except the
  unlinked fork with no remote of its own.
- It does not link FUSE into its server. A front or service crash does not take
  the plugin down; the plugin restarts it.
- Cloning a claim that a pod is using, and following later Kubernetes and
  provisioner releases, are not covered; see [CSI](../csi.md) for the supported
  cases.
