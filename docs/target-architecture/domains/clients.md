# Domain: Clients (the unbuilt parts)

**Proposed, partly built.** Attach, rebind, detach, reconcile, sessions, each fork's one writer for life and no
attach while the volume drains are built and described in [docs/trees.md](../../trees.md#clients) and the
[library](../../library/domains.md#domain-invariants), as are a launcher's bound to the forks it created and the
invariants C1 to C6. This page proposes the rest: grants with view profiles and `Sees`, a launcher's profile bound,
snapshot clients and `advance`, operator actors, and a session close that ends its client. It is one part of the system around the git model ([the system](../system.md)); the git model
([git-model.md](../git-model.md)) wins wherever the two disagree. Rust sketches show surfaces only. The commit that
lands the slice that builds these parts deletes this page and moves the behaviour into the library or operator docs.

The owner's guidance that applies here is in `OWNER-GUIDANCE.md` at the repository root. Everything else on this page
is a design choice.

| Term | Meaning here | Is not |
|---|---|---|
| **grant** | what a client may do (`Access`), see (`ViewProfileId`), and how it sees (`Sees`); built today with access, a live tree and `PerFile`. A view presents each entry's recorded owner, so a grant names none | a role |
| **launcher bound** | what a launcher may grant: the root or only forks it created (`ForksItCreates`, built), and the view profiles it may give | the grant |
| **snapshot client** | a read-only client of a pinned snapshot of a tree (Trees'), moved only by `advance` | a read-only live client (built) |
| **actor** (operator) | an authenticated operator, with roles as data | the git model's tree actor |

**Not this domain:** saves, locks and transactions (git-model), whether a host is alive (Health), forks and pinned
snapshots (Trees), what a profile shows (Rules), serving file calls (Views), volume lifecycle (Volume).

## 1. Contract additions

```rust
pub struct Grant {                 // built: access, a live tree, PerFile
    view_profile: ViewProfileId,   // minted by Rules, so it exists
    sees: Sees,                    // the one home of the consistency setting
}
pub enum Sees {
    Live { tree: TreeRef, consistency: Consistency }, // PerFile (built) | WholeLanding (views §3)
    Snapshot { of: TreeRef },      // a pinned snapshot of `of`'s head, read-only, moved only by advance
}
impl Grant {
    pub fn snapshot(of: TreeRef, v: ViewProfileId) -> Grant; // always ReadOnly
}
pub struct Binding {               // built: client, session, tree; plus
    serves: Served,                // Tree(TreeRef), or Snapshot(SnapshotId) for Sees::Snapshot
    txn: Option<TxnDir>,           // its open transaction, restored at rebind (git-model §9; not settled, ledger R5.4)
    grant: Attenuated,             // the Grant after both checks: never wider than the LaunchCap
    rules: RuleSetVersion,
}
pub trait Clients {                // built: attach, rebind, detach, reconcile, attached, attached_any
    fn advance(&mut self, c: ClientId, by: &LaunchCap) -> Result<Binding, ClientError>; // Snapshot only: a fresh pin
    fn advanced(&mut self, c: ClientId, by: &LaunchCap) -> Result<(), ClientError>;     // the view rebound: old pin released
    fn end(&mut self, c: ClientId, by: &ActorCap) -> Result<(), ClientError>;           // Operator role
    fn actor(&self, id: AuthenticatedId) -> Result<ActorCap, ClientError>;
}
pub enum AttachmentEndReason { /* built: Detached, Reconciled; plus */ SessionClosed, Operator }
```

Calls out, besides the built ones (Trees' `facts(f)` for a `ForksItCreates` attach): Trees' `pin(of, PinCap)` for a
snapshot; Rules' `view_profile`, `version`; the tree's actor for an open transaction at
`rebind`. `LastDetached` of a snapshot's last client ends the pinned snapshot (Trees).

## 2. Invariants

| # | Invariant | Enforced by | Strength |
|---|---|---|---|
| C2 (rest) | No launcher grants a profile outside its bound (the fork check is built) | `attach` builds a private `Attenuated` only after both checks; `Binding` takes only an `Attenuated` | construction time |
| C3 (rest) | A snapshot client has no `ClientCap` | `Grant::snapshot` is the only way to build `Sees::Snapshot` and sets `ReadOnly` | construction time |
| C7 (rest) | A session close while the service runs ends its client. No lease or heartbeat ends an attachment | `SessionEvents::closed` from the `rpc` adapter; no clock port in this domain | compile time |

**Ports.** `ActorDirectory` (deployment config, read-only); `SessionEvents::closed` (the `rpc` adapter: a Unix-socket
peer close ends the attachment).

## 3. Flows

**A fork launcher attaches.** `attach(live_read_write(Fork F, profile, PerFile), session, cap)`: Clients asks Trees'
`facts(F)` (created by this launcher, not ended: built), checks the profile against the launcher's bound and builds
the `Attenuated` grant before the built attach goes on.

**A snapshot advances.** `advance(c)` asks Trees for a fresh pin and returns a new `Binding`; after
`Views::rebound`, `advanced(c)` drops the old pin, publishing its `LastDetached` if it was the last.

| Policy | Current setting (design choice unless cited) |
|---|---|
| Actor directory | authenticated id → actor, roles (`Operator`; more are rows), from deployment config |
| Admission while draining | `attach` refused (built); `advance` allowed |

## 4. Tests

| Test | Rules out |
|---|---|
| A fork launcher asks for a profile above its bound: refused, no row | a launcher widening what its clients see |
| Kill a fork's view host while the service runs: `AttachmentEnded(SessionClosed)`, `LastDetached{F}`; the root keeps writing | a dead client holding opens forever |
| Compile-fail: build `ClientCap`, `ActorCap`, `Writer::Client` or a writable snapshot `Grant` in another crate | forging authority in code |

## 5. Costs

1. **A session close ends the client:** a view host restarted in place while the service runs comes back as a new
   client, a fork's on a new fork; the old fork ends, and what it saved still lands.
2. **A launcher that never returns** leaves its clients attached, so the volume never releases until an operator
   calls `end`.

## Where it meets the existing code

- **The attached mark** (`patch/attached.rs`) and **front slots and their leases** (`history_service/fronts.rs`)
  belong to the recording engine and go with it. **Peer authentication** (`SO_PEERCRED`) stays as the id `actor` and
  launchers start from.

**Not verified:** none of these parts is built or tested.
