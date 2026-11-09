# Large and binary files on trees

**Proposed, not built.** This page proposes binary and large files in the tree model. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The model's bytes are ASCII and binary files beyond NUL bytes are not modelled. The tree library stores whole files
and aligns lines once per save. Saving and landing a 100 MiB file is slow and slows every other save: see
[save and take speed](save-and-take-speed.md). The existing recording engine stores large files through git-lfs
([recording](../../recording.md)).

## What it proposes

A sketch: a file past a size, or one that is not text, is one opaque unit. It is never diffed, and a concurrent
change loses the whole file. Its content lives in a large-file store, with a pointer in the commit.

## Open choices

- The size and the content test that make a file opaque.
- Whether the large-file store is git-lfs as the existing engine uses it, or one for trees alone.
