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

The crate's documentation carries one example, a doctest: a tree volume
made and opened through the engine's state area, which gives the volume its
id, then one file written, closed and read back, every name reached through
`agentfs::` alone. It takes two routes outside the consumer API (below):
the `StateArea` port called directly, and the actor guard's open with a
named writer. `cargo test --doc -p agentfs` runs it and is the crate's
gate: nextest does not run doctests, so `cargo nextest run -p agentfs`
fails with exit status 4, "no tests to run".

## Not re-exported

- **The recording engine, `agentfs-core`.** It is the other engine, with its
  own state format, and the library does not reach it: a consumer of it
  depends on `agentfs-core` by name ([its page](core.md)).
- **The filesystem layer, `agentfs-fuse`.** A tree view over FUSE is the
  `agentfs` binary's ([filesystem](filesystem.md)).
- **The leaves**: `agentfs-os`, `agentfs-git`, `agentfs-lines`,
  `agentfs-failpoints` and `agentfs-align`, mechanisms below the engines.
  A few of their types reach the engine's API: `agentfs_lines::Image`, the
  engine's `From<agentfs_lines::Error>`, and `agentfs_os::file_lock::FileLock`.
  A consumer that names one depends on that leaf by name.

## The consumer API

A consumer that serves a volume is its own composition root: it wires the
domains onto the engine's ports, as the `agentfs` binary does. What it is
meant to call:

- The shared kernel's values, traits, events and ports (`agentfs::contracts`),
  and `contracts::caps::Minter::new`: the composition root mints the
  capabilities (C1 in the [domain invariants](domains.md#domain-invariants)).
- The domains' services (`VolumeService`, `ClientsService`, `TreesService`)
  over those ports ([the shared kernel and the domains](domains.md)).
- The engine's `state_area::TreeStateArea`: the Volume domain's `StateArea`,
  `VolumeStore` and `QueueFacts` ports, which `VolumeService` is built on;
  its `recording()` is the served volume once it is open.
- `tree_actor::Recording`, a served volume, and the reads of the guard
  `Recording::actor` answers ([the engine](engine.md#git-backed-tree-views)).
- `session::Sessions`, a client's one route to its tree
  ([sessions](engine.md#sessions)).
- `store::TreeStore`, the domains' ports over a served volume
  ([the domains' store](engine.md#the-domains-store)).

### Outside the consumer API

These items are `pub` for the workspace's own crates (the FUSE layer, the
binary, the conformance replay) and reachable through `agentfs`, but they
are not for a consumer. Inside this workspace `clippy.toml` bans most of them
outside their one caller; a consumer's build does not read this workspace's
`clippy.toml`, so nothing stops a consumer from calling them:

- The `StateArea` port's `create`, called directly: `VolumeService::ensure`
  calls it after its launch-scope check (VO6) and its judgement of what the
  state area holds (VO5).
- The guard's calls that name a tree and a writer (`write_open` and the
  other file calls `Recording::actor`'s guard takes a `WriterAt` for): a
  writer no Clients row knows of. A client writes through its session,
  which Clients binds (C5).
- `TreeActor::fork_with`, `record_rows` and `drop_fork`: row writes the
  domains' store makes; call the store.
- `TreeActor::close_writer`: a client's end, which `Sessions::unbind` makes.
- `TreeActor::create` and `create_with`: a volume made with a hand-made id;
  `VolumeService::ensure` makes it through `TreeStateArea`, which gives it
  one (VO5).
- `tree_git::TreeGit::open`, `open_with_lease`, `init` and
  `init_with_lease`: a tree volume's git store is history's.
- `tree_actor::COMPACT_GATE_VAR`, `ChannelGate`, `CompactGate` and `Gap`:
  the compaction gate the crash tests drive.

The Rust API is not versioned: its public types may change between
versions.
