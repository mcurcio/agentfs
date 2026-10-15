#!/usr/bin/env bash
# Checks what C and crypto the agentfs binary links, per feature set
# (design §17.3): the default build is pure Rust apart from libgit2 (no
# OpenSSL, no ring, no aws-lc); `otlp-tls` adds ring and rustls only; `ssh`
# adds russh with ring and never OpenSSL or libssh2; `history-tls` adds
# rustls with ring (and rcgen) only; only `https` brings OpenSSL.
set -euo pipefail

packages() { # cargo-tree-args...
  cargo tree --locked -p agentfs -e normal --prefix none --format '{p}' "$@" |
    awk '{print $1}' | sort -u
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
check "history-tls" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls' 'rustls' --features history-tls
check "history-tls" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls' 'ring' --features history-tls
check "history-tls" 'aws-lc-rs|aws-lc-sys|openssl|openssl-sys|native-tls' 'rcgen' --features history-tls
check "image features" 'aws-lc-rs|aws-lc-sys' 'ring' --features https,ssh,otlp-tls,history-tls
