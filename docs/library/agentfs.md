# The library: `agentfs`

`agentfs` is the library under one name: the one crate a consumer depends
on. It holds no code of its own. Each of its modules re-exports one crate of
this workspace whole, so each item keeps the one home its crate gives it.
No crate of the workspace depends on it. `crates/agentfs-repo` checks that
its source holds re-exports only, one module for each library of the ranks
it uses but the recording engine ([ranks](../architecture.md#ranks)).

| Module | Crate | What it is |
|---|---|---|
| `agentfs::engine` | `agentfs-engine` | The tree engine: tree views, their sessions, the domains' store and the state area ([page](engine.md)) |
| `agentfs::contracts` | `agentfs-contracts` | The shared kernel: values, capabilities, each domain's trait, errors and events, and the ports ([page](domains.md)) |
| `agentfs::volume` | `agentfs-volume` | The Volume domain: `ensure`, the lifecycle and the release verdict ([page](domains.md)) |
| `agentfs::clients` | `agentfs-clients` | The Clients domain: attachments, grants and each fork's one writer ([page](domains.md)) |
| `agentfs::trees` | `agentfs-trees` | The Trees domain: each fork's lifecycle ([page](domains.md)) |
| `agentfs::service` | `agentfs-service` | One volume composed: its domains wired onto one store, their events dispatched, the drain ([page](service.md)) |
| `agentfs::rules` | `agentfs-rules` | The Rules domain: where each new name of a tree lives, recorded or untracked ([trees](../trees.md#rules-and-the-untracked-area)) |

## Depending on it

The crate is not published to crates.io. Depend on it by git, or by path
from a checkout:

```toml
[dependencies]
agentfs = { git = "https://github.com/mcurcio/agentfs" }
# or
agentfs = { path = "../agentfs/crates/agentfs" }
```

It has no features. It builds libgit2 (through `agentfs-engine`) and links
no TLS, no SSH, no FUSE and no OpenTelemetry SDK or exporter:
[`ci/check-deps.sh`](../../ci/check-deps.sh) checks what it links.

The crate's documentation carries one example, a doctest, on the consumer
API's route alone: the composition root mints the launcher's capability, the
service's `init` makes the volume and `Domains::stage` serves it, one client
bound to the root writes a file through its session's guard (its permission
bits and owner from `agentfs::contracts::meta`), closes it and
reads it back, and `unstage` drains and releases the volume, every name
reached through `agentfs::` alone. `cargo test --doc -p agentfs` runs it and
is the crate's gate: nextest does not run doctests, so `cargo nextest run -p agentfs`
fails with exit status 4, "no tests to run".

## Not re-exported

- **The recording engine, `agentfs-core`.** It is the other engine, with its
  own state format, and the library does not reach it: a consumer of it
  depends on `agentfs-core` by name ([its page](core.md)).
- **The filesystem layer, `agentfs-fuse`.** A tree view over FUSE is the
  `agentfs` binary's ([filesystem](filesystem.md)).
- **The leaves**: `agentfs-os`, `agentfs-git`, `agentfs-lines`,
  `agentfs-failpoints` and `agentfs-align`, mechanisms below the engines.
  A few of their types reach the engine's API: `agentfs_lines::Image` and
  the engine's `Error::Lines`, which carries an `agentfs_lines::Error` (and, in a [test
  build](engine.md#test-builds), `agentfs_os::file_lock::FileLock`). A
  consumer that names one depends on that leaf by name.

## The consumer API

A consumer serves a volume through the service ([its page](service.md)):
it is the composition root that mints the capabilities, and the service
wires the domains onto the engine's ports, as the `agentfs` binary does.
What it is meant to call:

- `contracts::caps::Minter::new` and `LaunchCap::mint`: the composition root
  mints the capabilities (C1 in the [domain
  invariants](domains.md#domain-invariants)) and hands them to a
  `service::Launcher`.
- The service: `service::init`, `service::end_fork`, `service::Domains::stage`
  and the calls of the `Domains` it answers, `unstage` among them.
- The shared kernel's values, errors and events (`agentfs::contracts`), and
  the domains' values they carry. Each item has one path, its own crate's: a
  tree's identity is `contracts::TreeId` and an entry's metadata
  `contracts::meta`, never re-exported by the engine. No crate the library
  re-exports gives another crate's item a second path (`pub use` or `pub
  type`), which `the_library_gives_each_item_one_path` in agentfs-repo
  checks ([development](../development.md#build-settings)).
- `operator_settings::OperatorSettings::read`, the one value `stage` takes.
- `session::Sessions` and its `TreeSession`, a bound client's one route to
  its tree ([sessions](engine.md#sessions)), reached through
  `Domains::sessions`.

### Outside the consumer API

These are `pub` and reachable through `agentfs` but are not for a consumer.
Nothing stops one from calling them: a consumer's build does not read this
workspace's `clippy.toml`.

- The domains' services (`VolumeService`, `ClientsService`,
  `TreesService`), and the engine's `state_area::TreeStateArea` (the one
  default-build opener of a `tree_actor::Recording`) and `store::TreeStore`:
  public for the service and the tests. A consumer that wires them itself
  composes a volume a second way, around the service. Use
  `service::Domains::stage`.
- The `StateArea` port's `create`, called directly: a trait method the
  Volume domain calls. Use the service's `init`, which makes a volume
  through `VolumeService::ensure` after its launch-scope check (VO6) and its
  judgement of what the state area holds (VO5).
- The guard's calls that name a tree and a writer (`write_open` and the
  other file calls `Recording::actor`'s guard takes a `WriterAt` for): the
  engine's call surface under sessions, which the replay and its tests use.
  Their writer is one no Clients row knows of. Write through a client's
  session (`Domains::sessions`), which Clients binds (C5).

The rest of the engine's test-only surface is not in a consumer's build:
the items behind `agentfs-engine`'s `test-util` feature, which is off by
default and which the library never turns on ([test
builds](engine.md#test-builds)). The rows, a client's end and History's own
store opening are `pub(crate)` in the engine.

The Rust API is not versioned: its public types may change between
versions.
