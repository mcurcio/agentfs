# agentfs design

Status: draft 2, 2026-10-12, updated as phase 1 is built (the FUSE layer,
`agentfs mount`, `agentfs restore` and `agentfs revert`, the container
image and the Kubernetes sidecar deployment exist), and Git LFS from
phase 2 (§7). Draft 1 was
reviewed adversarially. See [reviews/2026-10-12-adversarial.md](reviews/2026-10-12-adversarial.md)
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
  lfs/       local LFS object cache (§7)
    objects/aa/bb/<oid>  cached objects, git-lfs's layout; never pruned
    tmp/                 objects being written
  meta/      the daemon's own bookkeeping
    lock                 flock held by the one process using the state area (§2.3)
    push/<remote>.json   each remote's push state (§11.2)
    push/<remote>.lock   flock held while pushing that remote
    repairs              paths a mount could not record, for the next one (§3.5)
    control/agentfs.sock the running mount's control socket for restores (§15);
                         directory 0700, socket 0600
```

### 2.3 Single writer of history

Exactly one recorder process owns the history repository and its branch. In
phase 1 that is the sidecar's daemon. With several pods (§14) it is a separate
history service per volume, fed by each pod's front. Either way history is a
straight line, and every commit is made by agentfs.

**Enforced by a state-area lock.** Every command that writes the state area
(`mount`, `run`, `record`, `flush`, `push`, and `restore` and `revert` when
no mount is running) first takes an exclusive
`flock` on `meta/lock`, before it opens the volume, and holds it until it
exits. The kernel releases it when the process dies, even after `kill -9`,
so a crash never leaves a stale lock.

- The file holds a line describing the holder (`pid=… command=…
  since=<UTC>`), used only in messages; a line left by a dead holder is
  never taken for a held lock.
- A command that finds the lock held exits with status 75 (`EX_TEMPFAIL`)
  and names the holder, unless `--wait-lock <duration>`
  (`AGENTFS_WAIT_LOCK`) lets it wait.
- So `agentfs push` while a volume is mounted is refused: the mount's own
  push loops push. A way to ask a running daemon to push now is a
  follow-up.
- `restore` and `revert` (§15) first try the running mount's control socket
  (`meta/control/agentfs.sock`, §2.2): a mount holds the lock and serves
  them itself, writing through its own mount. With nothing listening they
  take the lock and write the backing tree directly. While `run`,
  `record`, `flush` or `push` holds the lock they exit 75: `run` serves
  no restores.
- `agentfs init` takes the lock when the state area already exists. It is
  safe to re-run, as a pod's init container does at every start: for the
  same volume (and branch, if given) it exits 0 and only re-creates
  missing directories and, with `--include`, sets the config includes;
  for another volume or branch it exits 1 and changes nothing, so a PVC
  mix-up never repurposes a history. (It used to overwrite
  `agentfs.volume`.)
- `agentfs status` takes no lock and changes nothing: it reads history and
  the journal without recovery (it used to run recovery, which could
  truncate the journal or prune frozen copies under a running daemon). It
  reports the lock's holder. A torn journal tail is reported, and trimmed by
  the next command that takes the lock.
- `status` probes the lock by taking a shared lock and dropping it at once;
  taking the lock therefore retries for at least 250 ms even with no
  `--wait-lock`, so a probe cannot make it fail.
- The per-remote push locks (§11.2) stay; they now only matter within one
  process.

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

**How the FUSE layer implements this** (`agentfs-fuse`):

- **Dirty detection.** With passthrough the daemon never sees writes, so
  "changed" is read from the inode. An inode is *armed* by a write-open, a
  write the daemon serves, or a size change; FLUSH `fstat`s only armed
  inodes and records when the stamp (device, inode, size, mtime, ctime,
  mode) differs from the one taken just before the last freeze. Timestamps
  come from a coarse clock, so git's racy-timestamp rule applies: if the
  last record's stamp is within twice the coarse clock's resolution of the
  record time, the next FLUSH records regardless. A created file is
  recorded at its first FLUSH even when empty (`touch`); a write-open of an
  existing file takes its stamp at open, so opening and closing without a
  change records nothing.
- **`O_TRUNC`.** `FUSE_ATOMIC_O_TRUNC` is negotiated, so a shell `>` is one
  open with `O_TRUNC` and one record at close, even when nothing is written
  after the truncation. `truncate`/`ftruncate` remain setattr boundaries.
- **What is frozen.** FLUSH, RELEASE and the sweep hand the recorder
  `/proc/self/fd/<n>` of the handle's own descriptor, so the frozen content
  is exactly that inode's, whatever renames happened. Other boundaries open
  the path with `openat2` first and do the same.
- **Current paths.** An inode's paths are the names the mount has seen for
  it (looked up, created or linked), resolved through the parent chain at
  record time. A hard link made before the mount and never looked up is not
  known, so a change through another name is not recorded at it.
- **Renames.** A file rename records the source's removal and the
  destination's content in one record. A directory rename records the
  removal of both names and then every regular file and symlink under the
  destination, never inside a `.git` directory; past 4096 files it is
  several consecutive records, the first carrying the removals, holding one
  descriptor per file while each is made (the daemon raises its open-file
  limit at start). `RENAME_EXCHANGE` removes both names and records both
  trees. Renaming one hard link over another of the same inode changes
  nothing and records nothing. The cost is O(files) in the synchronous
  step; a tree-move change type is a follow-up.
- **`mknod` of a regular file** has no FLUSH to follow, so it is recorded at
  once (trigger `close`). FIFOs, sockets and devices are never recorded.
- **RELEASE** of the last handle that could write records the inode if it
  changed after its last record (trigger `release`).
- **The sweep** runs every `agentfs.sweepInterval` (default 30 s, at least
  1 s): it records inodes open for writing whose stamp changed (trigger
  `sweep`) and retries the repair queue (§3.5). At unmount a final sweep
  records every armed inode with any handle still open.
- **The `fsync` class** (§8) is phase 2: paths with `agentfs.commit=fsync`
  are recorded at close like any other, and `fsync` is passed through.

### 3.2 The synchronous step

The work done before the call returns must not depend on file size:

1. **Freeze** the inode's current content into `pending/`, as a reflink where the
   backing filesystem supports it (XFS, btrfs), otherwise as a copy.
   A file already over the smallest remote blob-size limit (§11.3) by its
   size on disk is refused before it is copied, unless it would be stored
   as LFS (§7); the frozen copy is checked again, and its store decided
   and journaled, since the file can change in between.
2. **Journal**: append a framed, checksummed entry with sequence number, inode,
   current paths, pending id, mode, trigger, caller UID, GID and PID, and time.
3. **Group commit**: one `fsync` of the journal covers every entry that arrived
   while the previous `fsync` was in progress.
4. Return.

Hashing, compression, object writes and commits happen afterwards (§3.4):
LFS content is hashed into the object cache when it is committed, and
uploaded when it is pushed (§7). Without reflink support, step 1 costs a
copy of the file, so recorded volumes should use XFS. The phase 1 benchmark (§18) measures this.

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
  | `.gitmodules` (any spelling, any directory) that git's config parser cannot read | | `gitmodulesParse` |
  | `.gitmodules` submodule name empty or with a `..` component (split on `/` and `\`) | | `gitmodulesName` |
  | `.gitmodules` `url` that is an option (`-…`), holds a decoded newline, climbs out of its root with `../` followed by `:` or `/`, or is an `http(s)`/`ftp(s)` URL git refuses | | `gitmodulesUrl` |
  | `.gitmodules` `path` starting with `-` | | `gitmodulesPath` |
  | `.gitmodules` `update` starting with `!` (a command) | | `gitmodulesUpdate` |
  | `.gitmodules` size | over 512 MiB (git's `core.bigFileThreshold`) | `gitmodulesLarge` |
  | Blob size | over the smallest `remote.<name>.agentfsMaxBlobSize`, unless stored as LFS (§7) | none: remote policy |

  There is one history for every remote, so the blob limit that applies is the
  smallest across remotes. Changing a remote's limits takes a remount. A path
  refused at commit time stays in the in-memory tracked set (§4) until the
  next open.
- **The synchronous step stays bounded.** Size checks are a `stat` and
  always run at record time. Content scans (the `.gitattributes` line
  length, the `.gitmodules` rules) run at record time only for files up to
  1 MiB, the bound tree rule files already have (§4.1). A larger file is
  scanned in full at commit time, where a refusal drops the change and
  raises `PathRefused` with stage `commit`, like any commit-time refusal.
- **`.gitmodules` checks** are a port of git's config parser and of the
  submodule checks in git's fsck (`agentfs_core::gitmodules`), tested
  against the `git` binary: the parser against `git config --list`, the
  checks against `git fsck`. git changed its check of `http(s)` and
  `ftp(s)` submodule URLs in 2.40 (from `credential_from_url` to
  `url_normalize`); remotes run either, so agentfs refuses what either
  version refuses. Entries before a parse error are still checked, as in
  git.
- `symlinkPointsToGitDir` and `gitmodulesParse` are only a warning or
  info in default git. agentfs always refuses them: a workload symlink
  into a nested repository's `.git` is not recorded, and history stays
  acceptable to remotes with stricter fsck settings.

### 3.5 When recording fails

If the synchronous step fails (for example, the state area is full):

- the call returns an error (`EIO` from FLUSH, or the operation's own error);
- the inode stays marked as changed, so the next boundary records it;
- agentfs raises an alarm.

Many programs ignore errors from `close`, so the alarm, not the error code, is
what people will see.

**Namespace operations.** A rename, unlink, rmdir, link, symlink or setattr
has already changed the backing tree when its record fails, so "the inode
stays marked" is not enough. It returns `EIO` and puts the affected paths
on a **repair queue**. The sweep (every `sweepInterval`, and at unmount)
re-derives each queued path's absolute state from the backing tree (a
file's content, a symlink, a directory's files, or a removal) and records
it with trigger `sweep`. Because records hold absolute state, a repair is
idempotent and cannot undo a later change. Until it succeeds, history
differs from the tree at those paths; the `RecordFailed` alarm says so. A
RELEASE whose record fails queues its paths too, since no handle is left to
record from.

The queue outlives the daemon. It is kept in `meta/repairs` in the state
area (one line per path: the caller's uid, gid and pid and the path in hex),
rewritten whenever it changes, and the next `agentfs mount` repairs what it
lists from the backing tree before it serves any request. A repair made
from an older entry never drops one queued again meanwhile. Paths still
queued at unmount are listed on exit with where they are saved; if the file
cannot be written either (the state area is full or unwritable), the exit
message lists them and says that history differs from the tree there until
each path is changed again.

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
- LFS endpoints and credentials are read only from operator config (§7).
  A tracked `.lfsconfig` is content only: agentfs never reads it, though
  it may steer the git-lfs of whoever clones the history.
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
| `agentfs.store` | `blob`, `lfs` | §7 | Force ordinary git object or LFS; `blob` does not override `filter=lfs` |
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
    sweepInterval = 30s        # §3.1: backstop for files left open; min 1s
    lfsThreshold = 8m          # §7; unset by default (no size-based LFS)

[lfs]                           # optional, as in git-lfs (§7)
    url = https://lfs.example.com/acme/agent-history   # every remote's endpoint

[remote "origin"]               # one section per push remote (§11)
    url = https://github.com/acme/agent-history.git
    pushurl = …                 # optional; wins over url, as in git
    agentfsMaxBlobSize = 50m    # optional; §3.4, §11.3
    agentfsCredentials = /run/secrets/agentfs/origin   # optional; §11.1
    lfsurl = …                  # optional; the LFS endpoint, unless lfs.url (§7)
    agentfsLfsCredentials = /run/secrets/agentfs/origin-lfs   # optional; §7

[remote "backup"]               # an SSH remote (§11.1)
    url = ssh://git@git.example.com/acme/agent-history.git
    agentfsSshKey = /run/secrets/agentfs/backup/ssh-privatekey   # required for SSH
    agentfsKnownHosts = /run/secrets/agentfs/backup/known_hosts  # required for SSH
```

Every key is described in [configuration.md](configuration.md).

Settings and remotes may also come from operator-owned fragments named by
`include.path` in `$GIT_DIR/config` (a Kubernetes ConfigMap, §17.1). They
are read as part of the repository's own config, and checked more strictly
than git does: each include must be an absolute path to a readable file (a
missing include fails the volume instead of being skipped), must not set
`agentfs.volume` or `agentfs.branch`, and must not be under the backing
tree or the mount point (a mount preflight error, like credentials);
`includeIf` is refused.

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
- **Paths git refuses** are not recorded. A path with a component that is
  exactly `.git` is skipped silently when that `.git` is git's own data: a
  directory that is (or is becoming, by the markers below) a git directory,
  as after `git init` at the root or in a clone, or a gitfile or `.git`
  symlink pointing at a git directory in the tree. A `git clone` on the
  mount would otherwise raise an alarm per object. Any other `.git` (an
  ordinary directory or file someone named `.git`) raises `PathRefused`
  once per such `.git` per mount, so its contents never leave history
  without a trace. A git directory written by a custom template whose first
  files are none of the markers can raise that alarm once. Every other path
  git refuses raises an alarm (`PathRefused`, §13): `.git` look-alikes in their
  NTFS and HFS spellings, such as `.GIT`, `git~1` and `.git.`, names that
  are not UTF-8, and the checks of §3.4. A refused path is reported per
  path; it never fails the rest of its record, and the operation itself
  still succeeds in the backing tree.

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
  `repositoryformatversion`; or any file of git's default template: its
  `description`, its `info/exclude`, or one of its sample hooks
  (`hooks/<name>.sample` starting with `#!`). git copies the template in
  directory order, which differs between filesystems, so any of these may be
  written first: the FUSE tests caught `git clone --bare` recording its
  sample hooks before `description` existed.
- **Commit-time recheck.** A custom template's files written before `HEAD`
  match none of these marks, so the recorder journals them. When their
  records are committed, every write or symlink at a path new to history
  is checked again: if a proper ancestor directory has become a git
  directory (a bare layout) meanwhile, the change is dropped silently, like
  git's own data, and counted on the commit span
  (`agentfs.git_dir_dropped`). Paths already in history and removals are
  never dropped, and working trees (`.git` beneath) are not affected. The
  recheck needs the real backing tree, so it runs for `mount`, `record`
  and `restore`, and for `flush` and `run` only with an explicit `--root`.
  Two limits remain: a custom template's files committed before `HEAD`
  appears (a commit window of 200 ms, by default, between them) stay in
  history, and a new file recorded into an ordinary directory that becomes
  a bare repository within the commit window (`git init --bare` into a
  non-empty directory) is dropped. The in-memory tracked set keeps a
  dropped path until the next open, as it does a refused one.
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
    `main/.git/worktrees/wt` in a tree mounted at `/workspace`. The FUSE
    layer now knows its mount point, so this can become an exact mapping;
    that is a follow-up.
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
  - `chown`, `chmod` and `utimens` are passed through to the backing tree,
    and so are extended attributes (on regular files and directories).
    `chmod` of a symlink is not supported (Linux has no `lchmod`).
  - **Ownership of new objects.** The daemon performs each operation with
    its own privileges once the kernel has checked the caller's. With
    `CAP_CHOWN` and `CAP_FOWNER` (root, or a non-root daemon given both;
    decided from the effective capability set, not the uid), it then gives
    every file, directory, symlink and device it creates to the caller: the caller's group, except under a setgid
    directory, where the backing filesystem already chose the directory's
    group. It then sets exactly the mode the caller asked for, so the
    daemon's own umask never applies. Without those capabilities everything
    it creates belongs to its own user. Owner and mode are always set on a
    descriptor of the new object (opened with `O_PATH | O_NOFOLLOW` and
    checked to be what was just created: the right type, owned by the
    daemon, one link unless a directory), never by name, so a name swapped
    for a symlink or hard link in the backing tree meanwhile is refused
    with `ESTALE` and nothing outside is touched. `create` makes the backing
    file with `O_EXCL`: if another writer made the name first, an exclusive
    create fails with `EEXIST`, and any other opens the existing file as
    `open` would (checking the caller against its mode bits, with the
    caller's primary group only), without changing its owner or mode.
  - **POSIX ACLs are not enforced** through the mount (`FUSE_POSIX_ACL` is
    not negotiated); only mode bits are.
  - **Inode numbers.** fuser sends the attribute's inode number as the FUSE
    node id, so `st_ino` (and `d_ino` in directory listings) is agentfs's
    node id: one per backing inode, shared by hard links, kept for the life
    of the mount even after the kernel forgets the node (at one small map
    entry per backing inode seen), but not stable across remounts. Tools
    that cache inode numbers, such as git's index for a repository on the
    mount, re-read files once after a remount.
  - **Confinement.** The daemon reaches the backing tree only through a
    descriptor opened at mount and `openat2` with `RESOLVE_BENEATH`,
    `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS` (Linux 5.6+), and
    checks that every path still leads to the node's backing inode
    (`ESTALE` otherwise). A symlink anywhere in a path, whatever the
    workload did to the tree, is never followed. Path resolution is ordered
    against renames (a rename holds a lock for writing; every other
    operation, including `getattr`, `readlink` and the extended-attribute
    calls, holds it for reading), so an ancestor rename never makes a
    `stat` of an existing file fail.
- **Hermes.** Its tool containers run as root while files are owned by UID
  10000. git's "dubious ownership" check then needs `safe.directory` in those
  containers. That is a consumer setting (§16), not an agentfs one.

## 7. Large files and LFS (phase 2)

Built ahead of the rest of phase 2 (`agentfs_core::lfs`). Large content can be stored as Git LFS
objects, with pointer blobs in history that an ordinary `git clone` plus
`git lfs pull` turns back into content.

**Which store.** Decided when a file is recorded, on the frozen copy's
size, and saved in the journal with the change (`Change::Write.store`;
older journals read as blob), so the blob-size gate stays at record time
and a replay is deterministic:

1. **Always a blob:** empty files; git's special files (`.gitattributes`,
   `.gitmodules`, `.gitignore`, `.mailmap`, in any spelling git treats as
   one) and `.lfsconfig`; a path no `.gitattributes` line can name exactly
   (one holding a tab, CR or LF, or whose line would reach git's 2048-byte
   limit). Symlinks are never LFS.
2. **`filter=lfs`:** LFS. `agentfs.store=blob` may not override it.
   Operator `info/attributes` always counts; a tracked `.gitattributes`
   only with `respectTreeRules = true` (§4.1). Otherwise a tree
   `filter=lfs` path is stored as a blob, which git-lfs's smudge passes
   through unchanged, so clones still show the content (but `git status`
   in such a clone may report it modified).
3. **`agentfs.store=lfs` or `blob`** (operator attributes only).
4. **`agentfs.lfsThreshold`:** content strictly larger is LFS. **Unset by
   default**, so size-based LFS is off until an operator sets it: phase 1
   remotes have no LFS server, and pushes would stall.
5. Otherwise a blob.

The blob size limit (`agentfsMaxBlobSize`, §3.4) does not apply to
content stored as LFS: history holds only the pointer. Before the freeze
the store is decided on the size on disk, so an LFS file far over the
limit is still copied while a blob over it is not.

**Commit.** Each LFS write is hashed (SHA-256) into the local object
cache and committed as a canonical pointer blob (git-lfs spec v1:
`version`, `oid sha256:…`, `size …`, nothing else) with the file's mode,
so the executable bit is kept. Hashing happens at commit time, outside the
synchronous step (§3.2). The cache object is durable before the branch
moves; a crash in between leaves a reusable object.

**History must be readable by ordinary git.** Every commit rebuilds a
block at the end of the root `.gitattributes`, **in history only** (the
backing tree is never written):

```text
<the workload's own .gitattributes, byte for byte>
# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced
/big.bin filter=lfs diff=lfs merge=lfs -text
/dir[[:space:]]with[[:space:]]space/x.psd filter=lfs diff=lfs merge=lfs -text
# agentfs-lfs-end
```

- One line per path currently stored as LFS, anchored with a leading `/`
  and escaped so it matches that path alone (`\`, `*`, `?`, `[` get a
  backslash; a space is `[[:space:]]`, as `git lfs track` writes it), with
  exactly the attributes `git lfs track` writes. Tested against
  `git check-attr` and gix-attributes. `[[:space:]]` also matches a tab,
  so `a b`'s line also covers `a<TAB>b` (harmless: that path cannot be
  LFS itself).
- At the end of the file, so its lines win over the workload's (the last
  match wins). A deeper `.gitattributes` that unsets `filter` for a
  managed path would still make clones show pointer text; agentfs does
  not detect that.
- A path leaves the block when it is removed, rewritten as a blob (for
  example shrunk below the threshold) or replaced by a symlink or a
  directory. The block lists the head's managed paths plus this commit's
  LFS writes; the head's block is trusted because agentfs wrote it.
- The workload's `.gitattributes` content is kept; any block it wrote,
  well formed or not, is removed. A root `.gitattributes` holding only the
  block is created and removed by agentfs.
- Very many LFS paths make the file large (about 100 bytes a path, up to
  fsck's 100 MiB); a refused result fails the commit (`CommitFailed`)
  rather than writing history no remote accepts.

**Endpoints** come only from operator config (the state area's config and
its includes; a tracked `.lfsconfig` is content, never read), with
git-lfs's precedence:

| Uploads (push) | Downloads (restore) |
|---|---|
| `lfs.pushurl` | |
| `lfs.url` | `lfs.url` |
| `remote.<name>.lfspushurl` | |
| `remote.<name>.lfsurl` | `remote.<name>.lfsurl` |
| derived from `pushurl`, else `url` | derived from `url` |

- An explicit `http(s)` URL is the endpoint as written. A remote URL
  derives `<url>/info/lfs` when it ends in `.git`, else
  `<url>.git/info/lfs`, as git-lfs does (GitHub, GitLab and Gitea serve
  LFS there).
- A local path or `file://` URL, explicit or derived, uses git-lfs's
  standalone-file layout: `<git dir>/lfs/objects/aa/bb/<oid>` of that
  repository, which `git lfs pull` in a clone of it reads.
- SSH and `git://` remotes derive nothing: agentfs has no
  `git-lfs-authenticate`, so they need an explicit `lfsurl`.
- An explicit endpoint that is not `http(s)`, `file://` or a path, or is
  `https` in a build without the `https` feature, makes the remote
  misconfigured (§11.1); a bad global `lfs.url` misconfigures every remote.

**Credentials** are bound to the endpoint's origin (scheme, host and
port) and never sent anywhere else:

1. `remote.<name>.agentfsLfsCredentials` (an absolute path, the same file
   format as `agentfsCredentials`), if set;
2. else, when the endpoint has the push URL's origin (as GitHub's derived
   endpoint does), `remote.<name>.agentfsCredentials`;
3. else a password in the endpoint URL;
4. else none.

So a separate LFS host never sees the git host's credentials, and an
`http` endpoint never gets those of an `https` push URL. The files are
re-read when they change, like other credentials, and `mount` refuses
one under the backing tree or mount point. agentfs's `Authorization`
goes to batch requests, and to an action's `href` only when it has the
endpoint's origin and the server gave the action no `Authorization` of
its own; the headers a server gives an action are sent as given.
Redirects are never followed and proxy environment variables are
ignored.

**Client.** Neither libgit2 nor gitoxide implements LFS. agentfs's own
client (`ureq` 3, pure Rust; TLS through native-tls, OpenSSL and the
system trust store, under the `https` feature, as libgit2 uses) speaks
the batch API with the `basic` transfer only: batches of 100 objects,
sequential, a `PUT` of each object the server lacks, then `verify` when
the server asks for it; a `GET` for downloads, hashed while it is written
and kept only if it matches. Timeouts: 30 s to connect, 5 minutes for a
batch or verify request, and for an object 5 minutes plus its size at
256 KiB/s.

**Upload before push.** Before every push attempt, each remote uploads
the LFS objects the commits it lacks reference (`head ^ last_pushed`, or
the whole branch when the remote has nothing or its last pushed commit is
unknown): blobs a commit adds or modifies, compared with its first parent
without rename detection, at paths in that commit's managed block, that
are canonical pointers. Pointer text a workload wrote at an unmanaged path
is ordinary content. Every object must be in the local cache. Only when
every upload succeeds does the git push run; any failure fails the attempt
with stage `lfs` (§13) and pushes no ref, with the usual alarm and backoff
(§11.2). A remote whose backlog references no LFS object never resolves or
contacts an endpoint, so an SSH remote without `lfsurl` keeps pushing
until LFS content appears. Failures reuse the push failure kinds:
`unreachable` (connections, timeouts, HTTP 429 and 5xx), `auth` (401, 403,
a missing or rejected credential), `untrusted_host` (a TLS certificate
that fails verification, recognised from OpenSSL's message), `refused`
(any other status or per-object error, a redirect) and `local` (a missing
cache object, no endpoint, no `https` feature).

**Cache.** `<state>/lfs/objects/aa/bb/<oid>`, git-lfs's layout, with
`<state>/lfs/tmp/` for objects being written: written to `tmp/`, fsynced,
renamed into place, directories fsynced. `agentfs.state.bytes{part="lfs"}`
measures it.

**Restore and revert** (§15) are LFS-aware: a pointer at a managed path is
checked out as its object's content, from the cache or else downloaded
from the remotes in name order, and every object a plan writes is fetched
during the check, before anything is written; a missing one refuses the
restore (`lfs_object`). The tree's file is compared by size and SHA-256.
The root `.gitattributes` is compared and written without the managed
block, and one holding only the block is absent from the tree.

**Retention.** LFS objects cannot be pruned from most LFS servers
(GitHub's needs the repository deleted), and pruned pointers break
clones. agentfs prunes nothing, neither on servers nor in the local cache.
It limits growth at the source instead, with operator rules and, later,
coalescing intervals and size caps per path.

**Not built:** `git-lfs-authenticate` for SSH remotes, concurrent or
resumable (tus) transfers, a per-remote LFS size limit, pruning cache
objects every remote has, proxy settings, an `agentfs lfs fetch` command,
and storing the fsync class (§8) as LFS.

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
  distinct writer. A pid of 0 (an operator outside the daemon's pid
  namespace, §15) is unknown and gets no `Agentfs-Pid` line.
- **Restores and reverts** (§15) are commits of their own, never grouped
  with workload changes. Their subject is `restore <path> from <short id>`
  or `revert <short id>: <subject>`, they carry `Agentfs-Trigger: restore`
  and `Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>` (the full
  id), and their author, `Agentfs-Uid` and `Agentfs-Pid` are the
  operator's: the `agentfs` command's own ids, or, through a running mount,
  those of the process that asked (§15). Each journal record carries the
  operation it belongs to; older journals without the field still read.

## 10. Secrets (not in scope)

agentfs does not scan content for secrets and does not hold pushes.

- **Keeping secrets out** is done with ordinary operator rules (§4). Each
  consumer excludes its own secret files (§16).
- **A secret that gets recorded anyway** is permanent in history, locally and on
  every remote it reaches. Removing it means rewriting history: a documented
  operator procedure (§15,
  [history-rewrite.md](runbooks/history-rewrite.md)) for the state area
  and each remote, run with the
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
  global and ProgramData config search paths and its home directory are
  cleared once per process, and pushes use anonymous remotes with explicit
  `<oid>:<ref>` refspecs, so `remote.<name>.push` and `fetch`, any global
  `url.<base>.insteadOf` or `http.*` setting, and `~/.ssh/known_hosts` are
  never consulted.
- **An invalid remote section never stops the volume.** A remote section is
  left out (never pushed, and its limit does not apply) when:
  - its name is not a valid agentfs name (§11.4, e.g. `team/backup`, which
    git accepts);
  - `agentfsCredentials`, `agentfsSshKey` or `agentfsKnownHosts` is not an
    absolute path, or `agentfsMaxBlobSize` is not a positive size;
  - its push URL is SSH and `agentfsSshKey` or `agentfsKnownHosts` is
    missing, `agentfsCredentials` is set, the URL carries a password, or
    the URL has no host or a bad port;
  - its push URL is not SSH and `agentfsSshKey` or `agentfsKnownHosts` is
    set;
  - its push URL is SSH and the binary was built without the `ssh` cargo
    feature;
  - an LFS endpoint (`lfs.url`, `lfs.pushurl`, `remote.<name>.lfsurl` or
    `lfspushurl`) is not `http(s)`, `file://` or a path, or is `https` in
    a build without the `https` feature (§7); a bad global key makes every
    remote misconfigured;
  - `agentfsLfsCredentials` is not an absolute path.

  Each one raises `RemoteMisconfigured` (§13) when the volume opens;
  `agentfs status` lists it and `agentfs push` reports it and exits 1.
  Recording, committing and the other remotes carry on. Files that are
  missing or unreadable at push time are push failures instead (`auth` for
  a credential or key, `untrusted_host` for known_hosts), so a Secret
  mounted late or being rotated keeps working once it appears.
- **Credentials** come from files the recorder reads, and are re-read when they
  change, so short-lived tokens and keys can be rotated by whatever writes
  the file.
  - `remote.<name>.agentfsCredentials` (HTTP and HTTPS remotes) is an
    absolute path to a file holding either git-credential `key=value` lines
    (`username=`, `password=`; other keys are ignored) or a bare token, used
    as the password with the username from the URL, else `x-access-token`.
  - Every file is re-read when its stat stamp (device, inode, size, mtime,
    ctime) changes. The stamp follows symlinks, so kubelet's atomic `..data`
    swap for Secret volumes and a rename over the file are both seen. An
    in-place rewrite of the same size within the filesystem's timestamp
    granularity would be missed.
  - A credential or key is bound to the host (and port) of the remote's push
    URL and is never given to any other host.
  - It gets one attempt per connection: a rejected credential or key is an
    authentication failure, not a retry loop.
  - HTTPS needs libgit2 built with TLS: the `https` cargo feature (OpenSSL).
  - `remote.<name>.agentfsLfsCredentials` (same format) authenticates an
    LFS endpoint on another origin than the push URL; an endpoint on the
    push URL's own origin reuses `agentfsCredentials` (§7). LFS traffic
    uses agentfs's own HTTP client (`ureq`), not libgit2, with the same
    rules: bound to its origin, no redirects followed, no proxies, TLS
    through OpenSSL and the system trust store with the `https` feature.
- **SSH remotes** (`ssh://`, `ssh+git://`, `git+ssh://`, and scp-like
  `[user@]host:path`, which needs a `:` before any `/`) need libgit2 built
  with libssh2: the `ssh` cargo feature (libssh2 and OpenSSL).
  - `remote.<name>.agentfsSshKey` is an absolute path to an unencrypted
    private key, OpenSSH (`BEGIN OPENSSH PRIVATE KEY`) or PEM (`RSA`, `EC`,
    PKCS#8), handed to libssh2 from memory, so no public key file is
    needed. Encrypted keys and `.pub` files are refused with an `auth`
    failure that says why. There is no ssh-agent and no passphrase support.
  - The user is the URL's, else `git`. libgit2's request for a user name
    before authenticating is answered without counting as the attempt.
  - The host key is verified by agentfs, in libgit2's certificate callback,
    which runs before any authentication, against
    `remote.<name>.agentfsKnownHosts`: an absolute path to an OpenSSH
    known_hosts file, required for every SSH remote. The host is looked up
    as the push URL's host, or `[host]:port` on a port other than 22, and
    the host libgit2 connected to must be that host. Plain and hashed
    (`|1|…`) names, comma lists, `*` and `?` wildcards, `!` negation and
    `@revoked` are honoured; `@cert-authority` (host certificates) is not
    supported. An unknown host, a changed key, a key type the file does
    not list for the host and a revoked key are all refused, so the key is
    never offered to that host, and the push fails as `untrusted_host` with
    the file, the name looked up, the key type, its SHA256 fingerprint and
    the `ssh-keyscan` command that lists the server's keys. Nothing is ever
    accepted on first use or written back.
  - libssh2 picks the host key type (ECDSA, then Ed25519, then RSA) before
    agentfs sees it, and git2 offers no way to steer that, so known_hosts
    must list every type the server has, as `ssh-keyscan` prints them.
  - libgit2 built with SSH also sends a local path containing a `:` after a
    `/` (`/srv/a:b.git`) to its SSH transport. agentfs treats such a path as
    local, as git does, so with the `ssh` feature its push fails, closed:
    neither a credential nor a key is offered and no host key is trusted.
- **Tested end to end in CI:** local, `file://`, `http://` and `https://`
  remotes in `ci/e2e-remotes.sh` (nginx and `git http-backend`), and SSH
  remotes in `ci/e2e-ssh.sh` (OpenSSH's sshd with a `git-shell` user and
  key-only authentication): Ed25519 OpenSSH and RSA PEM keys, plain and
  hashed known_hosts, a wrong key, unknown and changed host keys (the key
  is never offered: the sshd log shows no attempt with it), a key rotated
  under a running `agentfs run`, and a divergence over SSH.
  Git LFS (§7) in `ci/e2e-lfs.sh` against git-lfs's `lfs-test-server`
  behind the same nginx.

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
- **A per-remote lock** (`flock` on `meta/push/<remote>.lock`) keeps two
  push attempts on one remote from overlapping. Since the state-area lock
  (§2.3) a manual `agentfs push` can no longer run beside the service, so
  this guards only pushes within one process.
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
  non-fast-forward, authentication failure, an untrusted host (an SSH host
  key or TLS certificate that fails verification), an unreachable remote,
  or any of these while uploading LFS objects before the git push (stage
  `lfs`, §7: no ref is pushed then).
  Recording, committing and other remotes carry on. The recorder:
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
  - An operator resolves it (§15,
    [divergence.md](runbooks/divergence.md)). The divergence clears when a
    branch push next succeeds.
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
| `agentfs.push.failures` | counter, per remote, reason and stage | Rejected, non-fast-forward, authentication, untrusted host, network; stage `git`, or `lfs` for the LFS upload before the git push (§7) |
| `agentfs.push.diverged` | gauge, per remote | 1 while diverged |
| `agentfs.remote.misconfigured` | gauge, per remote | 1 for each invalid `[remote]` section (§11.1) |
| `agentfs.precheck.rejected_paths` | counter | Paths refused by fsck or limit checks, by stage and reason |
| `agentfs.patch.queue_depth`, `agentfs.patch.blocked` | gauge | Phase 3 |
| `agentfs.state.bytes` | gauge | Size of the state area by part |
| `agentfs.telemetry.export_failures` | counter, per signal | Telemetry exports (traces, metrics, logs) that failed; counted by the binary, exported once metrics get through |

Metric attributes are short and low-cardinality: `trigger`, `remote`,
`reason` (a refusal label or a push failure kind), `stage` (`record` or
`commit` for refused paths, `git` or `lfs` for push failures) and `part`
(`git`, `journal`, `pending`, `lfs`, `meta`). Paths never become metric
attributes. Span and log attributes are namespaced `agentfs.*`.

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
outcome, on failure the reason, the stage and any diverged ref, and when
the backlog references LFS objects `agentfs.lfs.objects`,
`agentfs.lfs.uploaded` and `agentfs.lfs.bytes`). Each span is
current while its operation runs, so alarm logs raised inside it carry its
trace. Record and commit spans are not linked; they correlate through the
sequence numbers. Patch-application spans come with phase 3.

**Logs.** A structured log record for every alarm condition: the event name
(`agentfs.path_refused`, `agentfs.push_failed`, ...), WARN for a refused
path and ERROR otherwise, the event's message as the body and its fields
(path, reason, remote, kind, heads, ...) as attributes; a push failure
carries its stage as `agentfs.push.stage`.

**Events.** The engine reports alarm conditions as events to a sink
(`agentfs_core::events::EventSink`). The telemetry layer wraps the
process's sink once (`Telemetry::sink`), turning events into the counters
and logs above; the CLI also prints alarms to stderr.

| Event | Raised when |
|---|---|
| `PathRefused` | A path fails the checks of §3.4 or §5, at record or commit time |
| `RecordFailed` | The synchronous step fails (§3.5) |
| `CommitFailed` | Turning records into a commit fails; the records stay journaled and are retried |
| `PushFailed` | A push attempt fails, with the remote, the kind (`unreachable`, `auth`, `untrusted_host`, `non_fast_forward`, `refused`, `local`), the stage (`git`, or `lfs` when uploading LFS objects failed and no ref was pushed, §7), the head, the last pushed commit, the consecutive failure count and the next retry |
| `Diverged` | The local head was saved under a diverged ref (§11.3) |
| `RemoteMisconfigured` | A `[remote "<name>"]` section is invalid, so that remote is left out (§11.1); raised when a volume opens and by `agentfs push` |
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

**Failed exports are visible.** Each exporter is wrapped: an export that
fails (the collector is down, refuses with an HTTP error, or times out)
prints `warning: telemetry: <signal> export failed: <error>` on stderr at
once, then at most once a minute with the number of failures in a row,
and `telemetry: <signal> export recovered after N failure(s)` when one
succeeds again. Each failure also counts in
`agentfs.telemetry.export_failures{signal}`, so a collector that drops
only traces or logs shows up in metrics. A failure never stops a command.

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

## 15. Recovery and undo

Recovery may be manual, but it must be supported and documented. Phase 1
delivers two commands, `agentfs restore` and `agentfs revert`, and
operator runbooks ([runbooks/](runbooks/)):

| Runbook | For |
|---|---|
| [recovery-and-undo.md](runbooks/recovery-and-undo.md) | Finding a change, pausing the workload, restore and revert, refusals, plain-git alternatives |
| [divergence.md](runbooks/divergence.md) | Resolving `refs/agentfs/<volume>/diverged/*` (§11.3) |
| [history-rewrite.md](runbooks/history-rewrite.md) | Removing a recorded secret from the state area and each remote (§10) |
| [stuck-pushes.md](runbooks/stuck-pushes.md) | Pushes that keep failing |
| [crashed-sidecar.md](runbooks/crashed-sidecar.md) | A sidecar whose daemon died: `ENOTCONN`, a holding sidecar, the pod restart (§17.1) |

**The supported path:**

1. **Find the change.** History is ordinary git on the remote, or in the
   state area: `git log --follow <path>`, `git show`, the remote's web
   view. Trailers (§9) say which pod, process and trigger made each
   change.
2. **Pause the workload** when possible. A live restore is supported,
   but a workload writing the same files can overwrite it again (its
   writes are recorded too), and a file the workload opens and writes in
   the moment between a path's last re-check and its write (see Safety)
   has those writes replaced before they are recorded.
3. **Restore or revert:**
   - `agentfs restore <path> <commit>` writes `<path>` (a file, symlink,
     directory, or `.` for the whole tree) as it was in `<commit>`,
     following `git restore --source=<commit> -- <path>`: every entry the
     commit has there is written, every entry history has there now but
     the commit did not is removed, and untracked files are never
     touched.
   - `agentfs revert <commit>` applies the inverse of the commit's diff
     against its first parent (the empty tree for a root commit), without
     rename detection. Merge commits are refused.
   - `<commit>` is any git revision; `HEAD` means the volume's branch. It
     must be on the branch (the head or an ancestor); other commits,
     including diverged refs, are restored with plain git.
   - With the workload stopped, plain git also works: `git show
     <commit>:<path> > <file>` into the backing tree, then `agentfs record
     <path>`.
4. **Resume the workload.**

**Two modes, one engine** (`agentfs_core::restore`: plan, check, apply):

- **Through a running mount.** `agentfs mount` listens on
  `meta/control/agentfs.sock` (§2.2): directory `0700`, socket `0600`,
  and each connection's `SO_PEERCRED` must be root or the daemon's user.
  Both ends address it as `/proc/self/fd/<dir fd>/agentfs.sock`, so a long
  state path never hits the 108-byte `sun_path` limit. The peer's pid is
  0 when it is in a pid namespace the daemon cannot see; it is then
  unknown and named in no `Agentfs-Pid` (§9). The protocol is JSON
  lines, version 1, one request per connection, at most 64 KiB, read
  within 10 s; a restore request must name its path (`.` for the whole
  tree; a missing path is refused, never taken as the root); one restore
  runs at a time and a second gets `busy` (exit 75). The daemon waits
  until every record made before the request, including one still being
  written to the journal, is committed (at least 10 s, or 20 commit windows; otherwise refused as
  history being behind), checks against its mount point, and writes
  through its own mount from the connection's thread. The writes go
  through the kernel like any workload's, so kernel caches, the inode
  table and dirty detection stay coherent, and the FUSE layer records
  them. The control socket is shut down before the unmount.
- **Directly.** With nothing listening, no mount (and so no workload
  through one) can be running. The command takes the state-area lock
  (§2.3), commits what is journaled, writes the backing tree (`--root`)
  through `openat2` descriptors confined like the FUSE layer's (§6), and
  records what it wrote. `run` serves no restores, so while it (or
  `record`, `flush`, `push`) holds the lock, restore exits 75.

**Recording.** The process applying a restore (the CLI's pid, or the
daemon thread's id, which FUSE reports as the caller's pid; the daemon's
pid is registered too) is registered as the operation's **actor** while
it runs. Every record by an actor, including late RELEASE and sweep
records naming it as the last writer, gets trigger `restore`, the operator
(the CLI's ids, or the socket peer's) as its caller, and the operation in
its journal record (an optional field older journals lack). The committer
gives each operation its own commit, never grouped with workload records
and held open while the operation runs, with `Agentfs-Trigger: restore`
and `Agentfs-Restore: <commit>` or `Agentfs-Revert: <commit>` (§9).
Workload records interleaved with a live restore split it into more than
one commit, since commits follow the journal's order.

**Safety.** Every check runs before anything is written:

- everything written must be recordable: the path rules (§4, including
  `-agentfs` and ignores for untracked paths) and every pre-commit check
  (§3.4) on the content;
- the tree must hold what history says at every path the restore covers,
  including paths history already has right; anything else is a change
  not in history, and is never overwritten, even with `--force`. The one
  exception is a tree that already holds the restore's target where
  history does not (a restore stopped between writing and recording, or
  content put back by hand): nothing there would be lost, so direct mode
  records it as part of the operation instead of calling it done, and a
  mount, which records only what it sees written, refuses it as not
  recorded;
- nothing untracked may be in the way: a file where a restored one goes,
  a directory with files where a file goes, or a symlink or file as a
  parent directory (symlinks are never followed);
- `--force` overrides only refusals where nothing is lost: a revert of
  paths changed again since the commit (the later versions stay in
  history), and files a workload holds open for writing through the
  mount.

**Re-checks and failures.** Each path is re-checked just before it
changes: one that no longer holds what the check saw, or (unless
`--force`) that a workload opened for writing through the mount since,
is left alone and the operation stops there. A window remains between
that re-check and the write: a workload that opens and writes the file
in that moment, without closing it, has those writes replaced before
they are recorded, so nothing unrecorded is lost only while the workload
is paused. Through a mount, the check's own read of a file a workload
has written but not closed ends with a FLUSH of a changed inode, a change
boundary (§3.1): that content is recorded before the refusal, so a retry
finds it in history and only the open handle (overridable) remains. A
failure part way records what was written and reports the error. A file
is rewritten in place only after the space its new content needs is
allocated, so a full disk or a file size limit leaves it whole; a write
that fails after that (an I/O error) can leave it part written, and
direct mode then records it as it is, so tree and history agree. A
re-run records what the tree already holds and writes the rest.

**Writing.** Removals run deepest first and prune directories they leave
empty (never above the restored path's parent); writes run shallowest
first. A regular file with one link is rewritten in place, keeping its
inode, owner, other mode bits and extended attributes, with only its exec
bits changed; a hard-linked file, a type change or a new file is a new
inode, so only the named path changes, as in git. New files and
directories take the owner and group of their nearest existing parent
when the process may chown (`CAP_CHOWN`), with mode `0644`/`0755` and
`0755`. Content, the executable bit and symlinks are restored, nothing
else (§6). Content stored as LFS is written from the object cache, or
downloaded from a remote during the checks, before anything is written
(§7); the root `.gitattributes` is written without the managed block.

**Operator history rewrite** removes content that must not stay in
history, such as a recorded secret (§10). It is a documented procedure,
[history-rewrite.md](runbooks/history-rewrite.md), not a command:

- run against the state area's repository and every remote the content
  reached, with the workload stopped, never automatically;
- `git filter-repo --prune-empty never` in a mirror clone of the state
  repository, so every commit and its `Agentfs-Seq` survive and
  filter-repo never edits the state area's config or remotes; the
  rewritten branch is fetched back, the reflog expired and old objects
  collected, and the push state (`meta/push/*.json`) deleted;
- on each remote, a force-push of the volume's branch with a lease, the
  one case where agentfs's no-force-push rule is set aside, by an
  operator, plus deleting the volume's other refs there and the host's
  own purge.

**Not built yet** (§19.2): a read-only history view for operators, undo
through the patch queue (§12), and asking a running daemon to push now.

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

The rule files are in `deploy/examples/hermes`, and the pod changes, the
one-time migration of Hermes' existing data and the initial import in
[deployment.md](deployment.md#10-hermes).

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
  - Git LFS (§7, `lfs`): store selection, pointers, the managed
    `.gitattributes` block, the object cache, endpoints and credentials,
    the batch client, uploads before each push, and LFS-aware restores;
  - the patch queue (§12);
  - telemetry (§13): implemented against the OpenTelemetry API.
- **`agentfs-fuse`, a library:** the FUSE layer (§2.1, §3.1, §6).
  - `Ops`: every filesystem operation over the backing tree, independent of
    the kernel, so all of its behaviour is unit-tested against real
    directories and state areas; the inode table; dirty detection; the
    change boundaries; the repair queue (§3.5).
  - The fuser adapter, the sweep thread, the layout preflight, mount and
    unmount.
  - It is a separate crate so the core stays free of FUSE and the binary
    stays a thin CLI.
- **`agentfs`, a binary:**
  - `init`, `mount`, `probe`, `status`, `flush`, `record`, `push`, `run`,
    `restore` and `revert`;
  - `init` is safe to re-run (§2.3): exit 0 for the volume it already
    holds, 1 for another volume or branch; `--include` sets the config's
    `include.path` entries (§4.3), and `--root` creates the backing tree;
  - `probe <mountpoint> [--allow-stale]` reports a live agentfs mount
    (exit 0), a stale one whose daemon died (1, or 0 with
    `--allow-stale`), nothing, or another filesystem (1), from
    `/proc/self/mountinfo` and a `statfs`; no lock, no state area. It is
    the sidecar's startup and readiness probe;
  - `push` pushes once to every remote (or those named with `--remote`) and
    exits 1 unless all succeeded; `run` runs the service in the foreground
    until SIGINT or SIGTERM, makes a final commit, waits up to
    `--push-grace` (default 5s) for pushes in progress, and exits at once on
    a second signal. `mount`, `flush`, `record`, `push` and `run` export
    telemetry (§13) and take the pod flags (`AGENTFS_POD`, ...), so every
    command's telemetry resource is the same. `record` records paths of a
    backing tree directly, without a mount;
  - `status` is read-only (§2.3): the head, the last committed sequence,
    the backlog, the state-area lock's holder, the LFS threshold, and each
    remote: its URL (userinfo redacted), last pushed commit, unpushed
    commits, consecutive failures, last error (with its stage) and any
    diverged ref, and its LFS upload endpoint and where that endpoint's
    credentials come from, or why it has none (resolved from config, no
    network);
  - `restore <path> <commit>` and `revert <commit>` (with `--force` and
    `--dry-run`) write content from history back into the tree, recorded
    as one commit (§15): through a running mount over its control socket,
    or directly into `--root` when nothing runs. Exit status 0 done, 1
    refused (a reason and a hint) or failed, 75 busy;
  - `mount`, `run`, `record`, `flush` and `push` take the state-area lock
    (§2.3) and exit 75 while another process holds it; `restore` and
    `revert` take it only in direct mode.
- **`agentfs mount <backing> <mountpoint>`** runs the FUSE session, the
  commit loop and the push loops in one process.
  - **Preflight.** Before anything is mounted it refuses a state area,
    backing tree and mount point that are not three separate directories
    (none inside another, after resolving symlinks), any remote section's
    `agentfsCredentials`, `agentfsSshKey`, `agentfsKnownHosts` or
    `agentfsLfsCredentials` file under
    the backing tree or the mount point, invalid sections included (an
    error, not `RemoteMisconfigured`: carrying on would expose the secret,
    or let the workload rewrite known_hosts to trust any host), a
    `--root` that is not the backing tree, an `include.path` fragment
    under the backing tree or the mount point, and a stale mount point
    (`ENOTCONN`, with the command that clears it). A missing mount point
    is created first (one level, mode 0755).
  - **A stale mount point with `--on-stale-mount hold`** (a sidecar
    restarted after its daemon died): it mounts nothing and takes no
    lock, says so, waits for SIGINT or SIGTERM, then lazily detaches the
    mount point if it is still a stale agentfs mount, and exits 1.
  - **Kernel options.** fuser, multi-threaded (`--threads`, default the CPU
    count between 2 and 16) with a cloned `/dev/fuse` descriptor per thread;
    `default_permissions`, `allow_other` (unless `--no-allow-other`),
    `nodev`, `nosuid`, `rw`, fsname `agentfs:<volume>`, subtype `agentfs`.
    INIT asks for `FUSE_ATOMIC_O_TRUNC` and `FUSE_PARALLEL_DIROPS`; never for
    the writeback cache (FLUSH must see every write the daemon serves),
    POSIX or BSD locks (they stay in the kernel, which is right for one
    mount) or POSIX ACLs. No `auto_unmount`: a crashed daemon leaves
    `ENOTCONN` rather than exposing the empty mount point to writes nothing
    would record. Entries are cached 1 s, attributes not at all.
  - **Passthrough.** `--passthrough auto` (default) asks for
    `FUSE_PASSTHROUGH` (maximum stack depth 1) when the kernel offers it and
    the daemon may register backing files (`CAP_SYS_ADMIN` in the initial
    user namespace: root, not root inside a rootless container), and keeps
    one backing id per inode while any handle on it is open. If registering
    fails for one inode (a stacked backing filesystem such as overlayfs,
    `EMFILE`), only that inode is served through the daemon while it has a
    handle open; every other inode keeps passthrough, and an inode that
    already has a backing id keeps using it (the kernel refuses to mix the
    two on one inode with `ETXTBSY`). Each fallback is logged ("passthrough
    refused for inode ..."; the first ten). `on` refuses to mount without
    the privilege, fails at INIT without kernel support, and if a
    registration fails later it fails that open with `EIO` and stops the
    mount (exit status 1); `off` never asks. The startup line reports the
    result and the reason. CI's `ubuntu-latest` kernel negotiates it (the
    `fuse` job runs `auto`, `on` and `off`, and asserts no open fell back).
  - **Restores.** After mounting it serves `restore` and `revert` on its
    control socket (§15).
  - **Shutdown** on SIGINT, SIGTERM or an external unmount: stop the
    control socket (waiting up to `--unmount-grace` for a restore in
    progress, which writes through the mount), unmount (a
    plain unmount, then a lazy detach if busy; `fusermount3 -u`, then
    `-u -z`, when not root), wait up to `--unmount-grace` (10 s) for the
    session to end, run a final sweep, make the final commit, wait up to
    `--push-grace` for pushes, flush telemetry, release the lock. A second
    signal exits 130.

  It works anywhere FUSE does, with no Kubernetes dependency.
- **Phase 1 deployment** ([deployment.md](deployment.md), manifests in
  `deploy/sidecar`): a privileged native sidecar (an init container with
  `restartPolicy: Always`) in the workload's pod, from the image
  `ghcr.io/mcurcio/agentfs` (§17.3).
  - **Volume.** The backing tree and the state area live on one
    `ReadWriteOncePod` PVC, mounted once at `/data` as `backing/` and
    `state/` (two subPath mounts would break reflinks with `EXDEV`,
    §3.2). The Deployment uses the `Recreate` strategy, so one pod writes
    the volume (§2.3).
  - **Init container** `agentfs init --include /etc/agentfs/config`, as
    root with every capability dropped: creates the state area and the
    backing tree on the first start, accepts its own volume and refuses
    another on every later one.
  - **Mount.** The sidecar mounts the FUSE filesystem at
    `/agentfs/workspace`, a subdirectory of an `emptyDir` it mounts at
    `/agentfs` with Bidirectional propagation; workload containers mount
    the `emptyDir` with HostToContainer propagation (or the mount itself
    with `subPath: workspace` at a fixed path, as Hermes needs, §16). A
    subdirectory, not the `emptyDir`'s root, so that a dead daemon leaves
    only the subdirectory answering `ENOTCONN`: a restarted sidecar can
    still be created, and the kubelet can still tear the pod down.
  - **Probes.** Startup: `agentfs probe --allow-stale /agentfs/workspace`
    (workload containers start once the mount is live; 5 minutes for
    journal recovery). Readiness: `agentfs probe /agentfs/workspace` (a
    dead daemon shows as a NotReady pod). No liveness probe.
  - **Configuration.** Settings and remotes from a ConfigMap fragment
    (`include.path`, §4.3); the operator rules from the same ConfigMap,
    mounted read-only over `info/exclude` and `info/attributes` in the
    sidecar (a change rolls the pod through the generated name's hash);
    credentials from a Secret; the pod flags from the downward API.
  - **A crashed daemon.** The workload's calls fail with `ENOTCONN`
    (nothing is written where it would not be recorded). The kubelet
    restarts the sidecar, which runs with `--on-stale-mount hold`: it does
    not remount while the workload holds the dead mount, so the pod goes
    NotReady. The recovery is a pod restart, by hand in phase 1
    (`kubectl delete pod`, [runbook](runbooks/crashed-sidecar.md)): the
    holding sidecar, stopped after the workload containers, detaches the
    stale mount so the pod's volumes can be removed, and the new pod
    recovers the journal and pushes. A sidecar that exits cleanly while
    the pod runs is not held (a known gap, [deployment.md](deployment.md#7-a-crashed-daemon)).
  - **Privilege.** `privileged: true` is what Bidirectional propagation
    requires; its cost and mitigations are in
    [deployment.md](deployment.md#9-security-trade-offs-of-the-privileged-sidecar).
    The CSI driver (phase 4) removes it from workload pods.

### 17.2 Layer 2: CSI driver (Rust)

Phase 4 (§14).

### 17.3 Libraries (verified 2026-10-12)

| Need | Choice | Notes |
|---|---|---|
| FUSE | `fuser` 0.18 | Active. Multi-threaded dispatch with cloned fds, passthrough (`BackingId`). Default features are pure Rust: `mount(2)` as root, `fusermount3` otherwise, no libfuse. It sends the attribute inode number as the node id, so `st_ino` is the node id (§6) |
| Git objects, commits, push, credentials | `git2` (libgit2) | Active. Has **no** filter or merge-driver registration, and its fsync option is only reachable through `libgit2-sys` |
| TLS and SSH transports | `git2` features `https` (OpenSSL) and `ssh` (libssh2 through `libssh2-sys`, over OpenSSL), behind agentfs's `https` and `ssh` cargo features | C libraries, so feature-gated: default builds stay free of OpenSSL. `ssh` keys are handed to libssh2 from memory (`Cred::ssh_key_from_memory`) |
| SSH host keys | agentfs's own known_hosts parser over `base64` and `sha1_smol` (HMAC-SHA1 for hashed names, written by hand and tested against RFC 2202) | Pure Rust. libgit2's own check reads `~/.ssh/known_hosts` only, so agentfs checks in the certificate callback instead (§11.1) |
| Ignore and attribute rules | `gix-ignore`, `gix-attributes` | Pure Rust; chosen over libgit2's rules for the reasons in §4.4. gitoxide has no push, so it does not replace `git2` |
| LFS | none exists: implemented in agentfs (§7) over `ureq` 3 (default features off: pure Rust, no TLS) and `sha2` 0.11; the `https` feature adds `ureq`'s `native-tls` (OpenSSL, as `git2/https`; its webpki root bundle is compiled in but unused: the platform verifier reads the system store) | ureq's `native-tls-no-default` feature does not compile the native-tls connector in 3.4 (its code is behind the `native-tls` feature), hence `native-tls`. `reqwest` stays in the binary only, for OTLP |
| Telemetry | `opentelemetry` 0.33 (API) in the core; `opentelemetry_sdk` and `opentelemetry-otlp` (`http-proto`, `reqwest-blocking-client`, no TLS) in the binary | OTLP http/protobuf export. Verified pure Rust: no OpenSSL, ring, aws-lc, cmake or protoc |
| Repack | the `git` CLI | libgit2 has no repack or gc |
| Container image | `debian:trixie-slim` with `fuse3`, `libssl3t64`, `ca-certificates` and `git`; built on `rust:1.99-trixie`; both pinned by digest | Not distroless: operators follow the runbooks with `git` through `kubectl exec`. Runs as root (§17.1). `linux/amd64` only; published to `ghcr.io/mcurcio/agentfs` as the commit sha and `latest`, only after the kind e2e passes |

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

**Where they run.** Tests 1, 2, 4, 5, 6, 7 and 11 run through real mounts
in the `fuse` CI job (`crates/agentfs/tests/mount.rs`, as root, with
passthrough `auto`, `on` and `off`), along with SQLite in WAL and DELETE
mode and git (`gc`, `fsck`) on the mount, though not test 14's restore of
every recorded version. Test 11 also runs as `ci/e2e-recovery.sh` in the
same job (the recovery runbook end to end, ending with a push of the
restore commits), and without a mount in `crates/agentfs/tests/cli_restore.rs`;
the divergence and history-rewrite runbooks run in the `e2e-runbooks` job.
Tests 8 and 12 (all but the record and commit failure counters, plus the
export-failure counter) run in the `e2e` jobs against a real git server
and OpenTelemetry Collector; test 8 runs over smart HTTP and HTTPS
(`e2e-remotes`) and over SSH (`e2e-ssh`, against OpenSSH's sshd). The
`e2e-kind` job (`ci/e2e-kind.sh`, in the `image` workflow) runs the
phase 1 deployment in a kind cluster with the image just built: test 4
in a pod (root and uid 10000 writers in separate containers, one through
a `subPath` of the FUSE mount), test 5 for the writers (no `/data`,
nothing but the tree under the mount), the operator-rules part of test
6, test 8 over HTTP from a pod with the credential Secret (authors and
pod, namespace and node trailers checked on the remote), test 11 through
`kubectl exec` and the control socket, the final commit on pod
deletion, and test 2 in a pod: `kill -9` of the daemon from the node,
`ENOTCONN` for the workload, the holding sidecar and NotReady pod,
`kubectl delete pod` completing, and the new pod recovering and pushing
the pre-crash write. Test 14's LFS part runs in the `e2e-lfs` job
(`ci/e2e-lfs.sh`: git-lfs's `lfs-test-server` behind nginx, the real
git-lfs client, `git clone` plus `git lfs pull`, over http and https)
and without a server in `crates/agentfs/tests/cli_lfs.rs` (local and
`file://` remotes; `git lfs pull` where git-lfs is installed, as on CI
runners). Not automated yet: test 3 (a kill at every step), 13
(performance), 14's SQLite part (§8 is not built) and 16.

## 19. Decisions and open questions

### 19.1 Decisions (2026-10-12)

| Topic | Decision |
|---|---|
| Storage model | Real backing tree with FUSE passthrough; the projection is logical only |
| Several pods on one volume | Supported by the CSI driver, one committer identity per replica (§14, §9) |
| Rejected pushes | Alarm, retry, keep operating (§11.3) |
| Remotes | Any git remote, several at once (§11.1) |
| Push timing | Interval, independent of pod lifecycle (§11.2) |
| Recovery | `agentfs restore` and `agentfs revert`, plus operator runbooks; further tooling discovered from use (§15) |
| Telemetry | OpenTelemetry (§13) |
| Secret scanning | Not in scope (§10) |
| Repository layout | Several volumes may share a repository, each with its own branch and ref namespace (§11.4) |
| Identity | Configurable templates with defaults (§9) |
| Backing filesystem | Both supported. XFS (reflink) makes the synchronous step size-independent; ext4 falls back to a copy. The phase 1 benchmark sets the documented recommendation (§3.2) |
| One process per state area | An exclusive `flock` on `meta/lock`, taken by `mount`, `run`, `record`, `flush`, `push`, and `restore` and `revert` in direct mode; exit 75 when held, `--wait-lock` to wait; `status` is read-only (§2.3) |
| FUSE layer placement | A separate library crate, `agentfs-fuse`, whose kernel-independent `Ops` carries all behaviour; the fuser adapter only maps replies (§17.1) |
| Dirty detection | Armed inodes compared by stat stamp, with git's racy-timestamp rule; created and `O_TRUNC`-opened files always recorded at their next close (§3.1) |
| `.git` paths | An exact `.git` component that is a git directory or gitfile is skipped without an alarm; any other `.git`, look-alikes and invalid names raise `PathRefused` (§5) |
| Layout | The state area, backing tree and mount point are separate; credentials are never under the backing tree. `mount` refuses otherwise (§17.1) |
| Passthrough | `auto` by default: used when the kernel and privileges allow, off with the reason reported otherwise, and switched off per inode (not per mount) if registering its backing file fails; `on` fails the mount instead (§17.1) |
| Failed namespace records | `EIO` plus a repair queue the sweep drains from the backing tree's current state, kept in `meta/repairs` across mounts and drained before the next mount serves (§3.5) |
| Privileges | Decided from the effective capabilities, not the uid: `CAP_CHOWN` + `CAP_FOWNER` to give new objects to their caller, `CAP_SYS_ADMIN` in the initial user namespace for passthrough (§6, §17.1) |
| `st_ino` | The FUSE node id, stable while mounted and shared by hard links, not across remounts (§6) |
| `.gitmodules` checks | git fsck's `gitmodulesParse`, `Name`, `Url`, `Path`, `Update` and `Large` are refused at record and commit time (`gitmodulesParse` too, though git only reports it); a port of git's config parser and URL checks, tested against the `git` binary; `http(s)` URLs must pass both git 2.39's and 2.40+'s checks (§3.4) |
| Synchronous-step bound | Content scans of special files run at record time only up to 1 MiB, the tree rule file bound; larger files are scanned in full at commit time. Size checks always run at record time (§3.4) |
| Nested custom-template gap | At commit time, a write at a path new to history under a directory that has since become a bare git directory is dropped silently; only with the real backing tree known (§5) |
| Telemetry export failures | Each OTLP exporter is wrapped: a stderr warning at once, then at most once a minute with a count, a recovery line, and the counter `agentfs.telemetry.export_failures{signal}`; telemetry still never stops a command (§13) |
| Restore modes | Live restore is supported: with a mount running, `restore` and `revert` ask it over the control socket and it writes through its own mount, so caches and dirty detection stay coherent; with nothing running they take the lock and write the backing tree directly; `run` serves none (exit 75) (§15) |
| Restore semantics | `git restore --source=<commit> -- <path>` for restore, the inverse of the first-parent diff for revert; only commits on the volume's branch; untracked files never touched (§15) |
| Restore safety | Every check before any write; content not in history is never overwritten, even with `--force`; `--force` overrides only what loses nothing (later recorded changes to a reverted path, files open for writing through the mount); each path is re-checked just before it changes; a partial apply is recorded, a tree already holding the target is recorded (direct) or refused (mount), and a re-run finishes the job (§15) |
| Operation commits | The applying process or thread is registered as an actor; its records get trigger `restore`, the operator as author, and the operation in the journal record; each operation is its own commit with `Agentfs-Restore` or `Agentfs-Revert` (§9, §15) |
| Control socket | `meta/control/agentfs.sock`, directory 0700, socket 0600, peer root or the daemon's user, addressed through `/proc/self/fd`; JSON lines, protocol v1, one operation at a time (§2.2, §15) |
| History rewrite | A documented procedure with `git filter-repo --prune-empty never` in a mirror clone, then a force-push with lease per remote by an operator; not a command (§10, §15) |
| SSH remotes | Built with the `ssh` cargo feature (libssh2). Each SSH remote names an unencrypted private key (`agentfsSshKey`) and a known_hosts file (`agentfsKnownHosts`), both required and both re-read when they change; the key is bound to the push URL's host and port and offered once per connection, as the URL's user, else `git`. No ssh-agent, no passphrases, no global or `~/.ssh` files (§11.1) |
| Host key verification | Done by agentfs against `agentfsKnownHosts`, in libgit2's certificate callback, before any authentication; plain and hashed names, `[host]:port`, wildcards, negation and `@revoked` honoured, `@cert-authority` not supported; unknown, changed, unlisted-type and revoked keys refused, never learned (§11.1) |
| Untrusted host reason | SSH host key refusals and TLS certificates libgit2 refuses (`GIT_ECERTIFICATE`: a self-signed or unknown CA, a host name the certificate does not cover) fail the push as `untrusted_host`, apart from `unreachable`, so a possible interception alarms differently from a network problem. A TLS handshake that fails before any certificate check (a server that drops it, as for a certificate host mismatch reached over an IP address in CI) stays `unreachable` (§11.3, §13) |
| Phase 1 pod layout | One `ReadWriteOncePod` PVC mounted once at `/data` (`backing/`, `state/`: reflinks fail across mounts), `Recreate` strategy; the FUSE mount at `/agentfs/workspace`, a subdirectory of a shared `emptyDir` (Bidirectional in the sidecar, HostToContainer in workloads), so a dead daemon does not block the restarted sidecar or the pod's teardown (§17.1) |
| Crashed daemon in a pod | Recovery is a pod restart, manual in phase 1. The restarted sidecar runs `mount --on-stale-mount hold`: no remount, no lock, NotReady pod, writes keep failing with `ENOTCONN`; on SIGTERM (after the workload containers stop) it detaches the stale mount. Automatic restart and fencing a clean exit are follow-ups (§17.1) |
| Sidecar probes | Startup `agentfs probe --allow-stale` (gates workloads; passes while holding), readiness `agentfs probe` (live only); no liveness probe, since a restart cannot remount safely; a shell-free subcommand reading mountinfo and `statfs` (§17.1) |
| Operator config in Kubernetes | Rules: the ConfigMap's `exclude` and `attributes` subPath-mounted read-only over `info/*` in the sidecar; settings and remotes: a git-config fragment in the same ConfigMap named by `include.path`, so `kubectl exec` commands and `git` see them. Includes must be absolute, exist, not set `agentfs.volume`/`agentfs.branch`, and stay outside the backing tree and mount point; `includeIf` is refused (§4.3) |
| `agentfs init` re-runs | Safe at every pod start: exit 0 for its own volume (taking the lock), 1 for another volume or branch with nothing changed; `--include` sets the includes declaratively; `--root` creates the backing tree (§2.3) |
| Image user | Root. The sidecar needs privileged for Bidirectional propagation, `CAP_SYS_ADMIN` for mount(2), `/dev/fuse` and passthrough, `CAP_CHOWN`/`CAP_FOWNER` for ownership, and to own the state area for libgit2. The init container drops every capability; workloads run as non-root (§17.1) |
| Image publishing | `linux/amd64`, published from `main` only after the kind e2e passes, pushing the tested image itself; tags: the full commit sha and `latest` (§17.3) |
| Kind e2e git server | The runner's nginx + `git http-backend`, reached from pods through a selector-less Service and an EndpointSlice on the kind network's gateway, so the credential Secret is exercised over HTTP basic auth and the bare repository stays on the runner for assertions; the daemon is killed from the node (PID 1 ignores SIGKILL from inside its namespace) |
| LFS store selection | Decided at record time on the frozen size and journaled with the write (older journals read as blob): always blob for empty files, special files, `.lfsconfig` and unnameable paths; then `filter=lfs` (operator, or tree only with `respectTreeRules`), which `agentfs.store=blob` cannot override; then `agentfs.store`; then `size > agentfs.lfsThreshold`, which is unset by default. The blob limit does not apply to LFS content (§7) |
| LFS `.gitattributes` | A block at the end of the root `.gitattributes`, in history only, rebuilt every commit: one anchored, escaped line per LFS path with `git lfs track`'s attributes; the workload's lines are kept and forged blocks replaced; a file holding only the block is agentfs's own (§7) |
| LFS endpoints and credentials | git-lfs's precedence (`lfs.pushurl`, `lfs.url`, `remote.<n>.lfspushurl`, `remote.<n>.lfsurl`, then derived `<url>.git/info/lfs`), from operator config only; local and `file://` remotes use the standalone-file layout; SSH needs an explicit `lfsurl`. Credentials: `agentfsLfsCredentials`, else `agentfsCredentials` on the push URL's own origin, else the URL's password; bound to the origin, sent to action hrefs only on that origin and without a server-given `Authorization`; no redirects, no proxies (§7) |
| LFS uploads | Before every push attempt, for the objects the commits the remote lacks reference at managed paths; a failure is a `PushFailed` with stage `lfs` and no ref pushed. Batch API, `basic` transfer only, sequential batches of 100, `verify` when offered (§7) |
| LFS cache and retention | `<state>/lfs/objects/aa/bb/<oid>`, written at commit time; nothing is pruned, locally or on servers (§7) |
| LFS restores | A pointer is checked out from the cache, else downloaded from the remotes in name order, every object before any write; tree content compared by SHA-256; `.gitattributes` without the managed block (§7, §15) |
| LFS client and TLS | agentfs's own client over `ureq` 3, pure Rust by default; the `https` feature adds `ureq`'s `native-tls` (OpenSSL, system trust store, as libgit2). An explicit https endpoint without the feature is a misconfigured remote; a derived one fails at push time as `local` (§7, §17.3) |
| LFS test server | CI uses git-lfs's `lfs-test-server` (pinned by commit, built with `go install`) behind the git server's nginx, which authenticates with separate users per host; rudolfs (no auth or verify) and giftless (JWT setup) were not used (§18) |

### 19.2 Open questions

1. **How iac produces patches** for the queue (§12).
2. **Performance budget** for the synchronous step, set after the phase 1
   benchmark (§18 test 13).
3. **Multi-pod (§14):** the network round trip per boundary, passthrough over a
   shared filesystem, RBAC and Pod Security.
4. **Recovery tooling** beyond `restore`, `revert` and the runbooks (§15):
   whether a read-only history view for operators is worth building,
   whether undo through the patch queue (§12) replaces manual restores for
   iac, and asking a running daemon to push now.

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
