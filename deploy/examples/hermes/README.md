# Hermes rules for agentfs

The operator files for running Hermes on an agentfs volume of the tree
engine ([docs/trees.md](../../../docs/trees.md)):

| File | Becomes | What it does |
|---|---|---|
| [`config`](config) | the ConfigMap key `config`, included by `agentfs init --include` | the operator settings: nested repositories kept out (built), a remote and a patch source, logged as not built at every boot until push and the patch queue are built |
| [`exclude`](exclude) | `<state>/history.git/info/exclude` | an allowlist: only `skills/`, `SOUL.md`, `config.yaml` and each profile's `SOUL.md`, `config.yaml`, `profile.yaml` and `skills/` are recorded when new |
| [`attributes`](attributes) | `<state>/history.git/info/attributes` | `-agentfs` on secrets (`.env`, `auth.json`), databases, sessions, logs and checkpoints, even if tracked |

To use them, replace the files in [`../../sidecar/config/`](../../sidecar/config)
(or point an overlay's `configMapGenerator` at these). The pod changes for
Hermes, and the one-time migration Job that copies its existing data in
through the mount, are in [docs/deployment.md](../../../docs/deployment.md#10-hermes).

`crates/agentfs/tests/deploy_examples.rs` checks these rules against a
representative Hermes layout. Check the profile layout against the Hermes
version you run before relying on them.
