#!/usr/bin/env bash
# End-to-end test of the OTLP transports (design §13) against a real
# OpenTelemetry Collector: metrics, a span and an alarm log arrive over
# http/protobuf, http/json and gRPC, each from its own service name, with
# OTEL_EXPORTER_OTLP_HEADERS sent along; with E2E_OTLP_TLS=1 (a binary
# built with --features otlp-tls) also over TLS with a CA bundle and a
# bearer token, and over mutual TLS, and a collector that refuses (a wrong
# token, no client certificate, a CA the system store does not have) gets
# nothing while the command still succeeds.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, the
# collector on localhost (4317-4322 as that file says) and, for TLS,
# OTEL_CERTS: the directory ci/otel-certs.sh wrote and the collector uses.

. "$(dirname "$0")/lib.sh"

: "${OTEL_OUT:?path of the collector output file}"

unset OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_PROTOCOL OTEL_SERVICE_NAME
export OTEL_METRIC_EXPORT_INTERVAL=500

new_volume transports
EXPORTS=0

arrived() { # service-name needle: one export from service-name carries needle
  grep -F "\"$1\"" "$OTEL_OUT" | grep -qF "$2"
}

record_with() { # service-name env-assignments...: records one file and a refused path
  local name=$1
  shift
  EXPORTS=$((EXPORTS + 1))
  echo "$name" >"$TREE/f$EXPORTS.txt"
  ln -sf elsewhere "$TREE/.gitmodules"
  env OTEL_SERVICE_NAME="$name" "$@" \
    "$AGENTFS_BIN" --state "$STATE" --root "$TREE" record "f$EXPORTS.txt" .gitmodules \
    >"$VOL/$name.out" 2>"$VOL/$name.err" || true
  grep -q "1 record(s) into 1 commit(s)" "$VOL/$name.out" || {
    cat "$VOL/$name.out" "$VOL/$name.err"
    fail "$name: the record failed"
  }
}

export_over() { # label service-name env-assignments...
  local label=$1 name=$2
  shift 2
  step "export over $label"
  record_with "$name" "$@"
  if grep -q "warning: telemetry" "$VOL/$name.err"; then
    cat "$VOL/$name.err"
    fail "$label: telemetry warned"
  fi
  wait_for 20 arrived "$name" '"agentfs.record.duration"' || fail "$label: metrics never arrived"
  pass "$label: metrics"
  wait_for 20 arrived "$name" '"name":"agentfs.record"' || fail "$label: the record span never arrived"
  pass "$label: the record span"
  wait_for 20 arrived "$name" 'agentfs.path_refused' || fail "$label: the path_refused log never arrived"
  pass "$label: the path_refused alarm log"
}

refused_over() { # label service-name env-assignments...
  local label=$1 name=$2
  shift 2
  step "refused: $label"
  record_with "$name" "$@"
  grep -q "export failed" "$VOL/$name.err" || {
    cat "$VOL/$name.err"
    fail "$label: no export failure was reported"
  }
  pass "$label: the failure is reported, the command succeeded"
  sleep 2
  if grep -qF "\"$name\"" "$OTEL_OUT"; then fail "$label: the collector got data"; fi
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
