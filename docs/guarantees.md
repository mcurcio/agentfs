# Guarantees and limits

agentfs records every change a workload makes to a directory, in git history
the workload cannot see. This page says what that promises, what it does not,
and which test backs each claim. A claim with no test named says so.

Other pages hold the detail: [recording.md](recording.md) (what is recorded and
when), [remotes.md](remotes.md) (pushing), [trees.md](trees.md) (the tree
views), [operator-guide.md](operator-guide.md) (running it).

## What agentfs does

- **Ordinary files.** Any process, root or not, uses the directory with
  ordinary file calls and never sees git. The files live in a real directory on
  a backing volume.
- **Recording.** agentfs sits in front of that directory as a FUSE filesystem.
  When a file is closed after a change, or renamed, deleted, linked or marked
  executable, agentfs durably records the new state before the call returns. It
  turns recorded states into commits a moment later.
- **Pushing.** It pushes the commits at regular intervals to any number of git
  remotes, or to none. GitHub is one possible remote, not an assumption.
- **Rules.** Path rules use git's own files and syntax: `.gitignore`,
  `.gitattributes`, `.lfsconfig`, `$GIT_DIR/info/*` and git config.

## Threat model

agentfs protects against an agent's **mistakes**, not a malicious agent. It
exists to:

- show every change an agent made, so mistakes can be found and recovered from;
- sync that history to remotes;
- let infrastructure as code (iac) change files with versioned patches instead
  of overwriting whole files.

"Invisible" means no history, journal, state or credential is reachable through
the mount, and nothing in the tree can turn recording off. agentfs does not try
to resist a workload that is actively trying to defeat it.

Two places treat the workload as less trusted: [forks](#forks) and the
[sandbox overlay](#the-sandbox-overlay).

## What "every change" means

A change is recorded at each **change boundary**, before the call that ends it
returns. [recording.md](recording.md#change-boundaries) lists them. Between
boundaries, such as the bytes of a file between two writes, nothing is recorded
separately. A few boundaries can only be recorded after the fact (a write
through a shared memory mapping, a file never closed); they are recorded by a
backstop that runs a short time later.

Empty directories are not recorded, because git cannot represent them.
Ownership, permission bits other than the executable bit, timestamps and
extended attributes are not recorded either.

## Recording comes first

- **Before the call returns.** A change at a boundary is in the journal, with
  its content frozen, and the journal is `fsync`ed, before the call that made it
  returns. Killing the daemon right after `close` returns, then remounting,
  shows the change in history.
- **Or it fails loudly.** If recording fails (for example the state area is
  full), the call returns `EIO` and agentfs raises an alarm (`RecordFailed`).
  The inode stays marked as changed, or the path goes on a repair queue that
  survives restarts, so a later boundary or the next mount records it. Many
  programs ignore errors from `close`: the alarm is what people see.
- **Nothing is traded for speed.** The synchronous step does not depend on the
  size of the file (it uses a reflink where the filesystem has one). See
  [performance.md](performance.md) for costs.
- **Rule files cannot turn recording off.** Ignore rules decide only whether
  new paths are recorded. A path already tracked stays recorded until deleted.
  A tracked `.gitignore` or `.gitattributes` is content, not a rule, unless the
  operator sets `respectTreeRules = true`, and even then it cannot set `agentfs.*`
  attributes or LFS endpoints.

Tests: the engine killed or failed at every point a workload reaches
(`crates/agentfs-core/tests/crash_points.rs`), the processes
(`crash_daemon.rs`) and a FUSE mount (`crash_mount.rs`) are all checked by one
oracle: every promised record committed exactly once, first-parent
`Agentfs-Seq` strictly increasing, no empty commits, `git fsck` clean, nothing
left in `pending/`.

## Crash and power loss

- Journal entries hold absolute state, so replaying one twice gives the same
  result. A torn last entry (its call never returned) is discarded.
- A commit writes objects, then `objects/`, then the branch ref, then the
  directories above the ref, and only then truncates the journal and removes
  the frozen copies. A power cut at any point leaves either the old head with
  its records still journaled, or the new head with everything it names.
- Crash safety is tested by killing at every step of record, commit, ref
  update and push.
- Power loss is tested on ext4 and XFS: `dm-log-writes` records every write and
  flush of a workload, and the log is replayed to every FUA (or flush) point,
  where the same promises are checked (`ci/power-loss.sh`).

**Not tested:** writes reordered between two flushes (replay is in write order
only), and a power cut through a FUSE mount (the power-loss job drives the
`record` command, not a mount). The commit order does not rely on the order of
writes inside an epoch, but only review says so.

## Files changed in place

For paths in the `fsync` class (databases), "before return" weakens to "at
most one interval late": a change to a member is in a journaled record no
later than one `agentfs.coalesce` after it was made, or at the group's last
close or unmount, whichever comes first. An `fsync` returning does not mean its
data is recorded.

If the daemon or node dies, the changes made since a group's last capture are
in the files, which SQLite recovers as usual, but not in history. They are
recorded when the database next changes, or by `agentfs record <path>` on a
stopped workload. See [recording.md](recording.md#files-changed-in-place-the-fsync-class).

## History is the product

- **No automatic rewrite, no force-push.** A divergence (someone else wrote the
  volume's branch) is saved under `refs/agentfs/<volume>/diverged/<timestamp>`
  and alarmed, never overwritten. Pushes are fast-forward only. The one exception
  is an operator's explicit `agentfs push --force-with-lease`, and it refuses
  history an agentfs recorder wrote that the head lacks
  ([remotes.md](remotes.md#the-operators-lease-push)).
- **First-parent history is a line.** One recorder writes the branch, and
  every commit is made by agentfs. A checkpoint adds a marker commit with a
  second parent; the first-parent chain stays linear
  ([patches.md](patches.md)).
- **A rejected push never touches the filesystem.** A failed push, an
  unreachable remote or a divergence raises an alarm and is retried with
  backoff. The workload keeps running and recording continues. Only recording
  failures reach the workload.
- **History rewrites are manual.**
  [runbooks/history-rewrite.md](runbooks/history-rewrite.md).

## Recovery is safe by default

`agentfs restore` and `agentfs revert` check everything before they write. They
never overwrite a change that is not in history, even with `--force`; they
refuse paths agentfs would not record; and they refuse to write over untracked
files. `--force` overrides only refusals where nothing is lost. Each path is
re-checked just before it changes.

One window remains: a workload that opens and writes a file between that
re-check and the write, without closing it, has those writes replaced before
they are recorded. Nothing unrecorded is lost only while the workload is
paused. See [runbooks/recovery-and-undo.md](runbooks/recovery-and-undo.md).

## Secrets

agentfs does not scan content for secrets and does not hold pushes. Keep
secrets out with operator rules (`info/exclude`, `-agentfs` in
`info/attributes`). A secret that gets recorded anyway is permanent in history,
locally and on every remote it reaches. Removing it is a documented operator
procedure, run with the workload stopped:
[runbooks/history-rewrite.md](runbooks/history-rewrite.md).

## Several pods on one volume

- Pods share one directory with the backing filesystem's semantics. Two pods
  writing the same file is last-writer-wins, as without agentfs. History
  records each pod's change boundaries in the order the history service
  received them, and a commit never holds two pods' changes.
- POSIX and BSD locks stay in each mount's kernel and are **not shared**
  between pods. A SQLite or other lock-coordinated database must be written
  from one pod.
- A volume is used from one node at a time. Several nodes writing one volume
  are not supported: attach is not a fence on most storage backends, and a lock
  on shared storage cannot fence a paused or cut-off holder.

Accepted residuals of several pods on one volume:

- **Same-tick writes.** Two pods' same-size writes within one `ctime` tick are
  resynced, so `HEAD` ends on the tree's state; the commits in between may hold
  the other pod's state for a moment, and the resync commit carries the history
  service's committer, not the pod's that wrote last.
- **Callers' groups at pid 0.** A FUSE daemon in another pid namespace than its
  callers cannot read their supplementary groups. Creations that only a
  supplementary group (a pod's `fsGroup`) allows fall back to the daemon, a
  racing open from another pod can fail with `EACCES`, and `create`'s check of an
  existing file counts the primary group only. The shipped CSI manifest sets
  `hostPID: true`; the fallbacks are counted
  (`agentfs.fuse.create_as_caller_fallbacks`).
- **Clock steps.** A late record frozen before a backward step of the wall
  clock, and racing writes after it, are ordered by `ctime`, which the step
  makes arbitrary for up to its length.
- **Live restores.** A pod that opens a file for writing between the service's
  per-path question and the write is not seen. A restore stopped at shutdown
  leaves the one path it was writing, if that outlasts `--shutdown-grace`,
  written and unrecorded.
- **Attribution.** A pod's late record (a close, or a sweep while it holds the
  file open) of a path another pod is writing may capture that pod's
  intermediate state, such as an empty file between its `O_TRUNC` and its write,
  under the first pod's committer. The other pod's close then records the final
  state; only the intermediate commit's attribution is wrong.

## The sandbox overlay

The sandbox overlay is a second mount point of a volume that shows only
recordable names. It is **a boundary for names**, not a security sandbox.

- **What it covers.** Through the overlay a sandbox can name, create, replace
  or move only recordable files, plus its own tmp files. It cannot make a
  symlink, hard link or special file, move a directory, change ownership, or
  change a rule. Tests: `a8_refusals`,
  `a5_hidden_names_answer_the_same_whether_or_not_they_exist`,
  `recorded_files_never_move_out`, `overlay_refusals`,
  `the_overlay_is_refused_on_a_front_and_with_tree_rules`.
- **What it does not cover.**
  - A file a sandbox already holds open stays writable after the main
    container moves it out of view, and those writes go unrecorded. Not tested:
    `a6_a_node_moved_out_of_view_is_gone_from_the_overlay` checks only that
    later lookups and opens of the node fail.
  - A planted repository layout hides that directory, from the overlay and
    from recording (`judge_answers`, `a11_directories`).
  - Volume-wide facts leak: free space (`statfs` is passed on; not tested),
    link counts (not tested), and "directory not empty" for a directory whose
    remaining entries are hidden (`overlay_refusals`).
  - Inode numbers of shown entries skip one per hidden entry in a listing,
    so a sandbox can count a directory's hidden entries, never name them (not
    tested). Lookups, removes and renames take no id for an entry hidden for its
    kind (`a5_hidden_entries_take_no_node_ids`).
  - Existence leaks at names a sandbox may place. Where a name is hidden only
    for what is there, `mkdir`, `create` and `rename` onto it answer `EACCES`,
    while the same call at an absent name succeeds. A sandbox can confirm that
    something unrecorded exists at a name it guesses, never its content or kind
    (`a5_hidden_entries_at_placeable_names_are_refused_under_the_names_ordering`).
  - Locks are not shared between the two mounts: each kernel keeps its own
    POSIX and BSD locks (not tested).
  - Mode-bit permission checks run outside the names lock, so a `chmod` through
    the main mount between the check and the operation is not seen by that one
    operation (not tested). The name checks run under the lock.
- **Where it runs.** On single-process mounts (the sidecar, `agentfs mount`),
  not on a history-service front and not on CSI volumes.
- **Capability assumptions.** Sandboxes lack `CAP_SYS_ADMIN` and
  `CAP_DAC_READ_SEARCH`. Stopping a privileged container from reaching the disk
  by other means is not supported.

## Forks

What a sandbox writes into a [fork](forks.md) is treated as potentially
hostile in everything agentfs itself does with it: names, paths, sizes,
symlinks and gitlinks are checked, and nothing is run. agentfs limits nothing
that users' own CI may do with what a fork publishes.

Accepted, with no hardening: a sandbox's file calls are served by the
privileged node plugin, which holds every profile's credentials, so a bug in
that front would expose node root and those credentials. Volume size is not
enforced on the node's base path, so a sandbox can fill it for every volume on
that node.

## Not goals

- **Enforcing content rules.** agentfs records; it does not reject writes.
- **Detecting secrets.** See above.
- **Pushing remote changes into a running workload.** Remote changes enter only
  at mount time, with nothing attached. An apply while a workload is attached
  only judges. Landing a patch while a workload runs is not supported.
- **Showing history to the workload.**
- **Undo tooling beyond `restore`, `revert` and the runbooks.** There is no
  read-only history view for operators and no undo through the patch queue.

## Tree views

The [tree views](trees.md) have their own limits, which that page lists.
