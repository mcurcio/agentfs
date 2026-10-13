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

# The LFS host of ci/git-http-server.sh (GIT_SERVER_LFS=1, 127.0.0.1:8081)
# with nginx rules for chosen requests: one regex location for $1 (a path
# regex) with the directives $2 (`limit_rate 256k;`, or
# `if ($request_method = PUT) { return 503; }`), authenticated and proxied
# like the host's own location. Print it into `lfs_faults`.
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
  nginx -p "$GIT_SERVER_DIR" -c "$GIT_SERVER_DIR/nginx.conf" -s reload
  local deadline=$((SECONDS + 20))
  until [ "$(curl -s http://127.0.0.1:8081/__faults)" = "$generation" ]; do
    [ $SECONDS -lt $deadline ] || fail "nginx did not load the LFS fault rules"
    sleep 0.2
  done
}
