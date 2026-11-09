#!/usr/bin/env python3
"""Measure mounted tree recording; invoked by measure-trees.sh, never a budget gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import statistics
import subprocess
import threading
import time


class Report:
    """The report file, whole and parseable from the first moment.

    It is written at once with status RUNNING and replaced atomically at each
    measurement, so a killed or timed-out run leaves what it reached. Samples
    are appended one JSON line at a time to <path>.samples.jsonl and flushed.
    """

    def __init__(self, path, header):
        self.path = Path(path)
        self.samples_path = Path(str(path) + '.samples.jsonl')
        self.data = dict(header)
        self.data['status'] = 'RUNNING'
        self.data.setdefault('measurements', {})
        self.lock = threading.Lock()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.samples = open(self.samples_path, 'w')
        self._write()

    def _write(self):
        temporary = self.path.with_name(self.path.name + '.tmp')
        temporary.write_text(json.dumps(self.data, indent=2, sort_keys=True) + '\n')
        os.replace(temporary, self.path)

    def measured(self, name, value):
        with self.lock:
            self.data['measurements'][name] = value
            self._write()

    def sample(self, name, row):
        with self.lock:
            self.samples.write(json.dumps({'measurement': name, **row}, sort_keys=True) + '\n')
            self.samples.flush()

    def finish(self, error=None):
        with self.lock:
            if error is not None:
                self.data['execution_error'] = error
            self.data['status'] = 'FAILED' if error is not None else 'COMPLETE'
            self._write()
            self.samples.close()


def threads(pid):
    """What each thread of a process is doing, as /proc says: for a report that
    must show where a stalled mount waits."""
    found = []
    for task in sorted(Path('/proc/%d/task' % pid).glob('*')):
        row = {'tid': task.name}
        for name in ('comm', 'wchan', 'syscall'):
            try:
                row[name] = (task / name).read_text().strip()
            except OSError as exc:
                row[name] = 'unreadable: %s' % exc.strerror
        try:
            row['state'] = [l.split(None, 1)[1].strip() for l in (task / 'status').read_text().splitlines() if l.startswith('State:')][0]
        except (OSError, IndexError):
            row['state'] = 'unreadable'
        found.append(row)
    return found


class Ambient:
    """Samples the published metrics while a mode that is not driven by save
    counts runs. Observation only: it decides nothing."""

    def __init__(self, report, metrics, name, interval=30, pid=None):
        self.report, self.metrics, self.name, self.interval, self.pid = report, metrics, name, interval, pid
        self.stop = threading.Event()
        self.started = time.monotonic_ns()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        while True:
            current = read_metrics(self.metrics)
            if current is not None:
                row = {'elapsed_ns': time.monotonic_ns() - self.started, 'metrics': current}
                # The mount rewrites the file every 100 ms, after taking the
                # actor's lock: an old file with a live mount says the actor is held.
                age = time.time() - Path(self.metrics).stat().st_mtime
                row['metrics_file_age_s'] = age
                if age > 60 and self.pid:
                    row['mount_threads'] = threads(self.pid)
                self.report.sample(self.name, row)
            if self.stop.wait(self.interval):
                return

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()


def command(*args):
    return subprocess.run(args, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False).stdout.strip()


def sha256(path):
    h = hashlib.sha256()
    with open(path, 'rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def notrun(reason):
    return {'status': 'NOT_RUN', 'reason': reason}


def distribution(values):
    values = sorted(values)
    # Nearest-rank percentile. All individual syscall samples are retained.
    import math
    return {'sample_count': len(values), 'p50_ns': statistics.median(values),
            'p99_ns': values[math.ceil(.99 * len(values)) - 1], 'samples_ns': values}


def calls(root, count):
    path = root / 'calls'
    fd = os.open(path, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
    samples = []
    started = time.monotonic_ns()
    try:
        for i in range(count):
            t = time.monotonic_ns()
            n = os.pwrite(fd, i.to_bytes(8, 'little'), 0)
            samples.append(time.monotonic_ns() - t)
            if n != 8:
                raise RuntimeError('short pwrite')
    finally:
        os.close(fd)
    return {'status': 'MEASURED', 'operation': 'pwrite 8 bytes at offset 0, one open descriptor',
            'elapsed_ns_including_close': time.monotonic_ns() - started, **distribution(samples)}


def hour(root, wal, duration, interval):
    path = root / 'hour'
    fd = os.open(path, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
    start_size = wal.stat().st_size
    started = time.monotonic_ns()
    deadline = started + int(duration * 1e9)
    count = 0
    try:
        while time.monotonic_ns() < deadline:
            n = os.pwrite(fd, count.to_bytes(8, 'little'), 0)
            if n != 8:
                raise RuntimeError('short pwrite')
            count += 1
            # No promised rate: time spent in calls is additional to the pause.
            remaining = (deadline - time.monotonic_ns()) / 1e9
            if remaining > 0:
                time.sleep(min(interval, remaining))
        elapsed = time.monotonic_ns() - started
        end_size = wal.stat().st_size
    finally:
        os.close(fd)
    return {'status': 'MEASURED' if duration >= 3600 and elapsed >= 3600 * 10**9 else 'SMOKE',
            'definition': 'WAL logical file-size delta while one file stays write-open; excludes open and close records',
            'write_pattern': 'pwrite 8-byte little-endian counter at offset 0; pause after each call',
            'pause_seconds': interval, 'write_count': count, 'bytes_written': count * 8,
            'elapsed_ns': elapsed, 'wal_start_bytes': start_size, 'wal_end_bytes': end_size,
            'wal_delta_bytes': end_size - start_size}


def read_metrics(path):
    if not path or not Path(path).is_file():
        return None
    return json.loads(Path(path).read_text())


METRIC_FIELDS = ('saves', 'save_nanos', 'takes', 'take_nanos', 'landings',
                 'landing_nanos', 'syncs', 'sync_nanos', 'waits', 'wait_nanos', 'queued')


def metrics_delta(before, after):
    if before is None or after is None:
        return {}
    return {key: after[key] - before[key] for key in METRIC_FIELDS
            if key in before and key in after}


def matches(path, size, digest):
    try:
        return path.stat().st_size == size and sha256(path) == digest
    except FileNotFoundError:
        return False


def settled(paths, size, digest, metrics, timeout):
    # Two identical published snapshots, separated by more than one publisher
    # interval, after both views agree. Counters are observation, not control.
    deadline = time.monotonic() + timeout
    previous = None
    stable_since = None
    while time.monotonic() < deadline:
        snapshot = read_metrics(metrics)
        if snapshot is not None and snapshot.get('queued') == 0 and all(
                matches(path, size, digest) for path in paths):
            now = time.monotonic()
            if snapshot != previous:
                previous, stable_since = snapshot, now
            elif now - stable_since >= 0.25:
                return snapshot
        else:
            previous, stable_since = None, None
        time.sleep(0.05)
    return None


def save_and_observe(sandbox, parent, size, timeout, metrics):
    name = 'landing-%d' % size
    child, main = sandbox / name, parent / name
    # A create is its own commit; seed and propagate it outside the window.
    with open(child, 'wb'):
        pass
    before = settled([child, main], 0, hashlib.sha256(b'').hexdigest(), metrics, timeout)
    if before is None:
        return notrun('seed file and empty queue did not converge')
    block = (b'0123456789abcdef' * 63 + b'\n') * 1024
    expected = hashlib.sha256()
    started = time.monotonic_ns()
    fd = os.open(child, os.O_WRONLY | os.O_TRUNC)
    try:
        remaining = size
        while remaining:
            chunk = block[:min(remaining, len(block))]
            expected.update(chunk)
            view = memoryview(chunk)
            while view:
                n = os.write(fd, view)
                if n <= 0:
                    raise RuntimeError('short write')
                view = view[n:]
            remaining -= len(chunk)
    finally:
        os.close(fd)
    saved = time.monotonic_ns()
    after = settled([child, main], size, expected.hexdigest(), metrics, timeout)
    observed = time.monotonic_ns()
    if after is None:
        return {'status': 'PARTIAL', 'reason': 'both views and empty queue did not converge',
                'bytes': size, 'save_open_write_close_ns': saved - started}
    delta = metrics_delta(before, after)
    isolated = delta.get('saves') == 1 and delta.get('landings') == 1
    return {'status': 'MEASURED' if isolated else 'PARTIAL', 'bytes': size, 'samples': 1,
            'seed': 'create empty sandbox file, wait for both views and an empty queue before timing',
            'save_open_write_close_ns': saved - started,
            'convergence_after_close_ns': observed - saved,
            'actor_metric_delta': delta,
            'save_component_ns': delta.get('save_nanos') if isolated else None,
            'take_component_ns': delta.get('landing_nanos') if isolated else None,
            'sync_component_ns': delta.get('sync_nanos') if isolated else None,
            'definition': 'wall time includes calls; convergence includes polling, full hashing and snapshot stability; actor counters include separate landing and sync work'}


def queue_progress(sandbox, parent, metrics, timeout):
    # One process performs a finite sequence; there are no cooperating processes
    # that wait on one another. Same-source queue entries retain their order.
    base = b'base\n'
    log = 'measure-queue-log'
    with open(parent / log, 'wb') as f:
        f.write(base)
    before = settled([parent / log, sandbox / log], len(base), hashlib.sha256(base).hexdigest(), metrics, timeout)
    if before is None:
        return notrun('log seed did not converge')
    held = os.open(parent / log, os.O_WRONLY)
    started = time.monotonic_ns()
    new_open_ns = []
    disjoint = []
    try:
        # Deliver the independent stream first: a later entry from the same
        # sandbox must not overtake its own blocked log entry (P1).
        for i in range(5):
            name = 'measure-queue-other-%d' % i
            data = ('independent %d\n' % i).encode()
            t = time.monotonic_ns()
            with open(sandbox / name, 'wb') as f:
                f.write(data)
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline and not matches(parent / name, len(data), hashlib.sha256(data).hexdigest()):
                time.sleep(0.05)
            disjoint.append({'name': name, 'visible_while_held': matches(parent / name, len(data), hashlib.sha256(data).hexdigest()),
                             'elapsed_from_save_start_ns': time.monotonic_ns() - t})
        with open(sandbox / log, 'wb') as f:
            f.write(b'queued log\n')
        waiting_deadline = time.monotonic() + timeout
        waiting = None
        while time.monotonic() < waiting_deadline:
            waiting = read_metrics(metrics)
            if waiting and waiting.get('waits', 0) > before.get('waits', 0) and waiting.get('queued', 0) > 0:
                break
            time.sleep(0.05)
        if not waiting or waiting.get('waits', 0) <= before.get('waits', 0) or waiting.get('queued', 0) == 0:
            return {'status': 'PARTIAL', 'reason': 'waiting queue entry not observed', 'disjoint_saves': disjoint}
        queued_at = time.monotonic_ns()
        for _ in range(5):
            t = time.monotonic_ns()
            fd = os.open(parent / log, os.O_WRONLY)
            try:
                if os.pwrite(fd, base, 0) != len(base):
                    raise RuntimeError('short log write')
            finally:
                os.close(fd)
            new_open_ns.append(time.monotonic_ns() - t)
        before_release = read_metrics(metrics)
    finally:
        os.close(held)
    released = time.monotonic_ns()
    content = b'queued log\n'
    after = settled([parent / log, sandbox / log], len(content), hashlib.sha256(content).hexdigest(), metrics, timeout)
    end = time.monotonic_ns()
    return {'status': 'MEASURED' if after is not None and all(x['visible_while_held'] for x in disjoint) else 'PARTIAL',
            'definition': 'one main log writer stays open; five sandbox saves to other files land, then a sandbox log save waits while five successive additional log opens write the existing bytes; original writer then closes',
            'queued_observed_after_ns': queued_at - started,
            'held_after_queue_observed_ns': released - queued_at,
            'convergence_after_release_ns': end - released if after else None,
            'disjoint_saves': disjoint, 'additional_open_write_close': distribution(new_open_ns),
            'actor_delta_before_release': metrics_delta(before, before_release),
            'actor_delta_after_convergence': metrics_delta(before, after),
            'scope': 'finite workload; does not establish a global starvation bound'}

def git(repo, *args, input_data=None):
    result = subprocess.run(['git', '--git-dir', str(repo), *args], input=input_data,
                            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
    return result.stdout


def disk_usage(path):
    logical = allocated = 0
    for directory, _, files in os.walk(path):
        for name in files:
            st = os.stat(os.path.join(directory, name))
            logical += st.st_size
            allocated += st.st_blocks * 512
    return {'logical_file_bytes': logical, 'allocated_bytes': allocated}


def await_save_count(metrics, target, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        current = read_metrics(metrics)
        if current is not None and current.get('saves', -1) >= target:
            return current
        time.sleep(0.05)
    raise RuntimeError('metrics did not reach %d saves before timeout' % target)


def line_map_storage(root, state, count, metrics, timeout, report, name, sample_every):
    repo = state / 'history.git'
    # Create is a separate commit. Wait for its published count before the window.
    deadline = time.monotonic() + timeout
    initial = read_metrics(metrics)
    while initial is None and time.monotonic() < deadline:
        time.sleep(0.05)
        initial = read_metrics(metrics)
    if initial is None or 'saves' not in initial:
        raise RuntimeError('save metrics unavailable')
    with open(root / 'line-map-measure', 'wb'):
        pass
    baseline = await_save_count(metrics, initial['saves'] + 1, timeout)
    before_head = git(repo, 'rev-parse', 'refs/heads/main').strip()
    before = disk_usage(repo)
    # Git's files format: the refs are a copy under refs/ (git-model §3).
    before_refs = disk_usage(repo / 'refs')
    started = time.monotonic_ns()
    reached = 0
    try:
        for i in range(count):
            # Each close saves a distinct one-line version, including deleted history.
            with open(root / 'line-map-measure', 'wb', buffering=0) as f:
                data = ('%08d\n' % i).encode()
                if f.write(data) != len(data):
                    raise RuntimeError('short save write')
            if (i + 1) % sample_every == 0:
                # FUSE release can finish after close returns: wait for the
                # actor, then record where it is.
                row = await_save_count(metrics, baseline['saves'] + i + 1, timeout)
                reached = i + 1
                report.sample(name, {'saves': reached, 'elapsed_ns': time.monotonic_ns() - started,
                                     'metrics': row})
        # The last saves, when count is not a multiple of the sample spacing.
        final = await_save_count(metrics, baseline['saves'] + count, timeout)
    except Exception as exc:
        report.measured(name, {'status': 'PARTIAL', 'reason': '%s: %s' % (type(exc).__name__, exc),
                               'saves_requested': count, 'saves_reached_at_last_sample': reached,
                               'elapsed_ns': time.monotonic_ns() - started})
        raise
    elapsed = time.monotonic_ns() - started
    if final['saves'] != baseline['saves'] + count:
        raise RuntimeError('unexpected number of save events')
    # Git's refs are a copy made after the save is on record: read them once
    # the last save's copy has landed.
    deadline = time.monotonic() + timeout
    while True:
        after_head = git(repo, 'rev-parse', 'refs/heads/main').strip()
        commits = git(repo, 'rev-list', after_head, '^' + before_head).splitlines()
        if len(commits) >= count or time.monotonic() > deadline:
            break
        time.sleep(0.05)
    if len(commits) != count:
        raise RuntimeError('expected %d save commits, found %d' % (count, len(commits)))
    # Metadata is an envelope blob containing line maps and object/file IDs.
    expressions = ''.join(c + ':metadata.json\n' for c in commits)
    records = git(repo, 'cat-file', '--batch-check=%(objectname) %(objecttype) %(objectsize) %(objectsize:disk)',
                  input_data=expressions).splitlines()
    if len(records) != count:
        raise RuntimeError('missing metadata object records')
    objects = {}
    sizes = []
    for record in records:
        fields = record.split()
        if len(fields) != 4 or fields[1] != 'blob':
            raise RuntimeError('invalid metadata object: ' + record)
        oid, _, size, compressed = fields
        sizes.append(int(size))
        objects[oid] = (int(size), int(compressed))
    commit_records = git(repo, 'cat-file', '--batch-check=%(objectname) %(objecttype) %(objectsize) %(objectsize:disk)',
                         input_data=''.join(c + '\n' for c in commits)).splitlines()
    if len(commit_records) != count:
        raise RuntimeError('missing commit object records')
    commit_sizes = [line.split() for line in commit_records]
    if any(len(row) != 4 or row[1] != 'commit' for row in commit_sizes):
        raise RuntimeError('invalid commit object records')
    return {'status': 'MEASURED', 'save_count': count, 'elapsed_ns': elapsed,
            'pattern': 'seed an empty file before the window; truncate and write one distinct 9-byte line, close after each save',
            'metadata_definition': 'metadata.json envelope blobs include line maps and object/file IDs; namespace.json is separate',
            'metadata_snapshot_logical_bytes_sum': sum(sizes), 'final_metadata_logical_bytes': sizes[0],
            'unique_metadata_blob_count': len(objects),
            'unique_metadata_logical_bytes': sum(v[0] for v in objects.values()),
            'unique_metadata_git_object_bytes_on_disk': sum(v[1] for v in objects.values()),
            'commit_object_logical_bytes': sum(int(row[2]) for row in commit_sizes),
            'commit_git_object_bytes_on_disk': sum(int(row[3]) for row in commit_sizes),
            'repository_before': before, 'repository_after': disk_usage(repo),
            'refs_before': before_refs,
            'refs_after': disk_usage(repo / 'refs'),
            'storage_note': 'object disk bytes use git cat-file; packed object attribution may include shared pack costs; repository totals include objects, refs and other files',
            'before_head': before_head, 'after_head': after_head}



def held_log(root, metrics, timeout, report, name, saves=24, size=8 << 20):
    # One log stays write-open while another file takes large saves. The
    # journal grows with every saved byte; a compaction keeps only the held
    # file's calls, so journal_bytes shows whether compaction holds (git-model §3).
    initial = None
    deadline = time.monotonic() + timeout
    while initial is None and time.monotonic() < deadline:
        initial = read_metrics(metrics)
        if initial is None:
            time.sleep(0.05)
    if initial is None:
        raise RuntimeError('save metrics unavailable')
    with open(root / 'held-other', 'wb'):
        pass
    held = os.open(root / 'held-log', os.O_CREAT | os.O_WRONLY, 0o600)
    started = time.monotonic_ns()
    reached = 0
    peak = 0
    try:
        os.write(held, b'one held line\n')
        baseline = await_save_count(metrics, initial['saves'] + 2, timeout)
        block = (b'0123456789abcdef' * 63 + b'\n') * 1024
        for i in range(saves):
            fd = os.open(root / 'held-other', os.O_WRONLY | os.O_TRUNC)
            try:
                # A save that leaves the file's bytes as they were commits nothing
                # (ledger R8.2, finding 36), so each save's bytes must differ or the
                # wait below for its count never ends.
                os.write(fd, b'save %012d\n' % i)
                for _ in range(size // len(block)):
                    os.write(fd, block)
            finally:
                os.close(fd)
            row = await_save_count(metrics, baseline['saves'] + i + 1, timeout)
            reached = i + 1
            peak = max(peak, row.get('journal_bytes', 0))
            report.sample(name, {'saves': reached, 'elapsed_ns': time.monotonic_ns() - started,
                                 'metrics': row})
    except Exception as exc:
        report.measured(name, {'status': 'PARTIAL', 'reason': '%s: %s' % (type(exc).__name__, exc),
                               'saves_requested': saves, 'saves_reached_at_last_sample': reached,
                               'journal_bytes_peak': peak})
        raise
    finally:
        os.close(held)
    final = read_metrics(metrics) or {}
    return {'status': 'MEASURED', 'saves': saves, 'bytes_per_save': size,
            'definition': 'one log stays write-open with one line written; another file is truncated and rewritten with the stated bytes after an 18-byte line that differs per save (R8.2), one close per save; journal_bytes sampled after each save',
            'journal_bytes_peak': peak, 'journal_bytes_final': final.get('journal_bytes'),
            'compactions': final.get('stages', {}).get('compaction', {}).get('count', 0)
            - baseline.get('stages', {}).get('compaction', {}).get('count', 0),
            'elapsed_ns': time.monotonic_ns() - started}


class Terminated(Exception):
    pass


def terminate(signum, _frame):
    raise Terminated('signal %d before the run finished' % signum)


MODES = ['calls', 'hour', 'storage', 'smoke', 'pilot', 'held-log']


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--output', required=True)
    p.add_argument('--mode', choices=MODES, required=True)
    p.add_argument('--reason')
    p.add_argument('--main')
    p.add_argument('--baseline')
    p.add_argument('--state')
    p.add_argument('--sandbox')
    p.add_argument('--metrics')
    p.add_argument('--landing', action='store_true')
    p.add_argument('--visibility-timeout', type=float, default=120)
    p.add_argument('--bin', required=True)
    p.add_argument('--count', type=int)
    p.add_argument('--interval', type=float, default=1.0)
    p.add_argument('--sample-every', type=int, default=100)
    p.add_argument('--mount-pid', type=int)
    a = p.parse_args()
    if a.interval <= 0 or a.visibility_timeout <= 0 or a.sample_every <= 0 or (a.count is not None and a.count <= 0):
        p.error('count, interval, sample-every and visibility-timeout must be positive')
    count = a.count or (1000 if a.mode in ('calls', 'smoke', 'pilot') else 10000)
    reason = a.reason or 'requires landing/line-map measurement instrumentation'
    run_url = None
    if os.environ.get('GITHUB_RUN_ID'):
        run_url = '%s/%s/actions/runs/%s' % (os.environ.get('GITHUB_SERVER_URL', 'https://github.com'),
                                             os.environ.get('GITHUB_REPOSITORY', ''), os.environ['GITHUB_RUN_ID'])
    header = {'schema_version': 2, 'mode': a.mode, 'ci_run': run_url,
              'source_sha': command('git', 'rev-parse', 'HEAD'),
              'source_status_porcelain': command('git', 'status', '--porcelain'),
              'binary_sha256': sha256(a.bin) if os.path.isfile(a.bin) else None,
              'tracked_diff_sha256': hashlib.sha256(subprocess.run(['git', 'diff', 'HEAD', '--binary'], stdout=subprocess.PIPE, check=True).stdout).hexdigest(),
              'kernel': platform.platform(), 'git_version': command('git', '--version'), 'filesystem': command('findmnt', '-T', a.state if a.state and os.path.exists(a.state) else '.', '-o', 'TARGET,SOURCE,FSTYPE,OPTIONS', '-n'),
              'configuration': {'tree_cache': 'FOPEN_DIRECT_IO', 'baseline': 'bindfs -o direct_io',
                                'state_path': a.state, 'baseline_path': a.baseline},
              'measurements': {k: notrun(reason) for k in ['journal_call_cost', 'hour_journal_bytes',
                   'line_map_bytes_per_10000_saves', 'save_take_1mib', 'save_take_100mib', 'queue_wait']}}
    report = Report(a.output, header)
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    error = None
    try:
        if not a.reason:
            if a.mode in ('calls', 'smoke'):
                if a.baseline:
                    with Ambient(report, a.metrics, 'ambient', pid=a.mount_pid):
                        baseline = calls(Path(a.baseline), count)
                        recorded = calls(Path(a.main), count)
                    report.measured('journal_call_cost', {'status': 'MEASURED' if a.mode == 'calls' else 'SMOKE',
                        'definition': 'wall time around pwrite syscall; includes FUSE and recording work, not isolated journal component time',
                        'plain_fuse': baseline, 'recording_fuse': recorded})
                else:
                    report.measured('journal_call_cost', notrun('direct-I/O bindfs baseline unavailable'))
            if a.mode == 'storage':
                report.measured('line_map_bytes_per_10000_saves', line_map_storage(
                    Path(a.main), Path(a.state), count, a.metrics, a.visibility_timeout,
                    report, 'storage_saves', a.sample_every))
            if a.mode == 'pilot':
                report.measured('pilot', line_map_storage(
                    Path(a.main), Path(a.state), count, a.metrics, a.visibility_timeout,
                    report, 'pilot_saves', a.sample_every))
            if a.mode == 'held-log':
                report.measured('held_log', held_log(Path(a.main), a.metrics, a.visibility_timeout,
                                                     report, 'held_log_saves'))
            if a.mode in ('hour', 'smoke'):
                with Ambient(report, a.metrics, 'ambient', pid=a.mount_pid):
                    report.measured('hour_journal_bytes', hour(
                        Path(a.main), Path(a.state) / 'calls.wal',
                        3600 if a.mode == 'hour' else 2, a.interval))
            if a.landing and a.mode == 'calls':
                with Ambient(report, a.metrics, 'ambient', pid=a.mount_pid):
                    for size in (1 << 20, 100 << 20):
                        report.measured('save_take_1mib' if size == 1 << 20 else 'save_take_100mib',
                                        save_and_observe(Path(a.sandbox), Path(a.main), size,
                                                         a.visibility_timeout, a.metrics))
                    report.measured('queue_wait', queue_progress(Path(a.sandbox), Path(a.main),
                                                                 a.metrics, a.visibility_timeout))
    except Exception as exc:
        error = '%s: %s' % (type(exc).__name__, exc)
    report.finish(error)
    return 1 if error else 0


if __name__ == '__main__':
    raise SystemExit(main())
