# agentfs target architecture: the system around the git model

**Layer:** **System (domains, contracts, flows)** › Domain (one page each) › Feature.
**Status:** revision 11, proposed: the target, not today's code; sketches are Rust surfaces. Revision 3 was approved
by the owner on 2026-11-02; revision 10 cuts the page to what [agentfs in git terms](git-model.md) does not cover.
**Read [git-model.md](git-model.md) first.** It is the one source of truth for trees as branches, saves, locks, the
queue and its referee, collisions, syncs, transactions, nested forks, history settings and crash recovery.
This page and the domain pages cover only the rest: clients, views and what each shows, tree storage and lifecycle,
replication, the volume, health, telemetry, deployment and versions. Where a page here seems to disagree with
git-model.md, git-model.md wins and the page has a bug.

## 0. The owner's words, and where each is designed

Nothing else on these pages is his; everything else is a design choice and says so.

| # | The owner's words | Date | Designed in |
|---|---|---|---|
| R1 | "I want the DDD to target the *ideal* design, and not just the easy one. It should be unbiased on the current shape of the code, and instead design the shape that makes the most functional sense — treating each domain as a hard boundary with strict contracts." | 2026-11-01 | every page |
| R2 | "a new set of architecture docs which lay out the overarching system architecture first, and then a per-domain doc of the domain architecture"; "Give the high level architecture first. I will review that before we waste time writing each domain doc" | 2026-11-01 | this set |
| R3 | "a virtual filesystem overlay for each client (including main) that is backed by a _real_ filesystem source-of-truth" | 2026-11-01 | [views](domains/views.md), [trees](domains/trees.md); git-model §3 |
| R4 | "the sandbox fs edits need to land back in the main container, and then future sandboxes should start from the edited files" | 2026-11-01 | git-model §5–§7; [trees](domains/trees.md) §2 |
| R5 | "Agentfs history shouod distinguish between the various containers so we can track direct edits on main vs the forks that sandboxes edit" | 2026-11-01 | git-model §3 (trailers); [clients](domains/clients.md) |
| R6 | "THE EDITS CAN LAND IN ONE COMMIT?!?" (read as: no commit holds two writers) | 2026-11-01 | git-model §3 |
| R7 | "Security boundary. Agentfs is the gatekeeper that exposes only what we want sandboxes to see" | 2026-10-30 | [rules](domains/rules.md), [views](domains/views.md), [clients](domains/clients.md) |
| R8 | a consumer's personal information never enters the agentfs repo (his exact words name the consumer, so they stay in the session notes) | 2026-10-30 | these pages name no consumer |
| R9 | "Whole seems best, right? A partial application has unknown side effects" | 2026-10-16 | git-model §6 (a take holds all its files); git-model §7: a save or transaction lands whole or loses whole (rule 3e, the owner's answer of 2026-11-03 to git-model §16 question 3) |
| R10 | "agentfs should support merging live" (2026-10-28); "No live landing yet", then "Build incrementally" (2026-10-29) | 2026-10-28/29 | git-model §5 |
| R11 | "Nothing is locked in. Everything is on the table" | 2026-11-01 | every page |
| R12 | "The general history should be preserved, yes. But I think it could be configurable if it needs to be every single commit, or whether a fork can be squashed so that mainline is just a merge tree of squashed fork lineage." | 2026-11-02 | git-model §11 |
| R13 | "A cooperating integration can define batches/transactions across files, and they must all land together. But a default usage, like the CNI provides, can operate per-file." (read: "CNI" is agentfs's CSI driver) | 2026-11-02 | git-model §7, §9: a transaction's commit lands whole or loses whole at every level (rule 3e); [views](domains/views.md) |
| R14 | "Not A, because that assumes that a clean merge is an acceptable merge" (quoted in agentfs `docs/design.md` §12) | 2026-10-16 | not yet designed (§5) |
| R15 | "Configurable with a sensible default" (of accepting a clean merge; `docs/design.md` §12) | 2026-10-29 | not yet designed (§5) |
| R16 | "i would like for the git history to remain compatible across minor versions, and only break on a major version change. The various agentfs components should be within the same version family (major for sure, maybe minor too, patch doesnt need to match)" | 2026-11-02 | §4 below |
| R17 | "no no no, dont go back on the fork design. thats a cleaner model. Im not arguing for one shared backend fs -- im arguing for making the fork design so rock-solid that it is almost as effective, by syncing forks quickly and efficiently to minimize conflict drift" | 2026-11-02 | git-model §8 |
| R18 | "If the changes are passing bi-directionally, then keeping the two trees relatively in-sync should minimize conflicts, right? Ideally, the two conflicting changes would be interleaved in commits, and both clients would see the others changes -- this would be the ideal parallel of a real, local filsystem, and those clients dont need to know that its all virtualized and backed by git." | 2026-11-02 | git-model §7–§8 |

**His answers still in force** (current choices, not rules):

| # | His answer | Date | Designed in |
|---|---|---|---|
| A1 | Main is a view like any sandbox | 2026-11-01 | [views](domains/views.md) |
| A2 | A sandbox's client is its own view mount, made by whoever starts the container | 2026-11-01 | [clients](domains/clients.md) |
| A3 | Sandbox saves land live ("continuous"); integrator patches stay "whenDetached" until he opens them | 2026-11-01 | sandbox saves: git-model §5; patches: not yet designed (§5) |
| A4 | "A fork whose branches are rejected and never land in the main branch will inevitably be lost, and that's acceptable" | 2026-11-01 | replaced for losing changes by "yes, \"losing\" patches should still be part of the history. i want to know who changed what when" (2026-11-03, git-model §4); still holds for a fork's whole line ([replication](domains/replication.md) §4) |
| A5 | Consistency: "It depends on the config!" | 2026-11-02 | [views](domains/views.md) §3 |
| A6 | A pinned fork is an acceptable snapshot | 2026-11-02 | [trees](domains/trees.md), [views](domains/views.md) |
| A7 | History setting: "one setting per volume" | 2026-11-02 | git-model §11 |
| A11 | File watchers: "we can take the "no guarantee" path for now" | 2026-11-02 | [views](domains/views.md) |
| A12 | "Yes, I think so. The sandboxes are generally peers and parallelism is assumed." | 2026-11-02 | git-model §5 |
| A13 | "Agreed. It’s a transaction, so a failure is an implicit rollback." | 2026-11-02 | git-model §9, §12 |
| A15 | "a transaction starting effectively creates a new index —maybe even an implicit branch in the local history" (in full: git-model §0) | 2026-11-02 | git-model §9 |
| — | **Close-to-open visibility accepted:** "Go", in reply to the recommendation to accept close-to-open visibility (other clients see a file's new content only once it is closed and saved) | 2026-11-03 | [views](domains/views.md) §2 |

A9's words ("freezing only the files that a landing touches sounds reasonable", 2026-11-02) still bind and are built
as the git model's locks (git-model §0, §6). A10's resolver chain and A14's `Lock` setting are replaced by the git
model's own answers (§7, §9). A8, his accepted question "This does assume that a *commit* is landed altogether,
not just a file" (2026-11-02), is met: a save (the whole file at one close) or a transaction's commit lands whole
or loses whole (git-model §7, rule 3e; his answer of 2026-11-03 to git-model §16 question 3).

## 1. The system in one picture

The git model sits in the middle: one repository per volume, one branch and one actor per tree, one queue per tree.
The domains on this page surround it and reach it only through the contracts in §2.

```mermaid
flowchart TB
  LA["launchers: sidecar, CSI node plugin, in-pod launcher"]
  OP["operators"]
  RM[("git remotes")]
  subgraph VHS["view hosts: never inside the client they serve"]
    V["Views"]
  end
  subgraph HS["history service: one per volume"]
    CL["Clients"]
    G["the git model: each tree's actor, queue, referee (git-model.md)"]
    TR["Trees: storage and lifecycle"]
    RP["Replication"]
    VO["Volume"]
  end
  HE["Health: outside what it judges"]
  RU["Rules: pure policy"]
  LA -->|attach, detach, reconcile, advance| CL
  LA -->|create, end a fork| TR
  LA -->|open, rebound a view| V
  LA -->|create, stage, unstage| VO
  OP -->|push, replace diverged, drain, accept start| RP & VO
  V -->|write-open, path calls, save at close, transactions: over its session| G
  V -->|reads the tree's working tree| TR
  G -->|TreeMoved| V & RP & VO
  CL -->|AttachmentEnded, LastDetached| G & TR & VO
  TR -->|branch created, dropped| G
  RP -->|push, fetch| RM
  HE -.->|probes| V & HS
  RU -.- V & G & CL
```

Telemetry receives every event and is left off the picture.

| Domain | Owns exclusively | Is not | Page |
|---|---|---|---|
| **the git model** | each tree's branch, index and working tree; saves, locks, the queue, the referee, syncs, transactions, collision refs, grafts, history settings | anything a client sees or who a client is | [git-model.md](git-model.md) |
| **Rules** | what is recorded, what each view profile shows, database groups | storage, mounts | [rules](domains/rules.md) |
| **Clients** | clients, writers, grants, sessions, attachments, every capability | liveness, what lands | [clients](domains/clients.md) |
| **Views** | serving each client's file calls, open files until close, kernel caches, what each consistency setting shows, forwarded file locks | ordering, merging, history | [views](domains/views.md) |
| **Trees** | where a tree's files live (working trees, pinned snapshots), confinement, space limits, when a fork is created, ends and drops | branches and commits (the git model) | [trees](domains/trees.md) |
| **Replication** | remotes, credentials, pushes (the one pusher), fetches, settlement, divergence | moving any local branch or ref | [replication](domains/replication.md) |
| **Volume** | lifecycle, start point, release verdict, an upgrade's drain | which forks exist (Trees) | [volume](domains/volume.md) |
| **Health** | whether the history service and each view host are alive | any serving or landing decision | [health](domains/health.md) |
| **Telemetry** | metrics, traces, alarms | any decision; never blocks | [telemetry](domains/telemetry.md) |

## 2. Cross-domain contracts

### 2.1 Shared kernel (`model`: values only, no I/O)

```rust
pub struct RelPath(/* private */);   // beneath the root and git-fsck clean; only RelPath::parse builds one
pub struct TreeName(/* private */);  // a confined name that may fail fsck (a nested `.git`): only views show it

pub enum TreeRef {
    Main,                            // refs/heads/main
    Fork(ForkId),                    // refs/heads/fork/<id>; its upstream is its parent tree (git-model §2)
}
pub enum Served {                    // what a view serves
    Tree(TreeRef),
    Snapshot(SnapshotId),            // a pinned, read-only checkout of one commit (trees §3)
}

pub enum Writer {                    // the Agentfs-Writer trailer (git-model §3): exactly one per commit
    Client(ClientId),                // built only through Clients' minter
    Agentfs,                         // a sync commit and the root commit
}
pub struct ClientId {                // names its writer alone, so a commit needs no lookup
    launcher: LauncherId,
    tree: TreeRef,
    label: ClientLabel,
    n: Ordinal,                      // unique per volume, never reused
}

pub enum Principal { Client(ClientId), Actor(ActorId), Launcher(LauncherId), Agentfs }
pub struct Stamp { at: Time, by: Principal }
pub struct Audited<T> { value: T, created: Stamp, changed: Stamp } // stamped by the repository layer

pub enum Component {                 // what Health judges; a restart is a new incarnation
    HistoryService { volume: VolumeId, run: Incarnation },
    ViewHost { host: HostId, run: Incarnation },
}
pub struct Version { major: u32, minor: u32, patch: u32 } // set by the release flow
pub struct Family { major: u32, minor: u32 }              // components that talk, and internal formats, share it (§4)
```

### 2.2 Authority: who mints what

Every capability and `Writer` has private fields and is built only through a `Minter` the composition root hands to
the one domain that mints it; clippy bans the minter's constructor anywhere else (construction plus build time).
Capabilities never cross a process edge: a view host holds a **session**, and the history service keeps the
capabilities bound to it.

| Capability | Minted by | Held by | It can never |
|---|---|---|---|
| `LaunchCap { launcher, may: Main \| ForksItCreates, view_profiles }` | the composition root, from deployment config | a launcher | (sandbox launcher) attach to main or a fork it did not create, widen a view profile, create, stage or unstage the volume |
| `ClientCap` | `Clients::attach`, `rebind` (read-write grants only) | the client's session, in the history service | save into another tree or as another writer |
| `PinCap` | the composition root | Clients | (anyone else) pin a snapshot |
| `StartCap` | the composition root | Volume (lent to Replication's `fetch_start`) | (anyone else) set where main's line starts |
| `ActorCap { actor, roles }` | `Clients::actor`, from an identity the edge authenticated | operators; roles are data | act as another actor |

**The view host's calls name no client and no tree.** A write-open, a path call, a save at close and a transaction
call arrive over a session, whose edge supplies the client, its tree and its `ClientCap`, and refuses writes from a
session with none: a sandbox never writes main. A session opens only within one `Family`; the rpc edge refuses
another (`OtherFamily(Version)`) before any domain is called.

### 2.3 Events

Delivery hands every event to its consumers and to `Telemetry::emit` in one step. A producer stages an event in the
unit of work of the change it reports (a transactional outbox).

| Event | Producer | Consumers | Ends which wait |
|---|---|---|---|
| `Attached` / `AttachmentEnded{client, why}` / `LastDetached{tree}` (in that order, one unit) | Clients | the tree's actor, Trees, Volume | an ended client's open files close as saves and its waiting opens drop (git-model §6); its open transaction rolls back (§9); a fork's or pin's end; release |
| `TreeMoved{tree, from, to, paths, collisions}` (design choice: the git model's actor publishes it once the branch, index and working tree have moved) | the tree's actor | Views of that tree; Replication; Volume | a view's cache; a push; release |
| `TakeWaiting{tree, files}` / `TakeReleased{tree}` (design choice: a take waiting for open files, git-model §6) | the tree's actor | Telemetry only | none: the operator sees which file holds which changes back |
| `OrphanDropped{tree, path, writer, bytes}` (design choice: calls on an object no path names at close, git-model §7) | the tree's actor | Telemetry only | none |
| `ViewCaughtUp{view, tree, to}` | Views | anyone who must see a move in that view | "visible in view V" |
| `ForkEnded{fork}` / `ForkDropped{fork}` | Trees | Clients, Replication, Volume | the writer claim, a carried remote line, an upgrade's release |
| `VolumeDraining` / `VolumeStaged` / `VolumeReleased{disposition}` | Volume | Clients, Replication / the same / the composition root, CSI | attachments refused; pushes skip spacing / reopen / the service exits |
| `StartLineChanged{state}` | Volume | Telemetry | the start-line alarm |
| `PushSettled{remote, outcome}` | Replication | Volume | release |
| `ServiceReady{volume}` | the history service, once every domain restored and every judge re-published its state | view hosts, launchers | blocked file calls; a launcher's rebind |
| `LivenessChanged{component, liveness}` | Health | launchers, Telemetry | a launcher's restart in place |

## 3. System-wide invariants

The git model's invariants are git-model §15. These are the rest; all are design choices except where a column
cites the owner.

| # | Invariant | Guaranteed by | How strongly |
|---|---|---|---|
| S1 | A client changes only its own tree (or its open transaction's directory); content from elsewhere enters a tree only through its queue (git-model §5) | the session edge; Views' write handle exists only for the own tree | construction + build time |
| S2 | A view shows only what its grant's view profile shows; a rules error hides, never shows; no history, credential or state area is reachable through a view (R7) | Rules, Views | test |
| S3 | A reader sees each file whole old or whole new; an open handle keeps the content it opened | the working-tree update (git-model §3), Views | test |
| S4 | Each fact has one judge: the tree's actor a tree's head; Clients attachment; each view what it shows (`ViewCaughtUp`); Health liveness; Volume release. Nothing waits for another view | all | review + test |
| S5 | Every wait ends on a named event. The only timers are operator schedules (push spacing and retry, repack, telemetry export), Health's probes, a launcher's restart window and the kernel's cache backstop | all | review |
| S6 | Every persisted row carries audit stamps written by the repository layer; commits carry their writer and time (git-model §3) | the `store` adapter | construction time |
| S7 | One node per volume and one history service per volume, held by a kernel lock | `store`, before any domain is built | boot time |
| S8 | Components that talk share one `Family`; internal formats are read only by their own family; history is read and continued by every release of its major (R16) | the rpc edge, `store`, the git model at boot | boot + connect time |

## 4. Versions and upgrades (R16)

| What | Kept across | May break at | Checked by |
|---|---|---|---|
| Git history: branches, trailers, refs (git-model §3, §4) | every minor and patch of a major (R16) | a major | at boot: a branch last written by another major is refused (`OtherMajor`) |
| Components that talk: history service, view hosts, launchers, CSI plugin, CLI | one family: same major **and minor** (a design choice from his "maybe minor too"; revisable); patch free | any minor | the rpc edge: `OtherFamily` |
| Settings: the history setting, Rules' files, space limits, start point and acceptances, `on_release`, remotes | every release, each by its owner | never: a release that cannot read one refuses to stage, naming it | each owner at boot |
| Internal formats: `store` rows, tree storage layout, rpc | one family | any minor | `store` at boot: `FormatsOf(family)` until that family drains the volume |

| Upgrade | Path |
|---|---|
| Patch | replace a component in place; a view host's restart ends its clients' open files (no live hand-over: design choice) |
| Minor or major | **drain, then upgrade:** an operator's `drain_for_upgrade`; every client detaches (its open files saved at its end), every queue drains, pushes settle, every fork drops; `Released`; the new release stages on main's branch alone ([volume](domains/volume.md)) |

Releases come from semantic-release on merge (owner, 2026-11-01): a format or rpc change is at least a minor, a
history break a major. Cost: every `feat` is a minor, so every feature release is a full drain.

## 5. What the git model does not design yet

Each needs its own gate; none is designed on these pages, and the older mechanisms for them (a held landing, a
resolver chain, a journal) are gone with the pages that held them. Only the rows that cite R or A numbers rest on
the owner's words (§0); the others are gaps found by review, not requirements of his.

| Gap | What is missing |
|---|---|
| R14, R15 (whether a clean merge is acceptable); A3 (integrator patches wait until "whenDetached") | approvals and landing timing are not modelled (git-model §16); every landing is accepted when its queue takes it |
| Integrator patches (A3); restore, revert (no owner words on these pages) | no source other than a fork's commits and syncs enters a queue (git-model §5) |
| R9, R13 for files changed while open (a database and its `-wal`, Rules' groups) | a save happens at close (git-model §6): a file held open is recorded call by call in the journal (git-model §3) but saves nothing until its close, and holds back the incoming changes to it (git-model §6, "Progress") |
| Large and binary files, LFS (no owner words on these pages) | binary files are not modelled (git-model §16) |
| Space limits and history truncation (no owner words on these pages) | not modelled (git-model §16); [trees](domains/trees.md) §4 keeps the per-tree quota as a design choice |

## 6. Deployment compositions and crate layout

Each mode composes the same domains; none reads a mode flag.

| Composition | History service | View hosts | Health | Who launches clients |
|---|---|---|---|---|
| **Sidecar** (`mount`, `run`) | a child process of the sidecar's supervisor | main and each sandbox view, as child processes | the supervisor; the platform's probe judges it | the sidecar |
| **History service with view hosts** | `serve-history` | one process per host, many views each | the supervising launcher | each host's launcher |
| **CSI** | started at stage, runs through Draining | one per publish, plus in-pod launchers' views | the node plugin, told by the in-pod launcher's `watch` | publish and unpublish; the in-pod launcher (`ForksItCreates` only) |
| **CLI** (`fork`, `status`; `status` opens read-only, lock-free) | lock held: a short-lived service; busy: a control-socket client | none | none | none |

```text
model        shared kernel (§2.1): values only
contracts    every trait, capability, minter and event; the store port shapes (one UnitOfWork, one Outbox<E>). Depends on model only
domains      rules, clients, views, trees, replication, volume, health, telemetry, and the git model's crates
             (the referee over its Lineage port, git-model §7; the tree actor and queue, §3, §5; names parked):
             one crate each, on model + contracts only; no I/O, ports as traits
adapters     store (lock, repositories, audit stamps, outbox) · fs · gitx · fuse · rpc · csi · net · otel
apps         agentfs, agentfs-csi: composition roots; they make the minters, wire domains to adapters, compute nothing
```

Boundaries, strongest first. **Compile time:** no domain crate sees another's internals. **Construction time:**
capabilities and `Writer` come only from minters. **Build time:** `cargo deny` bans domain-to-domain and
domain-to-adapter dependencies; clippy bans stray minters and I/O crates in domains. **Boot and connect time:** an
unbound port, or a volume or peer of another family, is refused (S8); zero remotes is normal ("Remotes can come and
go", 2026-10-17).

## 7. Costs and tracer bullet

1. **A sandbox sees a change only once it is closed** (close-to-open, accepted 2026-11-03), and landed changes
   rewrite working-tree files, so hard links across a landing break.
2. **Every write call is a journal append** before it returns (git-model §3), so a view host's death or restart
   loses no answered call; it still ends its clients' mounts, and a sandbox's restarted host gets a new fork, since
   a fork has one writer for life ([clients](domains/clients.md)).
3. **File watchers have no guarantee** (A11): `TreeMoved` and `ViewCaughtUp` are the reliable notices.
4. **A full checkout per fork** until Trees' storage optimisation serves the same files from less space.
5. **Five gaps wait for their own gate** (§5), three of them resting on the owner's words (R9, R13, R14, R15, A3).
6. **The architecture is not settled until the prototype passes** (git-model §18): durable writes, byte-level
   calls, rename-over with open handles and queue progress on a real mount, none of which the model runs.

**Tracer bullet** (design choice). A sidecar runs main's view and one sandbox view: (1) the sandbox closes p; main's
queue takes it and main's view shows it once caught up; (2) main closes s; the sandbox's queue syncs it and the
sandbox's view shows it; (3) a main client holds p open while the sandbox's p lands: the take waits, then writes;
(4) killing the sandbox's view host mid-write leaves main untouched, and the sandbox's answered write calls are
saved from the journal; (5) while main holds p open, the sandbox's change to q lands without waiting. The git side of each step is git-model's; this bullet
proves the views, sessions and events around it.

**Prior art.** AFS close-to-open consistency · NFS `COMMIT` and write-ahead logging (the journal) · fair
readers-writer locks and per-key ordering (progress) · FUSE `RELEASE` and notify-invalidate · overlayfs and reflink clones ·
object capabilities · the transactional outbox · Kubernetes liveness probes · Semantic Versioning · DDD context maps.

## New terms

| Term | Definition | Why needed | Could be |
|---|---|---|---|
| view host; session | the process serving views, never inside a client; its connection for one client (capabilities stay in the service) | A2, §2.2 | the `mount` process |
| tree | main or one sandbox's fork: a branch with its working tree (git-model §2) | R3, R5 | backing tree |
| `TreeMoved` | the actor's notice that a tree's branch and working tree moved | views drop caches; pushes start | the older feed's `Recorded` |
| version, family | a component's semantic version; its major and minor | R16 | release |
| grant, view profile | what a client may do and see, as data | R7 | role |
| launcher, actor (operator) | who attaches clients; an authenticated operator. Not the git model's tree actor | A2, §2.2 | CSI node plugin |

## Reconcile with existing code (agentfs origin/main b5c0dc8, from `docs/design.md`)

- **The sandbox overlay** (§2.4) becomes a fork with its own branch and working tree; it stops writing the shared tree.
- **The attached mark** (§19.1) goes with `WhenDetached` until §5's timing gate. **Locks** (§19.2 item 13) become forwarded kernel locks plus the git model's take locks.
- **The journal, the patch queue and restore** have no home until §5's gates; the commit loop becomes the actor's save at close.
- **Formats and versions:** semantic-release and the family check are new (today's release flow NOT VERIFIED).

**NOT RUN:** the agentfs source was not read; the Rust is reasoned, not compiled.

*History: revisions 1–9 are in git; revision 10 (2026-11-03) removes everything git-model.md now owns; revision 11 (2026-11-04) follows git-model's journal, progress rules and prototype plan.*
