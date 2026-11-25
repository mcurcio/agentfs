#!/usr/bin/env bash
# Checks what the agentfs binary and the library crate `agentfs` link.
# The binary, per feature set: the default build is pure Rust apart from
# libgit2 (no OpenSSL, no ring, no aws-lc); `otlp-tls` adds ring and rustls
# only; `ssh` adds russh with ring and never OpenSSL or libssh2; only `https`
# brings OpenSSL. The library: no recording engine, no FUSE, no
# OpenTelemetry SDK or exporter, no TLS and no libssh2, and no crate of the
# workspace with a test build's feature (test-util, failpoints) turned on.
# The recording engine is refused here, not by the ranks: its rank is the
# tree engine's (docs/target-architecture/future/crate-split.md, "Traps").
# The ranks refuse the FUSE layer.
set -euo pipefail

packages() { # cargo-tree-args...
  cargo tree --locked -p agentfs-cli -e normal --prefix none --format '{p}' "$@" |
    awk '{print $1}' | sort -u
}

library() {
  cargo tree --locked -p agentfs -e normal --prefix none --format '{p}' | awk '{print $1}' | sort -u
}

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

check() { # label forbidden-regex required-regex cargo-tree-args...
  local label=$1 forbidden=$2 required=$3
  shift 3
  local all
  all=$(packages "$@")
  local found
  found=$(grep -Ex "$forbidden" <<<"$all" || true)
  [ -z "$found" ] || fail "$label links $(echo "$found" | tr '\n' ' ')"
  if [ -n "$required" ]; then
    grep -Eqx "$required" <<<"$all" || fail "$label lacks $required"
  fi
  echo "ok   $label"
}

NO_TLS='ring|rustls|russh|aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls'
check "default build" "$NO_TLS" ""
check "otlp-tls" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls' 'ring' --features otlp-tls
check "otlp-tls" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls' 'rustls' --features otlp-tls
check "ssh" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls|libssh2-sys' 'russh' --features ssh
check "ssh" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls|libssh2-sys' 'ring' --features ssh
check "image features" 'aws-lc-rs|aws-lc-sys' 'ring' --features https,ssh,otlp-tls

all=$(library)
found=$(grep -Ex "agentfs-core|fuser|opentelemetry_sdk|opentelemetry-otlp|$NO_TLS|libssh2-sys" <<<"$all" || true)
[ -z "$found" ] || fail "library links $(echo "$found" | tr '\n' ' ')"
echo "ok   library"

features=$(cargo tree --locked -p agentfs -e normal,features --prefix none --format '{p}')
found=$(grep -E '^agentfs-[a-z-]+ feature "(test-util|failpoints)"' <<<"$features" | sort -u || true)
[ -z "$found" ] || fail "library turns on $(echo "$found" | tr '\n' ' ')"
echo "ok   library features"
