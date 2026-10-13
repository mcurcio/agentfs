# Performance

What recording costs, measured. agentfs makes the call that ends a change
(a `close` after a write, `unlink`, `rename`, ...) wait until the change
is durable (design §3.2): the content is frozen into `pending/` and a
journal record is appended and `fsync`ed. Hashing, compression, commits
and pushes come afterwards, off the workload's path. This page gives the
cost of both parts, the benchmark that measures them (design §18 test
13), and the performance budget (§19.2 item 1).

All numbers below are from the first CI run of the benchmark, on
2026-10-13 (commit `699bbb7`, run
[38334925092](https://github.com/mcurcio/agentfs/actions/runs/38334925092)):
GitHub's `ubuntu-latest` runner, 4 vCPUs (AMD EPYC 7763), 16 GiB, kernel
6.17, FUSE passthrough on, each filesystem a fresh 20 GiB loop device on
the runner's disk, no remotes configured (so no pushes). They are one run on a shared machine: treat
differences under about 30 % as noise. The loop device puts every
`fsync` through a second filesystem, so `fsync`-bound numbers are likely
somewhat higher than on a dedicated disk.

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
  plain directory: about 2.8–3.5 ms per file for a `git clone` (two
  boundaries per file: create and close), 2.3–3.3 ms per created file,
  0.8–1.3 ms per removed file.
- **Excluded paths still pay FUSE**: about 0.63 ms per small file written
  into an excluded `node_modules/`, against 0.035 ms in a plain
  directory, and nothing is recorded.
- **Commits keep up with ordinary work but not with bulk creation**:
  after 100,000 files the history caught up in about 8 minutes (about 420
  records a second); committing a 1 GiB file takes about 28 s (hashing
  and compressing it). Neither delays the workload.

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

| Workload | XFS plain | XFS agentfs | ext4 plain | ext4 agentfs | Catch-up (XFS, ext4) |
|---|---|---|---|---|---|
| close4k: write 4 KiB and `close`, 2,000 times over 50 files (p50) | 29 µs | 1.81 ms | 26 µs | 2.42 ms | 0.2 s |
| clone: `git clone` of git.git v2.47.0 (4,524 files), `agentfs.nested=working-tree` | 0.75 s | 12.7 s | 0.76 s | 16.0 s | 1.1 s, 0.7 s |
| npm: 30,000 small files into an excluded `node_modules/` | 1.09 s | 19.0 s | 0.93 s | 18.9 s | 0 s |
| create: 100,000 files of 64 bytes | 3.7 s | 230 s | 4.0 s | 330 s | 483 s, 474 s |
| rm: `rm -rf` of those 100,000 files | 5.7 s | 78 s | 2.7 s | 132 s | 0.2 s |
| bigclose, fsynced: `close` of a 1 GiB file after `fsync` | 0 | 3 ms | 0 | 2.86 s | 28 s |
| bigclose, dirty: `close` of a 1 GiB file just written | 0.11 s | 2.58 s | 0.30 s | 5.43 s | 56 s |

For comparison, copying the 1 GiB file and `fsync`ing the copy in the
plain directory took 2.6–2.9 s (clean) and 3.8–5.4 s (dirty): ext4's
freeze is that copy.

What the numbers mean:

- **Closes of small files** cost the step plus FUSE's round trips:
  about 0.5 ms more through a mount than in the engine alone.
- **A git clone** writes each file once (create, then close, two
  records), plus its `.git` directory, which is never recorded
  (`nested=working-tree`); the 60–83 commits per clone came out at the
  default `commitWindow` of 200 ms.
- **Excluded paths** (`info/exclude`) are decided at once and nothing is
  frozen or journaled, but every operation still goes through the daemon:
  `create`, `open`, `flush` and `release` round trips. Keep large
  dependency trees (`node_modules/`, build output) out of the mount
  altogether when that cost matters, not just excluded.
- **Bulk creation** outpaces the commit loop: creating 100,000 files took
  4–5.5 minutes and committing them about 8 more, during which `agentfs
  status` showed the backlog shrinking. The journal holds the records
  durably meanwhile, and the workload was never slowed by it.
- **Removals** are cheaper than creations (nothing frozen), but each is
  still a journaled `fsync`ed record.
- **A 1 GiB commit** is a loose object written by hashing and
  zlib-compressing the frozen copy, about 28 s. The dirty variant's
  catch-up took twice as long on both filesystems; the benchmark does not
  yet show why.

## Performance budget

Design §19.2 item 1 is decided: the budget is
[`ci/bench-budget.json`](../ci/bench-budget.json), checked by every run
of the benchmark. It is **advisory**: a result over budget is a warning
in the job and its summary, never a failure, because shared runners are
noisy. Each limit is about 1.5–2 times the slower result of the
first two runs. Workloads are budgeted in absolute time, not as ratios:
see [Run to run](#run-to-run).



| Budget | Limit | Measured (XFS, ext4) |
|---|---|---|
| Engine step, 4 KiB close, p50 | 3 ms | 1.3 ms, 1.9 ms |
| Engine step, 4 KiB close, p99 | 10 ms | 3.6 ms, 5.7 ms |
| Engine step, 4 KiB close with 8 threads, p99 | 12 ms | 5.0 ms, 6.1 ms |
| Engine step, unlink, p50 | 1.5 ms | 0.45 ms, 0.84 ms |
| XFS: freeze of 16 MiB, p50 | 1 ms | 0.44 ms |
| XFS: engine step for 16 MiB, p50 | 3 ms | 1.4 ms |
| 4 KiB close through FUSE, p50 | 4 ms | 1.8 ms, 2.4 ms |
| git clone of 4,524 files | 25 s | 12.7 s, 16.0 s |
| 30,000 small files into an excluded path | 30 s | 19.0 s, 18.9 s |
| 100,000 creates | 500 s | 230 s, 330 s |
| Catch-up after 100,000 creates | 12 min | 8.0 min, 7.9 min |
| `rm -rf` of 100,000 files | 200 s | 78 s, 132 s |
| XFS: close of a clean 1 GiB file | 0.5 s | 3 ms |
| XFS: close of a dirty 1 GiB file | 8 s | 2.6 s (5.3 s in the second run) |
| ext4: close of a clean 1 GiB file | 6 s | 2.9 s |
| ext4: close of a dirty 1 GiB file | 10 s | 5.4 s |

## Run to run

The second run, on the next commit
([38337353781](https://github.com/mcurcio/agentfs/actions/runs/38337353781)),
got an XFS runner whose plain directory was about three times faster
(30,000 small files in 0.33 s instead of 1.09 s, 100,000 creates in
1.2 s instead of 3.7 s), so the ratios against it rose to 52× and 231×,
while agentfs's own times moved by 25 % at most (the same workloads in
17.2 s and 275 s). The ext4 results were within about 20 % of the
first run; on XFS the engine step was 30–45 % slower (1.6 ms instead of
1.3 ms for a small file, 0.65 ms instead of 0.45 ms for an unlink). Compare absolute times across runs and
ratios only within one; that is why the budget is in absolute time.

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
   directories and `node_modules/` are not in it, and every created and
   removed file is;
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
  branch head) moved. A record whose commit would change nothing (the
  same content again) leaves no commit, so this can be lower than the
  number of boundaries.
- *Commits*: commits added to the branch.
