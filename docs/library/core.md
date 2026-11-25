# The library: `agentfs-core`

`agentfs-core` is the engine. It records change boundaries durably, turns
them into git history, pushes that history, restores from it, applies the
patch queue, runs forks, and serves the history service. It has no FUSE and
no command line. Anything that can tell it "this path now holds this
content" can record into a volume with it.

Use this layer when you build a different front end over agentfs's
recording (another filesystem, a build tool, a test harness), when you
write tooling that reads or changes a state area, or when you build an
adapter like the [Kubernetes one](kubernetes.md). The [filesystem
layer](filesystem.md) is one such front end.

This page lists the layer's main types and functions, with examples that
match the current signatures. `cargo doc -p agentfs-core --open` is the full
reference. [Architecture](../architecture.md) maps every module;
[recording](../recording.md) says what a recorded change is and when it is
recorded; [guarantees](../guarantees.md) says what the engine promises.

## What the layer offers

| Need | Module | Main items |
|---|---|---|
| Open a volume, record, commit | `volume` | `StateDir`, `init`, `open`, `Recorder`, `Committer`, `Input` |
| Background commit and push loops | `service` | `Service`, `ServiceHandle`, `Stop` |
| Push to remotes | `push` | `RemotePusher`, `open_all`, `PushOutcome` |
| Alarms and events | `events` | `Event`, `EventSink`, `MemorySink`, `NullSink` |
| OpenTelemetry signals | `telemetry`, `tree_telemetry` | `Telemetry`, `Telemetry::sink`; `observe_process`, `observe_unverified` (the tree engine's, on a `Meter` the caller makes) |
| Exclusive use of a state area | `lock`, and `agentfs_os::file_lock` | `StateLock`, `FileLock` |
| A binary's malloc arenas | `agentfs_os::allocator` | `limit_arenas` (glibc's `M_ARENA_MAX` to `ARENAS`, 2): a binary calls it first in `main`, before any thread; the library never does |
| Settings from the git config | `config`, and `agentfs_contracts::units` | `Settings`, `Settings::load`, `KEYS`; `parse_duration`, `parse_size`, `duration_arg` (a duration flag's value parser) |
| Path rules and checks | `rules`, `checks`, and `agentfs_contracts::paths` | `Rules`, `Checks`, `RepoPath` |
| Restore and revert | `restore`, `operations` | `plan`, `check`, `apply`, `record_applied` |
| Patch queue | `patch` | `Queue`, `Verdict`, `PatchBranch`, `submit` |
| Forks and start points | `fork` | `create`, `Finisher`, `start_from`, `delivery` |
| Release decision for a worktree | `release` | `decide`, `Release` |
| Several mounts, one writer | `history_service` | `HistoryServer`, `HistoryClient`, `FrontLease` |
| Large files | `lfs` | `ObjectCache`, `Store`, `fetch`, `prune` |
| Git-backed tree views | `tree_actor`, `state_area` | `TreeActor`, `TreeId` (a tree's identity, `root` or `fork/<n>`), `TreeStateArea` (the Volume domain's `StateArea` port; its `inspect` reads what a state area holds) |
| The domains' rows over a tree volume | `store`, `tree_actor` | `TreeStore` (the `VolumeStore`, `ForkStore`, `ClientStore` and `QueueFacts` ports), `Chain`, `RowDomain`, `RowWrite`, `RowEvent` |
| Line identity across saves | `agentfs_lines` | `LineMap` (runs of lines, never bytes), `LineMap::align` (git's histogram diff over two `Image`s, read a line at a time), `LineMap::align_bytes_with`, `histogram_with` and `histogram_files_with` (the diff's `git` child started by the spawner passed: the tree engine passes `agentfs_git::git_cli::counted`), `LineMap::append`, `LineMap::expand` (line by line, for the landing rules), `pieces` (where a merged file's lines are in its sources): the line-map library, generic over the caller's numbered version (`Id`) |

## The state area

A volume's state area is a directory the workload never sees.
`StateDir` names its parts:

| Method | Path under the state area | Holds |
|---|---|---|
| `git()` | `git/` | The bare history repository and the operator's settings (`config`, `info/exclude`, `info/attributes`) |
| `journal()` | `journal/journal` | The change log |
| `pending()` | `pending/` | Frozen copies of recorded content, in slots |
| `meta()` | `meta/` | Locks, push state, repair queues, the control and history sockets, patch state |
| `lfs()` | `lfs/` | The local LFS object cache |
| `tmp_layer()` | `tmp-layer/` | The sandbox overlay's private files |

`StateDir::usage()` reports the apparent size of each part. The state area
and the backing tree must be separate directories on filesystems you
control; the filesystem layer refuses a layout where one is inside the
other.

## Record and commit

A change boundary becomes a record in three steps: freeze the content into
`pending/` (a reflink where the backing filesystem has one, else a copy and
an `fsync`), append a framed, checksummed entry to the journal, and group
commit the journal with one `fsync`. The call returns after that. Hashing,
compression, object writes and commits happen later, in a `Committer`.

Records hold absolute state ("this path now holds this content"), never
deltas, so applying a record twice gives the same tree.

`volume::init` creates a state area, or accepts one that already belongs to
the volume. `volume::open` opens it, runs crash recovery and returns the
recorder, the committer and what recovery found:

```rust
use std::sync::Arc;
use agentfs_core::events::NullSink;
use agentfs_core::identity::Context;
use agentfs_contracts::paths::RepoPath;
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Input, StateDir};

fn record_one(state_dir: &std::path::Path, tree: &std::path::Path)
    -> Result<(), Box<dyn std::error::Error>>
{
    let state = StateDir::new(state_dir);
    volume::init(&state, "demo", None)?;

    let context = Context { volume: "demo".into(), ..Context::default() };
    let (recorder, committer, recovery) = volume::open(
        &state,
        tree,
        context,
        Arc::new(NullSink),
        &Telemetry::noop(),
    )?;
    println!("recovered {} uncommitted record(s)", recovery.uncommitted);

    let recorded = recorder.record(
        Trigger::Close,
        Caller { uid: 1000, gid: 1000, pid: std::process::id() },
        vec![Input::Write {
            path: RepoPath::new("notes/todo.txt")?,
            backing: tree.join("notes/todo.txt"),
            executable: false,
        }],
    )?;
    // `seq` is None when the rules skipped every input.
    for skipped in &recorded.skipped {
        eprintln!("not recorded: {} ({})", skipped.path, skipped.reason);
    }

    // Turn what is journaled into commits now (the Service does this on a timer).
    committer.commit_now()?;
    Ok(())
}
```

`Recorder` is cheap to clone and safe to share across threads. Its main
calls:

| Call | What it does |
|---|---|
| `record(trigger, caller, inputs)` | Applies the path rules and checks, freezes, journals. Returns `Recorded { seq, skipped, written }` |
| `record_group(...)` | The same for an fsync-class group: all or nothing |
| `record_as(..., origin)` | Records a repair for another pod's origin (used by fronts) |
| `record_move(...)` | Records a directory rename as one move |
| `begin_operation(...)` | Starts an operator operation (restore, revert, patch) whose writes become one commit |
| `would_record(path, symlink_target)` | Why a write would not be recorded, if it would not |
| `wait_committed(seq, timeout)` | Waits until history holds everything up to `seq` |
| `tracked_query(paths)` | Which paths are in the tracked set |

`Input` is one path's new state:

- `Write { path, backing, executable }`: the file at `backing` now holds
  the content for `path`.
- `Symlink { path, target }`.
- `Remove { path }`.
- `Tree { path }`: read the path from the backing tree, confined beneath
  its root and never through a symlink. Use it for paths you find yourself
  in a tree a workload may be changing.

`Trigger` names what ended the change (`Close`, `Rename`, `Unlink`,
`Rmdir`, `Link`, `Symlink`, `Setattr`, `Fsync`, `Release`, `Sweep`,
`Interval`, `Restore`, `Patch`). It goes into the commit's trailers.

Commit messages and trailers are built by `history`; identities come from
the templates in the settings (`identity`). See
[recording](../recording.md) for the commit format.

### Rules and checks

Before it freezes anything, `record` asks the path rules (`rules::Rules`)
whether each path is recorded or skipped, and why. Rules only classify
untracked paths: a tracked path always follows the tree. Kept inputs then go
through the pre-commit checks (`checks::Checks`): git's `fsck` rules and
the remote's size limits. A refused path is skipped with
`SkipReason::Refused`, raises `Event::PathRefused`, and the rest of the
record carries on. The committer checks every change again, for records
journaled before the limits changed.

### Ordering

A record claims its paths (`claims`) from the decision until the tracked set
is updated, so records that touch the same path, or a path above or below
it, are decided and journaled in order. Records for unrelated paths do not
wait for each other.

### The journal and crash recovery

`volume::open` is crash recovery:

- A torn final journal entry (it fails its checksum) is discarded. Its call
  never returned, so nothing was promised.
- Records already in history (at or below the head's `Agentfs-Seq`
  trailer) are dropped from the journal. The rest stay uncommitted.
- Frozen copies that nothing refers to are removed.
- A ref lock file a crash left in the history is removed with a warning.
- An intact entry that does not parse was written by a newer agentfs. Opening
  fails and names `agentfs flush`; nothing is discarded. Run `flush` with
  the newer version before you downgrade.

The journal and `pending/` are truncated only after the commit and the
branch ref are durable. The commit's fsync order is: objects, `objects/`,
the branch ref, the directories above the ref, then the truncation. A power
cut at any point leaves either the old head with its records still
journaled, or the new head with everything it names.

`volume::inspect(&state)` reads the head, the last committed sequence and the
backlog without opening the volume for writing.

### Recording failures

When the synchronous step fails (the state area is full, say), `record`
returns an error and raises `Event::RecordFailed`. The caller decides what
to do. The filesystem layer returns `EIO` and queues the path on a repair
queue; the queue format is `record::SavedRepairs`, read with
`record::read_repairs`. Because records hold absolute state, a repair is
idempotent. See the [recording-failures runbook](../runbooks/recording-failures.md).

## The background service

`service::Service` runs the loops a volume needs:

- a commit loop (thread `agentfs-commit`) that commits groups that can no
  longer grow, every `agentfs.commitWindow`, backing off after a failure;
- one push loop per remote (thread `agentfs-push-<remote>`), so a slow or
  hung remote never delays another;
- the periodic repack, on the commit thread: `git repack --geometric=2`
  every `agentfs.repackInterval`, with memory bounds (`agentfs_git::git_cli::PACK_CONFIG`, which the tree
  history's repack task uses too).
  A failed round raises `Event::RepackFailed` and is tried again later.

```rust
use std::time::Duration;
use agentfs_core::push;
use agentfs_core::service::Service;

// `committer` and `events` come from volume::open; `telemetry` is the same
// one passed to it.
let settings = committer.history().settings().clone();
let pushers = push::open_all(&state, &settings, events.clone(), &telemetry)?;
let service = Service::start(committer, pushers, events)?;

// A control path can ask for a commit and a push now.
let handle = service.handle();
let report = handle.push_now(&[], Duration::from_secs(30))?;

// On shutdown: stop the loops, make the final commit, give each remote that
// lacks commits one more push, all within the grace.
let report = service.shutdown(Duration::from_secs(5));
```

`ServiceHandle` is cloneable and is what a control socket holds. Its calls:

- `push_now(remotes, timeout)` commits everything recorded (except an
  operation still running, so a restore is never published half done), wakes
  the chosen push loops past their interval or backoff, and waits up to
  `timeout`. An empty list means every remote. It fails with
  `PushNowError::Stopping` or `UnknownRemote`.
- `checkpoint(...)` has the commit loop commit what is recorded and cut a
  checkpoint (`checkpoint::cut`).

The service takes no signals. The caller decides when to call `shutdown`.
A grace of zero skips the final push. At the end of the grace each push is
cancelled: a push still negotiating sends nothing, and one still sending
over the network is left on its detached thread until the process exits. A
push to a local remote that is already updating that remote's files is
waited for, at most `service::LOCAL_UPDATE_CAP` more (10 s), because
cutting it off would leave the remote locked.

`ShutdownReport` says what the final commit and the final pushes did.

## Pushing

`push::RemotePusher::open(state, settings, remote, events, telemetry)`
makes a pusher for one configured remote. `push::open_all` makes one for
every remote that is pushed. A pusher keeps its state in
`meta/push/<remote>.json` and a lock in `meta/push/<remote>.lock`, so two
processes never push one remote at once.

| Item | Use |
|---|---|
| `push_once()` | One push attempt. Returns `PushOutcome` |
| `next_delay()` | `pushInterval` after a success, the backoff after a failure |
| `branch_claim(head)` | What the remote's branch is, against the head: `Claim::Create`, `UpToDate`, `FastForward` or `Diverged` |
| `list_refs(prefix)` | The remote's refs under a prefix |
| `push_with_lease(lease)` | An operator's lease push for a history rewrite |
| `unpushed()` | Commits the remote lacks |

A failed push, an unreachable remote and a divergence raise events and are
retried. The library never force-pushes and never rewrites history. A
diverged branch is saved under a `diverged/` ref and alarmed. See
[remotes](../remotes.md) for the refs agentfs writes and what each push
does.

`push::status` and `push::backlog` read per-remote status without opening
the volume.

## Settings

`config::Settings::load(&state.git())` reads the operator's settings from the
git directory's own config: `agentfs.*` keys, `[remote "<name>"]` sections
and `include.path` fragments. `History::settings()` returns them. A remote
section that is invalid does not stop the volume; it is listed in
`settings.invalid_remotes`, raises `Event::RemoteMisconfigured`, and only
that remote's pushes stop. `config::KEYS` lists every key. The reference is
[configuration](../configuration.md).

## Events and telemetry

The engine reports what operators need to see (refused paths, failed
records and commits, push failures, divergence, an unavailable history
service, and so on) as `events::Event` values handed to an `EventSink`:

```rust
use std::sync::Arc;
use agentfs_core::events::{Event, Events, MemorySink};

let sink = Arc::new(MemorySink::new());
let events: Events = sink.clone();
// ... run the volume with `events` ...
for event in sink.take() {
    if event.is_alarm() {
        eprintln!("alarm {}: {event:?}", event.name());
    }
}
```

`EventSink::emit` is called from the synchronous step of a record, on the
caller's threads. An implementation must never block: count, enqueue or
drop, but do not wait on I/O or on a lock held for long. A closure
`Fn(Event) + Send + Sync` is a sink. `NullSink` drops events; `MemorySink`
keeps them for tests and for callers that poll.

`telemetry::Telemetry::new(&meter, tracer, alarms)` builds OpenTelemetry
instruments over a `Meter` you supply. `Telemetry::noop()` builds nothing.
`Telemetry::sink(next)` wraps a sink so that events also become metrics and
logs. The library uses the OpenTelemetry API only. It does not start an
exporter: the `agentfs` binary does that, from the standard `OTEL_*`
variables. Open a volume once per process: each `open` registers another
backlog gauge. The signal names are in [telemetry](../telemetry.md).

The tree engine's instruments are `tree_telemetry`'s, registered on a
`Meter` the caller makes (the `agentfs` binary's, the API's no-op meter when
metrics are off, which drops them):
`tree_telemetry::observe_process(&meter, &not_built)` once per process, for
`agentfs.not_built` and the latency figures, and
`tree_telemetry::observe_unverified(&meter, counts)` once per served volume,
for `agentfs.history.unverified`, read from `counts` while it lives.

## Locks and sockets

The file locks, the shared-filesystem check and the listening sockets are
in `agentfs-os`, the operating-system leaf both engines and agentfs-csi use.

Every advisory lock agentfs takes is an `flock` through
`agentfs_os::file_lock::FileLock`. It unlocks before it closes the file. A
child process spawned by any thread shares an open file from its fork until
its exec, and a lock released only by closing would stay held for that whole
window. Nothing else in agentfs calls `flock`: `clippy.toml` disallows it.

`lock::StateLock::acquire(&state, "command", wait)` takes the
state-area lock, `meta/lock`. It is exclusive and the kernel releases it
when the holder exits, even after `kill -9`. The file also holds a one-line
description of the holder, for messages. `StateLock::holder(&state)` reads
it. `acquire` keeps trying for at least `MIN_ACQUIRE_WAIT` (250 ms) even
when you ask it not to wait.

```rust
use std::time::Duration;
use agentfs_core::lock::{LockError, StateLock};

match StateLock::acquire(&state, "my-tool", Duration::ZERO) {
    Ok(lock) => { /* you are the only writer */ drop(lock); }
    Err(e) => eprintln!("{e}"), // busy: another process holds it
}
```

`volume::open` assumes the caller holds this lock. It does not take it.

`agentfs_os::shared_fs` refuses a state area on a filesystem whose locks would not keep
other nodes out (NFS below 4.1, `nolock`, `local_lock` other than `none`).
That check runs inside `StateLock::acquire`.

Every Unix socket agentfs listens on goes through
`agentfs_os::listening_socket::Listener`, which is shut down (`SHUT_RDWR`) when stopped
or dropped. A socket that is only closed keeps taking connections for as
long as a forked child shares it before its exec, and each connection would
wait for a reply no one sends. A socket's path exists before it listens:
`bind(2)` creates the path and `listen(2)` follows, so a connect between them
is refused and a path's existence does not mean ready. agentfs-csi is ready
when `server::bind` returns: connects then queue until the socket is served.
`local_socket` binds sockets in the state
area, addresses them through `/proc/self/fd/<dir>/<name>` so a long path
never hits the 108-byte limit, and accepts only peers whose `SO_PEERCRED`
uid is root or the daemon's own (`local_socket::allowed`).

## Restore and revert

`restore` has three phases that do not depend on how the tree is reached, so
the command line and a daemon run the same code:

1. `plan(repo, branch_ref, &Request) -> Plan` works out what each path must
   become, from history alone. A restore follows `git restore
   --source=<commit> -- <path>`. A revert applies the inverse of a
   commit's diff against its first parent, with no rename detection, and
   refuses merge commits. The commit must be on the volume's branch.
2. `check(plan, repo, &dyn RecordPolicy, &dyn LfsObjects, &Dir, &dyn
   Liveness, force)` runs every refusal before anything is written and
   returns the plan without the steps the tree already matches. An LFS
   object a step needs is fetched into the cache first. `force` overrides
   only the refusals where nothing would be lost.
3. `apply(&plan, repo, objects, &Dir, &dyn Liveness, force)` re-checks
   each path just before it changes it, then writes: removals deepest first,
   then writes shallowest first. A regular file with one link is rewritten
   in place. It returns `Applied { written, removed }`, or an `ApplyError`
   that carries what was applied so far.

Recording is the caller's. `Recorder` implements `RecordPolicy`. Direct
mode (nothing mounted) records with `record_applied` under a running
operation:

```rust
use agentfs_os::confined::Dir;
use agentfs_core::restore::{self, NotLive, Pathspec, RecordMode, Request};

let tree = Dir::open(backing)?;
let history = committer.history();
let request = Request::Restore {
    spec: Pathspec::parse("notes/todo.txt")?,
    commit: "HEAD~3".into(),
};
let plan = restore::plan(history.repo(), &history.settings().branch_ref, &request)
    .and_then(|plan| restore::check(
        plan, history.repo(), &recorder, &objects, &tree, &NotLive, false))?;

let me = Caller { uid, gid, pid };
let guard = recorder.begin_operation(
    plan.kind, plan.source.to_string(), plan.summary.clone(), me, &[me.pid])?;
let applied = restore::apply(&plan, history.repo(), &objects, &tree, &NotLive, false)?;
restore::record_applied(&recorder, &tree, &applied, me, RecordMode::Chunked(4096))?;
guard.finish();
committer.commit_pending()?;
```

`objects` is any `lfs::fetch::LfsObjects`: `CacheOnly` (nothing is
downloaded), `CacheProbe`, or `Fetcher` (the cache, then the remotes).

Through a mount, the writes go through the mount and are recorded like any
workload's, so the caller implements `Liveness` to say which files a
workload holds open for writing, and that the tree records its own writes.

`RestoreError` separates refusals from failures: `is_refusal()`, `kind()`,
`hint()` and `overridable()`. Files in an fsync-class group are restored and
recorded as whole groups. The operator procedure is in the
[recovery runbook](../runbooks/recovery-and-undo.md).

## The patch queue

The patch queue lets a producer such as infrastructure as code propose
changes that agentfs applies without overwriting the workload's own
edits. The behaviour is in [patches](../patches.md). The library offers:

- `patch::submit::submit` and `approve` are the producer side. They drive the
  producer's own `git` through the `GitRunner` trait (`GitCli` is the real
  one).
- `patch::queue::Queue::new(QueueContext { .. }).process(fetch)` applies the
  queue with nothing attached: patches in inbox order, holding a patch that
  cannot land whole and going on, and stopping only at a gap, unrecorded
  changes or a failure. `QueueReport` says what landed, what was set aside
  and what is blocked. It fails only when the history or the state area
  cannot be read.
- `Queue::judge(&JudgeContext)` judges every queued patch read only, for a
  daemon that holds the volume. Nothing lands. It writes only the status ref
  and `meta/patch.json`. A patch that would land is `Verdict::Ready`.
- `patch::integrate::PatchBranch::read(repo, head, tip)` reads a patch
  branch and refuses what the queue does not land (a merge commit, a
  root commit, a gitlink, too many paths). The `Integrator` trait merges a
  patch onto the head with the `git` program (`GitMergeTree`, `GitRebase`).
- `patch::accept::AcceptanceRule` decides whether a cleanly merged patch may
  land: `CleanMerge`, `ApprovalRule` or `CommandRule` (the `patch-accept`
  hook).
- `patch::attached` records whether a workload may be attached when a
  daemon starts, so the queue waits.
- `patch::admit` and `patch::notify` serve `serve-git`'s pushers and tell
  the integrator about every verdict change.

`Verdict` is `Held`, `Waiting`, `Awaiting`, `NotJudged` or `Ready`, and
serialises as `status.json` does.

## Forks

A fork is a volume that starts from another volume's latest commit and
publishes one change for a receiver to gate. The operator's view is
[forks](../forks.md). The modules:

| Module | Offers |
|---|---|
| `fork::identity` | `ForkIdentity` (source, base, push target, link), `StartPoint`, ref names |
| `fork::create` | `create(&CreateRequest)`: makes a fork from a source's state area, without the source's lock or any credentials |
| `fork::finish` | `Finisher::open(...)`, `finish()`, `publish(deadline)`, `abandon`, `status` and `ForkState` (`Active`, `Finished`, `Published`, `Refused`, `Abandoned`) |
| `fork::listing` | `list` and `read` the published changes on a fork remote, each checked |
| `fork::deliver` | The `Delivery` contract and four methods: inbox, branch, command, webhook. `delivery(...)` builds one and validates it |
| `fork::start` | `start_from(&StartRequest)` starts a volume from a pinned commit of another worktree's branch; `accept` saves a moved start-branch tip |
| `fork::receive` | A local-only source receives its fork's refs into its own history; they are never applied |

A finished fork refuses every record (`VolumeError::ForkFinished`) and is
read-only.

## Release decision

`release::decide(&Worktree) -> Release` is the one decider of whether a
worktree may be deleted. It is pure. Its inputs are what each pushed remote
advertises for the branch (listed for this call, never read from the push
record), the journal's backlog, the class's `onDelete`, and a fork's state.
It answers `Delete`, `Keep(Kept)` or `Wait(Waiting)`. It never answers
`Delete` while a journal holds records history lacks, except for the unlinked
fork with no remote of its own, where deleting loses history on purpose. It
serves recording worktrees only: the [Kubernetes layer](kubernetes.md) runs
tree volumes, whose release is Volume's verdict at the end of a host's drain.

## Several mounts, one writer: the history service

In single-process mode one mount is the writer of the branch and holds the
state-area lock. With the history service, one process holds the lock and is
the only writer of the branch and the journal. Each pod's mount is a *front*
that freezes into its own pending slot and sends the record to the service,
which journals it and replies once it is durable. A front fails the boundary
if no durable reply comes.

Service side:

```rust
use agentfs_core::history_service::{Address, HistoryServer};

let address = Address::Unix(state.history_socket());
let server = HistoryServer::start(&address, recorder.clone(), &state, &settings, &telemetry)?;
// ...
let drained = server.shutdown(std::time::Duration::from_secs(10));
```

`HistoryServer::start` takes the local `Recorder` that `volume::open`
returned. The default socket directory is made `0700` and the socket `0600`.
It fails if another service listens there, and it refuses a recorder that is
itself a front. `listening_fronts()` returns the `Fronts` used to call live
fronts, and `fronts()` counts them.

Front side:

```rust
use std::sync::Arc;
use agentfs_core::history_service::{
    Address, ClientConfig, FrontInfo, FrontLease, HistoryClient,
};
use agentfs_core::record::Origin;
use agentfs_core::volume;

let lease = FrontLease::acquire(&state, "my front")?;     // pending slot 1..=1023
let front = FrontInfo {
    origin: Origin::from_context(&context),
    slot: lease.slot(),
    instance: rand_u64,
    pid: std::process::id(),
    version: env!("CARGO_PKG_VERSION").into(),
    views: Vec::new(),
};
let client = Arc::new(HistoryClient::connect(
    ClientConfig {
        address: Address::Unix(socket),
        timeout: std::time::Duration::from_secs(10),
        connect_wait: std::time::Duration::from_secs(30),
    },
    "demo", &settings.branch_ref, front, events.clone(), telemetry.clone(),
)?);
let recorder = volume::open_front(&state, tree, events, &telemetry, client.clone())?;
```

A front's `Recorder` has the same API as a local one. It decides the path
rules and runs the record-time checks, because only the front holds the
inode's descriptor. It then sends the draft in a `Record` request. A front
reads the settings once and runs no recovery. It takes no state-area lock.
It must hold its `FrontLease` for as long as the recorder lives.

**Protocol.** gRPC package `agentfs.history.v2`, protocol version 2
(`history_service::PROTOCOL_VERSION`), over a Unix socket on the node's own
filesystem. The contract is `crates/agentfs-core/proto/agentfs/history/v2/history.proto`.
Addresses are `unix:<path>` or an absolute path; any other scheme is
refused, since a front reaches only the service on its own node. Messages
are at most `MAX_MESSAGE` (64 MiB) each way. A front and a service of
different protocol versions refuse each other at `Hello`. A service lists
the optional calls it supports in `FEATURES` (`stamps`, `repair_origin`,
`listen`, `still_last`, `move`), so a feature added later needs no new
version, and a front never asks for one the service lacks.

The calls are `Hello` (first on every connection; it binds the front's
identity to the connection), `Tracked`, `Record`, `StillLast`, `Listen`,
and `grpc.health.v1` for `SERVICE_NAME` (`HistoryClient::ping`). The
service admits a front only when:

- the peer's uid is root or the service's own;
- it speaks the protocol version and names this volume;
- it holds the lease on the slot it names;
- the volume's identity templates render for its pod;
- fewer than `MAX_CONNECTIONS` (2048) connections are admitted.

A connection is bounded before its `Hello` too: one from a peer uid that may
not use the service is closed at accept, at most 64 wait for their `Hello` at
once, and one not admitted within 10 s is closed.

**Never resent.** A front never resends a request. A request that may have
reached the service fails with `ClientError::maybe_journaled() == true` and
its frozen copies stay for the service to commit; resending could journal
one frozen copy twice. The one exception is a request the HTTP client hands
back unsent because its connection had closed (the service restarted): it
goes out once on a new connection within the same deadline.
`ClientError` has the variants `Unreachable`, `Timeout`, `Lost`, `Refused`,
`Busy`, `Service` and `Protocol`. An outage raises
`Event::HistoryServiceUnavailable` once and
`Event::HistoryServiceRestored` at the next success.

**Pending slots.** A pending id's high 16 bits are a slot
(`pending::SLOT_SHIFT`). Slot 0 belongs to the holder of the state-area
lock. A front leases slot N with an exclusive `flock` on
`meta/fronts/slot-N.lock` for its whole life; the kernel releases it when the
front dies. `FrontLease::live`, `holders` and `leased_at` read the leases.

**Freeze stamps.** Two pods that write one path at nearly the same instant
can reach the service in the opposite order to their freezes. Each write
carries the `stamps::FreezeStamp` (device, inode, size, mtime, ctime) of its
backing file, taken just before the freeze. The service keeps the stamp of the
last write it journaled at each path for five minutes and skips a write whose
stamp has an older `ctime`. A write whose stamp is the file's current one is
never skipped.

**Calls from the service to a front.** A front opens one `Listen` stream.
`FrontHooks` is what a front implements to answer it:

```rust
pub trait FrontHooks: Send + Sync {
    fn settle(&self) -> Result<(usize, usize), String>;
    fn open_for_writing(&self, paths: &[RepoPath]) -> Vec<bool>;
    fn invalidate(&self, paths: &[RepoPath]) -> Result<(), String>;
}
```

`settle` records what is dirty and returns how many records it made and how
many paths still wait for a repair. `open_for_writing` says which paths the
front holds open for writing. `invalidate` drops the kernel's caches of the
paths a restore wrote. `NoHooks` is a front with nothing to settle.
`HistoryClient::listen(hooks)` runs the stream on its own thread and
reconnects by itself, backing off from 100 ms to 5 s. A restore through the
service calls every live front through `history_service::Fronts`
(`require_all`, `settle`, `open_for_writing`, `invalidate`) and is refused as
busy if a leased slot has no listening front.

**What fronts do not do.** They serve no control socket and apply no patch
queue. POSIX and BSD locks stay in each mount's kernel and are not shared, so
a SQLite or other lock-coordinated database must be written from one pod.

## Large files

`lfs` implements Git LFS: store selection by size and `.gitattributes`
(`policy`), the canonical pointer (`pointer`), the local object cache under
`lfs/objects` (`ObjectCache`), endpoints and credentials in git-lfs's
precedence (`endpoint`), the batch API client with concurrent, resumable
transfers (`client`, `pool`), the uploads a push needs first (`upload`),
`git-lfs-authenticate` for SSH remotes (`ssh_auth`), `fetch`, and `prune`.
The cache lock (`lfs::lock::CacheLock`) is shared by restores and fetches
and exclusive for a prune.

LFS is selected per path by the file's size and by attributes. The managed
`filter=lfs` block of the root `.gitattributes` exists in history only, never
in the tree. See [recording](../recording.md) and [remotes](../remotes.md).

## Git-backed tree views

`tree_actor::TreeActor` is a separate, smaller library: a git-backed volume
whose saves are ordered by one actor. It is not the recorder above and does
not share its state format. `TreeActor::create(root, identity, root_owner, service)` makes a
volume with its identity (`agentfs_contracts::volume::VolumeIdentity`: a name
and an id). Its Root commit records the root directory with mode 0755 and
`root_owner` (an `agentfs_contracts::clients::FileOwner`, uid and gid, parsed from
`UID:GID`), or the creating process's effective uid and gid when `None`.
A launcher passes it as `VolumeSpec.root_owner`; `ensure` uses it only when it
creates the volume and never compares it, since a found volume's root owner is
content. `TreeActor::open(root, service)` recovers one and never creates
(`Error::NotCreated`, `Error::Format` for another format). `service` is the pod
context of the process that serves it, which its own commits carry. It holds
an exclusive lease on its state area (`Error::Busy` if another process holds
it). `state_area::TreeStateArea` offers it to the Volume domain as its
`StateArea` port, whose `inspect` reads what a state area holds (`Found`)
without the lease, takes no lock and writes nothing: `TreeStateArea::at(root).inspect()`.
A read that failed is `agentfs_contracts::volume::VolumeError::Unreadable`,
naming the path and the I/O failure. Its recorded calls are `write_open`, `write`, `truncate`,
`sync`, `close`, `mkdir`, `rmdir`, `symlink`, `rename`, `unlink`, `set_meta`,
`set_times` and `replace_file`; each write-open and namespace call names its writer as an
`agentfs_contracts::pod::WriterAt`, the writer and its pod context, which the
commit carries ([trees](../trees.md#volume-identity-and-trailers)). Every call names its tree by a `TreeId`: `root`, the
tree with no upstream, or `fork/<n>`, made at journal event `n`. `trees()`
lists them, the root first and then the forks by number; `TreeId` parses from
and prints as that text. A fork is made with its row, through the store
adapter below; `read` reads a saved file whole into memory and `files` lists them;
`file_map` gives a saved file as a `FileMap` (its pieces in the journal and the
checkout as built, never bytes in memory), which a reader reads with
`FileMap::read` or `reader` with the actor unlocked and keeps across later
saves, and `read_open` gives a write-open's; `take_next` and `progress`
move queued changes between a tree and its upstream. `drain_changed` returns
the objects queue takes changed, each with its `TreeId`. A close that leaves
the bytes as they were commits nothing. `Error::errno()` maps each error to the errno a file call answers
with. The behaviour, the state format and the current limits are in
[trees](../trees.md); the FUSE adapter is in the [filesystem
layer](filesystem.md#tree-views).

A served volume (`Recording`) gates each call that starts new work in a tree:
`admit(tree)` returns at once, or `admit_then(tree, then)` runs `then` at once,
while the tree's lag and overlay are under their limits or the builder has
failed, else at the built event that brings them under
([trees](../trees.md#history-trails-the-files)). Each release of the actor's
lock (`ActorGuard`'s drop) reports every tree's overlay to the gate.
`Recording::set_overlay_limit(bytes)` sets the overlay limit (the launcher
passes `OperatorSettings::overlay_limit()`); `History::overlay_bytes(tree)`
gives a tree's overlay as last reported, in estimated bytes (0 before any
report), and `History::lag()` the lags and the builder's health.

How an open volume keeps its journal is an option of its open, `Upkeep`,
fixed for the life of the open and stored nowhere:
`TreeActor::{create_with, open_with}`, `Recording::{create_with, open_with}`
and `TreeStateArea::with_upkeep` take one, and `create`, `open` and
`TreeStateArea::at` use `Upkeep::default()`.

| Field | Default | Meaning |
|---|---|---|
| `segment_bytes` | 64 MiB | The next frame starts a new segment once the current one has a record and the frame would take it past this |
| `compact_from` | 64 MiB | The floor of compaction's due length. Unread with a gate |
| `gate` | none | A `CompactGate`: the compactor then compacts at every built event, and calls `at(gap)` at each of a compaction's five gaps (`Gap`), with no lock held. `Err` fails the compaction there |

`Upkeep::gated(gate)` is one frame per segment with that gate, for a test
that wants every built event to compact a small volume. `ChannelGate::pair()`
is a gate in a test's hands: it gives the gate, a receiver of each gap the
compactor reaches, where it stays parked, and a sender of verdicts. Dropping
both ends leaves a gate that never parks. The library reads no environment
variable for any of it.

Compaction runs on history's compactor, behind the call path
([trees](../trees.md#the-journal)), so a caller waits on its events:

| Call | Returns |
|---|---|
| `History::checkpointed()` | The last checkpoint counted (`Checkpointed`): its `generation`, 0 for the one the volume opened at and one more for each this open published, and its `built` point |
| `History::wait_checkpointed(after)` | Once a checkpoint of a generation after `after` is published. `WaitError::Stopped` at shutdown or once a compaction stopped the actor |
| `History::wait_compacted(generation)` | Once the compaction that published that generation has ended: the segments it covers are unlinked, or its unlink failed and was logged. The publish comes before the unlink |
| `History::compactor_through()` | The built point of the last built event the compactor is done with, compacted or not due |

Read the generation, act, then wait past it. Nothing waits on a length or a
timer.

Saves history could not check ([trees](../trees.md#when-history-cannot-check-a-save))
are flagged in their commits' `Agentfs-Unverified` trailer.
`TreeActor::verification()` gives the volume's `Verification` handle, whose
`counts()` (`UnverifiedCounts`: `unresolved`, `mismatch_confirmed`) never
takes the actor's lock; a launcher hands it to
`tree_telemetry::observe_unverified` for the `agentfs.history.unverified` gauge.
`tree_actor::verify_flagged(root, by, release)` is `agentfs verify`: it takes
the state area's lock (`Error::Busy`), opens only its `history.git`, and
checks every standing flag with `release` (`tree_actor::release()`, this
binary as `agentfs <version> build <hash>`), answering a `VerifyReport` of
each `Checked` flag (`Outcome::Wrote(Verdict)`, `SameRelease` or
`NoEvidence`), the commits whose verdict note does not parse, and how many
flags stand. The notes refs are `tree_actor::VERIFY_REF` and
`tree_actor::EVIDENCE_REF`; `tree_git::TreeGit` reads and writes git notes in
git's layout (`notes`, `note`, `note_tip`, `write_note`, the last a
compare-and-swap of the notes ref).

### Operator settings

`operator_settings` reads and writes a tree volume's operator settings:
`<state>/config` (`operator_settings::FILE`), in git-config syntax, holding the
`include.path` lines a launcher writes (`agentfs init --include`). It is the
launcher's file, apart from `history.git/config`, which the engine rewrites and
its `git` children read.

- `write_includes(state, includes)` makes `includes` exactly the file's
  `include.path` lines. The file is written whole (a temporary file, fsync,
  rename, a directory fsync) with mode 0600. Each include must be absolute
  (`SettingsError::RelativeInclude`, nothing written).
- `OperatorSettings::read(state)` reads the file with libgit2, which follows its
  includes. A missing file is empty settings; a file or include that cannot be
  read is `SettingsError::Read`. `includes()` gives the top-level includes as
  written, and `missing_includes()` those that do not exist: git skips them
  silently, so the launcher warns.
- `not_built()` lists every configured key whose capability tree volumes do not
  build yet, one per key, sorted. Each is a `NotBuilt { key, capability }`.
  Nothing configured is silently ignored.
- `overlay_limit()` gives `agentfs.overlayLimit`, the last value set, as a
  positive size, or `DEFAULT_OVERLAY_LIMIT` (32 MiB) unset; any other value is
  `SettingsError::BadValue { key, value, expected }`.

`classify(key, value)` is the one table. It compares the key lower-cased, and
the first match wins:

| Key | Capability |
|---|---|
| `include.path`, `includeIf.<condition>.path` | none: structure |
| `agentfs.nested` = `exclude` (any case) | none: tree volumes build it |
| `agentfs.overlayLimit` | none: tree volumes build it |
| `agentfs.lfsThreshold`, `lfs.*`, `remote.<name>.lfsurl`, `remote.<name>.lfspushurl`, `remote.<name>.agentfsLfsCredentials`, `remote.<name>.agentfsMaxLfsObjectSize` | `Lfs` |
| `agentfs.patch*`, `agentfsPusher.*` | `Patches` |
| `remote.*`, `agentfs.pushInterval`, `agentfs.forkRefs`, `http.proxy` | `Push` |
| anything else | `Other` |

`Capability::as_str` names each (`push`, `patches`, `lfs`, `other`) and
`Capability::meaning` says in one sentence what the volume does without it, for
the boot log. `tree_telemetry::observe_process(&meter, &not_built)` registers the
`agentfs.not_built` gauge: per capability, the number of settings configured
and not built ([telemetry](../telemetry.md)).

### Directories, metadata, links and times

Paths nest: `a/b/f` is file `f` in directory `a/b`, and `""` is the root
directory, wherever a call reads an entry (`node`, `children`, `set_meta`,
`set_times`). Each component is at most 255 bytes (`Error::NameTooLong`).

```rust
use agentfs_contracts::clients::FileOwner;
use agentfs_core::tree_actor::{Attrs, Perm, RenameMode, SetTime, TreeId};

let me = FileOwner { uid: 1000, gid: 1000 };
let exec = Attrs { perm: Perm::from_mode(0o755), owner: me };
v.mkdir(TreeId::Root, "bin", exec, &by)?;
let h = v.write_open(TreeId::Root, "bin/run.sh", &by, Some(exec))?; // created 0755
v.write(h, 0, b"#!/bin/sh\necho hi\n", false)?;
v.close(h)?;
v.rename(TreeId::Root, "bin", "tools", RenameMode::Replace, &by)?;      // everything under it moves
v.symlink(TreeId::Root, "run", "tools/run.sh", me, &by)?;
v.set_times(TreeId::Root, "run", None, Some(SetTime::At(0)), &by)?;  // no commit
for entry in v.children(TreeId::Root, "", 0, 100)? { /* name, number, kind */ }
```

- `write_open(tree, path, by, create)` creates a missing file when `create`
  gives its `Attrs` (permission bits and owner); `anonymous_open(tree, by,
  attrs)` is `O_TMPFILE`. `Attrs` is the part of an entry's `Meta` a caller
  chooses: the call gives the kind. In a set-group-ID directory a new entry
  takes the directory's group, and a new directory keeps the bit (Linux's
  `inode_init_owner`); an `O_TMPFILE` file does not (see
  [Limits](../trees.md#limits)).
- `mkdir` returns the directory's node number, `rmdir` removes an empty one
  (`Error::NotEmpty`), `symlink(tree, path, target, owner, by)` makes a link
  (mode `0777`, its bytes its target: 1 to 4095 bytes, no NUL) and returns its
  object, and `readlink` reads a link's target (`Error::Invalid` for anything
  else).
- `rename(tree, from, to, mode, by)` follows POSIX: a directory onto a file is
  `NotADirectory`, a file onto a directory `IsADirectory`, onto a directory
  with entries `NotEmpty`, into its own subtree `Invalid`, and
  `RenameMode::NoReplace` onto an entry `Exists`; onto an empty directory it
  replaces it, onto itself it changes nothing. A directory moves with every
  entry under it, each file keeping its object and identity; an open of one
  follows its object.
- `set_meta(tree, path, attrs, by)` is chmod and chown: the entry's kind kept,
  a step and a commit (`Meta by <writer>`) unless nothing changes.
  `set_times(tree, path, atime, mtime, by)` sets times (`SetTime::Now` is the
  step's time); it is a step that makes no commit.
- `node(tree, path)` gives a `Node`: its node number (`None` for the root; a
  view's inode is the number + 2), `Meta` (kind, mode, uid, gid), `Times`,
  length and link count. `node_at(tree, number)` finds a named entry by its
  number, with its path, and `open_node(tree, object)` a file no name reaches
  (an anonymous open's, or one unlinked while open). `children(tree, dir,
  skip, count)` lists one batch of a directory (`DirEntry`: name, number,
  kind).
- Times are each tree's own: a step's record time gives the mtime and ctime of
  what it changes (a save the file's, an entry added, removed or renamed its
  directories'), and history never holds them.

### Landing directories, metadata and links

A fork's change lands in its upstream, and a sync brings the upstream's
decisions to the fork, by the same rules as lines and names (rules 13 to 15 of
the model, `crates/agentfs-model/MODEL.md`). The referee judges them with the
change's first parent (what its writer saw), the change, and the upstream's
head:

- **Metadata.** A file's or a directory's kind, mode, uid and gid are one
  value; whether the entry is there is part of it. A change's new value lands
  when the upstream still holds the value the change's first parent holds, or
  already holds the same new value. Otherwise the upstream's value wins and
  the change loses whole, with cause `metadata`. A value is compared, not its
  history: an upstream that went from the seen value to another and back
  counts as unchanged, so the change lands over it. A file's metadata follows its
  identity, so a rename and a chmod of one file both apply, and an atomic save
  carries the new file's metadata. On a view, `chmod(2)` and `chown(2)` are
  two steps and two changes: a program that runs both (`install -m -o`, say)
  in two trees can collide on the value between them, with cause `metadata`,
  where one change of both would agree.
- **Directories.** A directory is its path: two trees making one directory
  with one value agree, and an empty directory is kept. A directory's rename
  is one change: every entry under it moves with it, each file keeping its
  identity, and the change lands or loses whole. A change loses whole, with
  cause `directory-not-empty`, when it removes or moves a directory under
  which the upstream has an entry its first parent lacks, moves a directory
  onto a path under which the upstream has such an entry, or puts an entry
  onto a path where the upstream has an unseen entry of the other kind (a
  file where a directory is). It loses whole, with cause `directory-gone`,
  when it puts an entry into a directory its first parent has and the
  upstream no longer has.
- **Links.** A link is a file whose bytes are its target and whose kind is
  `symlink`: its target follows the line rules and its kind the metadata
  rule. Of two concurrent retargets the upstream's wins; replacing a file by a
  link keeps the file's identity and changes its kind.
- **Syncs.** A sync takes the upstream's metadata and directories, except the
  fork's own values the upstream's queue has not yet decided (their commit is
  not in the upstream's history), and every directory one of the fork's
  entries still needs, until the upstream decides. So every entry's parent is
  a directory of its tree at every step, and a decided upstream `rmdir` or
  directory rename reaches the fork. One case the rules do not decide: an upstream
  entry the fork never had, under a directory the fork's undecided change
  removed or moved. A sync that would show the fork that case fails with
  `Error::Unruled`, logged as a failed take, and stays queued: it goes through
  once the upstream decides the fork's change, and until then the fork shows
  its own tree. A sync that delivers the upstream's decision with it passes
  through.

A losing change keeps its collision ref, as for lines and names. Its
landing commit's `Agentfs-Collisions` line lists each rule 13 or 14 loss as
`{"cause":"metadata"|"directory-not-empty"|"directory-gone","path":"<path>"}`,
the path a file's path or a directory's with a trailing `/`, before the
change's other versions, which lose `Whole`. A take's read keys include every
directory these rules judged and every path an entry of the change arrives
at: a step that makes, moves or removes an entry under such a directory after
the decision refuses the take, which is decided again.

### Rules and the untracked area

The Rules domain is its own pure crate, `agentfs-rules`: no I/O, built once.
`RuleSet::build(&RuleTexts { exclude, attributes }, Nested::Exclude)` takes the
bytes of `info/exclude` and `info/attributes` and refuses a rule tree volumes do
not build (`RulesError::NotBuilt` for an `agentfs.<name>` attribute or
`filter=lfs`, `RulesError::BadValue` for `agentfs=<value>`, each with its line).
`place(path, kind, PathFacts { parent_untracked, inside_repository })` answers
`Placement::Tracked` or `Placement::Untracked(reason)` for a new name, the
built-ins first; `never_records(path)` says whether `-agentfs` marks it.
`OperatorRules` is the operator layer alone: `ignored(path, is_dir)` gives the
`info/exclude` pattern that excludes a new path and `attributes(path)` what
`info/attributes` sets ([trees](../trees.md#rules-and-the-untracked-area)).

The tree actor reads `<state>/history.git/info/exclude` and
`info/attributes` when it opens, before anything is written; a rule not built
fails the open with `Error::Rules` (`EINVAL`). `TreeActor::rules()`,
`Recording::rules()` and `TreeSession::rules()` give the rule set. Every call
that makes a name (`write_open` with a create, `mkdir`, `symlink`,
`first_link`, and `rename` onto a name with no entry) refuses one the rules
place untracked with `Error::Untracked { path, reason }` (`EXDEV`), before
anything is journaled; a name that is not git-safe is refused first, as
`Error::Name`. Rules decide only for new names: a path with an entry stays
recorded, except that an open moves each tracked file or link `-agentfs` now
marks into its tree's untracked area and records its unlink as writer `agentfs`
(placed first, so a crash between the two places it again at the next open).

`replace_file(tree, path, attrs, src, len, by)` (and `SessionGuard::replace_file`)
is one step that leaves `path` holding exactly the `len` bytes read from `src`,
with `attrs`' mode and owner: the atomic save a program makes by renaming a
finished file over `path`.
An existing file keeps its object and identity, and its unchanged lines keep
theirs (its lines are aligned as a close's save aligns them); a missing file is
created as a create would make it, refused with `Error::Untracked` where the
rules place it untracked. The bytes go to the journal in chunk records just
before the step, so an error from `src` (`Error::Io`, `UnexpectedEof` when it
ends early) or a crash before the step leaves `path` as it was. The commit is a
direct save's, `Direct by <writer>` with no `Agentfs-Open`, queued for the
upstream as every save is; the same bytes and metadata answer `Saved::Unchanged`
and commit nothing. A directory at `path` answers `Error::IsADirectory`, a link
`Error::Invalid`, a missing parent `Error::NotFound`. Opens of the file keep
their own copies and save over it at their close.

`TreeActor::untracked_area(tree)`, `Recording::untracked_area(tree)` and a
view's `TreeSession::untracked_area()` give a tree's `UntrackedArea`,
`<state>/untracked/<tree>/`, made on first use (`path()` names it). It is
reached only beneath its directory: `stat(path)` is an `lstat`, `entries(dir)`
lists a directory and `sync_dir(path)` fsyncs one. Every change goes through
`writing()`, which holds the area against a fork's copy and gives an
`AreaWrite`: `parent(path)` opens a path's parent for an `*at` call,
`containers(path, meta_of)` makes the directories above a path with the modes
and owners of the tracked directories they stand for, and `place(path, src,
len, meta)` puts a file or link in place atomically (written under
`<state>/untracked/.staging/`, fsynced, renamed). A caller holding it never
takes the actor's lock. `fork_with` copies the upstream's area as the fork's before the
fork's step, holding the upstream's changes while it copies; `drop_fork`
removes a dropped fork's area once its drop is recorded, and an open removes
every copy no fork owns.

### Sessions

`session::Sessions::new(recording)` holds the history service's sessions of a
served volume and is Clients' `SessionBinder`: `open()` gives a new
`SessionId`, unique per run; Clients binds a client's authority to it (a
`ClientCap` for a read-write client, its tree for a read-only one). A view
reaches its tree only through `sessions.session(id)`, a `TreeSession`, whose
`lock()` takes the actor's lock and reads the binding under it. The
`SessionGuard` it gives has `TreeActor`'s file calls with the tree and writer
left out: a write answers `Error::ReadOnly` (`EROFS`) without a writer, and a
handle call answers `NotFound` unless the open is this tree's and this
writer's. `admit_then`, `durable_then` and `durable_all_then` run unlocked.
`flush` answers a FLUSH at once, with no wait for the disk: `Error::Stopped`
once a journal fsync has failed, never that fsync's own error. `durable_all`
blocks the caller until every record so far is durable: what a view waits for
before a change outside the journal destroys bytes a record carries.

A RELEASE goes in two halves ([pending saves](../trees.md#pending-saves)).
`TreeSession::start_close(handle)` begins the close under the lock and gives a
`tree_actor::Closing`: `Saved(saved)` when it saved at once, or
`Decide(SaveToDecide)`, a pending save. `SaveToDecide::finish()` decides it with
the lock released and lands it under the lock, after any earlier-begun save of
the same file; dropped unfinished, a panic included, it stays pending as a failed
close (counted in `failed_saves`). `close(handle)` and `Recording::close_with` are
both halves on the caller's thread, for the CLI, tests and recovery;
`Recording::start_close_with(handle, check)` is the first half with a check run
under the same hold, which `TreeSession::start_close` uses.
`SessionGuard::open_map(path)` is what an open reads: the file as this client's
pending saves leave it (their own calls and the page-write pieces their saves
take, replayed on the saved map in close order), else the saved map; never
another open's unsaved calls, and it never waits for a save. **`open_map` and
`write_open` may release the guard's lock inside them and take it again**: when
the client's pending saves need a fold, the guard's hold takes a snapshot and
the fold runs with the lock released, so other views go on meanwhile. Anything
read through the guard before the call may have moved when it returns: read
what a reply names from the open (`open_object`), not the path again. The
content a read-only open gets is as of its snapshot; a write-open is recorded in
the snapshot's hold and gets its view in a second hold, with the file's page
writes made meanwhile on it. A failed fold closes a write-open, as a client's
end closes an open (saved after the pending saves it would have folded, not a
failed save), and is its answer; `Error::NotFound` when the client ended
meanwhile; `Error::Stopped` when
the actor stopped. `TreeActor::write_open`, the library's one-hold call for the
CLI and tests, folds under the lock. `unlink`, `rename` (onto `to`),
`replace_file`, `truncate_path` and `set_times` answer `tree_actor::PathStep`: `Done(value)`,
or `WaitFor(SavePending)` with nothing done while the file they would delete or
replace has a pending save. `SavePending` is made only by the core and must be
used: hand it and a retry of the whole call to `when_saved_then(pending, then)`,
which runs `then` once no save of that file begun before the refusal is pending
(at once when none is, else on the thread that lands the last of them, with the
lock released; `Error::Stopped` when the actor stops first);
`Recording::when_saved_then` is the same without a session. `SavePending::before()`
is the journal's next record at the refusal. The retry takes its guard with
`TreeSession::lock_retrying(Some(pending))`, which refuses it again only for a save
begun before that refusal, so a save begun later lands after the call and a client
that keeps closing the file never keeps it waiting. A rename of the pending
file itself goes first. `pending_save(path)` names the file's pending save, if
any, for a caller that reads the file to put it elsewhere (a move out of the
tree). Once the actor stops, a `finish` queued behind another save of its file
and a `settle` wake and answer `Error::Stopped`. `Recording::settle` waits until
no save begun before it is pending (a close whose close record is below the
journal's next record when it starts; one begun after it, of any file, does not
hold it), then until history is built through every record by then; it is never called from a
thread that finishes saves. `TreeActor::metrics()` counts them in
`pending_saves`, `oldest_pending_save_ns` and `failed_saves`
([telemetry](../telemetry.md#pending-saves)).

`unbind` removes the binding and, under the same lock,
`TreeActor::close_writer` closes every open of the client's writer as a save,
in handle order, with no wait for the disk: each gets its close record first,
all before any save, so the end's order survives a restart; an open of a file
with a save in flight begun before the end is saved when that save lands, after
it. A failure (of an append or a save) is logged and an unsaved close stays
pending, saved at its tree's next step. Lock order: the actor, then the bindings.

### Rows and domain events

The domains (`agentfs-volume`, `agentfs-clients`, `agentfs-trees`) keep their
records as rows on the volume's one tree actor, through the ports in
`agentfs_contracts::ports`. `store::TreeStore::new(recording)` implements
them over a served volume (`Arc<Recording>`): Trees' `ForkStore` (`load`,
`create`, `end`, `drop_fork`), Clients' `ClientStore`, Volume's `VolumeStore` and `QueueFacts`. Each write is one journal step, the same commit
point as a save: a fork's branch and its row are one record, or a row step
holds rows alone. The store makes it durable (one group-commit fsync, outside
the actor's lock) before it returns. Each row is the domain's value with its
audit stamps (`agentfs_contracts::audit::Audited`): the store sets `created`
at a row's first write and keeps it, and sets `changed` at every write.

The actor names the chains but not their meaning: a row names its chain
(`tree_actor::Chain`: `Volume`, `Trees(fork)` or `Clients(tree)`), whose ref
form the actor checks, and its event (`RowEvent`: a kind such as
`ForkCreated`, a detail, the principal as `Agentfs-By` text, and its pod
context); what a row holds is the store's. `TreeActor::row(chain)` and
`rows(domain)` read the rows from
the actor's saved state, never Git, so they answer while history lags or the
builder fails. `TreeActor::fork_with`, `record_rows` and `drop_fork` run the
steps; only the store calls them (a clippy rule). `drop_fork(fork, rows)` is one
step that deletes the fork's branch and its queue's entries with its rows, and
forgets the tree at once; it answers `Busy` (the store's `Refused`) while its
upstream's queue holds an entry it queued, a file is open in it, or a fork names
it, so a drop never needs history. It is the one judge of whether a fork may
drop. An event carries its caller's pod context; an event by `agentfs` (a drop,
a claim's release, an end or a release the domains judge) carries
`service_at()`, the history service's, filled in by the store. The queue facts
are the journal's too: `queued()` never waits for history, and a stopped actor
answers `Stopped`, never an empty queue.

Clients' `ClientStore` (`load`, `attach`, `rebind`, `end`) keeps one row per
tree on `Chain::Clients(tree)`: its attached clients (`ClientRow`: the id, the
grant and the pod context) and a fork's writer claim, each entry stamped. An
attach's step number is the client's ordinal. One `end` step removes every
ended client with its `AttachmentEnded`, then writes `LastDetached` for each
tree left empty. `release_claim` removes a dropped fork's writer claim with
`ClaimReleased` (nothing to release writes nothing).

`TreesService` (`agentfs-trees`) answers a second keyed `create` with its open
fork and ends forks (`end`, `end_keyed`). `settle(clients)` is its one
judgement from facts asked now: every open unkeyed fork no client is attached
to (asked of Clients' `attached_any`) ends, and every ended fork the store lets
go drops. It runs at boot after Clients' `reconcile`, at each `LastDetached`
and after each take, so an end a crash or a failed step left undone is judged
again. `ClientsService::on_fork_dropped` releases the claim, and `reconcile`
(at boot and at the drain) also releases the claim of every fork Trees reports
dropped. `detach` is idempotent within a run: rows are removed at an end, so a
client ended in an earlier run answers `UnknownClient`. The composition root
dispatches the events between them ([trees](../trees.md#forks-keys-end-and-the-drop)).

Volume's `VolumeStore` (`load`, `record`) keeps the lifecycle row on
`Chain::Volume` (`refs/agentfs/volume`): one row step per move, `VolumeStaged`,
`VolumeDraining` (detail `Stop`) or `VolumeReleased` (detail `Keep`).
`TreeStateArea` is Volume's `StateArea`, `VolumeStore` and `QueueFacts` in one:
`open` recovers the volume and makes a `TreeStore` over it, to which the other
two delegate; before `open` the row, a write and the queues answer `Stopped`:
never a default nobody read. `VolumeService` (`agentfs-volume`) stages the volume
(`stage` opens the state area), unstages it, and on each `rejudge` while it
drains records the release once no client is attached and no queue holds an
entry; `ClientsService::on_volume_draining` and `on_volume_staged` refuse and
allow attaches meanwhile ([trees](../trees.md#volume-lifecycle)). Its
`release_verdict` judges a row or a queue it cannot read as `Staged`: never a
release on facts nobody read.

`StoreError` from a tree actor failure: `Stopped` and an unknown fsync
outcome are `Stopped` (the reopen decides whether the step survived); a failed
append is `NotDurable` (nothing recorded); damaged state is `Damaged`; a step
the actor refused (a drop while anything holds the fork, a row or upstream
that is not there) is `Refused`. Each domain's error has one `From<StoreError>`:
`Stopped` for a stopped or damaged store, its own word for a refusal
(`ClientError::UnknownClient`, `TreeError::ForkUnknown`), and `NotDurable` or
`Io` for a write that changed nothing. One function maps a tree actor failure
to its `IoKind` for every adapter (`store::io_kind`). A domain publishes its
event to its `EventSink` once the store returns; the composition root's queue
hands each on in order. Each event is on record as a commit on its chain
([trees](../trees.md#rows-and-event-commits)).

### Domain invariants

The Volume, Clients and Trees domains hold these, and the code cites them by
their labels. What the domains do not do yet is on their design pages
([target architecture](../target-architecture/README.md)).

| # | Invariant | Enforced by |
|---|---|---|
| C1 | Only Clients mints a `ClientCap`, so only Clients makes a client writer | private fields; `ClientCap::mint` takes the composition root's `Minter`; clippy bans `Minter::new` outside it |
| C2 | A `ForksItCreates` launcher reaches only the forks it created, never the root | `ClientsService` checks the launcher against Trees' `facts(fork)` before any row |
| C3 | A read-only client has no `ClientCap`: its session writes nothing | the session's authority is `Read` for a read-only grant |
| C4 | A fork has at most one read-write client, for life | the writer claim in the fork's clients row, kept after its client ends and released only at `ForkDropped` |
| C5 | A session binds one client, and the client acts only through it: its calls name no tree and no writer | `agentfs_core::session` owns the `ClientCap` |
| C6 | `LastDetached{tree}` is published exactly once each time a tree's last client ends | the end and its events are one row step |
| C7 | Every end has an event (detach, reconcile, a fork's end); no lease or heartbeat ends an attachment | no clock in the domains |
| VO1 | `Released` only when the volume is not staged, no client is attached and every queue is empty | only the pure `release::decide` builds a `Cleared` |
| VO3 | A move and its event are one unit | one row step, its event commit built from it |
| VO4 | The release verdict reads only facts asked at judgement time; nothing polls | the verdict is pure over `ReleaseFacts`; no clock in the domain |
| VO5 | A volume's id never changes, and an `ensure` with another name at the same location writes nothing | `decide_ensure` answers `SpecMismatch` before any write |
| VO6 | A fork launcher cannot `ensure`, stage or unstage the volume | `LaunchCap` with `may: Root`, else `NotPermitted` |

## Errors

Errors are `thiserror` enums, one per area; none is a catch-all.

| Type | Raised by |
|---|---|
| `volume::VolumeError` | `init`, `open`, `Recorder::record`. Wraps `HistoryError`, `io::Error`, `PathError`, `RulesError` and a front's `ClientError`; also `OtherVolume`, `OtherBranch`, `OtherFork`, `OtherStart` (a state area that belongs to something else is left unchanged), `NoWorktree`, `ForkFinished` |
| `history::HistoryError` | The history repository |
| `lock::LockError` | `StateLock::acquire` |
| `history_service::ClientError` | A front's calls |
| `restore::RestoreError`, `ApplyError` | Restore and revert |
| `patch::PatchError` | The patch queue |
| `fork::finish::FinishError`, `create::CreateError`, `start::StartError` | Forks |
| `tree_actor::Error` | Tree views |

## What this layer does not do

- It does not mount anything or speak FUSE. That is the [filesystem
  layer](filesystem.md).
- It has no command line, takes no signals and starts no OpenTelemetry
  exporter. The `agentfs` binary does those.
- `volume::open` does not take the state-area lock. The caller does, so a
  caller that opens a volume without the lock is a second writer.
- It does not rewrite history or force-push by itself. History rewrites are
  manual operator procedures ([history-rewrite](../runbooks/history-rewrite.md)).
- It does not land patches while a workload runs. The patch queue applies
  at mount with nothing attached. `Queue::judge` reads only.
- It does not record ownership, timestamps, extended attributes or any
  permission bit but the executable bit; git cannot represent them.
- It does not hide the state area from a workload. A front end must keep the
  state area outside the tree it serves; the filesystem layer's preflight
  checks that.
- Its public types may change between versions. The history service protocol
  is versioned (`PROTOCOL_VERSION`); the Rust API is not.
