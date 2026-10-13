#!/usr/bin/env bash
# Prints the data points of one metric from the collector's file output
# (ci/otelcol.yaml: OTLP JSON, one export per line), in the order they
# arrived, one per line: the value (a sum or gauge value, or a
# histogram's count), then its attributes as `key=value` joined by `,`.
#
#   ci/otel-points.sh FILE METRIC
#
# Needs jq.

set -euo pipefail

file=${1:?usage: ci/otel-points.sh FILE METRIC}
metric=${2:?usage: ci/otel-points.sh FILE METRIC}
jq -r --arg name "$metric" '
  .resourceMetrics[]?.scopeMetrics[]?.metrics[]?
  | select(.name == $name)
  | (.gauge // .sum // .histogram).dataPoints[]?
  | [ (.asInt // .asDouble // .count // "" | tostring),
      ([.attributes[]? | "\(.key)=\(.value | (.stringValue // .intValue // .boolValue // .doubleValue) | tostring)"]
       | join(",")) ]
  | join(" ")' "$file"
