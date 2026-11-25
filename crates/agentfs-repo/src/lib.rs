//! Checks over the whole repository, run as this crate's tests: the
//! documentation against the code, the workflows and CI scripts, and the
//! workspace's crates and their layers (docs/development.md, "Running
//! tests"; docs/architecture.md, "Layers"). Its library is what those
//! checks share with the binaries' own documentation tests: a reader of the
//! markdown in `docs/` and of the workspace as `cargo metadata` describes
//! it. Tests only: no published crate depends on it, and it depends on no
//! other agentfs crate, so a change to the documentation alone builds and
//! checks in seconds.
