# Adversarial review of design draft 1 (2026-10-12)

Four reviewers each read [design.md](../design.md) draft 1 (commit `b1b2d61`)
through one lens, checking claims against upstream sources where they could.
This record lists their findings and what draft 2 did with each. Reviewer
claims marked as inference in their reports are marked *(inference)* here.

The owner's decisions after the review:

- drop the projection as the storage model;
- the CSI driver must support several pods mounting one volume, each with its
  own committer identity plus pod metadata;
- a rejected push raises an alarm and the filesystem keeps working;
- GitHub is not the only remote;
- pushes run at intervals, not at pod shutdown;
- recovery is to be discovered and may be manual, but must be supported;
- use OpenTelemetry.

## Cross-cutting conclusion

Three of four reviewers independently found that building the directory from
git objects, journal, staging files and a separate pass-through area broke
ordinary file behaviour. That covered unlinked or renamed open files, hard
links, mmap, copy on every write-open, area moves on rule edits, and libgit2
rule matching. Draft 2 replaces it with a real backing tree and FUSE
passthrough, and records by interception.

## Filesystem, FUSE and durability

| # | Finding | Severity | Draft 2 |
|---|---|---|---|
| 1 | Path-keyed staging and the two physical areas break POSIX (unlink/rename of open files, hard links, readers, O(size) reads and copy-up) | blocker | Fixed: real backing tree, recording per inode (§2.1) |
| 2 | Only FLUSH is synchronous. It fires on every `close` of every duplicated fd. RELEASE is async. mmap writes arrive late | blocker | Fixed: dirty-FLUSH trigger; RELEASE and sweep as stated late backstops; truncate, link, symlink added (§3.1) |
| 3 | No crash protocol; libgit2 does not fsync by default; journal replay not idempotent | blocker | Fixed: checksummed absolute-state journal, `Agentfs-Seq` in commits, libgit2 fsync plus directory fsyncs (§3.3) |
| 4 | Ignore-decides-area contradicts git; renames across areas are O(size) and non-atomic; `git init` at the root | major | Fixed: git's tracked/untracked model; root never nested (§4, §5) |
| 5 | Synchronous hashing and fsyncs make `close` slow; no threading plan | major | Fixed: O(1) freeze plus group commit; hashing deferred; multi-threaded fuser (§3.2, §17). Benchmarks in test 13 |
| 6 | Permissions, mtimes and ownership lost or misreported | major | Fixed by the backing tree (§6) |
| 7 | SQLite assumptions false (`synchronous=NORMAL`, write freeze, rollback journal, group keys, storage) | major | Fixed: per-stem groups, trailing timer, write hold, no passthrough or writeback for the class (§8). Moved to phase 2 |
| 8 | readdir and inode stability | minor | Resolved by the backing tree |
| 9 | "A temp-file save is one change" is false | minor | Removed; grouping is cosmetic (§3.4) |
| 10 | Sparse files | minor | The backing tree keeps holes; recorded objects do not. Open under storage limits |

## Git, libgit2, LFS and remotes

| # | Finding | Severity | Draft 2 |
|---|---|---|---|
| 1 | Ignores never affect tracked files in git | blocker | Fixed (§4) |
| 2 | LFS pointers without a tracked `filter=lfs` show as pointer text in ordinary clones | blocker | Fixed: agentfs maintains tracked LFS attributes; `blob` may not override `filter=lfs` (§7) |
| 3 | libgit2 cannot evaluate rules on a bare repo plus in-memory projection | blocker | Resolved: rules evaluated against the real backing tree; gix parsers as fallback (§4.4) |
| 4 | A commit a remote refuses (fsck, size) jams the push queue forever | blocker | Fixed: pre-commit fsck and per-remote limit checks; a refused push raises an alarm without affecting the filesystem (§3.4, §11.3) |
| 5 | Tracked `.gitattributes` can turn recording off or change guarantees | major | Fixed: `agentfs.*` operator-only; `respectTreeRules` default false (§4.1) |
| 6 | `.lfsconfig` can redirect uploads with agentfs's credentials; other git-lfs keys ignored | major | Fixed: operator-only endpoints, host-bound credentials, git-lfs keys honoured (§7) |
| 7 | LFS pruning is impossible on GitHub; libgit2 makes no thin packs | major | Fixed: no pruning, size caps, periodic `git repack` (§7, §8) |
| 8 | Patch queue: multi-commit patches, merge base, non-atomic ref moves | major | Fixed: single-commit patches, parent as base, idempotency by trailer, status ref (§12) |
| 9 | git2-rs lacks filter and merge-driver registration; gix lacks push | major | Corrected (§17.3) |
| 10 | GitHub cannot enforce a single writer for personal repos or custom refs; divergence undefined | major | Fixed: detection, not enforcement; divergence procedure; colon-free ref names (§11.3, §11.4) |
| 11 | Nested detection misses bare repos and other cases | major | Fixed (§5) |
| 12 | Attribution: PIDs in the mount's namespace; no container identity | major | Fixed: committer is the replica, metadata in trailers, PIDs documented as namespace-local (§9) |
| 13 | Read cost of blobs; `text`/`eol` filters not applied; mode and mtime loss | minor | Resolved by the backing tree. Git content filters are not applied, since history stores bytes as written |

## Kubernetes and CSI

| # | Finding | Severity | Draft 2 |
|---|---|---|---|
| 1 | Ephemeral inline volumes have no lasting identity, so state cannot follow the workload | blocker | Fixed: persistent volumes for CSI (§14); RWOP PVC for phase 1 (§17.1) |
| 2 | Network work and patch processing inside NodePublish (2-minute kubelet deadline, retries) | blocker | Fixed: no network on mount or unmount, patches in the history service (§14) |
| 3 | Final push in NodeUnpublish hangs termination | blocker | Fixed: interval pushes only (§11.2) |
| 4 | RWO volume moves, scheduling, chained waits | major | Phase 1 avoids it (sidecar). Phase 4 open items (§14) |
| 5 | Single writer not enforced across pods | major | Superseded: several pods are now supported through one history service per volume (§14) |
| 6 | No recovery from daemon crash or upgrade (ENOTCONN) | major | Phase 1: pod restart is the recovery. Phase 4: must be specified and tested (§14, §17.1) |
| 7 | Credentials read only at publish; tokens expire | major | Fixed: credential files re-read on change (§11.1) |
| 8 | Pod authors control safety settings; RBAC breadth | major | Fixed: operator-owned parameters; RBAC inventory open (§14) |
| 9 | The Hermes docker-in-docker path is not designed | major | Described as a consumer requirement, including the `docker_volumes` policy decision (§16) |
| 10 | Talos and Pod Security specifics | minor | Open for phase 4 |
| 11 | State-area sizing and full-disk behaviour | minor | Recording-failure behaviour defined (§3.5); state size exported (§13) |
| 12 | No metrics or alarm mechanism | minor | Fixed: OpenTelemetry (§13) |

Phase 4 note (the CSI driver, design §14.2, [csi.md](../csi.md)), on the
findings left open above:

- **4.** One node per volume, enforced through attach: `attachRequired:
  true` makes Kubernetes refuse a second node (Multi-Attach) while the
  first holds the VolumeAttachment, ControllerPublish refuses it too, and
  `ReadWriteMany` is refused. A volume moves with its pods once they are
  gone. Several nodes per volume stay open (a cross-node transport).
- **6.** Recovery is specified and tested in the driver: a crashed front is
  restarted in its slot and its mount bind-stacked over the stale target,
  a crashed service restarted with journal recovery, and a restarted node
  plugin reconciles from its node records; the e2e-csi job kills each.
  An upgrade still interrupts the node's mounts for seconds (`OnDelete`).
- **8.** The RBAC inventory is done: the sidecars' minimal roles, no
  Secrets, pods or snapshots, and no API access for the node plugin.
- **10.** Pod Security is done: only the driver's namespace is
  `privileged`; workloads run under `restricted`. Talos and other
  hardened node images are untested.

## Objective and scope

| # | Finding | Severity | Draft 2 |
|---|---|---|---|
| 1 | The design does not reach Hermes' sandboxed tool writes | blocker | §16 |
| 2 | Secrets would be pushed permanently | blocker | Owner declined secret scanning. Consumers exclude secret files with operator rules; recorded secrets need the operator rewrite procedure (§10, §15) |
| 3 | No undo story | major | Supported manual path, plus discovery (§15) |
| 4 | Tracked rule files can turn recording off | major | Fixed (§4.1) |
| 5 | The projection is the main hidden complexity | major | Dropped |
| 6 | Value of "before return" versus an event-driven recorder | major | Kept, with a precise definition of a change boundary (§1.2) |
| 7 | Attribution mostly fiction | major | Reframed (§9) |
| 8 | CSI not needed for v1 | major | Phase 1 is a sidecar; CSI is phase 4 |
| 9 | SQLite class is scope creep for the first consumer | major | Moved to phase 2 |
| 10 | Bare repos; README overclaims | major | Detection fixed (§5). The README states nested repositories are excluded by default |
| 11 | Divergence and mount-time reconciliation undefined | major | §11.3, §12 |
| 12 | iac changes need a pod restart | minor | Stated (§16) |
| 13 | No performance budget | minor | Benchmarks first (§18), then a budget (§19) |
| 14 | Operations gaps | minor | §13 |

The reviewer's size estimate *(inference)*: the full draft-1 design was 12–20k
lines of Rust, several months for one person. A minimum slice proving the core
property was 3–5k lines, roughly phase 1 here.

Alternatives the reviewers weighed:

- an event-driven recorder (inotify or fanotify) about 100 ms late: meets
  everything except recording short-lived intermediate states;
- NFS, lakeFS or S3 mounts, and btrfs or ZFS snapshots: rejected.
