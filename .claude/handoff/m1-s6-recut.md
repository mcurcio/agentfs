## S6. Volume lifecycle (re-cut 2026-11-14 on `m1-s3-recut.md`, `m1-s4-recut.md` and `m1-s5-recut.md`)

**Needs:** S3, S4, S5. **Run:** core. **Builds:** as `m1-pages.md` S6: `stage`, `unstage`, the Stop drain, `decide` and
`verdict`, `rejudge`, `VolumeStore` and `QueueFacts` on the state area; settlement `NoRemotes` until M4; SIGTERM
unstages; Clients' `on_volume_draining` / `on_volume_staged`.

**What stands.** The Volume rules (`stage`, `unstage`, `rejudge`, `release_verdict`, `decide`'s blocker order), the
acceptance tests, the mutation probes and the docs of the old page. `TreeStateArea` implements `VolumeStore` and
`QueueFacts` by delegating to the `TreeStore` it makes at `open` over its `Arc<Recording>` (before: `load` →
`Ok(None)`, `record` → `Err(Stopped)`, `queued` → empty); `record` is one `record_rows` step on `Chain::Volume`.

**What changes, and why.**
- **No `close_writer` in the root.** A drain's detach saves each client's opens inside `Sessions::unbind` (S4), before
  its `AttachmentEnded`. `VolumeDraining` → `clients.on_volume_draining()`, then the CLI detaches its clients and
  closes its views (S4's lazy unmount).
- **Every trigger is an event on the root's one channel.** `RootEvent { Host(HostEvent), Stop }`; the `ViewHost`'s
  `notify` wraps `HostEvent`; a `signal_hook::iterator::Signals` thread sends `Stop` on SIGTERM or SIGINT (never a
  polled flag). `Stop` → `unstage`; `HostEvent::Landed` → `trees.settle()` (S5), then `volume.rejudge()`;
  `AttachmentEnded` and `LastDetached` → `rejudge()`; `VolumeReleased` → leave the loop, `ViewHost::wait`, exit 0.
- **The boot order is S4's.** `stage` (replacing the direct open), restore, rebind or attach each view, then
  `reconcile(live)`, then `trees.settle()` (S5): never `reconcile(&ClientSet::new())` first, which would end the
  clients a restart rebinds.
- **`C` for `VolumeService`** is `Shared<Option<ClientsService<..>>>` (S4's `shared.rs`), `AttachedLookup` answering
  an empty set while `None`, filled after `stage`.
- **Queues trail the builder.** `queued` is read from the journal (S3), but a queued entry is taken only once built,
  so a failed builder keeps `QueuesNotEmpty`: the drain waits, recording goes on, the process stays up with the
  builder's alarm, and Kubernetes' grace period ends it with nothing recorded lost (principle 5; a cost, stated in
  docs/trees.md).

**Frozen signatures.**
```rust
// crates/agentfs/src/trees_cmd.rs (binary-local)
enum RootEvent { Host(agentfs_fuse::tree_view::HostEvent), Stop }
// crates/agentfs/src/shared.rs (S4's file)
impl<S, B, F, E> AttachedLookup for Shared<Option<ClientsService<S, B, F, E>>>
where S: ClientStore, B: SessionBinder, F: ForkLookup, E: EventSink;
```
Every other S6 signature is the committed code's (`VolumeService`, `release.rs`) and the old page's.

**Owned files.** As the old page, plus `crates/agentfs/src/shared.rs`; `crates/agentfs-core/src/store.rs`
(`VolumeStore`), `state_area.rs`; `Cargo.toml` and `crates/agentfs/Cargo.toml` (`signal-hook`), its row in
architecture.md's Technology choices.

**Acceptance.** The old page's table, with one change: `a_lifecycle_move_and_its_row_are_one_record` kills at S3's
points (`tree.before_step`: neither; `tree.wal_after_payload`: both, unbuilt). Added:
`a_drain_goes_on_while_the_builder_fails` (core: unstage with the builder failing at `tree.build_before_batch`, a
queued landing: `Draining`, nothing released; cleared: taken, `Released`).

**Mutation probes.** The old page's two, plus: reconcile before rebinding at boot → `ci/e2e-trees.sh` (S4's probe,
which must stay red with stage in front); SIGTERM handled by a flag the loop polls → the SIGTERM test hangs past its
bound (no event wakes the loop).

**Docs.** As the old page; docs/trees.md also states the drain's wait on a failed builder.
