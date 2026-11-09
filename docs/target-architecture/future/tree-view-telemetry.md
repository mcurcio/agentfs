# Telemetry export from the tree views

**Proposed, not built.** This page proposes sending the tree views' metrics, traces and alarms to OpenTelemetry. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

`trees mount --metrics-output FILE` writes counters, the journal's length and each actor stage's runs, time and git
children every 100 ms and at unmount. It never takes part in ordering. Nothing is exported to a collector.

## What it proposes

The tree actor publishes events and the Telemetry domain ([telemetry](../domains/telemetry.md)) turns them into
metrics, spans and alarms for an OpenTelemetry collector. Paths are log and span attributes, never metric
labels. Export never blocks a save.
