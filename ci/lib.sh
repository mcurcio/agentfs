# Helpers for the end-to-end scripts in ci/. Source, don't run.
#
# AGENTFS_BIN names the binary under test (default: target/debug/agentfs).

set -euo pipefail

AGENTFS_BIN=${AGENTFS_BIN:-$PWD/target/debug/agentfs}

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() {
  printf '\033[31mFAIL:\033[0m %s\n' "$*" >&2
  # An annotation on the run, readable without the job's log.
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    printf '::error title=%s::%s\n' "$(basename "$0")" "$(printf '%s' "$*" | tr '\n' ' ' | cut -c1-900)"
  fi
  exit 1
}

pass() { printf '\033[32mok\033[0m   %s\n' "$*"; }

# on_exit COMMAND: runs COMMAND when this shell exits, and in no other. Under
# bash 5.2 a forked child (a `( ... ) &` subshell, or `cmd &` before it execs)
# that gets a fatal signal before it resets its traps runs the EXIT trap it
# inherited (fixed upstream in bash53-012): a script's cleanup then runs twice
# at once, and the copy in the child unmounts or kills what the script still
# uses. The trap compares the exiting shell's $BASHPID with the one that
# registered it ($$ is the parent's inside a subshell), so a subshell may
# register its own. `case` keeps $?, so COMMAND still reads the exit status.
# Every ci/ script registers its EXIT cleanup here: docs_reference.rs
# (`ci_exit_traps_run_in_their_own_shell`) refuses a bare `trap ... EXIT`.
on_exit() { # command
  trap "case \$BASHPID in $BASHPID) $1 ;; esac" EXIT
}

# Operator runbooks run as written: the fenced block after
# `<!-- ci: <name> -->` in docs/runbooks/<runbook>.
RUNBOOKS=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/docs/runbooks
runbook_block() { # runbook name
  awk -v m="<!-- ci: $2 -->" '
    { line = $0; sub(/^[ \t]+/, "", line) }
    !found && line == m { found = 1; next }
    found && line ~ /^```/ { if (inside) exit; inside = 1; next }
    inside { print }' "$RUNBOOKS/$1"
}

# Runs that block in this shell, as an operator would paste it.
run_block() { # runbook name
  local code
  code=$(runbook_block "$1" "$2")
  [ -n "$code" ] || fail "no block '$2' in $1"
  printf '\033[2m# %s: %s\n%s\033[0m\n' "$1" "$2" "$code"
  eval "$code"
}

assert_eq() { # expected actual message
  [ "$1" = "$2" ] || fail "$3: expected '$1', got '$2'"
  pass "$3"
}

assert_contains() { # haystack needle message
  case "$1" in *"$2"*) pass "$3" ;; *) fail "$3: '$2' not found in: $1" ;; esac
}

# Waits up to $1 seconds for the command in the remaining arguments to
# succeed.
wait_for() {
  local deadline=$((SECONDS + $1)); shift
  until "$@" >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || return 1
    sleep 0.5
  done
}

# The LFS host of ci/git-http-server.sh (GIT_SERVER_LFS=1, 127.0.0.1:8081)
# with nginx rules for chosen requests: one regex location for $1 (a path
# regex) with the directives $2 (`limit_rate 256k;`, or `return 503;`),
# authenticated and proxied
# like the host's own location. Print it into `lfs_faults`. A `return`
# there answers every method.
lfs_location() { # path-regex directives
  printf 'location ~ %s { %s auth_basic "lfs"; auth_basic_user_file %s; proxy_pass http://127.0.0.1:9001; }\n' \
    "$1" "$2" "$GIT_SERVER_DIR/htpasswd-lfs"
}

# Replaces the LFS host's fault rules with $1 (nginx locations; empty
# clears them), reloads nginx and waits until the new rules answer.
LFS_FAULTS_GENERATION=0
lfs_faults() { # locations
  : "${GIT_SERVER_LFS_FAULTS:?start ci/git-http-server.sh with GIT_SERVER_LFS=1}"
  LFS_FAULTS_GENERATION=$((LFS_FAULTS_GENERATION + 1))
  local generation="$$-$LFS_FAULTS_GENERATION"
  {
    printf '%s\n' "${1:-}"
    printf 'location = /__faults { return 200 "%s"; }\n' "$generation"
  } >"$GIT_SERVER_LFS_FAULTS"
  echo "LFS host fault rules: ${1:-none}" >&2
  nginx -p "$GIT_SERVER_DIR" -c "$GIT_SERVER_DIR/nginx.conf" -s reload
  local deadline=$((SECONDS + 20))
  until [ "$(curl -s http://127.0.0.1:8081/__faults)" = "$generation" ]; do
    [ $SECONDS -lt $deadline ] || fail "nginx did not load the LFS fault rules"
    sleep 0.2
  done
  # The new worker answers, but the old one may still accept connections
  # with the old rules until it exits: wait for it to (worker_processes 1).
  local master
  master=$(cat "$GIT_SERVER_DIR/nginx.pid")
  until [ "$(pgrep -P "$master" | wc -l)" -le 1 ]; do
    [ $SECONDS -lt $deadline ] || fail "nginx's old worker did not exit after the reload"
    sleep 0.1
  done
}
