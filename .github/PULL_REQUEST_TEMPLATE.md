## What and why

<!-- What this changes and the problem it solves. Link the issue: "Fixes #123". -->

## How it was tested

<!-- Tests added or changed, and what you ran locally. Say what only CI can
check (FUSE, root, kind, Helm) and what is untested. -->

## Checklist

- [ ] The title follows [Conventional Commits](https://www.conventionalcommits.org/) (`type(scope): subject`).
- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass.
- [ ] Behaviour changes are in `docs/design.md`, new keys or flags in `docs/configuration.md`, and new failure modes have a runbook.
- [ ] User-visible changes are under **Unreleased** in `CHANGELOG.md`.
- [ ] The `ci`, `image`, `helm` and `docs` runs are green.
