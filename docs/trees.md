# Tree views

A tree view is a mount of a git-backed tree. Each close saves the file into its
own tree, and queues carry changes between an **upstream** and its **forks**. The
tree views are what `agentfs init` and `agentfs mount` run: a tree volume, on its
own state area. The recording engine ([recording.md](recording.md)) is what the
CSI driver runs; its commands refuse a tree volume, and `init` and `mount`
refuse its state areas.

Use them to try the model of upstreams and forks: a fork edits like an ordinary
directory, its saves flow to its upstream, and the upstream's saves flow back,
without anyone asking. What the model decides on every collision is written down
in the [rules ledger](target-architecture/rules-ledger.md) and the
[git model](target-architecture/git-model.md). This page says how to run the views,
what they do today, and where they stop.

## Terms

- A **tree** is a branch of one bare git repository.
- A **fork** is a tree made from another tree, its **upstream**. A tree with no
  upstream is the root. Tree views make forks of the root only: a fork of a fork
  is refused.
- A tree's **identity** is `root` or `fork/<n>`, where `n` is the journal event that
  made the fork; it is never reused. The tree's branch (`refs/heads/root`,
  `refs/heads/fork/<n>`), its queue, its checkout and the `Agentfs-Tree` trailer of
  every commit made in it name it by its identity, never by a branch name it is
  published under. Forks are taken in order of `n`, after the root.
- A **sync** brings the upstream's version into a fork. A **landing** brings a
  fork's save into its upstream. Two lines **collide** when each was written
  without seeing the other; the upstream's version wins and the loser's lines are
  kept.
- A tree's entries are files, **links** (symbolic links: a file whose bytes are
  its target) and **directories**. A directory is its path, as in git: it has no
  identity of its own, so two trees that make the same directory make the same
  one.
- An entry's **metadata** is one value: its kind (file, link or directory), its
  permission bits (`mode & 07777`), its uid and its gid. History records it, and
  compares it whole.
- A **directory record** is a directory's metadata in a commit (`dirs/`). Git
  keeps no empty directory; a directory record keeps one.
- An entry's **node number** says which file or directory a view's inode is: a
  file's or link's object number, a directory's number (the record that made
  it). A directory a take adds gets a new number from the take's step, never
  the source tree's, and a take may add at most 1,048,576 (2^20) directories.
  Like object numbers, node numbers are the journal's, never history's.
- **Tree-local times** are an entry's access, modification and change times.
  Each tree keeps its own, in its journal and checkpoints; history never holds
  them, as git records no file times.

## Run it

You need Linux with `/dev/fuse`, `fusermount3` and the `git` release pinned in
`ci/git-release` (`ci/check-tree-git.sh` checks it). Line identity depends on the
`git` release, so the library looks `git` up in a fixed `PATH` and fails on another
release in CI.

```sh
cargo build --locked -p agentfs
target/debug/agentfs --state /tmp/agentfs-trees/state init --volume trial
target/debug/agentfs --state /tmp/agentfs-trees/state mount \
  /tmp/agentfs-trees/root --fork-mount /tmp/agentfs-trees/fork --fork-key trial
```

Use ordinary file calls in either view. Once every view is up the command prints
`mounted volume trial at /tmp/agentfs-trees/root, fork fork/N at
/tmp/agentfs-trees/fork; stop with SIGINT or SIGTERM` on stderr, after `fork tree:
fork/N`. Without `--fork-key` every mount makes a new fork, which ends when its view
is unmounted; under a key, every mount finds the same fork until `fork end
--fork-key` ends it ([forks](#forks-keys-end-and-the-drop)). Stop the mount with
SIGTERM or SIGINT, or unmount every view with `fusermount3 -u PATH`: either drains
the volume, and the process exits 0 once it is released
([lifecycle](#volume-lifecycle)).

| Command or flag | Meaning |
|---|---|
| `init --volume NAME` | Create the volume, or find it under the same name, then recover it; prints `<name> <id> created` or `found`. Another name or another format (a recording state area among them) exits 1 with nothing written |
| `--owner UID:GID` (`init`) | The owner a new volume's root directory is recorded with; a found volume is never changed |
| `--include FILE` (`init`) | The operator settings' includes ([operator settings](#operator-settings)) |
| `mount ROOT` | Recover and mount the root on an empty directory, created when missing. It never creates a volume: run `init` first |
| `mount ROOT -- COMMAND [ARG]...` | Run COMMAND once every view is up, and drain when it exits ([running a command](#running-a-command-in-the-mount)) |
| `--volume NAME` (`mount`) | Refuse, before anything is written, a volume with another name |
| `--no-allow-other` | Only the mounting user reaches the views. By default other users do (FUSE `allow_other`, which needs `user_allow_other` in `/etc/fuse.conf` when the mounting user is not root) |
| `--namespace`, `--pod`, `--pod-uid`, `--node` | Where the views run: the pod context of their commits (below) |
| `--fork-mount DIR` | Also mount a fork of the root in DIR: a new one, or with `--fork-key` the open one under that key (made the first time) |
| `--fork-key KEY` | The fork's key (needs `--fork-mount`): 1 to 128 bytes of printable ASCII with no whitespace. A key whose fork has ended exits 1 |
| `--writer LABEL`, `--fork-writer LABEL` | The root's and the fork's client label (default `root-client`, `fork-client`): a safe name of at most 64 bytes. A label is the launcher's word, not an authenticated identity |
| `--read-only`, `--fork-read-only` | Mount the root, or the fork, read-only: its client cannot write, and every write answers `EROFS`. `--fork-read-only` takes the default fork label |
| `--on-stale-mount refuse\|hold\|recover`, `--seal-mountpoint` | What a restart does with a stale mount its crashed predecessor left, and the fence on a bare mount point ([restarting](#restarting-after-a-crash)) |
| `fork end --fork-key KEY` | End the fork under the key; it drops once nothing holds it. Exits 75 while a mount holds the volume, 1 for an unknown key |
| `--metrics-output FILE` | Write counters, the journal's length, each stage's runs, time and git children, and the objects stored, at each built or failed event, at least every 100 ms, and at unmount; each file carries `seq`, one more each time |

Mount points must be empty, separate and outside the state area: a mount point
holding files exits 1, since the mount would hide them and nothing would record
them. `init`, `mount` and `fork end` take `--state` only: `--root`
(`AGENTFS_ROOT`) or a nonzero `--wait-lock` exits 1, so an old manifest fails
loudly. Metrics never take part in ordering. Every flag is in
[configuration](configuration.md#flags-and-environment).

### Operator settings

The operator's settings of a tree volume are `<state>/config`, in git-config
syntax, kept apart from `history.git/config`, which the engine rewrites and its git
children read. `init --include FILE` (repeatable, absolute, existing) makes the
file exactly those `include.path` lines, as git's own includes; without `--include`
a re-run keeps them. Every `mount` reads the file with libgit2, which follows the
includes, before the volume is staged and before a mount point is created or
sealed (a boot that holds over a stale mount reads and logs it too). An include
at any depth (one named by an included file too, a relative one resolved against
the directory of the file naming it, as git does) is checked:

- an include under a view's mount point exits 1, since the workload could change
  the operator's settings;
- a missing include is warned of (`warning: operator settings: include <path> is
  missing`), as git skips it;
- an `includeIf.<condition>.path` line is logged as not built (`other`): libgit2
  evaluates no condition for a file outside a repository, so its target is never
  read;
- each key whose capability tree volumes do not build yet is logged at every boot,
  one line per key, and the mount serves anyway:

  ```text
  not built: remote.origin.url (push): this volume pushes to no remote yet; its history stays in the state area
  not built: agentfs.patchremote (patches): queued patches are not applied
  ```

  The capabilities are `push` (remotes, `agentfs.pushInterval`, `agentfs.forkRefs`,
  `http.proxy`), `patches` (`agentfs.patch*`, `agentfsPusher.*`), `lfs` and
  `other`. The `agentfs.not_built` gauge counts them by `capability`
  ([telemetry](telemetry.md)), the standing alarm for a setting that does
  nothing yet. `agentfs.nested = exclude` is built and not listed, and so is
  `agentfs.overlayLimit` (the lag gate's overlay limit, below): a value that is
  not a positive size exits 1 before the volume is staged. Every key is in
  [configuration](configuration.md#tree-volumes-the-operator-settings-file).

### Running a command in the mount

`mount ROOT [FLAGS] -- COMMAND [ARG]...` starts COMMAND once every view is up,
with the mount's stdio and environment, as `dbus-run-session` and `ssh-agent
COMMAND` do. Its exit is a trigger like a stop: the volume drains, and the mount
exits with the command's status when that is not 0 (128 plus the signal's number
for a command a signal ended), else with the drain's. A SIGTERM or SIGINT while
it runs is sent on to it as SIGTERM, and the drain starts. A command that cannot
start (not found) drains the volume and exits 127. A migration copies files in
this way, through the view, with no wait for the mount to appear.

### Restarting after a crash

A killed mount leaves each view's mount point a stale agentfs mount: every call on
it answers `ENOTCONN`, and `agentfs probe` reports `stale` (exit 1, or 0 with
`--allow-stale`). The views mount as `agentfs:<volume>`, and a fork view as
`agentfs:<volume>:<tree>`, so `probe` and the restart tell them from other mounts.
The next `mount` judges each mount point before it stages anything, by
`--on-stale-mount` (`AGENTFS_ON_STALE_MOUNT`):

- `refuse` (the default): exit 1, naming the fix (`--on-stale-mount recover` or
  `hold`, or `umount -l PATH`).
- `hold`: mount nothing and take no lock; wait for SIGTERM or SIGINT, then detach
  every stale layer and exit 1. A pod held this way recovers at its next start.
- `recover`: stack each view over its stale mount: FUSE mounts at a private
  directory under `<state>/stack/`, is bind-mounted over the stale mount, so every
  copy of the mount point (a container's propagated mount, a `subPath` bind of the
  mount point itself) reaches the new view, and the private mount is detached. A
  bind of a directory inside the dead view is not reached: it answers `ENOTCONN`
  until its container restarts. The mount prints `stacked over a stale mount at
  PATH left by a crashed daemon`; the stale layers below are detached when the view
  ends, and a dead layer a crash left under `<state>/stack/` is detached at the
  next start. It needs `CAP_SYS_ADMIN`. A view stacks only over its own stale
  mount, the exact source it would mount with: a stale mount of another volume or
  of another tree (a fork's at the root's mount point) exits 1, and so does a fork
  mount without `--fork-key`, which makes a new fork that never stacks over a
  crashed fork's mount (give that fork's key to resume it, or `umount -l PATH`).

A mount point inside the state area exits 1 before anything is created there. A
live mount there exits 1 (`already mounted`), and so does another filesystem.
With `--seal-mountpoint` (`AGENTFS_SEAL_MOUNTPOINT`, root only) each bare mount
point is made root's with mode 0000 before mounting, so a non-root workload that
reaches it while no view is mounted (a `subPath` bound while a sidecar restarts)
gets `EACCES` instead of writing where nothing records it.

### Serving views on request

`agentfs host` runs one volume's view host for a parent process, which opens
and closes views on it and creates and ends keyed forks over the host's
standard input, a Unix socket only the parent holds:

```sh
agentfs --state STATE host --launcher ID --volume NAME   # stdin: the parent's socket
```

It boots as `mount` does, under the launcher `ID`: the volume checked against
`--volume`, the operator settings read and each key not built logged, the
volume staged and its domains restored, the view host started. It then writes
a hello line and answers each request in order: `open` a view of the root or
of the open fork under a key at a mount point, with a client label, an access
and the pod context of its saves; `close` it; `reconcile` (end the launcher's
clients no open view uses); `fork_create` and `fork_end` under
a key. A view at a stale mount of its own source is stacked over it
([restarting](#restarting-after-a-crash)), and its client rebound when one of
the launcher, tree, label and access is left from a crashed host, so a fork's
one writer survives the crash. The parent closing its end (EOF), SIGTERM or
SIGINT stops the host: with no view open it drains the volume and exits 0 once
it is released ([lifecycle](#volume-lifecycle)); with views open it exits 3 at
once and leaves them stale, as a crash does, for the next host to rebind and
stack over. The last view ending does not stop it. The
messages and refusals are in
[library/filesystem](library/filesystem.md#the-view-host-command).

### CSI volumes

The CSI driver ([csi](csi.md)) makes each claim's volume with `agentfs init
--include <profile>.config` under `<base>/<pv>/state`, installs the profile's
rule files into `state/history.git/info/`, and runs one `agentfs host
--launcher csi` per volume in use on its node. Each pod's view is opened at the
kubelet's target with that pod's context; a fork claim is a keyed fork of its
source's root on the same host, under the claim's PV name. Deleting a root
claim drains its volume and keeps the directory.

## Volume identity and trailers

`init --volume NAME` gives a new volume a random id (RFC 9562 version 4) and
writes it with the name to the state area's `volume` file, before the repository and
`format`. Every start reads `format` and `volume` before it writes anything. The id
never changes; the name is a label and a guard.

Every commit names its volume, its writer and where the writer ran, in this order:

```text
<subject>

Agentfs-Writer: <writer>
Agentfs-Cause: <Root | Direct | Landing | Sync>
Agentfs-Tree: <root | fork/<n>>
Agentfs-Event: <step number, 0 for the Root>
Agentfs-Volume: <volume name>
Agentfs-Volume-Id: <id>
Agentfs-Namespace: <namespace>     each pod field only when given
Agentfs-Pod: <pod>
Agentfs-Pod-Uid: <pod uid>
Agentfs-Node: <node>
Agentfs-Format: envelope-2         every commit but the Root
Agentfs-Open: <n>                  a save that closed an open
Agentfs-Collisions: <json>         a landing that decided any
```

A save's writer is its client (below), `<label>#<n>@<launcher>/<tree>`, for example
`root-client#5@cli/root`. A save carries its writer and the pod context of the
mount it came through. The
journal keeps both with each write-open, so a save recovery makes after a crash
names the writer that opened the file and where it ran, not the restarted process.
A landing merge's subject is `Landing by <writer>`, and its writer and pod trailers
are those of the save it lands. A sync, the Root commit and every event `agentfs`
causes (a drop, a claim's release, an end or a release the domains judge) are
`agentfs`'s, with the pod context of the process that serves the volume; an event a
launcher's call causes (a fork's create or end, an attach, a detach) carries the
launcher's.

## Clients

Each view is one **client** of its tree, attached by its launcher (the trial CLI,
`cli`) before the view mounts. A client is named by its label, its number, its
launcher and its tree: `<label>#<n>@<launcher>/<tree>`. Its number is the journal
step of its attach, unique in the volume and never reused. The name is every save's
writer, so a commit needs no lookup to say who made it; the label and launcher hold
no `#`, `@` or `/`, so the name parses back.

- **Sessions.** A view reaches its tree only through its client's session. A
  read-write client's session writes as the client; a read-only client's writes
  nothing (`EROFS`), and its view is mounted read-only. A session reaches only its
  own client's open files.
- **One writer per fork, for life.** A fork's first read-write client claims it. The
  claim stays after the client ends, so a second read-write client is refused, while
  the first is attached and after it ends. `mount` then exits 1: "this fork
  has had its writer; mount it with --fork-read-only or use a new fork". The
  root has no claim. The claim is released when the fork is dropped
  (`ClaimReleased`).
- **Ends.** When a view is unmounted, its client is detached. Its open files close
  as saves first, in handle order, durable before the detach returns; a write after
  that answers `EROFS`. The end is one step with `AttachmentEnded`, then
  `LastDetached` when its tree has no client left.
- **Restart.** A killed `mount` leaves its clients attached. The next mount
  rebinds the one of the same launcher, tree, label and access (the latest when
  several are): the same client and writer, with the new mount's pod context
  (`Rebound`), so a fork's writer comes back after a kill. Then it ends every other
  client of its launcher (`AttachmentEnded`, reason `Reconciled`). A clean unmount
  ends the client, so a fork's writer does not come back after one.
- **Never waits for history.** Attach, rebind and end are durable before they
  return, never wait at the lag gate or for history, and answer while the builder
  fails.

A failed end changes nothing: no room is reserved for it, and the next mount's
reconcile ends what it left.

## Rows and event commits

The volume's own events (a fork created, ended and dropped, clients attached,
rebound and ended, the volume staged, draining and released) are on record
like file changes: each is a commit in the volume's repository, on a chain of commits beside
the tree it concerns.

| Chain | Holds |
|---|---|
| `refs/agentfs/trees/fork/<n>` | fork `n`'s row (`ForkCreated`, `ForkEnded`, `ForkDropped`); kept after the fork is dropped |
| `refs/agentfs/clients/<tree>` | the clients attached to a tree and a fork's writer claim (`Attached`, `Rebound`, `AttachmentEnded`, `LastDetached`, `ClaimReleased`) |
| `refs/agentfs/volume` | the volume's lifecycle (`VolumeStaged`, `VolumeDraining Stop`, `VolumeReleased Keep`) |

Each event commit's tree is one file, `row.json`: the row's value after the event
with its audit stamps (`created` set at the row's first write and kept, `changed` at
every write, each a time and who). Its parent is the chain's previous event (none
for the first). Its message is the event:

```text
<Kind> <detail>

Agentfs-Kind: <Kind>
Agentfs-By: <client <id> | launcher <id> | agentfs>
Agentfs-Tree: <root | fork/<n>>    on a tree's chain only
Agentfs-Event: <step number>
Agentfs-Volume: <volume name>
Agentfs-Volume-Id: <id>
Agentfs-Namespace: <namespace>     each pod field only when given
Agentfs-Pod: <pod>
Agentfs-Pod-Uid: <pod uid>
Agentfs-Node: <node>
Agentfs-Format: row-1
```

An event commit has no `Agentfs-Writer` or `Agentfs-Cause`, so nothing that reads
file commits takes it for a save. `git log refs/agentfs/trees/fork/3` is fork 3's
life; `git show refs/agentfs/trees/fork/3:row.json` is its row now.

- **One record.** A fork's create is one journal record: its branch and its row,
  committed together or not at all, made durable (one group-commit fsync, outside
  the actor's lock) before a mount's new fork returns. Its event
  commit is built behind, with the step's batch, as a save's commit is.
- **Read from the journal.** Rows are read from the actor's saved state, never Git:
  they answer while the builder lags or fails, and a row step never waits at the lag
  gate. A restart reads each built chain's `row.json`, then replays the later
  records.
- **Local for now.** The row chains are kept in the volume's repository; pushing
  them to remotes comes with replication.

## Forks: keys, end and the drop

A fork is open, then ended, then dropped. Its row on `refs/agentfs/trees/fork/<n>`
says which, and every move is one journal step with its event commit, durable
before the call returns.

- **Keys.** A launcher may create a fork under a key (`--fork-key`). A second create
  under the key returns the open fork and writes nothing; once that fork has ended,
  the key answers "ended" and makes no new fork.
- **End.** A fork created without a key ends, by `agentfs`, once no client is
  attached to it (its view is unmounted, or a crash left it with none), or when its
  creating launcher (or the volume's own) ends it. A keyed fork ends only by its key (`fork end --fork-key`), for any
  launcher that presents it: a fork claim outlives the pods that mount it. An ended
  fork is never written again.
- **Drop.** An ended fork drops once its upstream's queue holds nothing it put
  there (every save has reached the referee), no file is open in it and no fork
  names it as upstream. The drop is one step with `ForkDropped`: the fork's branch
  and every entry of its own queue go, and the builder deletes its refs and then
  its checkout. Its row chain and the collision refs of its losing saves stay; its
  landed saves stay reachable from the root's landing merges. Its sync merges carry
  no edits of its own and become unreachable (`git fsck` lists them as dangling).
- **When.** The end of an unattached unkeyed fork and the drop are judged at boot
  (after the clients are reconciled), after each landing a view host takes, at each
  last detach and at each end; each judgement asks the facts now (Clients for who is
  attached, the journal for what holds a fork, never history), so an end or a drop a
  crash or a failed step left undone is judged again, and a failing builder holds no
  drop back. A drop that meets an open file waits for the next judgement. The boot's
  reconcile also releases the writer claim of a fork that dropped before its claim
  was released.
- **Restart.** A drop on record is a drop after a crash: the restart forgets the
  fork, deletes a checkout a crash left, and builds what was not built. A take of a
  dropped fork finds nothing to take; one decided before the drop is refused when
  it would land.

The journal records a drop as `Change::Drop { fork }`: its pointer changes delete
the branch and the queue entries, and the actor, the restart's replay and the
builder each forget the tree.

## Volume lifecycle

A volume is staged while a mount serves it, draining once it is told to stop, and
released when nothing promised would be lost if the process stopped now. Its row on
`refs/agentfs/volume` says which; every move is one journal step with its event
commit, durable before the call returns. A volume `init` just created has no
row and counts as released.

- **Stage.** `mount` stages the volume first: it takes the state area's lock,
  recovers the volume and records `VolumeStaged` (by `launcher cli`, with the
  mount's pod context). A mount after a kill finds the volume staged and writes
  nothing; one during a drain cancels it. Then it restores the forks and clients,
  rebinds or attaches each view's client, reconciles the rest, and drops what may
  drop ([clients](#clients), [forks](#forks-keys-end-and-the-drop)).
- **Stop.** SIGTERM or SIGINT, the exit of `-- COMMAND`, or the last view unmounted, unstages the volume:
  `VolumeDraining Stop`. From then no client attaches. The mount detaches each
  client, whose open files close as saves first (a file a process still holds open
  is saved as it stands, and a write after that answers `EROFS`), then unmounts each
  view lazily (`fusermount3 -uz`), and reconciles: every other client of its
  launcher left in the index (one a failed end at boot kept) ends too, so the
  release is judged from Clients alone. A reconcile that cannot record says
  `tree ReconcileFailed` on stderr. Every signal is an event on the mount's one
  channel; nothing polls.
- **Release.** Each client's end and each landing a take makes re-judge the
  release. It waits while the volume is staged, while any client is attached, and
  while any tree's queue holds an entry; with none, `VolumeReleased Keep` (by
  `agentfs`) is recorded, the mount leaves its loop, waits for every view to end,
  and exits 0. A view whose file a process holds open ends when that file closes.
  No remote is asked yet: with none, the volume is settled.
- **A failed builder holds the release.** Queues are read from the journal, but an
  entry is taken only once history is built behind it. While the builder fails,
  the drain waits: the volume stays `Draining`, recording goes on, the process stays
  up with the builder's alarm, and Kubernetes' grace period ends it. Nothing
  recorded is lost: the next mount recovers every record and builds it.
- **A stopped actor ends serving.** After a recording failure stops the actor,
  nothing more can be recorded, so no drain can finish. Once its views have
  ended, the mount exits 1 with the actor's error; a stop closes its views first.

## What a tree does

- **Saves.** A close applies the file's byte calls to the current saved file and
  aligns its lines once (git's histogram diff, the one call in `agentfs-align`,
  shared with the model). A close that leaves the bytes as they were commits
  nothing: no commit and no queue entry. The history does not show that the program
  saved.
- **Close-to-open.** An open file is private until it closes: other readers see
  only saved content. A read-only descriptor keeps the bytes it opened on; a
  writable one reads its own calls and the file's page writes.
- **Flow.** The queues carry changes between the upstream and the fork. The
  upstream's queue decides collisions and keeps the losers. A fork receives what no
  open file holds back: a file open for writing, and its decisions, wait, and the
  rest arrives when the file closes.
- **Syncs.** When a fork has nothing waiting and shows what its upstream shows, a
  sync fast-forwards past the fork's earlier sync merge, which carries no edits. No
  ref reaches that merge any more.
- **Landings.** A landing on the upstream's head is a fast-forward: there is no
  landing commit, so history does not show when it landed or that a queue took it.
  Writer and tree still show. A landing locks names as well as files, so it can wait
  on an open file it does not change.
- **Directories, metadata and links.** A directory is its path, so two trees
  making one directory agree; a chmod or chown lands over the value its writer
  saw; a link lands as a file whose bytes are its target. A change that loses by
  these rules loses whole and names its cause: `metadata`,
  `directory-not-empty` or `directory-gone` ([landing directories, metadata and
  links](library/core.md#landing-directories-metadata-and-links)).
- **After a truncate**, writes count as a rewrite unless every call is an append.
- **The release of `git` decides alignment.** An upgrade is a known change: later
  saves can align repeated lines differently. Stored bytes and identities do not
  change.

`fsync` and close make the file's calls durable; the final release publishes the
file.

## Recording and durability

Every file call is in the journal before it returns: one frame, written with one
`write(2)` and no fsync. A call that returned survives a crash or kill of agentfs,
because its frame is in the kernel. It survives a power cut once one of these has
returned:

- `close(2)` of a descriptor that wrote (FUSE `FLUSH`), or `fsync(2)` of it;
- each `write(2)` of a descriptor opened `O_SYNC` or `O_DSYNC`;
- `fsync(2)` of the view's directory (FSYNCDIR), or of a read-only descriptor.

Each of these makes every record so far durable, not only the descriptor's own:
creates, renames, unlinks, `mkdir`, `rmdir`, `symlink`, `chmod`, `chown` and
time sets included, so `fchmod` then `fsync` keeps the mode. A power cut loses
the calls and steps made since the last of these.
Waits share fsyncs (group commit): one fsync answers every wait whose records were
written before it began. The fsync runs off the view's request thread and outside
the actor's lock, so a waiting close holds no other call. A failed fsync stops the
actor: its waiters get its error and every later call `EIO` (see
[Recovery](#recovery)).

## File maps

No file's bytes are held in memory, by the views or by the builder. A file is a map: pieces of journal frames
and of the private checkout's file as built, by file offset. A write adds one
piece that points at its frame, so it costs its own bytes at any file size: a
4 KiB write into a 1 GiB file copies 4 KiB, into its frame. An open starts from
the saved map, shared until it writes (copy on write). A close applies the
open's calls to the saved map with the same function the builder replays, and
tells a close that changes nothing by comparing only the bytes the open's calls
touched. A view reads a map with the actor unlocked: the lock is held only to
clone it.

- After a batch is built and checked out, the builder reads each changed file
  from its checkout instead of the frames, through a hard link in the state
  area's `bases/` (cleared at each start), and the actor takes that map at its
  next call where it saved the same step. A checkout writes each file to a
  temporary name and renames it over the old one, so a reader that opened the
  old version keeps reading it through its link; the link goes when the last
  map lets go of it.
- A map holds no descriptor: reads open the link through a cache of at most
  256 open files per process, so a volume of any number of files opens and
  reads under the default open-files limit.
- A take's merged files are in chunk records of at most 16 MiB just before
  its step, written from the files whose lines it keeps (a version's bytes are
  the same in every file that holds it), and their maps read them there; a
  take of a file of any size records.
- The builder aligns a save reading both versions of the file a line at a
  time, with a sampled index of where lines start. A new file and an append
  run no `git diff`; any other save writes both versions to `align/` in the
  state area for one `git diff` and removes them. `git diff` itself reads both
  files whole, so an edit of a file near the host's memory needs that memory
  in the `git` child. Blobs over 8 MiB stream through `git fast-import` and
  `git cat-file` (`core.bigFileThreshold`), which never hold them whole.
- The save record holds the result's length and a digest of what the close
  wrote: each piece its own calls and page-write pieces left, by file offset,
  length and place in the journal, and the bytes it kept as runs. The digest
  is computed from the saved length and the close's calls alone, never from
  how the kept bytes are sourced (journal frames or the checkout's file), so
  the builder, which rebases at other points than the view, reaches the same
  digest for the same save. A replay that does not reach it fails
  (`Disagrees`).

## The library

`agentfs_core::tree_actor::TreeActor` holds an exclusive state-area lease.
`TreeActor::create` makes a volume, `TreeActor::open` recovers one and never creates,
and `tree_actor::inspect` reads what a state area holds without the lease.
`tree_actor::Recording` serves a volume: the actor behind its one lock, with
`durable`, `durable_all` and their deferred forms waiting for durability outside it.
`Recording::open` makes every recovered record durable and serves at once, from the
journal: it never waits for history. Dropping the last `Recording` makes every record
durable and builds it.
`Recording::history` is the one owner of the Git store (`History`: the built point,
`wait_built`, built snapshots, lag), and `settle` waits until every record so far is
built: call it before reading Git. `TreeActor::head` settles first. Each recorded call names
its writer as a `WriterAt` (the writer and its pod context). `write_open`, `write`,
`truncate` and `close` are the recorded file calls; `mkdir`, `rmdir`, `symlink`,
`rename`, `unlink`, `set_meta` (chmod and chown) and `set_times` the recorded
steps on names, metadata and times. `node`, `node_at` and `children` show each
entry as a view presents it. A view reaches them through
`agentfs_core::session`: a `TreeSession` locks the actor with its client's tree and
writer, and `Sessions` is the binder Clients binds each client's authority to. Upstream and fork
share one bare repository. Merge identity is separate from an open object's
identity. See [library/core.md](library/core.md).

## What a commit holds

Each commit (`Agentfs-Format: envelope-2`) keeps three entries per file or link
and one per directory, in two levels of hashed folders:

```text
files/<aa>/<bb>/<path>      the file's bytes, a link's target
lines/<aa>/<bb>/<path>      JSON [identity, line map, meta]: the file's lines as runs, never their bytes
names/<cc>/<dd>/<id>        JSON name history of one file identity, ended files included
dirs/<aa>/<bb>/<key>        JSON path and meta of one directory
dirs/root                   JSON path ("") and meta of the root directory
```

`aa` and `bb` are hex digits 1-2 and 3-4 of `git hash-object --stdin` of the
path's bytes; `cc` and `dd` the same of the identity in decimal. The folders are
fixed (256 by 256), so an entry's place never changes when others come or go,
and a commit that changes one file rewrites only the folders on its three paths:
the builder's cost per save is the same at 100 files and at 100,000. `git log --
files/2e/65/a` is the history of a file named `a` (`printf a | git hash-object
--stdin` starts `2e65`). A file's path nests under its folders
(`files/7f/3c/skills/web/SKILL.md`): a file and a directory never share a path,
so no blob there is ever a folder too. A directory's record is one leaf named by
`key`, the whole `git hash-object --stdin` of its path (40 hex digits, of which
`aa` and `bb` are the first four), so a clone or checkout of history never meets
a name too long however deep the directory; the record holds the path:
`{"path":"a/b","kind":"dir","mode":"0755","uid":0,"gid":0}`.
The Root commit holds `dirs/root` alone, the root directory owned by
`init --owner UID:GID`, else by the process that ran `init`, mode `0755`.

A `files/` entry has git's own mode: `100755` when the owner may execute the
file (`S_IXUSR`), else `100644`, and `120000` for a link, whose blob is its
target. So `git checkout` and GitHub show executable bits and links. Metadata is
JSON with the mode as four octal digits:
`{"kind":"file","mode":"0644","uid":1000,"gid":1000}`. It travels in the
`lines/` entry a save rewrites anyway, so a save costs no more. Each tree's
private checkout (`trees/<tree>/` in the state area) mirrors `files/`:
`trees/root/2e/65/a`. A link is checked out as a regular file (mode `0600`)
holding its target: the state area never holds a symbolic link.

A directory step is a commit like a save: `Mkdir by <writer>`, `Rmdir by
<writer>`, `Symlink by <writer>`, and `Meta by <writer>` for a chmod or chown. A
directory's rename is one commit: every file under it gets a new name version,
so its cost grows with the entries under it. An explicit set of times
(`utimensat`) makes no commit.

A line map is `{"lines": [runs], "ancestors": [runs]}`. A line run is
`{"identity": "e:i", "version": "e:j", "count": n}`: `n` lines whose
identities and versions number on (`e:i`, `e:i+1`, ...), with `"dead": true`
for deleted lines and `"open": len` when its last line has no newline (that
line's length in bytes). The live lines divide the file's bytes in order,
each at its newline. An ancestor run `{"version": "e:j", "count": n, "over":
"d:k"}` says each of `n` versions was written over the one numbered alike
from `d:k`; without `over` they are new lines. A new file is one run of each
at any size, so a map costs its saves' edits, not its file's bytes.

Object numbers (which file an open handle writes to), directory numbers and
times are not in history. They are the journal's: a checkpoint holds each tree's
objects as of its built point, and each tree's directory numbers and times and
its files' times (`locals`), and the steps after it move them. Each built-log
entry also records what its batch moved of them, tree by tree: the objects set
or unnamed, the directories set or removed, the files' times set or dropped, a
tree the batch made (all of it) or dropped. A start after a kill, whose built
point is past its checkpoint, carries the checkpoint's objects and locals to the
built point with those records and reads each tree once, at the built point. A
checkout of history shows checkout times, as git's does.

## The journal

Every step (a create, a link, a close that saves, a mkdir, an rmdir, a symlink,
a rename, an unlink, a chmod or chown, a set of times, a fork, a take, rows
alone) is one record in the journal: the change itself, the rows it
writes ([Rows and event commits](#rows-and-event-commits)), and the pointer changes
its build publishes (branch moves, queue entries, collision refs, row chains), each
value an oid or "what step `n`'s build gives" (on a row chain, step `n`'s last
event commit on it). It is appended as a call is and made
durable as a call is. That record is the commit point. The actor decides the step,
appends it and applies it to the saved state the views serve; it runs no git.

- The journal is a directory of segments, `journal/<first record>.seg`, each
  about 64 MiB; a frame never spans two, and a frame over 64 MiB has a segment of
  its own. A new segment's directory entry is fsynced before the durable mark
  covers any record in it.
- A failed step keeps serving and saves at the next step in its tree. Only a failed
  journal `fsync` (a leader's or a compaction's) or rollback stops the actor; a
  failed fsync is never retried, since the page cache then says nothing about the
  disk.
- Recovery reads the journal and serves from it without waiting for history: the
  saved state is the trees as built (read from Git and the private checkouts)
  with every later record replayed on them by the transitions the actor ran at
  the call. It then closes abandoned opens through the ordinary step; the builder
  catches up behind, as at any time (see
  [History trails the files](#history-trails-the-files)), and a failed build
  raises its alarm without refusing the open. The recovered records are made
  durable before anything is served (the directory first), and the steps past the
  built point count in the lag gate as they did at their call. A journal whose
  steps are all built compacts at once; otherwise compaction runs at the first
  call after the builder catches up. It refuses
  a missing journal, a missing `format` file, a lost object at a pointer's tip, and
  a journal that names a tree by anything but its identity, has no root, gives the
  root an upstream, gives an upstream to a fork with no branch, or gives a fork an
  upstream other than the root (forks are made only of the root). It deletes
  nothing outside a first init's own marker.
- Compaction runs when the segments pass `max(64 MiB, twice the last compacted
  length)`: it writes a checkpoint at the start of a new segment, then drops
  (unlinks) every older segment whose records the builder has read and which
  holds no live open's call and no page write memory holds. A held file pins only its own
  frames: they are copied forward (marked `carried`, their record numbers kept)
  once the dead bytes they pin exceed their own, so the segments stay under about
  twice the live opens' bytes plus a segment. A frame is named by its record
  and an offset into it, wherever it is carried, so a save's digest does not
  change when it moves. The journal records where each carried frame went; the
  actor moves its live opens, page writes and saved maps there as it compacts,
  and the builder moves its own before its next batch, so the dropped segment's
  space returns then. A reader holding a dropped segment (a view's open file)
  keeps reading it; its space returns when the last lets go. Frames are at most
  96 MiB; a take's bytes are in records of at most 16 MiB. The constants are not
  settings. A checkpoint's pointers are the built ones, so a compaction that is
  due waits for the first call after the builder has built every step and read
  past the oldest segment; calls after the last step (a file under constant
  writes) never hold it back, since the checkpoint holds them, and only the
  segment they share with dead records waits for the next step. While steps
  trail, the journal grows; once it is twice its due length, the lag gate holds
  every tree's new work until the builder has built every step. A compaction
  that fails before its checkpoint changes nothing and is retried at the next
  call; one that fails while dropping leaves those segments for the next. Its
  fsyncs (every segment the checkpoint covers, the directory, the frames it
  copies forward) stop the actor when they fail, as a leader's do.
- Recovery folds the last checkpoint, the live opens' calls and the held page
  writes it names from the segments before it (kept or carried, each once), and
  every frame after it.
- Every frame carries the durable mark: the last record a finished fsync covered
  when the frame was written. At the first invalid frame, a later valid frame with
  this journal's salt whose mark passes the last valid record means a durable record
  was lost: corruption, and the journal refuses to open. Otherwise the hole is a
  torn tail of records no fsync had covered, saved to `journal/torn` and cut off,
  with every later segment. A journal whose valid records end before the built
  log's point is corruption too: the builder made those records durable before it
  built them.
  Bit rot in the very last frame, or in records not yet durable or built, reads as
  a tear and drops those records; the log line and that file are the evidence.
- The state area is `volume`, `history.git`, `journal/`, `built.log`, `align/`,
  `bases/`, `actor.lock`, `format` and rebuilt private checkout caches, which the workload
  never sees. A create writes an `initializing` marker first and removes it last;
  the next `init` starts a cut-short create over. The state format is
  `agentfs-trees-12`. **Earlier state formats (11 included) do not open**, and
  are never converted ([upgrading](runbooks/upgrading-the-sidecar.md)).

## History trails the files

A builder thread makes each step's commits behind the call path, in batches of at
most 256 steps, so a long backlog ends in a built event every batch. For a batch it
makes every record through the last one appended durable (one fsync of the
journal), reads them, replays each step on the trees as built (the same transitions
the actor ran, then the line alignment), and feeds the commits to one
`git fast-import`, each changed file's blob streamed from its map. It installs the batch in this order: the pack, then its index;
the new pack files and `objects/pack` fsynced; the batch's entry in `built.log`
fsynced. That entry moves the built point. Only then are Git's refs, `config` and
the private checkouts copied, the checked-out files' maps rebased onto them
([File maps](#file-maps)), and waiters woken.

- A save is visible to every view at its release, before it is built. Git's refs,
  commits and checkouts show only what is built.
- Takes are decided on built history, off the actor's lock (below, "Takes").
- A commit is dated by its step's record, in `+0000`. A replay after a crash may
  make other objects for steps nothing had seen built.
- A failed batch (`tree HistoryBuildFailed: <e>`) changes nothing reachable and is
  retried at the next step or wait; recording goes on. A batch whose replay
  disagrees with its record (`Disagrees`) fails the same way at every retry: see
  [runbooks/recording-failures.md](runbooks/recording-failures.md). A failed copy
  after the built-log entry is logged (`tree CopyStale`) and redone whole at the next
  batch.
- Dropping the volume builds every record appended, so a clean shutdown leaves
  history built through its last record. After a kill the builder builds what
  was left unbuilt, behind the reopened views.
- No record after a checkpoint names a step the checkpoint covers: a step whose
  pointers were read before a compaction names those values as the oids they
  were built as, so a restart, which builds from the checkpoint, builds every
  later record as an uninterrupted run would.
- `--metrics-output` reports `history`: the built point, the last record appended,
  the oldest unbuilt step's age, the worst tree's lag and the builder's health.
- **The lag gate.** Each tree's lag is the builder's estimated time for its unbuilt
  steps: the larger of their count at a cost per step and the bytes they write
  (the bytes a save or take leaves, plus a constant for the commit and its three
  hashed folder levels, which do not grow with the tree) at a cost per byte, each
  measured on the last batch as if it bore the whole batch. Over 120 s, a call
  that starts new work in that tree (a create, a write-open, `O_TMPFILE`, a link,
  a mkdir, an rmdir, a rename, an unlink, a chmod, a path truncate) waits for the built event that brings the
  lag under; other trees, writes, flushes and closes do not wait, and the view
  answers other requests meanwhile. One gate thread per volume answers every
  waiting call at the built or failed event that admits it.
  The kernel holds a directory's lock while a create, link, rename or unlink in it
  waits, so a lookup or listing of that directory waits with it. A failed builder
  holds nothing back. Over 30 s,
  `tree HistoryLagging` is logged once; over the limit, `tree HistoryStalled`.
  The levels are constants, not settings.
- **The overlay limit.** The actor holds, per tree, each name, directory, object
  and file map its steps changed since the built point (its overlay); a built
  event drops what that build covers, so after each rebuild a tree's overlay is
  its unbuilt steps' entries. The gate estimates it at 256 bytes an entry and
  counts entries, not a changed map's pieces (the lag's byte term covers a save's
  bytes). The actor reports every tree's overlay as it releases its lock, so the
  gate sees it grow at each step and shrink at each rebuild, without a timer.
  Over `agentfs.overlayLimit` (32 MiB unless set,
  [configuration](configuration.md#tree-volumes-the-operator-settings-file)),
  the tree's calls that start new work wait as over the lag limit, until the
  built event whose rebuild brings the overlay under; the health is `Stalled`
  meanwhile and `tree HistoryStalled` is logged once. The check is before the
  call, so one step larger than the limit (a rename of a directory of many
  names) is admitted, and the calls after it wait until it is built. A failed
  builder holds nothing back here either: the overlay cannot shrink while it
  has failed.

## Takes

A tree's queue entries are taken one at a time per volume, by the view's worker or
by `take_next`. Each take runs in three parts:

1. Under the actor's lock: pending closes in the tree save first, then the take
   reads its inputs: the keys the tree's write-opens hold, the last step that moved
   the tree's head, and the last record appended.
2. With the lock released: it waits for history built through that record, then
   plans and judges the first entry P1 lets go on that built snapshot. Calls go on
   meanwhile.
3. Under the lock: it lands as a take step, or is refused when a step after the
   decision touched a key it read (`FilesChanged`), a write-open holds a key it
   writes (`Held`), a fast-forward's head moved (`HeadMoved`), or its entry is
   gone (`EntryGone`). A refused take is logged (`tree TakeRefused`) and decided
   again at once, on the history that refused it.

A take judges directories, metadata and links by the model's rules 13 to 15, as
the library describes ([landing directories, metadata and
links](library/core.md#landing-directories-metadata-and-links)); a landing that
fast-forwards takes the fork's commit, directories and all. A take never leaves
an entry without its directory.

A landed take read nothing of its tree that changed after its decision, so it decides
what a take made in one step at its land would. A sync reads its upstream's head as
of its decision, as a sync taken then would; the upstream's later saves queue later
entries. A fork's entries are planned once per take: each is a sync marker, and P1
takes the first or none. The worker wakes on built and failed events,
never a timer. A take whose files change faster than the builder's lag plus its
decision waits until they stop.

## The view

The view host (`agentfs_fuse::tree_view::ViewHost`) mounts each view on its
client's session, never waiting for history: the tree must be among the actor's
saved trees. A view serves the tree's directories: `mkdir`, `rmdir`, creates
anywhere, and renames across directories (`RENAME_NOREPLACE` too;
`RENAME_EXCHANGE` and `RENAME_WHITEOUT` answer `EINVAL`), with POSIX's errors
(`ENOTEMPTY`, `ENOTDIR`, `EISDIR`, `EINVAL` for a directory moved into itself).
Inode 1 is the root directory; every other inode is its entry's node number + 2.
Each entry shows its recorded kind, mode, owner, group, link count, size and
times. A new entry takes the mode the kernel gives (its umask applied) and the
calling process's uid and gid; in a set-group-ID directory it takes that
directory's group, and a new directory keeps the set-group-ID bit. `chmod`,
`chown` and `chgrp` record the new mode, owner and group as one change, the
values not given kept. The kernel judges who may make them and every access by
the recorded mode and owner (`default_permissions`): root may chown, an owner
may chgrp to a group it is in. The mount root starts owned by `init --owner`, else
by the process that ran `init`, mode 0755; a workload that needs it gets it by a chown
through the mount, which history records as any other.

`symlink` makes a link (mode 0777, owned by the caller) whose target, which must
be UTF-8, is its bytes; `readlink` reads it, the kernel follows links, and a
link is renamed and removed like a file. Opening a link itself answers `ELOOP`.
A regular file's `mknod` is a create; a FIFO or a socket is made in the
untracked area (below), and a device answers `EPERM`.

Times are each tree's own and never in history. A save (at its last close, not
at each write) moves its file's mtime and ctime, and a create, rename or remove
moves its directory's. `touch` and `utimensat` set atime and mtime as a step
that builds no commit, so the head does not move; the times survive a restart.
A time before the epoch answers `EINVAL`. atime is the last time set, else the
time the entry was made: as on Linux a write does not move it, and reads do not
either. The FUSE adapter uses zero entry and attribute TTLs and
direct I/O, and negotiates neither passthrough nor the writeback cache. A read with
no lock owner is a page fill and a write with `FUSE_WRITE_CACHE` is a page write.
`max_background` is as high as the kernel allows, to keep a release ahead of a later
open. One thread answers requests in order; a `FLUSH`, `FSYNC`, FSYNCDIR or `O_SYNC`
write is answered from the journal's waiter thread once its records are durable. The queue worker takes one entry at a time, releasing the actor between takes,
and drops the kernel's pages of files a take changed. A file open for writing
holds back that fork's syncs from its upstream until it closes; no alarm is raised
for a waiting open. See [library/filesystem.md](library/filesystem.md).

## Rules and the untracked area

The operator's rules decide which new names a tree records. A name they leave
out lives in the tree's **untracked area**: a plain directory of the state area
that the view merges into the tree. Nothing in it reaches the journal, history,
a queue or a remote.

**The rule files.** `<state>/history.git/info/exclude` and
`<state>/history.git/info/attributes`, in git's syntax. They are read once, when
the volume opens, before anything is written; a missing file is empty, and a
change takes a remount. Files in the tree, `.gitignore` and `.gitattributes`
among them, are ordinary content and decide nothing.

```text
# history.git/info/exclude
*.log
/sessions/

# history.git/info/attributes
*.db     -agentfs
*.db-wal -agentfs
.env     -agentfs
```

**Rules decide only for new names**, as git's ignore rules apply only to
untracked paths. A path the tree already records stays recorded, whatever
`info/exclude` says; one `-agentfs` marks leaves history at the next open
(below). The other way, a file already in the untracked area stays untracked
when a rule is loosened (a pattern removed, an allowlist line such as
`!/skills/` added): its later edits are not recorded. Each open names every
such entry on stderr, `tree Unadopted: tree=<id> path=<path>: ...`; an
untracked directory is named once, not walked. To record such a file, move it
out of the mount and back in. A new name is placed by the first of these that matches:

1. Its directory is untracked: everything under it is too.
2. A `.git` component: a nested repository's own files.
3. A FIFO or a socket: `mkfifo`, and a unix socket's `bind`, make it in the
   area.
4. `-agentfs` in `info/attributes`: never recorded.
5. Inside a nested repository: a directory above the name, other than the
   tree's root, holds a `.git` entry.
6. Excluded by `info/exclude`: the last matching pattern wins, matching is
   case-sensitive, and once a directory is excluded nothing beneath it is
   re-included.

Anything else is recorded. A rule tree volumes do not build refuses the
volume when it opens, naming the line, before anything is written: any
`agentfs.<name>` attribute, `filter=lfs`, and `agentfs=<value>`. Other git
attributes are accepted and change nothing.

**A tracked path marked `-agentfs` leaves history when the volume opens**, as
`git rm --cached` takes a file out of the index and leaves it in place. For each
tracked file or link of each tree that `-agentfs` now marks, the open first
places its saved bytes, mode and owner in the tree's untracked area (its
directories as containers with their own mode and owner), then records its
unlink as writer `agentfs`: one `Unlink by agentfs` commit per path, and one
stderr line, `tree Untracked: tree=<id> path=<path>: marked -agentfs in
info/attributes`. The file reads back as before, now untracked. A crash between
the two steps leaves the path tracked with a copy in the area; the next open
places it again and records the unlink. Its earlier commits stay in history:
rewriting them is a manual procedure
([runbooks/history-rewrite.md](runbooks/history-rewrite.md)). Directories stay
tracked.

**What untracked means.** An untracked file is an ordinary Linux file: a write
is seen at once by every reader, shared mappings stay coherent, `fsync` makes it
durable, and record locks work within the view. No call on it is journaled or
waits at the lag gate, takes and syncs never carry it, and history never shows
it. Its mode, owner and times are the file's own; a new one is owned by the
process that made it when the view host may give files away (`CAP_CHOWN` and
`CAP_FOWNER`), else by the view host's user. Reads and writes through an open
untracked file never wait for the volume's lock; calls that make, remove or
rename an untracked name, and listings, take it as tracked calls do, so each
name is in one half at a time.

**One namespace.** `ls` shows both halves. A tracked entry wins its name: an
untracked entry under the same name stays hidden until the tracked one goes. A
tracked directory that holds untracked entries is not empty, so `rmdir`, and a
directory's rename onto it, answer `ENOTEMPTY`; renaming it moves its untracked entries with it. Moves
and links between the halves are under "Moves across the halves" below.

**Nested repositories.** `git init repo` in a view records the directory
`repo`; `repo/.git` and every file made inside `repo` afterwards are untracked,
so the repository works and its files stay out of the tree's history.

**Forks.** A new fork's untracked area starts as a copy of its upstream's,
taken when the fork is made:

- The copy is the upstream's files at one instant. The upstream's untracked
  changes wait while it is taken, so an open database is copied as a power cut
  would leave it, and SQLite's write-ahead log recovers it when the fork first
  opens it.
- A fork never has part of a copy. The copy is made under
  `<state>/untracked/.staging/`, made durable, and renamed into place before the
  fork's step is recorded. A crash before the step leaves a copy no fork owns,
  which the next open removes.
- Each file is reflinked where the filesystem can (`FICLONE`), else copied. A
  file with several names is copied once, and its other names are links to the
  copy. FIFOs, sockets, and names that are not UTF-8 (which no view makes) are
  not copied.
- Later writes on either side stay on that side.
- A dropped fork's area is removed when its drop is recorded, whether or not
  anything built the fork.

The costs: every secret in the upstream's area (an `.env`, a token) is copied
into every fork; each copy takes space where reflinks are not available; and the
upstream's untracked writes, with every call on the volume, wait while the copy
is taken, for as long as copying the area takes.

**Where it lives.** `<state>/untracked/<tree>/` (mode 0700), made on first use
and reached only beneath it, never following a link. Directories in it that
stand for tracked directories take their mode and owner. Untracked data shares
the state area's disk: an area that fills it stops recording too (`ENOSPC` on
the journal).

### Moves across the halves

A rename's target is in the **tracked** class when a tracked entry holds the
name, **untracked** when an untracked entry holds it, and otherwise where the
rules would place a new name of the source's kind (so
`mv notes.md logs/notes.md` with `/logs/` excluded moves the file out).

| Source | Target class | What happens |
|---|---|---|
| Tracked file or link | Tracked | The tree's rename, as always |
| Tracked directory | Tracked | The tree's rename; its untracked entries move with it |
| Tracked file or link | Untracked | Moved out: its saved bytes, mode and owner are placed in the area, then its unlink is recorded |
| Tracked directory | Untracked | `EXDEV`: `mv` copies the files across, then removes the tree's |
| Untracked file | Tracked | Moved in: one save of the whole file under the new name, then the area's file is removed |
| Untracked link | Tracked, no entry there | Moved in: a new link, then the area's link is removed |
| Untracked link | Tracked, an entry there | `EXDEV` |
| Untracked directory, FIFO or socket | Tracked | `EXDEV` |
| Anything untracked | Untracked | A plain rename in the area |

`RENAME_NOREPLACE` answers `EEXIST` for a name in either half;
`RENAME_EXCHANGE` and `RENAME_WHITEOUT` answer `EINVAL`. A file moved in keeps
the identity of the tracked file it replaces and the identities of its
unchanged lines, as a save after an edit does. So Python's
`tempfile.NamedTemporaryFile` plus `os.replace` from an excluded temporary name
onto an allowlisted `config.yaml` records one save of `config.yaml`, and
history never shows the temporary name.

A hard link (`link`) is made within the untracked half; a tracked file that
`O_TMPFILE` made takes its first name in the tree, as always; a named tracked
file answers `EPERM`; a link across the halves answers `EXDEV`. `mkfifo` and a
unix socket's `bind` make the entry in the area; `mknod` of a device answers
`EPERM`. `O_TMPFILE` in an untracked directory answers `EOPNOTSUPP`, since
its file could only become untracked. A read-only view answers `EROFS` to all
of these.

The costs:

- A move across the halves copies the file's bytes. A move into the tree
  copies them into the journal under the volume's lock, so every call on the
  volume waits while it copies, for as long as the file takes.
- A crash during a move across the halves may leave both copies: the file in
  the area and in the tree.
- A crash during a tracked directory's rename, after its tree step, may leave
  its untracked entries under the old name.
- A descriptor opened on an untracked file before it moved into the tree
  answers `ESTALE` to writes and truncates; its reads keep the bytes it had.
- A file moved out of the tree ends its write-opens as an unlink does: what
  they had not saved is dropped (`tree OrphanDropped` on stderr).

## Recovery

On a stopped actor, preserve the state, fix the I/O or space problem and reopen. On
`CorruptJournal`, `MissingObject` or an unknown format, follow
[runbooks/recording-failures.md](runbooks/recording-failures.md#5-a-tree-state-area-that-refuses-to-open).
Nothing truncates journal records or resets history as repair.

## Limits

What the tree views do not support:

- **Git-safe UTF-8 names only** in the tree, each component at most 255 bytes,
  and UTF-8 link targets. A `.git` name lives in the untracked area; git's
  other spellings of it (`.GIT`, `git~1`) answer `EINVAL`. Hard links to a
  tracked file past an anonymous file's first answer `EPERM`; devices answer
  `EPERM`; FIFOs and sockets are untracked; extended attributes and ACLs are
  not supported.
- **Times.** A file's mtime moves at its last close, not at each write
  (close-to-open). Reads are not tracked: atime is the last time set, else
  the time the entry was made. A checkout of history shows checkout times, as git does.
- **A file no name reaches has no metadata steps.** `fchmod`, `fchown` and
  `futimens` of an `O_TMPFILE` file, or of a file unlinked while open, answer
  `EOPNOTSUPP`, so the `O_TMPFILE`, `fchmod`, `linkat` pattern fails at the
  `fchmod`. An `O_TMPFILE` file made in a set-group-ID directory keeps the
  caller's group, and `linkat` keeps it.
- **Rules not built.** Only `-agentfs` and `info/exclude` decide; other
  `agentfs.*` attributes, LFS, `agentfs.nested`, `respectTreeRules` and
  extended attributes on untracked files are not built. A lookup that misses
  and each create cost an `lstat` in the area, plus one per directory above
  the name.
- **Replication and patches are not built**: a tree volume pushes to no remote
  and applies no patch queue. A configured remote or patch source is said at
  every boot and counted by `agentfs.not_built` ([operator
  settings](#operator-settings)). The recording commands (`status`, `push`,
  `restore`, `revert`, `patch` and the rest) refuse a tree volume with `not built
  yet`.
- **Nested forks, transactions, `Client::Outside`, view profiles, CSI, several
  view hosts, upgrades and snapshot opens** are not built. The designs for them
  are in [target-architecture](target-architecture/README.md).
- **Telemetry export** carries one gauge, `agentfs.not_built`, and the binary's
  own export failures, over OTLP when the `OTEL_*` variables name a collector
  ([telemetry](telemetry.md)). `--metrics-output` is the only other measurement.
- **Large files.** A save costs its own file: the builder's memory does not
  grow with the file (17 MiB for a 1 GiB save in one local run, its history
  built in about 40 s; see [performance.md](performance.md)). Still: the
  state area needs about three times a large file while history builds (its
  journal segments, its blob, its checkout); an edit that is not an append
  runs `git diff`, which holds both versions; a take expands its files' line
  maps line by line and reads the files it merges, through `git cat-file` for
  any over 8 MiB; and a line map keeps every deleted line, so maps of files
  edited often grow with their edits.
- **A call can wait.** A call that starts new work in a tree waits while that
  tree's lag is over the limit, or while an overdue compaction waits for the
  builder (see [History trails the files](#history-trails-the-files)); every call
  waits while a take lands under the actor's lock. A take waits for its entry's
  write-opens to close, so a file held open holds the takes that write it. The
  model replays do not cover the waits; the mounted cases do.
- **Shared mappings have a gap.** A write through an `mmap` is saved as a page. A
  mapped store that writes the bytes its page last held records nothing, so a
  descriptor write made meanwhile stands where Linux would keep the store: with a
  file `abcdefgh`, one writer `pwrite`s `WX` at 0, then another's mapped store writes
  back `ab`; Linux keeps `abcdefgh`, agentfs gives `WXcdefgh`. This was found by
  reading the code and has not been run. A store not yet written back is lost with
  the view host, which Linux would keep, and `stat` and `sendfile` show unsaved
  state.
- **Close-to-open** rests on kernel 7.0 and the sources of v6.8, v6.14 and v7.0: 64
  closes over a busy view, from the same process and another. A readahead in flight
  on the handle was not exercised.
- **The pack syncs** cannot be judged by prefix replay on ext4 and XFS (a later
  fsync carries the pack's directory entry). `ci/trees-object-dir-syncs.sh` proves
  only the order of system calls (pack files and `objects/pack`, then the built-log
  entry), not a crash.
- **Replays are not a proof.** The model's scenarios and seeded random interleavings
  are replayed on the model, the library and real mounts. They show no `mmap` call
  and no client call waiting on a take, and do not cover every interleaving.
- **A debug build and one runner.** Measurements are not budgets. Two runs of one
  mode have differed by up to two times.

## How it is verified

CI scripts judge the build; local `tree_actor`, `tree_git` and replay tests use real
repositories but are not FUSE or power-loss evidence. The scripts are mapped to their
jobs in [development.md](development.md); the method is in
`crates/agentfs-model/VERIFYING.md`.

- `e2e-trees.sh`, `e2e-trees-ordering.sh`: recording, objects, anonymous first link,
  immediate reopen; directories, modes, a moved directory and an executable
  script through a view, owners (chown, chgrp, a set-group-ID directory), links
  and times (`touch` makes no commit), and their history (`trees-posix.py`).
- `e2e-tree-conformance.sh`: fixed model cases and byte calls against a plain Linux
  file.
- `e2e-tree-replay.sh`: the model's in-scope scenarios replayed on a fresh model, the
  library and real mounts, comparing each tree's files, directories, kinds and
  metadata, each collision's lost lines and each loss's cause by rules 13 and 14,
  with invariants read from git (D1 among them: every entry's parent has a `dirs/`
  record). The rules' scenarios for directories, metadata and links are also kept
  as traces in `crates/agentfs-conformance/tests/rules/`, which `cargo test`
  replays on the model and the library. A mount makes only the calls one system
  call can: a chmod and a chown are two changes on Linux, so a trace that changes
  both at once, or makes a directory or link owned by another user, replays on the
  library alone.
- `e2e-tree-pagecache.sh`: shared-mapping behaviour in 21 cases, and 64 closes
  reopened.
- `e2e-tree-restart.sh`, `e2e-tree-recording-faults.sh`, `e2e-tree-enospc.sh`:
  view-host death, kills at each failpoint, state exhaustion.
- `power-loss-trees.sh`: `dm-log-writes` replay on ext4 and XFS, on the upstream and
  a fork, each checkpoint also rebuilding the refs from the journal alone.
- `measure-trees.sh`: plain-FUSE comparison, an hour of writes, 10,000 saves, 1 and
  100 MiB files, waits and a held log. `[phase1-measure]` in a pushed commit message
  runs them all.

Not tested: a fork's untracked copy taken while a program is writing through the
upstream's view. `mount` makes its fork before any view serves, so no built
launcher copies an area under live writes. `e2e-trees.sh` copies the crash image of
a SQLite database whose writer and view host were killed mid-write; the library's
`tree_rules` tests copy areas while threads change them under the area's hold.
