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
| OpenTelemetry signals | `telemetry` | `Telemetry`, `Telemetry::sink` |
| Exclusive use of a state area | `lock`, `file_lock` | `StateLock`, `FileLock` |
| Settings from the git config | `config` | `Settings`, `Settings::load`, `KEYS` |
| Path rules and checks | `rules`, `checks`, `paths` | `Rules`, `Checks`, `RepoPath` |
| Restore and revert | `restore`, `operations` | `plan`, `check`, `apply`, `record_applied` |
| Patch queue | `patch` | `Queue`, `Verdict`, `PatchBranch`, `submit` |
| Forks and start points | `fork` | `create`, `Finisher`, `start_from`, `delivery` |
| Release decision for a worktree | `release` | `decide`, `Release` |
| Several mounts, one writer | `history_service` | `HistoryServer`, `HistoryClient`, `FrontLease` |
| Large files | `lfs` | `ObjectCache`, `Store`, `fetch`, `prune` |
| Git-backed tree views | `tree_actor` | `Volume`, `TreeId` (a tree's identity, `root` or `fork/<n>`) |
| Line identity across saves | `tree_reconcile` | `LineMap`, `Line`, `LineMap::align` (git's histogram diff), `LineMap::append`: the line-map library, generic over the caller's version (`Id`) |

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
use agentfs_core::paths::RepoPath;
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
  every `agentfs.repackInterval`, with memory bounds (`repack::REPACK_ARGS`).
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

## Locks and sockets

Every advisory lock agentfs takes is an `flock` through `file_lock::FileLock`.
It unlocks before it closes the file. A child process spawned by any thread
shares an open file from its fork until its exec, and a lock released only
by closing would stay held for that whole window. Nothing else in the
crate calls `flock`.

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

`shared_fs` refuses a state area on a filesystem whose locks would not keep
other nodes out (NFS below 4.1, `nolock`, `local_lock` other than `none`).
That check runs inside `StateLock::acquire`.

Every Unix socket agentfs listens on goes through
`listening_socket::Listener`, which is shut down (`SHUT_RDWR`) when stopped
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
use agentfs_core::confined::Dir;
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
fork with no remote of its own, where deleting loses history on purpose. The
[Kubernetes layer](kubernetes.md) calls it from DeleteVolume.

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

`tree_actor::Volume` is a separate, smaller library: a git-backed volume
whose saves are ordered by one actor. It is not the recorder above and does
not share its state format. It holds an exclusive lease on its state area
(`Volume::open(root)` fails with `Error::Busy` if another process holds it).
Its recorded calls are `write_open`, `write`, `truncate`, `sync`, `close`,
`rename` and `unlink`. Every call names its tree by a `TreeId`: `root`, the
tree with no upstream, or `fork/<n>`, made at journal event `n`. `trees()`
lists them, the root first and then the forks by number; `TreeId` parses from
and prints as that text. `fork()` makes a fork of the root and returns its
`TreeId`, `read` and `files` read saved bytes, and `take_next` and `progress`
move queued changes between a tree and its upstream. `drain_changed` returns
the objects queue takes changed, each with its `TreeId`. A close that leaves
the bytes as they were commits nothing. `Error::errno()` maps each error to the errno a file call answers
with. The behaviour, the state format and the current limits (flat files
only) are in [trees](../trees.md); the FUSE adapter is in the [filesystem
layer](filesystem.md#tree-views).

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
