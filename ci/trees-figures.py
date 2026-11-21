#!/usr/bin/env python3
"""The tree views' release figures (docs/performance.md, "Tree views, release build").

Runs six flat workloads through a mounted tree view, each against its own
deadline, compares each figure with its bar in ci/trees-budget.json, and
writes the report as JSON. A figure that misses its bar sets `warn`. A gated
one (`"gate": true` in the budget: the many-file `create`) prints an
`::error::` line and the script exits 1 once the report is written; any
other prints a `::warning::` line. A workload that
runs out of time reports what it reached by its deadline; no figure is
extrapolated, because a save's cost grows with the tree. ci/e2e-trees-tracer.sh
runs it as its step 4, and again after it restarts the view (step 5) with
--baseline. Standard library only.

    trees-figures.py --view DIR --filesystem FS --image ID --budget PATH --output PATH
                     [--build release] [--only WORKLOAD]...
    trees-figures.py --view DIR ... --baseline REPORT --restart-mount-s S
                     --restart-peak PATH --output PATH
    trees-figures.py --sample-peak PID --stop-file PATH --peak-output PATH
    trees-figures.py --self-test --budget PATH
    trees-figures.py --self-test-warn --budget PATH

`--self-test` judges a synthetic report whose `create` fell one file short of
its count, with no view, and exits as a run would: 1 while `create` is gated.
`--self-test-warn` judges one where every other bar is missed, and exits as a
run would: 0 while only `create` is gated (owner, 2026-11-20: the new bars
warn, they do not fail CI).

With --baseline, the view was restarted on the state the first run left:
the script reports `restart` and runs `long_history` only, and writes the
first run's report with those two added.

Every memory figure is the sampled peak of the view's RssAnon: its anonymous
resident memory, what the kernel cannot reclaim (clean file-backed git index
pages are not counted, owner 2026-11-20). A PeakSampler reads
/proc/PID/status every 20 ms on a thread of its own from just before the
workload until its history is built (after drain, so the build is inside the
window); a spike shorter than the interval can fall between two samples.
Each memory bar warns and never fails: it is a mark for regressions, not a
target (owner, 2026-11-20: "I dont have a number in mind. Im just looking to
cut waste").

`--sample-peak` samples one process the same way until the stop file exists,
then writes `{"peak_anon_kib": N, "samples": M}` to --peak-output (exit 1
when the process was gone before any sample). The tracer starts it on the
restarted view as soon as `docker run -d` returns and the container's pid is
read, so the view's first milliseconds are unsampled, and touches the stop
file once the view is mounted: that output is --restart-peak.

Every timed save ends with a stat(2) of the file. The view saves a file when
the kernel releases it, which reaches agentfs just after close(2) returns,
and the view answers requests one at a time and in order, so the stat
returns only once the save is committed. Each figure therefore includes the
commit, not only the close.

    close_4k   a 4 KiB file rewritten and closed `count` times: p50 and p99 of
               one open, write, close and stat, in ms
    create     one-line files created in the root until `count` or the
               deadline: the count reached, and the cost of a save against
               the files in the tree
    close_64m  one 64 MiB file written in 1 MiB blocks (random bytes, varied
               per MiB, as the benchmark's bigclose), closed and statted: total
               seconds, or the bytes written by the deadline
    close_1g   one 1 GiB file written the same way, then closed and statted:
               the seconds of the close and stat alone (the benchmark's dirty
               bigclose), the total, or the bytes written by the
               deadline. Before it, 32 files of 1 MiB written
               in one write each: `write_1g` is the median of the big file's
               last 64 writes over the median of those (a write costs its own
               bytes at any file size)
    history_1g the 1 GiB save's drain_s: from its close and stat to the built
               point reaching it (needs --metrics)
    memory_1g  `peak_extra_mib`, the view's sampled RssAnon peak through the
               1 GiB workload, its save and its build, over its RssAnon
               before, and `peak_anon_mib`, that peak (needs --view-pid)
    memory_create `kib_per_file`, the view's sampled RssAnon peak through
               the `create` workload and its build, over its RssAnon before,
               per file created: what the view holds for each file of a tree,
               and `peak_anon_mib`, that peak (needs --view-pid and
               --metrics). `create`'s `first_2000_ms` is the mean ms of its
               first 2,000 creates
    close_frag one 64 MiB file written in 1 MiB blocks and closed, then
               opened again for 200,000 64-byte writes at random offsets,
               closed and statted: `close_s`, the close and stat alone, and
               `lock_hold_max_ms`, the longest hold of the volume's actor lock
               (stages.actor_lock.max_nanos in the metrics file, read once its
               history is built; needs --metrics). The maximum is the whole
               process's: only a run with `--only close_frag` holds the close
               alone, which is how the bar is read
    write_1m   1,000,000 sequential 64-byte appends in one open, closed and
               statted: `close_s`, and `peak_extra_mib` and `peak_anon_mib`,
               read through the save and its build as memory_1g's are (needs
               --view-pid)
    restart    (--baseline) the view stopped and mounted again on the state
               the first run left (about 50,000 files): `mount_s`, from the
               start to its `mounted` line (the tracer's --restart-mount-s),
               and `peak_anon_mib`, the new view's sampled RssAnon peak from
               its start to its `mounted` line (--restart-peak)
    long_history (--baseline) 2,000 more creates on the restarted view:
               `ratio`, their mean ms over the first run's `create`
               `first_2000_ms`

With --metrics, every workload also reports `lock`: the volume lock's holds
by kind of work (`compaction`, `catch_up`, `drops`, `close`, `take`,
`fork_copy`, `other`; docs/telemetry.md, "The volume lock") and the waits
to take it, from the metrics file read before the workload and again once
its history is built and one more save (`fig-nudge`, the first call after
history caught up, which runs the compaction the workload made due) is
built too. Per kind: `holds`, the holds it ran in; `total_ms`,
its time; `longest_ms`, its longest share of one hold, given only when that
maximum, kept process-wide, grew during this workload (else null: an
earlier workload's was longer). `longest_kind` names the kind with the
longest share among those. The figures carry no bar: they are read, and
never warn or fail (owner, 2026-11-20). Run a workload alone (`--only`) to
read its longest holds without an earlier workload's.

Each workload runs on a worker thread. One the deadline catches inside a
save gets `grace_s` more to come back; one still blocked then is stuck in
the view, so the FUSE connection is aborted (/sys/fs/fuse/connections, as
root) to free it, and the workloads after it are reported as not run.
"""

import argparse
import json
import math
import os
import random
import statistics
import subprocess
import sys
import threading
import time

MIB = 1 << 20
BIG_BYTES = 64 * MIB
HUGE_BYTES = 1024 * MIB
SMALL_FILES = 32
LAST_WRITES = 64
FRAG_WRITES = 200_000
SMALL_WRITE = 64
APPENDS = 1_000_000
HISTORY_CREATES = 2_000
GUARANTEE = ("saved before the view answers any later request; "
             "history built later (drain_s: from the workload's end to the built point reaching it)")
FUSECTL = "/sys/fs/fuse/connections"


def write_file(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        while data:
            data = data[os.write(fd, data):]
    finally:
        os.close(fd)


def page(n):
    """4 KiB of text whose first line is n, so every rewrite is a change."""
    head = f"{n}\n".encode()
    line = b"x" * 63 + b"\n"
    body = head + line * (4096 // 64)
    return body[:4096]


def ms(seconds):
    return round(seconds * 1000, 3)


def p99(samples):
    ordered = sorted(samples)
    return ordered[max(0, math.ceil(0.99 * len(ordered)) - 1)]


def tree_files(view):
    return sum(len(files) for _, _, files in os.walk(view))


class Run:
    """One workload's progress, shared between its worker and the referee.

    `samples` holds the seconds of each save that ended by the deadline;
    `step` advances the shared state only while the deadline has not passed.
    """

    def __init__(self, deadline):
        self.deadline = deadline
        self.samples = []
        self.extra = {}
        self.finished = False
        self.error = None
        self.lock = threading.Lock()

    def open(self):
        return time.monotonic() < self.deadline

    def done(self, start):
        end = time.monotonic()
        with self.lock:
            if end <= self.deadline:
                self.samples.append(end - start)
                return True
            return False


def close_4k(view, run, target):
    path = os.path.join(view, "fig-close4k")
    run.extra["tree_files"] = tree_files(view)
    for n in range(1, target + 1):
        if not run.open():
            return
        data = page(n)
        start = time.monotonic()
        write_file(path, data)
        os.stat(path)
        if not run.done(start):
            return
    run.finished = True


def create(view, run, target):
    run.extra["tree_files_at_start"] = tree_files(view)
    for n in range(1, target + 1):
        if not run.open():
            return
        path = os.path.join(view, f"fig-create-{n}")
        start = time.monotonic()
        write_file(path, f"{n}\n".encode())
        os.stat(path)
        if not run.done(start):
            return
    run.finished = True


def close_64m(view, run, _target):
    path = os.path.join(view, "fig-64m")
    block = bytearray(os.urandom(MIB))
    run.extra["bytes_written"] = 0
    start = time.monotonic()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for n in range(BIG_BYTES // MIB):
            if not run.open():
                return
            block[:8] = n.to_bytes(8, "little")
            chunk = memoryview(block)
            while chunk:
                chunk = chunk[os.write(fd, chunk):]
            run.extra["bytes_written"] += MIB
    finally:
        os.close(fd)
    os.stat(path)
    run.finished = run.done(start)


def metrics_of(path):
    """The view's metrics file, or None."""
    if not path:
        return None
    try:
        with open(path) as file:
            return json.load(file)
    except (OSError, ValueError):
        return None


def close_1g(view, run, _target, metrics=None):
    block = bytearray(os.urandom(MIB))
    run.extra.update(bytes_written=0)
    small = []
    for n in range(SMALL_FILES):
        if not run.open():
            return
        block[:8] = n.to_bytes(8, "little")
        fd = os.open(os.path.join(view, f"fig-1m-{n}"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        try:
            start = time.monotonic()
            written = os.write(fd, block)
            small.append(time.monotonic() - start)
            assert written == MIB, "one write of 1 MiB"
        finally:
            os.close(fd)
    path = os.path.join(view, "fig-1g")
    writes = []
    start = time.monotonic()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    closing = None
    try:
        for n in range(HUGE_BYTES // MIB):
            if not run.open():
                return
            block[:8] = n.to_bytes(8, "little")
            chunk = memoryview(block)
            began = time.monotonic()
            while chunk:
                chunk = chunk[os.write(fd, chunk):]
            writes.append(time.monotonic() - began)
            run.extra["bytes_written"] += MIB
        closing = time.monotonic()
    finally:
        os.close(fd)
    os.stat(path)
    end = time.monotonic()
    if end > run.deadline:
        return
    run.samples.append(end - closing)
    run.extra["total_s"] = round(end - start, 3)
    last = statistics.median(writes[-LAST_WRITES:])
    first = statistics.median(small)
    run.extra["write_1g"] = {"p50_last_ms": ms(last), "p50_1m_ms": ms(first),
                             "ratio": round(last / first, 3) if first else None}
    run.finished = True


def write_all(fd, data):
    chunk = memoryview(data)
    while chunk:
        chunk = chunk[os.write(fd, chunk):]


def close_frag(view, run, _target):
    """A 64 MiB file fragmented by 200,000 random small writes in one open,
    then closed: the close that held every view of the volume for 99 s."""
    path = os.path.join(view, "fig-frag")
    block = bytearray(os.urandom(MIB))
    run.extra.update(writes=0)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for n in range(BIG_BYTES // MIB):
            if not run.open():
                return
            block[:8] = n.to_bytes(8, "little")
            write_all(fd, block)
    finally:
        os.close(fd)
    os.stat(path)
    # The same offsets and bytes in every run.
    rng = random.Random(1)
    closing = None
    fd = os.open(path, os.O_WRONLY)
    try:
        for n in range(FRAG_WRITES):
            if n % 1000 == 0 and not run.open():
                return
            os.pwrite(fd, rng.randbytes(SMALL_WRITE), rng.randrange(BIG_BYTES - SMALL_WRITE))
            run.extra["writes"] += 1
        closing = time.monotonic()
    finally:
        os.close(fd)
    os.stat(path)
    end = time.monotonic()
    if end > run.deadline:
        return
    run.samples.append(end - closing)
    run.finished = True


def write_1m(view, run, _target):
    """1,000,000 sequential 64-byte appends in one open, then closed."""
    path = os.path.join(view, "fig-appends")
    line = b"x" * (SMALL_WRITE - 1) + b"\n"
    run.extra.update(writes=0)
    start = time.monotonic()
    closing = None
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_APPEND, 0o644)
    try:
        for n in range(APPENDS):
            if n % 1000 == 0 and not run.open():
                return
            write_all(fd, line)
            run.extra["writes"] += 1
        closing = time.monotonic()
    finally:
        os.close(fd)
    os.stat(path)
    end = time.monotonic()
    if end > run.deadline:
        return
    run.samples.append(end - closing)
    run.extra["total_s"] = round(end - start, 3)
    run.finished = True


def long_history(view, run, target):
    """`create`'s files again, on the restarted view of a long history."""
    for n in range(1, target + 1):
        if not run.open():
            return
        path = os.path.join(view, f"fig-history-{n}")
        start = time.monotonic()
        write_file(path, f"{n}\n".encode())
        os.stat(path)
        if not run.done(start):
            return
    run.finished = True


def by_tree_size(samples, start_files):
    """The cost of a save against the files in the tree, in windows of saves
    ending at 1, 2 and 5 times each power of ten, and the last save."""
    edges = []
    edge = 1
    while edge < len(samples):
        for step in (1, 2, 5):
            if edge * step < len(samples):
                edges.append(edge * step)
        edge *= 10
    edges.append(len(samples))
    rows, low = [], 0
    for high in edges:
        window = samples[low:high]
        if window:
            rows.append({"tree_files": start_files + high, "saves": len(window),
                         "p50_ms": ms(statistics.median(window)), "max_ms": ms(max(window))})
        low = high
    return rows


def figures(name, run, budget):
    """The report's figure for one workload, from what it reached by its deadline."""
    samples = run.samples
    figure = {"deadline_s": budget["deadline_s"], "finished": run.finished, **run.extra}
    if name == "close_4k":
        figure.update(target=budget["count"], count=len(samples))
        if samples:
            figure.update(p50_ms=ms(statistics.median(samples)), p99_ms=ms(p99(samples)))
    elif name == "create":
        first = samples[:HISTORY_CREATES]
        figure.update(target=budget["count"], count=len(samples),
                      total_s=round(sum(samples), 3),
                      first_2000_ms=(ms(statistics.mean(first))
                                     if len(first) == HISTORY_CREATES else None),
                      cost_by_tree_size=by_tree_size(samples, run.extra["tree_files_at_start"]))
    elif name == "long_history":
        figure.update(target=budget["count"], count=len(samples))
        if samples:
            figure.update(ms_per_create=ms(statistics.mean(samples)))
    elif name in ("close_1g", "close_frag", "write_1m"):
        figure.update(bytes={"close_1g": HUGE_BYTES, "close_frag": BIG_BYTES,
                             "write_1m": APPENDS * SMALL_WRITE}[name])
        if samples:
            figure.update(close_s=round(samples[0], 3))
    else:
        figure.update(bytes=BIG_BYTES)
        if samples:
            figure.update(total_s=round(samples[0], 3))
    if run.error:
        figure["error"] = run.error
    return figure


def missed(figure, budget, filesystem):
    """Whether a figure misses its bar: a finish is required, and a metric,
    when it has one, is within its `max` (one number, or one per filesystem);
    `metrics` holds several, each with its own max."""
    if not figure["finished"]:
        return True
    bars = dict(budget.get("metrics", {}))
    if "metric" in budget:
        bars[budget["metric"]] = budget["max"]
    for metric, value in bars.items():
        limit = value[filesystem] if isinstance(value, dict) else value
        found = figure.get(metric)
        if found is None or found > limit:
            return True
    return False


def gated_misses(workloads, budgets):
    """The names of the gated workloads in a report that missed their bars."""
    return [w["name"] for w in workloads
            if w.get("warn") and budgets.get(w["name"], {}).get("gate")]


def report_miss(name, held, value, filesystem):
    """One line for a missed bar: `::error::` when it is gated, else
    `::warning::`."""
    shown = {k: v for k, v in value.items() if k != "cost_by_tree_size"}
    if held.get("gate"):
        print(f"::error title=tree views, {filesystem}::{name} missed its bar: {held['text']}: "
              f"{json.dumps(shown)}", flush=True)
    else:
        print(f"::warning title=tree views, {filesystem}::{name} misses its bar, "
              f"{held['text']}: {json.dumps(shown)}", flush=True)


def self_test(budgets, filesystem="ext4"):
    """Judges a synthetic `create` one file short of its count, unfinished:
    the exit status a run with that figure has."""
    held = budgets["create"]
    value = {"finished": False, "target": held["count"], "count": held["count"] - 1}
    workloads = [{"name": "create", **value, "warn": missed(value, held, filesystem)}]
    for workload in workloads:
        if workload["warn"]:
            report_miss(workload["name"], held, value, filesystem)
    return 1 if gated_misses(workloads, budgets) else 0


def self_test_warn(budgets, filesystem="ext4"):
    """Judges a synthetic report where every bar but `create`'s is missed
    (unfinished) and `create` met its count: the exit status a run with
    those figures has, 0 while each of them only warns."""
    workloads = []
    for name, held in budgets.items():
        if name == "create":
            value = {"finished": True, "target": held["count"], "count": held["count"]}
        else:
            value = {"finished": False}
        warn = missed(value, held, filesystem)
        workloads.append({"name": name, **value, "warn": warn})
        if warn:
            report_miss(name, held, value, filesystem)
    return 1 if gated_misses(workloads, budgets) else 0


class Renames:
    """The view's metrics file being renamed into place, as inotify events
    (the view writes it at each built or failed event and every 100 ms
    between): a reader waits on the event, never on a timer."""
    IN_MOVED_TO = 0x80

    def __init__(self, path):
        import ctypes
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.name = os.path.basename(path).encode()
        self.fd = self.libc.inotify_init1(os.O_CLOEXEC)
        if self.fd < 0:
            raise OSError(ctypes.get_errno(), "inotify_init1")
        directory = os.path.dirname(os.path.abspath(path)).encode()
        if self.libc.inotify_add_watch(self.fd, directory, self.IN_MOVED_TO) < 0:
            raise OSError(ctypes.get_errno(), "inotify_add_watch")

    def wait(self, until):
        """Returns once the file was renamed into place again, or False at
        the monotonic time `until` (a bound on a hang)."""
        import select
        import struct
        while True:
            left = until - time.monotonic()
            if left <= 0:
                return False
            ready, _, _ = select.select([self.fd], [], [], left)
            if not ready:
                return False
            events = os.read(self.fd, 65536)
            at = 0
            while at < len(events):
                _, _, _, size = struct.unpack_from("iIII", events, at)
                name = events[at + 16:at + 16 + size].rstrip(b"\0")
                at += 16 + size
                if name == self.name:
                    return True

    def close(self):
        os.close(self.fd)


def drain(metrics, cap):
    """Seconds from now until the built point reaches the record last appended
    now, read from the metrics file the view rewrites; None past `cap`. Each
    file carries `seq`: one numbered two past the file read now was written
    from a snapshot taken after now, so its `appended` covers the workload."""
    start = time.monotonic()
    until = start + cap
    renames = Renames(metrics)
    try:
        seen = metrics_of(metrics)
        floor = (seen or {}).get("seq", 0) + 2
        target = None
        while True:
            now = metrics_of(metrics)
            if now and now.get("seq", 0) >= floor:
                if target is None:
                    target = now["history"]["appended"]
                if now["history"]["built_through"] >= target:
                    return round(time.monotonic() - start, 3)
            if not renames.wait(until):
                return None
    finally:
        renames.close()


def host():
    """The runner's CPU model and count: runners of one label differ, and a
    figure is compared across runs only with this beside it."""
    model = None
    try:
        with open("/proc/cpuinfo") as cpuinfo:
            for line in cpuinfo:
                if line.startswith("model name"):
                    model = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    return {"cpu": model, "cpus": os.cpu_count()}


def proc_kib(pid, field):
    """A /proc/PID/status field in KiB, or None."""
    try:
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith(field + ":"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def mib(kib):
    """KiB as MiB to one place, or None."""
    return None if kib is None else round(kib / 1024, 1)


class PeakSampler:
    """The peak of a process's RssAnon (anonymous resident memory, in KiB),
    read from /proc/PID/status every `interval_s` on a daemon thread between
    start and stop. Measurement only: nothing waits on a sample, and a spike
    shorter than the interval can fall between two."""

    def __init__(self, pid: int, interval_s: float = 0.02):
        self.pid, self.interval_s = pid, interval_s
        self.peak, self.samples = None, 0
        self._stopped = threading.Event()
        self._thread = None

    def _sample(self):
        value = proc_kib(self.pid, "RssAnon")
        if value is not None:
            self.samples += 1
            self.peak = value if self.peak is None else max(self.peak, value)
        return value

    def _run(self):
        while not self._stopped.wait(self.interval_s):
            self._sample()

    def start(self) -> int | None:
        """Reads RssAnon now and starts sampling; returns that first read
        (the `before` of a figure), or None."""
        before = self._sample()
        self._thread = threading.Thread(target=self._run, name=f"peak-{self.pid}", daemon=True)
        self._thread.start()
        return before

    def stop(self) -> int | None:
        """Stops sampling after one last read; returns the peak seen, or None
        when no sample was read."""
        self._stopped.set()
        if self._thread is not None:
            self._thread.join()
        self._sample()
        return self.peak


def sample_peak(pid, stop_file, output):
    """--sample-peak: samples `pid` until `stop_file` exists (or the process
    is gone), then writes its peak; 1 when no sample was read."""
    sampler = PeakSampler(pid)
    sampler.start()
    while not os.path.exists(stop_file) and os.path.exists(f"/proc/{pid}"):
        time.sleep(sampler.interval_s)
    peak = sampler.stop()
    if peak is None:
        print(f"::warning title=tree views::process {pid} was gone before any sample", flush=True)
        return 1
    temporary = output + ".tmp"
    with open(temporary, "w") as out:
        json.dump({"peak_anon_kib": peak, "samples": sampler.samples}, out)
        out.write("\n")
    os.replace(temporary, output)
    return 0


def abort_connection(dev):
    """Abort the view's FUSE connection, the one way to free a thread blocked
    in a request the view never answers. Returns whether it was aborted."""
    if not os.path.isdir(FUSECTL) or not os.listdir(FUSECTL):
        subprocess.run(["mount", "-t", "fusectl", "none", FUSECTL], check=False)
    try:
        with open(os.path.join(FUSECTL, str(os.minor(dev)), "abort"), "w") as control:
            control.write("1")
        return True
    except OSError as error:
        print(f"::warning title=tree views::could not abort the FUSE connection: {error}",
              flush=True)
        return False


WORKLOADS = [("close_4k", close_4k), ("create", create), ("close_64m", close_64m),
             ("close_1g", close_1g), ("close_frag", close_frag), ("write_1m", write_1m),
             ("long_history", long_history)]
# Run only on a view restarted on the first run's state (--baseline).
AFTER_RESTART = ("long_history",)
# Workloads whose memory a PeakSampler reads: memory_create, memory_1g, write_1m.
PEAKS = ("create", "close_1g", "write_1m")



LOCK_KINDS = ("compaction", "catch_up", "drops", "close", "take", "fork_copy", "other")


def stages_of(metrics):
    """The view's process-wide stage totals from its metrics file, or None."""
    now = metrics_of(metrics)
    try:
        return now["stages"]
    except (KeyError, TypeError):
        return None


def lock_figures(before, after):
    """The volume lock's holds by kind and its waits between two stage
    snapshots (see the module's docstring), or None without both."""
    def one(now, then):
        grew = now["max_nanos"] > then["max_nanos"]
        return {"holds": now["count"] - then["count"],
                "total_ms": round((now["nanos"] - then["nanos"]) / 1e6, 3),
                "longest_ms": round(now["max_nanos"] / 1e6, 3) if grew else None}
    try:
        hold = {kind: one(after["lock_hold"][kind], before["lock_hold"][kind])
                for kind in LOCK_KINDS}
        wait = one(after["lock_wait"], before["lock_wait"])
    except (KeyError, TypeError):
        return None
    known = {kind: v["longest_ms"] for kind, v in hold.items() if v["longest_ms"] is not None}
    return {"hold": hold, "wait": wait,
            "longest_kind": max(known, key=known.get) if known else None}


def nudge(view, metrics, cap):
    """One save of `fig-nudge` after a workload's history is built, then
    waits for its build: the first call after history caught up runs the
    compaction then due, which the workload's journal made due. Whether the
    build was reached within `cap`."""
    try:
        write_file(os.path.join(view, "fig-nudge"), b"nudge\n")
        os.stat(os.path.join(view, "fig-nudge"))
    except OSError as error:
        print(f"::warning title=tree views::the nudge after a workload failed: {error}", flush=True)
        return False
    return drain(metrics, cap) is not None


def lock_hold_ms(metrics):
    """The longest hold of the actor's lock so far, process-wide, in ms, from
    the view's metrics file; None without one."""
    now = metrics_of(metrics)
    try:
        return round(now["stages"]["actor_lock"]["max_nanos"] / 1e6, 3)
    except (KeyError, TypeError):
        return None


def restarted(args, baseline):
    """The `restart` figure: the tracer's time to mount and the new view's
    sampled RssAnon peak through its mount (--restart-peak), on the state the
    first run left."""
    files = {w["name"]: w for w in baseline["workloads"]}.get("create", {}).get("count")
    peak = None
    if args.restart_peak:
        try:
            with open(args.restart_peak) as file:
                peak = json.load(file).get("peak_anon_kib")
        except (OSError, ValueError) as error:
            print(f"::warning title=tree views::could not read the restart's peak: {error}",
                  flush=True)
    return {"finished": args.restart_mount_s is not None and peak is not None,
            "mount_s": args.restart_mount_s,
            "peak_anon_mib": mib(peak),
            "create_files": files}


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true",
                        help="judge a synthetic report whose create fell short, and exit as a run would")
    parser.add_argument("--self-test-warn", action="store_true",
                        help="judge a synthetic report where every bar but create's missed, and exit as a run would")
    parser.add_argument("--view")
    parser.add_argument("--filesystem")
    parser.add_argument("--image")
    parser.add_argument("--budget")
    parser.add_argument("--output")
    parser.add_argument("--build", default="release",
                        help="the build measured, as the report names it (the image ships release)")
    parser.add_argument("--metrics",
                        help="the view's --metrics-output file: each workload then reports drain_s")
    parser.add_argument("--drain-cap", type=float, default=900.0,
                        help="how long to wait for history to be built after a workload")
    parser.add_argument("--view-pid", type=int,
                        help="the view's process: the memory figures then read it")
    parser.add_argument("--only", action="append", choices=[n for n, _ in WORKLOADS],
                        help="run only this workload (repeatable; a probe's shortcut)")
    parser.add_argument("--baseline",
                        help="the first run's report: the view was restarted on its state, so "
                             "report restart and run long_history, adding both to that report")
    parser.add_argument("--restart-mount-s", type=float,
                        help="with --baseline: the seconds the restarted view took to mount")
    parser.add_argument("--restart-peak",
                        help="with --baseline: --sample-peak's output for the restarted view's mount")
    parser.add_argument("--sample-peak", type=int, metavar="PID",
                        help="sample PID's RssAnon until --stop-file exists, then write its peak")
    parser.add_argument("--stop-file", help="with --sample-peak: stop once this file exists")
    parser.add_argument("--peak-output", help="with --sample-peak: where the peak is written")
    args = parser.parse_args()
    if args.sample_peak is not None:
        if args.stop_file is None or args.peak_output is None:
            parser.error("--sample-peak needs --stop-file and --peak-output")
        sys.exit(sample_peak(args.sample_peak, args.stop_file, args.peak_output))
    if args.budget is None:
        parser.error("--budget is required")
    with open(args.budget) as file:
        policy = json.load(file)
    budgets, grace = policy["budgets"], policy["grace_s"]
    if args.self_test:
        sys.exit(self_test(budgets))
    if args.self_test_warn:
        sys.exit(self_test_warn(budgets))
    for needed in ("view", "filesystem", "image", "output"):
        if getattr(args, needed) is None:
            parser.error(f"--{needed} is required")
    baseline_report = None
    if args.baseline:
        with open(args.baseline) as file:
            baseline_report = json.load(file)
    # Read before any workload: a stat of the view blocks while it is stuck.
    dev = os.stat(args.view).st_dev

    workloads, stopped, aborted = [], False, False
    if baseline_report is not None:
        # Read before any workload: the restarted view's peak through its mount.
        value = restarted(args, baseline_report)
        held = budgets["restart"]
        warn = missed(value, held, args.filesystem)
        workloads.append({"name": "restart", "budget": held["text"], **value, "warn": warn})
        print(f"restart on {args.filesystem}: {json.dumps(value)}", flush=True)
        if warn:
            report_miss("restart", held, value, args.filesystem)
    for name, work in WORKLOADS:
        if args.only and name not in args.only:
            continue
        if (name in AFTER_RESTART) != (baseline_report is not None):
            continue
        budget = budgets[name]
        entry = {"name": name, "budget": budget["text"], "guarantee": GUARANTEE}
        if stopped:
            value = {"deadline_s": budget["deadline_s"], "finished": False,
                     "not_run": "an earlier workload was stuck in the view"}
            workloads.append({**entry, **value, "warn": True})
            report_miss(name, budget, value, args.filesystem)
            continue
        run = Run(time.monotonic() + budget["deadline_s"])
        # The memory figures' window: from before the workload until its
        # history is built (after drain), so the build is inside it.
        sampler = PeakSampler(args.view_pid) if args.view_pid and name in PEAKS else None
        before = sampler.start() if sampler else None
        stages_before = stages_of(args.metrics)

        def worker(work=work, run=run, target=budget.get("count")):
            try:
                if work is close_1g:
                    work(args.view, run, target, args.metrics)
                else:
                    work(args.view, run, target)
            except OSError as error:
                run.error = str(error)

        thread = threading.Thread(target=worker, name=name, daemon=True)
        thread.start()
        thread.join(max(0.0, run.deadline - time.monotonic()) + grace)
        if thread.is_alive():
            # Nothing else can reach the view: free it, report and leave.
            stopped, aborted = True, abort_connection(dev)
            thread.join(grace)
            entry["stuck"] = f"a save was still running {grace} s after the deadline"
        with run.lock:
            figure = figures(name, run, budget)
        if args.metrics and not stopped:
            figure["drain_s"] = drain(args.metrics, args.drain_cap)
        peak = sampler.stop() if sampler else None
        extra = peak - before if peak is not None and before is not None else None
        if name == "close_frag":
            # Read once its history is built: a metrics file written after it.
            held = lock_hold_ms(args.metrics) if figure.get("drain_s") is not None else None
            figure.update(lock_hold_max_ms=held,
                          lock_hold_scope="process" if not args.only else "--only " + " ".join(args.only))
            figure["finished"] = figure["finished"] and held is not None
        if name == "write_1m":
            figure.update(peak_extra_mib=mib(extra), peak_anon_mib=mib(peak))
            figure["finished"] = figure["finished"] and extra is not None
        if name == "long_history":
            first = {w["name"]: w for w in baseline_report["workloads"]}.get("create", {}).get(
                "first_2000_ms")
            mine = figure.get("ms_per_create")
            figure.update(create_first_2000_ms=first,
                          ratio=round(mine / first, 3) if mine and first else None)
        if figure.get("drain_s") is not None:
            # A compaction due once history caught up runs at the next call:
            # the nudge is that call, so it lands in this workload's window.
            nudged = nudge(args.view, args.metrics, args.drain_cap)
            figure["lock"] = lock_figures(stages_before, stages_of(args.metrics)) if nudged else None
            lock = figure["lock"] or {}
            kind = lock.get("longest_kind")
            if kind:
                print(f"{name} on {args.filesystem}: longest lock hold {kind} "
                      f"{lock['hold'][kind]['longest_ms']} ms, longest wait "
                      f"{lock['wait']['longest_ms']} ms", flush=True)
        found = [(name, budget, figure)]
        if name == "create":
            # memory_create: the peak through its history's build, so the
            # builder's trees are in it.
            count = figure.get("count", 0)
            measured = extra is not None and figure.get("drain_s") is not None and count > 0
            found.append(("memory_create", budgets["memory_create"],
                          {"finished": figure["finished"] and measured,
                           "kib_per_file": round(extra / count, 2) if measured else None,
                           "peak_anon_mib": mib(peak)}))
        if name == "close_1g":
            # write_1g: the same run's writes, held to its own bar.
            derived = budgets["write_1g"]
            found.append(("write_1g", derived,
                          {"finished": figure["finished"], **figure.get("write_1g", {})}))
            # history_1g and memory_1g: its build, held to theirs.
            drained = figure.get("drain_s")
            found.append(("history_1g", budgets["history_1g"],
                          {"finished": figure["finished"] and drained is not None,
                           "drain_s": drained}))
            found.append(("memory_1g", budgets["memory_1g"],
                          {"finished": figure["finished"] and extra is not None,
                           "peak_extra_mib": mib(extra), "peak_anon_mib": mib(peak)}))
        for shown, held, value in found:
            warn = missed(value, held, args.filesystem)
            workloads.append({**entry, "name": shown, "budget": held["text"], **value, "warn": warn})
            print(f"{shown} on {args.filesystem}: {json.dumps(value)}", flush=True)
            if warn:
                report_miss(shown, held, value, args.filesystem)

    report = {"build": args.build, "filesystem": args.filesystem, "image": args.image,
              "host": host(), "connection_aborted": aborted, "workloads": workloads}
    if baseline_report is not None:
        report = {**baseline_report,
                  "connection_aborted": baseline_report.get("connection_aborted", False) or aborted,
                  "workloads": baseline_report["workloads"] + workloads}
    temporary = args.output + ".tmp"
    with open(temporary, "w") as out:
        json.dump(report, out, indent=2)
        out.write("\n")
    os.replace(temporary, args.output)
    sys.stdout.flush()
    # A worker still blocked in the view must not hold the exit. Only this
    # run's figures decide it: a first run's were judged by that run.
    os._exit(1 if gated_misses(workloads, budgets) else 0)


if __name__ == "__main__":
    main()
