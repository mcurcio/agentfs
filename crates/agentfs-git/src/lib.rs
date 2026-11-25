//! Git as agentfs uses it below both engines: the `git` program run in an
//! environment of its own, with the count of the children it starts and
//! every repack's memory bounds, and the process's one libgit2 setup
//! (docs/architecture.md, "Technology choices", says why each runs where it
//! does). Nothing here knows a volume, a tree's layout or a recording rule.
