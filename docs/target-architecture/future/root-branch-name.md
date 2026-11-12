# Configuring the root branch's name

**Proposed, not built.** This page proposes that a volume is configured with the name of its root tree's branch,
instead of the engine naming it `main`. The commit that lands this slice deletes this page and moves the behaviour
into the library or operator docs.

## Why

- Guidance, owner, 2026-11-11: "refs/heads/main should not be special or hardcoded. Consumers will configure the
  name of the root branch. If that work is already planned, we can keep to the plan." It was not planned; this page is
  the plan.
- Guidance, owner, 2026-11-08: "there is no "main" in this new model of upstreams and forks". The root is the tree
  with no upstream ([module cut](module-cut.md), "The shape of the tree"); its name is not part of what makes it the
  root.
- Principle 3 of `AGENTS.md`: operator policy uses git idioms and git-config syntax in the git directory. Principle 6:
  several volumes may share one repository, so one fixed name cannot serve all of them.

## What stands until then

The tree engine names the root `main`: its branch is `refs/heads/main`, its commits carry `Agentfs-Tree: main`, and
its queue and collision refs sit under `refs/queue/main/` and `refs/collisions/main/`. [Trees](../../trees.md) and
[configuration](../../configuration.md) describe that engine as it is.

The executable model already takes the root's name as a value (`agentfs_model::world::TreeNames`), and its own runs
name the root `root`; the digest labels the root `T0`, so the name changes no outcome. Conformance replays against
the engine with the engine's name, held in one constant, `agentfs_conformance::ENGINE_ROOT`. The two names differ on
purpose, and conformance does not pass the engine's name to the model: `digest`'s unit tests show the model's outcome
does not depend on it.

## What changes

The name `main` is written into:

- agentfs-core: `tree_actor/mod.rs` (16 lines outside its tests: the root commit's `Agentfs-Tree`, its branch, the
  `refs/queue/main/` prefix, and each place that picks the root out by name), and the tests of `fork/deliver.rs`,
  `tests/tree_actor.rs`, `tree_line_rules.rs`, `tree_pages.rs`, `tree_git.rs` and `tree_stages.rs`;
- agentfs-fuse: `tree_view.rs`'s tests;
- the CLI: `trees_cmd.rs` (`trees init` prints `main <head>`, `trees fork` forks the root, the `trees mount MAIN`
  meta-name, the `--writer` default `main-client`) and `tests/cli_fork_deliver.rs`;
- the tree scripts under `ci/` (`e2e-tree*.sh`, `e2e-trees*.sh`, `trees-*.py`, `trees-*.sh`, `measure-trees.*`);
- conformance's `ENGINE_ROOT`, and whatever step 4 builds on it (its engine-naming type);
- the docs: [trees](../../trees.md) (the root, the quick start and the command table) and
  [configuration](../../configuration.md) (`--writer`).

## Proposal (design choices, for this step's architect)

- The name is a git-config value in the volume's git directory, set once by `trees init`.
- Its key and flag are open (question 3). [agentfs in git terms](../git-model.md) §4 maps the root's ref to the older
  engine's history branch, `refs/heads/agentfs/<volume>`, which `agentfs.branch` and `--branch` configure
  ([configuration](../../configuration.md)). Reusing them would make one key mean a different branch in each engine
  while both ship, and that key already has a default and may not be set by operator fragments.
- The name is checked with git's own branch-name rules, and may not be `fork` or `txn` or start with `fork/` or
  `txn/`, where forks and transactions live. The model's `TreeNames::new` applies the same rule. The model depends on
  no other crate, so the rule is written twice, once in each; each has its own test (the model's is in `digest`'s
  unit tests).
- The target Volume domain carries it in `VolumeSpec`, fixed at create ([Volume](../domains/volume.md)).
- Conformance configures the engine with a name other than `main`, and `ENGINE_ROOT` is deleted. The model keeps
  its own name.

## Open questions for the owner

This step's architect asks them at its design gate, before anything is built, each labelled "general guidance" or
"absolute requirement".

1. **Existing volumes.** Their root is `refs/heads/main`. Choices: record `main` as their configured name (no ref
   moves, but it is a path kept for old volumes); rename the ref at upgrade (history untouched, but the name changes
   on mirrors too); or recreate the volume. Background: guidance, 2026-11-02, "i would like for the git history to
   remain compatible across minor versions, and only break on a major version change"; and guidance, 2026-11-06, on
   state formats from earlier work, "drop support. we are pre-production and there is no need to worry about
   "supporting" old concepts". The tree views are a trial ([trees](../../trees.md): "Use them to try the model"),
   and the repository has no release tag; whether any tree-view volume is in use is the owner's to say.
2. **No name set.** Refuse to start, or use a default, and which default.
3. **The key.** A new key and flag for the root's name, or the older engine's `agentfs.branch` and `--branch` reused
   (see the proposal).

## Costs

- Every place above that names the root learns to take the name, and each test that names `main` either keeps it as
  its configured name or takes another.
- The root's name is checked in two places, the engine and the model, kept equal by their tests, not by one shared
  function.
- Until this step lands, conformance runs the engine under `main` only, so a literal `main` left in the engine stays
  unseen.

## Order

After [module cut](module-cut.md) step 5, which splits `tree_actor/mod.rs`, where most of the literals live, and
before step 6. Step 5's architect may pull it earlier.

## Gate

- The ci and image runs are green.
- A `phase-1.yml` dispatch passes with conformance configuring a root that is not `main`, and `ENGINE_ROOT` deleted.
- The full tree-view CI run passes (about 2 to 3 hours, [module cut](module-cut.md), "Costs").
- `docs_reference` passes with the key's row in [configuration](../../configuration.md).
- The engine's check of the name and the model's `TreeNames::new` refuse the same names, each in its own test.
- [Trees](../../trees.md) and [configuration](../../configuration.md) are rewritten, and this page is deleted.
