#!/usr/bin/env bash
# End-to-end test of the OTLP transports (design §13) against a real
# OpenTelemetry Collector: metrics, a span and an alarm log arrive over
# http/protobuf, http/json and gRPC, each from its own service name, with
# OTEL_EXPORTER_OTLP_HEADERS sent along.
#
# Needs OTEL_OUT, the file the collector from ci/otelcol.yaml writes, and
# the collector listening on localhost:4318 (HTTP) and localhost:4317
# (gRPC).

. "$(dirname "$0")/lib.sh"

: "${OTEL_OUT:?path of the collector output file}"

unset OTEL_EXPORTER_OTLP_ENDPOINT OTEL_EXPORTER_OTLP_PROTOCOL
export OTEL_METRIC_EXPORT_INTERVAL=500

new_volume transports

arrived() { # service-name needle: one export from service-name carries needle
  grep -F "\"$1\"" "$OTEL_OUT" | grep -qF "$2"
}

export_over() { # protocol endpoint service-name
  step "export over $1"
  local n=$((++EXPORTS))
  echo "$1" >"$TREE/f$n.txt"
  ln -sf elsewhere "$TREE/.gitmodules"
  OTEL_EXPORTER_OTLP_PROTOCOL=$1 OTEL_EXPORTER_OTLP_ENDPOINT=$2 OTEL_SERVICE_NAME=$3 \
    OTEL_EXPORTER_OTLP_HEADERS=x-agentfs-ci=transports \
    agentfs record "f$n.txt" .gitmodules >"$VOL/$3.out" 2>"$VOL/$3.err" || true
  if grep -q "warning: telemetry" "$VOL/$3.err"; then
    cat "$VOL/$3.err"
    fail "$1: telemetry warned"
  fi
  wait_for 20 arrived "$3" '"agentfs.record.duration"' || fail "$1: metrics never arrived"
  pass "$1: metrics"
  wait_for 20 arrived "$3" '"name":"agentfs.record"' || fail "$1: the record span never arrived"
  pass "$1: the record span"
  wait_for 20 arrived "$3" 'agentfs.path_refused' || fail "$1: the path_refused log never arrived"
  pass "$1: the path_refused alarm log"
}

EXPORTS=0
export_over http/protobuf http://localhost:4318 agentfs-ci-http-protobuf
export_over http/json http://localhost:4318 agentfs-ci-http-json
export_over grpc http://localhost:4317 agentfs-ci-grpc

step "every OTLP transport passed"
