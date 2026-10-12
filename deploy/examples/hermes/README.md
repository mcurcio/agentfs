# Hermes rules for agentfs

The operator files for running Hermes (design §16) on an agentfs volume:

| File | Becomes | What it does |
|---|---|---|
| [`config`](config) | the ConfigMap key `config`, included by `agentfs init --include` | settings and the push remote |
| [`exclude`](exclude) | `<state>/git/info/exclude` | an allowlist: only `skills/`, `SOUL.md`, `config.yaml` and each profile's `SOUL.md`, `config.yaml`, `profile.yaml` and `skills/` are recorded when new |
| [`attributes`](attributes) | `<state>/git/info/attributes` | `-agentfs` on secrets (`.env`, `auth.json`), databases, sessions, logs and checkpoints, even if tracked |

To use them, replace the files in [`../../sidecar/config/`](../../sidecar/config)
(or point an overlay's `configMapGenerator` at these). The pod changes for
Hermes, the one-time migration of its existing data and the initial import
are in [docs/deployment.md](../../../docs/deployment.md#hermes).

`crates/agentfs/tests/deploy_examples.rs` checks these rules against a
representative Hermes layout. Check the profile layout against the Hermes
version you run before relying on them.
