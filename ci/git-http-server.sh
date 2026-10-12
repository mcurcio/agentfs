#!/usr/bin/env bash
# Starts a git smart-HTTP server for the end-to-end tests: nginx in front of
# `git http-backend` (through fcgiwrap), with HTTP basic auth, on
#   http://localhost:8080/git/<repo>.git
#   https://localhost:8443/git/<repo>.git  (certificate for "localhost" only)
#
# Usage: ci/git-http-server.sh start <dir> [--trust-ca]
#        ci/git-http-server.sh stop <dir>
#
# `start` prints environment assignments for the test scripts:
#   GIT_SERVER_REPOS, GIT_SERVER_HTPASSWD, GIT_HTTP_BASE, GIT_HTTPS_BASE.
# --trust-ca installs the throwaway CA into the system trust store (sudo),
# which is where libgit2's OpenSSL backend looks.
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

if [ "${3:-}" = "--trust-ca" ]; then
  sudo cp "$dir/tls/ca.crt" /usr/local/share/ca-certificates/agentfs-ci.crt
  sudo update-ca-certificates >/dev/null
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
  access_log $dir/logs/access.log;
  client_body_temp_path $dir/tmp/body;
  proxy_temp_path $dir/tmp/proxy;
  fastcgi_temp_path $dir/tmp/fastcgi;
  uwsgi_temp_path $dir/tmp/uwsgi;
  scgi_temp_path $dir/tmp/scgi;
  client_max_body_size 0;

  server {
    listen 127.0.0.1:8080;
    listen 127.0.0.1:8443 ssl;
    server_name localhost;
    ssl_certificate $dir/tls/server.crt;
    ssl_certificate_key $dir/tls/server.key;

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
EOF
