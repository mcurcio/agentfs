# The library: the shared kernel and the domains

The tree views' records are kept by three domains, each a crate that is pure
over ports: Volume (`agentfs-volume`), Clients (`agentfs-clients`) and
Trees (`agentfs-trees`). They share one kernel, `agentfs-contracts`: the
values, capabilities, each domain's trait, errors and events, and the ports
an adapter implements. None does I/O, and no domain depends on another.
Each domain, the Rules domain (`agentfs-rules`) among them, is `no_std`
outside its tests, so the compiler refuses its code naming the file system,
a process or the network ([ranks](../architecture.md#ranks)). A
consumer reaches them as `agentfs::contracts`, `agentfs::volume`,
`agentfs::clients` and `agentfs::trees` ([the library](agentfs.md)).

The kernel's values are each named from it alone, never through a crate
that uses them: a tree's identity (`agentfs_contracts::TreeId`, with
`NotATree`), and an entry's metadata as history records it
(`agentfs_contracts::meta`: `Kind`, `Perm` with `NotAPerm`, `Attrs` and
`Meta`), which the tree engine's calls take and the FUSE layer passes on.

The tree engine implements the ports over a served volume: [the domains'
store](engine.md#the-domains-store) says how a row is kept, and the
[architecture](../architecture.md#agentfs-contracts) maps every module. What
the domains do not do yet is on their design pages ([target
architecture](../target-architecture/README.md)).

## Rows and domain events

Each domain keeps its records as rows through the ports in
`agentfs_contracts::ports`. A port's write is durable before it returns, and
a row comes back as the domain's value with its audit stamps
(`agentfs_contracts::audit::Audited`). How the tree engine keeps that promise
(journal steps, chains, stamping) is on [the domains'
store](engine.md#the-domains-store).

Clients' `ClientStore` (`load`, `attach`, `rebind`, `end`) keeps one row per
tree: its attached clients (`ClientRow`: the id, the
grant and the pod context) and a fork's writer claim, each entry stamped. An
attach's step number is the client's ordinal. One `end` step removes every
ended client with its `AttachmentEnded`, then writes `LastDetached` for each
tree left empty. `release_claim` removes a dropped fork's writer claim with
`ClaimReleased` (nothing to release writes nothing).

`TreesService` (`agentfs-trees`) answers a second keyed `create` with its open
fork and ends forks (`end`, `end_keyed`) through Trees' `ForkStore` (`load`,
`create`, `end`, `drop_fork`). `settle(clients)` is its one judgement from
facts asked now: every open unkeyed fork no client is attached to (asked of
Clients' `attached_any`) ends, and every ended fork the store lets go drops.
It runs at boot after Clients' `reconcile`, at each `LastDetached` and after
each take, so an end a crash or a failed step left undone is judged again.
`ClientsService::on_fork_dropped` releases the claim, and `reconcile` (at
boot and at the drain) also releases the claim of every fork Trees reports
dropped. `detach` is idempotent within a run: rows are removed at an end, so
a client ended in an earlier run answers `UnknownClient`.

Volume's `VolumeStore` (`load`, `record`) keeps the volume's lifecycle row:
one row step per move, `VolumeStaged`,
`VolumeDraining` (detail `Stop`) or `VolumeReleased` (detail `Keep`). Its
`StateArea` port makes and opens the volume, and `QueueFacts` reports what
the queues hold. `VolumeService` (`agentfs-volume`) stages the volume (`stage`
opens the state area), unstages it, and on each `rejudge` while it drains
records the release once no client is attached and no queue holds an entry;
`ClientsService::on_volume_draining` and `on_volume_staged` refuse and allow
attaches meanwhile ([trees](../trees.md#volume-lifecycle)). Its
`release_verdict` judges a row or a queue it cannot read as `Staged`: never a
release on facts nobody read.

A store answers a failure as one `StoreError`: `Stopped` (the store stopped,
or the outcome of a step is unknown), `NotDurable` (nothing recorded),
`Damaged` or `Refused` (a step the store refused; [the domains'
store](engine.md#the-domains-store) says which). Each domain's error has one
`From<StoreError>`: `Stopped` for a stopped or damaged store, its own word for
a refusal (`ClientError::UnknownClient`, `TreeError::ForkUnknown`), and
`NotDurable` or `Io` for a write that changed nothing.

A domain publishes its event to its `EventSink` once the store returns. The
service's queue ([`agentfs-service`](service.md#dispatch)) hands each on in
order, after the call that caused it returns, and dispatches the events
between the domains: a `LastDetached`
settles Trees, a `ForkDropped` releases Clients' writer claim,
`VolumeDraining` and `VolumeStaged` refuse and allow attaches, and each end
re-judges the volume's release ([trees](../trees.md#forks-keys-end-and-the-drop)). Each event is on record as
a commit on its chain ([trees](../trees.md#rows-and-event-commits)).

## Domain invariants

The Volume, Clients and Trees domains hold these, and the code cites them by
their labels. A ban that `clippy.toml` holds binds the crates of this
workspace only: a consumer's build does not read it.

| # | Invariant | Enforced by |
|---|---|---|
| C1 | Only Clients mints a `ClientCap`, so only Clients makes a client writer | private fields; `ClientCap::mint` takes the composition root's `Minter`; in this workspace only, clippy bans `Minter::new` outside it; a consumer of the library is its own composition root ([the library](agentfs.md#the-consumer-api)) |
| C2 | A `ForksItCreates` launcher reaches only the forks it created, never the root | `ClientsService` checks the launcher against Trees' `facts(fork)` before any row |
| C3 | A read-only client has no `ClientCap`: its session writes nothing | the session's authority is `Read` for a read-only grant |
| C4 | A fork has at most one read-write client, for life | the writer claim in the fork's clients row, kept after its client ends and released only at `ForkDropped` |
| C5 | A session binds one client, and the client acts only through it: its calls name no tree and no writer | `agentfs_engine::session` owns the `ClientCap` |
| C6 | `LastDetached{tree}` is published exactly once each time a tree's last client ends | the end and its events are one row step |
| C7 | Every end has an event (detach, reconcile, a fork's end); no lease or heartbeat ends an attachment | no clock in the domains |
| VO1 | `Released` only when the volume is not staged, no client is attached and every queue is empty | only the pure `release::decide` builds a `Cleared` |
| VO3 | A move and its event are one unit | one row step, its event commit built from it |
| VO4 | The release verdict reads only facts asked at judgement time; nothing polls | the verdict is pure over `ReleaseFacts`; no clock in the domain |
| VO5 | A volume's id never changes, and an `ensure` with another name at the same location writes nothing | `decide_ensure` answers `SpecMismatch` before any write |
| VO6 | A fork launcher cannot `ensure`, stage or unstage the volume | `LaunchCap` with `may: Root`, else `NotPermitted` |
