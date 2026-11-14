## S4. Clients and views (re-cut 2026-11-14 on the speed epic and `m1-s3-recut.md`)

**Needs:** S1, S3. **Run:** core. **Builds:** `ClientsService` (`restore`, `attach`, `rebind`, `detach`, `reconcile`,
`attached`, `attached_any`); `ClientId` as `Agentfs-Writer`; the fork writer claim; sessions over `Arc<Recording>`;
the view host as `Views`; an ended client's opens saved.

**Forced by the engine.** Sessions and the view host wrap `Arc<Recording>` (its lock holds no fsync and no wait), not
`Arc<Mutex<TreeActor>>`; three view calls run unlocked (`admit_then`, `durable_then`, `durable_all_then`); `Ordinal`
is the attach's `RecordNo`; `head()` waits for history, so opening a view never calls it (principle 5); the services
hold `Rc` (S3's `DomainEvents`), so worker threads reach the composition root by a channel. `rebind` is built: without
it a fork's claim, spent at a kill, stops the mounted conformance replay, `ci/e2e-trees.sh` and
`e2e-tree-recording-faults.sh` writing the fork they remount (clients.md §5's restart flow; C4 holds: one client, for life).

**Observable behaviour.** A save names its client, `<label>#<n>@<launcher>/<tree>`. A second read-write attach to a
fork, live or after a clean detach, is `ForkTaken`. After a kill the same launcher, label and access rebinds its
restored client: same id, its new pod context. Attach, rebind and ends are durable before they return, never wait at
the lag gate or for history, and answer while the builder fails. A detached client's opens are saved and durable
before `AttachmentEnded`; its later writes answer `EROFS`.

**Contracts (changed).**
```rust
pub trait Clients: AttachedLookup {   // + clients.md §3's rebind, with M1's pod context. Only a client restored at
    /// boot, not yet bound this run. One step: its row's `at` (`Rebound`); then bound as `attach` binds.
    /// Errors: `UnknownClient` (never attached, or ended), `NotItsLauncher`, `NotPermitted` (bound this run), `Stopped`, `NotDurable`.
    fn rebind(&mut self, client: &ClientId, at: PodContext, session: SessionId, by: &LaunchCap) -> Result<Binding, ClientError>;
}
pub enum ClientError { /* committed, + */ #[error("the volume has stopped; reopen it")] Stopped }
impl From<StoreError> for ClientError;  // NotDurable(k) -> NotDurable(k); Stopped, Damaged, Refused -> Stopped
pub trait ClientStore { /* + */ fn rebind(&mut self, client: &ClientId, at: &PodContext, by: &Principal) -> Result<(), StoreError>; }
```
Bodies: `ClientId` `Display`/`FromStr` (split at the first `#`, the first `@` after it, the first `/` after that);
`ClientLabel::parse` (`is_safe_name`, at most 64 bytes). `ClientStore::end`'s doc: errors `Stopped`, `NotDurable`;
no room is reserved, and the next boot's reconcile ends what a failed end left.

**Clients (pure).** `attach`: `NotPermitted` for a `ForksItCreates` launcher on the root or another's fork;
`TreeUnknown` for a fork not `Open`; `ForkTaken` for a read-write grant on a claimed fork; `Draining` after
`on_volume_draining` (S6). Then `store.attach`, `sessions.bind` (`Write(ClientCap::mint(.., WriterAt { writer: id text,
at }))` or `Read(tree)`), `Attached`. `detach`, `reconcile`: `sessions.unbind` each first, then one `store.end` with
the ended and each tree left empty; `AttachmentEnded`s, then `LastDetached` once per tree (C6). A failed `end` leaves the
index as it was (`reconcile` answers `[]`). **Store:** `impl ClientStore for TreeStore`, one `record_rows` step per
call on `Chain::Clients(tree)`, row `TreeClients`, `Ordinal(step.get())`, entries stamped by S3's one helper; kinds
`Attached`, `Rebound`, `AttachmentEnded` (detail: client, reason), `LastDetached`.

**Sessions (`crates/agentfs-core/src/session.rs`, new).**
```rust
#[derive(Clone)] pub struct Sessions { /* recording: Arc<Recording>, bound: Arc<Mutex<BTreeMap<SessionId, SessionAuthority>>>, next: Arc<AtomicU64> */ }
impl Sessions {
    pub fn new(recording: Arc<Recording>) -> Self;
    pub fn recording(&self) -> &Arc<Recording>;                    // the view host's takes and metrics
    pub fn open(&self) -> SessionId;                               // unique per process run
    pub fn session(&self, id: SessionId) -> Option<TreeSession>;   // None unless bound
}
impl SessionBinder for Sessions { /* bind; unbind: see below */ }
#[derive(Clone)] pub struct TreeSession { /* sessions: Sessions, id: SessionId, tree: TreeId */ }
impl TreeSession {
    pub fn tree(&self) -> TreeId;
    pub fn lock(&self) -> Result<SessionGuard<'_>, tree_actor::Error>;   // reads the binding under the actor's lock
    pub fn admit_then(&self, then: Then);                                 // this tree's lag gate
    pub fn durable_then(&self, handle: u64, then: Then);
    pub fn durable_all_then(&self, then: Then);
}
pub struct SessionGuard<'a> { /* actor: MutexGuard<'a, TreeActor>, tree: TreeId, writes_as: Option<WriterAt> */ }
// tree_actor (recording/mod.rs); `close_writer`: clippy `disallowed-methods` outside session.rs
pub fn open_by(&self, handle: u64) -> Result<(TreeId, &WriterLabel)>;
/// Every open and pending close of `writer` in `tree` closes as a save, in handle order (git-model §6).
pub fn close_writer(&mut self, tree: TreeId, writer: &WriterLabel) -> Result<Vec<Saved>>;
```
`SessionGuard`'s methods are `TreeActor`'s of the same name, receiver and result, with `tree` and `by` removed:
`read`, `file_map`, `file_len`, `files_from`, `object_at`, `path_of`, `is_anonymous`, `inode_size`, `page_fill`;
writes, `Error::ReadOnly` (new, `EROFS`) without a `ClientCap` or after unbind: `write_open`, `write_open_truncating`,
`anonymous_open`, `first_link`, `rename`, `unlink`; handle calls, `NotFound` unless `open_by` is this tree and writer:
`open_object`, `read_open`, `write`, `truncate`, `page_writeback`, `close`. `unbind`: under the actor's lock, the
binding goes and `close_writer` runs; unlocked, `durable_all`; a failure is logged, an unsaved close stays pending.
Lock order: the actor, then the bindings.

**View host (`tree_view.rs`).** `View` holds a `TreeSession` and its grant's `acts_as`; `Binding` and `serve*` go.
```rust
pub enum HostEvent { ViewEnded(ViewId), Landed }   // Landed: after each landed take, its pages dropped
impl ViewHost {
    pub fn new(sessions: Sessions, state: &Path, metrics_output: Option<&Path>,
               notify: Arc<dyn Fn(HostEvent) + Send + Sync>) -> io::Result<Self>;   // starts the take and metrics workers
    pub fn wait(self) -> io::Result<()>;   // after every view ended: joins the workers; a stopped actor is an error
}
impl Views for ViewHost { /* open, close */ }
```
`open`: `UnknownSession`; today's mount point checks (`MountFailed(EINVAL)`); the tree among the actor's saved trees
(`MountFailed(ENOENT)`); a thread joins the session, then `ViewEnded`. `close`: a lazy unmount, idempotent.

**Composition root.** `crates/agentfs/src/shared.rs` (new): `pub(crate) struct Shared<T>(Rc<RefCell<T>>)`, `Clone`,
`impl ForkLookup for Shared<TreesService<..>>`. `run` takes the `Minter` by value (the cap first, then the minter to
`ClientsService::restore`). `trees mount`: open; Trees and Clients restored on one `TreeStore`; per view (root
`--writer` or `--read-only`; fork `--sandbox-writer` or `--sandbox-read-only`) a session, then `rebind` of the
restored client of this launcher, tree, label and access (highest ordinal), else `attach`; `reconcile(live)`;
`ViewHost::open` each; on the channel, `ViewEnded(v)` → `detach`; none left → `wait`, exit 0. `ForkTaken`: "this fork
has had its writer; mount it with --sandbox-read-only or use a new fork", exit 1. Domain events drain after each call.

**Owned files.** `crates/agentfs-contracts/src/{clients,name,ports}.rs`; `crates/agentfs-clients/src/service.rs`;
`crates/agentfs-core/src/{store,session,lib}.rs`, `tree_actor/{mod,recording/mod}.rs`; `clippy.toml`;
`crates/agentfs-fuse/src/tree_view.rs`; `crates/agentfs/src/{trees_cmd,shared,main}.rs`; `tests/tree_clients.rs`
(new); `ci/e2e-trees.sh` and each `ci/` script that remounts a fork for writing after a clean unmount (a new fork or
read-only; the commit lists each); the docs below.

**Acceptance (`tests/tree_clients.rs` unless named).**

| Test | Shows |
|---|---|
| `a_second_writer_on_a_fork_is_fork_taken` (live, after a clean detach, after reopen) | C4 |
| `a_restored_writer_is_rebound_with_its_id` (undetached, reopen, rebind with another pod: same id, a save names it and the new pod; again: `NotPermitted`) | restart |
| `three_clients_killed_all_end_at_boot` (`reconcile(empty)`: three `Reconciled`, `LastDetached` once per tree) | C6 |
| `a_commit_names_its_sessions_client`; `a_read_only_session_cannot_write`; `a_session_reaches_only_its_own_handles` | C5, C3 |
| `a_detached_clients_open_file_is_saved` (its bytes saved and durable before `detach` returns) | principle 1 |
| `clients_go_on_while_the_builder_fails` (`tree.build_before_batch` error: attach, rebind, detach answer; cleared: built) | principle 5 |
| clients unit `a_fork_launcher_reaches_only_its_forks`; contracts unit `client_id_text_round_trips` | C2; the format |
| `ci/e2e-trees.sh`: kill, remount, write the fork; clean unmount, remount to write: exit 1 | end to end |

**Mutation probes.** Delete the claim at detach → `a_second_writer...`; `LastDetached` per client → `three_clients...`;
rebind as a fresh attach → `a_restored_writer...`; unbind skips `close_writer` → `a_detached_clients...`; no own-handle
check → `a_session_reaches...`; the CLI reconciles before it rebinds → `ci/e2e-trees.sh`.

**Costs.** `rebind` lands in M1 (the design deferred it); after a clean unmount a fork's writer cannot return
(clients.md cost 1). **Docs.** configuration.md (client labels, `--read-only`, `--sandbox-read-only`); docs/trees.md
(clients, writer text, claim, restart); library/core.md (`Sessions`), library/filesystem.md (`ViewHost`);
architecture.md; CHANGELOG.
