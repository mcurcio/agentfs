# Forge CI on hidden fork refs

**Proposed, not built.** This page proposes checking whether each forge runs CI on a push to a hidden fork ref. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

A hidden fork pushes to `refs/agentfs/<fork>/history`, outside `refs/heads/`. Whether a forge (GitHub, Gitea,
GitLab) runs workflows or sends push webhooks for such a push is not verified. A bare repository runs only its own
hooks ([forks](../../forks.md)).

## What it proposes

Check each forge an operator relies on, before the operator chooses `hidden` to keep a sandbox's output away from its
CI, and record the answer per forge.
