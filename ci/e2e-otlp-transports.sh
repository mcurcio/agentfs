#!/usr/bin/env bash
# End-to-end test of the OTLP transports against a real OpenTelemetry
# Collector: `agentfs mount ROOT -- true` on a tree volume whose operator
# settings name a remote delivers its `agentfs.not_built` gauge over
# http/protobuf, http/json and gRPC, each from its own service name, with
# OTEL_EXPORTER_OTLP_HEADERS sent along; with E2E_OTLP_TLS=1 (a binary
# built with --features otlp-tls) also over TLS with a CA bundle and a
# bearer token, and over mutual TLS, and a collector that refuses (a wrong
# token, no client certificate, a CA the system store does not have) gets
# nothing while the mount still succeeds.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, the
# collector on localhost (4317-4322 as that file says), /dev/fuse and
# fusermount3 (the mount is --no-allow-other) and, for TLS, OTEL_CERTS: the
# directory ci/otel-certs.sh wrote and the collector uses.

. "$(dirname "$0")/lib.sh"

: "${OTEL_OUT:?path of the collector output file}"

unset OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_PROTOCOL OTEL_SERVICE_NAME
export OTEL_METRIC_EXPORT_INTERVAL=500

WORK=$(mktemp -d)
STATE=$WORK/state
printf '[remote "origin"]\n\turl = https://git.example.com/acme/history.git\n' >"$WORK/settings"
"$AGENTFS_BIN" --state "$STATE" init --volume transports --include "$WORK/settings" >/dev/null
mkdir "$WORK/mnt"

arrived() { # service-name needle: one export from service-name carries needle
  grep -F "\"$1\"" "$OTEL_OUT" | grep -F "$2" >/dev/null
}

mount_with() { # service-name env-assignments...: mounts, runs `true`, drains
  local name=$1
  shift
  env OTEL_SERVICE_NAME="$name" "$@" \
    "$AGENTFS_BIN" --state "$STATE" mount "$WORK/mnt" --no-allow-other -- true \
    >"$WORK/$name.out" 2>"$WORK/$name.err" || {
    cat "$WORK/$name.out" "$WORK/$name.err"
    fail "$name: the mount failed"
  }
}

export_over() { # label service-name env-assignments...
  local label=$1 name=$2
  shift 2
  step "export over $label"
  mount_with "$name" "$@"
  if grep "warning: telemetry" "$WORK/$name.err" >/dev/null; then
    cat "$WORK/$name.err"
    fail "$label: telemetry warned"
  fi
  wait_for 20 arrived "$name" '"agentfs.not_built"' || fail "$label: agentfs.not_built never arrived"
  pass "$label: agentfs.not_built"
}

refused_over() { # label service-name env-assignments...
  local label=$1 name=$2
  shift 2
  step "refused: $label"
  mount_with "$name" "$@"
  grep "export failed" "$WORK/$name.err" >/dev/null || {
    cat "$WORK/$name.err"
    fail "$label: no export failure was reported"
  }
  pass "$label: the failure is reported, the mount succeeded"
  # The collector refused the export the mount reports failed, so it has
  # nothing of it to write.
  if grep -F "\"$name\"" "$OTEL_OUT" >/dev/null; then fail "$label: the collector got data"; fi
  pass "$label: the collector got nothing"
}

HEADERS=OTEL_EXPORTER_OTLP_HEADERS=x-agentfs-ci=transports

export_over http/protobuf agentfs-ci-http-protobuf "$HEADERS" \
  OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export_over http/json agentfs-ci-http-json "$HEADERS" \
  OTEL_EXPORTER_OTLP_PROTOCOL=http/json OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
export_over grpc agentfs-ci-grpc "$HEADERS" \
  OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317

if [ "${E2E_OTLP_TLS:-0}" = 1 ]; then
  : "${OTEL_CERTS:?the directory ci/otel-certs.sh wrote}"
  CA=OTEL_EXPORTER_OTLP_CERTIFICATE=$OTEL_CERTS/ca.pem
  TOKEN="OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20agentfs-ci-token"
  CLIENT_CERT=OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE=$OTEL_CERTS/client.pem
  CLIENT_KEY=OTEL_EXPORTER_OTLP_CLIENT_KEY=$OTEL_CERTS/client.key

  export_over "https, http/protobuf, a bearer token" agentfs-ci-tls-http "$CA" "$TOKEN" \
    OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4319
  export_over "https, grpc, a bearer token" agentfs-ci-tls-grpc "$CA" "$TOKEN" \
    OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4320
  export_over "mutual TLS, http/json" agentfs-ci-mtls-http "$CA" "$CLIENT_CERT" "$CLIENT_KEY" \
    OTEL_EXPORTER_OTLP_PROTOCOL=http/json OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4321
  export_over "mutual TLS, grpc" agentfs-ci-mtls-grpc "$CA" "$CLIENT_CERT" "$CLIENT_KEY" \
    OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4322

  refused_over "a wrong bearer token" agentfs-ci-bad-token "$CA" \
    "OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer%20wrong" \
    OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4319
  refused_over "gRPC without a bearer token" agentfs-ci-no-token "$CA" \
    OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4320
  refused_over "mutual TLS without a client certificate" agentfs-ci-no-client "$CA" \
    OTEL_EXPORTER_OTLP_PROTOCOL=grpc OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4322
  refused_over "a CA only the bundle has, without the bundle" agentfs-ci-no-ca "$TOKEN" \
    OTEL_EXPORTER_OTLP_ENDPOINT=https://localhost:4319
fi

step "every OTLP transport passed"
