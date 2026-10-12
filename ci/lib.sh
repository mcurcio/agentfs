# Helpers for the end-to-end scripts in ci/. Source, don't run.
#
# AGENTFS_BIN names the binary under test (default: target/debug/agentfs).

set -euo pipefail

AGENTFS_BIN=${AGENTFS_BIN:-$PWD/target/debug/agentfs}

step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mFAIL:\033[0m %s\n' "$*" >&2; exit 1; }
pass() { printf '\033[32mok\033[0m   %s\n' "$*"; }

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

# A fresh volume in a new temporary directory. Sets VOL, STATE and TREE.
new_volume() { # volume-name
  VOL=$(mktemp -d)
  STATE=$VOL/state
  TREE=$VOL/tree
  mkdir -p "$TREE"
  agentfs init --volume "$1" >/dev/null
}

# Runs agentfs against the current volume.
agentfs() {
  AGENTFS_STATE=$STATE AGENTFS_ROOT=$TREE AGENTFS_POD=${AGENTFS_POD:-ci-0} \
    "$AGENTFS_BIN" "$@"
}

# Starts agentfs in the background with output to $1, setting AGENTFS_PID to
# the binary's own PID (so signals reach it, not a subshell).
agentfs_start() { # log args...
  local log=$1; shift
  AGENTFS_STATE=$STATE AGENTFS_ROOT=$TREE AGENTFS_POD=${AGENTFS_POD:-ci-0} \
    "$AGENTFS_BIN" "$@" >"$log" 2>&1 &
  AGENTFS_PID=$!
}

# Sets keys in the volume's history repository config.
volume_config() { # key value
  git --git-dir "$STATE/git" config "$1" "$2"
}

local_head() { # volume-name
  git --git-dir "$STATE/git" rev-parse "refs/heads/agentfs/$1"
}

remote_ref() { # bare-repo ref
  git --git-dir "$1" rev-parse --verify --quiet "$2" || true
}
