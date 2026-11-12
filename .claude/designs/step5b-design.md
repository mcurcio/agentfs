# Module-cut step 5b: the root branch's name is configured (design, rev. 3)

Layer: interfaces and implementation, inside [module cut](docs/target-architecture/future/module-cut.md) and
[root branch name](docs/target-architecture/future/root-branch-name.md). Read on `main`, 22d55f9.

**Decision.** `trees init --root-branch NAME` names the root once; the journal's checkpoint records it as the root's
identity, as the model records `TreeId::ROOT`. Is-this-the-root and where-a-fork-lands derive from that record; no
code compares a tree to `main`. State format 2 → 3. Order: **5b before 5** (§7). Where the name lives and whether it
has a default are the owner's (§10, Q1, Q2); the design is built on my recommended answers.

## 1. Requirements, guidance, proposed

Requirements (asked as absolute and answered yes, or called a rule):
- R1. Owner, 2026-11-09, asked as absolute: a design doc lives until its slice is built and the landing commit deletes
  it ("Yes, this is a good idea to record: docs should be written towards future readers"). So the PR's last commit
  deletes `root-branch-name.md` and every link to it (§8). No requirement covers the root's name itself.

Guidance (dated, his words):
- G1. 2026-11-11: "refs/heads/main should not be special or hardcoded. Consumers will configure the name of the root
  branch."
- G2. 2026-11-08: "there is no "main" in this new model of upstreams and forks".
- G3. 2026-11-06, on old state formats: "drop support. we are pre-production and there is no need to worry about
  "supporting" old concepts". [trees.md already says "Earlier state formats do not open." A format-2 area is refused,
  as G3 says; its `history.git` stays an ordinary git repository. Not asked again.]
- G4. 2026-11-11: the light tier of the model gate serves "renames, docs and changes that move no rule logic" ("Yes").
- G5. AGENTS.md principle 3: "Operator policy lives in the git directory, which iac sets"; principle 1 (recording).
- G6. 2026-10-15: "Configuration with sensible defaults". Delivery: one PR per step to `main`, which the owner merges.

Proposed (mine, revisable). P1 and P2 are policy, so they are **asked** (§10); the rest is decided here:
- P1 (Q1). Given once, at create (`Volume::init(state, &RootName)`, `trees init --root-branch NAME`); recorded in each
  checkpoint's new `root` field and the Root commit's `Agentfs-Tree:` trailer. No git-config key: the engine rewrites
  `config` and `HEAD` at every open ("The repository's whole configuration; nothing else writes it", `tree_git.rs`).
  `HEAD` becomes `ref: refs/heads/<root>`, git's own record of the main line, a copy in the git directory.
- P2 (Q2). No default, library or CLI: `trees init` on an empty area without the flag fails, naming the flag.
- P3. `init` is idempotent, like `agentfs init --volume --branch` (configuration.md). Same root: it recovers. Another
  name: `RootMismatch`, refused before any journal write. Without the flag, `trees init` recovers an existing volume.
- P4. Names follow git's branch-name rule through libgit2 (`git2::Branch::name_is_valid`, equal to
  `git check-ref-format --branch` on all 19 names probed, §9), and may not be `fork`, `txn`, `fork/…` or `txn/…`.
- P5. Conformance and two CLI scripts use `dev/trunk`: a slash (nested checkouts, ref directories) and sorting *before*
  `fork/`, unlike `main`. P6. `trees mount`'s `MAIN` becomes `ROOT`; `--writer`'s default `main-client`, `root-client`.

## 2. Canonical shape

Touched: **root**, **upstream**, **fork**, **tree history** (branches, trailer, queue refs). **No change to the domain
model** (module-cut.md: "Its branch's name is configured, never fixed"). The engine holds the root as the model does,
an identity recorded at create (`TreeId::ROOT`, `world.rs:598`), not inferred from refs. Volume's `VolumeSpec { root:
BranchName }` (`domains/volume.md`) becomes `root: RootName`.

| Pattern | Canonical source | Where |
|---|---|---|
| Value object, parse-don't-validate (smart constructor) | Evans, *DDD* ch. 5; King, "Parse, don't validate" | `BranchName`, `RootName` |
| One source of truth, copies derived from it | rules:global, "One source of truth" | checkpoint `root` → `HEAD`, checkout dir, trailer |
| Idempotent create ("init is safe to rerun") | `git init` re-init; `agentfs init` | `Volume::init`, `trees init` |
| Primary branch named by `HEAD`, set at create | `git init --initial-branch` | `HEAD` copy, `--root-branch` |

New terms:

| Term | Definition | Why needed | Existing term it could be |
|---|---|---|---|
| `BranchName` | A string git accepts as a branch name | `tree_git` writes `HEAD` only from a valid branch; `RootName` builds on it | `VolumeSpec.root`'s type in volume.md |
| `RootName` | A `BranchName` no fork or transaction name can take | `Volume::init` takes only a usable root | the model's `TreeNames` check |
| `--root-branch NAME` | The `trees init` flag that names the root | G1: "consumers will configure the name" | `agentfs init --branch` (not reused: default `agentfs/<volume>`, other engine) |
| checkpoint `root` | The journal field holding the root's name | The root's identity is recorded, never inferred | none (new field; the reason for format 3) |
| `NotInitialized`, `RootMismatch` | `tree_actor::Error` variants | `open` no longer creates; `init` refuses a rename | `Error::Format` (kept for a damaged or older area) |

## 3. Design rules (design choices)

1. **The checkpoint's `root` is the only record of the root's identity.** `open` reads it from the fold and checks,
   before serving, that `refs/heads/<root>` is a pointer and the root has no entry in `upstreams`; otherwise `Format`.
   It does not infer the root from refs: `git-model.md` §4 keeps transactions as `refs/heads/txn/<id>` until commit or
   rollback (§12 deletes leftover ones at reopen), so "the branch with no upstream" is not unique once transactions
   are built. In the model a transaction's upstream is its fork (`world.rs:10`). `HEAD`, the checkout dir and the
   trailer are copies; every reader outside the engine, conformance included, takes `Volume::root()`.
2. **No code compares a tree to a literal name.** "Is this the root?" is `self.is_root(tree)` (equality with the
   recorded `root`, as the model's `t != TreeId::ROOT`). "Where does this fork land?" is `upstream_of(tree)`. "Who
   syncs from this tree?" is `forks_of(tree)`. A queue's ref prefix comes from one function, `queue_ref_prefix(tree)`.
3. **A name enters the engine only as a `RootName`.** Its constructor is the only check. `tree_git` takes a
   `&BranchName` for `HEAD` and writes every config string through one git-config quoting helper (`\` and `"` escaped,
   the rule section names already use, `tree_git.rs:133`).

## 4. Interfaces

```rust
// crates/agentfs-core/src/branch_name.rs  (new; altitude 3: wraps a libgit2 primitive)
/// A name git accepts for a branch (`git check-ref-format --branch`), via `git2::Branch::name_is_valid`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} is not a git branch name")]
pub struct NotABranch(pub String);
impl BranchName {
    pub fn new(name: &str) -> Result<Self, NotABranch>;
    pub fn as_str(&self) -> &str;
    /// `refs/heads/<name>`.
    pub fn reference(&self) -> String;
}
// also: impl FromStr for BranchName (Err = NotABranch, = new); impl Display (as_str)

// crates/agentfs-core/src/tree_actor/root_name.rs  (new; re-exported as tree_actor::RootName)
/// The root tree's branch name: a git branch name that is not `fork` or `txn`
/// and does not start with `fork/` or `txn/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootName(BranchName);
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RootNameError {
    #[error(transparent)] NotABranch(#[from] NotABranch),
    #[error("{0:?} is reserved for forks and transactions (fork, txn, fork/..., txn/...)")] Reserved(String),
}
impl RootName {
    pub fn new(name: &str) -> Result<Self, RootNameError>;
    pub fn as_str(&self) -> &str;
    pub fn branch(&self) -> &BranchName;
}
// also: impl FromStr for RootName (Err = RootNameError, = new; clap parses `--root-branch` with it); impl Display

// crates/agentfs-core/src/tree_actor/journal.rs  (changed)
Entry::Checkpoint { root: String, next_seq: u64, /* pointers, upstreams, opens, page_saved as today */ }
// mod.rs: const FORMAT: &str = "agentfs-trees-3\n";  a format-2 area fails with Error::Format, before the journal is read.

// crates/agentfs-core/src/tree_actor/mod.rs  (changed)
pub enum Error { /* existing */
    #[error("no tree volume here: create it with `agentfs trees init --root-branch NAME`")] NotInitialized,
    #[error("this volume's root is {recorded:?}, not {given:?}: a root's name is fixed when the volume is created")]
    RootMismatch { recorded: String, given: String },
}   // errno: both EIO (they arise at open only, never from a file call)
impl Volume {
    /// Create a tree volume at `state` (the directory too, mode 0700) with its root named `root`, or recover one
    /// whose root is `root`. Finishes a cut init. Errors: `RootMismatch` (before any journal write), and all of `open`'s.
    pub fn init(state: &Path, root: &RootName) -> Result<Self>;
    /// Recover an existing tree volume. Writes no volume: a missing directory is `NotInitialized` and is not created.
    /// On an existing directory it first takes `actor.lock` (creating that file) and sets mode 0700, as today, so a
    /// refused empty directory is left holding `actor.lock`, which `init` ignores. `NotInitialized` for an empty
    /// directory or a cut init; `Format` for another format or a checkpoint `root` that breaks design rule 1.
    pub fn open(state: &Path) -> Result<Self>;
    /// The root tree's name, from the journal.
    pub fn root(&self) -> &RootName;
}
// private: field `root: PathBuf` renamed `state`; new field `root: RootName`, set once at open; fn is_root(&self,
// tree: &str) -> bool; fn upstream_of(&self, tree: &str) -> Option<&str>; fn forks_of(&self, tree: &str) -> Vec<String>;
// fn queue_ref_prefix(tree: &str) -> String ("refs/queue/{tree}/"; not `queue::prefix`, a cut length, queue.rs:45);
// fn prepare_state_area(state: &Path, root: Option<&RootName>, lock: &Arc<FileLock>) -> Result<()>

// crates/agentfs-core/src/tree_git.rs  (changed)
impl TreeGit {
    /// Replaces `config` and `HEAD` (`ref: <head.reference()>`) atomically; `upstreams` maps a fork to its parent.
    pub fn write_config(&self, head: &BranchName, upstreams: &BTreeMap<String, String>) -> Result<()>;
}   // the `HEAD` constant is deleted

// crates/agentfs/src/trees_cmd.rs  (changed)
enum Command {
    /// Create the tree volume with its root branch named NAME, or recover it; prints `<root> <head>`.
    Init { #[arg(long, value_name = "NAME")] root_branch: Option<RootName> },
    Fork,
    Mount { root: PathBuf, /* … */ #[arg(long, default_value = "root-client")] writer: String, /* … */ },
}
// Init: Some(name) => Volume::init, None => Volume::open. Fork, Mount: Volume::open; Binding { tree } is volume.root().

// crates/agentfs-conformance (changed). The harness configures, as an operator would; the volume names.
/// What the harness creates each volume with: not `main` or `MODEL_ROOT`, a slash, sorts before `fork/`.
const CONFIGURED_ROOT: &str = "dev/trunk";   // private, read by against_library/against_mount; ENGINE_ROOT and
impl Library { pub fn new(dir: &Path, root: &RootName) -> Result<Self, CallError>; pub fn engine_root(&self) -> &str; }
impl Mounted { pub fn new(bin: &Path, work: &Path, root: &RootName, with_sandbox: bool, appends: Vec<bool>)
    -> Result<Self, CallError>; pub fn engine_root(&self) -> &str; }      // engine_names() deleted
// Both: Volume::init, names = EngineNames::new(volume.root().as_str()). against_real(trace, t, repo, root: &str, sink)
// builds EngineHistory's and Invariants' names from t's engine_root(); a later OpenTelemetry source, from Volume::root().
```

Composition roots (`trees_cmd`, conformance's `against_library` and `against_mount`, tests) build a `RootName` and
pass it to `Volume::init`; every later reader asks `Volume::root()`. Module map afterwards: `branch_name.rs` and
`tree_actor/root_name.rs` new; `tree_actor/mod.rs`, `journal.rs`, `tree_git.rs`, `trees_cmd.rs`, conformance `lib.rs`,
`library.rs`, `mounted.rs`, `engine_names.rs` (doc comment), `invariants.rs` (its tests name their own root) changed.

## 5. What moves where

| Site today (`tree_actor/mod.rs` unless named) | Becomes | Rule owner |
|---|---|---|
| `prepare_state_area`: `Agentfs-Tree: main`, `branch("main")`, checkpoint (675–689) | the given `RootName`; checkpoint `root` | init |
| `build_trees`: `contains_key("main")` (843) | design rule 1's check on the folded `root` | tree history |
| `fork`: `step/head/tree("main")`, `fork = (name, "main")` (879–892) | `self.root()` | tree history |
| compaction checkpoint (2014) | carries `root` | journal |
| `rebuild_owners`: `refs/queue/main/` (1389) | `queue_ref_prefix(root)` (only the root's queue holds landings while forks of forks are not exposed; fork queues hold sync blobs, 2218–2226) | tree history |
| `plan_sync`: `head("main")` (1552) | `head(upstream_of(tree))` | queue and locks |
| `pending_from`: `refs/queue/main/` (1629) | `queue_ref_prefix(upstream_of(tree))` | queue and locks |
| `progress_one`: sort `!= "main"` (1729) | sort by `!is_root` (root first, as today) | actor |
| `take_next`: `tree == "main"`, `source: "main"`, `refs/queue/{tree}/` (1748–1778) | `match upstream_of(tree)`, `queue_ref_prefix` | queue and locks |
| `enqueue_moves`: `== "main"`, `!= "main"`, `("main", head)` (2218–2226) | `forks_of` / `upstream_of` | queue and locks |
| `tree_git.rs:53` `HEAD`; unquoted `merge =` | `write_config(head, …)`, quoted values | tree_git |
| `trees_cmd.rs`: `println!("main …")`, `tree: "main"`, `MAIN`, `main-client` | `volume.root()`, `ROOT`, `root-client` | CLI |
| conformance `ENGINE_ROOT`, `engine_names()` | `CONFIGURED_ROOT` into `Volume::init`; names from `Volume::root()` | conformance |

Crossings go one way: CLI or harness → `RootName` → `Volume::init` → checkpoint → fold → `Volume::root()`. Unchanged:
`fork/deliver.rs`'s and `cli_fork_deliver.rs`'s `"main"` (an `--onto` branch, older engine) and their
`Reference::is_valid_name` checks: `BranchName` would refuse stored names (`HEAD`, `-x`) that open today.

## 6. Risks

| Risk | How the design avoids it |
|---|---|
| **Recording (principle 1).** A wrong queue prefix makes `pending_from` false; a sync could fast-forward past a queued landing (finding 51's class). | Every prefix comes from `queue_ref_prefix`, and `pending_from` asks `upstream_of`. A unit test under `dev/trunk` saves in a fork and asserts `pending_from(fork)` is true while the landing is queued and false after `take_next(root)`. The finding-51 fixture cannot stand in (its patch removes only `shows_the_same &&`). A state where `shows_the_same` holds while a landing waits may be unreachable; the implementer adds that end-to-end test if one exists. |
| A literal `main` survives. | Conformance (every scenario and seed) and two CLI scripts run under `dev/trunk`. A core end-to-end test (init, save, fork, landing, sync, reopen) runs under `dev/trunk` and `zz`, names on either side of `fork/`. |
| A root wrongly identified. | It is read, never inferred (rule 1). Unit tests: missing pointer, root with an upstream, both `Format`. |
| A name with `;`, `#`, `"` or `\` corrupts `config`. | One quoting helper; probe §9 reads `refs/heads/a"b;c#d\e` back intact. |
| A cut init cannot finish without the name. | `open` returns `NotInitialized`; `trees init --root-branch` finishes it. `e2e-tree-recording-faults.sh`'s re-init after `tree.init_before_format` passes the flag. |
| Conformance traces or digests change. | Traces carry `TreeId`s, not names. The model is untouched except one doc comment (`world.rs:363`). |

## 7. Slices, order and gates

**Order: 5b before 5.** (a) Step 5's gate is "conformance … 0 mismatched"; after 5b it runs under `dev/trunk`, so any
`main` step 5's split re-creates fails it. (b) 5b is small, a tracer before the large split. (c) Step 5 designs
against `is_root`, `upstream_of`, `forks_of` and `queue_ref_prefix`, and re-probes against main after 5b merges.

One PR, two slices. Every `trees init` caller gets the flag in S1, but nine of the eleven run only under
`phase-1.yml`: S1 is green alone on ci and image; those nine are first run by the step's one dispatch, on the PR head.

| Slice | Changes | Proven by |
|---|---|---|
| **S1, tracer: the engine takes the name** | `branch_name.rs`, `root_name.rs`, checkpoint field, format 3, `init`/`open`, every row of §5 but conformance's, quoting and `HEAD`, the CLI flag. Core and fuse tests build volumes through a per-file helper `init(dir)` = `Volume::init(dir, &RootName::new("main")?)`; reopen sites keep `open`. Conformance calls `Volume::init` with `RootName::new("main")`. Scripts: `e2e-trees.sh` and `e2e-tree-conformance.sh` pass `--root-branch dev/trunk` and name the root as `HEAD`; `trees-conformance.py` (`refs/queue/main/` at :412), `trees-mmap.py`, `trees-tmpfile.py` read it with `git symbolic-ref --short HEAD`; the other nine pass `--root-branch main`. New tests: `branch_name` against `git check-ref-format --branch`; reserved list; rule 1's checks; quoting read back by `git config --get`; `pending_from` (§6); two-name end-to-end, `init` mismatch, `open` on missing and empty directories, cut init, idempotent `init`, format 2 refused | Locally: fmt, clippy `-D warnings`, `cargo test --workspace`, `bash -n` and `py_compile` on the eleven scripts. ci (~5 min: `e2e-trees.sh`, `e2e-trees-ordering.sh` on FUSE) and image (~5–9 min). The nine phase-1-only scripts (conformance, page cache, restart, recording faults, ENOSPC, tree power loss and its check, object-dir syncs, measurements): **the step dispatch** |
| **S2: conformance off `main`, docs, page deleted** | §4's conformance block; `world.rs:363` comment; every doc edit in §8 | Light tier (its `agentfs-model/src` diff is one comment, VERIFYING.md's mechanical test): `run.py --anchors-only`, `ci/model-digest.sh check`, model, conformance and safety-net tests, fmt, clippy, through `ci/model-docker.sh`. ci, image, `docs_reference` |
| **Step gate** | the PR head | One `phase-1.yml` dispatch, the full tree-view run: S1's nine scripts and S2's conformance under `dev/trunk`. Step 4's took 73 min wall (run 43146523186, 2026-11-12, read with `gh run view`) |

`bench.yml` and `power-loss.yml` run no tree script (only `ci/bench.sh` and the older engine's power loss); the tree
power loss and measurements are the dispatch's jobs. Real cost: one dispatch for 5b, another for 5, two merges.

**Gate line moved, not dropped.** "The engine's check … and the model's `TreeNames::new` refuse the same names" holds
for the reserved names, each tested; for git's rule the model's `git init -b` accepts `HEAD` (§9), `RootName` refuses
it, and no outcome differs. That model change goes into module-cut row 6 (§8) and step 6 takes it.

## 8. Docs (every edit the PR makes)

- **Deleted (R1):** `docs/target-architecture/future/root-branch-name.md`; its row at `target-architecture/README.md:45`
  and its entry at `SUMMARY.md:74`.
- **Links to it, repointed to [trees](docs/trees.md):** `module-cut.md:26`; `git-model.md:74` and `rules-ledger.md:36`
  (baseline pages: the link only, no wording); `domains/volume.md:63` (and `root: RootName`). `OWNER-GUIDANCE.md:26`'s
  bracket ("the plan is …root-branch-name.md") becomes "[Built: `trees init --root-branch`.]". Code comments naming it:
  conformance `lib.rs:37` (deleted with `ENGINE_ROOT`), `engine_names.rs:3` (now `Volume::root()`).
- **`module-cut.md` otherwise:** row 5b moves above 5, "Built. …", no link; row 4's "from `ENGINE_ROOT` alone" becomes
  "from `Volume::root()`"; row 6 gains "The model's `TreeNames::new` refuses `HEAD`, as the engine's `RootName` does";
  line 174's sentence goes; Costs' "about 2 to 3 hours" becomes "one `phase-1.yml` dispatch, 73 min for step 4's".
  The page stays (rows 5, 6 and both later rows); the commit that builds its last row deletes it (R1).
- **Behaviour:** `docs/trees.md` (Terms: "A tree with no upstream is the root; its name is given at `trees init`";
  quick start; commands; format 3), `configuration.md` (`--root-branch`, `--writer` default, `ROOT`), `library/core.md`
  (`Volume::init`, `open`, `root`, errors), `library/filesystem.md` (quick start, "root view"), `architecture.md` (rows
  for the two new files; conformance names from `Volume::root()`), `supported-configurations.md` ("a root mount").
  `OWNER-GUIDANCE.md` gets Q1 and Q2's answers, dated, in his words.

## 9. Probes (real output)

```text
$ git check-ref-format --branch …   (git 2.43.0)
main ok | a;b ok | a#b ok | a"b ok | HEAD refused | -x refused | team/main ok | fork ok | fork/1 ok
refs/heads/x ok | @ ok | a..b refused | .x refused | x.lock refused | é ok
$ probe crate, git2 0.21.0: Branch::name_is_valid vs Reference::is_valid_name("refs/heads/"+n)
"HEAD" branch=Ok(false) ref=true | "-x" branch=Ok(false) ref=true | all other 17: branch == ref == check-ref-format
$ git config --get on   merge = refs/heads/a;b | merge = refs/heads/a#b | merge = "refs/heads/a;b"
fork/1 -> refs/heads/a | fork/2 -> refs/heads/a | fork/3 -> refs/heads/a;b
$ git config --get branch.f.merge   (value written "refs/heads/a\"b;c#d\\e")  ->  refs/heads/a"b;c#d\e
$ HEAD = ref: refs/heads/dev/trunk; git symbolic-ref --short HEAD  ->  dev/trunk
$ git init --bare -b NAME
HEAD exit=0 HEAD=ref: refs/heads/HEAD | a;b exit=0 | a..b: fatal: invalid initial branch name, exit=128
$ git apply --check crates/agentfs-conformance/tests/regressions/finding-51-sync-fast-forward.patch  -> exit=0
$ grep -rn root-branch-name -> 10 sites (§8) + .claude/errata.md (kept) | grep -l "trees init" ci/* -> 11; ci.yml 2
$ gh run view 43146523186  -> phase-1, workflow_dispatch, trees/module-cut-step-4, 03:11:38 to 04:24:33, success
```

NOT RUN: `cargo check`/`test` (no code written); model gate tiers, digest, corpus, conformance (forbidden here); CI,
dispatch, FUSE. Whether `shows_the_same` can hold while a landing waits is unproven either way.

## 10. Owner questions (both general guidance)

The root's name is the branch the volume's history is kept on, today always `main`. G1, G5 and G6 pull different
ways, so these are yours:

```text
iac ─ trees init --root-branch dev/trunk ─▶ journal: root = "dev/trunk" (the record) ─ every open ─▶ history.git/HEAD
```

**Q1. Where does the root's name live?** Background: principle 3 puts operator policy in the git directory, and the
old page proposed a git-config key. But the name is identity, not a setting: it cannot change after create (every
ref, trailer and queue carries it). The engine already owns `history.git/config` and rewrites it at every open.

| Option | Gives | Costs | You would see |
|---|---|---|---|
| **Journal, `HEAD` a copy (recommended)** | One record; git's own idiom (`git init --initial-branch` writes `HEAD`) | iac passes a flag once, not a key it can edit | `trees init --root-branch NAME`; `git symbolic-ref HEAD` shows it |
| A new git-config key in `history.git/config` | Settable like other policy | Config is no longer wholly the engine's; key and refs must be kept equal, a later edit refused at open | A key in configuration.md that only works once |
| Reuse `agentfs.branch` | No new name | One key meaning two branches across engines; its default is `agentfs/<volume>` | Confusion when both engines ship |

**Q2. When no name is given at create, what happens?** Background: your 2026-10-15 "Configuration with sensible
defaults" and your 2026-11-11 "should not be special or hardcoded" point different ways. The library always takes
the name explicitly; only the CLI could default.

| Option | Gives | Costs | You would see |
|---|---|---|---|
| **Refuse (recommended)** | A name fixed forever is always chosen, never inherited | The quick start gains a flag | "create it with `--root-branch NAME`" |
| Default `main`, CLI only | Shortest quick start; the engine still never compares to it | `main` is written into the CLI as a default | `trees init` makes `main` |
| Git's own default (`init.defaultBranch`, else git's fallback) | Follows the operator's git setting | The name depends on the machine running init | Whatever `git init` would pick there |

The recommended answers keep §4 as written. Q2's other answers change only the flag's default and leave nine scripts
untouched; Q1's others add a config key read at open and checked against the checkpoint.

## Review disposition

Review: root inferred (`txn/*`), finding-51 fixture, CLI never non-`main`, deliver untouched, model accepts `HEAD`,
`open` creates nothing, name order, escaping — all **changed**; byte-identical **moot**; questions — see Critic 6.
Critic: 1 doc inventory — **changed**: §8 lists all ten sites, the module-cut order, Costs line and row 4; bench and
power-loss claim corrected (they run no tree script). 2 HEAD hand-off — **changed**: carried in module-cut row 6;
module-cut.md's deletion stated. 6 P1/P2 — **changed**: asked as Q1, Q2 with honest options; old Q1 folded into G3.
8 `ROOT_BRANCH` — **changed**: a private `CONFIGURED_ROOT` feeds only `Volume::init`; names come from
`Volume::root()`. 9 S1 alone — **changed**: the gate table says which gate proves each script. 10 — **changed**:
`queue_ref_prefix`. 12 merge overlap at `world.rs:363` — **accepted**, a one-line comment. 3–5, 7, 11: other steps.
