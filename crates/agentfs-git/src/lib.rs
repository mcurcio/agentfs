//! Git as both of agentfs's engines use it: the `git` program run in an
//! environment of its own with every repack's memory bounds, the process's
//! one libgit2 setup, and a tree volume's object store
//! (docs/architecture.md, "Technology choices", says why each runs where it
//! does). Nothing here knows a volume's recording rules.
