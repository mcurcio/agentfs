#!/usr/bin/env python3
"""Markdown report and budget check for ci/bench.sh (docs/performance.md).

    bench-report.py [--budget ci/bench-budget.json] OUT_DIR...

Reads env.json, record.jsonl and results.jsonl from each OUT_DIR and
prints markdown, one section per filesystem: the OUT_DIRs of one
filesystem (named by env.json, else by the directory), such as the
`many` and `rest` shards of the bench workflow, are reported together.
Each section gives the synchronous record step without FUSE,
then each workload through the mount beside its plain-directory baseline,
as medians over the runs. With --budget, each budget the results exceed
is printed as a GitHub `::warning::` line on stderr and listed in the
report. Budgets are advisory: the exit status is 0 unless the input is
missing or unreadable. Standard library only.

A budget file holds {"budgets": [{"match": {...}, "metric": "...",
"max": N, "why": "..."}]}. `match` selects rows by equal fields: record
step rows have suite "record_step", fs, case, size, threads, freeze and
the p50_us … max_us percentiles; workload rows have suite "e2e", fs,
workload, unit, baseline, agentfs (medians of the workload's main
metric), ratio (agentfs / baseline), drain_s, records and commits.
"""

import argparse
import json
import os
import statistics
import sys

# The main metric of each workload: (field, unit). A plain close of a big
# file costs next to nothing, so a ratio against it means nothing: those
# workloads (NO_RATIO) are compared with the cost of a copy instead.
METRICS = {
    "close4k": ("p50_us", "µs p50"),
    "clone": ("seconds", "s"),
    "npm": ("seconds", "s"),
    "create": ("seconds", "s"),
    "mvdir": ("seconds", "s"),
    "rm": ("seconds", "s"),
    "bigclose-dirty": ("close_s", "s close(2)"),
    "bigclose-fsync": ("close_s", "s close(2)"),
}

NO_RATIO = {"bigclose-dirty", "bigclose-fsync"}

ORDER = list(METRICS)


def read_jsonl(path):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def median(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def fmt(value, digits=None):
    if value is None:
        return "–"
    if isinstance(value, float):
        if digits is not None:
            return "%.*f" % (digits, value)
        if value >= 100:
            return "%.0f" % value
        if value >= 10:
            return "%.1f" % value
        return "%.2f" % value
    return str(value)


def workload_rows(results, fs):
    rows = []
    names = sorted(
        {r["workload"] for r in results},
        key=lambda w: (ORDER.index(w) if w in ORDER else len(ORDER), w),
    )
    for name in names:
        field, unit = METRICS.get(name, ("seconds", "s"))
        base = [r for r in results if r["workload"] == name and r["mode"] == "baseline"]
        mine = [r for r in results if r["workload"] == name and r["mode"] == "agentfs"]
        b = median([r.get(field) for r in base])
        a = median([r.get(field) for r in mine])
        row = {
            "suite": "e2e",
            "fs": fs,
            "workload": name,
            "unit": unit,
            "runs": len(mine),
            "baseline": b,
            "agentfs": a,
            "ratio": (a / b) if a is not None and b and name not in NO_RATIO else None,
            "drain_s": median([r.get("drain_s") for r in mine]),
            "records": median([r.get("records") for r in mine]),
            "commits": median([r.get("commits") for r in mine]),
            "files": median([r.get("files") for r in mine]),
            "copy_s": median([r.get("copy_s") for r in base]),
        }
        rows.append(row)
    return rows


def matches(row, match):
    return all(row.get(k) == v for k, v in match.items())


def check(budgets, rows, fss):
    """[(budget, row, value)] for every budget a row exceeds, and
    [(budget, fs)] for every filesystem in fss that has no row a budget
    matches (a budget naming its fs is checked on that fs only, when it is
    reported), so a workload missing from one filesystem is listed."""
    over = []
    unmatched = []
    for budget in budgets:
        match = budget.get("match", {})
        hit = set()
        for row in rows:
            if not matches(row, match):
                continue
            value = row.get(budget["metric"])
            if value is None:
                continue
            hit.add(row["fs"])
            if value > budget["max"]:
                over.append((budget, row, value))
        wanted = [match["fs"]] if "fs" in match else fss
        unmatched.extend((budget, fs) for fs in wanted if fs in fss and fs not in hit)
    return over, unmatched


def describe(row):
    if row["suite"] == "e2e":
        return "%s %s" % (row["fs"], row["workload"])
    return "%s %s" % (row["fs"], row["case"])


def read_dirs(dirs):
    """{fs: {"envs": [...], "record": [...], "results": [...]}}, in the
    order the filesystems first appear."""
    by_fs = {}
    for d in dirs:
        env_path = os.path.join(d, "env.json")
        env = {}
        if os.path.exists(env_path):
            with open(env_path) as f:
                env = json.load(f)
        fs = env.get("fs") or os.path.basename(os.path.normpath(d))
        group = by_fs.setdefault(fs, {"envs": [], "record": [], "results": []})
        group["envs"].append(env)
        group["record"].extend(read_jsonl(os.path.join(d, "record.jsonl")))
        group["results"].extend(read_jsonl(os.path.join(d, "results.jsonl")))
    return by_fs


def describe_env(env):
    shard = env.get("shard")
    return "%s%s on kernel %s, %s CPU(s) (%s), %s MiB; agentfs %s, %s.%s" % (
        "Shard %s: " % shard if shard and shard != "all" else "",
        env.get("fstype", "?"),
        env.get("kernel", "?"),
        env.get("cpus", "?"),
        env.get("cpu", "?"),
        env.get("mem_mib", "?"),
        (env.get("commit") or "?")[:12],
        env.get("git", "git ?"),
        " Quick mode: smaller workloads." if env.get("quick") else "",
    )


def report(dirs, budgets):
    out = []
    all_rows = []
    by_fs = read_dirs(dirs)
    for fs, group in by_fs.items():
        record = group["record"]
        results = group["results"]
        for r in record:
            r["fs"] = fs
        out.append("## Benchmark: %s" % fs)
        out.append("")
        for env in group["envs"]:
            out.append(describe_env(env))
            out.append("")
        out.append("### The synchronous record step, without FUSE (µs)")
        out.append("")
        if record:
            out.append("| Case | Freeze | n | p50 | p90 | p99 | max |")
            out.append("|---|---|---|---|---|---|---|")
            for r in record:
                out.append(
                    "| %s | %s | %d | %s | %s | %s | %s |"
                    % (
                        r["case"],
                        r.get("freeze") or "",
                        r["n"],
                        fmt(r["p50_us"], 0),
                        fmt(r["p90_us"], 0),
                        fmt(r["p99_us"], 0),
                        fmt(r["max_us"], 0),
                    )
                )
        else:
            out.append("No results: the record_step bench did not run.")
        out.append("")
        out.append("### Workloads through the mount (medians)")
        out.append("")
        rows = workload_rows(results, fs)
        if rows:
            out.append(
                "| Workload | Unit | Runs | Plain directory | agentfs | Ratio "
                "| Commit catch-up (s) | Records | Commits |"
            )
            out.append("|---|---|---|---|---|---|---|---|---|")
            for row in rows:
                out.append(
                    "| %s | %s | %d | %s | %s | %s | %s | %s | %s |"
                    % (
                        row["workload"],
                        row["unit"],
                        row["runs"],
                        fmt(row["baseline"]),
                        fmt(row["agentfs"]),
                        fmt(row["ratio"], 2) + "×" if row["ratio"] else "–",
                        fmt(row["drain_s"]),
                        fmt(row["records"]),
                        fmt(row["commits"]),
                    )
                )
            copies = [r for r in rows if r["copy_s"] is not None]
            if copies:
                out.append("")
                out.append(
                    "Copying the big file plus `fsync` in the plain directory (a "
                    "freeze by copy, for comparison): %s s."
                    % ", ".join("%s %s" % (r["workload"], fmt(r["copy_s"])) for r in copies)
                )
        else:
            out.append("No results: the workloads did not run.")
        out.append("")
        all_rows.extend({**r, "suite": "record_step"} for r in record)
        all_rows.extend(rows)

    over, unmatched = check(budgets, all_rows, list(by_fs))
    if budgets:
        out.append("## Budgets (advisory, ci/bench-budget.json)")
        out.append("")
        if over:
            for budget, row, value in over:
                message = "%s: %s = %s over the budget of %s (%s)" % (
                    describe(row),
                    budget["metric"],
                    fmt(float(value)),
                    budget["max"],
                    budget.get("why", ""),
                )
                out.append("- **over**: " + message)
                print("::warning title=bench budget::" + message, file=sys.stderr)
        else:
            out.append("All budgets with a result met.")
        for budget, fs in unmatched:
            out.append(
                "- no result on %s for %s %s" % (fs, json.dumps(budget["match"]), budget["metric"])
            )
        out.append("")
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--budget")
    parser.add_argument("dirs", nargs="+")
    args = parser.parse_args()
    budgets = []
    if args.budget:
        with open(args.budget) as f:
            budgets = json.load(f)["budgets"]
    for d in args.dirs:
        if not os.path.isdir(d):
            print("bench-report: no such directory: %s" % d, file=sys.stderr)
            return 1
    print(report(args.dirs, budgets))
    return 0


if __name__ == "__main__":
    sys.exit(main())
