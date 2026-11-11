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
the engine with the engine's name, held in one constant, `agentfs_conformance::ENGINE_ROOT`.

## What changes

The name `main` is written into:

- agentfs-core: `tree_actor/mod.rs` (about 29 places outside tests) and `fork/deliver.rs`, with their tests
  (`tests/tree_actor.rs`, `tree_line_rules.rs`, `tree_pages.rs`, `tree_git.rs`, `tree_stages.rs`);
- agentfs-fuse: `tree_view.rs`;
- the CLI: `trees_cmd.rs` (`trees init` prints `main <head>`, `trees fork` forks the root, the `trees mount MAIN`
  meta-name, the `--writer` default `main-client`) and `tests/cli_fork_deliver.rs`;
- the tree scripts under `ci/` (`e2e-tree*.sh`, `e2e-trees*.sh`, `trees-*.py`, `trees-*.sh`, `measure-trees.*`);
- conformance's `ENGINE_ROOT`;
- the docs: [trees](../../trees.md) (the root, the quick start and the command table) and
  [configuration](../../configuration.md) (`--writer`).

## Proposal (design choices, for this step's architect)

- The name is a git-config value in the volume's git directory, set once by `trees init`.
- It reuses the existing names, `agentfs.branch` and `--branch`. The older engine's history branch is the same
  concept, and [agentfs in git terms](../git-model.md) §4 already maps the root's ref to today's
  `refs/heads/agentfs/<volume>`.
- The name is checked with git's own branch-name rules, and may not be `fork` or `txn` or start with `fork/` or
  `txn/`, where forks and transactions live. The model's `TreeNames::new` applies the same rule.
- The target Volume domain carries it in `VolumeSpec`, fixed at create ([Volume](../domains/volume.md)).
- Conformance configures the engine with a name other than `main` and passes it to the model, and `ENGINE_ROOT` is
  deleted.

## Open questions for the owner

Each is labelled "general guidance" or "absolute requirement" when it is asked.

1. **Existing volumes.** Their root is `refs/heads/main`. Choices: record `main` as their configured name (no ref
   moves); rename the ref at upgrade (history untouched, but the name changes on mirrors too); or recreate the volume.
   Background: guidance, 2026-11-02, "i would like for the git history to remain compatible across minor versions,
   and only break on a major version change".
2. **No name set.** Refuse to start, or use a default, and which default.

## Order

After [module cut](module-cut.md) step 5, which splits `tree_actor/mod.rs`, where most of the literals live, and
before step 6. Step 5's architect may pull it earlier.

## Gate

- The ci and image runs are green.
- A `phase-1.yml` dispatch passes with conformance configuring a root that is not `main`, and `ENGINE_ROOT` deleted.
- The full tree-view CI run passes (about 2 to 3 hours, [module cut](module-cut.md), "Costs").
- `docs_reference` passes with the key's row in [configuration](../../configuration.md).
- [Trees](../../trees.md) and [configuration](../../configuration.md) are rewritten, and this page is deleted.
