## S5. Fork lifecycle (re-cut 2026-11-14 on the speed epic, `m1-s3-recut.md` and `m1-s4-recut.md`)

**Needs:** S3, S4. **Run:** core. **Builds:** keys, end, the drop, `settle`, `on_fork_dropped`; `--fork-key`, `trees end`.

**Forced by the engine.** A drop is a journal step of its own kind: the actor forgets the fork at once, the trailing
builder deletes its refs and, at its copy after the built event, the checkout. `drop_fork` takes `(fork, rows)` with
`RecordNo`, by `agentfs`, as S3's row steps. Its checks read the journal (S3's `pending_from`), never history: a failed
builder holds no lifecycle back (principle 5). A take of a dropped fork finds it empty; one decided before the drop is
refused at its land (`EntryGone`, built). The after-take hook is S4's `HostEvent::Landed`.

**Observable behaviour.** `create` under a key returns its open fork. An ended fork drops once its upstream's queue holds
nothing of it, no file is open in it and no fork names it, in one record with its row, durable before the store
returns. Then: no branch, queue entries or checkout; its row chain and collision refs stay; a restart recovers it
dropped. Its sync merges (no edits) become unreachable; its saves stay reachable from merges or collision refs.

**Tree actor.**
```rust
impl TreeActor {   // `drop_fork`: clippy `disallowed-methods` outside store.rs
    /// One step: `fork`'s branch and every entry of its queue deleted, with the rows `rows` builds; the fork leaves the
    /// saved trees, upstreams, pages, heads and touched keys at once. `Busy`: `pending_from(fork)`, an open or pending
    /// close in it, or `has_child(fork)`. `NotFound`: the root, or no such tree.
    pub fn drop_fork<F>(&mut self, fork: TreeId, rows: F) -> Result<()>
    where F: FnOnce(RecordNo) -> Result<Vec<RowWrite>>;
    pub fn service_at(&self) -> &PodContext;   // the `at` of an `agentfs` event no caller gives one for
}
Change::Drop { fork: String }                                   // journal/mod.rs; `build_cost` -> None, as `Rows`
pub(in crate::tree_actor) fn remove(&mut self, fork: TreeId);   // history/upstreams.rs
```
The open's replay and the builder apply `Drop`; recovery's check holds as is. `take_one` of a gone tree: `Took::Empty`.

**Store and contracts.** `ForkStore::end` (`Ended`, `ForkEnded`), `drop_fork` (`Dropped`, `ForkDropped`; `Busy` →
`Refused`), `ClientStore::release_claim` (`ClaimReleased` on `Chain::Clients(fork)`, `at` = `service_at`), each as S3's
write. `ForkKey::parse` body. `TreeError` gains `Stopped` with `From<StoreError>` as S4's `ClientError` (`Refused` in
`create` stays `UpstreamUnknown`), unless S3 settled the mapping first.

**Trees.** `create(_, Some(key), ..)`: `Open` under the key → it; `Ended`/`Dropped` → `Ended`; none → create with the
key. `end(f)`: `ForkUnknown`; `NotPermitted` unless `Root` or its creator, and for a keyed fork; `Open` → `store.end` +
`ForkEnded`, then `settle`; ended: `Ok`. `end_keyed(key)`: `KeyUnknown`; else as `end`, any launcher.
`on_last_detached(t)`: an open unkeyed fork ends as `Principal::Agentfs`; else nothing. `settle()`: each `Ended` fork
with `!pending_from && !has_child` → `store.drop_fork` + `ForkDropped`; `Refused` (an open) leaves it for the next
settle; answers the dropped. **Clients.** `on_fork_dropped(f)`: `release_claim` and the index's claim.

**Composition root.** One dispatcher in `trees_cmd.rs`, draining `DomainEvents` after each call: `LastDetached{t}` →
`trees.on_last_detached(t)`; `ForkDropped{f}` → `clients.on_fork_dropped(f)`; `HostEvent::Landed`, and boot after
`reconcile` → `trees.settle()`. stderr: `fork tree: fork/<n>`. The mounted harness creates its keyed fork through
`TreesService` and remounts its key after a crash (S4's rebind keeps the writer).

| Was | Now |
|---|---|
| `trees fork`; `--fork fork/N` | removed |
| `trees mount ROOT --sandbox DIR` | `trees mount ROOT [--fork-mount DIR [--fork-key K] [--fork-writer LABEL \| --fork-read-only]]`: the keyed fork made or returned; no key: a new unkeyed fork, ended at its last detach |
| `--sandbox-writer`, `--sandbox-read-only` | `--fork-writer`, `--fork-read-only` |
| — | `trees end --fork-key K` (`PodArgs`): `end_keyed`; `Busy`, exit 75, while a mount holds the lease |

**Owned files.** `crates/agentfs-contracts/src/{name,trees}.rs`; `crates/agentfs-{trees,clients}/src/service.rs`;
`crates/agentfs-core/src/store.rs`, `tree_actor/{mod,takes}.rs`, `tree_actor/journal/{mod,fold,commit_point}.rs`,
`tree_actor/history/{builder,built,lag,upstreams,refs}.rs`; `clippy.toml`; `crates/agentfs/src/trees_cmd.rs`;
`crates/agentfs-conformance/src/mounted.rs`; every `ci/` script using `--sandbox`, `--fork` or `trees fork`;
`crates/agentfs-core/tests/tree_store.rs`; the docs.

| Test (`tests/tree_store.rs` unless named) | Shows |
|---|---|
| `a_second_create_under_a_key_returns_its_fork` (trees unit and core) | idempotent per key |
| `an_ended_fork_drops_only_after_its_landing_is_taken` (queued: nothing; taken: dropped, branch and `refs/queue/fork/<n>/` gone, the loser's collision ref and the row chain at `ForkDropped` kept) | the drop rule |
| `a_drop_waits_for_its_open_file`; `a_drop_and_its_row_are_one_record` (kills as S3's) | principle 1; one record |
| `a_dropped_fork_recovers_dropped` (`tree.build_before_batch` error: end and drop answer; reopen, clear: no branch, queue or checkout) | principle 5 |
| `a_take_of_a_dropped_fork_is_empty`; `last_detached_ends_an_unkeyed_fork_not_a_keyed_one`; `a_dropped_forks_claim_is_released` | takes; §2; C4 |
| the mounted conformance replay and `ci/power-loss-trees.sh` on `--fork-mount`, crashes included | harnesses moved |

**Mutation probes.** Ignore the key → `a_second_create...`; skip `pending_from`, or keep the fork's queue entries →
`an_ended_fork_drops...`; no `Upstreams::remove` → `a_dropped_fork_recovers...`; skip the open check →
`a_drop_waits...`. **Docs.** configuration.md; docs/trees.md (keys, end, drop, `Drop`); domains/trees.md keeps only
pinned snapshots, storage and quotas; CHANGELOG.
