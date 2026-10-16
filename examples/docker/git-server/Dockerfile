# syntax=docker/dockerfile:1
#
# A throwaway git smart-HTTP server for the Docker examples
# (compose.remote.yaml, docs/docker.md): nginx with basic auth in front of
# `git http-backend` through fcgiwrap, serving /srv/git/<repo>.git at
# http://git-server:8080/git/<repo>.git. Not hardened: for trying the
# example only.

FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
RUN apt-get update \
 && apt-get install -y --no-install-recommends git nginx fcgiwrap openssl \
 && rm -rf /var/lib/apt/lists/*
COPY nginx.conf /etc/git-server/nginx.conf
COPY entrypoint.sh /usr/local/bin/git-server
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/git-server"]
