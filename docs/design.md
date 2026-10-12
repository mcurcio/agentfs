
| `RemoteMisconfigured` | A `[remote "<name>"]` section is invalid, so that remote is left out (§11.1); raised when a volume opens and by `agentfs push` |# agentfs design

Status: draft 2, 2026-10-12, updated as phase 1 is built. Draft 1 was reviewed
adversarially. See [reviews/2026-10-12-adversarial.md](reviews/2026-10-12-adversarial.md)
for the findings and what changed because of them.

## 1. Objective

agentfs records every change an agent makes to a directory, in git history
the agent cannot see.

- **Ordinary files.** Any process, root or not, uses the directory with ordinary
  file calls and never sees git. The files live in a real directory on a backing
  volume.
- **Recording.** agentfs sits in front of that directory as a FUSE filesystem.
  When a file is closed after being changed, or renamed, deleted, linked or
  marked executable, agentfs durably records the new state before the call
  returns. It later turns recorded states into commits.
- **Pushing.** Commits are pushed at regular intervals to one or more git
  remotes. GitHub is one possible remote, not an assumption.
- **Logical projection.** Logically, the branch's history is the sequence of
  states of the directory. Physically, the directory is a normal tree, and the
  history lives outside the mount.
- **Rules.** Path rules use git's own idioms: `.gitignore`, `.gitattributes`,
  `.lfsconfig`, `$GIT_DIR/info/*` and git-config syntax.

### 1.1 Threat model

agentfs protects against an agent's **mistakes**, not a malicious agent. It
exists to:

- show every change an agent made, so mistakes can be found and recovered from;
- sync that history to remotes;
- let infrastructure-as-code (iac) change files with versioned patches instead
  of overwriting whole files.

"Invisible" means no history, journal, state or credential is reachable through
the mount. agentfs does not try to resist a workload that is actively trying to
defeat it.

### 1.2 What "every change" means

A change is recorded at each **change boundary** (§3.1), before the call that
ends it returns. Intermediate states between boundaries, such as the bytes of a
file between two writes, are not recorded separately. A few boundaries can only
be recorded after the fact; §3.1 lists them.

### 1.3 Baseline, extras, phases

| Phase | Contents |
|---|---|
| 1 | The recorder as a native sidecar for one pod: recording, commits, interval push to any remote, path rules, operator-only safety settings, push checks, OpenTelemetry, a supported manual recovery path |
| 2 | Git LFS, and the `fsync` class for databases (§8) |
| 3 | The ordered patch queue for iac (§12) |
| 4 | The CSI driver, including volumes mounted by several pods at once (§14) |

### 1.4 Not goals

- **Enforcing content rules.** agentfs records; it does not reject writes.
- **Detecting secrets.** agentfs does not scan content (§10).
- **Pushing remote changes into a running workload.** Remote changes enter only
  at mount time.
- **Showing history to the workload.**

## 2. Architecture

```
 workload processes
        │  ordinary file calls
        ▼
 ┌──────────────┐   reads/writes go straight to backing files (FUSE passthrough)
 │ agentfs FUSE │──────────────────────────────────────────────┐
 │   (front)    │  intercepts namespace ops + change boundaries │
 └──────┬───────┘                                              ▼
        │ record (synchronous)                         backing tree (real files)
        ▼
 ┌──────────────┐   async: hash, objects, commits, push    state area
 │   recorder   │─────────────────────────────────────▶  (journal, pending,
 │ (history svc)│                                          git dir, LFS cache)
 └──────────────┘
```

### 2.1 Backing tree

The visible directory is a real directory tree on the backing volume.

- **Reads and writes** go straight to the backing files. On Linux 6.9 and later,
  the kernel's FUSE passthrough means data I/O does not go through the daemon at
  all. fuser exposes this through `BackingId`.
- **Normal behaviour.** Because the files are real:
  - hard links, mmap, `O_APPEND`, sparse files and SQLite behave normally;
  - unlinking or renaming an open file behaves normally;
  - permissions, ownership, timestamps and extended attributes are kept by the
    backing filesystem.
- **What the daemon handles.** It handles lookups, directory operations, renames,
  links and attribute changes, and it sees the change boundaries it must record.
- **Recording is per inode.** A change is attributed to the inode's current
  paths at the moment of recording, not to the path it was opened with.

### 2.2 State area

The state area is a separate directory on the same volume, never visible
through the mount:

```
<state>/
  git/       the history repository (bare): objects, refs, config, info/*
  journal/   durable record log (§3.3)
  pending/   frozen copies of recorded content awaiting commit (§3.2)
  lfs/       local LFS object cache (phase 2)
  meta/      the daemon's own bookkeeping
    push/<remote>.json   each remote's push state (§11.2)
    push/<remote>.lock   flock held while pushing that remote
```

### 2.3 Single writer of history

Exactly one recorder process owns the history repository and its branch. In
phase 1 that is the sidecar's daemon. With several pods (§14) it is a separate
history service per volume, fed by each pod's front. Either way history is a
straight line, and every commit is made by agentfs.

Not yet enforced: two agentfs processes on one state area (for example
`agentfs record` or `flush` alongside `agentfs run`) are not excluded; only
pushes are locked, per remote (§11.2). An exclusive state-area lock taken when
the volume is opened is a follow-up.

## 3. Recording

### 3.1 Change boundaries

| Event (FUSE op) | Recorded | Before return? |
|---|---|---|
| `close` (FLUSH) of a file changed since its last record | its content and executable bit | yes |
| `rename` | source removed, destination added | yes |
| `unlink`, `rmdir` | removal | yes |
| `link`, `symlink` | new path | yes |
| `setattr` changing size (`truncate`) or the executable bit | new content or mode | yes |
| `fsync` on a path marked `agentfs.commit=fsync` | content, coalesced (§8) | yes, at most once per interval |
| RELEASE (all handles and mappings gone) | content, if changed after the last FLUSH | **no**: backstop for mmap and late writes |
| periodic sweep | content of files still open and changed | **no**: backstop for files never closed |

Rules that make this precise:

- **FLUSH records only if the inode changed.** FLUSH is sent on every `close()`
  of every duplicated descriptor, for example each forked child that inherited
  stdout. Unchanged inodes are skipped, so a build writing to a log does not
  record the log thousands of times. A changed one is still recorded at every
  close.
- **RELEASE is asynchronous.** Its result never reaches the caller, which is why
  writes through shared memory mappings, or after the last close, are recorded
  late.
- **Empty directories** are not recorded, because git cannot represent them.
  `mkdir` alone is not a boundary.

### 3.2 The synchronous step

The work done before the call returns must not depend on file size:

1. **Freeze** the inode's current content into `pending/`, as a reflink where the
   backing filesystem supports it (XFS, btrfs), otherwise as a copy.
   A file already over the smallest remote blob-size limit (§11.3) by its
   size on disk is refused before it is copied; the frozen copy is checked
   again, since the file can change in between.
2. **Journal**: append a framed, checksummed entry with sequence number, inode,
   current paths, pending id, mode, trigger, caller UID, GID and PID, and time.
3. **Group commit**: one `fsync` of the journal covers every entry that arrived
   while the previous `fsync` was in progress.
4. Return.

Hashing, compression, object writes, LFS uploads and commits happen afterwards
(§3.4). Without reflink support, step 1 costs a copy of the file, so recorded
volumes should use XFS. The phase 1 benchmark (§18) measures this.

### 3.3 Journal and replay

- Entries hold **absolute state** (path → pending content and mode, or a
  removal), never deltas, so replaying an entry twice gives the same result.
- **Torn entries.** A torn final entry, which fails its checksum, is discarded.
  Its call never returned, so nothing was promised.
- **Commits record the last journal sequence number they include,** as a trailer
  (`Agentfs-Seq: <n>`).
- **Truncation.** The journal and `pending/` are truncated only after the commit
  and branch ref are durable.
- **libgit2 fsync** is enabled for objects and refs (`GIT_OPT_ENABLE_FSYNC_GITDIR`,
  set through libgit2-sys). agentfs also fsyncs new object directories itself,
  because libgit2 does not.
- **After a crash,** the recorder resumes from the branch head's `Agentfs-Seq`
  and replays later entries.

### 3.4 Commits

- A commit builder turns journal entries into commits a few milliseconds after
  they are recorded. The service's commit loop wakes every `commitWindow` and
  commits each group that can no longer grow: one whose first entry is more
  than `commitWindow` old, or is in the future after the clock stepped back.
  A failed commit raises `CommitFailed` (§13), and the loop backs off (doubling,
  up to 30 s) while the entries stay journaled. On shutdown everything left is
  committed, including the open group.
- **Grouping only affects how history reads,** not whether a change is kept.
  Entries close together in time are grouped. Inside a nested repository with
  `nested = working-tree` (§5), its `index.lock` bounds a group, with a timeout
  for stale locks.
- **Commit metadata** is in §9.
- **Every path is checked twice** against git's fsck rules and the configured
  limits of the remotes (§11.3):
  - **at record time**, in the synchronous step. This is the gate: a path that
    fails is never journaled or tracked, its frozen copy is removed at once,
    and the rest of the record carries on;
  - **again at commit time**, as a backstop for records journaled before the
    limits changed, for example records recovered after a remount with a
    stricter remote. A refused change is dropped from its commit.

  A refused path stays in the backing tree, its last recorded content (if
  any) stays in history, and an alarm is raised (`PathRefused`, §13).
- **The checks**, beyond the path rules of §5:

  | Rule | Threshold | git fsck id |
  |---|---|---|
  | Path length | over 4096 bytes | none: agentfs's own (`PATH_MAX` at checkout) |
  | Symlinked `.gitmodules`, `.gitattributes`, `.gitignore`, `.mailmap`, in any NTFS or HFS+ spelling, including 8.3 short names (`gitmod~1`, `gi7eba~1`) | | `gitmodulesSymlink`, `gitattributesSymlink`, `gitignoreSymlink`, `mailmapSymlink` |
  | Symlink target length | over 4096 bytes | `symlinkTargetLength` |
  | Symlink target with a `.git` component, in any spelling | | `symlinkPointsToGitDir` |
  | `.gitattributes` size | over 100 MiB | `gitattributesLarge` |
  | `.gitattributes` line length | 2048 bytes or more (git's test is `>=`) | `gitattributesLineLength` |
  | Blob size | over the smallest `remote.<name>.agentfsMaxBlobSize` | none: remote policy |

  There is one history for every remote, so the blob limit that applies is the
  smallest across remotes. Changing a remote's limits takes a remount. A path
  refused at commit time stays in the in-memory tracked set (§4) until the
  next open.
- **Not yet checked:** the content rules for `.gitmodules` (`gitmodulesParse`,
  `gitmodulesUrl`, `gitmodulesPath`, `gitmodulesName`, `gitmodulesLarge`). A
  remote with `receive.fsckObjects` may refuse a commit carrying a hostile
  `.gitmodules`, leaving that remote alarming until an operator rewrites
  history.
- `symlinkPointsToGitDir` is only a warning in default git. agentfs always
  refuses it, so a workload symlink into a nested repository's `.git` is not
  recorded.

### 3.5 When recording fails

If the synchronous step fails (for example, the state area is full):

- the call returns an error (`EIO` from FLUSH, or the operation's own error);
- the inode stays marked as changed, so the next boundary records it;
- agentfs raises an alarm.

Many programs ignore errors from `close`, so the alarm, not the error code, is
what people will see.

## 4. Path rules

agentfs follows git's model: **ignore rules decide only whether new
(untracked) paths are recorded. A tracked path stays recorded until it is
deleted, even if a rule later matches it.** Rules never decide where data
lives; all data lives in the backing tree.

As with git's index, a path is **tracked** if it is in the branch head or has
been recorded and is awaiting commit, so this holds during commit lag and
after crash recovery. A removal is recorded only if something is tracked at
or below the path; removing a path that was never recorded is not journaled.
A record holds its paths, and every path above or below them, from the
decision until the tracked set is updated, so a removal racing an in-flight
first write of the same path waits for it and is journaled after it.
Records for unrelated paths do not wait for each other.

### 4.1 Where rules come from

| Concern | Tracked (in the tree; edits are recorded) | Operator (in the git dir; iac sets them; invisible) |
|---|---|---|
| Ignore | `.gitignore` in any directory | `$GIT_DIR/info/exclude` |
| Attributes | `.gitattributes` in any directory | `$GIT_DIR/info/attributes` |
| LFS endpoint | `.lfsconfig` | `$GIT_DIR/config` |
| agentfs settings | none | `[agentfs]` in `$GIT_DIR/config` |

**Safety settings are operator-only.**

- `agentfs.*` attributes are read only from `info/attributes`.
- LFS endpoints and credentials are read only from operator config.
- Tracked `.gitignore`, `.gitattributes` and `.lfsconfig` files are recorded as
  content. They take part in rules only if `respectTreeRules = true`, and even
  then they cannot set `agentfs.*` attributes or LFS endpoints.

The default is `false`, so an agent's edit to a tracked rule file cannot turn
recording off or redirect uploads.

Built-in system- and user-level git attribute and exclude files are never
consulted. Only the files above count.

**How the sources combine.** These are deliberate departures from git, so
that operator rules always apply:

- Operator and tree ignore rules are evaluated as separate layers, and either
  one excludes a path. In git a `.gitignore` takes precedence over
  `info/exclude`, so a tree `!pattern` could re-include an operator exclude;
  in agentfs it cannot.
- Within each layer git's own rules hold: the last matching pattern wins, a
  deeper `.gitignore` wins over a shallower one, and nothing inside an
  excluded directory can be re-included.
- Tree attribute macros are kept apart from operator ones, so a tree
  `[attr]` line cannot change what an operator line means. `agentfs.*` values
  from tree files are dropped after macro expansion. Macros are honoured only
  in `info/attributes` and the root `.gitattributes`, as in git.
- Matching is always case-sensitive; `core.ignoreCase` is not consulted.
- Tree rule files are read only if they are regular files, never through a
  symlink (as git does since 2.32), and a file over 1 MiB is ignored (git
  ignores oversized attribute files), so the synchronous step stays bounded.
- `info/exclude` and `info/attributes` are read when the volume is opened.
  Changing them takes a remount, which fits iac setting them.

### 4.2 agentfs attributes (operator `info/attributes`)

| Attribute | Values | Default | Meaning |
|---|---|---|---|
| `agentfs.commit` | `close`, `fsync` | `close` | Boundary class (§3.1, §8) |
| `agentfs.coalesce` | duration | `30s` | Minimum interval between `fsync` records for one group |
| `agentfs.group` | name | unset | Paths with the same group *and the same stem* are recorded together (§8) |
| `agentfs.store` | `blob`, `lfs` | §7 | Force ordinary git object or LFS |
| `-agentfs` | | | Never record this path |

- `-agentfs` stops writes to a path being recorded even when the path is
  already tracked. A removal of a tracked path is still recorded, since it
  carries no content and keeps history consistent with the tree. History is
  never rewritten (§10).
- Attribute patterns follow git: a directory pattern such as `secrets/` does
  not cover the directory's contents. Write `secrets/** -agentfs`.

### 4.3 Settings

```ini
[agentfs]
    respectTreeRules = false   # tracked .gitignore/.gitattributes are content only
    nested = exclude           # §5
    commitWindow = 200ms       # group journal entries this close in time
    pushInterval = 60s         # §11
    lfsThreshold = 8m          # phase 2, §7

[remote "origin"]               # one section per push remote (§11)
    url = https://github.com/acme/agent-history.git
    pushurl = …                 # optional; wins over url, as in git
    agentfsMaxBlobSize = 50m    # optional; §3.4, §11.3
    agentfsCredentials = /run/secrets/agentfs/origin   # optional; §11.1
```

### 4.4 Implementation note

Rules are evaluated by agentfs over the pure-Rust `gix-ignore` and
`gix-attributes` parsers, fed only with buffers agentfs reads itself:
`info/exclude`, `info/attributes`, and the tree's `.gitignore` and
`.gitattributes`. Nothing global can leak in.

Draft 2 proposed pointing libgit2 at the backing tree first. Checking
libgit2 1.9.7's source ruled that out:

1. `git_ignore__for_path` always loads every working-tree `.gitignore` on the
   path, with no way to turn that off, so `respectTreeRules = false` could
   not be honoured.
2. It merges every source by git's precedence (a `.gitignore` beats
   `info/exclude`) and cannot report which file an attribute came from, so
   tree files could neither be kept from re-including operator excludes nor
   from setting `agentfs.*`.
3. Its attribute cache takes `core.excludesFile` and `core.attributesFile`
   from the merged system, global and XDG config, falling back to the XDG
   defaults. Disabling that needs process-wide search-path overrides.
4. `git2::Repository` is not `Sync`, so every check from the FUSE threads
   would be serialised behind a lock.

Nested repositories (§5) are detected by probing the backing tree directly,
not by opening them with libgit2, which would read global config.

## 5. Nested repositories

**Detection.** A directory is a nested repository if it contains:

- a `.git` directory;
- a `.git` file or symlink that points to a git directory inside the tree; or
- a git directory itself: the layout of a bare repository (`HEAD`, `objects/`
  and `refs/`), or any mark of one still being created (below).

The last case covers `git clone --bare`, `git init --bare` and tool-managed
stores such as Hermes' `checkpoints/store`.

**Default (`nested = exclude`).** A nested repository's directory is not
recorded: neither its working files nor its git directory.

**Option (`nested = working-tree`).** Working files are recorded, the git
directory is not, and the inner `index.lock` bounds commit groups (§3.4).

**Rules that prevent surprises:**

- **The mount root is never treated as nested.** `git init` at the root creates
  an ordinary directory named `.git`, which agentfs does not record.
- **Tracked paths stay tracked** (§4). If `git init` runs in a directory whose
  files are already recorded, those files keep being recorded. Only new paths
  under it are excluded. Nothing is deleted from history because of nesting.
- **Paths git refuses** are not recorded, and each raises an alarm
  (`PathRefused`, §13). This includes any `.git` component and its NTFS and HFS
  look-alikes, such as `.GIT`, `git~1` and `.git.`, and the checks of §3.4. A
  refused path is reported per path; it never fails the rest of its record.

**Detection details.**

- Only the proper ancestor directories of a path are examined, never the
  mount root and never the path itself.
- **Repositories under construction.** `git init --bare` writes
  `description`, `hooks/`, `info/exclude`, `refs/`, `HEAD` and `config`
  before `objects/` exists, and each file is a change boundary. Because
  tracked paths stay tracked, anything recorded then would be recorded
  forever, including later `config` edits that may carry credentials in a
  remote URL. So any one of these marks a git directory: a `HEAD` or
  `HEAD.lock` whose first line is a symbolic ref (`ref: refs/…`) or an object
  id; a `config` or `config.lock` whose `[core]` section sets
  `repositoryformatversion`; or git's default template `description`. Only a
  custom template's files written before `HEAD` can still be recorded.
- **`.git` files and symlinks.** A `.git` file counts only if its first line is
  `gitdir: <path>`; a `.git` symlink is read, never followed. Either target
  must be a git directory inside the backing tree: `HEAD` plus `objects/` and
  `refs/`, or `HEAD` plus `commondir` (a linked worktree). Targets are paths
  in the workload's view of the mount, so they are never resolved on the
  host:
  - a relative target is resolved lexically against the directory holding
    `.git`, and is ignored if it climbs out of the tree;
  - an absolute target (what `git worktree add` writes by default) carries
    the mount point, which agentfs does not know, so each of its suffixes is
    tried from the tree root: `/workspace/main/.git/worktrees/wt` is found as
    `main/.git/worktrees/wt` in a tree mounted at `/workspace`. Once the
    FUSE layer knows its mount point this can become an exact mapping.
- **Bounded reads.** These files are workload-controlled and read in the
  synchronous step, so they are opened without following symlinks and
  without blocking (a FIFO cannot hang the daemon), must be regular files,
  and are read only up to 4 KiB; a larger one does not count, as git refuses
  oversized gitfiles.
- Under `nested = working-tree`, a directory that is a git directory is
  still skipped entirely. This also covers a submodule's git directory kept
  inside the tree.
- With several nested repositories on a path, the outermost one is reported.

## 6. Metadata and permissions

- **Kept by the backing filesystem:** all metadata, including permissions,
  ownership, timestamps and extended attributes.
- **Recorded in history:** what git can represent: file content, the executable
  bit, and symlinks.
- **Not recorded:** ownership, other permission bits, timestamps and extended
  attributes. Recovery (§15) restores content and the executable bit only.
- **Access.**
  - The mount uses `allow_other` and the kernel's `default_permissions`. The
    kernel checks the real stored ownership and modes, so root and non-root
    callers behave as they would on the backing filesystem.
  - `chown`, `chmod` and `utimens` are passed through to the backing tree.
- **Hermes.** Its tool containers run as root while files are owned by UID
  10000. git's "dubious ownership" check then needs `safe.directory` in those
  containers. That is a consumer setting (§16), not an agentfs one.

## 7. Large files and LFS (phase 2)

Whether content is stored as an ordinary git object or an LFS pointer:

1. `agentfs.store`, if set;
2. otherwise a `filter=lfs` attribute;
3. otherwise `lfsThreshold`: larger content goes to LFS.

**History must be readable by ordinary git.**

- Whenever agentfs stores a path as LFS, it also maintains a matching tracked
  `.gitattributes` entry (`filter=lfs diff=lfs merge=lfs -text`), exactly as
  `git lfs track` does. Clones and remote web views then show content, not
  pointer text.
- `agentfs.store=blob` may not override a `filter=lfs` attribute.

**Endpoint and credentials.**

- LFS endpoints come only from operator config. git-lfs's keys are honoured:
  `lfs.url`, `lfs.pushurl`, `remote.<name>.lfsurl`.
- Credentials are bound to their configured host and never sent anywhere else.
- History and LFS content may live on different servers.

**Implementation.** Neither libgit2 nor gitoxide implements LFS. agentfs
implements:

- the pointer format;
- the LFS batch API, including `verify`;
- a local object cache.

LFS objects are uploaded before the commits that reference them are pushed.

**Retention.** LFS objects cannot be pruned from most LFS servers (GitHub's
needs the repository deleted), and pruned pointers break clones. agentfs does
not prune. It limits growth at the source instead, with coalescing intervals
and size caps per path.

## 8. Files changed in place: the `fsync` class (phase 2)

Databases and similar files are held open and changed in place. Recording them
only on `close` could mean never.

**Groups.**

- A group is the set of paths with the same `agentfs.group` value *and the same
  stem* in one directory, for example `state.db`, `state.db-wal` and
  `state.db-journal`.
- `-shm` files are never recorded.
- Two databases in one directory are two groups.

**Triggers.**

- An `fsync` on any member, at most once per `agentfs.coalesce` interval.
- **A trailing timer.** SQLite with `synchronous=NORMAL` does not `fsync` on
  commit, so a group with unrecorded changes is recorded when its interval
  elapses even without an `fsync`.
- At `close` and at unmount.

**Consistent capture.**

- While a group is frozen into `pending/`, writes to every member are held back.
  The daemon can do this because it receives every write for these paths: they
  are opened without passthrough and with writeback caching off.
- The whole group is frozen as one unit. A restored database plus its `-wal` or
  `-journal` then recovers normally.

**Storage.**

- Store databases as LFS (§7) or as ordinary objects, as the operator chooses.
- libgit2 writes each version as a full compressed loose object and its push
  makes no thin packs. So the recorder repacks periodically, for example by
  running `git repack`, and size caps limit the worst case.

**Guarantee.** For this class "before return" weakens to "at most one interval
late". This is deliberate.

## 9. Commit metadata

**Committer: the replica that made the change.** Every pod mounting a volume has
its own committer identity. Operators set it with templates; the defaults are
below.

```ini
[agentfs "identity"]
    committerName  = {replica}
    committerEmail = {podUid}@{volume}.agentfs.invalid
    authorName     = uid-{uid}
    authorEmail    = uid-{uid}@{volume}.agentfs.invalid
```

**Template variables:**

- `{volume}`, `{namespace}`, `{pod}`, `{podUid}`, `{node}`;
- `{replica}`: the StatefulSet pod name, which is stable across restarts;
  otherwise the pod name;
- `{uid}`, `{gid}` (author templates only).

**Rules.**

- Rendered identities must pass git's fsck rules (§3.4). A template that does
  not is a configuration error at mount.
- The default `.invalid` domain can never belong to a real account, so commits
  are never linked to a person on a hosted remote.

**Author: the writer process** as FUSE reports it. PIDs and UIDs are as seen
from the mount's own PID namespace. Identity beyond UID needs the workload's
cooperation (§1.3, later phases).

**Trailers: pod metadata.**

```
Agentfs-Seq: 18342
Agentfs-Trigger: close
Agentfs-Namespace: agent
Agentfs-Pod: hermes-7c9d8-x2k4q
Agentfs-Pod-Uid: 0b6f…
Agentfs-Node: hawking
Agentfs-Uid: 10000
Agentfs-Pid: 4711
```

- **Phase 1** gets pod metadata from the downward API.
- **The CSI driver** gets it from `podInfoOnMount`.
- **Grouped commits** carry one `Agentfs-Uid` and `Agentfs-Pid` line per
  distinct writer.

## 10. Secrets (not in scope)

agentfs does not scan content for secrets and does not hold pushes.

- **Keeping secrets out** is done with ordinary operator rules (§4). Each
  consumer excludes its own secret files (§16).
- **A secret that gets recorded anyway** is permanent in history, locally and on
  every remote it reaches. Removing it means rewriting history: a documented
  operator procedure (§15) for the state area and each remote, run with the
  workload stopped. It is never automatic.

## 11. Remotes and pushing

### 11.1 Remotes

- **Any git remotes,** over HTTPS or SSH, configured by the operator. GitHub,
  Gitea or a bare repository on a server all work the same way. Local paths
  and `file://` URLs work too, which is how pushing is tested.
- **Several push remotes** can be configured. Each has its own push state and
  alarms.
- **Configuration** is the standard `[remote "<name>"]` sections (`url`, and
  `pushurl`, which wins as in git) plus agentfs's own keys (§4.3), read only
  from the history repository's own config file. libgit2's system, XDG,
  global and ProgramData config search paths are cleared once per process,
  and pushes use anonymous remotes with explicit `<oid>:<ref>` refspecs, so
  `remote.<name>.push` and `fetch`, and any global `url.<base>.insteadOf` or
  `http.*` setting, are never consulted.
- **An invalid remote section never stops the volume.** A remote whose name
  is not a valid agentfs name (§11.4, e.g. `team/backup`, which git accepts),
  whose `agentfsCredentials` is not absolute, or whose `agentfsMaxBlobSize`
  is not a positive size is left out: it is never pushed and its limit does
  not apply. Each one raises `RemoteMisconfigured` (§13) when the volume
  opens; `agentfs status` lists it and `agentfs push` reports it and exits 1.
  Recording, committing and the other remotes carry on.
- **Credentials** come from files the recorder reads, and are re-read when they
  change, so short-lived tokens can be rotated by whatever writes the file.
  - `remote.<name>.agentfsCredentials` is an absolute path to a file holding
    either git-credential `key=value` lines (`username=`, `password=`; other
    keys are ignored) or a bare token, used as the password with the username
    from the URL, else `x-access-token`.
  - The file is re-read when its stat stamp (device, inode, size, mtime,
    ctime) changes. The stamp follows symlinks, so kubelet's atomic `..data`
    swap for Secret volumes and a rename over the file are both seen. An
    in-place rewrite of the same size within the filesystem's timestamp
    granularity would be missed.
  - A credential is bound to the host (and port) of the remote's push URL
    and is never given to any other host.
  - It gets one attempt per connection: a rejected credential is an
    authentication failure, not a retry loop.
  - Only username and password (token) credentials are supported. SSH keys
    need libgit2 built with libssh2, and HTTPS needs it built with TLS; the
    current build environment has neither, so only local and `file://`
    remotes are exercised end to end.

### 11.2 Interval push

- Pushes run every `pushInterval`, independent of the workload's lifecycle.
  Nothing waits for a pod to stop, and stopping a pod never waits for a push.
  The first push of each remote happens as soon as the service starts.
- Each remote is pushed from its own thread, so a slow or hung remote cannot
  delay another. libgit2 has no way to cancel a push in progress, so
  shutdown waits for pushes only up to a grace period, then leaves them.
- **Network timeouts.** libgit2 is set process-wide to give up on a
  connection after 30 s and on a stalled read or write after 5 min. A remote
  that accepts the connection and then stops responding ends as an
  `unreachable` failure with the usual alarm and backoff, rather than
  hanging its thread and silencing its alarms; `agentfs push`, which pushes
  remotes in turn, moves on to the next remote.
- Unpushed commits stay safe in the state area and go out on the next
  successful push.
- Pushes are fast-forward only.
- **Push state** is kept per remote in `meta/push/<remote>.json` (§2.2): the
  last pushed commit, the last attempt and success times, consecutive
  failures, the last error and any divergence. It is bookkeeping: a missing
  or unreadable file is treated as empty.
- **A per-remote lock** (`flock` on `meta/push/<remote>.lock`) keeps a manual
  `agentfs push` and the running service from pushing the same remote at
  once; whichever loses reports itself busy and does nothing.
- **Backoff.** After a failure the next attempt waits `pushInterval` doubled
  for each consecutive failure after the first, up to
  `max(pushInterval, 15m)`. A success resets it.

### 11.3 Limits and rejected pushes

- **Limits are configured per remote.** For now the only one is
  `remote.<name>.agentfsMaxBlobSize`. Commits are checked against it before
  they are made (§3.4); because one history goes to every remote, the
  smallest limit applies. git's fsck rules are always enforced, not per
  remote: history may later go to a stricter remote, and a commit no remote
  accepts can never be pushed. A maximum push size is deferred.
- **A rejected push never affects the filesystem.** Rejected means refused,
  non-fast-forward, authentication failure or an unreachable remote. Recording,
  committing and other remotes carry on. The recorder:
  - raises an alarm (`PushFailed`, §13) with the reason and the commit;
  - keeps retrying with backoff (§11.2);
  - keeps the remote's last successful commit, so the backlog is visible.
- **Non-fast-forward on the branch means someone else wrote it.**
  - The recorder pushes its head to
    `refs/agentfs/<volume>/diverged/<UTC timestamp, no colons>`
    (`YYYYMMDDTHHMMSSZ`) once per divergence, raising `Diverged`, then keeps
    raising `PushFailed` (`non_fast_forward`) at every attempt, with the same
    backoff as other failures.
  - The name is chosen and saved in the remote's push state as pending
    before that push, and confirmed after it. A crash in between reuses the
    pending name at the next attempt (the same head, or a fast-forward of
    it), so a divergence is saved under one name; only `Diverged` may be
    raised again after such a crash.
  - It never force-pushes.
  - An operator resolves it (§15). The divergence clears when a branch push
    next succeeds.
  - A non-fast-forward on one of the volume's other refs is reported the
    same way but saves no diverged ref. libgit2 rejects the whole push when
    any ref is not a fast-forward, so the patch-queue refs of phase 3 will
    need their own push policy.
- libgit2's local transport updates an existing remote ref without
  compare-and-swap, so another writer moving a local or `file://` remote
  between libgit2's ref listing and its update can be overwritten. Smart
  HTTP and SSH servers compare and swap.

### 11.4 Shared repositories

Several volumes may share one repository. Each volume has a name (`{volume}`,
set by the operator) and owns a branch and a ref namespace in that repository:

| Ref | Purpose |
|---|---|
| `refs/heads/agentfs/<volume>` | The volume's history (configurable: `[agentfs] branch`) |
| `refs/agentfs/<volume>/inbox/<n>` | Patch queue (§12) |
| `refs/agentfs/<volume>/applied/<n>` | Applied patches |
| `refs/agentfs/<volume>/status` | Recorder status for iac (§12) |
| `refs/agentfs/<volume>/diverged/<timestamp>` | Divergence (§11.3) |

**Pushing.** A recorder pushes only its own volume's refs (the branch and
`refs/agentfs/<volume>/*`), with explicit refspecs, so pushes from different
volumes never contend.

**Fetching.** At mount it fetches only its own volume's refs.

**Single writer.** Each recorder is the only intended writer of its volume's
refs. Remotes cannot always enforce that:

- GitHub can restrict pushers only on organisation-owned repositories.
- Tokens are scoped to repositories, not branches.
- Every volume sharing a repository can therefore write every other volume's
  refs.

agentfs **detects** other writers (§11.3) rather than relying on enforcement.

**Volume names** must be unique within a repository. Two recorders configured
with the same name would see each other as divergence.

**Names.** Volume and remote names must match `[A-Za-z0-9][A-Za-z0-9._-]*`,
without `..` and not ending in `.` or `.lock`. They become ref components and
state file names; a `/` in a volume name would let one volume's
`refs/agentfs/<volume>/` namespace contain another's.

## 12. Patch queue for iac (phase 3)

- **What a patch is:** a single commit pushed to the remote at
  `refs/agentfs/<volume>/inbox/<n>`, where `<n>` is a zero-padded sequence
  number. Its parent is the branch commit it was written against.
- **When it runs:** at mount, after journal replay and before the workload starts.
- **What it does:** each patch is applied in sequence order as a three-way merge
  against its parent, onto the current branch head.
  - **Success:** a commit authored by the patch's author, committed by agentfs,
    with `Agentfs-Inbox: <n>`.
  - **The first failure stops processing.** That patch and all later ones stay
    queued, in order.
  - The volume mounts regardless, and an alarm is raised.
- **Idempotency:** before applying patch `<n>`, the recorder checks the branch
  for a commit carrying `Agentfs-Inbox: <n>`. Moving `inbox/<n>` to
  `applied/<n>` is then only bookkeeping, and redoing it after a crash is
  harmless.
- **Visibility to iac:** the recorder writes the volume's status ref (§11.4) and
  emits telemetry (§13) with the queue head and the last failure.
- **Open:** how iac writes patches (§19).

## 13. Observability (OpenTelemetry)

The recorder exports metrics, traces and logs over OTLP.
`agentfs_core::telemetry` instruments the engine against the OpenTelemetry
API only; the binary picks the SDK and the exporter.

**Metrics:**

| Metric | Type | Use |
|---|---|---|
| `agentfs.record.duration` | histogram | Cost of the synchronous step, by trigger |
| `agentfs.record.failures` | counter | Recording errors (§3.5), by trigger |
| `agentfs.journal.backlog` | gauge | Entries recorded but not committed |
| `agentfs.commit.count` | counter | Commits made |
| `agentfs.commit.failures` | counter | Commit passes that failed; the records stay journaled |
| `agentfs.push.unpushed_commits` | gauge, per remote | Backlog |
| `agentfs.push.oldest_unpushed_age` | gauge, per remote | Backlog age, the main push alarm |
| `agentfs.push.failures` | counter, per remote and reason | Rejected, non-fast-forward, authentication, network |
| `agentfs.push.diverged` | gauge, per remote | 1 while diverged |
| `agentfs.remote.misconfigured` | gauge, per remote | 1 for each invalid `[remote]` section (§11.1) |
| `agentfs.precheck.rejected_paths` | counter | Paths refused by fsck or limit checks, by stage and reason |
| `agentfs.patch.queue_depth`, `agentfs.patch.blocked` | gauge | Phase 3 |
| `agentfs.state.bytes` | gauge | Size of the state area by part |

Metric attributes are short and low-cardinality: `trigger`, `remote`,
`reason` (a refusal label or a push failure kind), `stage` (`record` or
`commit`) and `part` (`git`, `journal`, `pending`, `lfs`, `meta`). Paths never
become metric attributes. Span and log attributes are namespaced `agentfs.*`.

`agentfs.state.bytes` is the apparent size (the sum of file lengths) of each
part, not allocated blocks: reflinks make both misleading, and apparent size
is deterministic. The journal backlog and the push gauges are read when
metrics are collected: the backlog from the open journal; unpushed commits,
oldest unpushed age (seconds since the oldest unpushed commit's committer
time, 0 when none) and divergence from each remote's push state plus a walk
of the branch, shared by the three push gauges in one collection.

**Traces.** Spans `agentfs.record` (trigger, inputs, caller uid and pid, the
sequence number when journaled, paths skipped), `agentfs.commit` (first and
last sequence numbers, records, paths refused, the commit id or
`agentfs.commit.empty`) and `agentfs.push` (remote, previous and new head,
outcome, and on failure the reason and any diverged ref). Each span is
current while its operation runs, so alarm logs raised inside it carry its
trace. Record and commit spans are not linked; they correlate through the
sequence numbers. Patch-application spans come with phase 3.

**Logs.** A structured log record for every alarm condition: the event name
(`agentfs.path_refused`, `agentfs.push_failed`, ...), WARN for a refused
path and ERROR otherwise, the event's message as the body and its fields
(path, reason, remote, kind, heads, ...) as attributes.

**Events.** The engine reports alarm conditions as events to a sink
(`agentfs_core::events::EventSink`). The telemetry layer wraps the
process's sink once (`Telemetry::sink`), turning events into the counters
and logs above; the CLI also prints alarms to stderr.

| Event | Raised when |
|---|---|
| `PathRefused` | A path fails the checks of §3.4 or §5, at record or commit time |
| `RecordFailed` | The synchronous step fails (§3.5) |
| `CommitFailed` | Turning records into a commit fails; the records stay journaled and are retried |
| `PushFailed` | A push attempt fails, with the remote, the kind (`unreachable`, `auth`, `non_fast_forward`, `refused`, `local`), the head, the last pushed commit, the consecutive failure count and the next retry |
| `Diverged` | The local head was saved under a diverged ref (§11.3) |
| `PushSucceeded` | A push attempt succeeded, including one with nothing new (not an alarm; sinks filter) |

A sink is called from the synchronous step, on the FUSE threads, so it must
never block. Backlog gauges (`unpushed_commits`, `oldest_unpushed_age`) come
from each remote's push state, not from events.

**Export.** The binary reads the standard `OTEL_*` variables. A signal is
exported only when an OTLP endpoint is set for it
(`OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT`);
with none, with `OTEL_SDK_DISABLED=true`, or with
`OTEL_<SIGNAL>_EXPORTER=none`, telemetry is a no-op and nothing is built.
This build exports OTLP http/protobuf over plain http only, which is what an
in-cluster collector needs; gRPC, JSON and TLS (`https`) endpoints are
deferred. They are reported with a warning and that signal stays off: a
telemetry misconfiguration never stops a command, in keeping with telemetry
never affecting the filesystem. Telemetry is flushed at exit, waiting at
most 5 seconds.

**Alarms** are alert rules on these metrics in the consumer's monitoring stack.
agentfs emits the signals; it does not page anyone itself. The CSI driver may
also emit Kubernetes Events on the pod.

## 14. Volumes mounted by several pods (CSI, phase 4)

A volume can be mounted by several pods at once, on one node or several.

**Shape.**

- **Backing tree:** on storage every mounting pod can reach. On one node that can
  be local; across nodes it needs a shared filesystem such as NFS or SeaweedFS.
- **A front per pod:** a FUSE mount that does the synchronous step of §3.2 for
  that pod's changes.
  - It freezes content into the volume's shared `pending/`.
  - It hands a journal entry to the volume's history service, and returns only
    once the service has made the entry durable.
- **One history service per volume:** the single writer of §2.3. It orders
  entries from all fronts, commits and pushes. Each front's entries carry that
  pod's committer identity and pod metadata (§9).

**Concurrency semantics.** Pods share one directory with the backing
filesystem's semantics. Two pods writing the same file is last-writer-wins, as
it would be without agentfs. History records each pod's change boundary as
its own commit, in the order the history service received them.

**CSI requirements carried over from review:**

- **Persistent identity.** A volume is a PersistentVolume or PVC, not an inline
  ephemeral volume. Its state and history service outlive any pod, and garbage
  collection follows the volume's lifecycle.
- **No network work on mount or unmount.** Mounting a volume never clones, fetches
  or pushes. The history service does that work in the background (§11.2),
  except for a first-ever clone. Patch processing (§12) happens in the history
  service before it accepts the first front, with a time limit.
- **Operator-owned parameters.** Remote, rules and credentials are set by the
  StorageClass or PV, not by pod authors.
- **Recovery** from a front or service crash is specified and tested. This
  covers:
  - how a crashed front's mount is replaced or its pod restarted;
  - how the service resumes from its journal.

**Open for this phase:**

- whether the cost of a network round trip on every change boundary is
  acceptable;
- whether FUSE passthrough works over the chosen shared filesystem;
- the RBAC and Pod Security inventory.

## 15. Recovery and undo (to be discovered)

Recovery may be manual, but it must be supported and documented. Phase 1
delivers a supported path; better tooling is discovered from real use.

**Phase 1 supported path:**

1. **Find the change.** History is ordinary git on the remote, or in the state
   area. Use any git tool: `git log --follow <path>`, `git show`, the remote's web
   view. Trailers (§9) say which pod, process and trigger made each change.
2. **Pause the workload.** Stop it, or scale it to zero.
3. **Restore.** Copy file content from the wanted commit into the backing tree.
   - A small command does this: `agentfs restore <path> <commit>`, or `agentfs
     revert <commit>` for a whole commit.
   - With the workload stopped, plain `git show <commit>:<path> > <file>`
     against the state area's repository also works.
   - The restore is itself recorded, with `Agentfs-Trigger: restore`.
4. **Resume the workload.**

**To discover:**

- whether restoring with the workload live is safe enough to support;
- whether a read-only history view for operators (not the workload) is worth
  building;
- whether undo through the patch queue (§12) replaces manual restores for iac.

**Operator history rewrite** removes content that must not stay in history,
such as a recorded secret (§10). It is a documented procedure:

- run against the state area's repository and every remote the content reached;
- run with the workload stopped;
- never automatic.

On remotes it requires a force-push of the volume's branch, the one case where
agentfs's no-force-push rule is set aside, by an operator.

## 16. First consumer: Hermes

Hermes is the first consumer, used to validate the design, not a design
target. What it needs:

- **What is mounted.** Only agent-authored definitions are recorded, for example
  `skills/` and each profile's `SOUL.md`, `config.yaml`, `profile.yaml` and
  `skills/`. The operator rules exclude Hermes' secrets (`.env`, `auth.json`),
  databases, sessions, logs, caches and `checkpoints/`.
- **Two kinds of writer.**
  - The Hermes process writes to the mount directly.
  - Hermes' file and terminal tools run in containers on a docker-in-docker
    sidecar, so the mount must also reach that sidecar, and tool containers need
    it as a volume.
  - In the iac workload that is a deliberate policy change to the `docker_volumes`
    pin, which the owner must approve.
- **Root init.** The Hermes image's start-up runs as root. With a real backing
  tree and `default_permissions`, root reads the mount like any other directory.
- **Restarts.** iac patches (§12) apply only at mount, which for Hermes means a
  pod restart.

## 17. Layers and libraries

### 17.1 Layer 1: core and daemon (Rust)

- **`agentfs-core`, a library:**
  - recording (§3);
  - rules (§4);
  - the commit builder;
  - pre-commit checks (§3.4);
  - push (§11);
  - the background service: the commit loop and one push loop per remote,
    with clean shutdown (§11.2);
  - alarm events, through a non-blocking sink (§13);
  - LFS (§7);
  - the patch queue (§12);
  - telemetry (§13): implemented against the OpenTelemetry API.
- **`agentfs`, a binary:**
  - `init`, `mount`, `status`, `flush`, `push`, `run`, `restore`, `revert`;
  - `push` pushes once to every remote (or those named with `--remote`) and
    exits 1 unless all succeeded; `run` runs the service in the foreground
    until SIGINT or SIGTERM, makes a final commit, waits up to
    `--push-grace` (default 5s) for pushes in progress, and exits at once on
    a second signal. `flush`, `record`, `push` and `run` export telemetry
    (§13) and take the pod flags (`AGENTFS_POD`, ...), so every command's
    telemetry resource is the same. `record` stands in for the FUSE layer until it exists;
  - `status` shows each remote: its URL (userinfo redacted), last pushed
    commit, unpushed commits, consecutive failures, last error and any
    diverged ref;
  - mounted with fuser (multi-threaded, with passthrough).

  It works anywhere FUSE does, with no Kubernetes dependency.
- **Phase 1 deployment:** a privileged native sidecar (an init container with
  `restartPolicy: Always`).
  - It mounts the FUSE filesystem into an `emptyDir` with Bidirectional
    propagation.
  - Workload containers mount it with HostToContainer propagation.
  - The backing tree and state area live on a `ReadWriteOncePod` PVC owned by
    the workload.
  - If the sidecar crashes, the mount fails (`ENOTCONN`) until the pod restarts.
    Phase 1 accepts restarting the pod as the recovery.

### 17.2 Layer 2: CSI driver (Rust)

Phase 4 (§14).

### 17.3 Libraries (verified 2026-10-12)

| Need | Choice | Notes |
|---|---|---|
| FUSE | `fuser` | Active. Multi-threaded dispatch, passthrough (`BackingId`) |
| Git objects, commits, push, credentials | `git2` (libgit2) | Active. Has **no** filter or merge-driver registration, and its fsync option is only reachable through `libgit2-sys` |
| Ignore and attribute rules | `gix-ignore`, `gix-attributes` | Pure Rust; chosen over libgit2's rules for the reasons in §4.4. gitoxide has no push, so it does not replace `git2` |
| LFS | none exists | Implemented in agentfs (§7) |
| Telemetry | `opentelemetry` 0.33 (API) in the core; `opentelemetry_sdk` and `opentelemetry-otlp` (`http-proto`, `reqwest-blocking-client`, no TLS) in the binary | OTLP http/protobuf export. Verified pure Rust: no OpenSSL, ring, aws-lc, cmake or protoc |
| Repack | the `git` CLI | libgit2 has no repack or gc |

## 18. Acceptance tests

1. **No gaps.** Each of these produces the expected recorded state:
   - an editor's temp-file-and-rename save;
   - an append;
   - `truncate`;
   - `mv` of a directory;
   - `rm -r`;
   - a hard link;
   - an executable-bit change;
   - a symlink;
   - unlink of an open file;
   - rename over an open file;
   - log rotation while the writer is open.
2. **Before return.** Killing the daemon right after `close` returns, then
   remounting, shows the change in history.
3. **Crash safety.** Killing the daemon or node at every step of record → commit
   → ref update loses nothing and duplicates nothing.
4. **Any user.** Root and UID 10000 in one pod read and write with normal
   permission semantics. `chmod 600` persists.
5. **Invisible.** No history, journal, state or credential is reachable through
   the mount.
6. **Rules.**
   - Ignores apply only to new paths.
   - A tracked file stays recorded after it is ignored.
   - With the default settings, tracked rule files cannot turn recording off.
7. **Nested repositories.**
   - A clone, a bare clone and `git init` at the root behave as §5 says.
   - Nothing is deleted from history by nesting.
8. **Remotes.**
   - Interval pushes reach two remotes.
   - An unreachable remote, a refused push and a non-fast-forward each raise the
     alarm while the filesystem keeps working.
9. **Pre-commit checks.** A path git's fsck or a remote limit would refuse is not
   recorded, and raises an alarm.
10. **Shared repository.**
    - Two volumes push to one repository on their own branches and ref
      namespaces without contending.
    - A volume's recorder writing into another volume's refs is detected as
      divergence.
    - Committer identities follow the configured templates.
11. **Recovery.** Following §15, a file and a whole commit are restored, and the
    restore is recorded.
12. **Telemetry.** Every metric in §13 is exported and moves as expected in the
    tests above.
13. **Performance.** Recorded latency and throughput for:
    - a mid-size `git clone` with `nested = working-tree`;
    - `npm install` into an excluded path;
    - `rm -rf` of 100k files;
    - closing a 1 GB file, on XFS (reflink) and ext4 (copy).
14. **Phase 2.** SQLite in WAL mode with `synchronous=NORMAL`, and in DELETE mode:
    every recorded version restores and passes `PRAGMA integrity_check`. LFS
    content is readable through an ordinary `git clone` plus `git lfs pull`.
15. **Phase 3.** Three queued patches where the second conflicts:
    - the first applies;
    - the second and third stay queued;
    - the volume mounts;
    - a crash and replay does not apply the first twice.
16. **Phase 4.** Two pods on two nodes mount one volume. Each pod's changes carry
    its own committer identity, and history keeps every boundary from both.

## 19. Decisions and open questions

### 19.1 Decisions (2026-10-12)

| Topic | Decision |
|---|---|
| Storage model | Real backing tree with FUSE passthrough; the projection is logical only |
| Several pods on one volume | Supported by the CSI driver, one committer identity per replica (§14, §9) |
| Rejected pushes | Alarm, retry, keep operating (§11.3) |
| Remotes | Any git remote, several at once (§11.1) |
| Push timing | Interval, independent of pod lifecycle (§11.2) |
| Recovery | Manual but supported; tooling discovered from use (§15) |
| Telemetry | OpenTelemetry (§13) |
| Secret scanning | Not in scope (§10) |
| Repository layout | Several volumes may share a repository, each with its own branch and ref namespace (§11.4) |
| Identity | Configurable templates with defaults (§9) |
| Backing filesystem | Both supported. XFS (reflink) makes the synchronous step size-independent; ext4 falls back to a copy. The phase 1 benchmark sets the documented recommendation (§3.2) |

### 19.2 Open questions

1. **How iac produces patches** for the queue (§12).
2. **Performance budget** for the synchronous step, set after the phase 1
   benchmark (§18 test 13).
3. **Multi-pod (§14):** the network round trip per boundary, passthrough over a
   shared filesystem, RBAC and Pod Security.
4. **Recovery tooling** beyond the phase 1 path (§15).

## 20. Prior art

| Project | What it is | Why not use it |
|---|---|---|
| gitfs (Presslabs; fork `vtemian/gitfs`) | Python FUSE filesystem over git that commits after idle | Commits after a 5 s idle period; merges while running; history visible in the mount; no LFS. Last commits 2021 and 2025-08-11. Design reference |
| Cloudflare ArtifactFS | FUSE client for Cloudflare Artifacts | Commits only on `git commit`; single-user mount (`allow_root` impossible through jacobsa/fuse); built for large lazily loaded repositories. PR 197 in iac stalled on it |
| JuiceFS CSI driver | FUSE filesystem with a CSI driver and mount pods | Reference for §14's recovery problems |
| Hermes `checkpoint_manager` | Shadow git store outside the project, snapshot before each turn | Evidence that a git directory outside the tree works for undo; snapshots per turn, not per change |
| Mesa | Hosted versioned filesystem for agents | Hosted private beta |
| Coregit, Freestyle | Hosted git APIs for agents | APIs, not mounts |
| lakeFS | Branches and commits over object storage | Explicit commits, not git |
