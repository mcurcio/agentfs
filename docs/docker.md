# agentfs with Docker

agentfs runs under plain Docker the way it runs on Kubernetes: one container mounts the volume with FUSE, and the workload's
containers use the mount like any directory. This page covers the
examples in [`examples/docker`](../examples/docker), how they are wired
and why, operations, what is and is not tested, and why there is no
Docker volume plugin.

Section numbers (§) on this page are this page's own. What agentfs promises is in [guarantees](guarantees.md). The Kubernetes
sidecar is in [deployment.md](deployment.md); every setting is in the
[configuration reference](configuration.md).

## 1. Requirements

- **A Linux host running Docker Engine.** The agentfs container's mount
  reaches other containers through the host's mount table, so the host
  directory it mounts into must be on a *shared* mount. It is on systemd
  hosts, where `/` is shared. Docker Desktop cannot propagate mounts to
  the host (Docker's [bind mount
  docs](https://docs.docker.com/engine/storage/bind-mounts/)), so the
  examples do not work there.
- **Docker Engine 25 or later**, for `bind-recursive` (`recursive` in
  Compose) and the health check's start interval. CI runs the examples on
  GitHub's `ubuntu-24.04` runner, whose versions the job log prints.
- **The fuse kernel module** (`modprobe fuse`) and `/dev/fuse`.
- **The image** `ghcr.io/mcurcio/agentfs` ([deployment.md
  §1](deployment.md#1-the-image)). The examples default to `latest`; set
  `AGENTFS_IMAGE` to a commit tag or a digest in real use.

Rootless Docker is not tested.

## 2. The layout

```
host
├── <dir>/                  a directory on a shared mount
│   └── workspace/          the FUSE mount (what workloads use)
└── docker volume           /data in the agentfs containers only:
                            state/, the volume's state area (its files,
                            journal and history.git)

agentfs-init   agentfs init --include /etc/agentfs/config --owner <uid:gid>   (runs once per start)
agentfs        agentfs mount /agentfs/workspace
               <dir> bound at /agentfs, rshared
workload       <dir> bound at /agentfs, read-only, rslave, recursive writable
```

- **The volume lives in one Docker volume**, mounted at `/data` in the
  agentfs containers only: its state area, `/data/state`, holds the
  files, the journal and the history (`history.git`, branch `root`), as
  on Kubernetes ([deployment.md §3](deployment.md#3-the-pod-piece-by-piece)).
  The containers run the tree engine ([trees](trees.md)): there is no
  backing tree. The workload never mounts the Docker volume, so the
  history, journal, state and credentials are out of its sight
  ([guarantees](guarantees.md)).
- **The agentfs container binds the host directory with `rshared`
  propagation** and mounts FUSE at `workspace/` inside it, so the mount
  appears on the host. It is a subdirectory, not the bound directory
  itself, for the reason given in deployment.md §3: a dead mount answers
  `ENOTCONN`, and only that subdirectory should.
- **Workload containers bind the same host directory read-only, with
  `rslave` propagation and `bind-recursive=writable`.** `rslave` carries
  the FUSE mount in, and the mount that replaces it after a crash (§5
  below). `writable` keeps that submount writable: Docker otherwise makes
  the submounts of a read-only bind read-only too. The bare directory
  under the mount stays read-only, so a write while no daemon is mounted
  fails with `EROFS` instead of landing where nothing records it.
- **Privileges.** FUSE needs `/dev/fuse` and `CAP_SYS_ADMIN`, and
  Docker's default AppArmor profile denies `mount(2)`, so the agentfs
  container gets `--device /dev/fuse --cap-add SYS_ADMIN --security-opt
  apparmor=unconfined`. `--privileged` covers all three and more. The
  cost is the one in [deployment.md
  §9](deployment.md#9-security-trade-offs-of-the-privileged-sidecar): a
  bug in agentfs is a bug in a container that can mount filesystems on
  the host. Workload containers need no privileges.
- **Operator settings** come from a git-config file bound at
  `/etc/agentfs/config`, which `agentfs init --include` makes the include
  of the volume's operator settings file, read at every mount
  ([deployment.md §4](deployment.md#4-configuration)). A key tree volumes
  do not build yet, such as a remote, is logged as `not built` at every
  boot.

## 3. `docker run`

[`docker-run.sh`](../examples/docker/docker-run.sh) is the example as a
script, so CI runs exactly what this page describes:

```sh
examples/docker/docker-run.sh up
examples/docker/docker-run.sh workload --name app --user 10000:10000 --cap-drop ALL \
  debian:trixie-slim sh -c 'echo hello > /agentfs/workspace/hello.txt; sleep infinity'
docker exec agentfs-example git --git-dir /data/state/history.git log --stat root
examples/docker/docker-run.sh down
```

- `up` creates the Docker volume `agentfs-$VOLUME`, runs `agentfs init`
  in a one-off container, starts the container `agentfs-$VOLUME` and
  waits until its health check (`agentfs probe /agentfs/workspace`) sees
  the live mount. History is built behind the calls: `git log` shows a
  save a moment after its close returned.
- `workload` runs `docker run -d` with the workload's bind of the host
  directory and the working directory `/agentfs/workspace`, plus your
  own options and image.
- `down` stops the workload containers first, so nothing holds the
  mount, then agentfs, which drains the volume, unmounts and exits 0 (it
  prints `agentfs exited 0`); a clean stop leaves history built through
  its last record. The history stays in the Docker volume for the next
  `up`.

Its settings are environment variables, listed at the top of the script:
`VOLUME`, `MOUNT_DIR` (the host directory), `AGENTFS_IMAGE`,
`AGENTFS_CONFIG` (the operator settings; default
[`config/local`](../examples/docker/config/local), no remote),
`AGENTFS_SECRETS` (a directory of credential files, bound at
`/run/secrets`) and `WORKLOAD_USER`.

The agentfs container's flags, besides the privileges of §2:

| Flag | Why |
|---|---|
| `--restart unless-stopped` | Docker restarts a crashed daemon (§5 below) |
| `--stop-timeout 60` | Time for the drain at `docker stop` |
| `-e AGENTFS_ON_STALE_MOUNT=recover` | A restarted daemon stacks over the stale mount the dead one left |
| `-e AGENTFS_SEAL_MOUNTPOINT=true` | The bare mount point is root's with mode 0000 |
| `-e OTEL_SDK_DISABLED=true` | No collector in the example; set `OTEL_EXPORTER_OTLP_ENDPOINT` instead ([telemetry.md](telemetry.md)) |
| `--health-cmd 'agentfs probe /agentfs/workspace'` | Healthy once the mount is live; 5 minutes' start period for the volume's recovery |

## 4. Docker Compose

[`compose.yaml`](../examples/docker/compose.yaml) has three services:
`agentfs-init` (a one-shot `agentfs init`), `agentfs` (the mount, with
the flags of §3) and `workload` (uid 10000, every capability dropped).
`depends_on` gives the order: `agentfs` starts once `agentfs-init` has
succeeded, and `workload` once `agentfs` is healthy. The mount appears in
`examples/docker/mnt/workspace` on the host.

```sh
cd examples/docker
mkdir -p mnt                # Docker does not create a long-syntax bind's source
docker compose up -d
docker compose exec workload sh -c 'echo hello > hello.txt'
docker compose exec agentfs git --git-dir /data/state/history.git log --stat root
docker compose down
```

`docker compose down` stops `workload` before `agentfs`, which then
drains the volume; `stop_grace_period: 60s` gives it the time.
`down -v` also deletes the volume and its history.

**With a remote.** [`compose.remote.yaml`](../examples/docker/compose.remote.yaml)
adds a throwaway git server (nginx and `git http-backend`, built from
[`git-server/`](../examples/docker/git-server)), binds
[`config/remote`](../examples/docker/config/remote) as the operator
settings instead (the remote `origin` at
`http://git-server:8080/git/history.git`), and passes the credentials
file [`secrets/origin`](../examples/docker/secrets/origin) as a Compose
secret at `/run/secrets/origin`, which the server also reads for its own
password. **Push is not built on tree volumes yet:** nothing is pushed,
and the agentfs container logs the remote's keys as not built at every
boot:

```sh
docker compose -f compose.yaml -f compose.remote.yaml up -d
docker compose -f compose.yaml -f compose.remote.yaml logs agentfs | grep '^not built'
# not built: remote.origin.agentfscredentials (push): this volume pushes to no remote yet; its history stays in the state area
# not built: remote.origin.url (push): this volume pushes to no remote yet; its history stays in the state area
```

For a real remote, put its URL in `config/remote`, your token in the
secret (never committed), and drop the `git-server` service; it is
pushed to once push is built. The keys of each kind of remote are in
[configuration.md](configuration.md#ssh-remotes).

The same image is the CSI chart's git server ([helm.md](helm.md#the-git-server)),
published as `ghcr.io/mcurcio/agentfs-git-server`. Its entrypoint takes
`init` (create `/srv/git/history.git` if it is missing, then exit),
`serve`, or nothing (both, as here). Started as root, as here, it runs
nginx's workers and `git http-backend` as `www-data`; started as another
user, as the chart does, it can have a read-only root filesystem, and
needs `/srv/git` and `/run/git-server` writable. `GIT_SERVER_HTPASSWD`
names an htpasswd file to use instead of `/run/secrets/origin`, and
`GIT_REPO` another repository name.

**Settings:** `VOLUME`, `AGENTFS_IMAGE`, `WORKLOAD_IMAGE` and
`WORKLOAD_USER` are read from the environment or an `.env` file.

## 5. Users, operations and crashes

**Users.** `agentfs init --owner "$WORKLOAD_USER"` (default
`10000:10000`) records a new volume's root directory as that user's, so a
non-root workload can write at the top of the volume; a found volume is
never changed. Files belong to the user who created them, with the mode
they asked for, and history records each one's owner and mode
([deployment.md §5](deployment.md#5-users-and-permissions)).

**Operations** run in the agentfs container, whose environment names the
state area, as `kubectl exec` does in [deployment.md
§6](deployment.md#6-operations):

```sh
docker exec agentfs-example git --git-dir /data/state/history.git log --stat root
docker exec agentfs-example agentfs probe /agentfs/workspace
```

(With Compose: `docker compose exec agentfs ...`.) `status`, `push`,
`restore` and `revert` are not built on tree volumes yet: each refuses
with a message naming what is not built.

**A crashed daemon.** Writes fail with `ENOTCONN` until Docker restarts
the container (`restart: unless-stopped`). The new daemon finds the
stale mount at `/agentfs/workspace`, recovers the volume and stacks over
it (`stacked over a stale mount at /agentfs/workspace left by a crashed
daemon` in its log), and propagation carries the new mount to the host
and into the running workload containers, which need no restart
([deployment.md §7](deployment.md#7-a-crashed-daemon)). A daemon stopped
cleanly while a workload runs drains the volume and leaves the workload
on the bare, read-only directory: restart the workload after agentfs. If
the host itself is left with a dead mount (the Docker daemon stopped,
say), `umount -l <dir>/workspace` as root clears it; the next `up` mounts
again.

## 6. Volumes with no remote

The examples without `compose.remote.yaml` are local-only:
[`config/local`](../examples/docker/config/local) names no remote, so
agentfs records every change into the history in the Docker volume and
logs nothing as not built. Zero remotes is not an error and needs
nothing declared. Until push is built for tree volumes every volume is
local-only: back up the Docker volume if the history must outlive the
host.

## 7. Why there is no Docker volume plugin

A managed volume plugin would let `docker volume create -d agentfs` and
`-v name:/path` replace the host directory and its propagation flags,
and would work where propagation to the host cannot (Docker Desktop). It
was evaluated and not built, because it is not a small addition:

- **Its own service.** A plugin is a long-running process in its own
  container that answers Docker's volume plugin API (`Create`, `Remove`,
  `Mount`, `Unmount`, `Path`, `Get`, `List`, `Capabilities`) over a Unix
  socket, and makes every mount under its `propagatedMount` directory
  ([Docker's volume plugin
  docs](https://docs.docker.com/engine/extend/plugins_volume/)).
- **Supervision agentfs already has once.** Per volume it would run
  `agentfs init` and a daemon on the first `Mount` and stop it on the
  last `Unmount` (Docker calls them once per container, with an ID),
  restart a crashed daemon over its stale mount, and keep each volume's
  options (remote, credentials) on disk so `List` and `Get` survive a
  plugin restart. That is what the CSI node plugin does for Kubernetes
  ([csi.md](csi.md)). A plugin would duplicate it or need it factored out of
  `agentfs-csi` into a shared crate first.
- **A new release path.** Plugins are packaged as a root filesystem and
  a `config.json` and published with `docker plugin push`, which the
  `image` workflow does not do.
- **One process holds every volume.** A plugin crash, upgrade or
  `docker plugin disable` takes every volume's mount at once; the
  examples keep one agentfs container per volume.

Together that is a project of its own, so this page documents the
container pattern instead. A route that would reuse the CSI driver as it
is: Docker Swarm's cluster volumes take a CSI plugin packaged as a
managed plugin. It is Swarm-only and has not been tried.

## 8. How it is tested

The `docker` workflow ([`.github/workflows/docker.yml`](../.github/workflows/docker.yml))
builds the image from the commit and runs
[`ci/e2e-docker.sh`](../ci/e2e-docker.sh) once per example, in parallel
jobs: `run` (`docker-run.sh`, no remote), `compose` (no remote) and
`compose-remote` (with the git server). The compose jobs add a root
writer next to the example's uid 10000 workload
([`ci/docker/compose.ci.yaml`](../ci/docker/compose.ci.yaml)). Each job
checks that:

- root and uid 10000 write through the mount with normal permissions,
  and the directory under the mount is read-only to them;
- the workload sees only the mount under `/agentfs` and only its own
  files in the tree, and has neither `/data` nor the Docker volume
  mounted;
- uid 10000 writes at the top of the volume (`init --owner`);
- the writes are in the history (`state/history.git`, branch `root`) with
  the writers' uids as their owners;
- `agentfs status` refuses the tree volume, naming what is not built;
- with the remote, its keys are logged as not built and nothing reaches
  the git server;
- stopping drains the volume (agentfs exits 0, and history holds the last
  write), and leaves nothing mounted on the host.

The `compose` job also kills the daemon with `SIGKILL` from the host and
checks that Docker restarts it, that it stacks over the stale mount, and
that the running workload writes through the new mount without a
restart.

A further job runs upstream Hermes on the compose example, with
docker-in-docker for its tool containers
([deployment.md §10](deployment.md#how-it-is-tested)).

Not tested: Docker Desktop and rootless Docker, `--privileged` instead
of the three narrower options, and a clean stop of agentfs while a
workload runs.
