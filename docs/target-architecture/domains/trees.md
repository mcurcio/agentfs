# Trees: storage, quotas and pinned snapshots

**Proposed, not built.** The fork lifecycle (create, keys, end, the drop) is built and described in
[docs/trees.md](../../trees.md#forks-keys-end-and-the-drop). This page proposes the rest of the Trees domain (where
working trees live, their space, and pinned snapshots), one part of the system around the git model ([the system](../system.md)). The git model ([git-model.md](../git-model.md)) wins wherever the two disagree. Rust sketches show surfaces only. The commit that lands the slice that builds this domain deletes this page and moves the behaviour into the library or operator docs.

**What this page covers.** In git terms a tree is a branch with its own index and working tree (git-model §2, §3);
its branch, saves, queue, syncs and transactions are the git model's. Trees owns the rest: where the working tree's
files live, their confinement and space, and pinned snapshots.

## 0. Guidance this domain serves

The owner's guidance that applies here is in `OWNER-GUIDANCE.md` at the repository root. Everything else on this page is a design choice.

## 1. What Trees owns, and what it is not

| Owns | Is not |
|---|---|
| **storage**: where each tree's working tree and each transaction's directory live, and serving them from less space than full copies, if it can | the branch, index or what a checkout writes (git-model §3) |
| **confinement**: every path is reached beneath its tree's root, no symlink followed | which names a view shows (Rules, Views) |
| **space**: each fork's limit and the volume's fork budget | the volume's lifecycle (Volume) |
| **pinned snapshots**: a read-only tree of one commit, for `Sees::Snapshot` | history reads (git) |

## 2. The fork lifecycle

Built: [docs/trees.md](../../trees.md#forks-keys-end-and-the-drop). What remains here: a fork's space is reserved at
create and released at its drop (section 4), and forks of forks (git-model §10) are not built.

## 3. Pinned snapshots

A pinned snapshot is a read-only checkout of one commit of a tree's branch, made complete before it is visible and
never written; a snapshot client's `advance` makes a fresh one, and each is deleted at its `LastDetached` (design
choice). It has no branch and no queue: nothing lands in it.

## 4. Storage, confinement and space

| Rule (design choices) | Enforced by | Strength |
|---|---|---|
| T1. Every path is reached beneath its tree's root; no symlink, `..` or magic link is followed | the only path type is `RelPath`; the only resolver is a beneath-only open; no method takes a `Path` | compile time + test |
| T2. A view's write handle reaches only its own tree's working tree (or its transaction's directory) | other trees are typed read-only handles | compile time |
| T3. A file is replaced whole: a reader sees whole old or whole new (S3) | the working tree and the repository share one device, so each file a checkout writes can be renamed into place | boot time (checked at open) + test |
| T4. A fork cannot fill the disk the root writes to | a per-fork quota set at create, reserved in full from the fork budget (thick); without quota support, editable forks are refused | boot time |
| T5. Storage is an optimisation, never a second truth: whatever serves a working tree serves exactly the files the git model's checkout would write | one test suite runs against full checkouts and the optimised storage | test |

**Storage** (design choice). Each working tree is, in git-model §3, a full checkout. Trees may serve it from a reflink
clone of its upstream's working tree, or an overlay over it (a "fork layer"), only if T5 holds.

| Setting | Values | Current choice |
|---|---|---|
| Per-fork space limit | bytes | one value per volume (placeholder: 1 GiB) |
| Fork budget | bytes, at most the free space the operator gives forks | placeholder: 25 % of the volume |
| Reservation | thick · thin | thick: no fork, nor the root, runs out of promised space; thin lets one burst fail everyone's writes |
| Directory renames in a fork | refused with `EXDEV` (as today) | tools copy and delete |

## 5. Public contract

```rust
pub trait Trees {                                                                      // + create, end, end_keyed, facts, live (built)
    fn pin(&mut self, of: TreeRef, by: &PinCap) -> Result<SnapshotId, TreeError>;      // ends at its LastDetached
    fn handle(&self, s: Served) -> TreeHandle;                                         // confined root for a view
}
pub enum TreeError {                                                                   // + the built variants
    SpaceLimit,         // the fork budget cannot reserve the limit
    QuotaUnavailable,   // no per-tree quota support: editable forks refused (T4)
    ApprovalNotBuilt,   // the fork's settings need landing approvals, which is not built
    EscapesRoot,
}
```

The filesystem holds the files; each fork's row is built (docs/trees.md, "Rows and event commits").

## 6. Tests

| Test | Rules out |
|---|---|
| A symlink planted as any ancestor, or as the last component, is never followed | escaping the tree (T1) |
| A reader opened before a landing rewrites p reads the old bytes whole; a reopen reads the new | torn reads (T3) |
| A fork fills its quota while the root saves: the root's save succeeds | a fork denying the root (T4) |
| The same scenarios on full checkouts and on optimised storage give identical files | storage as a second truth (T5) |
| A crash while filling a snapshot leaves none visible | a partial snapshot |

## 7. Costs

1. **Full checkouts cost a copy per fork** until optimised storage passes T5.
2. **Thick reservation** caps how many forks fit: 1,000 forks at 1 GiB need a 1 TB budget, idle or not.
3. **Quotas narrow the filesystems** that can host forks (XFS, ext4 with project quotas, btrfs).
4. **Directory renames inside a fork stay refused** (`EXDEV`).

## For the code architects (parked)

Quota mechanics per filesystem and project-id limits at 1,000 forks; reflink or overlay layout for optimised storage;
whether `git worktree add` or a plain checkout makes each working tree; snapshot naming.

## Where it meets the existing code

- **Confinement** (`agentfs-core` `confined.rs` and `agentfs-fuse` `backing.rs`, a duplicate by the module's own words) becomes one confined `fs` adapter.
- **The tmp layer** (`overlay/tmp.rs`) becomes a fork's working tree; recordable names stop going to the shared tree.
- **The existing volume fork** (a whole volume cloned from another's latest commit) becomes an in-volume fork with its own branch. Cost: quotas, snapshots and the drop rule are new code.

**Not verified:** reflink, project quotas, beneath-only resolution and rename atomicity were not tested.

