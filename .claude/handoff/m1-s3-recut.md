## S3. Store: volume events as commits beside their trees (re-cut 2026-11-14 on the speed epic, d9d1fdd)

**Needs:** S1, the speed epic as built. **Run:** core (phase-1's regression tier on the push). **Builds:** rows in
the step, their commits in the builder; `store`; Trees' `restore`, unkeyed `create`, `facts`, `live`; every fork
through `Trees::create`, its `ForkCreated` in the branch's step.

**Observable behaviour.** A fork's create is one journal record (branch and row), durable before the store returns
(one group-commit fsync, off the actor's lock). Its commit on `refs/agentfs/trees/fork/<n>` trails, built with the
step's batch, as a save's is. Rows are read from the actor's saved state, never Git: they answer while the builder lags
or fails, and a row step never waits at the lag gate. At restart: built chains' `row.json`, then later steps replayed.

**Records (state format `agentfs-trees-10`).**
```rust
Entry::Step { by: WriterAt, change: Change, pointers: Vec<PointerChange>,
              #[serde(default, skip_serializing_if = "Vec::is_empty")] rows: Vec<RowRecord> }
Change::Rows        // rows alone; `build_cost` -> Result<Option<(TreeId, u64)>>, None for Rows
/// One event commit the build writes, in order: `chain` its ref (`refs::row_chain`), `row` the JSON text.
pub(super) struct RowRecord { pub chain: String, pub row: String, pub kind: String, pub detail: String,
                              pub by: String, pub at: PodContext }
// history/built.rs: wakes the builder; None adds to no tree's lag
pub(in crate::tree_actor) fn appended_step(&self, step: RecordNo, cost: Option<(TreeId, u64)>);
```

`record_step` makes each chain's `PointerChange` (`new` = `Target::Step(this)`) after `before_record`; no caller
passes one. `Step(n)` on a row ref: step `n`'s last commit on that chain; elsewhere its head. `compacted` skips rows.

**Tree actor.**
```rust
// tree_actor/rows.rs (new; public types re-exported). Both enums: Clone, Copy, Debug, Eq, Ord, Hash.
pub enum RowDomain { Volume, Trees, Clients }
/// `refs/agentfs/volume`, `refs/agentfs/trees/fork/<n>` (kept after the drop), `refs/agentfs/clients/<tree>`.
pub enum Chain { Volume, Trees(ForkId), Clients(TreeId) }      // impl: pub fn domain(self) -> RowDomain
pub struct RowWrite { pub chain: Chain, pub row: String, pub event: RowEvent }
pub struct RowEvent { pub kind: &'static str, pub detail: String, pub by: String, pub at: PodContext }
pub(in crate::tree_actor) struct SavedRows(BTreeMap<Chain, String>);   // one apply: the call and the replay
impl SavedRows { pub fn apply(&mut self, rows: &[RowRecord]) -> Result<()>; }

impl TreeActor {   // each step's writer: `agentfs`, the service's pod context
    /// A fork of `upstream` at its saved head with the rows `rows` builds; replaces `fork()`. `NotFound`: no upstream.
    pub fn fork_with<F>(&mut self, upstream: TreeId, rows: F) -> Result<TreeId>
    where F: FnOnce(TreeId, RecordNo) -> Result<Vec<RowWrite>>;
    pub fn record_rows<F>(&mut self, rows: F) -> Result<RecordNo>          // one `Change::Rows` step
    where F: FnOnce(RecordNo) -> Result<Vec<RowWrite>>;
    pub fn row(&self, chain: Chain) -> Option<String>;
    pub fn rows(&self, domain: RowDomain) -> Vec<(Chain, String)>;
    pub fn queued(&self) -> Vec<TreeId>;                // queue facts from the journal's pointers, no wait
    pub fn pending_from(&self, fork: TreeId) -> bool;   // was `#[cfg(test)]` and waited for history
    pub fn has_child(&self, fork: TreeId) -> bool;
}
```

`pending_from` reads each entry's source tree, kept by the actor: set by `publish`, replayed after the built point,
read at open from built entries' `Agentfs-Tree`. `fork_with`, `record_rows`: clippy `disallowed-methods` outside
`store.rs`. **Builder.** `CommitSpec.tree` becomes `on: BuildRef` (`Tree(TreeId)` | `Rows`: `refs/build/<tree>`,
`refs/build/rows`); empty `parents` only on `Rows`, after a `reset`. `format.rs`: `ROW_FILE = "row.json"`,
`row_edits(&str) -> Vec<Edit>` (`DeleteAll`, `row.json` inline). `message.rs`: `Message::Row { row: &RowRecord,
chain: Chain, event: EventNumber }`, §0.4's format. After a step's change, each `RowRecord` in order is a commit over
its chain's pointer in the batch (none: a root commit), dated by the record's `at_ns`.

**Store adapter (`crates/agentfs-core/src/store.rs`, new).**
```rust
#[derive(Clone)] pub struct TreeStore { /* recording: Arc<Recording> */ }
impl TreeStore { pub fn new(recording: Arc<Recording>) -> Self; }
impl ForkStore for TreeStore { /* S3: load, create; S5: end, drop_fork */ }
impl QueueFacts for TreeStore { /* the actor's */ }   // S4: ClientStore; S6: VolumeStore, via TreeStateArea
```

A write: under `Recording::actor()`, read the chain's row, stamp, run the step; unlock; `Recording::durable_all()`.
Stamping, its one home: a first write sets `created = changed = Stamp { at: now, by }`; later ones keep `created`.
`StoreError`: `Stopped` → `Stopped`; `Io` → `NotDurable` (nothing recorded); `Durable` → `Stopped` (reopen decides if
the step survived); `Format`, `CorruptJournal`, `MissingObject`, `Json` → `Damaged`; `NotFound`, `Busy`,
`PointerMoved` → `Refused`. A stopped actor's `QueueFacts` answer empty and `false`: what follows fails `Stopped`.

**Trees.** `restore`, `facts`, `live` (not `Dropped`), unkeyed `create` (`NestedNotBuilt`; `UpstreamUnknown` from
`Refused`; `ForkCreated` after the store returns; `by` Root or the creator); `Some(key)`: `todo!()` until S5.

**Trial CLI, conformance, tests.** `trees fork`, and `trees mount --sandbox` without `--fork`, call
`Trees::create(Root, None, ForkSettings { landing: Automatic }, cap)` on a `TreeStore`; events go to
`crates/agentfs/src/domain_events.rs`: `#[derive(Clone, Default)] pub(crate) struct DomainEvents(Rc<RefCell<VecDeque
<Event>>>)`, `impl EventSink`, `fn next(&self) -> Option<Event>`. `mounted.rs` forks through `TreeStore` +
`TreesService` (its replays carry a row chain); `Library`, a bare `TreeActor`, forks rowless. History walks use
`--exclude=refs/agentfs/* --all`. `fork()`'s 37 sites in agentfs-core use a rowless test helper.

**Owned files.** `crates/agentfs-core/src/tree_actor/{mod.rs, rows.rs, takes.rs, journal/{mod,fold,commit_point}.rs,
history/{builder,built,feed,format,message,refs,mod,take}.rs}`, `store.rs`, `lib.rs`; `clippy.toml`;
`crates/agentfs-trees/src/service.rs`; `crates/agentfs/src/{trees_cmd,domain_events,main}.rs`;
`crates/agentfs-conformance/src/{library,mounted,invariants,git_history}.rs`; `crates/agentfs-core/tests/{tree_store
(new),tree_actor,tree_line_rules,tree_pages,tree_stages,support/trees}.rs`; the docs below.

**Acceptance (`tests/tree_store.rs` unless named).**

| Test | Shows |
|---|---|
| `a_fork_and_its_row_are_one_record` (kill at `tree.before_step`: neither; at `tree.wal_after_payload`: both, unbuilt) | one commit point |
| `an_event_is_durable_before_create_returns` (`journal_fsync` covers the record) | power-cut safe |
| `a_row_chain_is_a_commit_chain_of_events` (root commit, `ForkCreated`, §0.4's trailers, `Agentfs-Event: <n>`, tree `row.json` = `row()`) | on record in Git |
| `a_failed_builder_stops_no_event` (`tree.build_before_batch` errors: create, `row`, `restore` answer; cleared, built) | principle 5 |
| `a_restart_builds_the_rows_it_had_not_built` (kill unbuilt; reopen: `row()` at once; one commit, same oid twice) | recovery |
| `created_survives_a_change_and_a_compaction` (`store.rs` unit: two writes, `settle`, reopen compacts) | the stamps |
| `queue_facts_come_from_the_journal` (a fork's landing queued, builder failing: `pending_from` true; built, reopened: true; taken: false) | `QueueFacts` |
| trees unit `create_twice_unkeyed_makes_two_forks`, `nested_is_refused`; `a_row_message_is_its_literal_text` | Trees; format |

**Mutation probes.** Row in its own step → `a_row_chain...` (`Agentfs-Event` is `<n+1>`); restamp `created` →
`created_survives...`; skip `durable_all` → `an_event_is_durable...`; a row ref's `Step(n)` read as the head →
`a_row_chain...`; gate row steps → `a_failed_builder...`. **Figures:** none new; budgets in `ci/trees-budget.json` hold.

**Docs.** `docs/trees.md` (rows, event commits, restart); git-model §4 (row refs, pushed from M4) and §12's table;
`architecture.md`, `library/core.md` (`TreeStore`, `rows.rs`); CHANGELOG (format 10). This page overrides §0.2's
`refs/agentfs/objects/` and "row commit in the step's `build`", and §0.4's "format 4".
