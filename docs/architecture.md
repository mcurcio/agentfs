# Architecture

How the code is organised and how a change travels from a write to a
remote. [design.md](design.md) is the specification and says *why*; this
page says *where*. Section references (§) are to design.md.

## Crates

| Crate | Kind | What it is |
|---|---|---|
| `agentfs-core` | library | The engine: path rules, the journal, frozen copies, commits, pushes, restores, the patch queue, the history service, LFS, telemetry. No FUSE and no CLI |
| `agentfs-fuse` | library | The FUSE layer: a passthrough filesystem over the backing tree that calls the engine at every change boundary (§2.1, §3.1, §6) |
| `agentfs` | binary | The `agentfs` command (§17.1): `init`, `mount`, `run`, `record`, `flush`, `push`, `status`, `probe`, `serve-history`, `restore`, `revert`, `patch`; OpenTelemetry export |
| `agentfs-csi` | library and binary | The CSI driver (§14.2): `agentfs-csi --mode controller\|node\|all`, which supervises `agentfs serve-history` and `agentfs mount` children |

The dependencies run one way: `agentfs-csi` → (`agentfs` as a child
process) → `agentfs-fuse` → `agentfs-core`. `agentfs-csi` links
`agentfs-core` only to create state areas and read their status; it never
records.

## Module map

Every module of every crate, one line each. A test
(`crates/agentfs-core/tests/docs_reference.rs`) fails when a module is
missing from these tables or a row names a module that no longer exists;
`tests.rs` files are not listed.

### `agentfs-core`

| Module | Role |
|---|---|
| `lib.rs` | Crate root: the module list and the engine's overview |
| `volume.rs` | Opens a state area: crash recovery, then the [`Recorder`](#the-synchronous-step) used at every boundary and the `Committer` that turns records into commits |
| `record.rs` | What the journal stores: one record per change boundary, absolute state, never deltas (§3.1, §3.3) |
| `journal.rs` | The durable change log: framed, checksummed records with group-commit `fsync` (§3.2, §3.3) |
| `pending.rs` | Frozen copies in `pending/`: `FICLONE` reflink, else copy plus `fsync`; slot-numbered ids for fronts (§3.2 step 1) |
| `claims.rs` | Per-path ordering: a record claims its paths from decision to tracked-set update (§3.2, §4) |
| `tracked.rs` | The tracked set, git's index in agentfs: branch head plus durable records (§4) |
| `rules.rs` | Path rules: recorded or skipped, and why, from `info/exclude`, `info/attributes` and optionally tree rules (§4, §5) |
| `nested.rs` | Detects nested git repositories, including ones being created (§5) |
| `probe.rs` | Bounded, non-blocking reads of small workload files (`.gitignore`, `.git` files) during the synchronous step |
| `paths.rs` | Repository-relative paths and the names git refuses to store |
| `checks.rs` | git fsck rules and remote limits, checked at record time and again at commit (§3.4) |
| `gitmodules.rs` | A port of git fsck's `.gitmodules` rules (§3.4) |
| `fsync_class.rs` | The `fsync` class for databases: membership and groups (§8) |
| `history.rs` | The bare history repository: records to commits with `Agentfs-*` trailers (§3.3, §3.4, §9) |
| `identity.rs` | Committer and author identities from templates (§9) |
| `service.rs` | The background loops: commit loop, one push loop per remote, repack (§3.4, §11.2) |
| `push.rs` | Pushing the volume's own refs to each remote, with per-remote state and backoff (§11) |
| `remote_auth.rs` | Credentials and host-key checks for one remote, shared by push and patch fetch (§11.1) |
| `remote_url.rs` | Pure URL helpers: transport, host, port, userinfo, as libgit2 sees them (§11.1) |
| `credentials.rs` | Credential and SSH key files for remotes (§11.1) |
| `known_hosts.rs` | OpenSSH known_hosts parsing and matching (§11.1) |
| `watched.rs` | Files re-read when their stat stamp changes: credentials, keys, known_hosts |
| `repack.rs` | Periodic `git repack --geometric=2` of the history (§8) |
| `restore.rs` | Restore and revert: plan from history, check, apply to a tree (§15) |
| `confined.rs` | `openat2`-confined access to the tree for restores (§6, §15) |
| `operations.rs` | Restores and reverts in progress: their writes become one operation commit (§15) |
| `lock.rs` | The state-area lock, an `flock` on `meta/lock` (§2.3) |
| `local_socket.rs` | Unix sockets in the state area with peer-credential checks, for the control socket and the history service |
| `fsutil.rs` | Small helpers for the state files under `<state>/meta` |
| `config.rs` | Operator settings from the history repository's own git config, and `config::KEYS` (§4.3, §11.1) |
| `events.rs` | Alarm and event signals, handed to a non-blocking sink (§13) |
| `telemetry.rs` | OpenTelemetry instruments and spans over the API only; the binary picks exporters (§13) |
| `history_service/mod.rs` | One history service per volume fed by fronts (§14) |
| `history_service/protocol.rs` | The front ↔ service wire protocol: length-prefixed JSON frames, v1 |
| `history_service/server.rs` | The service's socket: authentication, leases, one thread per connection |
| `history_service/client.rs` | A front's pooled, never-resending connections to the service (§3.5) |
| `history_service/lease.rs` | A front's `flock` lease on its pending slot (§2.2, §14) |
| `lfs/mod.rs` | Git LFS (§7): large content as LFS objects with pointer blobs in history |
| `lfs/pointer.rs` | The canonical LFS pointer format |
| `lfs/policy.rs` | Which store a recorded file goes to: blob or LFS (§7, §4.2) |
| `lfs/attributes.rs` | The managed `filter=lfs` block in the root `.gitattributes`, in history only |
| `lfs/cache.rs` | The local LFS object cache, `<state>/lfs/objects` |
| `lfs/endpoint.rs` | Each remote's LFS endpoint and credentials, in git-lfs's precedence |
| `lfs/client.rs` | The batch API client (basic transfer) and the standalone-file transfer |
| `lfs/upload.rs` | Which LFS objects a push must upload first |
| `lfs/fetch.rs` | LFS objects for restores: cache first, then the remotes |
| `lfs/fake.rs` | An in-process LFS server for tests |
| `patch/mod.rs` | The patch queue for iac (§12) |
| `patch/submit.rs` | `agentfs patch submit`: the producer side, driving the producer's own `git` |
| `patch/fetch.rs` | Fetching the inbox refs from the patch remote, best effort |
| `patch/queue.rs` | Applying the queue at mount, before the workload sees the tree |
| `patch/status.rs` | The status ref iac reads, and the local cache in `meta/patch.json` |

### `agentfs-fuse`

| Module | Role |
|---|---|
| `lib.rs` | Crate root and overview |
| `ops.rs` | Every filesystem operation without the kernel, unit-tested against a real directory and state area |
| `fuse.rs` | The kernel adapter: `fuser::Filesystem` over `Ops`, and kernel passthrough negotiation |
| `boundary.rs` | Recording a change boundary: builds the inputs and calls the recorder, or a front's client (§3.1, §3.2, §3.5) |
| `dirty.rs` | Whether an inode changed since it was last recorded, so clean closes record nothing |
| `inodes.rs` | The inode table: FUSE node ids, backing inodes and their paths |
| `names.rs` | What a path in the mount is, for recording (§5) |
| `backing.rs` | Confined access to the backing tree (§2.1, §6) |
| `groups.rs` | fsync-class groups: holding writes while a group is captured, and scheduling captures (§8) |
| `sweep.rs` | The periodic sweep: files held open, failed records, the fsync-class timer (§3.1, §3.5) |
| `privileges.rs` | What the daemon may do as the caller or as itself (§6) |
| `mount.rs` | Layout checks, mounting, unmounting, and reading mount state from `/proc/self/mountinfo` |

### `agentfs`

| Module | Role |
|---|---|
| `main.rs` | The CLI (`Cli`), command dispatch, `init`, `status`, `record`, `flush`, `push`, `run` and `mount` |
| `daemon.rs` | Signals and shutdown for `run` and `mount` |
| `front.rs` | `mount --history-service`: a front of a history service (§14) |
| `serve_history.rs` | `agentfs serve-history`: the history service of a volume (§14) |
| `hold.rs` | `mount --on-stale-mount hold`: a restarted sidecar waiting out a stale mount |
| `control.rs` | The control socket of a running mount, through which `restore` and `revert` act (§2.2, §15) |
| `recovery.rs` | `agentfs restore` and `agentfs revert`, through a mount or directly (§15) |
| `patch_cmd.rs` | `agentfs patch` and the mount's patch phase (§12) |
| `otel.rs` | Telemetry export: the OpenTelemetry SDK, exporters and the environment it reads (§13) |
| `doc_tables.rs` | Test only: a small markdown reader for the documentation tests |
| `reference_tests.rs` | Test only: checks docs/configuration.md against the CLI's flags and variables |

### `agentfs-csi`

| Module | Role |
|---|---|
| `lib.rs` | Crate root, the driver name and overview |
| `main.rs` | The `agentfs-csi` command (`Cli`) |
| `csi.rs` | The CSI v1 protocol, generated at build time |
| `server.rs` | Serving the CSI services on a Unix socket |
| `identity.rs` | The CSI Identity service |
| `controller.rs` | The Controller service: CreateVolume, DeleteVolume, ControllerPublish |
| `node.rs` | The Node service: validates calls and hands them to the supervisor |
| `supervisor.rs` | One `serve-history` per staged volume and one front per pod, restarted when they crash |
| `launcher.rs` | Starting the `agentfs` children |
| `mounter.rs` | The node plugin's mount operations: bind mounts onto the kubelet's targets |
| `params.rs` | StorageClass and PersistentVolume parameters |
| `volume.rs` | Volumes on disk: creating one, adopting a static PV |
| `records.rs` | The driver's persisted records (`csi.json`) |
| `layout.rs` | Where the driver keeps things |
| `error.rs` | Errors mapped onto gRPC status codes |

## Data flow

### The synchronous step

What happens before a change boundary's call (`close` after a write,
`unlink`, `rename`, `fsync` in the fsync class, ...) returns to the
workload (§3.1, §3.2). Its cost is measured in
[performance.md](performance.md).

```text
workload ── close(2) ──▶ kernel FUSE ──▶ fuse.rs ──▶ ops.rs
                                                     │ dirty.rs: changed since last record?
                                                     ▼
                                               boundary.rs
                                                     │
                       ┌─────────── single process ──┴── front (§14) ──────────────┐
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

A record that cannot be made durable fails the call with `EIO` (§3.5); a
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
one commit (`operations.rs`); without a mount they write the backing tree
directly through `confined.rs` and record in-process.

## Crash model

What a crash at each point leaves, and what the next open does (§3.3,
§15). "Returned" means the workload's call returned success.

| Crash during | State left | On the next open |
|---|---|---|
| The freeze, before the journal append | A frozen copy no record names | Removed as an orphan; the call never returned, so nothing was promised |
| The journal append or its `fsync` | A torn final frame | Fails its checksum and is discarded; the call never returned |
| After the journal `fsync` (call returned) | A durable record and its frozen copy | Replayed: committed by the next commit loop. Records hold absolute state, so replaying twice gives the same tree |
| A commit, before the branch ref moves | Loose objects only | The records are still uncommitted and are committed again; the objects are reused or left for gc |
| After the branch ref moves | The commit's `Agentfs-Seq` names its last record | Records up to that sequence are dropped from the journal and their frozen copies removed |
| A push | The remote either has the new ref or not; pushes are never forced | The push loop pushes again at start; a push that already landed changes nothing the second time (§11.2) |
| A front's process (§14) | Its leased slot's frozen copies; maybe a request in flight | The kernel releases the lease. A request that may have been journaled is never resent; the boundary failed with `EIO`. A restarted front passes its old slot to drain it |
| The history service | Fronts' requests fail with `EIO` until it is back | The service reopens the volume as above; fronts reconnect |
| A restore | Some paths written, each recorded as an ordinary boundary | The partial apply is recorded and committed; re-running the restore finishes the job (§15) |

The daemon never holds the only copy of anything a returned call
promised: content is in `pending/` and the journal, both `fsync`ed, until
it is in a commit.
