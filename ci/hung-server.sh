#!/usr/bin/env bash
# A TCP server that accepts every connection and never answers: a git
# remote that hangs mid-request (design §11.2, "Network timeouts"). Each
# connection is held open by a child that sleeps for an hour.
#
# Usage: ci/hung-server.sh start <port> <pidfile>
#        ci/hung-server.sh stop <pidfile>
#
# Needs socat.

set -euo pipefail

case "${1:?start|stop}" in
start)
  port=${2:?port}
  pidfile=${3:?pidfile}
  socat "TCP-LISTEN:$port,bind=127.0.0.1,reuseaddr,fork" SYSTEM:'sleep 3600' \
    </dev/null >/dev/null 2>&1 &
  echo $! >"$pidfile"
  for _ in $(seq 50); do
    # bash's /dev/tcp: the connect succeeds once socat listens.
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then exit 0; fi
    sleep 0.1
  done
  echo "hung-server: nothing listens on $port" >&2
  exit 1
  ;;
stop)
  pidfile=${2:?pidfile}
  [ -f "$pidfile" ] || exit 0
  pid=$(cat "$pidfile")
  pkill -P "$pid" 2>/dev/null || true
  kill "$pid" 2>/dev/null || true
  ;;
*)
  echo "unknown command $1" >&2
  exit 2
  ;;
esac
