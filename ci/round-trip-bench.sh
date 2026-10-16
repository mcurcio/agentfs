#!/usr/bin/env bash
# A front's round trip against the single-process record (design §14.1,
# §19.1 "Front round-trip limit"): runs the history service bench
# (crates/agentfs-core/tests/history_service.rs, `round_trip_cost`, no
# FUSE) RUNS times, takes each run's ratio of the 1-thread record p50
# through a front to the single-process one, and fails when the median
# ratio exceeds LIMIT. One run is too noisy on shared runners to judge
# (1.14 to 1.39); the median of several is not. Each run's table goes to
# stdout and the step summary, the ratios to a notice on the run.
#
#   ci/round-trip-bench.sh

. "$(dirname "$0")/lib.sh"

# Odd, so the median is one run's ratio. A run takes about 7 s on CI after
# the first builds the bench, so five add about 30 s to the job.
RUNS=5
LIMIT=1.45

summary() { [ -z "${GITHUB_STEP_SUMMARY:-}" ] || printf '%s\n' "$*" >>"$GITHUB_STEP_SUMMARY"; }

summary "### History service round trip (passthrough ${AGENTFS_TEST_PASSTHROUGH:-auto} job; no FUSE)"
RATIOS=()
for run in $(seq "$RUNS"); do
  step "round-trip bench, run $run of $RUNS"
  out=$(cargo test --locked --release -p agentfs-core --test history_service \
    -- --ignored --nocapture round_trip_cost)
  printf '%s\n' "$out"
  summary "**Run $run of $RUNS**" "" "$out"
  # "single-process record, 1 thr   p50  799.2 µs ...": p50 is field 6.
  local_p50=$(awk '/^single-process record, 1 thr / { print $6 }' <<<"$out")
  front_p50=$(awk '/^front record, 1 thr / { print $6 }' <<<"$out")
  [ -n "$local_p50" ] && [ -n "$front_p50" ] ||
    fail "run $run: no 1-thread record p50 in the bench output"
  RATIOS+=("$(awk -v f="$front_p50" -v l="$local_p50" 'BEGIN { printf "%.3f", f / l }')")
done

MEDIAN=$(printf '%s\n' "${RATIOS[@]}" | sort -g | sed -n "$(((RUNS + 1) / 2))p")
RESULT="front / single-process record p50, 1 thread, per run: ${RATIOS[*]}; median $MEDIAN (limit $LIMIT)"
echo "$RESULT"
summary "" "$RESULT"
notice_summary "round trip ratio (${AGENTFS_TEST_PASSTHROUGH:-auto})" "$RESULT"
awk -v m="$MEDIAN" -v l="$LIMIT" 'BEGIN { exit !(m <= l) }' ||
  fail "a front's record costs $MEDIAN times the single-process one at the median of $RUNS runs, over the limit of $LIMIT (design §19.1): ${RATIOS[*]}"
pass "round trip: median ratio $MEDIAN within $LIMIT"
