# The root's published name

**Proposed, not built.** This page proposes how a consumer configures the branch name the root tree is published
under. It is built in milestone M4 of [one engine](one-engine.md), with the publisher. The commit that lands M4
deletes this page and moves the behaviour into the library or operator docs.

## Identity and name

A tree's identity and its published name are two things.

| | What it is | Who sets it | Where it lives |
|---|---|---|---|
| **identity** | `root` (the tree with no upstream), or `fork/<n>` | the engine | history (the `Agentfs-Tree` trailer), the journal, every internal ref and checkout |
| **published name** | the branch a remote shows the root under | the consumer, as configuration | the publisher's setting, read at boot by Replication; never in history and never in the volume's spec |

History records the identity, so a name a consumer changes rewrites nothing: the next push publishes the same
commits under the new name. Nothing in the engine compares a tree to a name. The root is the root because it has no
upstream ([module cut](module-cut.md), "The shape of the tree").

```text
   identity (history, journal, internal refs)            published name (the publisher's setting)
   TreeId::Root    = "root"     refs/heads/root    ──publisher──▶  remote: <volume prefix>/<configured name>
   TreeId::Fork(7) = "fork/7"   refs/heads/fork/7
```

Each volume publishes its refs under a prefix of its own, so several volumes can share one repository (principle 6
of `AGENTS.md`) without their roots meeting.

## What stands until then

The tree engine names the root by its identity, `root` (`refs/heads/root`, `Agentfs-Tree: root`), and publishes
nothing. [Trees](../../trees.md) and [configuration](../../configuration.md) describe it as it is. Until M4 there is no
published name, because nothing publishes.

## Proposal (design choices, for M4's architect)

- The published name is a git-config value in the volume's git directory, which iac sets (principle 3), read at boot
  by Replication. Changing it takes effect at the next boot.
- It is not part of the volume's spec ([Volume](../domains/volume.md)), so changing it is never a spec mismatch.
- It is checked with git's own branch-name rules.

## Open questions for M4's gate

Each is asked labelled "general guidance" or "absolute requirement".

1. **The key.** A new key, or the recording engine's `agentfs.branch` and `--branch` reused. Today `agentfs.branch`
   names the recording engine's history branch, is fixed at `init` and may not be set from an operator's include
   ([configuration](../../configuration.md)); reusing it while both engines ship gives one key two meanings. git's
   own `remote.<name>.push` is the idiom, but today's pusher sends explicit `<oid>:<ref>` refspecs on purpose.
2. **No name set.** Refuse to publish, or publish under a default, and which default.
3. **A rename.** What happens to the ref under the old name on each remote: left as it is, or deleted.
4. **The prefix.** Its form: the volume's name, its UUID, or both. The [git model](../git-model.md)'s
   `refs/agentfs/<volume>/` (its ref table, collision refs) stands for this prefix; M4 settles its form. The answer
   also decides whether a volume's name may change: under a name prefix a rename moves the volume's published refs,
   under a UUID prefix it moves nothing.

## Costs

- A rename leaves the old ref on remotes unless the answer to question 3 deletes it, and deleting a ref on a remote
  is a step towards rewriting what others read.
- What publishing under the same name a recording volume used, on the same remote, meets on the first push depends
  on question 4 ([one engine](one-engine.md), "Existing recording volumes"): a divergence, a refused push, or
  nothing at all.
- Under a UUID prefix, a volume re-created after its state was lost gets a new UUID and publishes a parallel history
  beside the old one, with no alarm.

## Gate

- M4's gate: a `phase-1.yml` run without the seed shards (about 20 min wall, [one engine](one-engine.md), "Costs"),
  and ci and image.
- `docs_reference` passes with the key's row in [configuration](../../configuration.md).
- The tracer publishes under a configured name, then under a changed one.
- [Trees](../../trees.md), [remotes](../../remotes.md) and [configuration](../../configuration.md) are rewritten, and
  this page is deleted.
