# The Kubernetes adapter: `agentfs-csi`

`agentfs-csi` is a CSI driver. It lets a cluster hand an agentfs volume to a
pod the way it hands out any other volume: a StorageClass, a claim, a mount.
It is the third integration layer. It adds no recording of its own: every
volume is a tree volume ([trees](../trees.md)), and the driver runs the
`agentfs` binary of the [filesystem layer](filesystem.md) for it, `agentfs
init` to make a volume and one `agentfs host` per volume in use, driven over
the host's control protocol ([the view host
command](filesystem.md#the-view-host-command)).

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

Each claim is a tree volume on the disk of the node that made it, for good,
served to one pod; a fork claim is a fork inside its source's volume. Every
multi-node access mode is refused.

- There is no controller Deployment, no attach step and no stage step. The
  driver advertises `VOLUME_ACCESSIBILITY_CONSTRAINTS`. NodeGetInfo and
  CreateVolume answer the node's topology,
  `topology.agentfs.mcurcio.github.io/node = <node>`, so the PV's node affinity
  keeps every pod of the claim on that node. Use `WaitForFirstConsumer` so the
  scheduler picks the node first.
- `CSIDriver.attachRequired` is `false`. ControllerPublish, ControllerUnpublish,
  NodeStage and NodeUnstage answer `UNIMPLEMENTED`; the node does not advertise
  `STAGE_UNSTAGE_VOLUME`.
- The driver name is `agentfs.mcurcio.github.io` (`agentfs_csi::DRIVER_NAME`).
- It supports persistent volumes only (`volumeLifecycleModes: [Persistent]`).
  Inline ephemeral volumes are refused (`node::EPHEMERAL_REFUSED`), since their
  parameters would be the pod author's.
- It supports mount volumes with a single-node access mode
  (`SINGLE_NODE_WRITER`, `SINGLE_NODE_SINGLE_WRITER`,
  `SINGLE_NODE_MULTI_WRITER`, `SINGLE_NODE_READER_ONLY`), with no mount flags.
  A block volume, a mount flag or a multi-node mode (`volume::MULTI_NODE`) is
  `INVALID_ARGUMENT`.
- It advertises `CLONE_VOLUME` and clones only into a fork class. Any other
  content source, and every snapshot, is `INVALID_ARGUMENT`.

| Module | Offers |
|---|---|
| `csi` | The vendored CSI v1.13 protocol, generated at build time |
| `identity` | `IdentityService` and the topology key |
| `controller` | `ControllerService`: CreateVolume and DeleteVolume on this node; the recording-format refusal |
| `node` | `NodeService`: validates a call, resolves the claim (adopting a static PV over a kept volume) and hands it to the supervisor |
| `supervisor` | `Supervisor`: `agentfs init` per volume made, one `agentfs host` per volume in use, views opened at the kubelet's targets, one pod per claim, restarts with backoff, reconcile at start, release |
| `host_client` | `HostClient`: the plugin's end of a host's control channel, an actor owning the stream |
| `launcher` | `Launcher` and `AgentfsLauncher`: spawns `agentfs init` and `agentfs host` (`init_spec`, `host_spec`), the host's standard input one end of a socket pair |
| `mounter` | `Mounter` and `KernelMounter`: what is mounted at a target, and lazy detaches of views no host serves |
| `params` | StorageClass and PV parameters (`VolumeParams`), the refusals' texts, the pod context of a publish |
| `layout` | `Layout`: where the driver keeps things; `target_key`, the view's label |
| `records` | `ClaimRecord` (`csi.json`) and `PublishedRecord` (`<nodeState>/published/<pv>.json`), written whole |
| `volume` | Capability checks and a profile's config |
| `server` | The CSI endpoint on a Unix socket, with graceful shutdown |
| `error` | `Error`, mapped onto gRPC status codes |

## How a CSI call maps onto agentfs

| CSI call | What happens |
|---|---|
| CreateVolume | Inspects `<base>/<pv>/state` first (a recording-format state area is `ALREADY_EXISTS`, naming the moving runbook). Then `Supervisor::ensure` runs `agentfs --state <base>/<pv>/state init --volume <name> --include <profiles>/<profile>.config` (Volume's ensure) and installs the profile's rule files, and `csi.json` is written. The request's topology must include this node (`RESOURCE_EXHAUSTED` otherwise). A fork class makes a fork claim ([CSI](../csi.md#14-forks)) |
| NodePublish | Resolves the claim to a volume and a tree (`TreeRef::Root`, or `TreeRef::Fork(key)` for a fork claim). `Supervisor::publish` refuses a claim published at another target, starts the volume's host if none runs, sends `open` with the target, the label `csi-<target_key>`, the read-only flag and the pod's context, and writes the published record |
| NodeUnpublish | `Supervisor::unpublish` sends `close`, removes the record and the target directory, and stops the host after its last view |
| DeleteVolume | A root claim: `FAILED_PRECONDITION` while the volume is published on the node; else `Supervisor::release` starts a host with no view and stops it (exit 0 is Volume's release), and `csi.json` is renamed `csi.retained.json`. A fork claim: `fork_end` by key on its source's host |

The node plugin spawns the `agentfs` binary for each volume rather than
linking FUSE into the gRPC server. A crashed host cannot take the server down,
and the boot, telemetry and drain are the binary's own. The process tree is
`tini`, then `agentfs-csi`, then its children. Children's output goes to the
plugin's stderr line by line, prefixed `[host <pv>]` or `[init <pv>]`; the last
lines are kept for error messages. Inherited `AGENTFS_*` variables are removed
from a child's environment; `OTEL_*` ones pass through.

`podInfoOnMount` gives NodePublish the pod's name, namespace and uid
(`params::pod_context`), with the node's name; the host's view writes as a
client with that context, so each save carries its pod's trailers.

## Error mapping

`error::Error` has one variant per gRPC code, and `From<Error> for
tonic::Status` maps them: `InvalidArgument`, `NotFound`, `AlreadyExists`,
`FailedPrecondition`, `Aborted`, `Unavailable`, `ResourceExhausted` and
`Internal`. A message says what to do about it. `Unavailable` means "not ready
yet": the container orchestrator retries. A host's refusal maps by its kind:
`fork_taken`, `key_unknown`, `key_ended` and `stale` to `FAILED_PRECONDITION`,
`occupied` to `ALREADY_EXISTS`, `mount_failed` to `INTERNAL`, `draining` and
`stopped` to `UNAVAILABLE`, `nested_not_built` and `approval_not_built` to
`INVALID_ARGUMENT`, each with the host's message. No call does network work.

## Parameters

A StorageClass is parsed by `params::VolumeParams::from_create(pv_name,
&parameters)`, a static PV's volume attributes (and the context CreateVolume
returns, `to_context`) by `from_static`:

| Key | Meaning |
|---|---|
| `profile` | Required. Names a profile: `<profile>.config`, `.exclude` and `.attributes`, under `--profiles-dir` |
| `volumeName` | A template over the claim's namespace and name and the PV name. The default is `{pvcNamespace}.{pvcName}.{pvName}`. It must contain `{pvName}`, so a re-made claim is a new volume. In a static PV, a literal name (`volume` is the same key) |
| `onDelete` | `retain` only; `delete` is `ON_DELETE_NOT_BUILT` |
| `fork` | `"true"`: a fork class (`is_fork_class`) |

`startFrom` (`START_FROM_NOT_BUILT`) and `workloadMounts`
(`WORKLOAD_MOUNTS_NOT_BUILT`) are refused as not built; `link` and `remotes`
are a fork class's. An unknown key is refused, `branch` included. Keys under
`csi.storage.k8s.io/` feed the template and are otherwise ignored. A fork
class takes `fork` and `landing` only ([CSI](../csi.md#14-forks)).

## The supervisor

`Supervisor::new(NodeConfig, Arc<dyn Launcher>, Arc<dyn Mounter>)` owns every
host on the node. Three locks order its calls, each named for what it orders:

- **A claim's lock** is held by its publish or unpublish, from the one-pod
  check to the record's write. A second NodePublish or NodeUnpublish of a
  claim whose call still runs is answered `ABORTED` at once (the CSI
  specification's "operation pending for volume"), and the kubelet retries;
  calls of other claims go on. Each call runs as a task of its own to the end,
  so a kubelet that gives up drops only its wait.
- **A volume's start lock** orders its host's start and stop, the replay,
  every write of the volume's records and every detach of its targets. It is
  held across a host's reply only while a host starts (replay and reconcile,
  on a host no other call sees yet), while it stops, and for a fork's create
  or end. An open or close of a running host's view is never under it, so one
  close that waits on a held file stalls only its own claim.
- **`using`**, a short lock never held across a wait on a host, holds each
  volume's claims whose open is in flight. A volume is idle when none is and
  it has no published record; deciding that and taking the host out of
  service are one step under it, so a publish never gets a host being
  stopped.

- **Ensure** runs `agentfs init` and installs the profile's rule files
  (`agentfs_engine::operator_settings::install_rules`) into
  `state/history.git/info/`. It refuses while the volume is served, since its
  settings change only while it is not.
- **Starting a host** installs the rule files again, spawns `agentfs host`
  (`launcher::host_spec`), reads its `Hello` (checking the volume's name and
  the protocol version: another version is `FAILED_PRECONDITION` and never
  tried again), then opens every recorded view of the volume whose target is
  still there, stacked over a stale mount a crashed host left, and sends
  `reconcile`. Only then do other calls see the host. A recorded view whose
  target is gone is dropped.
- **Publish** allows one pod per claim: a claim published at another target is
  `FAILED_PRECONDITION` (`several pods on one volume are not built`), at the
  same target with another read-only flag `ALREADY_EXISTS`, at the same target
  with the same flag a success.
- **Stopping a host** calls `HostClient::close`: the stream closes, whatever
  clones of the client other tasks still hold.
  With no view open (every stop the supervisor asks for) the host drains and
  exits 0 once Volume releases the volume; with views open (the plugin's
  shutdown) it exits 3 at once and leaves them stale for the next host
  ([the view host command](filesystem.md#the-view-host-command)).
- **A host that exits unasked** is started again after `next_backoff` (1 s,
  doubling to `Backoff::max`, back to 1 s after a minute of uptime) while
  records of the volume remain, with every view replayed. A start that fails
  is tried again with backoff. These delays are its only timers, beside one
  bound: `AgentfsLauncher` waits up to 1 s per output pipe for a child's last
  lines before it reports the exit, because a grandchild may hold the pipes
  open; lines later than that miss the exit's error text.
- **Release** starts the volume's host with no view and stops it; its exit 0
  is Volume's release verdict.
- **Reconcile**, at plugin start, drops the published records whose target is
  gone, starts a host per volume that still has views, and logs each
  recording-format volume under the base path, which it never serves
  (`ReconcileReport`).
- **Shutdown** stops every host and waits for each to exit.

`HostClient::start(stream)` reads the `Hello` and answers
`StartError::Version { host, plugin }` for another protocol version (the
stream closed) or `StartError::Gone`; `call(request)` sends one request and
awaits its reply; `close()` closes the stream. Two tasks own the stream ([host
protocol v2](filesystem.md#the-view-host-command)): a writer gives each call
an id (from 1, never used again) and writes its request in an envelope, and a
reader hands each reply to the call its id names, in whatever order the host
answers. Calls do not wait for each other. A caller whose wait the kubelet's
deadline cancels drops only its own wait: its id stays pending until its
reply arrives, and that reply is discarded. A reply with an id no call has is
a broken protocol: every waiter gets `HostGone` and the stream closes.

## Where things live

```text
<base>/                         --base-path: this node's own disk, never a network filesystem
  <pv>/                         a claim's directory (the PV name), 0700
    state/                      the tree state area (history in state/history.git)
    csi.json                    the claim record (CreateVolume, or a static PV)
    csi.retained.json           the record once DeleteVolume kept the volume
  <fork-pv>/csi.json            a fork claim: its fork lives in its source's volume
<node-state>/published/<pv>.json   the claim's published view
<profiles>/<profile>.config|.exclude|.attributes
```

Records are JSON written whole (a temporary file renamed over it, mode 0600).
A field this driver does not know fails the load, so a record of another build
is refused, never half-read.

## Recovery

A crashed host is started again with its views replayed and its clients
rebound (same launcher `csi`, tree, label and access), so a fork keeps its one
writer. A restarted plugin does the same from `<node-state>/published/`. The
procedures are in [CSI](../csi.md#8-recovery) and the [CSI recovery
runbook](../runbooks/csi-recovery.md).

## What this layer does not do

- It records nothing itself: the hosts it runs do.
- It pushes nothing, lands no patches and runs no release scan: push and the
  patch queue are not built on tree volumes, and each host logs those settings
  as not built.
- It does not delete a volume: DeleteVolume keeps the directory.
- It serves one pod per claim, and never a volume on two nodes.
- It never serves a recording-format volume.
- It exports no telemetry of its own; its hosts do, through the `OTEL_*`
  variables it passes on.
