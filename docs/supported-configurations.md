# Supported configurations

The ways agentfs can be put together, as reference material to check a
design against. Section 1 lists the dimensions a configuration varies
on. Section 2 gives named example configurations, each a combination of
those values. Section 3 says how to use them.

This page links to the homes of each fact rather than restating them.
[design.md](design.md) is authoritative, and every key and flag is in
the [configuration reference](configuration.md). Section numbers (§)
refer to design.md.

Every value is marked:

| Mark | Meaning |
|---|---|
| **built** | In the code today; the cell names its design section, key or flag |
| **designed, not built** | Part of the merge-queue design the owner approved on 2026-10-29 (bead agentfs-mia, design §19.2 item 11, [open questions](design.md#192-open-questions)). Names are provisional. Do not rely on it |
| **not supported** | Refused by a recorded decision (§19.1) |

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

## 1. Dimensions

### 1.1 Integration point

Where a consumer taps in. agentfs has three layers; a higher one uses
the lower ones ([architecture.md](architecture.md#crates)).

| Layer | Crate | What a consumer uses | Status |
|---|---|---|---|
| Library | `agentfs-core` | Rust calls with no mount and no agentfs process: opening a state area and recording (`volume`, the `Recorder`), restore and revert planning (`restore`), the patch queue (`patch`), forks (`fork::create`, `fork::finish`, `fork::listing`), delivering a fork's change through the `Delivery` contract (`fork::deliver`), the deletion decider (`release::decide`) | **built** (module map: [architecture.md](architecture.md#agentfs-core)) |
| Filesystem | `agentfs-fuse` and the `agentfs` binary | `agentfs mount` (the FUSE mount); `run` (the commit and push loops without a mount); `serve-history` with `mount --history-service` fronts (§14.1); one-shot commands: `init`, `record`, `flush`, `push`, `status`, `probe`, `restore`, `revert`, `patch submit`, `patch apply`, `patch status`, `fork create`, `fork finish`, `fork status`, `fork abandon`, `fork list`, `fork deliver`, `start accept`, `lfs fetch`, `lfs prune` | **built** ([flags](configuration.md#flags-and-environment)) |
| Filesystem | the control socket, `meta/control/agentfs.sock` | `push` (push now), `restore` and `revert` against a running daemon; JSON lines, version 1, root or the daemon's user only | **built** (§2.3, §15) |
| Filesystem, packaged for Kubernetes | the sidecar manifests and the sidecar library chart | the binary as a privileged native sidecar, one pod per volume | **built** ([deployment.md](deployment.md), [helm.md](helm.md)) |
| Kubernetes adapter | `agentfs-csi` | StorageClass parameters (`profile`, `volumeName`, `onDelete`, `startFrom`, `workloadMounts`, `fork`, `link`, `remotes`), profiles in a ConfigMap, credentials in a Secret, claims, DeleteVolume's release decider | **built** ([parameters](configuration.md#the-csi-driver-agentfs-csi), §14.2) |
| Every layer | what agentfs writes for others to read | the history branch and `refs/agentfs/<volume>/*` on remotes (§11.4); the patch status ref; OpenTelemetry metrics, traces and alarms (§13); `status`, `patch status --json` and `fork status --json` | **built** |
| Every layer | checkpoint: an integrator says a batch of work is done, which cuts one commit for it | | **designed, not built** |
| Every layer | notify (agentfs tells an integrator a patch arrived) and apply (an integrator tells agentfs to write waiting patches now) | | **designed, not built** |
| Filesystem | `agentfs serve-git`: agentfs as a git server that takes pushes | | **designed, not built** |
| Kubernetes adapter | a git endpoint per node plugin for offers, approvals and a sidecar's checkpoints | | **designed, not built** |

### 1.2 Sharing topology

How many clients use one volume, and through what. This is the
sharing-one-volume world.

| Value | How | What clients get | Status |
|---|---|---|---|
| One process, one mount | `agentfs mount BACKING MOUNTPOINT`; one workload process | Every change boundary recorded before the call returns (§3.1, §3.2) | **built** |
| Several processes, one mount | Several processes or containers use one FUSE mount: one pod's containers (deployment.md §10, Hermes and its tool containers), or several Docker workload containers on one host mount ([docker.md](docker.md#2-the-layout)) | One kernel and one daemon. Grouped commits carry one `Agentfs-Uid` and `Agentfs-Pid` line per writer (§9) | **built** |
| Several pods, one volume, one node | CSI only: one front per pod (`mount --history-service`), one history service per volume (`serve-history`) | Files shared with the backing filesystem's semantics: two pods writing one file is last writer wins; POSIX locks are not shared between pods, so a SQLite database is written from one pod ([csi.md](csi.md#6-using-a-volume)). A commit never holds two pods' changes; each pod has its own committer (§9, §14) | **built** (§14.1, §14.2) |
| Several pods through the sidecar | | The sidecar is one pod per volume ([deployment.md](deployment.md#11-limitations)) | use the CSI driver |
| Several nodes, one volume | `ReadWriteMany`, `ReadOnlyMany` | Refused: the claim stays Pending. Pods on other nodes read the history from a remote | **not supported** (§19.1, 2026-10-15) |
| A volume moving between nodes | | A worktree stays on its node for good. Continuing elsewhere is a new volume started from the old branch (example 10) | **not supported** as a move (§14.2, [moving.md](runbooks/moving.md)) |

### 1.3 Remote topology

Where history goes and what comes back. This is the
exchanging-through-git world.

| Value | How | What happens | Status |
|---|---|---|---|
| No remote (local-only) | No `[remote "<name>"]` section | Records and commits; pushes nothing; no alarm. A remote added later gets all the history. The history dies with the disk ([remotes modes](configuration.md#remotes-modes)) | **built** (§11.1, §19.1 "Zero remotes") |
| One remote | One section with `url` | Pushed every `agentfs.pushInterval`. It is also the patch remote by default | **built** (§11.1, §11.2) |
| Several remotes | Several sections | Each has its own push thread, push state and alarms. The smallest `agentfsMaxBlobSize` and `agentfsMaxLfsObjectSize` apply to recording. There is no patch remote unless `agentfs.patchRemote` names one | **built** (§11.1, §11.3, §12) |
| Volumes sharing one repository | Several volumes, one remote URL, unique `agentfs.volume` names | Each writes only `refs/heads/agentfs/<volume>` and `refs/agentfs/<volume>/*`. Another writer is detected as a divergence, not prevented | **built** (§11.4) |
| Linked fork | `agentfs fork create` (default `--link linked`), or a CSI fork class | Pushes its history to the fork remote: the section with `agentfsForkRemote = true`, else the source's patch remote. A local-only source's forks publish into the source's own history. On finish it publishes one change on its base; the receiver gates it | **built** (§11.5) |
| Unlinked fork | `fork create --link unlinked [--remotes FILE]`, or a fork class with `link: unlinked` and `remotes` | Never delivers to its source and never pushes to the source's remotes. With no remotes of its own it is local-only and lost when deleted, the owner's choice | **built** (§11.5 "The link", §19.1 "Unlinked fork remotes") |
| Hidden fork history | `agentfs.forkRefs = hidden` in the source | New forks keep their history at `refs/agentfs/<fork>/history`, not under `refs/heads/` | **built**; whether a forge runs CI on it is open (§19.2 item 9) |
| Forks of forks | | `fork create` refuses a source that is a fork (`SourceIsFork`) | **designed, not built** (bead agentfs-mia.7) |
| Started from another worktree's branch | CSI start class, `startFrom: "<remote>:<branch>@<commit>"` | A new volume with its own branch whose history begins at that commit. `StartBranchMoved` alarms if the old branch gains commits | **built** (§11.3, §14.2) |
| Patch producers | iac, CI or a person: `agentfs patch submit` or plain git, to `refs/agentfs/<volume>/inbox/<n>` on the patch remote | Applied in order when the volume starts, stopping at the first that fails; status in `refs/agentfs/<volume>/status` | **built** (§12, [patches.md](patches.md)) |
| A fork's change delivered | `agentfs fork deliver <fork> --to inbox\|branch\|command\|webhook`, run by the receiver with its own credentials | `inbox` queues it as a patch; `branch` creates a pull request's head branch; `command` and `webhook` hand the payload to a program. The CSI driver never delivers | **built** (§11.5 "Delivery", [forks.md](forks.md#delivering-a-change)) |
| A forge in the loop (GitHub, Gitea) | An ordinary remote | agentfs pushes its own refs. It never opens, reads, reviews or merges a pull request. Anyone writing the volume's branch there is another writer: a divergence (§11.3) | **built** as a plain remote; nothing forge-specific |
| A patch merged live into a running volume | | Today patches are applied only at start (§12) | **designed, not built** |
| A patch that is a branch; a conflicting patch held while the rest go on; barriers | | Today a patch is one commit, and the first failure blocks the queue (§12) | **designed, not built** |

### 1.4 Knobs that change commits, pushes and patches

| Knob | Set in | What it changes | Status |
|---|---|---|---|
| `agentfs.commitWindow` | config, default `200ms` | Records this close in time become one commit. Grouping changes how history reads, not what is kept (§3.4) | **built** |
| Fixed commit boundaries | none | Restores, reverts and patches are commits of their own. A commit never mixes pods. With `agentfs.nested = working-tree`, a nested repository's `index.lock` bounds a group (§3.4, §9) | **built** |
| `agentfs.commit=fsync`, `agentfs.group`, `agentfs.coalesce` | `info/attributes` only | Databases: a group (`app.db`, `-wal`, `-journal`) recorded whole as one atomic record at `fsync`, by a timer, at its last close and at unmount; up to one `agentfs.coalesce` late (§8) | **built** |
| `agentfs.sweepInterval` | config, default `30s` | The backstop that records files held open and retries failed records (§3.1, §3.5) | **built** |
| `agentfs.pushInterval` | config, default `60s` | How often each remote is pushed, and the base of the failure backoff (§11.2) | **built** |
| Push now | `agentfs push` against a running daemon | Commits what is recorded and wakes each remote's push at once (§2.3, §11.2) | **built** |
| `--push-grace` | `mount`, `run`, `serve-history`, default `5s` | One last push of the final commit at shutdown; `0s` skips it (§11.2) | **built** |
| `agentfsMaxPushSize` | per remote | A backlog too large for one push goes out in first-parent steps (§11.3) | **built** |
| `agentfsMaxBlobSize`, `agentfsMaxLfsObjectSize` | per remote | Content over the smallest limit is refused at record time (`PathRefused`) unless stored as LFS (§3.4, §11.3) | **built** |
| `--passthrough auto\|on\|off` | `mount`; `agentfs-csi --passthrough` for fronts | Whether data reads and writes bypass the daemon (§2.1). `fsync`-class paths never use passthrough (§3.1) | **built** |
| Git LFS: `agentfs.lfsThreshold`, `agentfs.store`, `filter=lfs`, `lfs.url`, `remote.<name>.lfsurl` | config, `info/attributes` | Which content is stored as LFS and where it is uploaded ([Git LFS](configuration.md#git-lfs), §7) | **built** |
| Path rules: `info/exclude`, `-agentfs`, `agentfs.maxSize` | the git directory, set by the operator | Which new paths are recorded; `-agentfs` is never recorded even if tracked (§4) | **built** |
| `agentfs.respectTreeRules` | config, default `false` | Whether tracked `.gitignore` and `.gitattributes` take part in path rules. They can never set `agentfs.*` attributes (§4.1) | **built** |
| `agentfs.nested` | config, default `exclude` | Nested git repositories: record nothing in them, or their working files (§5) | **built** |
| `agentfs.repackInterval` | config, default `1h` | Storage only: repacks history; changes no commit (§3.4) | **built** |
| `agentfs.patchRemote`, `agentfs.patchFetchTimeout` | config | Where the inbox is fetched from and the queue status pushed; how long start waits for the fetch (§12) | **built** |
| When patches are applied | none | At `mount`; at `serve-history` start with no fronts running, which under CSI is NodeStage; by `agentfs patch apply` with the volume stopped. `agentfs run` does not apply them (§12, §14.2) | **built** |
| `patch submit --replace N`, `--skip N` | the producer | Unblocks a queue that stopped at patch N (§12) | **built** |
| `agentfs.forkRefs`, `--link`, `--remotes`, `onDelete` | source config; `fork create`; StorageClass | Where forks push, whether they deliver back, and what deleting a claim does (§11.5, §14.2) | **built** |
| `[agentfs "identity"]` templates | config | Committer and author of each commit (§9) | **built** |
| Restore and revert modes | `agentfs restore`, `revert`; `--dry-run`, `--force` | Through a running mount, through the history service, or directly with `--root` when nothing runs. Each is one commit. `run` serves none and the command exits 75 (§15) | **built** |
| Checkpoint | | An integrator cuts one commit per change intent | **designed, not built** |
| Notify and apply | | A hook on patch arrival (`patch-arrived`), and an apply call to write waiting patches into a running volume | **designed, not built** |
| Acceptance rule | | Whether an integrated patch may apply: a clean merge (the default), an approval, or a command | **designed, not built** |
| Merge or rebase | | How a patch is integrated, by the `git` program | **designed, not built** |
| Which line a remote gets | | The recorded branch (today) or a change line of checkpoint commits | **designed, not built** |

## 2. Example configurations

Each example names its dimension values, shows a setup with real keys
and flags, and says what the workload sees, what history and remotes
show, and its limits. Placeholders are in angle brackets.

### Example 1: one Docker workload, no remote

**For:** a developer or a single-host service that wants every change
on record and nothing leaving the host.

| Dimension | Value |
|---|---|
| Integration point | filesystem (the agentfs container runs `agentfs mount`) |
| Sharing | one process, or several workload containers on one mount |
| Remotes | none |

```sh
examples/docker/docker-run.sh up          # AGENTFS_CONFIG defaults to config/local: no [remote]
examples/docker/docker-run.sh workload --name app --user 10000:10000 --cap-drop ALL \
  debian:trixie-slim sh -c 'echo hello > /agentfs/workspace/hello.txt; sleep infinity'
docker exec agentfs-example git --git-dir /data/state/git log --format='%h %an %s' agentfs/example
```

- **The workload sees** an ordinary directory, `/agentfs/workspace`. It
  never sees the history, which is in a Docker volume only the agentfs
  container mounts ([docker.md](docker.md#2-the-layout)).
- **History:** the branch `agentfs/example` in that Docker volume.
  `agentfs status` says `remotes:   local-only (no remote)`.
- **Limits:** Linux with Docker Engine 25 or later; not Docker Desktop.
  The history is lost with the Docker volume unless it is backed up or a
  remote is added ([docker.md](docker.md#6-volumes-with-no-remote)).

### Example 2: a Kubernetes sidecar pushing to GitHub

**For:** one pod whose directory should be on record in a GitHub
repository.

| Dimension | Value |
|---|---|
| Integration point | filesystem, packaged as the sidecar |
| Sharing | one pod, one mount |
| Remotes | one (GitHub); it is also the patch remote |

`deploy/sidecar/config/config`, with the GitHub repository:

```ini
[agentfs]
	pushInterval = 60s

[remote "origin"]
	url = https://github.com/<owner>/<history repo>.git
	agentfsCredentials = /run/secrets/agentfs/origin
```

- **The workload sees** the FUSE mount in its container
  ([deployment.md](deployment.md#3-the-pod-piece-by-piece)).
- **History:** `agentfs/<volume>` on GitHub, pushed every 60 s and at
  shutdown within `--push-grace`. The credentials file holds a token
  (or `username=` and `password=` lines) and is re-read when the Secret
  rotates (§11.1).
- **Limits:** a privileged sidecar
  ([deployment.md](deployment.md#9-security-trade-offs-of-the-privileged-sidecar));
  one pod per volume. A failed push alarms and retries; recording carries
  on (§11.3).

### Example 3: Hermes as the deploy example describes it

**For:** the first consumer (§16): an agent that edits its own skills,
persona and profiles, whose configuration iac also changes.

| Dimension | Value |
|---|---|
| Integration point | filesystem, as the sidecar |
| Sharing | several processes on one mount: Hermes and the docker-in-docker sidecar that runs its tool containers |
| Remotes | one; it is the patch remote; iac is the patch producer |
| Knobs | the Hermes rule files: an allowlist `exclude`, `-agentfs` on secrets, databases, sessions, logs and checkpoints; `agentfs.nested = exclude` |

The files are in [`deploy/examples/hermes`](../deploy/examples/hermes),
and the pod changes are in
[deployment.md §10](deployment.md#10-hermes). iac queues a change in its
clone of the history repository, then restarts the pod:

```sh
agentfs patch submit --volume hermes --put config.yaml=./config.yaml -m "<why>"
kubectl delete pod <hermes pod>
```

- **The workload sees** `/opt/data` as before. Tool containers see the
  same files.
- **History:** the agent's edits to recorded paths, grouped by
  `agentfs.commitWindow`; each applied patch as its own commit, authored
  by the patch's author, with `Agentfs-Trigger: patch`.
- **Limits:** patches apply only at a pod restart (deployment.md
  [Restarts and patches](deployment.md#restarts-and-patches)). A blocked
  queue does not stop the pod; alert on `agentfs.patch.blocked`. Passing
  the mount into tool containers changes iac's `docker_volumes` pin,
  which is the owner's call. Check the profile layout against the Hermes
  version you run.

### Example 4: several pods on one CSI volume

**For:** replicas or cooperating agents that work in one directory on
one node, without privileges in the workload.

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter |
| Sharing | several pods, one volume, one node: a front per pod, one history service |
| Remotes | the profile's |

The example is [`deploy/csi/example`](../deploy/csi/example): the
`agentfs` StorageClass (`profile: example`, `onDelete: retain`,
`volumeBindingMode: WaitForFirstConsumer`), a `ReadWriteOnce` claim, and
a Deployment with two replicas.

- **The workload sees** the files with the backing filesystem's
  semantics. Two pods writing one file is last writer wins. POSIX locks
  are not shared between pods
  ([csi.md](csi.md#6-using-a-volume)).
- **History:** `agentfs/<pvcNamespace>.<pvcName>.<pvName>`. Each commit
  is one pod's, with that pod as committer and `Agentfs-Pod`,
  `Agentfs-Namespace` and `Agentfs-Node` trailers.
- **Limits:** one node for good; draining it leaves the pods Pending.
  Multi-node access modes are refused. A boundary waits for the history
  service, at most `--history-timeout`, then fails with `EIO`
  ([csi.md](csi.md#12-limitations)).

### Example 5: a sandbox fork per tool call, landed in its source

**For:** an agent that runs each risky tool call in a throwaway copy of
its volume and keeps the result only after review.

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter (fork class) for the sandbox; filesystem (`fork list`, `fork deliver`) for the receiver |
| Sharing | the source: any; the sandbox: one pod |
| Remotes | a linked fork; the fork remote is the profile's `agentfsForkRemote` section, else the source's patch remote |

[`deploy/csi/example/fork-storageclass.yaml`](../deploy/csi/example/fork-storageclass.yaml)
is the fork class. The sandbox pod gets a pod-scoped claim on it whose
`dataSource` is the source's claim, and pod affinity to the source's
pods:

```yaml
volumes:
  - name: sandbox
    ephemeral:
      volumeClaimTemplate:
        spec:
          storageClassName: agentfs-fork
          dataSource: { kind: PersistentVolumeClaim, name: <source claim> }
          accessModes: [ReadWriteOnce]
          resources: { requests: { storage: 1Gi } }
```

Deleting the pod deletes the claim, which finishes the fork and
publishes its change. The receiver, from its own clone:

```sh
agentfs fork list --remote origin --source <source volume>
agentfs fork deliver <fork> --to inbox --volume <source volume>
```

- **The sandbox sees** the source's files as of its latest commit. The
  source does not see the sandbox's writes.
- **History:** the fork's branch `agentfs/<fork>`, then
  `refs/agentfs/<fork>/change` and `/finish`, on the fork remote. After
  delivery the change is `inbox/<n>` of the source, and then one patch
  commit on the source's branch.
- **Limits:** the source applies the change at its next start (for CSI,
  its next NodeStage), not while it runs; a change that conflicts blocks
  the queue (§12). The sandbox must run on the source's node. Delivery
  pushes unreviewed output where the target's own CI acts on it
  (§19.1 "Fork content"). Landing the change live and holding a
  conflicting one while others go on are **designed, not built**.

### Example 6: forks of forks

**For:** a sandbox that starts its own sandboxes, with each level's
change rolled up into its parent.

**Not built.** `agentfs fork create` and CSI CreateVolume refuse a
source that is itself a fork (`SourceIsFork`, §11.5 "Create"). The
merge-queue design covers it (bead agentfs-mia.7): a fork's base would
be a fork point it makes for itself, and each level would land its
children's changes as patches. Today the closest built shape is a flat
one: every sandbox forks the one source (example 5).

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

```sh
agentfs --state /data/a/state init --volume agent-a --include /etc/agentfs/config
agentfs --state /data/b/state init --volume agent-b --include /etc/agentfs/config
```

Under CSI, the default `volumeName` template already makes each claim's
name unique.

- **Each workload sees** only its own volume.
- **History:** `refs/heads/agentfs/agent-a` and `agentfs/agent-b`, with
  each volume's patch queue under its own `refs/agentfs/<volume>/`
  (§11.4). Pushes from different volumes never contend.
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
  open (§19.2 item 9).

### Example 9: an operator restores or reverts

**For:** an operator undoing an agent's mistake.

| Dimension | Value |
|---|---|
| Integration point | filesystem (`restore`, `revert`, over the control socket when a daemon runs) |
| Sharing | any |
| Remotes | any; nothing is fetched |

Find the commit with `git log` in the state area or the remote's web
view, pause the workload when you can, then:

```sh
agentfs --state <state> restore notes/plan.md <commit> --dry-run
agentfs --state <state> restore notes/plan.md <commit>
agentfs --state <state> revert <commit>
# Under CSI, from the node plugin pod of the volume's node:
kubectl -n agentfs-csi exec <node plugin pod> -c agentfs-csi -- \
  agentfs --state /var/lib/agentfs-csi/volumes/<pv>/state restore <path> <commit>
```

- **The workload sees** the restored files at once when a mount or the
  history service writes them.
- **History:** one commit per operation, `restore <path> from <id>` or
  `revert <id>: <subject>`, with `Agentfs-Trigger: restore` and the
  operator as author (§9).
- **Limits:** the commit must be on the volume's branch; a diverged ref
  is restored with plain git. A workload writing the same files can
  overwrite the restore (its writes are recorded too). While `agentfs run`
  or `record` holds the volume, the command exits 75. Steps:
  [recovery-and-undo.md](runbooks/recovery-and-undo.md).

### Example 10: a CSI volume continued on another node

**For:** an operator whose node is lost or retired.

| Dimension | Value |
|---|---|
| Integration point | Kubernetes adapter (start class) |
| Sharing | the new volume: any, on the new node |
| Remotes | the old volume's remote holds its branch; the new volume starts from it |

A start class names the old branch at a commit; a new claim on it is a
new worktree with its own branch:

```yaml
parameters:
  profile: example
  startFrom: "origin:agentfs/<old volume>@<commit>"
```

- **The workload sees** the tree of that commit.
- **History:** the new branch `agentfs/<new volume>` begins at the
  commit. If the old branch gains commits later (a late push from the old
  node), `StartBranchMoved` alarms until the operator takes them in or
  runs `agentfs start accept --tip <tip>`
  ([start-branch-moved.md](runbooks/start-branch-moved.md)).
- **Limits:** commits the old node never pushed are not in it. The alarm
  waits for the new volume's next push and watches only the start remote
  ([moving.md](runbooks/moving.md)).

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

## 3. How to use this page

- **Check a design against every example it touches**, not only the one
  it was written for. A change to how patches apply touches examples 3,
  5, 8 and 11; a change to the front protocol touches example 4 and
  every restore through the history service in example 9.
- **Say which world the design lives in.** File semantics (locks,
  renames, caches) belong to section 1.2. Merging, order and gating
  belong to section 1.3. A design that needs both, such as landing a
  patch into a volume several pods use, must hold in both.
- **Hermes combines several examples.** As deployed by the example it is
  example 3 (several processes, iac patches) and can be example 8 (its
  branch reviewed on GitHub). Receiving sandbox forks (example 5) and
  forks of forks (example 6) are where the unbuilt merge queue would
  apply.
- **Never count a designed value as available.** If a design needs a
  value marked **designed, not built**, it depends on bead agentfs-mia;
  say so.
- **Add an example** when a new consumer or a new combination appears,
  with the same fields, and mark every value.
