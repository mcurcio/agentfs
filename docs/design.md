# agentfs design

Status: draft 2, 2026-10-12. Nothing here is implemented. Draft 1 was reviewed
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
  meta/      the daemon's own bookkeeping (sequence numbers, push state)
```

### 2.3 Single writer of history

Exactly one recorder process owns the history repository and its branch. In
phase 1 that is the sidecar's daemon. With several pods (§14) it is a separate
history service per volume, fed by each pod's front. Either way history is a
straight line, and every commit is made by agentfs.

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
  they are recorded.
- **Grouping only affects how history reads,** not whether a change is kept.
  Entries close together in time are grouped. Inside a nested repository with
  `nested = working-tree` (§5), its `index.lock` bounds a group, with a timeout
  for stale locks.
- **Commit metadata** is in §9.
- **Every commit is checked before it is made** against git's fsck rules and the
  configured limits of each remote (§11.3). Paths that would fail are not
  recorded. They stay in the backing tree, and an alarm is raised (§13).

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

### 4.2 agentfs attributes (operator `info/attributes`)

| Attribute | Values | Default | Meaning |
|---|---|---|---|
| `agentfs.commit` | `close`, `fsync` | `close` | Boundary class (§3.1, §8) |
| `agentfs.coalesce` | duration | `30s` | Minimum interval between `fsync` records for one group |
| `agentfs.group` | name | unset | Paths with the same group *and the same stem* are recorded together (§8) |
| `agentfs.store` | `blob`, `lfs` | §7 | Force ordinary git object or LFS |
| `-agentfs` | | | Never record this path |

### 4.3 Settings

```ini
[agentfs]
    respectTreeRules = false   # tracked .gitignore/.gitattributes are content only
    nested = exclude           # §5
    commitWindow = 200ms       # group journal entries this close in time
    pushInterval = 60s         # §11
    lfsThreshold = 8m          # phase 2, §7
```

### 4.4 Implementation note

libgit2 evaluates ignore and attribute rules against a working directory.
Because the backing tree is a real directory, agentfs can point libgit2 at it,
with system and global sources disabled. If that proves unworkable (for
example, recursion through the mount), the fallback is a rules engine over the
`gix-ignore` and `gix-attributes` parsers.

## 5. Nested repositories

**Detection.** A directory is a nested repository if it contains:

- a `.git` directory;
- a `.git` file that points to a valid git directory; or
- the layout of a bare repository (`HEAD`, `objects/` and `refs/`).

The last case covers `git clone --bare` and tool-managed stores such as Hermes'
`checkpoints/store`.

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
- **Paths git refuses** are not recorded, and each raises an alarm. This includes
  any `.git` component and its NTFS and HFS look-alikes, such as `.GIT`,
  `git~1` and `.git.`.

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
  Gitea or a bare repository on a server all work the same way.
- **Several push remotes** can be configured. Each has its own push state and
  alarms.
- **Credentials** come from files the recorder reads, and are re-read when they
  change, so short-lived tokens can be rotated by whatever writes the file.

### 11.2 Interval push

- Pushes run every `pushInterval`, independent of the workload's lifecycle.
  Nothing waits for a pod to stop, and stopping a pod never waits for a push.
- Unpushed commits stay safe in the state area and go out on the next
  successful push.
- Pushes are fast-forward only.

### 11.3 Limits and rejected pushes

- **Limits are configured per remote,** for example maximum blob size, maximum
  push size and whether fsck rules are enforced. Commits are checked against
  them before they are made (§3.4).
- **A rejected push never affects the filesystem.** Rejected means refused,
  non-fast-forward, authentication failure or an unreachable remote. Recording,
  committing and other remotes carry on. The recorder:
  - raises an alarm (§13) with the reason and the commit;
  - keeps retrying with backoff;
  - keeps the remote's last successful commit, so the backlog is visible.
- **Non-fast-forward means someone else wrote the branch.**
  - The recorder pushes its head to
    `refs/agentfs/<volume>/diverged/<UTC timestamp, no colons>` once per
    divergence, then keeps alarming.
  - It never force-pushes.
  - An operator resolves it (§15).

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

**Pushing.** A recorder pushes only its own volume's refs, with explicit
refspecs, so pushes from different volumes never contend.

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

**Metrics (examples):**

| Metric | Type | Use |
|---|---|---|
| `agentfs.record.duration` | histogram | Cost of the synchronous step, by trigger |
| `agentfs.record.failures` | counter | Recording errors (§3.5) |
| `agentfs.journal.backlog` | gauge | Entries recorded but not committed |
| `agentfs.commit.count` | counter | Commits made |
| `agentfs.push.unpushed_commits` | gauge, per remote | Backlog |
| `agentfs.push.oldest_unpushed_age` | gauge, per remote | Backlog age, the main push alarm |
| `agentfs.push.failures` | counter, per remote and reason | Rejected, non-fast-forward, authentication, network |
| `agentfs.push.diverged` | gauge, per remote | 1 while diverged |
| `agentfs.precheck.rejected_paths` | counter | Paths refused by fsck or limit checks |
| `agentfs.patch.queue_depth`, `agentfs.patch.blocked` | gauge | Phase 3 |
| `agentfs.state.bytes` | gauge | Size of the state area by part |

**Traces.** Spans for record, commit, push and patch application, carrying
journal sequence numbers and commit ids.

**Logs.** Structured events for every alarm condition, with commit, path and
reason.

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
  - pre-commit checks;
  - push (§11);
  - LFS (§7);
  - the patch queue (§12);
  - telemetry (§13).
- **`agentfs`, a binary:**
  - `init`, `mount`, `status`, `flush`, `restore`, `revert`;
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
| Ignore and attribute parsing (fallback) | `gix-ignore`, `gix-attributes` | gitoxide has no push, so it is not a full alternative |
| LFS | none exists | Implemented in agentfs (§7) |
| Telemetry | `opentelemetry` crates | OTLP export |
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
