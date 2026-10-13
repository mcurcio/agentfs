#!/usr/bin/env bash
# Starts a git smart-HTTP server for the end-to-end tests: nginx in front of
# `git http-backend` (through fcgiwrap), with HTTP basic auth, on
#   http://localhost:8080/git/<repo>.git
#   https://localhost:8443/git/<repo>.git  (certificate for "localhost" only)
#   https://localhost:8444/              (self-signed, untrusted: refuses all)
#
# Usage: ci/git-http-server.sh start <dir> [--trust-ca]
#        ci/git-http-server.sh stop <dir>
#
# `start` prints environment assignments for the test scripts:
#   GIT_SERVER_REPOS, GIT_SERVER_HTPASSWD, GIT_HTTP_BASE, GIT_HTTPS_BASE,
#   GIT_HTTPS_UNTRUSTED_BASE (a self-signed certificate nothing trusts),
#   GIT_SERVER_ACCESS_LOG.
# Every request is logged to logs/access.log as
# `$msec $server_port $remote_user "$request" $status $request_time` ($msec is when
# the request ended, so it started at $msec - $request_time), for ordering,
# per-host credential and concurrency checks.
# --trust-ca installs the throwaway CA into the system trust store (sudo),
# which is where libgit2's OpenSSL backend looks.
#
# GIT_SERVER_EXTRA_LISTEN=<ip> also serves plain http on <ip>:8080, for
# clients that cannot reach the runner's loopback (pods in a kind cluster
# reach the kind network's gateway). 127.0.0.1 is always served.
#
# GIT_SERVER_LFS=1 adds Git LFS routes for ci/lfs-server.sh (design §7):
#   http://localhost:8080/git/<repo>.git/info/lfs/...   the derived endpoint,
#     and /git/<repo>/objects/... its object hrefs, proxied to 127.0.0.1:9002
#     under the git server's own users (htpasswd);
#   http://127.0.0.1:8081/...  a separate LFS host with its own users
#     (htpasswd-lfs), proxied to 127.0.0.1:9001;
# and prints GIT_SERVER_LFS_HTPASSWD and LFS_SEPARATE_BASE. The LFS host
# also logs every request to logs/lfs-range.log as
# `$msec $server_port $status "$http_range" $body_bytes_sent "$request" $request_time`
# (GIT_SERVER_LFS_RANGE_LOG), and includes the file GIT_SERVER_LFS_FAULTS
# (empty at start) ahead of its own location, so a test can add regex
# locations that throttle or fail chosen requests (`lfs_faults` in
# ci/lib.sh).
#
# Needs nginx, fcgiwrap, git and openssl. Everything runs as the calling
# user except --trust-ca.

set -euo pipefail

cmd=${1:?start|stop}
dir=$(realpath -m "${2:?state directory}")

case "$cmd" in
stop)
  [ -f "$dir/nginx.pid" ] && kill "$(cat "$dir/nginx.pid")" 2>/dev/null || true
  [ -f "$dir/fcgiwrap.pid" ] && kill "$(cat "$dir/fcgiwrap.pid")" 2>/dev/null || true
  exit 0
  ;;
start) ;;
*) echo "unknown command $cmd" >&2; exit 2 ;;
esac

mkdir -p "$dir"/{repos,tmp,logs,tls}
: >"$dir/htpasswd"

# A throwaway CA and a server certificate valid for "localhost" only, so a
# request to 127.0.0.1 fails verification.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=agentfs CI CA" \
  -keyout "$dir/tls/ca.key" -out "$dir/tls/ca.crt" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout "$dir/tls/server.key" -out "$dir/tls/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\n' >"$dir/tls/san.ext"
openssl x509 -req -in "$dir/tls/server.csr" -CA "$dir/tls/ca.crt" \
  -CAkey "$dir/tls/ca.key" -CAcreateserial -days 2 -extfile "$dir/tls/san.ext" \
  -out "$dir/tls/server.crt" 2>/dev/null
# A self-signed certificate for "localhost" that no store trusts, served on
# 8444: a certificate that fails verification for a reason other than the
# host name.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost" \
  -keyout "$dir/tls/untrusted.key" -out "$dir/tls/untrusted.crt" 2>/dev/null

if [ "${3:-}" = "--trust-ca" ]; then
  sudo cp "$dir/tls/ca.crt" /usr/local/share/ca-certificates/agentfs-ci.crt
  sudo update-ca-certificates >/dev/null
fi

extra_listen=""
if [ -n "${GIT_SERVER_EXTRA_LISTEN:-}" ]; then
  extra_listen="listen ${GIT_SERVER_EXTRA_LISTEN}:8080;"
fi

# Every request, timed (for ordering, per-host credential and concurrency
# checks): $msec is when it ended, so it started at $msec - $request_time.
log_format="log_format agentfs '\$msec \$server_port \$remote_user \"\$request\" \$status \$request_time';"
access_log="access_log $dir/logs/access.log agentfs;"
lfs_locations=""
lfs_server=""
if [ "${GIT_SERVER_LFS:-}" = 1 ]; then
  : >"$dir/htpasswd-lfs"
  : >"$dir/lfs-faults.conf"
  log_format="$log_format
  log_format lfsrange '\$msec \$server_port \$status \"\$http_range\" \$body_bytes_sent \"\$request\" \$request_time';"
  lfs_locations="
    location ~ ^/git/([^/]+)\\.git/info/lfs/(.*)\$ {
      auth_basic \"git\";
      auth_basic_user_file $dir/htpasswd;
      proxy_pass http://127.0.0.1:9002/git/\$1/\$2;
    }
    location ~ ^/git/[^/.]+/objects/ {
      auth_basic \"git\";
      auth_basic_user_file $dir/htpasswd;
      proxy_pass http://127.0.0.1:9002;
    }"
  lfs_server="
  server {
    listen 127.0.0.1:8081;
    server_name 127.0.0.1;
    access_log $dir/logs/access.log agentfs;
    access_log $dir/logs/lfs-range.log lfsrange;
    include $dir/lfs-faults.conf;
    location / {
      auth_basic \"lfs\";
      auth_basic_user_file $dir/htpasswd-lfs;
      proxy_pass http://127.0.0.1:9001;
    }
  }"
fi

fcgiwrap -s "unix:$dir/fcgiwrap.sock" &
echo $! >"$dir/fcgiwrap.pid"
for _ in $(seq 50); do [ -S "$dir/fcgiwrap.sock" ] && break; sleep 0.1; done

backend=$(git --exec-path)/git-http-backend

cat >"$dir/nginx.conf" <<EOF
worker_processes 1;
pid $dir/nginx.pid;
error_log $dir/logs/error.log;
events { worker_connections 64; }
http {
  $log_format
  $access_log
  client_body_temp_path $dir/tmp/body;
  proxy_temp_path $dir/tmp/proxy;
  fastcgi_temp_path $dir/tmp/fastcgi;
  uwsgi_temp_path $dir/tmp/uwsgi;
  scgi_temp_path $dir/tmp/scgi;
  client_max_body_size 0;

  server {
    listen 127.0.0.1:8080;
    $extra_listen
    listen 127.0.0.1:8443 ssl;
    server_name localhost;
    ssl_certificate $dir/tls/server.crt;
    ssl_certificate_key $dir/tls/server.key;
$lfs_locations

    location ~ ^/git(/.*)\$ {
      auth_basic "git";
      auth_basic_user_file $dir/htpasswd;
      fastcgi_pass unix:$dir/fcgiwrap.sock;
      include /etc/nginx/fastcgi_params;
      fastcgi_param SCRIPT_FILENAME $backend;
      fastcgi_param GIT_PROJECT_ROOT $dir/repos;
      fastcgi_param GIT_HTTP_EXPORT_ALL "";
      fastcgi_param PATH_INFO \$1;
      fastcgi_param REMOTE_USER \$remote_user;
    }
  }

  server {
    listen 127.0.0.1:8444 ssl;
    server_name localhost;
    ssl_certificate $dir/tls/untrusted.crt;
    ssl_certificate_key $dir/tls/untrusted.key;
    location / { return 403; }
  }
$lfs_server
}
EOF
nginx -p "$dir" -c "$dir/nginx.conf"
for _ in $(seq 50); do
  curl -s -o /dev/null http://localhost:8080/ && break
  sleep 0.1
done

cat <<EOF
GIT_SERVER_DIR=$dir
GIT_SERVER_REPOS=$dir/repos
GIT_SERVER_HTPASSWD=$dir/htpasswd
GIT_HTTP_BASE=http://localhost:8080/git
GIT_HTTPS_BASE=https://localhost:8443/git
GIT_HTTPS_UNTRUSTED_BASE=https://localhost:8444/git
GIT_SERVER_ACCESS_LOG=$dir/logs/access.log
EOF
if [ "${GIT_SERVER_LFS:-}" = 1 ]; then
  cat <<EOF
GIT_SERVER_LFS_HTPASSWD=$dir/htpasswd-lfs
LFS_SEPARATE_BASE=http://127.0.0.1:8081
GIT_SERVER_LFS_FAULTS=$dir/lfs-faults.conf
GIT_SERVER_LFS_RANGE_LOG=$dir/logs/lfs-range.log
EOF
fi
