# syntax=docker/dockerfile:1
#
# A minimal git smart-HTTP server: nginx with basic auth in front of
# `git http-backend` through fcgiwrap, serving /srv/git/<repo>.git at
# http://<host>:8080/git/<repo>.git. The remote of the Docker examples
# (compose.remote.yaml, docs/docker.md) and of the CSI chart's `server:`
# values (docs/helm.md), published as ghcr.io/mcurcio/agentfs-git-server.
# A git remote, not a forge: no web UI, hooks or per-ref permissions.
# It runs as root (dropping to www-data), or as any user with a read-only
# root filesystem and a writable /run/git-server (entrypoint.sh).

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
RUN apt-get update \
 && apt-get install -y --no-install-recommends git nginx fcgiwrap openssl \
 && rm -rf /var/lib/apt/lists/*
COPY nginx.conf /etc/git-server/nginx.conf
COPY entrypoint.sh /usr/local/bin/git-server
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/git-server"]
