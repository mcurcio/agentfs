# Supported configurations

The ways agentfs can be put together, as reference material to check a
change or a design against. Section 1 lists the dimensions a configuration varies
on. Section 2 gives named example configurations, each a combination of
those values. Section 3 says how to use them.

This page links to the homes of each fact rather than restating them:
[guarantees](guarantees.md), [recording](recording.md),
[remotes](remotes.md), [patches](patches.md), [forks](forks.md) and
[csi.md](csi.md). Every key and flag is in the
[configuration reference](configuration.md).

Every value is marked:

| Mark | Meaning |
|---|---|
| **built** | In the code today; the cell names the key, flag or page |
| **not built** | Not in the code. Do not rely on it |
| **not supported** | Refused on purpose |

There are two different worlds, and a design must say which one it is
in:

- **Sharing one volume.** Clients use one agentfs volume: one backing
  tree, one copy of each file, one history. They see each other's writes
  at once. What matters is file semantics: locks, renames, who records
  what.
- **Exchanging through git.** Clients have separate volumes, or are not
  volumes at all (iac, a reviewer, a forge). They never share files: they
  share commits through remotes. What matters is refs, order, merging and
  gating.

agentfs has two engines:

- **Tree views** ([trees](trees.md)) are what every launcher runs: the
  sidecar, the Docker examples, the CLI's `agentfs init` and `agentfs
  mount`, and the CSI driver (`agentfs host`, [csi.md](csi.md)). Section
  1.5 says what they build and what they do not.
- **The recording engine** is a history service per volume
  (`serve-history`) and a front per mount (`front`), with the one-shot
  commands, on recording state areas. No launcher runs it; the CSI driver
  never serves a recording-format volume
  ([moving.md](runbooks/moving.md)).

Sections 1.1 to 1.4 describe the recording engine unless a cell says
otherwise. On a tree volume, every value section 1.5 marks **not built**
is said at boot, never silently: a configured setting is logged with a
`not built:` line, counted by the `agentfs.not_built` gauge and alerted
by `AgentfsNotBuilt`, and a recording command refuses the volume.

## 1. Dimensions

### 1.1 Integration point

Where a consumer taps in. agentfs has three layers; a higher one uses
the lower ones ([architecture.md](architecture.md#crates)).

| Layer | Crate | What a consumer uses | Status |
|---|---|---|---|
| Library | `agentfs-core` | The recording engine. Rust calls with no mount and no agentfs process: opening a recording state area and recording (`volume`, the `Recorder`), restore and revert planning (`restore`), the patch queue (`patch`), forks (`fork::create`, `fork::finish`, `fork::listing`), delivering a fork's change through the `Delivery` contract (`fork::deliver`), the deletion decider (`release::decide`) | **built** (module map: [architecture.md](architecture.md#agentfs-core), page: [library/core](library/core.md)) |
| Library | `agentfs`, re-exporting `agentfs-engine`, the shared kernel, the domains and `agentfs-service` | The tree engine, depended on as `agentfs` ([library/agentfs](library/agentfs.md)). Rust calls with no mount and no agentfs process: a working volume through the service (`service::init`, `service::Domains::stage`, and a bound client's session; [library/service](library/service.md)). The consumer API is that route alone ([library/agentfs](library/agentfs.md#the-consumer-api)) | **built** (module map: [architecture.md](architecture.md#agentfs-engine), page: [library/engine](library/engine.md)) |
| Filesystem | `agentfs-fuse` and the `agentfs` binary | Tree views: `init`, `mount` (with `-- COMMAND`), `host` (views on a parent's requests over its standard input, [library/filesystem](library/filesystem.md#the-view-host-command)), `fork end` and `probe` ([trees](trees.md#run-it)). Recording engine: `serve-history` with `front` fronts; `run` (the commit and push loops without a mount); one-shot commands: `record`, `flush`, `push`, `checkpoint`, `status`, `probe`, `restore`, `revert`, `patch submit`, `patch approve`, `patch apply`, `patch status`, `fork create`, `fork finish`, `fork status`, `fork abandon`, `fork list`, `fork deliver`, `start accept`, `lfs fetch`, `lfs prune` | **built** ([flags](configuration.md#flags-and-environment)) |
| Filesystem | the control socket, `meta/control/agentfs.sock` | `push` (push now), `restore`, `revert`, `apply`, `checkpoint` and `judge` against a running daemon; JSON lines, version 1, root or the daemon's user only | **built** |
| Filesystem, packaged for Kubernetes | the sidecar manifests and the sidecar library chart | the binary as a privileged native sidecar on tree views, one pod per volume | **built** ([deployment.md](deployment.md), [helm.md](helm.md)) |
| Kubernetes adapter | `agentfs-csi` | Tree volumes: StorageClass parameters (`profile`, `volumeName`, `onDelete: retain`, `fork`, `landing: automatic`), profiles in a ConfigMap, claims and fork claims, one `agentfs host` per volume in use; section 1.5 lists what it refuses | **built** ([csi.md](csi.md), [parameters](configuration.md#the-csi-driver-agentfs-csi)) |
| Every layer | what agentfs writes for others to read | the history branch and `refs/agentfs/<volume>/*` on remotes; the patch status ref; OpenTelemetry metrics, traces and alarms; `status`, `patch status --json` and `fork status --json` | **built** |
| Filesystem | checkpoint: an integrator says a batch of work is done, which cuts one commit for it | `agentfs checkpoint [-m TEXT]`, or the control socket's `checkpoint`: one commit on `refs/agentfs/<volume>/change-line` | **built** |
| Filesystem | notify: agentfs tells an integrator the queue's verdicts changed | the hook `$GIT_DIR/hooks/patch-notify`; the webhook `agentfs.patchNotifyUrl` | **built** ([patches](patches.md), [keys](configuration.md)) |
| Filesystem | apply: an integrator asks agentfs to land waiting patches now | `agentfs patch apply`; `serve-git`'s `POST /agentfs/apply`. It lands only with nothing attached. Under a running daemon it only judges and answers "workload attached; lands at next mount" (exit 75) | **built** |
| Filesystem | `agentfs serve-git --listen HOST:PORT`: agentfs as a git server that takes patch pushes | git's smart HTTP; pushers are `[agentfsPusher "<name>"]` sections with rights `offer`, `approve`, `barrier`, `integrator` | **built** |
| Kubernetes adapter | a git endpoint per node plugin for offers, approvals and a sidecar's checkpoints | `agentfs-csi` runs no `serve-git` | **not built** |

### 1.2 Sharing topology

How many clients use one volume, and through what. This is the
sharing-one-volume world.

| Value | How | What clients get | Status |
|---|---|---|---|
| One process, one mount | Tree views: `agentfs mount ROOT`; one workload process | Every call recorded in the tree's journal before it returns ([trees](trees.md#recording-and-durability)) | **built** |
| A mount and a fork of it | Tree views: `agentfs mount ROOT --fork-mount DIR [--fork-key KEY]` | The fork's saves land in the root as they save, and the root's flow back ([trees](trees.md#forks-keys-end-and-the-drop)) | **built** |
| Several processes, one mount | Several processes or containers use one FUSE mount: one pod's containers ([deployment.md](deployment.md#10-hermes): Hermes and its tool containers), or several Docker workload containers on one host mount ([docker.md](docker.md#2-the-layout)) | One kernel and one daemon. Each save is its own commit, with the mount's writer and pod trailers ([trees](trees.md#volume-identity-and-trailers)) | **built** |
| Several pods, one volume, one node | A second pod's NodePublish of a CSI claim | Refused: `several pods on one volume are not built` (example 4) ([csi.md](csi.md#6-using-a-volume)) | **not built** |
| A pod and a fork claim's pod, one volume, one node | A CSI fork claim (`fork: "true"`, a `dataSource` claim) | The fork pod writes its own fork of the volume, served by the source's host; its saves land in the source automatically (example 5) ([csi.md](csi.md#14-forks)) | **built** |
| One daemon, two mount points: the main mount and the sandbox overlay | A program linking `agentfs-fuse` with `MountConfig::overlay` ([library/filesystem](library/filesystem.md#the-sandbox-overlay)) | One set of names, one set of open files, one recorder. The overlay shows only recordable files and keeps other new files in a private tmp layer | **not built** on any launcher: tree volumes do not build it and fronts never served it (example 12) |
| Several pods through the sidecar | | The sidecar is one pod per volume ([deployment.md](deployment.md#11-limitations)) | **not built**; a CSI fork claim gives a second pod a fork |
| Several nodes, one volume | `ReadWriteMany`, `ReadOnlyMany` | Refused: the claim stays Pending. Pods on other nodes read the history from a remote | **not supported** |
| A volume moving between nodes | | A CSI volume stays on its node for good; `startFrom` is refused (example 10) | **not built** ([moving.md](runbooks/moving.md)) |

### 1.3 Remote topology

Where history goes and what comes back. This is the
exchanging-through-git world.

| Value | How | What happens | Status |
|---|---|---|---|
| No remote (local-only) | No `[remote "<name>"]` section | Records and commits; pushes nothing; no alarm. A remote added later gets all the history. The history dies with the disk ([remotes modes](configuration.md#remotes-modes)) | **built** |
| One remote | One section with `url` | Pushed every `agentfs.pushInterval`. It is also the patch remote by default | **built** |
| Several remotes | Several sections | Each has its own push thread, push state and alarms. The smallest `agentfsMaxBlobSize` and `agentfsMaxLfsObjectSize` apply to recording. There is no patch remote unless `agentfs.patchRemote` names one | **built** |
| Volumes sharing one repository | Several volumes, one remote URL, unique `agentfs.volume` names | Each writes only `refs/heads/agentfs/<volume>` and `refs/agentfs/<volume>/*`. Another writer is detected as a divergence, not prevented | **built** |
| Linked fork | `agentfs fork create` (default `--link linked`) on a recording state area | Pushes its history to the fork remote: the section with `agentfsForkRemote = true`, else the source's patch remote. A local-only source's forks publish into the source's own history. On finish it publishes one change on its base; the receiver gates it | **built** |
| Unlinked fork | `fork create --link unlinked [--remotes FILE]` on a recording state area; a CSI fork class with `link: unlinked` is refused (`unlinked forks are not built`) | Never delivers to its source and never pushes to the source's remotes. With no remotes of its own it is local-only and lost when deleted, which is the user's choice | **built** |
| Hidden fork history | `agentfs.forkRefs = hidden` in the source | New forks keep their history at `refs/agentfs/<fork>/history`, not under `refs/heads/` | **built**; whether a forge runs CI on it is not verified |
| Forks of forks | `agentfs fork create` from a linked fork's state area; a CSI fork claim of a fork claim is refused (`forks of forks are not built`, example 6) | When the child finishes, its change is queued as the parent's next inbox number on the fork remote. The parent lands it at its next mount with nothing attached, and the parent's own change carries it up. A child that finishes after its parent is published as a top-level fork (`ForkParentFinished`). Refused: a fork of an unlinked fork, of a fork with hidden history, or of a fork of a local-only source | **built** ([patches](patches.md#forks-of-forks), [runbook](runbooks/patch-queue.md#9-forks-of-forks)) |
| Started from another worktree's branch | `startFrom: "<remote>:<branch>@<commit>"` in a CSI class | Refused: `startFrom is not built on tree volumes` (example 10). The library's `fork::start` remains for recording state areas | **not built** on any launcher |
| Patch producers | iac, CI or a person: `agentfs patch submit` or plain git, to `refs/agentfs/<volume>/inbox/<n>` on the patch remote, or pushed to `agentfs serve-git` | Applied in order with nothing attached. A patch that cannot land is set aside and the rest go on; the run stops only at a `gap`, `unrecorded` changes or a failure. Status in `refs/agentfs/<volume>/status` | **built** ([patches.md](patches.md)) |
| A fork's change delivered | `agentfs fork deliver <fork> --to inbox\|branch\|command\|webhook`, run by the receiver with its own credentials | `inbox` queues it as a patch; `branch` creates a pull request's head branch; `command` and `webhook` hand the payload to a program. No launcher delivers | **built** ([forks.md](forks.md#listing-and-delivering-a-change)) |
| A forge in the loop (GitHub, Gitea) | An ordinary remote | agentfs pushes its own refs. It never opens, reads, reviews or merges a pull request. Anyone writing the volume's branch there is another writer: a divergence | **built** as a plain remote; nothing forge-specific |
| A patch landed live into a running volume | | Patches land only with nothing attached: at the history service's start, or `agentfs patch apply` with no daemon. Patches never land while a workload runs | **not built** |
| A patch that is a branch | Any line of single-parent commits on an ancestor of the head, pushed to `inbox/<n>` | Merged by the `git` program and landed as one commit | **built** |
| A conflicting patch held while the rest go on; barriers | `patch submit --barrier` (an `Agentfs-Barrier: yes` tip) | A patch that cannot land is set aside whole (`PatchHeld`) and later patches go on. While a barrier is set aside, every later patch waits | **built** |

### 1.4 Knobs that change commits, pushes and patches

| Knob | Set in | What it changes | Status |
|---|---|---|---|
| `agentfs.commitWindow` | config, default `200ms` | Records this close in time become one commit. Grouping changes how history reads, not what is kept | **built** |
| Fixed commit boundaries | none | Restores, reverts and patches are commits of their own. A commit never mixes pods. With `agentfs.nested = working-tree`, a nested repository's `index.lock` bounds a group | **built** |
| `agentfs.commit=fsync`, `agentfs.group`, `agentfs.coalesce` | `info/attributes` only | Databases: a group (`app.db`, `-wal`, `-journal`) recorded whole as one atomic record at `fsync`, by a timer, at its last close and at unmount; up to one `agentfs.coalesce` late | **built** |
| `agentfs.sweepInterval` | config, default `30s` | The backstop that records files held open and retries failed records | **built** |
| `agentfs.pushInterval` | config, default `60s` | How often each remote is pushed, and the base of the failure backoff | **built** |
| Push now | `agentfs push` against a running daemon | Commits what is recorded and wakes each remote's push at once | **built** |
| `--push-grace` | `run`, `serve-history`, default `5s` | One last push of the final commit at shutdown; `0s` skips it | **built** |
| `agentfsMaxPushSize` | per remote | A backlog too large for one push goes out in first-parent steps | **built** |
| `agentfsMaxBlobSize`, `agentfsMaxLfsObjectSize` | per remote | Content over the smallest limit is refused at record time (`PathRefused`) unless stored as LFS | **built** |
| `--passthrough auto\|on\|off` | `front` | Whether data reads and writes bypass the daemon. `fsync`-class paths never use passthrough | **built** |
| Git LFS: `agentfs.lfsThreshold`, `agentfs.store`, `filter=lfs`, `lfs.url`, `remote.<name>.lfsurl` | config, `info/attributes` | Which content is stored as LFS and where it is uploaded ([Git LFS](configuration.md#git-lfs)) | **built** |
| Path rules: `info/exclude`, `-agentfs`, `agentfs.maxSize` | the git directory, set by the operator | Which new paths are recorded; `-agentfs` is never recorded even if tracked | **built** |
| `agentfs.respectTreeRules` | config, default `false` | Whether tracked `.gitignore` and `.gitattributes` take part in path rules. They can never set `agentfs.*` attributes | **built** |
| `agentfs.nested` | config, default `exclude` | Nested git repositories: record nothing in them, or their working files | **built** |
| `agentfs.repackInterval` | config, default `1h` | Storage only: repacks history; changes no commit | **built** |
| `agentfs.patchRemote`, `agentfs.patchFetchTimeout` | config | Where the inbox is fetched from and the queue status pushed; how long start waits for the fetch | **built** |
| When patches are applied | none | At `serve-history` start with no fronts running; by `agentfs patch apply` with the volume stopped. `agentfs run` does not apply them | **built** |
| `patch submit --replace N`, `--skip N` | the producer | Replaces or skips patch N: a set-aside patch, or the one a stopped queue stopped at | **built** |
| `agentfs.forkRefs`, `--link`, `--remotes` | source config; `fork create` | Where forks push, and whether they deliver back | **built** |
| `[agentfs "identity"]` templates | config | Committer and author of each commit | **built** |
| Restore and revert modes | `agentfs restore`, `revert`; `--dry-run`, `--force` | Through the history service, or directly with `--root` when nothing runs. Each is one commit. `run` serves none and the command exits 75 | **built** |
| Checkpoint | `agentfs checkpoint`, `-m TEXT` | Cuts one commit per change intent on the change line | **built** |
| Notify | `$GIT_DIR/hooks/patch-notify`; `agentfs.patchNotifyUrl`, `agentfs.patchNotifyCredentialFile`, `agentfs.patchNotifyTimeout` | One notice when the queue's verdicts change, after each queue run and each judging. A failure raises `NotifyFailed`, and the next queue event sends it again | **built** |
| Apply | `agentfs patch apply`, `--no-fetch` | Lands waiting patches when nothing is attached. Under a running daemon it only judges, and each patch that would land is `ready` | **built**; landing into a running volume is **not built** |
| Acceptance rule | `agentfs.patchAccept` = `cleanMerge` (default), `approval` (`agentfs patch approve N`) or `command` (`$GIT_DIR/hooks/patch-accept`); `agentfs.patchAcceptTimeout` | Whether a patch that merges cleanly may land | **built** |
| Merge or rebase | `agentfs.patchIntegrate` = `merge` (default) or `rebase`; merge drivers from `$GIT_DIR/info/attributes` | How a patch is merged onto the head, by the `git` program. Either way it lands as one commit | **built** |
| Which line a remote gets | `remote.<name>.agentfsPush` = `branch` (default) or `changeLine` | Every recorded commit, or only the checkpoints and the patches they merged | **built** |

### 1.5 Tree volumes: the sidecar, Docker, the CLI and the CSI driver

What a tree volume builds, and what says so at boot when it does not.
Under the CSI driver each volume's `agentfs host` logs the same lines at
every start, through the node plugin's log, and exports the gauge through
the `OTEL_*` variables the plugin passes on. A
`not built:` line names each configured key, one per key at every boot,
with its capability (`push`, `patches`, `lfs`, `other`); the
`agentfs.not_built{capability}` gauge counts them and `AgentfsNotBuilt`
alerts on it ([operator settings](trees.md#operator-settings),
[telemetry](telemetry.md)). A recording command run on a tree volume exits
1 with ``error: `agentfs <command>` runs on the recording engine only;
<state> is a tree volume, where it is not built yet``.

| Value | Status on tree volumes | What says so |
|---|---|---|
| Operator rules: `info/exclude`, `-agentfs` in `info/attributes`, the untracked area | **built** ([trees](trees.md#rules-and-the-untracked-area)) | |
| Operator settings as git-config includes (`init --include`) | **built** ([trees](trees.md#operator-settings)) | |
| Restart in place after a crash (`--on-stale-mount recover`) | **built** ([trees](trees.md#restarting-after-a-crash)) | |
| A command run in the mount (`mount ROOT -- COMMAND`) | **built** ([trees](trees.md#running-a-command-in-the-mount)) | |
| Pushing to a remote (`[remote]`, `agentfs.pushInterval`, `agentfs.forkRefs`, `http.proxy`), and `push` | **not built**: history stays in the state area | `not built: remote.origin.url (push): this volume pushes to no remote yet; its history stays in the state area`; `agentfs.not_built{capability="push"}`; `push` and `status` refuse |
| Patches (`agentfs.patch*`, `[agentfsPusher]`), `patch`, `serve-git` and patch notices | **not built**: queued patches are not applied | `not built: agentfs.patchremote (patches): queued patches are not applied`; `capability="patches"`; the commands refuse |
| `restore` and `revert` | **not built** | the commands refuse; the history is plain git in `<state>/history.git` |
| Git LFS (`agentfs.lfsThreshold`, `lfs.*`, `filter=lfs`) | **not built** | `capability="lfs"`; a `filter=lfs` rule refuses the volume, naming its line; `lfs` refuses |
| Database groups (`agentfs.commit=fsync`, `agentfs.group`) and every other `agentfs.*` attribute | **not built** | the volume is refused at open, naming the line |
| The sandbox overlay | **not built** | no launcher flag offers it |
| Forked volumes, fork delivery, unlinked forks, hidden fork history | **not built**; a tree volume's own forks are built (section 1.2) | `fork create`, `fork deliver` and the other `fork` commands except `fork end` refuse; `agentfs.forkRefs` is logged as `push` |
| Recording without a mount (`run`, `record`, `flush`) and checkpoints | **not built** | the commands refuse |
| State areas on NFS | **not built** | not tested on tree volumes |
| Every other key in the settings | **not built** | `capability="other"`: `tree volumes do not read this setting`. `agentfs.nested = exclude` is built and not listed |

The CSI driver on tree volumes ([csi.md](csi.md)):

| Value | Status | What says so |
|---|---|---|
| A claim's own tree volume on one node (`profile`, `volumeName`), made by `agentfs init` at CreateVolume | **built** ([csi.md](csi.md#7-what-each-csi-call-does)) | |
| One `agentfs host` per volume in use; each pod's view at the kubelet's target with its pod's trailers; restart with views replayed | **built** ([csi.md](csi.md#2-architecture)) | |
| DeleteVolume keeping the directory (`onDelete: retain`) | **built** | `kept volume <pv> at <dir>: ...` in the plugin's log |
| A static PV over a kept volume | **built** ([csi.md](csi.md#5-parameters)) | |
| Fork claims (`fork: "true"`, `landing: automatic`): an in-volume fork landing in its source, ended at DeleteVolume | **built** ([csi.md](csi.md#14-forks)) | |
| Deleting a volume (`onDelete: delete`) | **not built**: the directory is kept | CreateVolume: `onDelete: delete is not built on tree volumes: ...` |
| Several pods on one claim | **not built** | NodePublish: `several pods on one volume are not built` |
| A second pod of a fork claim after its writer detached | **not built** | the fork `has had its writer` |
| Moving a volume (`startFrom`) | **not built** | CreateVolume: `startFrom is not built on tree volumes: ...` |
| `workloadMounts` | **not built** | CreateVolume: `workloadMounts is not built on tree volumes: ...` |
| Unlinked forks (`link: unlinked`, `remotes`) | **not built** | CreateVolume: `unlinked forks are not built` |
| Landing approvals (`landing: approval`) | **not built** | CreateVolume, from Trees: `landing approvals are not built` |
| Forks of forks | **not built** | CreateVolume, from Trees: `forks of forks are not built` |
| Push, the patch queue at publish, LFS, restore and revert | **not built** | each host's `not built:` lines and `agentfs.not_built` |
| Recording-format volumes made by an older driver | **not supported**: never served | NodePublish and DeleteVolume: `FAILED_PRECONDITION` naming the directory and [moving.md](runbooks/moving.md); the plugin logs each at start |
| Several nodes (`ReadWriteMany`, `ReadOnlyMany`) | **not supported** | `multi-node access modes are not supported` |

## 2. Example configurations

Each example names its dimension values, shows a setup with real keys
and flags, and says what the workload sees, what history and remotes
show, and its limits. Placeholders are in angle brackets.

### Example 1: one Docker workload, no remote

**For:** a developer or a single-host service that wants every change
on record and nothing leaving the host.

| Dimension | Value |
|---|---|
| Integration point | filesystem (the agentfs container runs `agentfs init` and `agentfs mount` on a tree volume) |
| Sharing | one process, or several workload containers on one mount |
| Remotes | none |

```sh
examples/docker/docker-run.sh up          # AGENTFS_CONFIG defaults to config/local: no [remote]
examples/docker/docker-run.sh workload --name app --user 10000:10000 --cap-drop ALL \
  debian:trixie-slim sh -c 'echo hello > /agentfs/workspace/hello.txt; sleep infinity'
docker exec agentfs-example git --git-dir /data/state/history.git log --stat root
```

- **The workload sees** an ordinary directory, `/agentfs/workspace`. It
  never sees the history, which is in a Docker volume only the agentfs
  container mounts ([docker.md](docker.md#2-the-layout)).
- **History:** the tree's branch `root` in `history.git` in that Docker
  volume ([trees](trees.md)).
- **Limits:** Linux with Docker Engine 25 or later; not Docker Desktop.
  The history is lost with the Docker volume unless the volume is backed
  up: tree volumes push to no remote yet, so adding one keeps nothing
  ([docker.md](docker.md#6-volumes-with-no-remote)).

### Example 2: a Kubernetes sidecar with a GitHub remote configured (push not built)

**For:** one pod whose directory should be on record in a GitHub
repository.

| Dimension | Value |
|---|---|
| Integration point | filesystem, packaged as the sidecar (tree views) |
| Sharing | one pod, one mount |
| Remotes | one (GitHub): **not built** on tree volumes |

`deploy/sidecar/config/config`, with the GitHub repository, which
`init --include` makes one of the operator settings' includes
([deployment.md](deployment.md#4-configuration)):

```ini
[remote "origin"]
	url = https://github.com/<owner>/<history repo>.git
	agentfsCredentials = /run/secrets/agentfs/origin
```

- **The workload sees** the FUSE mount in its container
  ([deployment.md](deployment.md#3-the-pod-piece-by-piece)).
- **History:** the branch `root` in `/data/state/history.git`, on the
  pod's claim. Nothing reaches GitHub: pushing is **not built** on tree
  volumes. Each boot logs `not built: remote.origin.url (push): this
  volume pushes to no remote yet; its history stays in the state area`,
  one line per remote key, and `agentfs.not_built{capability="push"}`
  raises `AgentfsNotBuilt` for as long as the remote stays configured
  (section 1.5).
- **Limits:** a privileged sidecar
  ([deployment.md](deployment.md#9-security-trade-offs-of-the-privileged-sidecar));
  one pod per volume.

### Example 3: Hermes as the deploy example describes it

**For:** an agent that edits its own skills,
persona and profiles, whose configuration iac also changes.

| Dimension | Value |
|---|---|
| Integration point | filesystem, as the sidecar (tree views) |
| Sharing | several processes on one mount: Hermes and the docker-in-docker sidecar that runs its tool containers |
| Remotes | one; it is the patch remote; iac is the patch producer: pushing and patches **not built** on tree volumes |
| Knobs | the Hermes rule files under `history.git/info`: an allowlist `exclude`, `-agentfs` on secrets, databases, sessions, logs and checkpoints |

The files are in [`deploy/examples/hermes`](../deploy/examples/hermes),
and the pod changes are in
[deployment.md section 10](deployment.md#10-hermes).

- **The workload sees** `/opt/data` as before. Tool containers see the
  same files when Hermes' `terminal.docker_volumes` passes the mount in.
- **History:** the agent's edits to recorded paths, as saves on the
  branch `root` in the state area. Secrets, databases, sessions and logs
  live in the tree's untracked area and never reach history
  ([trees](trees.md#rules-and-the-untracked-area)).
- **Not built:** iac's patches and the push. A patch iac queues is not
  applied; each boot logs `not built: agentfs.patchremote (patches):
  queued patches are not applied` and a `push` line per remote key, and
  `agentfs.not_built` raises `AgentfsNotBuilt` (section 1.5).
- **Limits:** whether tool containers get the mount is the deployment's
  policy choice; with `terminal.docker_volumes` empty they still write
  their home into the volume (never recorded). Check the profile layout
  against the Hermes version you run.

### Example 4: several pods on one CSI volume

**Not built.** One claim's volume serves one pod: a second pod's
NodePublish of the same claim is refused with `volume <pv> is published at
<target> on this node: one pod per volume; several pods on one volume are
not built`, a `FailedMount` event on the pod
([csi.md](csi.md#6-using-a-volume)).

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter |
| Sharing | one pod per claim; **not built** for several |
| Remotes | the profile's, logged as not built |

The example is [`deploy/csi/example`](../deploy/csi/example): the
`agentfs` StorageClass (`profile: example`, `onDelete: retain`,
`volumeBindingMode: WaitForFirstConsumer`), a `ReadWriteOncePod` claim,
and a Deployment of one replica with `strategy: Recreate`.

- **The workload sees** its claim's files at its mount, a view of the
  volume's root.
- **History:** the branch `root` in `<base>/<pv>/state/history.git` on
  the node. Each save carries `Agentfs-Pod`, `Agentfs-Namespace`,
  `Agentfs-Pod-Uid` and `Agentfs-Node` trailers.
- **Limits:** one node for good; draining it leaves the pod Pending.
  Multi-node access modes are refused. A RollingUpdate on a
  `ReadWriteOnce` claim stalls on the one-pod refusal. A second pod that
  needs the files gets a fork claim (example 5)
  ([csi.md](csi.md#12-limitations)).

### Example 5: a fork claim per tool call, landed in its source

**For:** an agent that runs a tool call in a pod of its own, writing a
fork of its volume whose saves land in the volume as they are made.

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter (a fork class) |
| Sharing | the source: one pod; the fork claim: one pod, for its life |
| Remotes | none: the fork lives in its source's volume |

[`deploy/csi/example/fork-storageclass.yaml`](../deploy/csi/example/fork-storageclass.yaml)
is the fork class (`fork: "true"`, `landing: automatic` by default). The
tool pod gets a pod-scoped claim on it whose `dataSource` is the source's
claim, and pod affinity to the source's pod:

```yaml
volumes:
  - name: fork
    ephemeral:
      volumeClaimTemplate:
        spec:
          storageClassName: agentfs-fork
          dataSource: { kind: PersistentVolumeClaim, name: <source claim> }
          accessModes: [ReadWriteOncePod]
          resources: { requests: { storage: 1Gi } }
```

- **The fork pod sees** the source's files as of CreateVolume, then its
  own writes and the source's saves as they land.
- **History:** a keyed fork of the source's root in the source's
  `history.git`, under the fork claim's PV name; each of its saves
  carries the fork pod's trailers and lands in the source's branch
  `root` automatically, so the source's pod sees it
  ([trees](trees.md#forks-keys-end-and-the-drop)). Deleting the pod
  deletes its claim, which ends the fork; it drops once its last save
  has landed.
- **Limits:** the fork pod must run on the source's node. One pod per
  fork claim: once its writer detaches, a new pod is refused (`has had
  its writer`). `landing: approval` is refused (`landing approvals are
  not built`), so every save lands; `link: unlinked` is refused
  (`unlinked forks are not built`). There is no fork list, delivery or
  review step on tree volumes ([csi.md](csi.md#14-forks)).

### Example 6: forks of forks

**Not built** on tree volumes. A CSI fork claim whose `dataSource` is
another fork claim stays Pending with a `ProvisioningFailed` event:
`forks of forks are not built` (Trees refuses it). The first fork keeps
working and landing in its source. `ci/e2e-csi-fork-of-fork.sh` asserts
this refusal ([csi.md](csi.md#14-forks)).

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter (a fork class claim on a fork claim) |
| Sharing | each fork: one pod |
| Remotes | none |

On recording state areas, `agentfs fork create` from a linked fork's state
area still makes a fork of a fork, whose change rolls up into its parent
([patches](patches.md#forks-of-forks),
[the runbook](runbooks/patch-queue.md#9-forks-of-forks)); no launcher runs
it.

### Example 7: several volumes sharing one repository

**For:** many agents whose histories should sit in one repository, one
branch each.

| Dimension | Value |
|---|---|
| Integration point | any |
| Sharing | one volume per agent |
| Remotes | one URL for all; a unique `agentfs.volume` each |

Each volume's config names the same remote:

```ini
[remote "origin"]
	url = https://git.example.com/acme/agents-history.git
	agentfsCredentials = /run/secrets/agentfs/origin
```

Under CSI, the default `volumeName` template already makes each claim's
name unique. Tree volumes (`agentfs init --volume agent-a`) push to no
remote yet (section 1.5).

- **Each workload sees** only its own volume.
- **History:** `refs/heads/agentfs/agent-a` and `agentfs/agent-b`, with
  each volume's patch queue under its own `refs/agentfs/<volume>/`. Pushes from different volumes never contend.
- **Limits:** a token is scoped to the repository, so every volume can
  write every other's refs. agentfs detects another writer as a
  divergence; it does not prevent one. Two volumes with the same name see
  each other as divergence.

### Example 8: a volume's branch reviewed as a GitHub pull request

**For:** a team that reviews what an agent changed with GitHub's tools.

| Dimension | Value |
|---|---|
| Integration point | filesystem or Kubernetes adapter |
| Sharing | any |
| Remotes | GitHub, as example 2; the forge is in the loop |

agentfs pushes `agentfs/<volume>`. A person opens the pull request:

```sh
gh pr create --repo <owner>/<history repo> --head agentfs/<volume> --base <review base>
```

For a fork's change, `agentfs fork deliver <fork> --to branch` creates
the pull request's head `agentfs-review/<fork>` (one squashed commit on
base, or with `--onto <branch>` merged onto that branch's head). A person
or CI still opens the pull request.

- **The workload sees** nothing of the review.
- **History:** the pull request shows the volume's commits with their
  `Agentfs-*` trailers. What GitHub does with it (CI, review, merge into
  another branch) is GitHub's; agentfs never reads it.
- **Limits:** agentfs is the only intended writer of
  `agentfs/<volume>`. A commit pushed to that branch on GitHub, a review
  suggestion committed there included, is another writer: agentfs saves
  its head under `refs/agentfs/<volume>/diverged/*`, raises `Diverged`,
  and every later push of the branch fails as `non_fast_forward` until
  an operator resolves it ([divergence.md](runbooks/divergence.md)).
  Recording carries on. Changes the reviewers want
  in the volume go through the patch queue (`agentfs patch submit`) and
  land at its next start. Whether GitHub runs CI on a hidden fork ref is
  open.

### Example 9: an operator restores or reverts

**For:** an operator undoing an agent's mistake.

| Dimension | Value |
|---|---|
| Integration point | filesystem (`restore`, `revert`, over the control socket when a daemon runs); on tree volumes **not built** |
| Sharing | any |
| Remotes | any; nothing is fetched |

Find the commit with `git log` in the state area or the remote's web
view, pause the workload when you can, then:

```sh
agentfs --state <state> restore notes/plan.md <commit> --dry-run
agentfs --state <state> restore notes/plan.md <commit>
agentfs --state <state> revert <commit>
```

- **The workload sees** the restored files at once when the history
  service writes them.
- **History:** one commit per operation, `restore <path> from <id>` or
  `revert <id>: <subject>`, with `Agentfs-Trigger: restore` and the
  operator as author.
- **Limits:** the commit must be on the volume's branch; a diverged ref
  is restored with plain git. A workload writing the same files can
  overwrite the restore (its writes are recorded too). While `agentfs run`
  or `record` holds the volume, the command exits 75. On a tree volume
  (the sidecar, Docker, the CLI, the CSI driver) both commands refuse, as
  not built.
  Steps: [recovery-and-undo.md](runbooks/recovery-and-undo.md).

### Example 10: a CSI volume continued on another node

**Not built.** A tree volume stays on the node that created it. A class
with `startFrom` is refused at CreateVolume (`startFrom is not built on
tree volumes: a volume starts empty; moving a volume between nodes is not
built (docs/runbooks/moving.md)`), and the claim stays Pending with a
`ProvisioningFailed` event.

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter |
| Sharing | the new claim: one pod, on the new node |
| Remotes | none: push is not built |

- **What an operator can do:** copy the files out through the old
  volume's mount into a new claim on the other node; the history does
  not continue, and the old directory stays kept on its node
  ([moving.md](runbooks/moving.md)).
- **A recording-format volume** made by an older driver is never served;
  the same runbook archives its history and moves its files onto a tree
  volume.

### Example 11: two remotes, one of them a backup, with LFS

**For:** a volume pushed to a forge and to an internal backup, with
large files kept out of git.

| Dimension | Value |
|---|---|
| Integration point | any |
| Sharing | any |
| Remotes | several; the patch remote named explicitly |
| Knobs | `agentfs.lfsThreshold`, per-remote limits |

```ini
[agentfs]
	patchRemote = origin
	lfsThreshold = 8m

[remote "origin"]
	url = https://github.com/<owner>/<history repo>.git
	agentfsCredentials = /run/secrets/agentfs/origin
	agentfsMaxBlobSize = 100m

[remote "backup"]
	url = ssh://git@backup.internal/agents/history.git
	agentfsSshKey = /run/secrets/agentfs/backup-key
	agentfsKnownHosts = /run/secrets/agentfs/known_hosts
```

- **The workload sees** no difference.
- **History:** the same branch on both remotes, each pushed on its own
  thread with its own state and alarms. Content over 8 MiB is stored as
  LFS and uploaded to each remote's LFS endpoint
  ([endpoints](configuration.md#endpoints)).
- **Limits:** the smallest blob limit across remotes applies to
  recording. Without `agentfs.patchRemote` two remotes mean no patch
  remote. The SSH remote needs a binary built with the `ssh` feature, and
  each remote needs an LFS server
  ([configuration](configuration.md#remote-name)).

### Example 12: the sandbox overlay

**Not built** on any launcher. The overlay is a second mount point that
shows sandboxes only the files the rules would record, keeping other new
files in a private tmp layer. Tree volumes do not build it, and neither
the sidecar's manifests and chart nor the CSI driver offer it ([deployment.md](deployment.md#12-the-sandbox-overlay)). A
program linking `agentfs-fuse` turns it on with `MountConfig::overlay`
([library/filesystem](library/filesystem.md#the-sandbox-overlay)); what it
covers and what it does not is in
[guarantees](guarantees.md#the-sandbox-overlay), and the limits it would
close are in [Overlay gaps](target-architecture/future/sandbox-overlay-gaps.md).

## 3. How to use this page

- **Check a design against every example it touches**, not only the one
  it was written for. A change to how patches apply touches examples 3,
  8 and 11; a change to the front protocol touches every restore through
  the history service in example 9; a change to the sandbox overlay
  touches example 12. A change to tree volumes touches examples 1 to 6
  and 10 and section 1.5; a change to the CSI driver or `agentfs host`
  touches examples 4, 5, 6 and 10.
- **Say which world the design lives in.** File semantics (locks,
  renames, caches) belong to section 1.2. Merging, order and gating
  belong to section 1.3. A design that needs both, such as landing a
  patch into a volume several pods use, must hold in both.
- **Hermes combines several examples.** As deployed by the example it is
  example 3 (several processes; iac patches, not built on its sidecar)
  and can be example 8 (its branch reviewed on GitHub, which needs a
  push). Its first deploy on a CSI claim is example 4 with a `hermes`
  profile, and a tool call's fork claim is example 5.
- **Never count an unbuilt value as available.** If a design needs a
  value marked **not built**, such as landing into a running volume, say
  so.
- **Add an example** when a new consumer or a new combination appears,
  with the same fields, and mark every value.
