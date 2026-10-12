# agentfs design

Status: draft, 2026-10-12. Nothing here is implemented.

## 1. Objective

agentfs presents the latest commit of a git branch as an ordinary directory.
Any process can use it, root or non-root, with no knowledge of git.

- **Recorded before return.** Every completed change made through the mount
  (create, write-and-close, rename, delete, mode change) is durably recorded
  before the call that completes it returns.
- **Committed and pushed.** Recorded changes become commits on the branch, and
  a recorder outside the workload's view pushes them to a remote in the
  background.
- **Large files.** Large files go to Git LFS, whose store may differ from the
  repository's remote.
- **Rules use git's idioms**: `.gitignore`, `.gitattributes`, `.lfsconfig`,
  `$GIT_DIR/info/*` and git-config syntax.
- **History is never visible in the mount.**

The core idea is a **projection**. The directory is not a copy that gets backed
up; it is a view of the history's latest state plus changes in progress.

### 1.1 Threat model

agentfs protects against an agent's **mistakes**, not against a malicious
agent. It exists to:

- show every change an agent made, so mistakes can be found and undone;
- sync that history to GitHub;
- let infrastructure-as-code (iac) change files with versioned patches instead
  of overwriting whole files.

So "invisible" is enough: no `.git` in the mount and no credentials in the
workload. agentfs does not try to resist a workload that is actively trying to
defeat it, or a compromised node.

### 1.2 Baseline and extras

| Baseline (must have) | Extras (later) |
|---|---|
| Every completed change recorded | Attribution beyond container and UID, such as agent, session or tool call |
| History pushed to a remote | Reconciling iac patches (§9) |
| Path rules in git idioms | Deeper handling of nested repositories (§7) |

### 1.3 Not goals

- **Enforcing content rules.** agentfs records; it does not reject writes.
- **Several pods writing one volume at once.** A volume has one mount at a time.
- **Pushing remote changes into a running workload.** Remote changes are only
  taken in at mount time.
- **Showing history to the workload.** The agent cannot read its own history
  through the mount.

## 2. Concepts

### 2.1 Two storage areas

Every path belongs to exactly one area, decided by the ignore rules (§4):

| Area | Holds | Versioned | Pushed |
|---|---|---|---|
| **Versioned** | tracked paths | yes, as git objects | yes |
| **Pass-through** | ignored paths, nested repositories (§7), and files git cannot store (special files, empty directories) | no; plain files on the state volume | no |

Both areas survive restarts because both live on the state volume (§10).

### 2.2 Record, then commit

"Recorded before return" and "committed" are separate steps:

1. **Record (synchronous).** When a change completes, agentfs writes the new
   content into the git object store and appends an entry (path, object id,
   mode, trigger, uid, pid, time) to a local **journal**. It `fsync`s both, then
   lets the call return. This is the guarantee.
2. **Commit (asynchronous, milliseconds later).** A commit builder groups journal
   entries into commits, updates the tree, and moves the branch ref. Grouping is
   cosmetic: it decides how history reads, not whether a change is kept.

After a crash, journal entries not yet in a commit are replayed into commits at
the next start.

### 2.3 What the mount shows

The mount shows:

- the branch's latest committed tree,
- plus journal entries not yet committed,
- plus files currently open for writing.

A file being written lives in a staging file on the state volume until it is
closed. Other processes reading it see the staging file, which matches normal
POSIX visibility.

### 2.4 What git cannot represent

agentfs inherits git's data model:

- Only the executable bit of the mode is recorded. Ownership, timestamps and
  extended attributes are not.
- Empty directories are not recorded; they live in the pass-through area.
- Symlinks are recorded as symlinks.
- Hard links are recorded as independent files.
- Special files (FIFOs, sockets, devices) live in the pass-through area.
- Paths git refuses, such as any `.git` component, are handled by rules (§7),
  never silently dropped.

## 3. Change triggers

| Event | Recorded when | Notes |
|---|---|---|
| File opened for writing, then closed | at `close` | Includes `O_TRUNC` and appends |
| `rename` | at `rename` | Records source and destination together, so a temp-file save is one change |
| `unlink`, `rmdir` | at the call | |
| `chmod` changing the executable bit | at the call | Other mode bits are not recorded |
| `symlink` | at the call | |
| `fsync` on a path marked `agentfs.commit=fsync` | at `fsync`, coalesced | See §5 |

A file held open and modified without `fsync` is recorded when it is closed.

## 4. Path rules

agentfs reads git's own rule files. Two places hold them:

- **Tracked** files are part of the content. They are visible in the mount, the
  agent may edit them, and edits to them are themselves recorded.
- **Operator** files live in the git directory, outside the mount. iac supplies
  them and the agent cannot see them.

| Concern | Tracked | Operator |
|---|---|---|
| Ignore (pass-through area) | `.gitignore` in any directory | `$GIT_DIR/info/exclude` |
| Per-path attributes | `.gitattributes` in any directory | `$GIT_DIR/info/attributes` (takes precedence over tracked attributes) |
| LFS location | `.lfsconfig` | `lfs.url` in `$GIT_DIR/config` (overrides `.lfsconfig`) |
| Daemon settings | none | `[agentfs]` sections in `$GIT_DIR/config` |

### 4.1 agentfs attributes

Custom attributes use the `agentfs.` prefix:

| Attribute | Values | Default | Meaning |
|---|---|---|---|
| `agentfs.commit` | `close`, `fsync` | `close` | Change trigger (§3, §5) |
| `agentfs.coalesce` | duration | `30s` | Minimum interval between `fsync` records for one path or group |
| `agentfs.group` | name | unset | Paths in the same group, in the same directory, are recorded together |
| `agentfs.store` | `blob`, `lfs` | see §6 | Force ordinary git object or LFS |
| `-agentfs` | | | Never record this path; it goes to the pass-through area. Equivalent to ignoring it |

Example operator `info/attributes` for a directory holding SQLite databases:

```gitattributes
*.db      agentfs.commit=fsync agentfs.coalesce=30s agentfs.group=sqlite agentfs.store=lfs
*.db-wal  agentfs.group=sqlite
*.db-shm  -agentfs
```

### 4.2 Daemon settings

```ini
[agentfs]
    # Content larger than this goes to LFS unless agentfs.store says otherwise.
    lfsThreshold = 8m
    # When false, only operator ignore rules count; tracked .gitignore files are
    # recorded as content but do not decide what is recorded.
    respectTreeIgnores = true
    # How nested repositories are handled (§7).
    nested = exclude
```

### 4.3 Tracked ignores and safety

In git, a tracked `.gitignore` can override `info/exclude` (a `!pattern`
re-includes a path). So an agent that writes `*` into `.gitignore` stops
recording everything after that edit. The edit itself is recorded, so it is
visible, but later changes are not.

`respectTreeIgnores = false` makes only operator rules count. The default is
`true` because it is git's behaviour; agent volumes should set it to `false`.

## 5. Files changed in place (`fsync` class)

Some files are held open and changed in place: SQLite databases, other embedded
databases, long-running logs. Recording them only at `close` could mean never.
Paths marked `agentfs.commit=fsync` are recorded on `fsync` instead:

- **Coalescing.** At most one record per path or group per `agentfs.coalesce`
  interval. The latest content is always recorded at `close`, and when the mount
  is unmounted.
- **Groups.** Paths sharing `agentfs.group` in one directory are captured
  together, from the same instant. For SQLite in WAL mode, the database and its
  `-wal` file must be captured together. Restoring both lets SQLite recover
  normally. The `-shm` file is regenerated by SQLite and must not be recorded.
- **Guarantee.** For these paths, "recorded before return" weakens to "recorded
  at most one coalescing interval late". This is deliberate.
- **Memory-mapped shared writes** (the SQLite `-shm` pattern) are not supported on
  recorded paths; such files belong in the pass-through area.

### 5.1 Storage growth

Snapshots of a large, frequently changed file grow storage quickly. A 50 MB
database recorded every 30 seconds is up to about 144 GB a day in LFS, which
stores whole files. Two mitigations, both open (§13):

- **Store as ordinary git objects** (`agentfs.store=blob`). Git's pack
  compression stores the differences between versions, which suits database
  pages well. GitHub rejects individual files over 100 MB.
- **Retention.** Prune LFS content for old versions of `fsync`-class paths while
  keeping their pointers. History still shows that a version existed, and its
  hash, but the content may be gone.

## 6. Large files and LFS

Whether content becomes an ordinary git object or an LFS pointer:

1. `agentfs.store` (§4.1), if set;
2. otherwise `filter=lfs` from `.gitattributes`, as in standard git LFS;
3. otherwise `lfsThreshold`: larger content goes to LFS.

**LFS location** follows git-lfs's own rules: `lfs.url` in `$GIT_DIR/config`,
then `.lfsconfig`, then the repository remote. A typical setup keeps history on
GitHub and LFS content on a self-hosted LFS server backed by in-cluster object
storage.

**Implementation.** Neither libgit2 nor gitoxide implements LFS, so agentfs
implements:

- the pointer-file format;
- the LFS HTTP batch API (upload and download);
- a local LFS object cache on the state volume.

LFS objects are uploaded before the commits that reference them are pushed.

## 7. Nested git repositories

Agents clone repositories into their workspace. A clone has its own `.git`
directory (or a `.git` file, for submodules and worktrees), its own objects and
its own nested `.gitignore` files.

**Constraint.** A git tree cannot contain a path with a `.git` component. Git
and libgit2 both refuse it as a security protection. Recording a nested `.git`
would also flood history: the index is rewritten on every `git status`.

**Default: nested repositories are excluded.** Any directory containing a
`.git` entry, at any depth, is treated as ignored. The whole directory, working
files and `.git` alike, lives in the pass-through area. It works normally and
survives restarts, but is not versioned or pushed.

**Option: `nested = working-tree`.** Configurable globally or per path. The
nested repository's working files are recorded like any other file, while its
`.git` stays in the pass-through area. Its own `.gitignore` files apply to what
is below them, as in git. Inner `.git/index.lock` gives natural grouping: while
it exists, changes in that repository become one commit, so a `git checkout`
of hundreds of files reads as one change.

**Later options:**

- Commit trailers recording the inner repository's state
  (`Agentfs-Nested: path=src/foo head=<sha> dirty=true`).
- Fetching the inner repository's objects into agentfs's object store under
  `refs/agentfs/nested/<path>/*`, so even unpushed inner commits are kept.

**Edge case.** When `git init` runs in a directory whose files are already
recorded, the directory becomes nested. agentfs records one commit that removes
those files from the versioned area, with a trailer saying why
(`Agentfs-Reason: nested-repository`), so it does not read as the agent
deleting them.

## 8. History and the remote

- **Single writer.** agentfs is the only writer of its branch. On GitHub, branch
  protection allows only agentfs's credential to push it.
- **Straight-line history.** Every commit on the branch is made by agentfs.
- **Fast-forward pushes only.** If the remote branch has moved anyway, agentfs
  pushes its commits to `refs/agentfs/diverged/<timestamp>` and raises an alert.
  It never rewrites or force-pushes history.
- **Push queue.** Pushes happen in the background, retrying on failure.
  Unpushed commits stay on the state volume (§10).
- **Commit metadata.** Author is the container and UID that made the change.
  Committer is agentfs. Trailers carry the trigger and caller:

  ```
  Agentfs-Trigger: close
  Agentfs-Pid: 4711
  ```

## 9. Reconciling iac patches (extra)

The core only records. Taking in changes from outside is an extra that runs
only at mount time, before the workload starts.

**The patch queue.**

- iac adds patches to the queue as commits on numbered refs:
  `refs/agentfs/inbox/<sequence>`.
- At mount, agentfs applies them **in sequence order**. Each is cherry-picked
  onto the current branch head with a three-way merge.
- **Success.** The result becomes a commit on the branch, authored by the
  patch's author and committed by agentfs, with a trailer
  `Agentfs-Inbox: <sequence>`. The inbox ref moves to
  `refs/agentfs/applied/<sequence>`.
- **First failure.** Processing **stops**. The failing patch and every later one
  stay in the queue, in order. agentfs records the failure, emits an event, and
  mounts the volume anyway. Recording is never blocked by a reconciliation
  problem.
- **Next mount.** Processing resumes at the head of the queue. A person or iac
  fixes or withdraws the failing patch.

Because patches become ordinary commits made by agentfs, the branch keeps a
single writer and a straight line.

**Per-path merge drivers** are a possible refinement. `.gitattributes`
`merge=<driver>` selects a driver for a path, and libgit2 lets agentfs register
custom drivers. For example, a YAML-aware driver could let iac own certain keys.

**Open:** how iac produces patches and pushes them to the inbox. The baseline
above does not depend on it.

## 10. Layers

### 10.1 Layer 1: the FUSE layer (Rust)

- **`agentfs-core`, a library crate:**
  - the rules engine (ignores and attributes, via libgit2);
  - the journal and commit builder;
  - the projection;
  - LFS;
  - the push queue;
  - the patch queue.
- **`agentfs`, a binary.** It mounts with `fuser` and works anywhere FUSE does:
  a laptop, Docker, CI. Mount options include `allow_other` or `allow_root`,
  and the owner (`uid`, `gid`, `umask`) reported for files.

Candidate commands:

```
agentfs init   --state <dir> --remote <url> --branch <name>
agentfs mount  --state <dir> <mountpoint> [--allow-other] [--uid N --gid N]
agentfs status --state <dir>
agentfs flush  --state <dir>      # commit and push everything pending
```

State directory layout:

```
<state>/
  git/          bare repository: objects, refs, config, info/{exclude,attributes}
  journal/      the durable change log (§2.2)
  staging/      files currently open for writing (§2.3)
  passthrough/  the pass-through area (§2.1)
  lfs/          local LFS object cache
```

### 10.2 Layer 2: the CSI driver

A Kubernetes CSI node plugin. Volumes are declared inline in the pod spec
(ephemeral inline CSI volumes), so no controller is needed at first. Volume
settings name the remote, branch, owner and operator rules (or a ConfigMap
holding them). The push credential comes from a Secret referenced by the volume
(`nodePublishSecretRef`) and never enters the workload.

**Mount pods.** For each volume, the driver starts a small mount pod that runs
Layer 1. This follows JuiceFS's CSI driver and solves two problems:

1. **Lifetime.** FUSE daemons running inside the driver's own container would
   die when it restarts, leaving workloads with broken mounts.
2. **Durability.** The mount pod holds a small PVC for its state directory, so
   journal entries and unpushed commits follow the volume if the workload moves
   to another node.

Lifecycle:

- **On mount (`NodePublishVolume`):**
  - ensure the mount pod and its state PVC exist;
  - clone, or fetch and process the patch queue (§9);
  - write the operator rule files;
  - start `agentfs mount`;
  - bind-mount the result into the pod.
- **On unmount (`NodeUnpublishVolume`):**
  - flush;
  - push one final time;
  - unmount.

## 11. Attribution

FUSE reports the caller's UID, GID and PID with every request. agentfs maps
these to commit author and trailers (§8). Attributing a change to an agent,
session or tool call needs the workload's cooperation, for example a variable or
side channel the workload sets. That is an extra.

## 12. Acceptance tests

1. **No gaps.** Each of these produces the expected recorded state:
   - an editor's temp-file-and-rename save;
   - an append;
   - a truncate;
   - `mv` of a directory;
   - `rm -r`;
   - an executable-bit change;
   - a symlink.
2. **Recorded before return.** Killing the daemon immediately after `close`
   returns, then remounting, shows the change in history.
3. **Any user.** Root and UID 10000 containers in one pod both read and write the
   mount.
4. **Invisible.** No `.git`, journal, state or credential is reachable from the
   mount.
5. **Rules.** `.gitignore`, `info/exclude` and `.gitattributes` decide the area
   and store exactly as git would. `respectTreeIgnores = false` ignores tracked
   `.gitignore` files.
6. **SQLite.** A WAL-mode database under steady writes is recorded at most once
   per coalescing interval. Each recorded version, restored with its `-wal`,
   opens cleanly and passes `PRAGMA integrity_check`.
7. **LFS.** Content over the threshold is stored as an LFS pointer, uploaded to
   the configured LFS location before the referencing commit is pushed, and
   restored on a fresh mount.
8. **Nested repository.** `git clone` into the mount works. By default nothing
   under the clone is recorded, and the clone survives a remount.
9. **Remote.** History reaches the remote. A remote branch moved by someone else
   leads to a push to `refs/agentfs/diverged/*`, not a force-push.
10. **Patch queue.** Three queued patches where the second conflicts: the first
    applies, the second and third stay queued in order, and the volume mounts.

## 13. Open questions

1. **Libraries.** libgit2 through `git2-rs` (mature push, credentials, filters
   and merge drivers) or gitoxide (`gix`, pure Rust; push support needs
   checking). The working assumption is `git2-rs`.
2. **CSI layer language.** Rust (one toolchain, gRPC via `tonic`) or Go (the
   Kubernetes CSI ecosystem's usual language).
3. **Retention** for `fsync`-class content (§5.1): pruning LFS content keeps
   history honest but loses data. Does storing these as ordinary git objects
   avoid the need?
4. **Commit grouping** policy beyond the nested-repository case. One commit per
   change, or group changes that complete within a short window?
5. **How iac produces patches** for the queue (§9).
6. **Locking.** Is one mount per volume enforced by a lease on the state PVC, by
   the CSI driver, or both?
7. **Performance budget.** An acceptable added delay per `close` from the
   synchronous record step (object write and two `fsync`s).

## 14. Prior art

| Project | What it is | Why not use it |
|---|---|---|
| gitfs (Presslabs; community fork `vtemian/gitfs`) | Python FUSE filesystem over git that commits after an idle period | Commits after 5 s idle, not before return; fetches and merges while running; exposes history in the mount; no LFS; 10 MB file cap by default. Last commit 2021 (original) and 2025-08-11 (fork). A design reference |
| Cloudflare ArtifactFS | FUSE client for Cloudflare Artifacts: a lazily loaded git working copy with a writable layer | Commits only when `git commit` runs; single-user mount (`allow_root` impossible through jacobsa/fuse); built for large repositories |
| Mesa | Hosted versioned filesystem for agents, FUSE-mounted | Private beta, hosted; no self-hosting, git sync or LFS described |
| Coregit, Freestyle | Hosted git APIs for agents | REST APIs, not mounts |
| lakeFS | Open-source branches and commits over object storage | Explicit commits, not git |
