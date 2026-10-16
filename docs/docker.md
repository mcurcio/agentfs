# agentfs with Docker

agentfs runs under plain Docker the way it runs on Kubernetes (design
§17.1): one container mounts the volume with FUSE, and the workload's
containers use the mount like any directory. This page covers the
examples in [`examples/docker`](../examples/docker), how they are wired
and why, operations, what is and is not tested, and why there is no
Docker volume plugin.

Section numbers (§) refer to [design.md](design.md). The Kubernetes
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
                            backing/ (the files) and state/ (the history)

agentfs-init   agentfs init --include /etc/agentfs/config   (runs once per start)
agentfs        agentfs mount /data/backing /agentfs/workspace
               <dir> bound at /agentfs, rshared
workload       <dir> bound at /agentfs, read-only, rslave, recursive writable
```

- **The volume's files and history share one Docker volume**, mounted at
  `/data` in the agentfs containers only. The backing tree and the state
  area are siblings on one filesystem, as on Kubernetes ([deployment.md
  §3](deployment.md#3-the-pod-piece-by-piece)): reflinks fail across
  mounts. The workload never mounts it, so the history, journal, state
  and credentials are out of its sight (§1.1).
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
- **Settings and remotes** come from a git-config fragment bound at
  `/etc/agentfs/config`, which `agentfs init --include` names in the
  state area's config ([deployment.md §4](deployment.md#4-configuration)).

## 3. `docker run`

[`docker-run.sh`](../examples/docker/docker-run.sh) is the example as a
script, so CI runs exactly what this page describes:

```sh
examples/docker/docker-run.sh up
examples/docker/docker-run.sh workload --name app --user 10000:10000 --cap-drop ALL \
  debian:trixie-slim sh -c 'echo hello > /agentfs/workspace/hello.txt; sleep infinity'
docker exec agentfs-example git --git-dir /data/state/git log --format='%h %an %s' agentfs/example
examples/docker/docker-run.sh down
```

- `up` creates the Docker volume `agentfs-$VOLUME`, runs `agentfs init`
  in a one-off container, starts the container `agentfs-$VOLUME` and
  waits until its health check (`agentfs probe /agentfs/workspace`) sees
  the live mount.
- `workload` runs `docker run -d` with the workload's bind of the host
  directory and the working directory `/agentfs/workspace`, plus your
  own options and image.
- `down` stops the workload containers first, so nothing holds the
  mount, then agentfs, which makes its final commit and unmounts (it
  prints the `final commit:` line). The history stays in the Docker
  volume for the next `up`.

Its settings are environment variables, listed at the top of the script:
`VOLUME` (the branch is `agentfs/<volume>`), `MOUNT_DIR` (the host
directory), `AGENTFS_IMAGE`, `AGENTFS_CONFIG` (the fragment; default
[`config/local`](../examples/docker/config/local), no remote),
`AGENTFS_SECRETS` (a directory of credential files, bound at
`/run/secrets`) and `WORKLOAD_USER`.

The agentfs container's flags, besides the privileges of §2:

| Flag | Why |
|---|---|
| `--restart unless-stopped` | Docker restarts a crashed daemon (§5 below) |
| `--stop-timeout 60` | Longer than `--unmount-grace` (10 s) plus `--push-grace` (5 s) plus the final commit, so `docker stop` never cuts the final commit short |
| `-e AGENTFS_ON_STALE_MOUNT=recover` | A restarted daemon mounts over the stale mount the dead one left |
| `-e AGENTFS_SEAL_MOUNTPOINT=true` | The bare mount point is root's with mode 0000 |
| `-e OTEL_SDK_DISABLED=true` | No collector in the example; set `OTEL_EXPORTER_OTLP_ENDPOINT` instead ([telemetry.md](telemetry.md)) |
| `--health-cmd 'agentfs probe /agentfs/workspace'` | Healthy once the mount is live; 5 minutes' start period for journal recovery |

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
docker compose exec agentfs git --git-dir /data/state/git log --format='%h %an %s' agentfs/example
docker compose down
```

`docker compose down` stops `workload` before `agentfs`, which then
makes its final commit; `stop_grace_period: 60s` gives it the time.
`down -v` also deletes the volume and its history.

**With a remote.** [`compose.remote.yaml`](../examples/docker/compose.remote.yaml)
adds a throwaway git server (nginx and `git http-backend`, built from
[`git-server/`](../examples/docker/git-server)), binds
[`config/remote`](../examples/docker/config/remote) as the fragment
instead (the remote `origin` at `http://git-server:8080/git/history.git`,
pushed every 5 s), and passes the credentials file
[`secrets/origin`](../examples/docker/secrets/origin) as a Compose
secret at `/run/secrets/origin`, which the server also reads for its own
password:

```sh
docker compose -f compose.yaml -f compose.remote.yaml up -d
docker compose -f compose.yaml -f compose.remote.yaml \
  exec -u www-data git-server git --git-dir /srv/git/history.git log agentfs/example
```

For a real remote, put its URL in `config/remote`, your token in the
secret (never committed), and drop the `git-server` service. SSH remotes
and LFS take the keys in [configuration.md](configuration.md#ssh-remotes),
as on Kubernetes.

**Settings:** `VOLUME`, `AGENTFS_IMAGE`, `WORKLOAD_IMAGE` and
`WORKLOAD_USER` are read from the environment or an `.env` file.

## 5. Users, operations and crashes

**Users.** `agentfs-init` gives the root of the tree to `WORKLOAD_USER`
(default `10000:10000`) after `agentfs init`, so a non-root workload can
write at the top of the volume; directories are not recorded, their
files are. Files belong to the user who created them, with the mode they
asked for ([deployment.md §5](deployment.md#5-users-and-permissions)).
The daemon cannot read the supplementary groups of a caller in another
container's pid namespace ([design §6](design.md#6-metadata-and-permissions)),
the same limit as the Kubernetes sidecar.

**Operations** run in the agentfs container, whose environment names the
state area, as `kubectl exec` does in [deployment.md
§6](deployment.md#6-operations):

```sh
docker exec agentfs-example agentfs status
docker exec agentfs-example agentfs push          # push now
docker exec agentfs-example agentfs restore notes/plan.md <commit>
```

(With Compose: `docker compose exec agentfs agentfs status`.)

**A crashed daemon.** Writes fail with `ENOTCONN` until Docker restarts
the container (`restart: unless-stopped`). The new daemon finds the
stale mount at `/agentfs/workspace`, recovers the journal and mounts
over it (`stacked over a stale mount` in its log), and propagation
carries the new mount to the host and into the running workload
containers, which need no restart ([deployment.md
§7](deployment.md#7-a-crashed-daemon)). A daemon stopped cleanly while a
workload runs leaves the workload on the bare, read-only directory:
restart the workload after agentfs. If the host itself is left with a
dead mount (the Docker daemon stopped, say), `umount -l <dir>/workspace`
as root clears it; the next `up` mounts again.

## 6. Volumes with no remote

The examples without `compose.remote.yaml` configure no remote: agentfs
records and commits every change to the history in the Docker volume and
never pushes. That is what agentfs allows today, by leaving the
`[remote]` section out. A declared local-only mode is coming with
milestone F8. Back up the Docker volume, or add a remote, if the history
must outlive the host.

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
  (§14.2). A plugin would duplicate it or need it factored out of
  `agentfs-csi` into a shared crate first.
- **A new release path.** Plugins are packaged as a root filesystem and
  a `config.json` and published with `docker plugin push`, which the
  `image` workflow does not do.
- **One process holds every volume.** A plugin crash, upgrade or
  `docker plugin disable` takes every volume's mount at once; the
  examples keep one agentfs container per volume.

Together that is a milestone of its own, so this one documents the
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
- the writes are committed to `agentfs/<volume>` with the writers' uids
  as authors, and with the remote, reach the git server;
- `agentfs status` runs in the agentfs container;
- stopping makes the final commit (and pushes it, with the remote), and
  leaves nothing mounted on the host.

The `compose` job also kills the daemon with `SIGKILL` from the host and
checks that Docker restarts it, that it mounts over the stale mount, and
that the running workload writes through the new mount without a
restart.

Not tested: Docker Desktop and rootless Docker, `--privileged` instead
of the three narrower options, `restore` and `push` through `docker
exec`, SSH and LFS remotes under Docker, and a clean stop of agentfs while
a workload runs.
