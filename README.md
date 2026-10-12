# agentfs

A filesystem that records every change an agent makes.

agentfs sits in front of an ordinary directory as a FUSE filesystem. Any
process, root or not, uses the directory with normal file calls and never sees
git. Each time a file is closed after a change, or renamed, deleted or linked,
agentfs durably records the new state before the call returns. It then commits
it to a git branch and pushes at regular intervals to one or more git remotes.
The history lives outside the mount, where the agent cannot see or reach it.

Path rules use git's own files (`.gitignore`, `.gitattributes`, `.lfsconfig`,
`$GIT_DIR/info/*`). Nested git repositories are excluded by default.
Telemetry is exported over OpenTelemetry.

It is built in two layers:

1. **Core and daemon**: a Rust library and `agentfs` binary that run anywhere
   FUSE does, with no Kubernetes dependency. The first deployment is a sidecar.
2. **CSI driver**: gives pods agentfs volumes, including volumes several pods
   mount at once.

## Status

Phase 1 in progress. See [docs/design.md](docs/design.md) and the
[review record](docs/reviews/2026-10-12-adversarial.md).

Built so far, in `agentfs-core`: path checks, the journal (group commit,
crash recovery), frozen copies (reflink, else copy), the commit builder,
identity templates and settings, and path rules: operator and (optionally)
tree ignore and attribute rules with git's tracked/untracked behaviour, and
nested repository detection. The `agentfs` command has `init`, `status`,
`flush` and `record`; `record` stands in for the FUSE layer and reports the
paths the rules skip, with the reason.

## Building

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

libgit2 is built from source, so a C compiler is needed. HTTPS and SSH
support for pushing are not enabled yet.
