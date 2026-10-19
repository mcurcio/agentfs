#!/usr/bin/env python3
"""Per-close latency of a change boundary (design §14, §19.2 item 2).

Runs N cycles of open, write 4 KiB, close in DIR and prints the close(2)
latency percentiles in microseconds. Through an agentfs mount, close is
where the synchronous step happens (design §3.2): through a front it
includes the round trip to the history service.

    boundary-latency.py [--json] DIR [N] [LABEL] [DIR LABEL]...

Prints one line: `LABEL n=N p50=… p90=… p99=… max=…` (microseconds), or
with --json one JSON object with the fields label, n, p50_us, p90_us,
p99_us, max_us and mean_us. The numbers are indicative only: CI runners
are shared and noisy.

Further DIR LABEL pairs are measured interleaved with the first: each
cycle runs once in every directory in turn, and one line is printed per
directory. Two directories measured one after the other see different
moments of a shared runner, and a burst of noise in one moment decides
their comparison; interleaved, the noise falls on all of them alike.
"""

import json
import os
import sys
import time


def percentile(sorted_values, p):
    if not sorted_values:
        return 0.0
    k = (len(sorted_values) - 1) * p / 100.0
    lo = int(k)
    hi = min(lo + 1, len(sorted_values) - 1)
    return sorted_values[lo] + (sorted_values[hi] - sorted_values[lo]) * (k - lo)


def cycle(directory, i, payload):
    """One open, write 4 KiB, close in `directory`: the close's latency (µs)."""
    path = os.path.join(directory, "f%d" % (i % 50))
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    # New content every time, so every close is a change to record.
    payload[:8] = i.to_bytes(8, "little")
    os.write(fd, payload)
    start = time.perf_counter_ns()
    os.close(fd)
    return (time.perf_counter_ns() - start) / 1000.0


def report(label, n, closes, as_json):
    closes.sort()
    if as_json:
        print(
            json.dumps(
                {
                    "label": label,
                    "n": n,
                    "p50_us": percentile(closes, 50),
                    "p90_us": percentile(closes, 90),
                    "p99_us": percentile(closes, 99),
                    "max_us": closes[-1],
                    "mean_us": sum(closes) / len(closes),
                }
            )
        )
        return
    print(
        "%s n=%d p50=%.0f p90=%.0f p99=%.0f max=%.0f"
        % (
            label,
            n,
            percentile(closes, 50),
            percentile(closes, 90),
            percentile(closes, 99),
            closes[-1],
        )
    )


def main():
    argv = [a for a in sys.argv if a != "--json"]
    as_json = len(argv) != len(sys.argv)
    if len(argv) < 2 or (len(argv) > 4 and len(argv) % 2 != 0):
        print(__doc__, file=sys.stderr)
        return 2
    n = int(argv[2]) if len(argv) > 2 else 1000
    targets = [(argv[1], argv[3] if len(argv) > 3 else argv[1])]
    targets += [(argv[k], argv[k + 1]) for k in range(4, len(argv), 2)]
    for directory, _ in targets:
        os.makedirs(directory, exist_ok=True)
    payload = bytearray(os.urandom(4096))
    closes = [[] for _ in targets]
    for i in range(n):
        for (directory, _), samples in zip(targets, closes):
            samples.append(cycle(directory, i, payload))
    for (_, label), samples in zip(targets, closes):
        report(label, n, samples, as_json)
    return 0


if __name__ == "__main__":
    sys.exit(main())
