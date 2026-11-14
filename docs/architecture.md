# Architecture

How the code is organised and how a change travels from a write to a
remote. [guarantees](guarantees.md) says what agentfs promises and
[recording](recording.md) how it records; this page says *where* the code
is. The libraries behind it, and why each was chosen, are under
[Technology choices](#technology-choices).

## Crates

| Crate | Kind | What it is |
|---|---|---|
| `agentfs-core` | library | The engine: path rules, the journal, frozen copies, commits, pushes, restores, the patch queue, the history service, LFS, telemetry. No FUSE and no CLI |
| `agentfs-fuse` | library | The FUSE layer: a passthrough filesystem over the backing tree that calls the engine at every change boundary |
| `agentfs` | binary | The `agentfs` command (every subcommand is listed in [configuration](configuration.md#commands)); OpenTelemetry export |
| `agentfs-csi` | library and binary | The CSI driver: `agentfs-csi`, one per node beside csi-provisioner, which makes the node's worktrees and supervises `agentfs serve-history`, `agentfs mount` and the release scan's `agentfs flush` and `agentfs push` children |
| `agentfs-align` | library | Git's histogram diff, called one way, for line identity (git-model §7). `agentfs-core` and `agentfs-model` both depend on it, so line identity does not depend on how `git` is called; one module, `lib.rs` |
| `agentfs-contracts` | library | The shared kernel of the domains around the git model ([the system](target-architecture/system.md)): values (the tree and volume identities, names, the pod context), capabilities and their minter, each domain's trait, errors and events, and the ports adapters implement. No I/O; depends on no other agentfs crate |
| `agentfs-volume` | library | The Volume domain, pure over its ports: `ensure`, the lifecycle and the release verdict |
| `agentfs-clients` | library | The Clients domain, pure over its ports: attachments, grants and each fork's one writer |
| `agentfs-trees` | library | The Trees domain, pure over its ports: each fork's lifecycle |
| `agentfs-conformance` | library and binary, tests only | The model's recorded calls against the real tree library and real mounts (git-model §18). Depends on `agentfs-core` and `agentfs-model`; nothing depends on it |

The dependencies run one way: `agentfs-csi` → (`agentfs` as a child
process) → `agentfs-fuse` → `agentfs-core` → `agentfs-contracts`. The
domain crates (`agentfs-volume`, `agentfs-clients`, `agentfs-trees`) depend
on `agentfs-contracts` alone, and on no other domain. `agentfs-csi` links
`agentfs-core` only to create state areas and read their status; it never
records.

## Module map

Every module of every crate, one line each. A test
(`crates/agentfs-core/tests/docs_reference.rs`) fails when a module is
missing from these tables or a row names a module that no longer exists;
`tests.rs` files are not listed.

### `agentfs-contracts`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the module list and the shared kernel's overview |
| `audit.rs` | Principals and the audit stamps every row carries (`Audited<T>`), written by the store adapter |
| `caps.rs` | Capabilities (`LaunchCap`, `ClientCap`) and the one `Minter` a composition root makes |
| `clients.rs` | The Clients domain's contract: client ids, grants, bindings, errors, events and the `Clients` trait |
| `name.rs` | Validated names and labels (volume, pod field, writer, client, launcher, fork key) and the one safe-name rule |
| `pod.rs` | The pod context a writer ran in, and a writer with it (`WriterAt`), as commits name them |
| `ports.rs` | The ports adapters implement: the state area, each domain's rows as journal steps, queue facts, sessions and the event sink |
| `tree.rs` | A tree's identity, `root` or `fork/<n>`, and a fork's number: the text history, refs and checkouts name a tree by |
| `trees.rs` | The Trees domain's contract: fork settings, facts, phases, errors, events and the `Trees` trait |
| `views.rs` | The Views domain's contract: mount points, mount options, errors and the `Views` trait |
| `volume.rs` | The Volume domain's contract: identity, spec, lifecycle, the release verdict's facts and blockers, errors, events and the `Volume` trait |

### `agentfs-volume`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the Volume domain's overview |
| `ensure.rs` | `ensure`'s one decision from what the state area holds |
| `release.rs` | The release verdict, pure: every blocker, and the `Cleared` proof only `decide` builds |
| `service.rs` | The Volume domain over its ports: ensure, stage, unstage and the Stop drain |

### `agentfs-clients`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the Clients domain's overview |
| `service.rs` | The Clients domain over its ports: attach, detach, reconcile and the per-tree index rebuilt from rows |

### `agentfs-trees`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the Trees domain's overview |
| `service.rs` | The Trees domain over its ports: fork create, end, end by key, the drop and the reaction to a tree's last detach |

### `agentfs-core`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the module list and the engine's overview |
| `volume.rs` | Opens a state area: crash recovery, then the [`Recorder`](#the-synchronous-step) used at every boundary and the `Committer` that turns records into commits |
| `record.rs` | What the journal stores: one record per change boundary, absolute state, never deltas |
| `state_area.rs` | The `StateArea` port over the tree actor: inspect without the lock, create a volume with a new id, open and keep the recovered volume, served (`Recording`) |
| `tree_actor/mod.rs` | The tree actor: the actor, recovery and queue orchestration. A step is one unit of work: decided, appended to the journal as the change itself, applied to the saved state, with no git on the call path; creating a volume (its identity file, then `format` last), inspecting and opening a state area, recovery (history built through the journal, then abandoned opens closed); queue takes decided on built history and recorded as take steps |
| `tree_actor/history/mod.rs` | Tree history: the store opened and created in one place, snapshots read from commits, object tables, the owners of direct saves, private checkouts, and the clean-up of a cut-short build's packs |
| `tree_actor/history/builder.rs` | The builder, on its own thread: reads durable records from its own cursor, replays each step on the built trees with the actor's transitions, packs a batch's commits with `git fast-import`, then installs pack, built-log entry, refs, `config` and checkouts in that order; a replay that disagrees with its record stops building (`Disagrees`) |
| `tree_actor/history/built_log.rs` | The built log, the builder's own file: one fsynced entry per batch naming the built point and the value each step's build gave its pointers |
| `tree_actor/history/feed.rs` | The builder's packer: one `git fast-import` child per batch, fed blobs and commits; fsyncs the pack files it installed and their directory |
| `tree_actor/history/lag.rs` | The lag gate's arithmetic, pure: each tree's lag from its unbuilt steps and the builder's measured rates, whether its new work may start, the health it gives, and the alarm and limit (constants) |
| `tree_actor/history/format.rs` | A tree commit's layout, its one home: `files/<name>`, `metadata.json` and `namespace.json`, written as edits of the parent's tree and read back as snapshots and checkouts |
| `tree_actor/call_path.rs` | The call path as a view host, the CLI and the conformance replay reach it (`Recording`): the actor behind its one lock, with durability waits outside it, blocking or deferred to a callback; History reached without the lock; takes and `settle`; dropping it makes every record durable and builds every step. The lag gate holds a tree's calls that start new work while its lag is over the limit, deferred to a callback for the view |
| `tree_actor/history/built.rs` | History as built, the one owner of a tree volume's Git store: the built point, built snapshots (only it constructs one), built and failed events that wake waiters and the builder, the oldest unbuilt step's age and the builder's health |
| `tree_actor/history/message.rs` | Every message a tree-view commit carries, its one home: each message's bytes (Root, direct saves, landings, syncs) with the volume and pod trailers, and the readers of its trailers, the landed writer's among them (target git-model §5) |
| `tree_actor/history/refs.rs` | The ref layout of a tree volume's history, its one home: branches, queues and their entries, collision refs, object tables, and the tips that reach every save (target git-model §4) |
| `tree_actor/history/take.rs` | Queue takes decided on built history: a landing judged by the referee into its upstream, and a fork's prefix sync planned from its upstream's commits, each as the head move, snapshot and collisions a take step records |
| `tree_actor/history/upstreams.rs` | Each fork's upstream, the shape of a volume's trees: added when a fork is made, checked at recovery, and written as the journal's checkpoint and Git's config |
| `tree_actor/journal/mod.rs` | The actor's journal: salted checksummed frames, each appended with one write and no fsync and carrying the durable mark, the commit point for calls and steps (a step's record is the change itself and the pointer changes its build publishes), torn-tail and corruption rules, compaction, and the builder's reads of a frame at an offset (target git-model §3) |
| `tree_actor/journal/commit_point.rs` | The actor's appends to the journal and when it compacts: a checkpoint is written with history built through its last record, its pointers the built ones |
| `tree_actor/journal/durable.rs` | The journal's one numbering (`RecordNo`) and its durable mark: one fsync leader whose fsync covers every record appended before it began (group commit), and a waiter thread that answers deferred waits |
| `tree_actor/journal/fold.rs` | What a journal's frames add up to, pure: pointers as the records name them, upstreams, live opens with their calls, and page writes (target git-model §12) |
| `tree_actor/pointers.rs` | Branches, queue entries, collision refs and object tables, as oids built or as a record's targets (an oid, or a step's value): pure, no I/O |
| `tree_actor/recording/mod.rs` | Recording: a tree's file calls (open, read, write, truncate), each journalled (no fsync) before it returns, and the steps that save them: create, close, rename, unlink, each decided, appended and applied with no git |
| `tree_actor/recording/pages.rs` | The kernel's one page cache per file: page fills, page writes and the size replies for shared mappings, and the one rule of which page-write pieces a close applies, which the actor and the builder both run (rules ledger, R1.4) |
| `tree_actor/queue.rs` | Queue and locks: P1 eligibility, held keys and the `Cut` of a prefix sync |
| `tree_actor/referee/mod.rs` | The referee: every line and name rule of a fork and its upstream, pure. `land` composes verdicts, names, placing and whole into a decision; `sync` keeps a fork's undecided versions; `touches` gives a step's lock keys |
| `tree_actor/referee/ancestry.rs` | The one ancestry walk (`descends`) over names and lines alike, and the one union of line ancestries |
| `tree_actor/referee/names.rs` | Names: a landing's name onto a path a live upstream file holds (R11.2), and a sync's names with the fork's undecided ones kept |
| `tree_actor/referee/placing.rs` | Where versions go in a file's lines: a landing's applying lines and its joins (R10.4, R10.8), a sync's undecided lines (R10.6, R10.9) |
| `tree_actor/referee/undecided.rs` | A fork's undecided versions, through the referee's one question to history (`Landed`) |
| `tree_actor/referee/verdicts.rs` | Rule 3 for one change, for names and lines alike: each changed version's verdict and its loss so far |
| `tree_actor/referee/whole.rs` | R3e: a change lands whole or not at all; the collisions in their order |
| `tree_actor/stages.rs` | Measurements only, never read by a decision: runs and time per actor stage, and the Git children the actor's path has started (counted at `git_cli::counted`, the one spawn site; target git-model §18) |
| `tree_actor/tree.rs` | A tree's shared types: the event number a step is given, the line versions (`<event>:<index>`, ordered as their text) and line maps it writes; a tree's shape (names, objects, identities) with its one transition per step (`Shape::apply`), the actor's saved state (`SavedTree`) and the builder's (`Tree`, with line maps and name histories) |
| `tree_reconcile.rs` | The line-map library: histogram alignment and line identities, generic over the caller's version |
| `tree_git.rs` | The tree actor's Git store: durable objects, the rebuildable copy of the refs in files format and the private checkouts through libgit2 in process; `config` and `HEAD`; the history walks and `init` as Git children, which retain the actor lease. It knows no ref layout and no trailer |
| `journal.rs` | The durable change log: framed, checksummed records with group-commit `fsync` |
| `pending.rs` | Frozen copies in `pending/`: `FICLONE` reflink, else copy plus `fsync`; slot-numbered ids for fronts |
| `claims.rs` | Per-path ordering: a record claims its paths from decision to tracked-set update |
| `stamps.rs` | Freeze stamps: the history service orders fronts' writes of one path by the backing file's stat stamp at freeze; the last write journaled at each path |
| `tracked.rs` | The tracked set, git's index in agentfs: branch head plus durable records, a tree so a directory move is O(1) |
| `reconcile.rs` | Directory moves: the duties that reconcile a moved tree before its move is committed |
| `rules.rs` | Path rules: recorded or skipped, and why, from `info/exclude`, `info/attributes` and optionally tree rules |
| `visibility.rs` | The sandbox overlay's visibility gate: which names it shows and where a new or moved name goes, decided from the rules alone |
| `nested.rs` | Detects nested git repositories, including ones being created |
| `non_empty.rs` | A list with at least one element by construction (`config::Remotes::Pushed`) |
| `probe.rs` | Bounded, non-blocking reads of small workload files (`.gitignore`, `.git` files) during the synchronous step |
| `paths.rs` | Repository-relative paths and the names git refuses to store |
| `checks.rs` | git fsck rules and remote limits, checked at record time and again at commit |
| `gitmodules.rs` | A port of git fsck's `.gitmodules` rules |
| `fsync_class.rs` | The `fsync` class for databases: membership and groups |
| `history.rs` | The bare history repository: records to commits with `Agentfs-*` trailers |
| `fork/mod.rs` | Forked volumes: a volume started from another's latest commit that publishes one change for a receiver to gate |
| `fork/identity.rs` | A fork's identity, fixed at create (source, base, push target, link: linked or unlinked), and its ref names |
| `fork/create.rs` | Creating a fork from a source's state area, without the source's lock or any credentials |
| `fork/finish.rs` | The finisher: the change and finish commits, publishing to the fork remote, the single decider of a refused publish, the operator's abandon, and the fork status DeleteVolume reads |
| `fork/listing.rs` | The published changes on a fork remote, each checked (fork name, finish trailers, change id, parent, tree), read in a receiver's repository with its own git |
| `fork/deliver.rs` | Library delivery of a published change: the `Delivery` contract, its four methods (inbox, branch, command, webhook), the payload (schema 1) and the one factory that validates a method; never used by the CSI driver |
| `fork/start.rs` | Starting a volume from a pinned commit of another worktree's branch on a remote, and the operator's `start accept` that saves an old-branch tip under the volume's own `accepted/*` refs |
| `fork/receive.rs` | A local-only source receiving its fork's branch, change and finish refs into its own history, never applied |
| `identity.rs` | Committer and author identities from templates |
| `service.rs` | The background loops: commit loop, one push loop per remote, repack |
| `push.rs` | Pushing the volume's own refs to each remote, with per-remote state and backoff |
| `push/claim.rs` | The branch rule's pure deciders: `claim` (create or fast-forward, from the remote's value in the connection) and `start_branch_moved` (the start-branch alarm) |
| `push/stop.rs` | A push thread's phase (idle, attempting, updating a local remote, finishing) under one lock, and cancelling it at shutdown: nothing is sent after the cancel, and a local push already updating the remote's pack and refs is waited for until it reports, never cut off |
| `release.rs` | The one deletion decider: delete, keep or wait for a worktree, from its remotes mode, what each remote advertises, the journal's backlog, `onDelete` and a fork's status |
| `push_size.rs` | Measuring a push's git pack against `agentfsMaxPushSize` before connecting, and the first-parent step that fits |
| `remote_auth.rs` | Credentials and host-key checks for one remote, shared by push and patch fetch, and the scope that hands an SSH remote to the transport |
| `ssh_transport.rs` | `ssh` feature: agentfs's libgit2 subtransport for SSH URLs over russh: host key or certificate check before authentication, the key (decrypted) or ssh-agent identities, timeouts |
| `remote_url.rs` | Pure URL helpers: transport, host, port, userinfo, SSH path, as libgit2 sees them |
| `proxy.rs` | `http.proxy` and `remote.<name>.proxy`: parsing, and libgit2's proxy options |
| `credentials.rs` | Credential, SSH key and passphrase files for remotes |
| `known_hosts.rs` | OpenSSH known_hosts parsing and matching, and `@cert-authority` host certificate checks |
| `watched.rs` | Files re-read when their stat stamp changes: credentials, keys, known_hosts |
| `repack.rs` | Periodic `git repack --geometric=2` of the history |
| `checkpoint.rs` | Checkpoints: one change-line commit per intent (`refs/agentfs/<volume>/change-line`), with the head's tree and the patches landed since as parents, merged back into the branch by a two-parent marker |
| `git_cli.rs` | Running the `git` program on a state area's git directory, in an environment of its own: the one home for `repack.rs` and the patch merge |
| `command.rs` | Running an operator's program without a shell: `PATH` only, its own process group killed at the deadline, input on stdin; the one runner of fork delivery's command method and the `patch-accept` hook |
| `restore.rs` | Restore and revert: plan from history, check, apply to a tree |
| `confined.rs` | `openat2`-confined access to the tree for restores |
| `operations.rs` | Restores and reverts in progress: their writes become one operation commit |
| `file_lock.rs` | `FileLock`, the one way agentfs takes an advisory file lock (`flock`, shared or exclusive, waiting or not), unlocked before its file is closed so a child process still sharing the file cannot hold it on; `clippy.toml` disallows the lock calls elsewhere |
| `lock.rs` | The state-area lock, an `flock` on `meta/lock`, taken only on a filesystem whose locks keep other nodes out |
| `shared_fs.rs` | Which filesystems a state area may live on: refuses NFS below 4.1, `nolock` and `local_lock` other than `none` whenever the lock is taken; tells a network filesystem from a node's own disk for agentfs-csi's base path |
| `mountinfo.rs` | The one parser of `/proc/self/mountinfo`: device, root, mount point, fstype, source and super options of each mount, and the mount holding a path |
| `local_socket.rs` | Unix sockets in the state area with peer-credential checks, for the control socket and the history service |
| `listening_socket.rs` | `Listener`, the one way agentfs listens on a Unix socket (control socket, history service, agentfs-csi), shut down (`SHUT_RDWR`) when stopped or dropped so a child process still sharing it cannot take connections no one answers; `clippy.toml` disallows raw listener construction elsewhere |
| `fsutil.rs` | Small helpers for the state files under `<state>/meta` |
| `config.rs` | Operator settings from the history repository's own git config, and `config::KEYS` |
| `events.rs` | Alarm and event signals, handed to a non-blocking sink |
| `failpoints.rs` | Named failpoints on the durable path, acting only in test builds with the `failpoints` feature ([crash model](#crash-model)) |
| `telemetry.rs` | OpenTelemetry instruments and spans over the API only; the binary picks exporters |
| `history_service/mod.rs` | One history service per volume fed by fronts |
| `history_service/wire.rs` | The gRPC contract `agentfs.history.v2` (generated from `proto/`) and its validated conversions to agentfs types |
| `history_service/bridge.rs` | Synchronous callers on a tokio runtime (Tokio's bridging pattern): multi-thread, or current-thread driven by its callers; for the client and the server |
| `history_service/server.rs` | The service's socket: tonic, per-connection admission by `Hello` (peer credentials, lease, identity) within a deadline, `grpc.health.v1`; `Recorder` calls off the runtime's workers |
| `history_service/client.rs` | A front's HTTP/2 connections to the service: multiplexed calls driven by the calling threads, and the listening stream's; deadlines on every call, never resends |
| `history_service/lease.rs` | A front's `flock` lease on its pending slot |
| `history_service/listener.rs` | A front's listening stream: answers the service's settle, open-for-writing and invalidate calls |
| `history_service/fronts.rs` | The service's links to listening fronts, called to serve live restores |
| `lfs/mod.rs` | Git LFS: large content as LFS objects with pointer blobs in history |
| `lfs/pointer.rs` | The canonical LFS pointer format |
| `lfs/policy.rs` | Which store a recorded file goes to: blob or LFS |
| `lfs/attributes.rs` | The managed `filter=lfs` block in the root `.gitattributes`, in history only |
| `lfs/cache.rs` | The local LFS object cache, `<state>/lfs/objects` |
| `lfs/endpoint.rs` | Each remote's LFS endpoint and credentials, in git-lfs's precedence |
| `lfs/ssh_auth.rs` | `git-lfs-authenticate` for SSH remotes: the command, running it over the SSH transport, and its answer |
| `lfs/client.rs` | The batch API client (basic transfer) and the standalone-file transfer |
| `lfs/pool.rs` | The worker pool that moves up to `lfs.concurrenttransfers` objects at once |
| `lfs/referenced.rs` | Which LFS objects commits reference: one tree, or a range of history |
| `lfs/lock.rs` | The cache lock, `<state>/lfs/lock`: shared by restores and fetches, exclusive for a prune |
| `lfs/prune.rs` | `agentfs lfs prune`: cached objects confirmed on every remote's endpoints, and their deletion |
| `lfs/upload.rs` | Which LFS objects a push must upload first |
| `lfs/fetch.rs` | LFS objects for restores and `agentfs lfs fetch`: cache first, then the remotes |
| `lfs/fake.rs` | An in-process LFS server for tests |
| `patch/mod.rs` | The patch queue for iac |
| `patch/attached.rs` | Whether a workload may be attached when a daemon starts, so the queue waits: the mount's attached mark (`meta/attached.json`), the pod scope it records (`PodScope`, from mountinfo) and the judgement of a start (`judge`: another pod's mark lands, anything else defers with a reason) |
| `patch/admit.rs` | `serve-git`'s pushers and their rights, and admitting a push whole before git keeps it |
| `patch/accept.rs` | Acceptance rules: whether a cleanly merged patch may land (clean merge, approval, the `patch-accept` hook) |
| `patch/integrate.rs` | A patch branch, and merging it onto the branch head with the `git` program |
| `patch/submit.rs` | `agentfs patch submit`: the producer side, driving the producer's own `git` |
| `patch/fetch.rs` | Fetching the inbox refs from the patch remote, best effort |
| `patch/notify.rs` | Telling the integrator of every verdict change: the `patch-notify` hook and the notify webhook, with the delivered cursor in `meta/notify.json` |
| `patch/queue.rs` | Applying the queue at mount, before the workload sees the tree; judging it read only while a daemon holds the volume |
| `patch/status.rs` | The status ref iac reads, and the local cache in `meta/patch.json` |

### `agentfs-fuse`

| Module | Role |
|---|---|
| `tree_view.rs` | Main/sandbox FUSE views over one target-model actor: direct I/O, saved-reader snapshots, byte-call writes, FLUSH, FSYNC, FSYNCDIR and `O_SYNC` writes answered once durable, and the kernel's page fills, page writes and invalidations for shared mappings (rules ledger, R1.4) |
| `lib.rs` | Crate root and overview |
| `ops.rs` | Every filesystem operation without the kernel, unit-tested against a real directory and state area |
| `fuse.rs` | The kernel adapter: `fuser::Filesystem` over `Ops`, and kernel passthrough negotiation |
| `session.rs` | What one FUSE session serves: the `Operations` trait that `fuse.rs` calls, implemented by `Ops` and by the overlay |
| `overlay.rs` | The sandbox overlay's session: the visibility gate and the view owner in front of the shared `Ops` |
| `overlay/tmp.rs` | The sandbox overlay's tmp layer: files a sandbox creates under names the rules skip, private to the overlay, in a store of the state area, moved into the tree by rename |
| `invalidate.rs` | Kernel cache coherence across a daemon's FUSE sessions: each session reports the changes it served (`Coherent`), and a worker thread sends them to the other sessions' kernels (`Invalidator`) |
| `access.rs` | Mode-bit permission checks, as the kernel's `generic_permission` decides them |
| `boundary.rs` | Recording a change boundary: builds the inputs and calls the recorder, or a front's client |
| `creds.rs` | Creating as the caller: a worker thread's filesystem uid, gid and groups switched for one system call |
| `dirty.rs` | Whether an inode changed since it was last recorded, so clean closes record nothing |
| `inodes.rs` | The inode table: FUSE node ids, backing inodes and their paths |
| `names.rs` | What a path in the mount is, for recording |
| `views.rs` | Where workloads see the mount: the mount point, its bind mounts (mountinfo read through `agentfs_core::mountinfo`), `--workload-mount` |
| `backing.rs` | Confined access to the backing tree |
| `groups.rs` | fsync-class groups: holding writes while a group is captured, and scheduling captures |
| `sweep.rs` | The periodic sweep: files held open, failed records, the fsync-class timer |
| `privileges.rs` | What the daemon may do as the caller or as itself |
| `mount.rs` | Layout checks, mounting (also stacked over a stale mount, for `--on-stale-mount recover`), unmounting, detaching stale layers, and the mount state at a path (mountinfo read through `agentfs_core::mountinfo`) |

### `agentfs`

| Module | Role |
|---|---|
| `trees_cmd.rs` | `trees init` (`ensure` through the Volume domain), `fork` and `mount` composition root; separate state format |
| `pod_args.rs` | The pod flags (`--namespace`, `--pod`, `--pod-uid`, `--node`) one clap group shares between the recording engine's commands and `trees` |
| `main.rs` | The CLI (`Cli`), command dispatch, `init`, `status`, `record`, `flush`, `push` (direct), `run` and `mount` |
| `daemon.rs` | Signals and shutdown for `run` and `mount` |
| `front.rs` | `mount --history-service`: a front of a history service |
| `serve_history.rs` | `agentfs serve-history`: the history service of a volume |
| `serve_git.rs` | `agentfs serve-git`: the history repository as a git server for patch pushes (`git http-backend` behind an HTTP server), and its `pre-receive` and `post-receive` hooks |
| `hold.rs` | `mount --on-stale-mount hold`: a restarted sidecar waiting out a stale mount (`recover`, which mounts over it, is in `main.rs`) |
| `control.rs` | The control socket of a running daemon (`mount`, `run`, `serve-history`), through which `push`, `restore` and `revert` act |
| `push_now.rs` | `agentfs push` through a running daemon: the daemon's handler and the client |
| `recovery.rs` | `agentfs restore` and `agentfs revert`, through a mount, through the history service, or directly |
| `patch_cmd.rs` | `agentfs patch` and the mount's patch phase |
| `checkpoint_cmd.rs` | `agentfs checkpoint`: through a running daemon's control socket (its commit loop cuts it), else under the state-area lock |
| `fork_cmd.rs` | `agentfs fork create`, `finish` (`--retry`), `status`, `abandon`, `list` and `deliver` |
| `start_cmd.rs` | `agentfs start accept --tip`: a started volume's answer to the start-branch alarm |
| `lfs_cmd.rs` | `agentfs lfs`: fetching objects into the local LFS cache, and pruning it |
| `otel.rs` | Telemetry export: the OpenTelemetry SDK, exporters (http/protobuf, http/json, gRPC on its own runtime) and the environment it reads |
| `otel/tls.rs` | The `otlp-tls` feature: TLS to the collector (CA bundle or system store, client certificate) for the HTTP and gRPC exporters |
| `doc_tables.rs` | Test only: a small markdown reader for the documentation tests |
| `reference_tests.rs` | Test only: checks docs/configuration.md against the CLI's flags and variables |

### `agentfs-conformance`

| Module | Role |
|---|---|
| `lib.rs` | Crate root, one call per kind of replay, each recording what the system did into a sink; which traces fit a real mount |
| `engine_names.rs` | `EngineNames`: the engine's `TreeId` of each tree of a trace, the root as `root`, fork n as the n-th fork made |
| `git_history.rs` | Its own parse of the engine's commits (`git cat-file`, `metadata.json`, `namespace.json`), and `EngineHistory`: the collisions and heads a replay compares, read from git |
| `library.rs` | `Library`: the shared calls on `Volume`, seeded as the model's first commit |
| `mounted.rs` | `Mounted`: the shared calls as real system calls on a mount of a failpoints build, each take through the take gate |
| `invariants.rs` | What must hold in the real system, read from `git`, its own parse of `metadata.json` and `namespace.json` and its own byte oracle, never `tree_actor` |
| `bin/phase1-replay.rs` | `phase1-replay model\|library\|mount DIR [--record OUT]`: replays the traces, with floors, against `open-findings.txt`; `--record` writes what the system did with each |

### `agentfs-csi`

| Module | Role |
|---|---|
| `lib.rs` | Crate root, the driver name and overview |
| `main.rs` | The `agentfs-csi` command (`Cli`): one node plugin serving every service; refuses a base path on a network filesystem |
| `csi.rs` | The CSI v1 protocol, generated at build time |
| `server.rs` | Serving the CSI services on a Unix socket |
| `identity.rs` | The CSI Identity service and the topology key |
| `controller.rs` | The Controller service on each node: CreateVolume on this node (topology, start classes, forks of a local source), DeleteVolume through the release decider |
| `node.rs` | The Node service: validates calls and hands them to the supervisor; this node's topology |
| `supervisor.rs` | One `serve-history` per staged volume and one front per pod, restarted when they crash; NodeStage never creates a worktree; a finished fork published read-only |
| `launcher.rs` | Starting the `agentfs` children |
| `release_scan.rs` | The release scan: one `agentfs flush` per worktree no pod stages whose journal holds records history lacks, then one `agentfs push` until it exits 0 at the head and remotes, with backoff; one `agentfs fork finish` per fork whose deletion asked for it, skipping refused and abandoned forks |
| `mounter.rs` | The node plugin's mount operations: bind mounts onto the kubelet's targets |
| `params.rs` | StorageClass and PersistentVolume parameters |
| `volume.rs` | Worktrees on this node's disk: creating one (empty, started, a fork), adopting a static PV over its worktree |
| `records.rs` | The driver's persisted records (`csi.json`) |
| `layout.rs` | Where the driver keeps things |
| `error.rs` | Errors mapped onto gRPC status codes |

## Data flow

### The synchronous step

What happens before a change boundary's call (`close` after a write,
`unlink`, `rename`, `fsync` in the fsync class, ...) returns to the
workload. Its cost is measured in
[performance.md](performance.md).

```text
workload ── close(2) ──▶ kernel FUSE ──▶ fuse.rs ──▶ ops.rs
                                                     │ dirty.rs: changed since last record?
                                                     ▼
                                               boundary.rs
                                                     │
                       ┌─────────── single process ──┴── front ──────────────┐
                       ▼                                                           ▼
          volume.rs Recorder::record                                 volume.rs Recorder (front)
            claims.rs   claim the paths                                rules.rs, checks.rs
            rules.rs    recorded or skipped?                           pending.rs freeze into its slot
            tracked.rs  (ignore rules: untracked only)                 history_service/client.rs ── Record ──▶
            checks.rs   fsck rules, remote limits                      server.rs: claims, rules again,
            pending.rs  freeze: reflink, else copy+fsync               journal.rs append + group fsync
            journal.rs  append + group-commit fsync                    ◀── durable reply ──
            tracked.rs  update; release the claims
                       │                                                           │
                       └──────────────────────────────┬────────────────────────────┘
                                                      ▼
                                        the call returns to the workload
```

A record that cannot be made durable fails the call with `EIO`; a
path the rules skip returns at once with nothing frozen.

### After the call returns

```text
journal.rs + pending/ ──▶ service.rs commit loop (every commitWindow)
                              volume.rs Committer ─▶ history.rs: blobs (lfs/ for large files),
                                                    trees, commit with Agentfs-Seq ─▶ branch
                          service.rs push loop per remote (every pushInterval)
                              push.rs (+ lfs/upload.rs, remote_auth.rs) ─▶ remote
                          repack.rs (every repackInterval)
```

### Mount start

`agentfs mount` takes the state-area lock (`lock.rs`), opens the volume
(`volume.rs`: journal replay, dropping committed records, removing orphan
frozen copies), runs the patch phase (`patch_cmd.rs`, `patch/fetch.rs`,
`patch/queue.rs`) before anything can see the tree, then mounts
(`mount.rs`) and starts the background loops (`service.rs`). A front
(`front.rs`) instead leases a pending slot and connects to
`serve-history`, which holds the lock and runs the loops.

### Restores and reverts

`agentfs restore` and `revert` (`recovery.rs`) plan from history
(`restore.rs`), then write through the running mount's control socket
(`control.rs`) so the writes are recorded like any other and grouped into
one commit (`operations.rs`); under `serve-history` the service settles the fronts
(`history_service/fronts.rs`), writes the backing tree itself and has
each front drop its kernel caches (`history_service/listener.rs`); with
neither they write the backing tree directly through `confined.rs` and
record in-process.

### The push loop

One loop per remote (`service.rs`), every `pushInterval`, with backoff
after failures. Each attempt (`push.rs`, `push_once`), in this
order:

1. takes the remote's push lock (a repack waits a second and tries again
   while any push holds one, so a push never reads a pack being deleted) and reloads the
   remote's push state, since another process may have pushed;
   with `agentfsMaxPushSize` set, measures the pack the remote lacks
   (`push_size.rs`) before connecting: over the limit, only the longest
   first-parent step that fits is pushed below (branch only, its LFS
   objects, applied refs held), and a single commit over it fails as
   `too_large`;
2. **LFS objects first** (`lfs/upload.rs`, `lfs/client.rs`): every
   object referenced at a managed path by a commit the remote lacks is
   uploaded through the batch API before any ref, so a ref never reaches
   the remote ahead of its objects, up to `lfs.concurrenttransfers`
   at once (`lfs/pool.rs`), with objects over the remote's
   `agentfsMaxLfsObjectSize` refused first. A failure ends the attempt at
   stage `lfs` with no ref pushed. The remote's proxy (`proxy.rs`), if
   set, carries these requests and libgit2's `http(s)` connections below;
3. **refs**: the branch and the volume's `diverged/*` and `accepted/*`
   refs, never forced. In libgit2's push negotiation, from the branch's
   old value in that connection, `push/claim.rs` decides: the branch is
   created or fast-forwarded only; one another worktree wrote stops the
   push before anything is sent, and is a divergence.
   The push state records the last pushed commit. For a started volume
   pushing to its start remote, the start branch is listed first and
   `start_branch_moved` raises the start-branch alarm;
4. **the queue**, for the patch remote only and after every branch
   attempt, failed or not: the status ref (forced, a snapshot) and,
   only when the branch push worked, the `applied/<n>` refs; then, once
   those are on the remote, the deletion of each applied `inbox/<n>`,
   leased on the oid listed. Its failures never count against the branch.

### Shutdown

SIGINT, SIGTERM or an external unmount start it (`daemon.rs`); a second
signal exits at once with status 130, which the journal makes safe. For
`mount` (`main.rs`, `mount.rs` `Mounted::shutdown`, `service.rs`):

1. a restore in progress through the control socket gets up to
   `--unmount-grace` to finish (`control.rs`);
2. the mount is unmounted (lazily if busy) and the FUSE session gets up
   to `--unmount-grace` to end; files still open after that are
   abandoned, and their later changes are not recorded;
3. the sweep and the fsync-class timer stop; a final sweep records every
   armed inode with a handle still open and retries the repair queue,
   and every changed fsync-class group is captured;
4. paths still unrepaired are saved in `meta/repairs` and listed on exit;
5. the commit loop stops after a final commit of everything journaled
   (exit status 1 if it fails: the records stay journaled for the next
   open);
6. each remote that lacks commits gets one more push of the final
   commit, all within `--push-grace` from the stop (pushes in progress
   included); at the deadline every push is cancelled (`push/stop.rs`),
   one still running is abandoned on its thread, unless it is updating a
   local remote's pack and refs, which is waited for up to
   `LOCAL_UPDATE_CAP`, and `--push-grace 0s` skips the final push. `run` and
   `serve-history` stop the same way (`Service::shutdown`), so the
   last commit before a stop reaches the remotes when they answer in
   time, and otherwise goes out on the next start.

## Technology choices

Each choice is the right technology for its job. agentfs uses libgit2
through `git2` and its own LFS, HTTP and SSH clients, and runs the `git`
program where git does the job better.

| Need | Choice | Why |
|---|---|---|
| FUSE | `fuser` 0.18, vendored with the upstream `FUSE_TMPFILE` dispatch backport | Multi-threaded dispatch with cloned fds and passthrough (`BackingId`). Its default features are pure Rust: `mount(2)` as root, `fusermount3` otherwise, no libfuse. The backport exposes anonymous file creation; provenance is in `vendor/fuser/AGENTFS-PATCH.md`. It sends the attribute inode number as the node id, so `st_ino` is the node id |
| Git objects, commits, push, credentials | `git2` (libgit2) | It has no filter or merge-driver registration, and its fsync option is only reachable through `libgit2-sys` |
| TLS and SSH transports | `git2`'s `https` feature (OpenSSL) behind agentfs's `https` cargo feature. For SSH, agentfs's own libgit2 subtransport over `russh` (ring and the pure-Rust `rsa` crate), with libgit2 built without libssh2 | `https` is a C library, so it is feature-gated and default builds stay free of OpenSSL. `ssh` needs no OpenSSL or libssh2 (`ci/check-deps.sh` checks), and russh gives the transport the host key before authentication, a host key algorithm preference and host certificates, which libssh2 cannot negotiate. The `rsa` crate has a timing side-channel advisory on private-key operations: Ed25519 keys are recommended |
| SSH host keys | agentfs's own known_hosts parser over `base64` and `sha1_smol` | Pure Rust. The transport checks host keys with it before any authentication and never reads `~/.ssh` |
| Ignore and attribute rules | `gix-ignore`, `gix-attributes` | Pure Rust, fed only buffers agentfs reads itself, so nothing global can leak in. libgit2's rules cannot do this: it always loads every working-tree `.gitignore` on the path, merges every source by git's precedence without saying which file an attribute came from, takes `core.excludesFile` from the merged system and global config, and `git2::Repository` is not `Sync`. gitoxide has no push, so it does not replace `git2` |
| LFS | agentfs's own client over `ureq` 3 and `sha2` | No LFS client library exists. `ureq` is pure Rust with no TLS by default and proxies through its built-in `CONNECT` support. The `https` feature adds its `native-tls` (OpenSSL). agentfs builds `ureq`'s connector chain itself, through `ureq`'s `unversioned::transport` API, which is outside its semver promise: a `ureq` upgrade must check the chain against `ureq`'s `DefaultConnector`. The reason is that `ureq`'s TCP transport fails a read that returns `EINTR`, and Linux never restarts such a read after a signal |
| Telemetry | `opentelemetry` (API) in the core; `opentelemetry_sdk` and `opentelemetry-otlp` in the binary, with `tokio` (one worker, started only for gRPC). The `otlp-tls` feature adds rustls (ring) | OTLP http/protobuf, http/json and gRPC. The default build is pure Rust with no OpenSSL, ring, aws-lc, cmake or protoc. `otlp-tls` adds ring only. `ci/check-deps.sh` checks both |
| Patch pushes to agentfs | `agentfs serve-git`: git's own `git http-backend` behind `tiny_http` | git's server is the prior art for serving git: receive-pack's quarantine and `pre-receive` give "admit before keeping", and `transfer.hideRefs` limits visible refs (names only). `tiny_http` over `hyper`: the binary has no async runtime outside OTLP gRPC, and a request is a CGI run. Plain HTTP, with TLS left to a fronting proxy. Only the smart endpoints are routed, since dumb HTTP would hand out every object |
| Patch submission | the producer's `git` CLI (`ls-remote`, `fetch`, `commit-tree`, `push --force-with-lease`) | It runs with the producer's own credentials and config, and the lease makes a number create-only |
| Tree actor | Its call path runs no git: a step's journal record is the change itself. A builder thread makes the commits behind it: `git fast-import` packs each batch, `git2` (libgit2) reads history and writes the copy of the refs and the private checkouts; the `git` program for history walks; serde and CRC32 for the journal and the built log; histogram alignment and a line and name referee in the core | The call path's cost then grows with the change, not the tree, and a save is power-cut safe once its open's FLUSH (or an fsync, or the builder) made its records durable. `git fast-import` over libgit2 for the builder: it builds each commit's trees from path edits of its parent's, deltifies a listing rewritten per commit, streams a large blob in bounded memory, and writes one pack per batch, where libgit2 would write each object loose or hold a batch in memory. Its durability is agentfs's: fast-import installs the pack before its index, the builder fsyncs every new pack file and `objects/pack`, then fsyncs the batch's built-log entry, and only then copies refs, so nothing names an object that might be lost. Its commits are the bytes `git commit-tree` writes for the record's date in `+0000`. Refs are a copy in git's files format, written in one locked libgit2 transaction that checks every old value; fast-import's own refs (`refs/build/*`) are scratch, deleted after each install and at recovery. The `git` program stays where it does the job: `rev-list --missing=print` checks each tip at recovery, `log` rebuilds each save's commit at open. agentfs writes `config` and `HEAD` itself, because git's `config` rename has no fsync. Every git child on the actor's path starts at one counted site (`git_cli::counted`): the builder's alignment `git diff` and its `git fast-import` |
| Line identity | `agentfs-align`: git's histogram diff, called one way | The library and the model use the same call, so line identity does not depend on how `git` is called. The release of `git` still decides what a diff answers, so one release is pinned (`ci/git-release`) |
| Repack | the `git` CLI (`git repack --geometric=2`, git 2.32 or later) | libgit2 has no repack or gc. The commit loop runs it with system and global config off |
| Patch merge | the `git` CLI: `git merge-tree --write-tree --merge-base` (git 2.40 or later), merge drivers from `$GIT_DIR/info/attributes` and `merge.<name>.driver`; mergiraf in the image | libgit2 cannot register merge drivers, so git merges. `rebase` lands as one commit like `merge`, since N commits would need N records and break "held whole" on a crash between them. Before merging, each configured driver's first word must be an executable, else the patch is not judged, because git reads a driver that cannot run as a conflict. mergiraf is a syntax-aware merge driver run as its own program and never linked; the image installs a checksummed static binary, with its licence and source location in `/usr/share/doc/mergiraf` |
| Container image | `debian:trixie-slim` with `fuse3`, `libssl3t64`, `ca-certificates`, `git`, `tini` and `mergiraf`; both build and base images pinned by digest. Ships `agentfs` and `agentfs-csi` | Not distroless: the daemon runs `git repack`, and operators follow the runbooks with `git` through `kubectl exec`. `tini` is a subreaper, since the node plugin shares the host's pid namespace. It runs as root. `linux/amd64` only |
| Front and CSI gRPC | `tonic` and `prost`, on `tokio`. The front's client is `hyper`'s HTTP/2 over the Unix socket | A request `hyper` hands back unsent is known never to have left, which tonic's `channel` would hide. No TLS: the sockets are node-local. `ci/check-deps.sh` checks the default build links no ring or rustls |
| gRPC codegen | `protox` and `tonic-prost-build` in `build.rs` | A pure-Rust protobuf compiler: no `protoc` on build hosts or in the image |
| Volume ids | the `uuid` crate (version 4, random) | The standard Rust implementation of RFC 9562: parsing and the hyphenated text in `agentfs-contracts`, generation (its `v4` feature, the OS's random source) only in the state-area adapter that creates a volume |
| CSI spec | `csi.proto` v1.13.0, vendored in `crates/agentfs-csi/proto` | Pinned by copy; updating it is a deliberate change |
| CSI sidecars | csi-provisioner (`--node-deployment`, in the node plugin pod), csi-node-driver-registrar and livenessprobe from `registry.k8s.io/sig-storage` | Pinned by digest in `deploy/csi/kustomization.yaml` |

## Crash model

What a crash at each point leaves, and what the next open does. "Returned" means the workload's call returned success.

| Crash during | State left | On the next open | Failpoint |
|---|---|---|---|
| The freeze, before the journal append | A frozen copy no record names | Removed as an orphan; the call never returned, so nothing was promised | `pending.after_freeze`, `journal.before_write` |
| The journal append or its `fsync` | A torn final frame | Fails its checksum and is discarded; the call never returned | `journal.before_fsync` |
| After the journal `fsync` (call returned) | A durable record and its frozen copy | Replayed: committed by the next commit loop. Records hold absolute state, so replaying twice gives the same tree | `journal.after_fsync`, `record.after_journal`, `fuse.after_record` |
| A commit, before the branch ref moves | Loose objects only | The records are still uncommitted and are committed again; the objects are reused or left for gc | `lfs.after_cache_insert`, `commit.after_tree`, `commit.sync_objects`, `commit.after_object` |
| A commit, inside the branch ref update (libgit2 writes `<ref>.lock`, then renames it) | Maybe `refs/heads/agentfs/<volume>.lock` | Ref lock files are removed at open, with a warning (see [recording](recording.md)); the records are committed again | none (the power-loss workflow) |
| After the branch ref moves | The commit's `Agentfs-Seq` names its last record | Records up to that sequence are dropped from the journal and their frozen copies removed | `commit.after_ref`, `commit.after_sync`, `journal.truncate.after_tmp`, `journal.truncate.after_rename`, `commit.before_pending_remove`, `pending.remove.between` |
| A push | The remote either has the new ref or not; pushes are never forced | The push loop pushes again at start; a push that already landed changes nothing the second time | `push.after_remote_update`, `state.json.before_rename` |
| A front's process | Its leased slot's frozen copies; maybe a request in flight | The kernel releases the lease. A request that may have been journaled is never resent; the boundary failed with `EIO`. A restarted front passes its old slot to drain it | `front.after_reply` |
| The history service | Fronts' requests fail with `EIO` until it is back | The service reopens the volume as above; fronts reconnect | `service.after_record` |
| A restore | Some paths written, each recorded as an ordinary boundary | The partial apply is recorded and committed; re-running the restore finishes the job | none |
| A push, after the LFS upload and before the ref push | Objects on the LFS server that no pushed ref references yet | The next attempt computes the objects again from the commits the remote lacks; the batch API skips those the server already has | `push.after_lfs_upload` |
| A push that found the branch diverged, after naming the ref the local head is saved as | The name in the push state as `pending_ref`; maybe the ref on the remote | The next attempt pushes the same name again (a no-op or a fast-forward), so the head is saved once, never under a second name | `push.diverged.after_pending`, `push.diverged.after_ref` |
| A push, between the branch push and the queue push | The branch on the remote; the queue refs as they were | The queue push follows the next attempt. Inbox refs are deleted only after their applied refs are on the remote, so the highest inbox number there never goes down | none |
| A repack (`repack.rs`; also a stop, which kills it) | git writes new packs before it deletes old ones, so every object is still in some pack or loose; maybe a temporary pack file | Nothing to do: the next round repacks again and ignores the temporary file. A repack never prunes, so nothing the journal or a ref needs is lost | `repack.running` |
| The patch phase at mount, after a patch's tree write and before its record (`patch/queue.rs`) | The backing tree holds the patch's content; history does not; `meta/patch.json` names the patch in flight | The next run's check finds those paths already written and records them as the patch. If the producer replaced or withdrew the patch meanwhile, the paths are put back to the branch head's state first | `patch.after_tree_write` |
| The patch phase, after the record and before its commit | A journaled patch record | Journal replay commits it with its `Agentfs-Inbox` trailer, and the trailer scan finds it applied; it is never applied twice | `patch.after_record` |
| The patch phase, after the commit and before the ref moves | The commit on the branch; `inbox/<n>` still there, no `applied/<n>` | The next run finds the commit by its trailers and finishes the ref moves (`applied/<n>` set, `inbox/<n>` deleted): what is applied is decided by the branch, never by refs | `patch.after_commit`, `queue.after_status` |
| A namespace record that failed (`EIO`), with paths on the repair queue | `meta/repairs`, rewritten whenever the queue changes (`meta/fronts/slot-N.repairs` for a front) | The next mount (or the front reusing the slot) re-derives each path's absolute state from the backing tree and records it before it serves any request; a queue left in the temporary file (a crash before its rename) is loaded too | `repairs.before_rename` |
| The daemon of a mount in a pod | A stale FUSE mount: the workload's calls fail with `ENOTCONN`, never reaching the bare directory beneath | The kubelet restarts the sidecar, which runs `mount --on-stale-mount recover`: it takes the lock, replays the journal and bind-stacks a new mount over the stale one, which propagation carries into the workload containers; at its stop it detaches the stale layers. With `hold` it mounts nothing and the pod is restarted by hand ([runbook](runbooks/crashed-sidecar.md)) | none |
| A CSI front or service | A stale mount at the pod's target, or fronts failing with `EIO` | The supervisor restarts it with backoff (1 s to 30 s): a front in its predecessor's slot, its new mount bind-stacked over the stale target; a service replays its journal | none |
| The CSI node plugin (deleted, upgraded or crashed) | Its children stop with it (with final commits when it is stopped gracefully); the staged and published records on the hostPath | Reconcile at plugin start: services for staged volumes (journal replay), fronts recovered for targets whose mount is still there, live or stale; targets the kubelet removed are forgotten. Containers that mount with `HostToContainer` see the new mount; others after a container restart ([runbook](runbooks/csi-recovery.md)) | none |

The Failpoint column names the failpoints (`agentfs_core::failpoints`,
test builds only) that stop the process at that point; the crash
harnesses kill or fail at each of them and check
that nothing promised is lost and nothing is committed twice: the engine
matrix in `agentfs-core/tests/crash_points.rs`, the processes in
`agentfs/tests/crash_daemon.rs` and the FUSE mount in
`agentfs/tests/crash_mount.rs` (run by `ci/crash-tests.sh`). `none`:
covered by the process-level tests named in [development](development.md) instead.

The daemon never holds the only copy of anything a returned call
promised: content is in `pending/` and the journal, both `fsync`ed, until
it is in a commit.
