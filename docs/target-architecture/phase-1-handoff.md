# Phase 1 handoff and exit evidence, 2026-11-09

**Verdict: Phase 1 is validated against §18 as written, with named limits (every exit row passes on CI, measurements recorded; four questions are open for the owner).** The build, its limits and the owner's open questions are in
[phase-1.md](phase-1.md). This page holds the exit table and the measurements. It replaces the
2026-11-06 checkpoint, which described a build on Git's reftable and failing exits; that build
is gone (the journal is the commit point, git-model §12) and its results are in the history of
this branch.

Work belongs to branch `docs/target-architecture` and [PR #23](https://github.com/mcurcio/agentfs/pull/23).
The owner merges. Nothing was pushed to main or force-pushed.

## Evidence used

| Name | What | Link |
|---|---|---|
| **P** | `phase-1` workflow on `6312204`, with `[phase1-measure]`; product code identical to `e9ab747` and to every later commit | [run 42563670392](https://github.com/mcurcio/agentfs/actions/runs/42563670392) |
| **S** | `phase-1` workflow on `e9ab747` with `[phase1-seeds]`: the library at seeds 1 to 500, four shards | [run 42555683429](https://github.com/mcurcio/agentfs/actions/runs/42555683429) |
| **C** | `ci` workflow on `6312204` | [run 42563672713](https://github.com/mcurcio/agentfs/actions/runs/42563672713) |
| **H** | `phase-1` workflow on `a0e24ca`, `[phase1-held]`: the held-log mode after its harness fix | [run 42570034409](https://github.com/mcurcio/agentfs/actions/runs/42570034409), held-log job only |

Every job ran git 2.47.3 (`ci/git-release`) on kernel 7.0.0-1017-azure.

## Exit table (git-model §18)

PASS means the job ran and every check in it passed. Limits are stated, not hidden.

| §18 exit | Status | Evidence and limit |
|---|---|---|
| Main and one fork: the model's scenarios through real calls | **PASS** | [Conformance job, P](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375677): the fixed mounted cases (loss, neighbors, inserts, unfinished tail, edited place, P1, P2, byte calls, findings 32 and 33) pass. The model's recorded calls replay on a fresh model (74 scenario and 94 seed traces), the library (74 and 94) and real mounts (71 and 16), none mismatched, none listed; 81 in-scope traces are not mountable. The library at seeds 1 to 500: 1,000 of 1,000 traces ([S](https://github.com/mcurcio/agentfs/actions/runs/42555683429), shards [0](https://github.com/mcurcio/agentfs/actions/runs/42555683429/job/128960726864), [1](https://github.com/mcurcio/agentfs/actions/runs/42555683429/job/128960727044), [2](https://github.com/mcurcio/agentfs/actions/runs/42555683429/job/128960726937), [3](https://github.com/mcurcio/agentfs/actions/runs/42555683429/job/128960727014)). Limits: no mapping call, no client call waiting on a take, crashes between calls only (VERIFYING.md "Phase 1"); seeds not rerun on later commits |
| Journal: kill after each step, exactly-once | **PASS** | [Journal and state exhaustion, P](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375608): kills at every journal, object, commit-point, ref-copy and checkout failpoint, and only an open at the ref rebuild, both compaction points and a first init. One process hosts the view and the actors; phase 1 has no separate history service |
| Journal: a returned `fsync` or close survives power loss | **PASS** | dm-log-writes replay in `flush` mode, each checkpoint also rebuilding the refs from the journal: [ext4](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375750), 810 checkpoints; [XFS](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375563), 501. Limit: replay cannot judge the object-directory syncs on these filesystems; the ext4 job's strace check shows only that each object's fsync precedes its journal record |
| Full state area: a write returns `ENOSPC` | **PASS** | Same job: write and rename before the commit point answer `ENOSPC` and change nothing; a rename past it answers 0 and stands; a failed journal `fsync` stops the actor with `EIO` |
| Close then open, same process and another | **PASS** | [Page-cache job, P](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375366): 64 closes reopened over a busy view, 0 wrong, in one process and in another. Limit: kernel 7.0 and the source of v6.8, v6.14 and v7.0; a readahead in flight on the handle was not exercised |
| Byte calls and save alignment | **PASS** | Conformance job, `byte_calls`: split writes, append, overlap, holes, growth and shrink match a plain Linux file; line maps pass an independent git histogram diff |
| Atomic replacement, `O_TMPFILE` and `linkat`, unlink and rename while open | **PASS** | [ci, "e2e (phase 1 tree views)", C](https://github.com/mcurcio/agentfs/actions/runs/42563672713/job/128985382505) |
| A view-host restart with handles open | **PASS** as §18 words it | [Page-cache job, P](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128985375366): the old handle's writes fail, the file holds what returned before the kill, saved once, and a file replaced through the new mount stays unchanged. A kernel handle cannot outlive its connection, so the same-handle reconnect form cannot occur |
| Two writers at stale offsets through the page cache; shared `mmap` | **PASS** | Same job: 21 of 21 cases and the strict mapping script. Limit: R1.4's accepted gap (phase-1.md) was found by reading, not run |
| Progress: unrelated stream, held log, new writers, cooperating programs, prefix sync | **PASS** for the finite scenarios | Conformance job, `progress_p1`, `progress_p2`, and the waits measured below. No starvation bound is claimed; overlapping writers can postpone a take indefinitely under the approved locks |
| Measurements, recorded, not gated | **RECORDED**, figures below | Below |

## Planted bugs

Each was planted on a branch or a scratch copy to show a check can fail.

| Bug | Caught by |
|---|---|
| Journal fsync per record removed (M1.15) | Yes, on ext4 and XFS, by the mark straight after a `rename` ([run](https://github.com/mcurcio/agentfs/actions/runs/42547830707)); the bare-write marks of that run passed, and why is unexplained |
| Object-directory syncs removed (M1.16) | Only by the system-call order check ([run](https://github.com/mcurcio/agentfs/actions/runs/42547834334)); prefix replay cannot judge it |
| `core.fsync` for objects dropped | Yes, both filesystems ([run](https://github.com/mcurcio/agentfs/actions/runs/42296893603)) |
| `FUSE_WRITE_CACHE` ignored, no lock-owner route, size only with page state, no invalidation after a take, `max_background` 16, object found by name at restart (M3.8 to M3.13) | Yes, each by a different page-cache case or reopen count: [M3.8](https://github.com/mcurcio/agentfs/actions/runs/42309475616) passes 13 of 21 cases, [M3.9](https://github.com/mcurcio/agentfs/actions/runs/42309478789) 5 of 21, [M3.10](https://github.com/mcurcio/agentfs/actions/runs/42309480548) fails `create-then-map` and `grown-mapping`, [M3.11](https://github.com/mcurcio/agentfs/actions/runs/42309484689) `take-refreshes-mapping`, [M3.12](https://github.com/mcurcio/agentfs/actions/runs/42312191546) reads old bytes in 4 and 1 of 64 reopens, [M3.13](https://github.com/mcurcio/agentfs/actions/runs/42309490679) the replaced file |
| R10.8 off on mounts (M4.1); M4.2 to M4.7 and three more on the library; M4.8 on the model | M4.1 on CI ([run](https://github.com/mcurcio/agentfs/actions/runs/42330173796)); the others only locally, in scratch copies, and not rerun on CI. Planted bugs trip `upstream_kept` and `full_lineage` (locally); `converged` fires only on a non-convergence the model shares, since the tree comparison before it catches any one-sided bug |

The branches are `wip/agentfs-eh5*`. They were not rerun on `6312204`: M1.15 and M1.16 ran on `6049b1b`,
whose durability paths match `e9ab747`.

## Measurements

Recorded, not gated: no figure is a budget (git-model §18). Source: run P, `6312204`
(`FOPEN_DIRECT_IO`, the runner's ext4, git 2.47.3, Linux 7.0 azure, debug build). One run of each,
and two runs of one mode have differed by up to 2 times, so read a figure as an observation.

| Measurement | Result | Job |
|---|---|---|
| Journal call cost against plain FUSE | 1,000 `pwrite` of 8 bytes at offset 0 on one open descriptor. Plain FUSE (`bindfs -o direct_io`): median **27.0 us**, p99 **53.1 us**. Recording mount: median **352.1 us**, p99 **607.0 us**. Wall time around the syscall, including FUSE and the journal's `fsync` | [calls](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128986022557) |
| One hour of journal growth | **501,785 bytes** over **3,600.000087 s**: 3,592 eight-byte `pwrite` calls (28,736 payload bytes), a pause of one second after each. Logical growth of `calls.wal`, open and close records excluded | [hour](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128986022568) |
| Line-map bytes per 10,000 saves | `metadata.json` **195,687 bytes** after 10,000 one-line saves (19.6 a save). The snapshots are **961,003,746** bytes logical and **241,338,078** as git objects (10,000 distinct blobs); the commits 3,615,564 logical and 2,251,085 on disk; the repository 245,688,644 bytes of files and 425,779,200 allocated; `refs/` 164. The journal is 15,044,086 bytes after 10,001 saves (1,504 a save). **2,560.9 s**, 390,047 git children (39 a save) | [storage](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128986022573) |
| Save and take, 1 MiB | Write and close **23.3 ms**; actor save 272.9 ms; landing 337.3 ms, of which the sync back is 5.2 ms; both views and an empty queue after **1.27 s** | calls |
| Save and take, 100 MiB | Write and close **31.9 s**; actor save 25.6 s; landing 288.2 s, of which the sync back is 0.14 s; both views and an empty queue after **339.1 s** | calls |
| Waits | One main log held open, five fork saves to other files, then a fork save of the log waiting. 1 wait, **740.9 s** in all (555.5 s before the log was released). The five independent saves were visible while it was held after **238.3, 238.5, 239.7, 240.9 and 241.2 s**; five more opens of the held log took a median **138.8 s**, p99 **139.0 s**; the held save was observed queued after 1,267 s, held for 694.6 s more; both views converged **98.7 s** after the release. The 100 MiB file is in the tree throughout (below) | calls |
| Held log and compaction | A log held open while another file takes 24 saves of 8 MiB: the journal peaks at **66,149,414 bytes**, drops to **745 bytes** at each of 2 compactions, and ends at 49,611,545. The held file's one line is all a compaction keeps. 24 saves took **115.5 s** | [held-log, H](https://github.com/mcurcio/agentfs/actions/runs/42570034409/job/129005062979) |
| Pilot, 1,000 saves | **95.7 s**; git children a save flat at 39.0 between saves 100 and 1,000 | [pilot](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128986022601) |

The first held-log run, in P, failed: it rewrote identical bytes on each save, and a save that leaves
a file's bytes as they were commits nothing (R8.2, finding 36), so it waited an hour for a second save
that never came ([job](https://github.com/mcurcio/agentfs/actions/runs/42563670392/job/128986022626)).
That was a harness defect, not a product one; a local library test with the same pattern showed the
second to fourth closes returning `Unchanged`. Each save now starts with its own line.

### What a 100 MiB file in the tree costs

One run, a debug build. Every save and take reads and rewrites the whole tree, so a large file taxes
every later step of every file, not its own.

- A save of the file itself: 31.9 s to write and close, 25.6 s for the actor's save, 339.1 s until
  both views converge. A 1 MiB file converges in 1.27 s, so 100 times the bytes cost 267 times the
  time.
- **Other files pay for it.** Five saves of unrelated small files took 238 to 241 s each to become
  visible while the large file was queued, and each new open of the held log took a median 138.8 s.
- Where the time goes, over the whole calls run (seconds, runs): `snapshot_read` 1,797.4 (97),
  `queue_plan` 1,280.6 (48, which encloses some of the stages beside it), `objects` 288.6 (19, one
  rehash and sync of the blob on each later save), `judge` 263.0 (14, one run 261.9 s), `checkout`
  24.7 (38). The sync back, 26.6 s in the run before the fast-forward fix, is now 0.14 s.
- The allowed fixes left are a memo of parsed snapshots keyed by commit, writing only the files that
  changed, and the line referee's own cost. None is made. Earlier local runs (Docker on macOS, debug)
  had `judge` grow faster than linearly with the file's lines: 28 s at 32 MiB, 109 s at 64 MiB, 316 s
  at 100 MiB.

### Why 10,000 saves took 2,561 s

Per stage totals over the 10,000 saves, seconds (runs): `queue_plan` 1,059.5 (10,001), `snapshot_read`
300.7 (20,003), `align` 281.8 (10,000), `objects` 458.7 (10,001), `checkout` 148.4 (20,003), `ref_copy` 160.1
(20,004), `journal_append` 191.1 (50,005, of which `fsync` 176.7). Git children a save are flat at 39,
because the owners of each queue entry are read once at open and the sync plan asks git once per commit,
not once per line (design §17.3). The stages grow with the line map, which is never pruned, and the per-save
cost rose from tens of milliseconds to hundreds. About a third of the run is inside `queue_plan` but in none
of its measured parts; the frozen `Stages` has no field for it.

## Continuation

Remaining work is the owner's four questions in [phase-1.md](phase-1.md), the module cut (planned after
phase 1), the pure-git walkthrough (planned, not built; git-model §18) and the exclusions listed there.
The takes-refresh case of `ci/trees-pagecache.py` still polls for the take; an event needs a failpoints
build and every take gated, which was judged too invasive for a P2. Suspicions neither reproduced nor
changed: the restart's close order after two failed closes, `Journal::sync` not marking the journal
unknown when a `FLUSH` fails, and queue-worker mutex starvation.
