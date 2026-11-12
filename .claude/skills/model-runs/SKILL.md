---
name: model-runs
description: Use before running anything of crates/agentfs-model in a container (its tests, the digest in ci/model-digest.sh, the planted-bug corpus in planted/run.py), before changing that harness, and when a model run is slow, hangs or fails without a test failing.
---

# Running the model

The rules are in `crates/agentfs-model/VERIFYING.md` ("Commands", "Refactoring without changing outcomes"). This skill
says how to run them and why, from runs that went wrong.

## Always through the wrapper

Every model job in a container goes through `ci/model-docker.sh -- COMMAND`, never a hand-written `docker run`.

| It does | Because |
|---|---|
| `TMPDIR=/tmp`, named volumes for `/target`, `/work` and the registry | Every run makes git repositories. On a Docker Desktop bind mount 8 seeds took 320 s against 24 s on the container's `/tmp`: one corpus pass did 1 bug in 19 minutes there. `run.py` and `collect` refuse a host file share (exit 2). |
| `--init` | Without it the job is PID 1 and never reaps orphaned git processes. A corpus run reached 63,472 zombie processes and hung for 2 h 24 min at 0% CPU, with `docker exec` failing. |
| `--pids-limit 4096` | A leak then fails within seconds (exit 4, "hit the container's limit") instead of freezing Docker. A fork loop failed in 5 s. |
| The stall alarm | No result for twice the slowest unit in `planted/budget.json` (or `--stall SECONDS`) kills the job and exits 3, naming the unit in flight. A hang never sits silently. |

Give each copy of the tree (a mutant probe) its own fresh `MODEL_TARGET_VOLUME`. Every copy mounts at `/src`, so two
copies sharing a target look like one tree to cargo, which keeps the first one's build when the second's files are
older than it: two mutant probes once reported the same 170 differing digests because the second ran the first's
binary. Volumes can be renamed for a run (`MODEL_WORK_VOLUME`, `MODEL_TARGET_VOLUME`, `MODEL_CARGO_VOLUME`) when someone watches
a particular one. Copy logs out of `/work` before you finish: the volume is not on the host.

## Fast and full

- `run.py` is fast by default: each test in its own process, scenarios first, stopping at the first test that catches
  the bug. `--full` runs both test targets whole (the old behaviour, for counting every catcher).
- `ci/model-digest.sh check` is fast: scenarios first, stopping at their first difference before any seed runs.
  `check --all` runs everything and lists every difference.

## Which tier a change needs

`VERIFYING.md` ("Refactoring without changing outcomes") holds the deciding test; read the diff against it, not your
sense of the change's risk. The owner judged the whole gate too heavy for a rename (2026-11-11, guidance: "That gate
process seems too heavy for this").

- **Light**, for docs, comments, renames at every site, imports, formatting, unit tests added in `src/**`'s
  `#[cfg(test)]` modules, and digest keys renamed with identical values. One job at a time through the wrapper:
  `run.py --anchors-only --work /work/anchors` (silent, rc 0), `ci/model-digest.sh check` (the scenario digests
  match; a budget-only line is fine), `cargo test --locked -p agentfs-model`, `cargo test --locked -p
  agentfs-conformance`, the safety-net tests, and `run.py --work /work/planted --only <ids>` for each bug whose anchor
  moved (each still caught). Then `cargo fmt --all --check` and clippy.
- **Full**, for any other change to the model's `src/**` or `tests/**` (a condition, a branch, a moved function), to
  the digest's text or the harness, or to a digest value or matrix row: `check --all` and the whole corpus in place of
  `check` and `--only`.

## The budget

`crates/agentfs-model/planted/budget.json` holds measured seconds, like `ci/bench-budget.json`: the corpus pass and its
slowest bug, the scenario digest, the whole digest and the slowest seed. A pass over 1.5 times its budget fails;
`run.py --write` and `ci/model-digest.sh record` measure again. Measure alone: the wrapper refuses a budget write
(exit 5) while another `agentfs-dev` container runs, because another job inflated times; `MODEL_BUDGET_CONTENDED=1`
writes them with a note saying so. Run `docker ps` first. Report the measured time, not an estimate.

## Changing the harness

Any change to `run.py`, `ci/model-digest.sh`, `ci/model-docker.sh`, `mounts.py`, `digest.rs` or the checks' names:

1. The quick tier first: `python3 -m unittest discover -s crates/agentfs-model/planted -p 'test_*.py'`, the pin test
   (`cargo test -p agentfs-model --lib digest`), three bugs (`run.py --only a,b,c` on a fresh `--work`) and the scenario
   digests (`ci/model-digest.sh check`, which stops after the scenarios on a difference).
2. A review of the change.
3. Only then a full run (`check --all`, the whole corpus).

A long run that is piped through `tee` or `tail` reports the pipe's exit status: run it unpiped, or read
`${PIPESTATUS[0]}`.
