#!/usr/bin/env bash
# Starts an HTTP proxy for the end-to-end tests of proxy settings (design
# §11.1): tinyproxy on 127.0.0.1:3128, in the foreground of a background
# job, allowing CONNECT to every port (no ConnectPort line) and logging
# every request (LogLevel Info) to <dir>/tinyproxy.log.
#
# Usage: ci/proxy-server.sh start <dir>
#        ci/proxy-server.sh stop <dir>
#
# `start` prints PROXY_URL and PROXY_LOG. Needs tinyproxy.

set -euo pipefail

cmd=${1:?start|stop}
dir=$(realpath -m "${2:?state directory}")

case "$cmd" in
stop)
  [ -f "$dir/tinyproxy.pid" ] && kill "$(cat "$dir/tinyproxy.pid")" 2>/dev/null || true
  exit 0
  ;;
start) ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac

command -v tinyproxy >/dev/null || { echo "tinyproxy is not installed" >&2; exit 1; }
mkdir -p "$dir"
: >"$dir/tinyproxy.log"
cat >"$dir/tinyproxy.conf" <<CONF
Port 3128
Listen 127.0.0.1
Timeout 600
MaxClients 100
LogFile "$dir/tinyproxy.log"
LogLevel Info
CONF
tinyproxy -d -c "$dir/tinyproxy.conf" >"$dir/tinyproxy.out" 2>&1 &
echo $! >"$dir/tinyproxy.pid"
# An absolute-form request through the proxy to a closed port: any HTTP
# answer (a 5xx from the proxy) means it is up; 000 means it is not yet.
answer() { curl -s -o /dev/null -w '%{http_code}' -x http://127.0.0.1:3128 http://127.0.0.1:9/ || true; }
for _ in $(seq 50); do
  [ "$(answer)" != 000 ] && break
  sleep 0.1
done
[ "$(answer)" != 000 ] || {
  cat "$dir/tinyproxy.out" "$dir/tinyproxy.log" >&2
  echo "tinyproxy did not start" >&2
  exit 1
}

cat <<OUT
PROXY_URL=http://127.0.0.1:3128
PROXY_LOG=$dir/tinyproxy.log
OUT
