# Performance

What recording costs, measured. agentfs makes the call that ends a change
(a `close` after a write, `unlink`, `rename`, ...) wait until the change
is durable (design §3.2): the content is frozen into `pending/` and a
journal record is appended and `fsync`ed. Hashing, compression, commits
and pushes come afterwards, off the workload's path. This page gives the
cost of both parts, the benchmark that measures them (design §18 test
13), and the performance budget (§19.2 item 1).

Unless a section says otherwise, the numbers below are from the first
CI run of the benchmark, on 2026-10-13 (commit `699bbb7`, run
[38334925092](https://github.com/mcurcio/agentfs/actions/runs/38334925092)):
GitHub's `ubuntu-latest` runner, 4 vCPUs (AMD EPYC 7763), 16 GiB, kernel
6.17, FUSE passthrough on, each filesystem a fresh 20 GiB loop device on
the runner's disk, no remotes configured (so no pushes). They are one
run on a shared machine; the next two runs, some on a different CPU,
were up to about 1.7 times slower in `fsync`-bound results and up to four
times faster in commit catch-up (see [Run to run](#run-to-run)). The
loop device puts every `fsync` through a second filesystem, so
`fsync`-bound numbers are likely somewhat higher than on a dedicated
disk.

## Summary

- **A small file's change costs 1.3–1.9 ms in the engine and 1.8–2.4 ms
  through FUSE** at the median (4 KiB, XFS–ext4). Most of it is the
  journal's `fsync`; a plain close costs 25–30 µs.
- **On XFS the step does not depend on the file's size.** Freezing a
  16 MiB file takes 0.44 ms, the same as 64 KiB, and closing a clean
  1 GiB file through a mount takes 3 ms. A 1 GiB file with dirty pages
  takes 2.6 s, because the reflink waits for writeback.
- **On ext4 the freeze is a copy**: 44 ms for 16 MiB, 2.9 s for a clean
  1 GiB file and 5.4 s for a dirty one. Use XFS (or btrfs) for volumes
  that hold large files; ext4 is fine for small ones.
- **Workloads of many small files run 14–83 times slower** than in a
  plain directory: about 2.8–3.5 ms per file for a `git clone`, 2.3–3.3
  ms per created file, 0.8–1.3 ms per removed file.
- **A file written and closed at once is recorded twice**: once at its
  close (FLUSH) and again, asynchronously, at RELEASE, through the
  racy-timestamp rule. This doubles the journal records, the frozen
  copies and the commit work of such files, though not the latency of the
  close. It is a known cost with a follow-up: see
  [Known costs](#known-costs).
- **Excluded paths still pay FUSE**: about 0.63 ms per small file written
  into an excluded `node_modules/`, against 0.035 ms in a plain
  directory, and nothing is recorded.
- **Commits keep up with ordinary work but not with bulk creation**:
  100,000 new files made about 200,000 records (the double record), which
  the history caught up with in about 8 minutes on AMD runners (about 420
  records a second) and about 2 minutes on Intel ones; committing a 1 GiB
  file takes about 28 s per record (hashing and compressing it), so 56 s
  for one just written. None of it delays the workload.

## The synchronous step, without FUSE

`cargo bench -p agentfs-core --bench record_step`
([source](../crates/agentfs-core/benches/record_step.rs)) times the
engine's part of a boundary, `Recorder::record`, on a real volume with
the commit loop running, so FUSE is not included. Microseconds:

| Case | XFS p50 | XFS p99 | ext4 p50 | ext4 p99 |
|---|---|---|---|---|
| freeze 4 KiB | 266 | 946 | 934 | 3098 |
| freeze 64 KiB | 452 | 836 | 1017 | 2189 |
| freeze 1 MiB | 452 | 692 | 1608 | 18837 |
| freeze 16 MiB | 440 | 549 | 44331 | 65023 |
| record close 4 KiB, 1 thread | 1265 | 3572 | 1851 | 5663 |
| record close 4 KiB, 8 threads | 2853 | 5007 | 3136 | 6121 |
| record close 1 MiB | 1800 | 4050 | 2881 | 6543 |
| record close 16 MiB | 1438 | 2713 | 31291 | 95173 |
| record unlink | 453 | 836 | 840 | 2041 |

- *freeze* is `PendingStore::freeze` alone, of a clean file: XFS
  reflinks, ext4 copies and `fsync`s the copy.
- *record close* is the whole step: path rules, claims, the record-time
  checks, the freeze, the journal append and its group-commit `fsync`.
  The first case (4 KiB, 500 calls) also warms the volume up, so the
  "1 thread" row is the steadier number for a small file.
- With 8 threads closing at once each call takes about twice as long,
  but 8 complete in that time: the journal's group commit shares one
  `fsync` among them, so throughput is 3.5 (XFS) to 4.7 (ext4) times one
  thread's.
- *record unlink* journals a removal and freezes nothing.

## Workloads through a mount

[`ci/bench.sh`](../ci/bench.sh) runs each workload in an `agentfs mount`
of a fresh volume and, as the baseline, in a plain directory on the same
filesystem, with the page cache dropped first. After each it waits until
`agentfs status` shows no backlog ("catch-up") and checks the history
against the backing tree. Medians of 3 runs, except close4k, create and
rm (1 run):

| Workload | XFS plain | XFS agentfs | ext4 plain | ext4 agentfs | Catch-up (XFS, ext4) | Records |
|---|---|---|---|---|---|---|
| close4k: write 4 KiB and `close`, 2,000 times over 50 files (p50) | 29 µs | 1.81 ms | 26 µs | 2.42 ms | 0.2 s | 4,000 |
| clone: `git clone` of git.git v2.47.0 (4,524 files), `agentfs.nested=working-tree` | 0.75 s | 12.7 s | 0.76 s | 16.0 s | 1.1 s, 0.7 s | 9,041–9,048 |
| npm: 30,000 small files into an excluded `node_modules/` | 1.09 s | 19.0 s | 0.93 s | 18.9 s | 0 s | 0 |
| create: 100,000 files of 64 bytes | 3.7 s | 230 s | 4.0 s | 330 s | 483 s, 474 s | 199,923–200,008 |
| rm: `rm -rf` of those 100,000 files | 5.7 s | 78 s | 2.7 s | 132 s | 0.2 s | 100,000 |
| bigclose, fsynced: `close` of a 1 GiB file after `fsync` | 0 | 3 ms | 0 | 2.86 s | 28 s | 1 |
| bigclose, dirty: `close` of a 1 GiB file just written | 0.11 s | 2.58 s | 0.30 s | 5.43 s | 56 s | 2 (1–3) |

*Records* is per run, over all three CI runs (the first run's close4k
count was not captured): the change boundaries are one per close, file
or removal, so every workload that writes a file and closes it at once
shows about twice as many records as boundaries. That is the double
record of [Known costs](#known-costs).

For comparison, copying the 1 GiB file and `fsync`ing the copy in the
plain directory took 2.6–2.9 s (clean) and 3.8–5.4 s (dirty): ext4's
freeze is that copy.

What the numbers mean:

- **Closes of small files** cost the step plus FUSE's round trips:
  about 0.5 ms more through a mount than in the engine alone. Each close
  makes two records, the second at RELEASE, after `close` has returned:
  it does not add to the latency measured here, but it takes a second
  journal `fsync` and freeze on the daemon's side.
- **A git clone** writes each working-tree file once and closes it: one
  boundary per file, recorded twice (FLUSH, then RELEASE), so about 9,046
  records for 4,524 files. Creating a file records nothing by itself.
  Its `.git` directory is never recorded (`nested=working-tree`); the
  60–83 commits per clone came out at the default `commitWindow` of
  200 ms.
- **Excluded paths** (`info/exclude`) are decided at once and nothing is
  frozen or journaled (the benchmark checks that the workload adds no
  records), but every operation still goes through the daemon: `create`,
  `open`, `flush` and `release` round trips. Keep large dependency trees
  (`node_modules/`, build output) out of the mount altogether when that
  cost matters, not just excluded.
- **Bulk creation** outpaces the commit loop: creating 100,000 files took
  4–5.5 minutes and made about 200,000 records, half of them the
  RELEASE duplicates, and committing them took about 8 more minutes on
  the AMD runners (2 on the Intel ones), during which `agentfs status`
  showed the backlog shrinking. The journal holds the records durably
  meanwhile, and the workload was never slowed by it.
- **Removals** are cheaper than creations (nothing frozen), and each is
  one journaled `fsync`ed record: no RELEASE follows an `unlink`.
- **A 1 GiB commit** is a loose object written by hashing and
  zlib-compressing the frozen copy, about 28 s per record. The dirty
  variant's catch-up takes twice as long because it makes two records:
  its close follows the last write within the racy window, so RELEASE
  freezes the file again (a second reflink on XFS, a second 1 GiB copy
  on ext4) and the commit loop hashes and compresses it twice. The
  fsynced variant's 2.6 s `fsync` puts its stamp outside the window, so
  it makes one record.

## Known costs

### A file written and closed at once is recorded twice

Change detection compares the inode's stamp (size, mtime, ctime, mode)
with the one taken before the last freeze (design §3.1). File timestamps
come from a coarse clock, so a write in the same clock tick as the
freeze would not move the stamp. git's racy-timestamp rule covers that:
when the recorded stamp is within twice the clock's resolution (at least
1 ms, typically 8 ms) of the time the freeze started, the next boundary
records again even if the stamp is unchanged.

A program that writes a file and closes it right away (an editor's save,
`git clone`, a compiler, `cp`) always lands in that window. Its `close`
records at FLUSH, and the RELEASE that follows the last close a moment
later finds the stamp still racy and records again. The benchmark shows
it in every workload that writes files:

- close4k rewrites 50 existing files 2,000 times and makes 4,000 records;
- create makes about 200,000 records for 100,000 files, and clone about
  9,046 for 4,524;
- bigclose, dirty, makes 2 records and takes 56 s to commit, while
  bigclose, fsynced, whose 2.6 s `fsync` takes the stamp out of the
  window, makes 1 and takes 28 s.

What it costs:

- **Not the latency of the call.** The second record is made at RELEASE,
  after `close` has returned (design §3.1), so the per-close numbers
  above do not include it.
- **The daemon's work and the disk's.** Each duplicate is a second freeze
  and a second journal `fsync`. For small files that doubles the
  `fsync`s; for a 1 GiB file on ext4 it is a second 1 GiB copy (about
  5 s) and a second 1 GiB file in `pending/` until the commit.
- **Commit catch-up.** The commit loop hashes and compresses the content
  twice, so the catch-up after bulk creation or a large file is about
  twice what one record per file would take. The catch-up budget below
  includes it. The duplicate changes nothing in the tree, so it adds no
  commit.

Why it is not simply skipped: RELEASE is the backstop for writes through a
shared memory mapping (design §3.1), which reach the file without the
daemon seeing them and can follow the FLUSH within the same clock tick.
Such a write leaves the stamp unchanged; only the racy rule records it.
Skipping the RELEASE record would lose it.

Follow-up (design §19.2 item 4): skip the duplicate when RELEASE can
show that nothing changed since the FLUSH freeze, for example by
comparing the file with its frozen copy while that copy is still in
`pending/` (cheap for small files, and with a reflink often by extent),
or by waiting out the window at FLUSH for files above a size where a few
milliseconds of latency are cheaper than a second freeze. The benchmark
checks the record counts of close4k and create lie between one and two
per boundary, so a fix shows as a drop to one, and a regression past two
fails the job.

## Performance budget

Design §19.2 item 1 is decided: the budget is
[`ci/bench-budget.json`](../ci/bench-budget.json), checked by every run
of the benchmark. It is **advisory**: a result over budget is a warning
in the job and its summary, never a failure, because shared runners are
noisy. Each limit is about 1.5 times the slowest of the first three runs
(six jobs, on two CPU types; see [Run to run](#run-to-run)). Workloads
are budgeted in absolute time, not as ratios.

| Budget | Limit | Measured (slowest to fastest of six jobs) |
|---|---|---|
| Engine step, 4 KiB close, p50 | 3.5 ms | 1.3–2.3 ms |
| Engine step, 4 KiB close, p99 | 10 ms | 2.8–5.7 ms |
| Engine step, 4 KiB close with 8 threads, p99 | 12 ms | 4.4–6.6 ms |
| Engine step, unlink, p50 | 2 ms | 0.45–1.2 ms |
| XFS: freeze of 16 MiB, p50 | 1 ms | 0.44–0.58 ms |
| XFS: engine step for 16 MiB, p50 | 3 ms | 1.3–1.6 ms |
| 4 KiB close through FUSE, p50 | 6 ms | 1.8–4.0 ms |
| git clone of 4,524 files | 30 s | 12.5–18.9 s |
| 30,000 small files into an excluded path | 30 s | 17.1–19.0 s |
| 100,000 creates | 600 s | 230–384 s |
| Catch-up after 100,000 creates (about 200,000 records) | 12 min | 2.0–8.1 min |
| `rm -rf` of 100,000 files | 270 s | 78–177 s |
| XFS: close of a clean 1 GiB file | 0.5 s | 2–10 ms |
| XFS: close of a dirty 1 GiB file | 8 s | 2.5–5.4 s |
| ext4: close of a clean 1 GiB file | 7 s | 2.8–4.3 s |
| ext4: close of a dirty 1 GiB file | 12 s | 4.9–8.1 s |

The catch-up budget measures the double record above: when the follow-up
lands it should roughly halve, and the limit should come down with it.

## Run to run

Three runs on consecutive commits, each with an XFS and an ext4 job, so
six jobs; `env.json` in each artifact names the CPU:

| Run | Commit | XFS job | ext4 job |
|---|---|---|---|
| [38334925092](https://github.com/mcurcio/agentfs/actions/runs/38334925092) | `699bbb7` | AMD EPYC 7763 | AMD EPYC 7763 |
| [38337353781](https://github.com/mcurcio/agentfs/actions/runs/38337353781) | `b67c5c0` | Intel Xeon 6973P-C | AMD EPYC 7763 |
| [38339428837](https://github.com/mcurcio/agentfs/actions/runs/38339428837) | `25f10d5` | AMD EPYC 7763 | Intel Xeon 6973P-C |

On the same CPU type and filesystem the results agreed within about
20 %. Between the two CPU types they did not:

- **The Intel runners were slower at `fsync` and copies.** Their ext4 job
  closed a small file in 4.0 ms through FUSE (2.3–2.4 ms on AMD), took
  177 s for the `rm -rf` (121–132 s), 7.3–8.1 s for the dirty 1 GiB
  close (5.4 s) and 4.3 s for the clean one (2.9 s); its 16 MiB freeze
  took 80 ms (44–46 ms). That run's close4k, at 4,010 µs, was just over
  the first budget of 4,000 µs and warned, which led to the limits
  above.
- **The Intel runners committed much faster**: the catch-up after
  100,000 creates took 119–136 s there and 453–484 s on AMD, and a 1 GiB
  commit 23 s instead of 28 s.
- **The plain-directory baselines moved more than agentfs did**: the
  Intel XFS runner wrote the 30,000 small files in 0.33 s instead of
  1.09 s and created 100,000 files in 1.2 s instead of 3.7 s, so the
  ratios against it rose to 52× and 231×, while agentfs's own times
  moved by 25 % at most.

Compare absolute times across runs and ratios only within one; that is
why the budget is in absolute time, and why its limits cover the slower
CPU. Keying the limits by CPU would make them tighter, but would need
more runs per CPU type than three runs give; it is not done.

The budget is a regression alarm, not a promise about other hardware.
On a dedicated disk with a fast `fsync` the small-file numbers are lower;
on network storage they are higher, by the cost of an `fsync` per change.

## The benchmark

[`.github/workflows/bench.yml`](../.github/workflows/bench.yml) runs on
every push to main that changes more than documentation, and on demand
(with `quick` for a smoke run of smaller workloads), once on XFS created
with `mkfs.xfs -m reflink=1` and once on ext4. Each job:

1. creates and mounts the loop filesystem, and writes `env.json` (kernel,
   CPU, memory, commit);
2. runs the `record_step` bench on it with
   `AGENTFS_BENCH_EXPECT_FREEZE=reflink` (XFS) or `copy` (ext4), which
   fails the job if the freeze used the other method;
3. runs the workloads above ([`ci/bench-workloads.py`](../ci/bench-workloads.py),
   [`ci/boundary-latency.py`](../ci/boundary-latency.py)) and their
   checks: the history equals the backing tree, the clones' git
   directories and `node_modules/` are not in it, every created and
   removed file is, the excluded files journaled nothing (a recorded
   marker written after each npm run must account for every new
   sequence number), close4k and create made between one and two records
   per boundary, and every workload's backlog drained within 30 minutes;
4. writes the report ([`ci/bench-report.py`](../ci/bench-report.py)) to
   the job summary, with the budget warnings, and uploads everything as
   the `bench-xfs` or `bench-ext4` artifact, kept 90 days:
   `record.jsonl`, `results.jsonl`, `record.md`, `env.json` and the mount
   logs.

A failed check fails the job; a slow result only warns.

### Running it yourself

The engine bench needs nothing but a directory on the filesystem to
measure:

```sh
AGENTFS_BENCH_DIR=/mnt/xfs/bench cargo bench -p agentfs-core --bench record_step
```

`AGENTFS_BENCH_SIZES` (default `4k,64k,1m,16m`), `AGENTFS_BENCH_N`
(calls per case, default 500), `AGENTFS_BENCH_THREADS` (default `1,8`),
`AGENTFS_BENCH_JSON` (a file to append JSON lines to) and
`AGENTFS_BENCH_EXPECT_FREEZE` (`reflink` or `copy`) configure it.

The workloads need Linux with `/dev/fuse`, `fuse3`, `git`, `jq`,
`python3` and passwordless sudo, and fetch git.git from GitHub
(`BENCH_GIT_URL`, `BENCH_GIT_TAG` to change it):

```sh
cargo build --release -p agentfs
ci/bench.sh dir /mnt/xfs/bench       # the filesystem that directory is on
ci/bench.sh xfs                      # or a fresh loop filesystem (also ext4)
python3 ci/bench-report.py --budget ci/bench-budget.json bench-out
```

`BENCH_QUICK=1` shrinks every workload; `BENCH_OUT` sets the output
directory (default `./bench-out`).

### Columns of the report

- *Plain directory*, *agentfs*: the workload's main metric, median of
  the runs; *Ratio* is agentfs over plain (none for the big-file closes,
  where a plain close costs nothing).
- *Commit catch-up*: from the end of the workload until `agentfs status`
  shows an empty backlog.
- *Records*: how far the committed journal sequence (`Agentfs-Seq` of the
  branch head) moved: the records journaled during the workload,
  including the RELEASE duplicates of [Known costs](#known-costs). A
  last record whose commit would change nothing (such as a duplicate
  committed on its own) leaves no commit and does not move the sequence,
  so this can be a little lower than the records journaled.
- *Commits*: commits added to the branch.
