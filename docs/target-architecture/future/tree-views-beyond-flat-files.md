# Tree views beyond flat files

**Proposed, not built.** This page proposes directories, symlinks, non-UTF-8 names and more than one link to a file in the tree views. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The tree views support flat regular files with git-safe UTF-8 names, and only an anonymous first link. Nothing
else is supported ([trees](../../trees.md) states the limits).

## What it proposes

- **Directories**, with directory renames and their effect on the line maps and name versions.
- **Symlinks**, recorded as git records them, with the confinement rules of [the trees domain](../domains/trees.md).
- **Names that are not UTF-8**, kept apart from the git-safe set. A view may show such a name though git would refuse it.
- **Hard links past the first**, and a line moved between files.
- **Path `truncate(2)`**.

[git-model §16](../git-model.md#16-open-questions-costs-and-what-is-not-modelled) lists what the model does not cover.
Each addition needs model rules first, then the library, then the replay and the mounted tests.
