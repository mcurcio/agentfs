# The library: `agentfs-engine`

`agentfs-engine` is the tree engine. It serves git-backed tree views: one
actor per volume orders every save of its trees into a journal, builds them
into git history, lands forks into their upstream, and keeps the rows of the
Volume, Clients and Trees domains. It has no FUSE and no command line, and it
opens no remote. It is the recording engine's sibling
([`agentfs-core`](core.md)): neither depends on the other. A consumer reaches
it as `agentfs::engine`, through [the library crate `agentfs`](agentfs.md).

Use this layer when you build a different front end over tree views (the
[filesystem layer](filesystem.md#tree-views) is one), when you write tooling
that reads or changes a tree volume's state area, or when you build an
adapter like the [Kubernetes one](kubernetes.md).

This page lists the layer's main types and functions, with examples that
match the current signatures. `cargo doc -p agentfs-engine --open` is the
full reference. [Architecture](../architecture.md#agentfs-engine) maps every
module; [trees](../trees.md) says what a tree view does and promises.

## What the layer offers

| Need | Module | Main items |
|---|---|---|
| Git-backed tree views | `tree_actor`, `state_area` | `TreeActor`, `TreeStateArea` (the Volume domain's `StateArea` port; its `inspect` reads what a state area holds) |
| The domains' rows over a tree volume | `store`, `tree_actor` | `TreeStore` (the `VolumeStore`, `ForkStore`, `ClientStore` and `QueueFacts` ports), `Chain`, `RowDomain`, `RowWrite`, `RowEvent` |
| Line identity across saves | `agentfs_lines`, a leaf the library crate does not re-export: depend on `agentfs-lines` by name | `LineMap` (runs of lines, never bytes), `LineMap::align` (git's histogram diff over two `Image`s, read a line at a time), `LineMap::align_bytes_with`, `histogram_with` and `histogram_files_with` (the diff's `git` child started by the spawner passed: the tree engine passes `TreeGit::start`, a holder of the lease counted by `agentfs_git::git_cli`), `LineMap::append`, `LineMap::expand` (line by line, for the landing rules), `pieces` (where a merged file's lines are in its sources): the line-map library, generic over the caller's numbered version (`Id`) |
| The landing rules the engine's takes carry out | `agentfs_referee`, which the library crate does not re-export: the engine is its one user ([page](referee.md)) | `land`, `sync`, `undecided` through the `Landed` port, `touches`, the queue's `eligible` and `cut`, and the values they read (`EventNumber`, `LineVersion`, `MergeFileIdentity`, `NameHistory`) |
| OpenTelemetry instruments | `telemetry` | `observe_process`, `observe_unverified`, on a `Meter` the caller makes |

## Git-backed tree views

`tree_actor::TreeActor` is a git-backed volume whose saves are ordered by
one actor. It is not the recording engine
([`agentfs-core`](core.md)) and does not share its state format. `TreeActor::create(root, identity, root_owner, service)`
(a [test build](#test-builds)'s; a consumer creates through the service's `init`) makes a
volume with its identity (`agentfs_contracts::volume::VolumeIdentity`: a name
and an id). Its Root commit records the root directory with mode 0755 and
`root_owner` (an `agentfs_contracts::clients::FileOwner`, uid and gid, parsed from
`UID:GID`), or the creating process's effective uid and gid when `None`.
A launcher passes it as `VolumeSpec.root_owner`; `ensure` uses it only when it
creates the volume and never compares it, since a found volume's root owner is
content. `TreeActor::open(root, service)` (a test build's too; a consumer
stages through the service, whose state area opens with `Recording::open_with`)
recovers one and never creates (`Error::NotCreated`, `Error::Format` for
another format). `service` is the pod
context of the process that serves it, which its own commits carry. It holds
an exclusive lease on its state area (`Error::Busy` if another process holds
it). Every process the engine starts for the volume holds the lease with it:
its git children (the builder's `git fast-import` and its alignment's `git
diff`, a read's git, the repack's git), each started through
`agentfs_os::inherited_lock::InheritedLock::spawn_with` (`TreeGit::start`, a
repo test's rule). A holder cannot start a process of its own: a seccomp
filter refuses every fork, so a git command that forked would fail loudly
("cannot fork"); none of those the engine runs does.
`Recording::end_git_children()` ends every one of them at once, as a crash
ends them (SIGKILL), and returns once each has exited: then no process the
engine started holds the lease, and none starts. A call that needs git after
it answers `ENOTCONN` (`Error::TreeGit` of an I/O error), as does one whose git
it killed. Dropping the Recording after it makes the journal durable, ends at
most one batch as failed (`tree HistoryBuildFailed`) and unlocks the lease;
what the journal holds and history lacks is built at the next open, as after a
crash. `agentfs host` makes it, through the service's `Domains::abandon`,
before it exits 3 ([library/filesystem](filesystem.md#the-view-host-command)).
`state_area::TreeStateArea` offers it to the Volume domain as its
`StateArea` port, whose `inspect` reads what a state area holds (`Found`)
without the lease, takes no lock and writes nothing: `TreeStateArea::at(root).inspect()`.
A read that failed is `agentfs_contracts::volume::VolumeError::Unreadable`,
naming the path and the I/O failure. Its recorded calls are `write_open`, `write`, `truncate`,
`sync`, `close`, `mkdir`, `rmdir`, `symlink`, `rename`, `unlink`, `set_meta`,
`set_times` and `replace_file`; each write-open and namespace call names its writer as an
`agentfs_contracts::pod::WriterAt`, the writer and its pod context, which the
commit carries ([trees](../trees.md#volume-identity-and-trailers)). Every call names its tree by a `TreeId`, the shared kernel's (`agentfs_contracts::TreeId`; the engine does not re-export it): `root`, the
tree with no upstream, or `fork/<n>`, made at journal event `n`. `trees()`
lists them, the root first and then the forks by number; `TreeId` parses from
and prints as that text. A fork is made with its row, through the store
adapter below; `read` reads a saved file whole into memory and `files` lists them;
`file_map` gives a saved file as a `FileMap` (its pieces in the journal and the
checkout as built, never bytes in memory), which a reader reads with
`FileMap::read` or `reader` with the actor unlocked and keeps across later
saves, and `read_open` gives a write-open's; `take_next` and `progress`
move queued changes between a tree and its upstream. `drain_changed` returns
the objects queue takes changed, each with its `TreeId`. A close that leaves
the bytes as they were commits nothing. `Error::errno()` maps each error to the errno a file call answers
with. The behaviour, the state format and the current limits are in
[trees](../trees.md); the FUSE adapter is in the [filesystem
layer](filesystem.md#tree-views).

A served volume (`Recording`) gates each call that starts new work in a tree:
`admit(tree)` returns at once, or `admit_then(tree, then)` runs `then` at once,
while the tree's lag and overlay are under their limits or the builder has
failed, else at the built event that brings them under
([trees](../trees.md#history-trails-the-files)). Each release of the actor's
lock (`ActorGuard`'s drop) reports every tree's overlay to the gate.
`Recording::set_overlay_limit(bytes)` sets the overlay limit (the service's
`Domains::stage` passes `OperatorSettings::overlay_limit()`); `History::overlay_bytes(tree)`
gives a tree's overlay as last reported, in estimated bytes (0 before any
report), and `History::lag()` the lags and the builder's health.

How an open volume keeps its journal is an option of its open, `Upkeep`,
fixed for the life of the open and stored nowhere:
`TreeActor::{create_with, open_with}`, `Recording::{create_with, open_with}`
and `TreeStateArea::with_upkeep` take one, and `create`, `open` and
`TreeStateArea::at` use `Upkeep::default()`. The `create` calls are a [test
build](#test-builds)'s; `TreeStateArea` creates in every build.

| Field | Default | Meaning |
|---|---|---|
| `segment_bytes` | 64 MiB | The next frame starts a new segment once the current one has a record and the frame would take it past this |
| `compact_from` | 64 MiB | The floor of compaction's due length. Unread with a gate |
| `gate` | none | Crate-private, set by `Upkeep::gated`. A `CompactGate`: the compactor then compacts at every built event, and calls `at(gap)` at each of a compaction's five gaps (`Gap`), with no lock held. `Err` fails the compaction there |

In a [test build](#test-builds), `Upkeep::gated(gate)` is one frame per
segment with that gate, for a test that wants every built event to compact a
small volume. `ChannelGate::pair()`
is a gate in a test's hands: it gives the gate, a receiver of each gap the
compactor reaches, where it stays parked, and a sender of verdicts. Dropping
both ends leaves a gate that never parks. The library reads no environment
variable for any of it.

Compaction runs on history's compactor, behind the call path
([trees](../trees.md#the-journal)), so a caller waits on its events:

| Call | Returns |
|---|---|
| `History::checkpointed()` | The last checkpoint counted (`Checkpointed`): its `generation`, 0 for the one the volume opened at and one more for each this open published, and its `built` point |
| `History::compactor_through()` | The built point of the last built event the compactor is done with, compacted or not due |

A [test build](#test-builds) also waits on them: read the generation, act,
then wait past it. Nothing waits on a length or a timer.

Saves history could not check ([trees](../trees.md#when-history-cannot-check-a-save))
are flagged in their commits' `Agentfs-Unverified` trailer.
`TreeActor::verification()` gives the volume's `Verification` handle, whose
`counts()` (`UnverifiedCounts`: `unresolved`, `mismatch_confirmed`) never
takes the actor's lock; a launcher hands it to
`telemetry::observe_unverified` for the `agentfs.history.unverified` gauge.
`tree_actor::verify_flagged(root, by, release)` is `agentfs verify`: it takes
the state area's lock (`Error::Busy`), opens only its `history.git`, and
checks every standing flag with `release` (`tree_actor::release()`, this
binary as `agentfs <version> build <hash>`), answering a `VerifyReport` of
each `Checked` flag (`Outcome::Wrote(Verdict)`, `SameRelease` or
`NoEvidence`), the commits whose verdict note does not parse, and how many
flags stand. The notes refs are `tree_actor::VERIFY_REF` and
`tree_actor::EVIDENCE_REF`; `tree_git::TreeGit` reads and writes git notes in
git's layout (`notes`, `note`, `note_tip`, `write_note`, the last a
compare-and-swap of the notes ref).

### Operator settings

`operator_settings` reads and writes a tree volume's operator settings:
`<state>/config` (`operator_settings::FILE`), in git-config syntax, holding the
`include.path` lines a launcher writes (`agentfs init --include`). It is the
launcher's file, apart from `history.git/config`, which the engine rewrites and
its `git` children read.

- `write_includes(state, includes)` makes `includes` exactly the file's
  `include.path` lines. The file is written whole (a temporary file, fsync,
  rename, a directory fsync) with mode 0600. Each include must be absolute
  (`SettingsError::RelativeInclude`, nothing written).
- `OperatorSettings::read(state)` reads the file with libgit2, which follows its
  includes. It is the only way to make an `OperatorSettings` (the type has
  no `Default`), so the settings a volume is staged with were read. A missing file is empty settings; a file or include that cannot be
  read is `SettingsError::Read`. `includes()` gives the top-level includes as
  written, and `missing_includes()` those that do not exist: git skips them
  silently, so the launcher warns.
- `not_built()` lists every configured key whose capability tree volumes do not
  build yet, one per key, sorted. Each is a `NotBuilt { key, capability }`.
  Nothing configured is silently ignored.
- `overlay_limit()` gives `agentfs.overlayLimit`, the last value set, as a
  positive size, or `DEFAULT_OVERLAY_LIMIT` (32 MiB) unset; any other value is
  `SettingsError::BadValue { key, value, expected }`.

`classify(key, value)` is the one table. It compares the key lower-cased, and
the first match wins:

| Key | Capability |
|---|---|
| `include.path`, `includeIf.<condition>.path` | none: structure |
| `agentfs.nested` = `exclude` (any case) | none: tree volumes build it |
| `agentfs.overlayLimit` | none: tree volumes build it |
| `agentfs.lfsThreshold`, `lfs.*`, `remote.<name>.lfsurl`, `remote.<name>.lfspushurl`, `remote.<name>.agentfsLfsCredentials`, `remote.<name>.agentfsMaxLfsObjectSize` | `Lfs` |
| `agentfs.patch*`, `agentfsPusher.*` | `Patches` |
| `remote.*`, `agentfs.pushInterval`, `agentfs.forkRefs`, `http.proxy` | `Push` |
| anything else | `Other` |

`Capability::as_str` names each (`push`, `patches`, `lfs`, `other`) and
`Capability::meaning` says in one sentence what the volume does without it, for
the boot log. `telemetry::observe_process(&meter, &not_built)` registers the
`agentfs.not_built` gauge: per capability, the number of settings configured
and not built ([telemetry](../telemetry.md)).

### Directories, metadata, links and times

Paths nest: `a/b/f` is file `f` in directory `a/b`, and `""` is the root
directory, wherever a call reads an entry (`node`, `children`, `set_meta`,
`set_times`). Each component is at most 255 bytes (`Error::NameTooLong`).

```rust
use agentfs_contracts::clients::FileOwner;
use agentfs_contracts::TreeId;
use agentfs_contracts::meta::{Attrs, Perm};
use agentfs_engine::tree_actor::{RenameMode, SetTime};

let me = FileOwner { uid: 1000, gid: 1000 };
let exec = Attrs { perm: Perm::from_mode(0o755), owner: me };
v.mkdir(TreeId::Root, "bin", exec, &by)?;
let h = v.write_open(TreeId::Root, "bin/run.sh", &by, Some(exec))?; // created 0755
v.write(h, 0, b"#!/bin/sh\necho hi\n", false)?;
v.close(h)?;
v.rename(TreeId::Root, "bin", "tools", RenameMode::Replace, &by)?;      // everything under it moves
v.symlink(TreeId::Root, "run", "tools/run.sh", me, &by)?;
v.set_times(TreeId::Root, "run", None, Some(SetTime::At(0)), &by)?;  // no commit
for entry in v.children(TreeId::Root, "", 0, 100)? { /* name, number, kind */ }
```

- `write_open(tree, path, by, create)` creates a missing file when `create`
  gives its `Attrs` (permission bits and owner, `agentfs_contracts::meta`); `anonymous_open(tree, by,
  attrs)` is `O_TMPFILE`. `Attrs` is the part of an entry's `Meta` a caller
  chooses: the call gives the kind. In a set-group-ID directory a new entry
  takes the directory's group, and a new directory keeps the bit (Linux's
  `inode_init_owner`); an `O_TMPFILE` file does not (see
  [Limits](../trees.md#limits)).
- `mkdir` returns the directory's node number, `rmdir` removes an empty one
  (`Error::NotEmpty`), `symlink(tree, path, target, owner, by)` makes a link
  (mode `0777`, its bytes its target: 1 to 4095 bytes, no NUL) and returns its
  object, and `readlink` reads a link's target (`Error::Invalid` for anything
  else).
- `rename(tree, from, to, mode, by)` follows POSIX: a directory onto a file is
  `NotADirectory`, a file onto a directory `IsADirectory`, onto a directory
  with entries `NotEmpty`, into its own subtree `Invalid`, and
  `RenameMode::NoReplace` onto an entry `Exists`; onto an empty directory it
  replaces it, onto itself it changes nothing. A directory moves with every
  entry under it, each file keeping its object and identity; an open of one
  follows its object.
- `set_meta(tree, path, attrs, by)` is chmod and chown: the entry's kind kept,
  a step and a commit (`Meta by <writer>`) unless nothing changes.
  `set_times(tree, path, atime, mtime, by)` sets times (`SetTime::Now` is the
  step's time); it is a step that makes no commit.
- `node(tree, path)` gives a `Node`: its node number (`None` for the root; a
  view's inode is the number + 2), `Meta` (kind, mode, uid, gid), `Times`,
  length and link count. `node_at(tree, number)` finds a named entry by its
  number, with its path, and `open_node(tree, object)` a file no name reaches
  (an anonymous open's, or one unlinked while open). `children(tree, dir,
  skip, count)` lists one batch of a directory (`DirEntry`: name, number,
  kind).
- Times are each tree's own: a step's record time gives the mtime and ctime of
  what it changes (a save the file's, an entry added, removed or renamed its
  directories'), and history never holds them.

### Landing directories, metadata and links

A fork's change lands in its upstream, and a sync brings the upstream's
decisions to the fork, by the same rules as lines and names (rules 13 to 15 of
the model, `crates/agentfs-model/MODEL.md`). The landing rules
([`agentfs-referee`](referee.md)) judge them with the
change's first parent (what its writer saw), the change, and the upstream's
head:

- **Metadata.** A file's or a directory's kind, mode, uid and gid are one
  value; whether the entry is there is part of it. A change's new value lands
  when the upstream still holds the value the change's first parent holds, or
  already holds the same new value. Otherwise the upstream's value wins and
  the change loses whole, with cause `metadata`. A value is compared, not its
  history: an upstream that went from the seen value to another and back
  counts as unchanged, so the change lands over it. A file's metadata follows its
  identity, so a rename and a chmod of one file both apply, and an atomic save
  carries the new file's metadata. On a view, `chmod(2)` and `chown(2)` are
  two steps and two changes: a program that runs both (`install -m -o`, say)
  in two trees can collide on the value between them, with cause `metadata`,
  where one change of both would agree.
- **Directories.** A directory is its path: two trees making one directory
  with one value agree, and an empty directory is kept. A directory's rename
  is one change: every entry under it moves with it, each file keeping its
  identity, and the change lands or loses whole. A change loses whole, with
  cause `directory-not-empty`, when it removes or moves a directory under
  which the upstream has an entry its first parent lacks, moves a directory
  onto a path under which the upstream has such an entry, or puts an entry
  onto a path where the upstream has an unseen entry of the other kind (a
  file where a directory is). It loses whole, with cause `directory-gone`,
  when it puts an entry into a directory its first parent has and the
  upstream no longer has.
- **Links.** A link is a file whose bytes are its target and whose kind is
  `symlink`: its target follows the line rules and its kind the metadata
  rule. Of two concurrent retargets the upstream's wins; replacing a file by a
  link keeps the file's identity and changes its kind.
- **Syncs.** A sync takes the upstream's metadata and directories, except the
  fork's own values the upstream's queue has not yet decided (their commit is
  not in the upstream's history), and every directory one of the fork's
  entries still needs, until the upstream decides. So every entry's parent is
  a directory of its tree at every step, and a decided upstream `rmdir` or
  directory rename reaches the fork. One case the rules do not decide: an upstream
  entry the fork never had, under a directory the fork's undecided change
  removed or moved. A sync that would show the fork that case fails with
  `Error::Unruled`, logged as a failed take, and stays queued: it goes through
  once the upstream decides the fork's change, and until then the fork shows
  its own tree. A sync that delivers the upstream's decision with it passes
  through.

A losing change keeps its collision ref, as for lines and names. Its
landing commit's `Agentfs-Collisions` line lists each rule 13 or 14 loss as
`{"cause":"metadata"|"directory-not-empty"|"directory-gone","path":"<path>"}`,
the path a file's path or a directory's with a trailing `/`, before the
change's other versions, which lose `Whole`. A take's read keys include every
directory these rules judged and every path an entry of the change arrives
at: a step that makes, moves or removes an entry under such a directory after
the decision refuses the take, which is decided again.

### Rules and the untracked area

The Rules domain is its own pure crate, `agentfs-rules`: no I/O, built once.
`RuleSet::build(&RuleTexts { exclude, attributes }, Nested::Exclude)` takes the
bytes of `info/exclude` and `info/attributes` and refuses a rule tree volumes do
not build (`RulesError::NotBuilt` for an `agentfs.<name>` attribute or
`filter=lfs`, `RulesError::BadValue` for `agentfs=<value>`, each with its line).
`place(path, kind, PathFacts { parent_untracked, inside_repository })` answers
`Placement::Tracked` or `Placement::Untracked(reason)` for a new name, the
built-ins first; `never_records(path)` says whether `-agentfs` marks it.
`OperatorRules` is the operator layer alone: `ignored(path, is_dir)` gives the
`info/exclude` pattern that excludes a new path and `attributes(path)` what
`info/attributes` sets ([trees](../trees.md#rules-and-the-untracked-area)).

The tree actor reads `<state>/history.git/info/exclude` and
`info/attributes` when it opens, before anything is written; a rule not built
fails the open with `Error::Rules` (`EINVAL`). `TreeActor::rules()`,
`Recording::rules()` and `TreeSession::rules()` give the rule set. Every call
that makes a name (`write_open` with a create, `mkdir`, `symlink`,
`first_link`, and `rename` onto a name with no entry) refuses one the rules
place untracked with `Error::Untracked { path, reason }` (`EXDEV`), before
anything is journaled; a name that is not git-safe is refused first, as
`Error::Name`. Rules decide only for new names: a path with an entry stays
recorded, except that an open moves each tracked file or link `-agentfs` now
marks into its tree's untracked area and records its unlink as writer `agentfs`
(placed first, so a crash between the two places it again at the next open).

`replace_file(tree, path, attrs, src, len, by)` (and `SessionGuard::replace_file`)
is one step that leaves `path` holding exactly the `len` bytes read from `src`,
with `attrs`' mode and owner: the atomic save a program makes by renaming a
finished file over `path`.
An existing file keeps its object and identity, and its unchanged lines keep
theirs (its lines are aligned as a close's save aligns them); a missing file is
created as a create would make it, refused with `Error::Untracked` where the
rules place it untracked. The bytes go to the journal in chunk records just
before the step, so an error from `src` (`Error::Io`, `UnexpectedEof` when it
ends early) or a crash before the step leaves `path` as it was. The commit is a
direct save's, `Direct by <writer>` with no `Agentfs-Open`, queued for the
upstream as every save is; the same bytes and metadata answer `Saved::Unchanged`
and commit nothing. A directory at `path` answers `Error::IsADirectory`, a link
`Error::Invalid`, a missing parent `Error::NotFound`. Opens of the file keep
their own copies and save over it at their close.

`TreeActor::untracked_area(tree)`, `Recording::untracked_area(tree)` and a
view's `TreeSession::untracked_area()` give a tree's `UntrackedArea`,
`<state>/untracked/<tree>/`, made on first use (`path()` names it). It is
reached only beneath its directory: `stat(path)` is an `lstat`, `entries(dir)`
lists a directory and `sync_dir(path)` fsyncs one. Every change goes through
`writing()`, which holds the area against a fork's copy and gives an
`AreaWrite`: `parent(path)` opens a path's parent for an `*at` call,
`containers(path, meta_of)` makes the directories above a path with the modes
and owners of the tracked directories they stand for, and `place(path, src,
len, meta)` puts a file or link in place atomically (written under
`<state>/untracked/.staging/`, fsynced, renamed). A caller holding it never
takes the actor's lock. `fork_with` copies the upstream's area as the fork's before the
fork's step, holding the upstream's changes while it copies; `drop_fork`
removes a dropped fork's area once its drop is recorded, and an open removes
every copy no fork owns.

### Sessions

`session::Sessions::new(recording)` holds the history service's sessions of a
served volume and is Clients' `SessionBinder`: `open()` gives a new
`SessionId`, unique per run; Clients binds a client's authority to it (a
`ClientCap` for a read-write client, its tree for a read-only one). A view
reaches its tree only through `sessions.session(id)`, a `TreeSession`, whose
`lock()` takes the actor's lock and reads the binding under it. The
`SessionGuard` it gives has `TreeActor`'s file calls with the tree and writer
left out: a write answers `Error::ReadOnly` (`EROFS`) without a writer, and a
handle call answers `NotFound` unless the open is this tree's and this
writer's. `admit_then`, `durable_then` and `durable_all_then` run unlocked.
`flush` answers a FLUSH at once, with no wait for the disk: `Error::Stopped`
once a journal fsync has failed, never that fsync's own error. `durable_all`
blocks the caller until every record so far is durable: what a view waits for
before a change outside the journal destroys bytes a record carries.

A RELEASE goes in two halves ([pending saves](../trees.md#pending-saves)).
`TreeSession::start_close(handle)` begins the close under the lock and gives a
`tree_actor::Closing`: `Saved(saved)` when it saved at once, or
`Decide(SaveToDecide)`, a pending save. `SaveToDecide::finish()` decides it with
the lock released and lands it under the lock, after any earlier-begun save of
the same file; dropped unfinished, a panic included, it stays pending as a failed
close (counted in `failed_saves`). `close(handle)` and `Recording::close_with` are
both halves on the caller's thread, for the CLI, tests and recovery;
`Recording::start_close_with(handle, check)` is the first half with a check run
under the same hold, which `TreeSession::start_close` uses.
`SessionGuard::open_map(path)` is what an open reads: the file as this client's
pending saves leave it (their own calls and the page-write pieces their saves
take, replayed on the saved map in close order), else the saved map; never
another open's unsaved calls, and it never waits for a save. **`open_map` and
`write_open` may release the guard's lock inside them and take it again**: when
the client's pending saves need a fold, the guard's hold takes a snapshot and
the fold runs with the lock released, so other views go on meanwhile. Anything
read through the guard before the call may have moved when it returns: read
what a reply names from the open (`open_object`), not the path again. The
content a read-only open gets is as of its snapshot; a write-open is recorded in
the snapshot's hold and gets its view in a second hold, with the file's page
writes made meanwhile on it. A failed fold closes a write-open, as a client's
end closes an open (saved after the pending saves it would have folded, not a
failed save), and is its answer; `Error::NotFound` when the client ended
meanwhile; `Error::Stopped` when
the actor stopped. `TreeActor::write_open`, the library's one-hold call for the
CLI and tests, folds under the lock. `unlink`, `rename` (onto `to`),
`replace_file`, `truncate_path` and `set_times` answer `tree_actor::PathStep`: `Done(value)`,
or `WaitFor(SavePending)` with nothing done while the file they would delete or
replace has a pending save. `SavePending` is made only by the core and must be
used: hand it and a retry of the whole call to `when_saved_then(pending, then)`,
which runs `then` once no save of that file begun before the refusal is pending
(at once when none is, else on the thread that lands the last of them, with the
lock released; `Error::Stopped` when the actor stops first);
`Recording::when_saved_then` is the same without a session. `SavePending::before()`
is the journal's next record at the refusal. The retry takes its guard with
`TreeSession::lock_retrying(Some(pending))`, which refuses it again only for a save
begun before that refusal, so a save begun later lands after the call and a client
that keeps closing the file never keeps it waiting. A rename of the pending
file itself goes first. `pending_save(path)` names the file's pending save, if
any, for a caller that reads the file to put it elsewhere (a move out of the
tree). Once the actor stops, a `finish` queued behind another save of its file
and a `settle` wake and answer `Error::Stopped`. `Recording::settle` waits until
no save begun before it is pending (a close whose close record is below the
journal's next record when it starts; one begun after it, of any file, does not
hold it), then until history is built through every record by then; it is never called from a
thread that finishes saves. `TreeActor::metrics()` counts them in
`pending_saves`, `oldest_pending_save_ns` and `failed_saves`
([telemetry](../telemetry.md#pending-saves)).

`unbind` removes the binding and, under the same lock,
`TreeActor::close_writer` closes every open of the client's writer as a save,
in handle order, with no wait for the disk: each gets its close record first,
all before any save, so the end's order survives a restart; an open of a file
with a save in flight begun before the end is saved when that save lands, after
it. A failure (of an append or a save) is logged and an unsaved close stays
pending, saved at its tree's next step. Lock order: the actor, then the bindings.

### The domains' store

The Volume, Clients and Trees domains keep their records as rows on the
volume's one tree actor, through the ports in `agentfs_contracts::ports`;
their services, rows, events and invariants are on [the shared kernel and
the domains](domains.md). `store::TreeStore::new(recording)` implements the
ports over a served volume (`Arc<Recording>`): Trees' `ForkStore` (`load`,
`create`, `end`, `drop_fork`), Clients' `ClientStore`, Volume's `VolumeStore`
and `QueueFacts`. Each write is one journal step, the same commit point as a
save: a fork's branch and its row are one record, or a row step holds rows
alone. The store makes it durable (one group-commit fsync, outside the
actor's lock) before it returns. Each row is the domain's value with its
audit stamps (`agentfs_contracts::audit::Audited`): the store sets `created`
at a row's first write and keeps it, and sets `changed` at every write.

The actor names the chains but not their meaning: a row names its chain
(`tree_actor::Chain`: `Volume`, `Trees(fork)` or `Clients(tree)`), whose ref
form the actor checks, and its event (`RowEvent`: a kind such as
`ForkCreated`, a detail, the principal as `Agentfs-By` text, and its pod
context); what a row holds is the store's. `TreeActor::row(chain)` and
`rows(domain)` read the rows from
the actor's saved state, never Git, so they answer while history lags or the
builder fails. `TreeActor::fork_with`, `record_rows` and `drop_fork` run the
steps; they are crate-private (`fork_with` and `drop_fork` `pub` in a [test
build](#test-builds)), and only the store calls them (a clippy rule). `drop_fork(fork, rows)` is one
step that deletes the fork's branch and its queue's entries with its rows, and
forgets the tree at once; it answers `Busy` (the store's `Refused`) while its
upstream's queue holds an entry it queued, a file is open in it, or a fork names
it, so a drop never needs history. It is the one judge of whether a fork may
drop. An event carries its caller's pod context; an event by `agentfs` (a drop,
a claim's release, an end or a release the domains judge) carries
`service_at()`, the history service's, filled in by the store. The queue facts
are the journal's too: `queued()` never waits for history, and a stopped actor
answers `Stopped`, never an empty queue.

`TreeStateArea` is Volume's `StateArea`, `VolumeStore` and `QueueFacts` in one:
`open` recovers the volume and makes a `TreeStore` over it, to which the other
two delegate; before `open` the row, a write and the queues answer `Stopped`:
never a default nobody read.

`StoreError` from a tree actor failure: `Stopped` and an unknown fsync
outcome are `Stopped` (the reopen decides whether the step survived); a failed
append is `NotDurable` (nothing recorded); damaged state is `Damaged`; a step
the actor refused (a drop while anything holds the fork, a row or upstream
that is not there) is `Refused`. One function maps a tree actor failure to
its `IoKind` for every adapter (`store::io_kind`).

## Telemetry

The tree engine's instruments are `telemetry`'s, registered on a `Meter` the
caller makes (the `agentfs` binary's, the API's no-op meter when metrics are
off, which drops them): `telemetry::observe_process(&meter, &not_built)` once
per process, for `agentfs.not_built` and the latency figures, and
`telemetry::observe_unverified(&meter, counts)` once per served volume, for
`agentfs.history.unverified`, read from `counts` while it lives. The library
uses the OpenTelemetry API only and starts no exporter. The signal names are
in [telemetry](../telemetry.md).

## Errors

Errors are `thiserror` enums, one per area; none is a catch-all.

| Type | Raised by |
|---|---|
| `tree_actor::Error` | Tree views; `Error::errno()` maps each to the errno a file call answers with |
| `tree_git::Error` | The tree volume's git store |
| `agentfs_contracts::volume::VolumeError` | `TreeStateArea`'s `StateArea` port |

## Test builds

The items only the workspace's tests and test builds call are behind the
`test-util` feature, off by default (the pattern of tokio's `test-util`). The
library crate `agentfs` never turns it on, and `ci/check-deps.sh` fails when
its build would (`ok   library features`). With it:

- `pub` where the engine also calls them, `pub(crate)` without it:
  `TreeActor::{create_with, open_with, fork_with, drop_fork}`,
  `Recording::{create_with, open_with}`, `TreeGit::{open_with_lease,
  init_with_lease}`, and `tree_actor::CompactGate`.
- Present only with it: `TreeActor::{create, open}`, `Recording::{create,
  open}`, `tree_actor::wait_for_lease`, `TreeGit::{open, init}`,
  `Upkeep::gated`, and `tree_actor::COMPACT_GATE_VAR`, `ChannelGate` and
  `Gap`.
- Present only with it, compaction's waits and the lock's count:

  | Call | Returns |
  |---|---|
  | `History::wait_checkpointed(after)` | Once a checkpoint of a generation after `after` is published. `WaitError::Stopped` at shutdown or once a compaction stopped the actor |
  | `History::wait_compacted(generation)` | Once the compaction that published that generation has ended: the segments it covers are unlinked, or its unlink failed and was logged. The publish comes before the unlink |
  | `History::wait_compactor(through)` | Once the compactor is done with a built event at or past `through` (`History::compactor_through`) |
  | `stages::holds_on_this_thread()` | How many holds of the actor's lock the calling thread has begun |

`TreeActor::record_rows` and `close_writer` are crate-private in every build.
The engine's own tests reach these items through a dev-dependency on itself
with the feature; the FUSE layer's and the binary's tests, the binary's
`failpoints` build and `agentfs-conformance` turn it on. A crate that depends
on the engine by name can still turn it on. `Minter::new` is not behind it: a
consumer's `main` is its composition root and mints.

## What this layer does not do

- It does not mount anything or speak FUSE. That is the [filesystem
  layer](filesystem.md#tree-views).
- It has no command line, takes no signals and starts no OpenTelemetry
  exporter. The `agentfs` binary does those.
- It opens no remote: it does not push, fetch or apply the patch queue. A
  tree volume's operator settings for those are reported as not built
  (`agentfs.not_built`).
- It does not share the recording engine's state format, and neither engine
  depends on the other.
- Its public types may change between versions; the Rust API is not
  versioned.
