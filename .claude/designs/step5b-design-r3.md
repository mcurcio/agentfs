# Module-cut step 5b, round 2 (rev. 4, reconciled): history records the tree's identity; nothing names `main`

Layer: interfaces and implementation, inside [module cut](docs/target-architecture/future/module-cut.md). Read on
`main`, 22d55f9. Replaces `step5b-design-r2.md`. Rev. 4 applies `plan.md` (critic F1-F3, F9). Disposition at the end.

**Decision.** The engine names every tree by a stable identity, `TreeId`: `root`, or `fork/<n>`, where `n` is the event
that made the fork. Its refs, queues, collision refs, checkouts and the `Agentfs-Tree` trailer all use that identity.
No branch name is stored in the journal, in history, in the engine or in the model (`TreeNames` stops taking one).
5b removes the hardcoded `main` and builds nothing else. The name the root is **published** under is the publisher's
setting (one-engine M4). What a commit says about **where** it was made is one-engine M1's, asked once there (its Q3).
State format 2 → 3. Order (`plan.md`): step 6, the plan PR, **5b**, 5, one-engine M1. **No owner question here.**

## 1. Requirements, guidance, proposed

Requirements (asked as absolute and answered yes, or called a rule):
- R1. Owner, 2026-11-09, asked as absolute: a design doc lives until its slice is built ("Yes, this is a good idea to
  record: docs should be written towards future readers"). No requirement covers names or trailers.

Guidance (dated, his words; revisable):
- G1. 2026-11-12: "It seems more important for an _instance_ of agentfs to know that it is "the root" than to have a
  specially named branch stand-in as a root process identifier." On the split: "i agree with that split -- i assumed
  that it already worked that way. But the trailer should be addressed so that it records meaningful information".
- G2. 2026-11-11: "refs/heads/main should not be special or hardcoded. Consumers will configure the name of the root
  branch."
- G3. 2026-11-12: users "should never have to do this manually" (of `trees init`); "the goal is to have one engine,
  ostensibly the new tree view. Until its ready, nobody is actually going to use it."
- G4. 2026-11-06: "drop support. we are pre-production". 2026-11-03: "i want to know who changed what when".

Proposed (mine): everything below. Every option of the one-engine page's Q3 keeps the identity 5b writes, so 5b
waits on no answer.

## 2. Goals and the step that delivers each

| End goal | Delivered by | 5b's part |
|---|---|---|
| One engine | One-engine M1 (`ensure` from a `VolumeSpec`), M7 and M8 (the binary and CSI call it), M9 (delete) | none. The engine those steps build on holds no `main` |
| "refs/heads/main should not be special or hardcoded" (G2, first sentence) | **5b** | all of it |
| "Consumers will configure the name of the root branch" (G2, second sentence) | One-engine **M4**, the publisher: its key (asked at M4's gate), and it deletes root-branch-name.md | none. A setting now would be one nothing reads |
| "the trailer should be addressed so that it records meaningful information" (G1) | **5b** (which tree, by identity); one-engine **M1** (where: volume and pod, its Q3) | the identity |
| Recording is the baseline | built (a save is journaled before the call returns) | changes no step of the save path. Its one recording risk is in §9 |
| Invisible to the workload | built (state outside the mount) | adds nothing reachable through the mount |
| Git idioms | One-engine M4 (the published name's key) | `branch.fork/<n>.merge = refs/heads/root` |
| Any remote, or none | One-engine M4 | none |
| History is the product | **5b**: history records identities, never a name that can change, so a rename rewrites nothing | all of it |

**How production creates a tree volume.** Not by `trees init`. One-engine M1 gives every launcher one idempotent
`ensure(VolumeSpec)`. The sidecar's init container calls it in place of today's `agentfs init --include` (M7), and CSI
CreateVolume calls it (M8). `trees init` stays a trial command until M7 folds `trees_cmd.rs` into `init` and `mount`.
5b changes only what it prints (`root <head>`).

## 3. Canonical shape

Domain terms touched: **root**, **fork**, **upstream**, **tree history** (trailers, refs). No domain change: the root
is the tree with no upstream, as in module-cut.md. The engine now holds it the way the model already does
(`calls::TreeId::ROOT`, written as `root`).

```text
   identity (history, journal, every internal ref)       name (the publisher's setting; one-engine M4)
   ───────────────────────────────────────────────       ──────────────────────────────────────────────
   TreeId::Root    = "root"     refs/heads/root    ──publisher──▶  remote: refs/heads/<configured name>
   TreeId::Fork(7) = "fork/7"   refs/heads/fork/7
```

| Pattern | Canonical source | Where |
|---|---|---|
| Entity identity apart from its attributes | Evans, *DDD* ch. 5 "Entities"; Kubernetes "Object Names and IDs" (`uid` beside `name`) | `TreeId`; the published name stays an attribute at the edge |
| Value object; parse, don't validate | Evans ch. 5; A. King, "Parse, don't validate" (2019) | `TreeId`, parsed once at the journal fold |
| Ports and adapters: policy at the edge | Cockburn, "Hexagonal architecture" | the name is applied by the publisher, never by the core |

New terms:

| Term | Definition | Why needed | Existing term it could be |
|---|---|---|---|
| `TreeId` (engine) | A tree's identity in its volume: `root` or `fork/<n>` | G1: identity, not a name, says "the root" | the model's `calls::TreeId` (same idea; numbers differ, as `EngineNames` maps) |
| published name | The branch name the root is pushed under; the publisher's setting, changeable, not in `VolumeSpec` | G1's split; G2 | `agentfs.branch` (M4's gate decides the key, §7) |

Design rules (choices):
1. **Identity is parsed once, at the journal fold.** Each stored tree string becomes a `TreeId`; a string that names no
   tree is `Format`. Above the fold, no code holds a tree as a `String`.
2. **No code compares a tree to a literal.** "Is this the root?" is `tree.is_root()`. "Where does this fork land?" is
   the upstream map's entry. Ref text comes from `TreeId`'s `Display`.
3. **The published name never enters `agentfs-core`'s tree actor.**

## 4. The trailer

| Field | Value | Change |
|---|---|---|
| `Agentfs-Tree` | `root` or `fork/<n>` | **was the name `main`; now the identity**. Read by `pending_from`, `take_next`, the checker and conformance |
| `Agentfs-Collision` (git-model §3) | names its tree the same way | `<root>` becomes `root` |
| `Agentfs-Writer`, `-Cause`, `-Event`, `-Format`, `-Open`, `-Collisions` | as today | none |

"When" stays the committer date (git-model §3). The committer stays git-model §2's "one fixed service identity"
(`agentfs@localhost`, `tree_git.rs:410`). The commit's origin (its volume, and the pod, namespace and node the older
engine writes, `history.rs:1131`) is added once, at one-engine M1, where `VolumeSpec.name` and the launcher's pod
context first exist (one-engine Q3). The history format therefore changes at 5b and again at M1; nothing reads
tree-view history in production (G3, G4).

**Exact edits to the baseline pages** (numbering kept; nothing else on them changes). The model writes
`Agentfs-Tree: root` already (`world.rs:364`, `MODEL_ROOT`), so these edits align the baseline with the model. The
model and the spec stay equal: neither writes an origin trailer until M1.
- `git-model.md:74`: "(the root, the tree with no upstream, its branch's name configured: [root branch name](…); a
  fork)" becomes "(the root, the tree with no upstream; a fork)". `refs/heads/<root>` becomes `refs/heads/root`.
- `git-model.md:196`: `# <root> | fork/<id> | txn/<id>: the branch it was made on` becomes
  `# root | fork/<id> | txn/<id>: the tree it was made on, by identity, never a published name`.
- `git-model.md:211–212`: `<root>` becomes `root` (two places).
- `git-model.md:228`: `refs/heads/<root>` becomes `refs/heads/root`; "yes, as today's
  `refs/heads/agentfs/<volume>`" becomes "yes, under its published name ([root branch name](…))".
- `rules-ledger.md:35–36`: "its branch is `refs/heads/<root>`, a name the volume is configured with ([root branch
  name](…))" becomes "its branch is `refs/heads/root`; the name it is published under is configured".
- `MODEL.md:3–4`: "(the tree with no upstream; its branch's name is configured, `world::TreeNames`, which refuses
  `fork`, `txn` and names under them, where forks and transactions live)" becomes "(the tree with no upstream, branch
  `refs/heads/root`; forks are `fork/<n>`, transactions `txn/<id>`)".
- `module-cut.md`: row 5b "Built"; "What this changes in the code", first bullet: "its name is configured
  (`world::TreeNames`)" becomes "it is `root`, as in the engine".

## 5. Internal refs and paths

| Today | After 5b |
|---|---|
| `refs/heads/main`, `refs/heads/fork/<n>` | `refs/heads/root`, `refs/heads/fork/<n>` |
| `refs/queue/main/<e>` | `refs/queue/root/<e>` |
| `refs/collisions/main/<loser>` | `refs/collisions/root/<loser>` |
| `refs/agentfs/objects/main` | `refs/agentfs/objects/root` |
| `HEAD` → `refs/heads/main` | `HEAD` → `refs/heads/root` (constant) |
| `[branch "fork/3"] merge = refs/heads/main` | `… merge = refs/heads/root` |
| `<state>/trees/main` | `<state>/trees/root` |

Every one is today's `format!` with the tree's `Display` text; 5b adds no ref module (step 5.3 builds `refs`, as its
design already plans). A queue prefix ends in `/`, so `fork/7/` never matches `fork/70/`. `TreeId::Txn` is added when
transactions are built.

## 6. Interfaces (frozen page; bodies are `...`)

```rust
// crates/agentfs-core/src/tree_actor/tree_id.rs  (new; `pub use tree_id::{NotATree, TreeId};` in tree_actor)
/// A tree's identity in its volume. Its text (`root`, `fork/<n>`) is what history records (`Agentfs-Tree`) and
/// what every internal ref and checkout is named by; never a published branch name (docs/trees.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TreeId {
    /// The tree with no upstream. Sorts before every fork.
    Root,
    /// A tree with one upstream, made at journal event `n` (never reused). Forks sort by `n`.
    Fork(u64),
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} names no tree: a tree is `root` or `fork/<n>`")]
pub struct NotATree(pub String);
impl TreeId { pub fn is_root(self) -> bool { ... } }
impl std::fmt::Display for TreeId { ... }      // "root" | "fork/<n>", n decimal, no leading zeros
impl std::str::FromStr for TreeId { type Err = NotATree; ... }  // exact inverse of Display ("fork/07" refused)

// crates/agentfs-core/src/tree_actor/mod.rs  (changed)
const FORMAT: &str = "agentfs-trees-3\n";      // a format-2 area is Error::Format, named, before its journal is read
impl Volume {
    /// Unchanged contract: create or recover. Recovery is `Format` also when a tree string is no `TreeId`, there is
    /// no `root` branch, the root has an upstream, or a fork's upstream is not a tree.
    pub fn open(state: &Path) -> Result<Self> { ... }
    pub fn trees(&self) -> Vec<TreeId> { ... }           // replaces tree_names(); root first, forks by number
    pub fn fork(&mut self) -> Result<TreeId> { ... }      // a fork of the root, as today
    pub fn head(&self, tree: TreeId) -> Result<Oid> { ... }
    // Every other public `tree: &str` parameter becomes `tree: TreeId`, with no other change: read, files,
    // write_open, write_open_truncating, anonymous_open, first_link, is_anonymous, rename, unlink, take_next,
    // inode_size; drain_changed returns Vec<(TreeId, u64)>.
}
// private, in mod.rs; step 5.3 moves them into `Upstreams` and `message`:
//   upstreams: BTreeMap<TreeId, TreeId>            (was <String, String>; validated at the fold, rule 1)
//   fn upstream_of(&self, tree: TreeId) -> Option<TreeId>
//   fn tree_trailer(message: &[u8]) -> Result<Option<TreeId>>   // pending_from and take_next, its two readers
// journal.rs: tree fields stay `String` on disk (the journal is a DTO); the fold parses them.
// queue.rs: `Plan { source: TreeId, .. }`.

// crates/agentfs-core/src/tree_git.rs: HEAD becomes `ref: refs/heads/root\n`; write_config unchanged in signature.
// crates/agentfs-fuse/src/tree_view.rs: `Binding { tree: TreeId, .. }`
// crates/agentfs/src/trees_cmd.rs (trial CLI): Init unchanged in shape, prints `root <head>`.
//   Mount { root: PathBuf, .., #[arg(long)] fork: Option<TreeId> /* `root` refused: not a fork */,
//           #[arg(long, default_value = "root-client")] writer: String, .. }
// crates/agentfs-conformance: ENGINE_ROOT and engine_names() deleted; EngineNames::new() takes no root.

// crates/agentfs-model/src/world.rs (S2): no name is configured, so nothing is refused.
//   TreeNames::new() takes no root; name(TreeId::ROOT) is MODEL_ROOT ("root"), its one text in the model.
//   The reserved-name assert goes. digest.rs: the four-root-name test and the refused-names test go.
//   Callers (phase1-oracle.rs ×3, conformance lib.rs and tests/replay.rs, digest.rs) call TreeNames::new().
```

No `VolumeName`, no `init`/`open` split, no checkpoint field, no `--volume`: none has a reader before M1.

## 7. Hand-off to the publisher (one-engine M4)

The published name is the publisher's setting: changeable, read at boot by its owner (Replication), not part of
`VolumeSpec` (the plan PR removes `root` from volume.md, per G1's split). M4's gate asks which key holds it, decides
what a rename does to the old remote ref, and its commit deletes root-branch-name.md. Two facts it needs:
- `agentfs.branch` today means the recording engine's history branch, fixed at `init` and barred from includes
  (configuration.md:52, :99). Reusing it while both engines ship gives one key two meanings.
- `remote.<n>.push` is git's own idiom (principle 3), but today's pusher never consults it on purpose: it sends
  explicit `<oid>:<ref>` refspecs on an anonymous remote (`push.rs:8–14`). Choosing it reverses that choice.

## 8. What moves where (`tree_actor/mod.rs` unless named)

| Site today | Becomes |
|---|---|
| `prepare_state_area`: Root message, `branch("main")` (675–689) | `TreeId::Root` |
| `build_trees`: `contains_key("main")` (843) | the fold's checks (§6 `open`) |
| `fork`: `"main"` ×3, `format!("fork/{seq}")` ×2 (879–895) | `TreeId::Root`, `TreeId::Fork(seq)` |
| `rebuild_owners` `refs/queue/main/` (1389) | `TreeId::Root`'s queue |
| `plan_sync` `head("main")` (1552) | `head(upstream_of(tree)?)` |
| `pending_from` (1629–1633) | its upstream's queue, `tree_trailer(..) == Some(tree)` |
| `progress_one` sort (1729) | `TreeId`'s order |
| `take_next` (1747–1778), `enqueue_moves` (2218–2230) | `is_root()`, `upstream_of`, `tree_trailer`, the upstream map |
| `tree_git.rs:53` `HEAD`; `merge = refs/heads/{parent}` (135) | `refs/heads/root`; `refs/heads/{upstream}` |
| `trees_cmd.rs` `"main"` ×2, `MAIN`, `main-client` | `TreeId::Root`, `ROOT`, `root-client` |

Unchanged: `fork/deliver.rs`'s and `cli_fork_deliver.rs`'s `"main"` (an `--onto` branch of the older engine; M9
deletes it).

## 9. Slices, gates and risks

One PR, three slices, from `main` after the plan PR merges (it rebases on step 6, which touches `world.rs` and
`digest.rs`). Line numbers here are 22d55f9's; the implementer re-reads them on that base.

| Slice | Changes | Proven by |
|---|---|---|
| **S1, tracer: identity everywhere** | §6's engine half. One compile unit: the public API change reaches fuse, the CLI and conformance. Core tests: `"main"` becomes `const ROOT: TreeId = TreeId::Root` (232 sites in `tests/tree_actor.rs`, counted on 22d55f9, mechanical). Scripts: git-facing `main` becomes `root` (`trees-conformance.py`, `measure-trees.py`, `check-tree-git.sh`, the e2e and power-loss tree scripts). Docs: trees.md, configuration.md (`--writer` default), library/core.md, library/filesystem.md, architecture.md (one new row) | fmt, clippy `-D warnings`, `cargo test --workspace`; `bash -n`, `py_compile`; the light model tier; ci, image |
| **S2: the model names no root** | §6's model half | the **full** model gate (a removed condition; ~41 min here): `check --all` equal on 2620/2620 (the digest labels the root `T0` under any name), matrix reproduced (no anchor is in `TreeNames`) |
| **S3: baseline pages** | §4's edits | light tier, `docs_reference`, ci |
| **Step gate** | PR head | one `phase-1.yml` dispatch (73 min, run 43146523186) |

5b writes no OWNER-GUIDANCE entry and does not touch root-branch-name.md: the plan PR does both (`plan.md`).

New tests: `TreeId` text round trip (`fork/07`, `main`, `fork/` refused); the fold's four `Format` cases; end to end
(open, save, fork, landing, sync, reopen) with no `main` in any ref, checkout or trailer; format 2 refused; 11 forks
taken in order root, `fork/9`, `fork/10`; `pending_from` true while a fork's landing is queued, false after
`take_next(Root)`.

| Risk | How the design meets it |
|---|---|
| A wrong queue prefix makes `pending_from` false, so a sync passes a queued landing (finding 51's class): **recording** | Prefixes come from the upstream map, the tree from `tree_trailer`; the `pending_from` test. Whether `shows_the_same` can hold while a landing waits is still unproven either way: this is a risk, not a delivery |
| A literal `main` survives | It names no tree, so any use fails in tests; the grep gate (`"main"` in `tree_actor/`, `tree_git.rs`, `tree_view.rs`, `trees_cmd.rs`) finds none |
| Fork order changes from text (`fork/10` before `fork/9`) to numeric | `progress_one` takes one step and returns; the root stays first. A fork's take is a sync (`take_sync`, mod.rs:1697) that writes only that fork's refs and enqueues nothing (`enqueue_moves`' callers are 1523 and 2201, landings and saves). So the order between forks changes which sync gets the next event number, never a tree's contents. The 11-fork test and the light tier check it |
| Conformance traces or digests change | Traces carry model `TreeId`s; the model writes `root` already; S2 changes no digest |

## 10. Owner question

None. Rev. 3 asked whether the identity is enough for "meaningful information" (its Q1). Every option kept the
identity and differed only in when "where" is added, so the question moved to the step that adds it: one-engine M1,
its Q3. 5b is the same under every answer.

NOT RUN: `cargo check`, `cargo test` (no code written; compiles are forbidden on this machine during the model gate).
The model gate tiers, the digest, the corpus and conformance were not run. CI, the dispatch and FUSE were not run. The
fork-order argument is from reading `take_sync` and `enqueue_moves`, not a run. Line numbers are from 22d55f9; the 232
count was re-read there (`grep -c '"main"'`).

## Review disposition

1. **Right.** 5b no longer claims "one engine" or the configurable name; §2 credits one-engine M1, M4, M7, M8 and M9.
2. **Right.** §2 says what production calls: M1's `ensure`, from the sidecar's init container (M7) and CSI
   CreateVolume (M8).
3. **Right.** The `Option` is gone with `--volume`: `trees init` keeps one behaviour.
4. **Right.** The costs of a volume trailer now (type, create/recover split, errors, field, format bump, flag) are why
   "where" waits for M1, where they are free.
5. **Right, in part.** On "separate refs avoid ambiguity": true for the older engine; tree views publish nothing yet.
6. **Right.** "Where" changes once, at M1.
7. **Right.** The key is M4's (§7). **Refuted in part:** today's pusher never consults `remote.<n>.push`
   (`push.rs:8–14`), so that option reverses a recorded choice.
8. **Right.** `Upstreams`, `refs` and `Message` stay step 5.3's; `VolumeName` and the create/recover split go. Kept:
   `TreeId`, format 3, numeric fork order (§9).
9. **Superseded.** 5b waits on no answer (§10).
10. **Right.** Neither the model nor the spec writes an origin trailer before M1.
11. **Right.** `pending_from` sits in the risks.

**Reconcile (`plan.md`, critic F1-F3, F8, F9).** F1: the published name is M4's (its key, its rename, deleting
root-branch-name.md); `VolumeSpec` no longer carries it, so "fixed at create" is gone. F2: "where" has one home, M1,
and one question, one-engine Q3; milestones renumbered to the one-engine page (M7 binary, M8 CSI, M9 delete). F3:
S2 is the one slice that removes `TreeNames`' root parameter and rewrites MODEL.md:3–4; step 6 adds no `HEAD` check.
F8: 232 sites, re-counted. F9: the stale `ensure` quote is gone; `step5b-design-r2.md` is marked superseded.
