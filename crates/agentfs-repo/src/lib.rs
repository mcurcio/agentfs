//! Checks over the whole repository, run as this crate's tests: the
//! documentation against the code, the workflows and CI scripts, the
//! sources' repository-wide rules, and the workspace's crates and their
//! ranks (docs/development.md, "Running tests"; docs/architecture.md,
//! "Ranks"). Its library is what those checks share with the binaries' own
//! documentation tests: a reader of the markdown in `docs/` and of the
//! workspace as `cargo metadata` describes it. Tests only: no published
//! crate depends on it, and its library depends on no other agentfs crate,
//! so a change to most documentation builds and checks in seconds.

pub mod doc_tables;
pub mod workspace;
