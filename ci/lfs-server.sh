#!/usr/bin/env bash
# Starts Git LFS servers for the end-to-end tests of design §7: git-lfs's
# reference lfs-test-server, three instances, behind the nginx of
# ci/git-http-server.sh (started with GIT_SERVER_LFS=1), which does the
# authentication:
#   separate  127.0.0.1:9001, served as http://127.0.0.1:8081 (the separate
#             LFS host, its own users)
#   same      127.0.0.1:9002, served under http://localhost:8080/git/ (the
#             endpoint derived from a git remote's URL, the git users)
#   tls       https://localhost:8445 directly, with the git server's
#             certificate for "localhost" (trusted with --trust-ca),
#             anonymous
#
# Usage: ci/lfs-server.sh start <dir>
#        ci/lfs-server.sh stop <dir>
#
# Needs LFS_TEST_SERVER (the binary: `go install
# github.com/git-lfs/lfs-test-server@<commit>`) and GIT_SERVER_DIR from
# ci/git-http-server.sh. `start` prints LFS_SERVER_DIR and LFS_HTTPS_BASE.

set -euo pipefail

cmd=${1:?start|stop}
dir=$(realpath -m "${2:?state directory}")

case "$cmd" in
stop)
  for pid in "$dir"/*.pid; do
    [ -f "$pid" ] && kill "$(cat "$pid")" 2>/dev/null || true
  done
  exit 0
  ;;
start) ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac

: "${LFS_TEST_SERVER:?the lfs-test-server binary}" "${GIT_SERVER_DIR:?start ci/git-http-server.sh first}"
mkdir -p "$dir"

start_one() { # name listen host scheme [cert key]
  local name=$1
  mkdir -p "$dir/$name"
  env LFS_LISTEN="tcp://$2" LFS_HOST="$3" LFS_SCHEME="$4" \
    LFS_CERT="${5:-}" LFS_KEY="${6:-}" \
    LFS_PUBLIC=true LFS_ADMINUSER=admin LFS_ADMINPASS=admin \
    LFS_CONTENTPATH="$dir/$name/content" LFS_METADB="$dir/$name/lfs.db" \
    "$LFS_TEST_SERVER" >"$dir/$name.log" 2>&1 &
  echo $! >"$dir/$name.pid"
}

start_one separate 127.0.0.1:9001 127.0.0.1:8081 http
start_one same 127.0.0.1:9002 localhost:8080 http
start_one tls 127.0.0.1:8445 localhost:8445 https \
  "$GIT_SERVER_DIR/tls/server.crt" "$GIT_SERVER_DIR/tls/server.key"

for url in http://127.0.0.1:9001/ http://127.0.0.1:9002/ https://localhost:8445/; do
  for _ in $(seq 100); do
    curl -s -o /dev/null "$url" && break
    sleep 0.1
  done
  curl -s -o /dev/null "$url" || { cat "$dir"/*.log >&2; echo "no LFS server at $url" >&2; exit 1; }
done

cat <<EOF
LFS_SERVER_DIR=$dir
LFS_HTTPS_BASE=https://localhost:8445
EOF
