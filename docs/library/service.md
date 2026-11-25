# The library: `agentfs-service`

`agentfs-service` hands a consumer a working tree volume. It wires the
Volume, Trees and Clients domains ([the shared kernel and the
domains](domains.md)) onto the tree engine's store as one launcher, and hands
each domain event to its consumers through one queue. At a stop it drains
the volume until Volume releases it. It is the one place a volume is
composed: the `agentfs` binary's `init`, `mount`, `host` and `fork end` use
it ([the filesystem](filesystem.md)), and a consumer reaches it as
`agentfs::service` ([the library](agentfs.md)). `crates/agentfs-repo`
checks that only it, the crate `agentfs` and the tests depend on the domains
it composes (`only_the_service_composes_the_domains`).

It mints nothing. Its caller's composition root mints the capabilities, as
the binary's `main` does, and hands them in.

## The launcher

A `Launcher` is who opens a volume. Its fields are private:

- `Launcher::new(minter, cap, at)` takes the caller's `Minter`, which only
  Clients uses to mint each read-write client's `ClientCap` (C1 in the
  [domain invariants](domains.md#domain-invariants)). It also takes the
  `LaunchCap` every call is made as, and the `PodContext` every call is
  made at. The volume keeps its journal with `Upkeep::default()`.
- `with_upkeep(upkeep)` sets how the volume keeps its journal
  ([the engine](engine.md#git-backed-tree-views)).

The caller mints the capability with `LaunchCap::mint(&minter, id,
LaunchScope::Root)`. Volume refuses a fork launcher (VO6).

## The calls

- `init(state, spec, &launcher, includes)` runs Volume's `ensure` as the
  launcher: it creates the volume, or finds it (VO5). Then it makes the
  recovery every start makes. When `includes` is not empty, it writes them
  as the operator settings' includes ([the
  engine](engine.md#operator-settings)) under that recovery's lock. It
  answers the `Ensured` id and outcome, and holds no lock once it returns.
- `end_fork(state, launcher, &key)` restores Trees and Clients with no
  Volume, so it judges no release. Then it ends the keyed fork under `key`.
  The fork drops now if nothing holds it, else at a later `stage`'s
  settle. It holds the state area's lock while it runs.
- `Domains::stage(state, launcher, &settings)` stages the volume through
  Volume. It recovers the volume and never creates one. It sets the
  overlay limit from `settings` before any view opens, then restores Trees
  and Clients on one store. It holds the state area's lock until the
  `Domains` is dropped. `settings` is `OperatorSettings::read(state)`, the
  one value that type has: read it once, check it, and stage with it.
  Nothing ties it to `state`, so a caller that passes another volume's
  settings gets that volume's overlay limit.

A `Domains` is not `Send`: its services and their queue are shared on one
thread. Its calls:

| Call | What it does |
|---|---|
| `sessions()` | The `Sessions` a bound client's view reaches its tree through ([sessions](engine.md#sessions)) |
| `bind(tree, label, access, at)` | One view's client on a new session, at `at`: the restored client of this launcher, tree, label and access rebound, else a new one attached, live and consistent per file. Answers the `Binding` or the `ClientError` |
| `detach_all(ids)` | Detaches each client, its opens saved first |
| `reconcile(live)` | Ends this launcher's clients not in `live` and releases dropped forks' claims; answers how many it ended |
| `settle()` | Ends every unkeyed fork no client is attached to, drops every ended fork nothing holds, and re-judges the release: at boot and after each landing |
| `fork(upstream, key, settings)` | A fork of `upstream`: the open one under `key`, or a new one. Its branch and its row are one step, durable when it returns |
| `end_keyed(key)` | Ends the keyed fork under `key`; it drops once nothing holds it |
| `keyed(key)` | The fork under `key` and its phase, whatever the phase; never makes one |
| `unstage(views)` | The drain begins (below); answers `Unstaged` |
| `released()` | Whether Volume has released the volume |
| `stopped()` | Whether the tree actor has stopped after a recording failure |

## Errors

`ServiceError` holds what the caller decides on:

- `Volume`: Volume's refusal or failure. A busy state area is always
  `Volume(VolumeError::Busy)`, from every call, because every call opens
  through the engine's state area.
- `Settings`: `init`'s includes not written, or `stage`'s overlay limit
  refused.
- `Forks` and `Clients`: the forks' or the clients' rows not read.
- `Fork`: `end_fork`'s refusal, such as `TreeError::KeyUnknown`.

`bind`, `fork` and `end_keyed` answer their domain's error. A staged volume
is always served: that is an invariant, not an error.

The service reports a failure its caller cannot act on as the tree library
reports one: a stderr line `tree <Name>: ...`. The next boot judges again
from asked facts. The names are `EventFailed` (an event's consumer failed),
`UnstageFailed` (the volume stays staged, and the next stop asks again),
`ReconcileFailed`, `SettleFailed` and `DetachFailed` (the next boot's
reconcile ends the client).

## Dispatch

Each domain publishes its event once its step is durable. After the call
that caused it returns, the service hands each event, in order, to its
consumers:

- `LastDetached` settles Trees (an unkeyed fork no client is attached to
  ends), then re-judges the release.
- `AttachmentEnded` re-judges the release.
- `ForkDropped` releases the fork's writer claim in Clients.
- `VolumeDraining` stops attaches and begins the drain; `VolumeStaged`
  allows them again.
- `VolumeReleased` makes `released()` true.

## The drain

`unstage(views)` asks Volume to unstage. If that begins the drain:

1. `views`, the clients of the caller's views, are detached, their opens
   saved.
2. This launcher's other clients end: the release is judged from the
   Clients index alone.
3. Other launchers' attached clients are logged as `tree DrainBlocked`. The
   release waits for them, and only their own launcher ends them
   ([recovery](../runbooks/csi-recovery.md)).
4. `Unstaged::Draining` is answered, and the caller closes every view it
   serves.

When Volume releases the volume at once, nothing was attached, so steps 1
to 3 are skipped. A second `unstage`, or one that failed, answers
`Unstaged::Unchanged`. The answer is `#[must_use]`.

## Costs

- One thread: a `Domains` is not `Send`.
- `init` and then `stage` recover the volume twice. The second recovery
  replays nothing new.
- `stage` reads no file: the caller reads the operator settings.
