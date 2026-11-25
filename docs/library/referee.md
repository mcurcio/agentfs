# The library: `agentfs-referee`

`agentfs-referee` holds the tree views' landing rules: how a fork's change
lands in its upstream and what a sync shows the fork. It is pure. It reads
history only through its `Landed` port, writes nothing and gives back plans,
which the tree engine ([`agentfs-engine`](engine.md)) carries out in its
queue takes. The engine is its one user. The crate `agentfs` does not
re-export it, so it is not part of the library's consumer API
([the library](agentfs.md)).

The rules are those of the git model
([git-model.md](../target-architecture/git-model.md), §6 to §8) and of the
model's rules 13 to 15 for metadata, directories and links
(`crates/agentfs-model/MODEL.md`). [Landing directories, metadata and
links](engine.md#landing-directories-metadata-and-links) says what they
decide for those entries. [Architecture](../architecture.md#agentfs-referee)
maps every module.

## What it decides

| Call | What it answers |
|---|---|
| `land(upstream, base, incoming, prior)` | One change (`incoming`, made on `base`) into the upstream's head: a `Decision`, the snapshot the upstream gets and the change's collisions. A change lands whole or not at all; `prior` holds the upstream history's earlier decisions |
| `undecided(fork, upstream, landed)` | The fork's versions and records the upstream has not decided (`Undecided`), asked of history through `landed` |
| `sync(upstream, fork, undecided, last_lines)` | A sync: the upstream's snapshot with the fork's undecided versions kept (`Synced`). It decides nothing. `Synced::unruled` names the first entry the rules do not decide, which the engine refuses to show (`Error::Unruled` there) |
| `ended_files(upstream, undecided)` | The files the upstream ended that the fork keeps under an undecided name, whose last lines the engine passes to `sync` |
| `touches(before, after)` | A step's lock keys: every file whose name, lines or metadata differ, each path it had or has, every directory that changed |
| `consulted(base, incoming)` | The upstream's keys a landing reads beside its own (rule 14): a later step that touches one refuses the take |
| `with_directories(keys)` | Those keys with the key of every directory above each name, the root left out |
| `queue::eligible`, `queue::cut` | The queue's order (P1) and the `Cut` a prefix sync takes to, which stops at the first change an open file holds |

The values they read and return are the crate's too: `EventNumber` (a step's
journal number), `LineVersion` (`"<event>:<index>"`, ordered as its text so
no stored byte depends on the type; `NotALineVersion` for any other text),
`Versions` (a step's new line versions in order), `Lines` (a file's line map,
`agentfs_lines::LineMap<LineVersion>`), `MergeFileIdentity` and `NameHistory`
with its `NameVersions`. A tree as the rules read it is a `Snapshot` of
`File`s, name histories and directories, each entry's metadata the shared
kernel's `agentfs_contracts::meta::Meta`.

## Its port and its errors

`Landed` is the rules' one question to history: whether the save numbered
`save` is in the upstream's history (`landed`), and whether the commit that
wrote the fork's value of a record is (`record_landed`). Its error is the
caller's, an associated type that the rules' own error converts into, so a
failure reading history crosses `undecided` intact. The engine implements it
over its git store, asking git once per commit.

The rules' own failures are one enum, `agentfs_referee::Error`:

| Variant | Meaning | The engine's error |
|---|---|---|
| `Format` | The history read is not one the rules wrote: a live name with no file, a version recorded twice differently | `Error::Format` |
| `Ancestry(Broken)` | An ancestry that is not a forest: `Cycle` or `Missing` | `Error::Format` |
| `Lines(agentfs_lines::Error)` | A line map the rules built or read is not valid | `Error::Lines`, the same value |

## Purity, checked

The crate's rank, `landing-rules`, is marked `no_std = true` in the
workspace manifest ([ranks](../architecture.md#ranks)), as is `domain`:

- **The compiler.** Outside its tests the crate is `no_std`
  (`#![cfg_attr(not(test), no_std)]`): code that names `std`, and so the file
  system, a process or the network, does not compile. Its collections and
  strings come from `alloc`.
- **The repository's checks.** `the_pure_ranks_are_no_std` in agentfs-repo
  requires that line of each library of a pure rank, and no `extern crate
  std` in its sources. The spawn check reads its sources too: they start no
  process.
- **What it links.** Its line maps come without their I/O half:
  `agentfs-lines`' default feature `io` (file images and the diff calls that
  take a spawner) is off. `ci/check-deps.sh` builds the crate's library
  alone, so no other package turns `io` on, and refuses git2, libgit2-sys,
  libc, rustix, OpenTelemetry or that feature in what it links
  ([development](../development.md#build-settings) lists what these checks
  do not read).

## Tests

Its tests are its own: the rules' cases (renames against edits, deletes,
rename-overs, unfinished lines, names falling back through a chain, the
stored text of maps, name histories and collisions), with bytes saved into
maps through git's histogram diff. `cargo nextest run -p agentfs-referee`
builds and runs them in seconds, without the engine
([build settings](../development.md#build-settings)).
