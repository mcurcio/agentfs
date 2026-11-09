# How agentfs records changes

This page says what agentfs records, when, and how it turns records into
commits. The promises are in [guarantees.md](guarantees.md); every key and
flag is in [configuration.md](configuration.md).

## The backing tree

The visible directory is a real directory on the backing volume. Reads and
writes go straight to the backing files. Because the files are real:

- hard links, `mmap`, `O_APPEND`, sparse files and SQLite behave normally;
- unlinking or renaming an open file behaves normally;
- permissions, ownership, timestamps and extended attributes are kept by the
  backing filesystem.

The daemon handles lookups, directory operations, renames, links and attribute
changes, and sees the change boundaries it must record. A change is attributed
to the inode's current paths at the moment of recording, not to the path it was
opened with. How the FUSE layer does this is in
[library/filesystem.md](library/filesystem.md).

The history lives in a separate **state area** on the same volume, never
visible through the mount (layout: [operator-guide.md](operator-guide.md)).

## Change boundaries

| Event | Recorded | Before the call returns? |
|---|---|---|
| `close` of a file changed since its last record | content and executable bit | yes |
| `rename` | source removed, destination added (a directory: one move) | yes |
| `unlink`, `rmdir` | removal | yes |
| `link`, `symlink` | new path | yes |
| `truncate`, or a change to the executable bit | new content or mode | yes |
| `mknod` of a regular file | the empty file | yes |
| `fsync` on a path in the `fsync` class | its whole group | yes, at most once per interval |
| last handle released, if changed after the last close | content | no: backstop for `mmap` and late writes |
| periodic sweep | content of files still open and changed | no: backstop for files never closed |
| unmount | every file still open and changed | at unmount |

Rules that make this exact:

- **Close records only if the inode changed.** `close` runs for every
  duplicated descriptor, for example each forked child that inherited stdout.
  Unchanged inodes are skipped, so a build writing to a log does not record the
  log thousands of times. A changed one is recorded at every close. Opening and
  closing a file without a change records nothing; creating one records it even
  when empty (`touch`).
- **Release is asynchronous.** Its result never reaches the caller, which is why
  writes through shared memory mappings, or after the last close, are recorded
  late. A writable handle is never given passthrough, so the daemon sees the
  write that a mapping makes.
- **A write and its release are one record** when nothing changed between
  them: a file written and closed at once is recorded once.
- **A shell's `>` is two records.** The shell opens the file, duplicates the
  descriptor and closes the original before the command writes. The file is
  recorded empty, then with its content. The two share a commit when they fall
  in one commit window; a slow command can put the empty file in a commit of
  its own, and reverting the later commit then leaves the file empty rather
  than removing it.
- **`O_TRUNC`** is part of the open, not a separate change.
- **Renaming a directory** is one record, however many files are under it.
  History carries what it holds beneath the source to the destination. Before
  each commit, agentfs walks the destination and corrects what recording each
  file would have decided differently: files the rules now exclude are removed,
  files the rules now record are added, files that no longer exist are removed.
  A commit never shows a moved tree without its corrections. If a correction
  keeps failing, `reconcile_failed` is raised and the move is committed as
  journaled ([runbooks/reconcile-failed.md](runbooks/reconcile-failed.md)). A
  rename inside an ignored directory records nothing. A move into a git
  directory commits as the source's removal.
- **FIFOs, sockets and devices** are never recorded.
- **Empty directories** are not recorded.
- **Hard links.** A change through a name the mount has never seen (a hard link
  made before the mount and never looked up) is not recorded at that name.
  Renaming one hard link over another of the same inode records nothing.

## The synchronous step

The work done before the call returns does not depend on file size:

1. **Freeze** the inode's current content into `pending/`, as a reflink where
   the backing filesystem has one (XFS, btrfs), otherwise as a copy. A file over
   the smallest remote blob-size limit is refused before it is copied, unless it
   is stored as LFS.
2. **Journal**: append a checksummed entry with sequence number, inode, paths,
   pending id, mode, trigger, caller UID, GID and PID, and time.
3. **Group commit**: one `fsync` of the journal covers every entry that arrived
   while the previous `fsync` was running.
4. Return.

Hashing, compression, object writes and commits happen afterwards. Without
reflink support step 1 copies the file, so volumes that hold large files should
use XFS. Measured costs are in [performance.md](performance.md).

Through a history-service front (several pods on one volume) the step is split:
the front checks the rules and freezes into its own slot, the service journals
and replies, and the front returns only after the reply, so a returned call is
as durable as in a single-process mount
([library/core.md](library/core.md)).

## Journal and commits

- Entries hold absolute state (a path's content and mode, or a removal), never
  deltas, so a replay is idempotent. The one exception is a directory move,
  which is why a commit skips entries at or below the head's `Agentfs-Seq`.
- A commit records the last journal sequence number it holds as a trailer
  (`Agentfs-Seq: <n>`). After a crash the recorder resumes from the branch
  head's `Agentfs-Seq` and replays later entries.
- The journal and `pending/` are truncated only after the commit and branch ref
  are durable. Objects are made durable before any ref names them.
- A journal entry written by a newer agentfs does not parse: opening it fails
  with an error naming `agentfs flush`, and nothing is discarded. Run
  `agentfs flush` with the newer version before downgrading.
- A leftover ref lock (`HEAD.lock`, `packed-refs.lock`, `refs/**/*.lock`) from a
  crash is removed when the volume opens, with a warning for each.

**Commits.** The commit loop wakes every `commitWindow` (default 200 ms) and
commits each group that can no longer grow. Entries close together in time are
grouped. Grouping affects only how history reads, never whether a change is
kept. A failed commit raises `CommitFailed` and the loop backs off (doubling, up
to 30 s) while the entries stay journaled. On shutdown everything left is
committed.

- **A commit whose tree would not change is not made.** Its records are
  consumed without one, so capturing an unchanged database costs a copy, not a
  commit. The exceptions are marker commits: a patch that changes nothing, a
  checkpoint's marker, and a fork's finish commit.
- **Atomic records** (an `fsync`-class group) are committed whole: if a
  commit-time check refuses any change in one, the whole record is dropped, with
  a `PathRefused` alarm.
- Inside a nested repository with `nested = working-tree`, the inner
  `index.lock` bounds a group.
- **Repack.** Every `agentfs.repackInterval` (default 1 h) the commit loop runs
  `git repack -d -q --geometric=2` between commits, so versions of a file
  changed in place are delta-compressed. It never prunes. It needs the `git`
  program (the container image ships it); without it every round raises
  `RepackFailed`. Commits wait while it runs. Memory is about 256 MiB plus the
  largest object it reads whole: budget that in the container's memory limit.
  Details are in [library/core.md](library/core.md).

### Checks on every path

Every path is checked against git's fsck rules and the remotes' limits twice:

- **At record time**, in the synchronous step. This is the gate: a path that
  fails is never journaled or tracked, and the rest of the record carries on.
- **Again at commit time**, as a backstop for records journaled before limits
  changed. A refused change is dropped from its commit.

A refused path stays in the backing tree, its last recorded content (if any)
stays in history, and `PathRefused` is raised. The rules:

| Rule | Threshold | git fsck id |
|---|---|---|
| Path length | over 4096 bytes | none: agentfs's own |
| Symlinked `.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`, in any NTFS or HFS+ spelling | | `gitmodulesSymlink`, `gitattributesSymlink`, `gitignoreSymlink`, `mailmapSymlink` |
| A path beneath a directory named `.gitmodules` or `.gitattributes` | | `gitmodulesBlob`, `gitattributesBlob` |
| Symlink target length | over 4096 bytes | `symlinkTargetLength` |
| Symlink target with a `.git` component, in any spelling | | `symlinkPointsToGitDir` |
| `.gitattributes` size | over 100 MiB | `gitattributesLarge` |
| `.gitattributes` line length | 2048 bytes or more | `gitattributesLineLength` |
| `.gitmodules` that git's config parser cannot read | | `gitmodulesParse` |
| `.gitmodules` submodule name empty or with a `..` component | | `gitmodulesName` |
| `.gitmodules` `url` that is an option, holds a newline, climbs out of its root, or is an `http(s)`/`ftp(s)` URL git refuses | | `gitmodulesUrl` |
| `.gitmodules` `path` starting with `-` | | `gitmodulesPath` |
| `.gitmodules` `update` starting with `!` | | `gitmodulesUpdate` |
| `.gitmodules` size | over 512 MiB | `gitmodulesLarge` |
| Blob size | over the smallest `remote.<name>.agentfsMaxBlobSize`, unless stored as LFS | none: remote policy |
| LFS object size | over the smallest `remote.<name>.agentfsMaxLfsObjectSize`, when stored as LFS | none: remote policy |

- There is one history for every remote, so the smallest limit across remotes
  applies. Changing a remote's limits takes a remount.
- Size checks are a `stat` and always run at record time. Content scans (the
  `.gitattributes` line length, the `.gitmodules` rules) run at record time for
  files up to 1 MiB; a larger file is scanned at commit time.
- `symlinkPointsToGitDir` and `gitmodulesParse` are only warnings in default
  git. agentfs always refuses them, so history stays acceptable to remotes with
  stricter fsck settings.
- The `.gitmodules` checks are a port of git's config parser and fsck checks,
  tested against the `git` program. agentfs refuses what either git 2.40's or
  an earlier release's URL check refuses.

## When recording fails

If the synchronous step fails (the state area is full, say):

- the call returns an error (`EIO` from `close`, or the operation's own error);
- the inode stays marked as changed, so the next boundary records it;
- `RecordFailed` is raised.

**Namespace operations** (rename, unlink, rmdir, link, symlink, setattr) have
already changed the backing tree when their record fails. They return `EIO` and
put the affected paths on a **repair queue**. The sweep (every
`sweepInterval`, default 30 s, at least 1 s, and at unmount) re-derives each
queued path's absolute state from the backing tree and records it. A repair is
idempotent and cannot undo a later change. Until it succeeds, history differs
from the tree at those paths. A `release` whose record fails queues its paths
too.

The queue survives restarts (`meta/repairs` in the state area), and the next
`agentfs mount` repairs what it lists before it serves any request. Paths still
queued at unmount are listed on exit with where they are saved; if the file
cannot be written either, the exit message lists them and says history differs
from the tree there until each path changes again.

Through a front, a service that is down, hung (no reply within
`--history-timeout`) or refusing fails a boundary the same way, raises
`HistoryServiceUnavailable` once per outage (`HistoryServiceRestored` when a
request succeeds again), and queues repairs in the front's own slot file. Its
sweep retries them, so history catches up once the service is back. A request
is never sent twice.

Operator steps: [runbooks/recording-failures.md](runbooks/recording-failures.md).

## Which paths are recorded

agentfs follows git's model: **ignore rules decide only whether new
(untracked) paths are recorded. A tracked path stays recorded until it is
deleted, even if a rule later matches it.** Rules never decide where data
lives; all data lives in the backing tree.

A path is **tracked** if it is in the branch head or was recorded and awaits
commit, so this holds during commit lag and after crash recovery. A removal is
recorded only if something is tracked at or below the path.

### Where rules come from

| Concern | Tracked (in the tree; edits are recorded) | Operator (in the git dir; iac sets them; invisible) |
|---|---|---|
| Ignore | `.gitignore` in any directory | `$GIT_DIR/info/exclude` |
| Attributes | `.gitattributes` in any directory | `$GIT_DIR/info/attributes` |
| LFS endpoint | `.lfsconfig` | `$GIT_DIR/config` |
| agentfs settings | none | `[agentfs]` in `$GIT_DIR/config` |

Safety settings are operator-only:

- `agentfs.*` attributes are read only from `info/attributes`.
- LFS endpoints and credentials are read only from operator config. A tracked
  `.lfsconfig` is content only: agentfs never reads it.
- Tracked `.gitignore`, `.gitattributes` and `.lfsconfig` files are recorded as
  content. They take part in rules only if `respectTreeRules = true` (default
  `false`), and even then they cannot set `agentfs.*` attributes or LFS
  endpoints.

Built-in system- and user-level git attribute and exclude files are never
consulted. One rule is built in: an NFS client's silly-rename name (`.nfs` and
24 hex digits) is never recorded. It stays visible through the mount, and a
restore never writes over it.

How the sources combine differs from git on purpose, so operator rules always
apply:

- Operator and tree ignore rules are separate layers, and either excludes a
  path. A tree `!pattern` cannot re-include an operator exclude.
- Within a layer git's rules hold: the last match wins, a deeper `.gitignore`
  wins over a shallower one, and nothing inside an excluded directory can be
  re-included.
- Tree attribute macros are kept apart from operator ones. `agentfs.*` values
  from tree files are dropped.
- Matching is always case-sensitive; `core.ignoreCase` is not consulted.
- Tree rule files count only if they are regular files, never through a
  symlink, and a file over 1 MiB is ignored.
- `info/exclude` and `info/attributes` are read when the volume opens. Changing
  them takes a remount.

### Attributes

Set these in the operator's `info/attributes`:

| Attribute | Values | Default | Meaning |
|---|---|---|---|
| `agentfs.commit` | `close`, `fsync` | `close` | Boundary class |
| `agentfs.coalesce` | duration, at least `1s` | `30s` | Minimum interval between `fsync` records of one group |
| `agentfs.group` | name | unset (a group of one) | Paths with the same group and the same stem are recorded together |
| `agentfs.maxSize` | size (`512k`, `100m`, `2g`) | unset | Content larger is refused (`max_size`), whatever the store |
| `agentfs.store` | `blob`, `lfs` | see LFS below | Force an ordinary git object or LFS; `blob` does not override `filter=lfs` |
| `-agentfs` | | | Never record this path |

- `-agentfs` stops writes to a path from being recorded even when it is already
  tracked. A removal of a tracked path is still recorded. History is never
  rewritten.
- Attribute patterns follow git: a directory pattern such as `secrets/` does not
  cover the directory's contents. Write `secrets/** -agentfs`.
- A bad `agentfs.commit`, `agentfs.coalesce` or `agentfs.maxSize` value fails
  the volume open with the line number, so a typo cannot silently turn a class
  off.

### Settings

```ini
[agentfs]
    respectTreeRules = false   # tracked .gitignore/.gitattributes are content only
    nested = exclude           # or working-tree
    commitWindow = 200ms       # group journal entries this close in time
    pushInterval = 60s
    sweepInterval = 30s        # backstop for files left open; min 1s
    repackInterval = 1h        # 0 or off disables; min 1m
    lfsThreshold = 8m          # unset by default (no size-based LFS)

[remote "origin"]              # one section per push remote
    url = https://github.com/acme/agent-history.git
    agentfsMaxBlobSize = 50m
    agentfsCredentials = /run/secrets/agentfs/origin
```

Settings and remotes may also come from operator-owned fragments named by
`include.path` in `$GIT_DIR/config` (a Kubernetes ConfigMap, say). Each include
must be an absolute path to a readable file (a missing one fails the volume),
must not set `agentfs.volume` or `agentfs.branch`, and must not be under the
backing tree or the mount point. `includeIf` is refused. Every key:
[configuration.md](configuration.md).

Rules are evaluated by agentfs over the pure-Rust `gix-ignore` and
`gix-attributes` parsers, fed only buffers agentfs reads itself, so nothing
global can leak in. libgit2 cannot do this: it always loads every working-tree
`.gitignore`, merges sources by git's precedence, and reads the global config.

## Nested repositories

A directory is a nested repository if it contains a `.git` directory; a `.git`
file or symlink that points to a git directory inside the tree; or a git
directory itself (the layout of a bare repository, or any mark of one still
being created). The last case covers `git clone --bare` and tool-managed bare
stores such as a checkpoint store.

- **Default (`nested = exclude`).** A nested repository's directory is not
  recorded: neither its working files nor its git directory.
- **`nested = working-tree`.** Working files are recorded, the git directory is
  not, and the inner `index.lock` bounds commit groups. A git directory is still
  skipped entirely, a submodule's included.

Rules that prevent surprises:

- **The mount root is never nested.** `git init` at the root makes an ordinary
  `.git`, which agentfs does not record.
- **Tracked paths stay tracked.** If `git init` runs in a directory whose files
  are already recorded, they keep being recorded. Only new paths under it are
  excluded. Nothing is deleted from history because of nesting.
- **Git's own data is skipped silently:** a path with a component exactly
  `.git` when that `.git` is a git directory (or is becoming one), a gitfile, or
  a `.git` symlink pointing at a git directory in the tree.
- **Other `.git` paths raise `PathRefused`** once per such `.git` per mount, so
  its contents never leave history without a trace. So does every other path git
  refuses: `.git` look-alikes (`.GIT`, `git~1`, `.git.`), names that are not
  UTF-8, and the checks above. A refused path is reported per path, never fails
  the rest of its record, and the operation still succeeds in the backing tree.
- With several nested repositories on a path, the outermost is reported.

Detection details:

- A repository under construction is detected by any of: a `HEAD` or
  `HEAD.lock` whose first line is a symbolic ref or an object id; a `config` or
  `config.lock` whose `[core]` section sets `repositoryformatversion`; or a file
  of git's default template (`description`, `info/exclude`, a `hooks/*.sample`
  starting with `#!`). Anything recorded before the marker appeared would
  otherwise stay in history, including later `config` edits that may carry
  credentials.
- At commit time, a write at a path new to history is checked again: if a
  proper ancestor has become a bare git directory meanwhile, the change is
  dropped silently. Two limits remain: a custom template's files committed
  before `HEAD` appears (one commit window apart) stay in history, and a new
  file recorded into an ordinary directory that becomes a bare repository
  within the commit window (`git init --bare` into a non-empty directory) is
  dropped.
- A `.git` file counts only if its first line is `gitdir: <path>`; a `.git`
  symlink is read, never followed. The target must be a git directory inside the
  backing tree, or a linked worktree's admin directory.
- Targets are paths in the workload's view of the mount. A relative target is
  resolved against the directory holding `.git`. An absolute target (what
  `git worktree add` writes) is mapped through the views agentfs knows: the mount
  point, other mounts of the same filesystem, and the paths given with
  `--workload-mount`. A target under no known view falls back to trying each of
  its suffixes from the tree root, counted in `agentfs.nested.gitdir_heuristic`;
  a nonzero count means a workload mounts the tree where agentfs was not told
  ([runbooks/worktree-heuristic.md](runbooks/worktree-heuristic.md)).
- These files are workload-controlled, so they are opened without following
  symlinks and without blocking, must be regular files, and are read only up to
  4 KiB.

## Metadata and permissions

- **Kept by the backing filesystem:** all metadata.
- **Recorded in history:** file content, the executable bit, and symlinks.
- **Not recorded:** ownership, other permission bits, timestamps and extended
  attributes. Recovery restores content and the executable bit only.
- **Access.** The kernel checks the real stored ownership and modes
  (`default_permissions`), so root and non-root callers behave as they would on
  the backing filesystem. `chown`, `chmod`, `utimens` and extended attributes
  are passed through. `chmod` of a symlink is not supported. POSIX ACLs are not
  enforced through the mount; only mode bits are.
- **New files belong to the caller.** With `CAP_SETUID`, `CAP_SETGID`,
  `CAP_CHOWN` and `CAP_FOWNER`, the daemon creates files, directories, symlinks
  and FIFOs as the calling user, so ownership is right from the instant the
  object exists. Without those capabilities everything it creates belongs to its
  own user. Creation is confined so that a name swapped for a symlink in the
  backing tree is refused (`ESTALE`) and nothing outside is touched.
- **Callers whose groups are unknown** (a caller outside the daemon's pid
  namespace) may get a file made by the daemon and then given to the caller; see
  [guarantees.md](guarantees.md#several-pods-on-one-volume).
- **Inode numbers** (`st_ino`) are agentfs's own, one per backing inode, shared
  by hard links, but not stable across remounts. Tools that cache inode numbers,
  such as git's index for a repository on the mount, re-read files once after a
  remount.
- **Confinement.** The daemon reaches the backing tree only through a
  descriptor opened at mount, never following a symlink in a path.

Mechanism: [library/filesystem.md](library/filesystem.md). A tool container that
runs as a different user than the file owner needs git's `safe.directory`; that
is a consumer setting, not an agentfs one.

## Commit metadata

**Committer: the replica that made the change.** Every pod mounting a volume
has its own committer identity, set by operator templates:

```ini
[agentfs "identity"]
    committerName  = {replica}
    committerEmail = {podUid}@{volume}.agentfs.invalid
    authorName     = uid-{uid}
    authorEmail    = uid-{uid}@{volume}.agentfs.invalid
```

Variables: `{volume}`, `{namespace}`, `{pod}`, `{podUid}`, `{node}`;
`{replica}` (the StatefulSet pod name, stable across restarts, otherwise the pod
name); and `{uid}`, `{gid}` (author templates only). A rendered identity must
pass git's fsck rules, or it is a configuration error at mount. The `.invalid`
domain can never belong to a real account, so commits are never linked to a
person on a hosted remote.

**Author: the writer process** as FUSE reports it. PIDs and UIDs are as seen
from the mount's own PID namespace.

**Trailers:**

```
Agentfs-Seq: 18342
Agentfs-Trigger: close
Agentfs-Namespace: agent
Agentfs-Pod: web-7c9d8-x2k4q
Agentfs-Pod-Uid: 0b6f…
Agentfs-Node: node-a
Agentfs-Uid: 10000
Agentfs-Pid: 4711
```

- A sidecar gets pod metadata from the downward API; the CSI driver from
  `podInfoOnMount`.
- Through a history service, each record keeps its front's origin (namespace,
  pod, pod UID, node, replica), so a replay after a crash commits it with the
  same committer. A commit never mixes origins. A front whose pod does not
  render its templates is refused when it connects. If the templates change so a
  journaled origin no longer renders, its records are committed with the
  service's own committer and keep the pod's trailers, with an
  `IdentityFallback` warning.
- Grouped commits carry one `Agentfs-Uid` and `Agentfs-Pid` line per distinct
  writer. A pid of 0 (an operator outside the daemon's pid namespace) is unknown
  and gets no `Agentfs-Pid` line.
- **Restores and reverts** are commits of their own, never grouped with workload
  changes. The subject is `restore <path> from <short id>` or
  `revert <short id>: <subject>`; the trailers are `Agentfs-Trigger: restore`
  and `Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>`; author and ids
  are the operator's.
- **Patches** are commits of their own too, authored by the patch's author and
  committed by the replica. The last paragraph carries `Agentfs-Trigger: patch`,
  `Agentfs-Patch: <patch commit>` and `Agentfs-Inbox: <n>`. Only that paragraph
  counts as proof a patch applied, so a path or a patch message cannot forge it
  ([patches.md](patches.md)).
- **A fork's finish commit** is a marker whose last paragraph carries
  `Agentfs-Trigger: finish`, `Agentfs-Fork`, `Agentfs-Source`, `Agentfs-Base`,
  `Agentfs-Change` (absent when nothing changed), `Agentfs-Fork-Remote` and
  `Agentfs-Unrecorded` ([forks.md](forks.md)).

## Large files and LFS

Large content can be stored as Git LFS objects, with pointer blobs in history
that an ordinary `git clone` plus `git lfs pull` turns back into content.

**Which store.** Decided when a file is recorded, on the frozen copy's size, and
saved in the journal, so a replay is deterministic:

1. **Always a blob:** empty files; git's special files (`.gitattributes`,
   `.gitmodules`, `.gitignore`, `.mailmap`) and `.lfsconfig`; a path no
   `.gitattributes` line can name exactly (one holding a tab, CR or LF, or whose
   line would reach git's 2048-byte limit). Symlinks are never LFS.
2. **`filter=lfs`:** LFS. `agentfs.store=blob` cannot override it. Operator
   `info/attributes` always counts; a tracked `.gitattributes` only with
   `respectTreeRules = true`. Otherwise a tree `filter=lfs` path is a blob, which
   git-lfs passes through unchanged, so clones still show the content.
3. **`agentfs.store=lfs` or `blob`** (operator attributes only).
4. **`agentfs.lfsThreshold`:** content strictly larger is LFS. Unset by default,
   so size-based LFS is off until an operator sets it; every remote then needs
   an LFS server.
5. Otherwise a blob.

The blob size limit does not apply to LFS content: history holds only the
pointer. LFS content has its own limit, `remote.<name>.agentfsMaxLfsObjectSize`
(the smallest across valid remotes applies); over it the file is refused
(`PathRefused`, reason `lfs_object_too_large`), never stored as a blob instead.

**Commit.** Each LFS write is hashed (SHA-256) into the local cache and committed
as a canonical pointer blob (git-lfs spec v1) with the file's mode. Hashing
happens at commit time, outside the synchronous step. The cache object is durable
before the branch moves.

**History must be readable by ordinary git.** Every commit rebuilds a block at
the end of the root `.gitattributes`, in history only (the backing tree is never
written):

```text
<the workload's own .gitattributes, byte for byte>
# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced
/big.bin filter=lfs diff=lfs merge=lfs -text
# agentfs-lfs-end
```

- One line per path currently stored as LFS, anchored and escaped so it matches
  that path alone, with the attributes `git lfs track` writes. A path leaves the
  block when it is removed, rewritten as a blob, or replaced by a symlink or
  directory.
- The block is last, so its lines win over the workload's. A deeper
  `.gitattributes` that unsets `filter` for a managed path would still make
  clones show pointer text; agentfs does not detect that.
- A block the workload wrote is removed; every other line is kept byte for byte.
  A root `.gitattributes` holding only the block is created and removed by
  agentfs.
- A pointer is committed only with its line. If the composed file would fail the
  checks (very many LFS paths, limits lowered below the head's own file), the
  commit falls back: its LFS writes become blobs (the blob limit applies), then
  its writes at or beneath the root `.gitattributes` are dropped with
  `PathRefused`, then the head's file is kept as is. Later commits are
  unaffected. The commit span carries `agentfs.lfs_as_blobs` when the first step
  ran.

**Cache.** `<state>/lfs/objects/aa/bb/<oid>`, git-lfs's layout. `agentfs lfs
fetch` fills it from the remotes: the objects the branch head references, those
of given revisions, or every object of the branch (`--all`); `--dry-run` reports
what is missing without contacting a server. Restores, reverts, patch applies and
`lfs fetch` hold the cache lock shared while they use it. Endpoints, credentials
and transfers are in [remotes.md](remotes.md#git-lfs-transfers).

**Restore and revert are LFS-aware.** A pointer at a managed path is checked out
as its object's content, from the cache or downloaded from the remotes in name
order. Every object a plan writes is fetched during the check, before anything is
written, so a missing one names its path and refuses the restore
(`lfs_object`). The root `.gitattributes` is compared and written without the
managed block. A dry run downloads and writes nothing.

**Retention.** LFS objects cannot be pruned from most LFS servers, and pruned
pointers break clones. agentfs never deletes anything on a server and never
prunes on its own. An operator runs `agentfs lfs prune`
([runbooks/lfs-storage.md](runbooks/lfs-storage.md)); it removes only cached
objects confirmed on every valid remote, refuses to run with a misconfigured
remote or none, takes the cache lock exclusive, and keeps recently cached
objects with `--older-than`. A remote added after a prune that lacks old objects
fails its push as `local` (stage `lfs`) with an `agentfs lfs fetch --all` hint.
Growth is limited at the source with operator rules, the `fsync` class's
interval and `agentfs.maxSize`.

**Not supported:** byte-level resumable uploads (git-lfs's `tus` adapter, which
no LFS server tested implements), git-lfs's pure SSH protocol
(`git-lfs-transfer`), proxy authentication, and `https://` or SOCKS proxies.

## Files changed in place: the `fsync` class

Databases and similar files are held open and changed in place. Recording them
only on `close` could mean never. The class gives them a different rule.

**Opting in.** Only the operator's `info/attributes` puts paths in the class; a
tracked `.gitattributes` cannot. The usual line for SQLite is

```text
*.db* agentfs.commit=fsync agentfs.group=sqlite
```

with `*.db*` so the pattern covers the `-wal` and `-journal` files too.

**Groups.** A group is the set of paths in one directory with the same
`agentfs.group` value and the same stem (`state.db`, `state.db-wal`,
`state.db-journal`). Every member must be in the class. A class path with no
group is a group of one. The group's interval is the smallest `agentfs.coalesce`
of its members. `-shm` files are never recorded: SQLite rebuilds the index from
the WAL. Two databases in one directory are two groups.

**Triggers.**

- An `fsync` or `fdatasync` on any member, if the group changed and no capture
  was attempted in the last interval. An `fsync` inside the interval is passed
  through and records nothing.
- A trailing timer: a group with unrecorded changes is captured one interval
  after its first change since the last capture (SQLite with
  `synchronous=NORMAL` does not `fsync` on commit).
- The close of the group's last open handle, and the release of its last handle.
  Closing one member while another stays open records nothing.
- Unmount.
- A rename, unlink, link or size change of a member is not recorded on its own:
  it marks the group changed, and the next capture records it, at most one
  interval later.
- During `restore` or `revert` through the mount, a group the operation changes
  is held back and captured once, whole, when it ends, so a restore never
  records a mix of two versions.
- `agentfs record <path>` records a member's whole group in one record; use it
  with the workload stopped.

**Consistent capture.** Writes to class paths go through the daemon, never
passthrough. A capture waits for writes in progress, blocks new ones, and copies
every member into `pending/` while nothing can change, so the copies are one
point in time, as a crash would leave them, which SQLite recovers from.

- **The cost is mount-wide for namespace changes.** While a group is copied,
  every rename, unlink, link, `mkdir`, `rmdir` and create anywhere on the mount
  waits for the copy. Reads and writes of other files do not. A 1 GiB database
  copied at 500 MB/s holds renames for about two seconds once per interval. Keep
  large databases on volumes whose workload is not rename-heavy, or raise
  `agentfs.coalesce`.
- A member that is gone is recorded as removed; any other read error fails the
  capture (`RecordFailed`) and leaves the group changed.
- **Whole or not at all.** A capture is one atomic record. If any member is
  refused (a check, `agentfs.maxSize`, a path rule), the whole capture is
  refused. A member that a path rule skips refuses the group too
  (`group_member_skipped`), since a database recorded without its WAL is not one
  point in time. A restored database plus its `-wal` or `-journal` always comes
  from one capture.

**Storage.** Members follow the same store rules as any path. `agentfs.maxSize`
refuses content larger than it, whatever the store (for a group, the whole
capture). There is no per-group budget beyond that and no pruning of old
versions.

**Guarantee and limits.** See [guarantees.md](guarantees.md#files-changed-in-place).

- `agentfs restore` and `agentfs revert` expand groups: restoring `app.db`
  restores its `-wal` or `-journal` from the same commit, in one operation. A
  member the plan cannot account for refuses the operation. Restoring a live
  database needs the workload stopped
  ([runbooks/recovery-and-undo.md](runbooks/recovery-and-undo.md#7-restoring-a-database)).
- A hard link to a member under a name outside the class is an ordinary file,
  and the class is per mount: a passthrough handle opened under a non-class name
  is not held by the gate.

## Secrets

agentfs does not scan content. See [guarantees.md](guarantees.md#secrets).
