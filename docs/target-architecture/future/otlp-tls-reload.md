# Reloading OTLP TLS files

**Proposed, not built.** This page proposes rebuilding the exporter when its TLS files change. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

The collector CA bundle, client certificate and key are read once, when the exporter is built. Rotating one (a
cert-manager renewal, say) takes a restart of the daemon ([telemetry](../../telemetry.md)).

## What it proposes

Watch the files and rebuild the exporter, or resolve them per connection, as git credentials are re-read when they
change.
