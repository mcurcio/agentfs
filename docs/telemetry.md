# Telemetry

agentfs exports OpenTelemetry metrics, traces and logs over OTLP,
configured with the standard `OTEL_*` environment variables. With no
endpoint set, telemetry is off and costs nothing. This page lists every metric, span, log
record and alarm event, with its attributes, and how to turn export on.

`mount`, `run`, `record`, `flush`, `push`, `serve-history`, `patch apply`,
`fork finish`, `fork abandon`, and `restore` and `revert` without a mount
export telemetry; a front (`mount` or `record` with `--history-service`)
exports its own. Under the CSI driver, the release scan's `agentfs push`
of each worktree no pod stages exports its own too, so its failures reach
the collector as `PushFailed` with the node plugin's `OTEL_*` settings.

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector:4318
agentfs run
```

- **Variables:** an endpoint enables export, per signal or for all;
  `OTEL_{TRACES,METRICS,LOGS}_EXPORTER=none` or `OTEL_SDK_DISABLED=true`
  turn it off. `OTEL_EXPORTER_OTLP_PROTOCOL` picks **http/protobuf** (the
  default), **http/json** or **grpc** (per signal too). An `https://`
  endpoint needs a binary built with `otlp-tls` (the image is): it
  verifies the collector against `OTEL_EXPORTER_OTLP_CERTIFICATE` or the
  system store, and sends a client certificate for mutual TLS; a bearer
  token goes in `OTEL_EXPORTER_OTLP_HEADERS`. Anything else is reported
  with a `warning: telemetry:` line on stderr and that signal stays off.
  Telemetry problems never stop a command. Every variable agentfs and the
  OpenTelemetry SDK read, including sampling (`OTEL_TRACES_SAMPLER`: on a
  busy volume, sample), is in the
  [configuration reference](configuration.md#environment-variables).
- **Failed exports** (collector down, an HTTP error) print
  `warning: telemetry: <signal> export failed: ...` on stderr, at once and
  then at most once a minute while they keep failing, and a
  `telemetry: <signal> export recovered` line when they work again. Each
  failure is also counted in `agentfs.telemetry.export_failures`.
- **Resource:** `service.name`, `service.version`, `service.instance.id`
  (the replica), `process.pid`, `agentfs.volume`, and `k8s.*` from the pod
  flags (`AGENTFS_NAMESPACE`, `AGENTFS_POD`, `AGENTFS_POD_UID`,
  `AGENTFS_NODE`).
- **Shutdown** flushes telemetry, waiting up to 5s, so a collector that is
  down can add up to 5s to a short command.

## Signals

agentfs exports these signals, all through `agentfs_core::telemetry`,
which uses the OpenTelemetry API only. The binary picks the SDK and the
exporter.

## Metrics

| Metric | Type | Use |
|---|---|---|
| `agentfs.record.duration` | histogram | Cost of the synchronous step, by trigger |
| `agentfs.record.failures` | counter | Recording errors, by trigger |
| `agentfs.journal.backlog` | gauge | Entries recorded but not committed |
| `agentfs.commit.count` | counter | Commits made |
| `agentfs.commit.failures` | counter | Commit passes that failed; the records stay journaled |
| `agentfs.repack.failures` | counter | Periodic repacks that failed |
| `agentfs.push.unpushed_commits` | gauge, per remote | Backlog |
| `agentfs.push.oldest_unpushed_age` | gauge, per remote | Backlog age, the main push alarm |
| `agentfs.push.failures` | counter, per remote, reason and stage | Rejected, non-fast-forward, authentication, untrusted host, network, too large (`too_large`); stage `git`, `lfs` for the LFS upload before the git push, or `queue` for the patch remote's queue refs |
| `agentfs.push.diverged` | gauge, per remote | 1 while diverged |
| `agentfs.start.moved` | gauge, per remote (the start remote of a started volume) | 1 while the start-branch alarm holds, as the last push to that remote saw it; 0 before the first push and after one that does not raise it |
| `agentfs.push.limited` | counter, per remote | Pushes that sent only part of the backlog because it was over `agentfsMaxPushSize` |
| `agentfs.remote.misconfigured` | gauge, per remote | 1 for each invalid `[remote]` section |
| `agentfs.remotes.mode` | gauge, by `mode` | 1, with the volume's remotes mode: `pushed`, `none_valid` or `local_only` |
| `agentfs.precheck.rejected_paths` | counter | Paths refused by fsck or limit checks, by stage and reason |
| `agentfs.patch.queue_depth` | gauge | Patches queued after the last applied one, the blocked one included |
| `agentfs.patch.blocked` | gauge | 1 while the patch queue is blocked |
| `agentfs.patch.held` | gauge | Patches set aside (held, waiting, awaiting or not judged) at the last run |
| `agentfs.state.bytes` | gauge | Size of the state area by part; part `tmp_layer` is the sandbox overlay's tmp files |
| `agentfs.telemetry.export_failures` | counter, per signal | Telemetry exports (traces, metrics, logs) that failed; counted by the binary, exported once metrics get through |
| `agentfs.front.round_trip` | histogram, by request | A front's round trip to the history service (`record`, `tracked`, `ping`), in seconds |
| `agentfs.front.service_failures` | counter, by stage | A front's failed requests to the history service: `connect`, `timeout`, `request` (connection lost or malformed reply), `refused`, `service` |
| `agentfs.history_service.fronts` | gauge | Fronts connected to this history service |
| `agentfs.history_service.superseded` | counter | Front writes the history service skipped because a newer state of the path was already journaled (freeze stamps) |
| `agentfs.history_service.resyncs` | counter | Paths the history service recorded again as the backing tree has them, after a front's change it could not order (identical stamps from two slots, a late removal or symlink) |
| `agentfs.fuse.create_as_caller_fallbacks` | counter | Objects a root daemon created as itself and then gave to the caller, because creating as the caller was refused (a device node, or a caller whose groups are unknown) or could not be set up |
| `agentfs.fuse.racy_rechecks` | counter, per outcome | Boundaries that would have recorded a file again only because its last record was racy: `skipped` when its bytes were that record's frozen copy, still the last state journaled at its path, else `recorded` |
| `agentfs.reconcile.records` | counter | Records made reconciling moved directories |
| `agentfs.reconcile.failures` | counter | Moved directories given up on after 5 failed reconciliations over at least 30 s (`reconcile_failed`) |
| `agentfs.nested.gitdir_heuristic` | counter | Absolute `gitdir:` or `.git` symlink targets under no known view of the tree, found by trying their suffixes; set `--workload-mount` (CSI: `workloadMounts`) |
| `agentfs.sandbox_overlay.refusals` | counter, by `reason` | Changes the sandbox overlay refused, each counted once: `not_recordable` (a name the rules skip for that kind: a directory they could record nothing beneath, a recorded file moved to a skipped name), `kind` (a symlink, hard link or special file), `cross_device` (a directory move, `EXDEV`), `hidden` (the source, or an entry at the target, is hidden), `rules_error` (the rules could not decide: `EIO`), `ownership` (an owner, group, set-id or sticky-bit change), `xattr` (an extended-attribute change). Reads of hidden names are not counted |
| `agentfs.fork.publish.duration` | histogram, per remote | Seconds from a fork's finish commit to its fork remote holding the change and finish refs: one point per fork, when the published mark is written |

Metric attributes are short and low-cardinality: `trigger`, `remote`,
`reason` (a refusal label, a push failure kind or a sandbox overlay
refusal), `stage` (`record` or
`commit` for refused paths, `git`, `lfs` or `queue` for push failures) and `part`
(`git`, `journal`, `pending`, `lfs`, `meta`, `tmp_layer`). Paths never become metric
attributes. Span and log attributes are namespaced `agentfs.*`.

`agentfs.state.bytes` is the apparent size (the sum of file lengths) of each
part, not allocated blocks: reflinks make both misleading, and apparent size
is deterministic. The journal backlog and the push gauges are read when
metrics are collected: the backlog from the open journal; unpushed commits,
oldest unpushed age (seconds since the oldest unpushed commit's committer
time, 0 when none) and divergence from each remote's push state plus a walk
of the branch, shared by the three push gauges in one collection.

**Sandbox overlay refusals.** `agentfs.sandbox_overlay.refusals{reason}`
counts each change a sandbox made through the overlay that was refused
([guarantees](guarantees.md)), once, with `reason`
one of:
- `not_recordable`: a name the rules skip for that kind (a directory
  they could record nothing beneath, a recorded file moved to a
  skipped name);
- `kind`: a symlink, hard link or special file;
- `cross_device`: a directory move, answered `EXDEV`;
- `hidden`: the source, or an entry at the target, is hidden;
- `rules_error`: the rules could not decide (`EIO`);
- `ownership`: an owner, group, set-id or sticky-bit change;
- `xattr`: an extended-attribute change.

Reads of hidden names are not counted: a hidden name answers like a
missing one. A steady count usually means a sandbox's tool works in a
way the overlay does not offer
([runbook](runbooks/sandbox-overlay.md#a-sandbox-cannot-save)).

## Spans

Spans `agentfs.record` (trigger, inputs, caller uid and pid, the
sequence number when journaled, paths skipped), `agentfs.commit` (first and
last sequence numbers, records, paths refused, the commit id or
`agentfs.commit.empty`) and `agentfs.push` (remote, previous and new head,
outcome, on failure the reason, the stage and any diverged ref, and when
the backlog references LFS objects `agentfs.lfs.objects`,
`agentfs.lfs.uploaded` and `agentfs.lfs.bytes`). Each span is
current while its operation runs, so alarm logs raised inside it carry its
trace. Record and commit spans are not linked; they correlate through the
sequence numbers. `agentfs.patch` covers one patch at mount (inbox number,
patch oid, outcome, the commit or the block reason); a push to the
patch remote carries the queue push's result as `agentfs.push.queue`
(`pushed`, `failed`, or `branch_not_ours`: nothing written).
`agentfs.fork.finish` covers writing a fork's change and finish commit,
once (`agentfs.fork`, `agentfs.fork.change` when there is one,
`agentfs.fork.unrecorded`), and `agentfs.fork.publish` one publish call
(`agentfs.fork`, `agentfs.remote`, `agentfs.fork.publish.outcome`:
`published`, `pending` or `refused`, and `agentfs.fork.refused.reason`),
with its `agentfs.push` spans inside it. `agentfs fork finish`
exports like `push`, so the CSI node plugin's finisher children export
too.

## Logs

A structured log record for every alarm condition, and at INFO
for a fork's lifecycle (`ForkFinished`, `ForkPublished`, `ForkAbandoned`:
`Event::is_logged`): the event name
(`agentfs.path_refused`, `agentfs.push_failed`, ...), WARN for a refused
path, a failed patch fetch, an ignored patch, a deferred patch queue, an
identity fallback and a limited push, and ERROR for every other alarm, the event's message as the body and its fields
(path, reason, remote, kind, heads, ...) as attributes; a push failure
carries its stage as `agentfs.push.stage`.

## Events

The engine reports alarm conditions as events to a sink
(`agentfs_core::events::EventSink`). The telemetry layer wraps the
process's sink once (`Telemetry::sink`), turning events into the counters
and logs above; the CLI also prints alarms to stderr.

| Event | Raised when |
|---|---|
| `PathRefused` | A path fails the checks for what agentfs records (size limits, fsck rules, nested repositories), at record or commit time, with the reason's label (for example `blob_too_large`, `lfs_object_too_large`, `max_size`) |
| `RecordFailed` | The synchronous step fails |
| `CommitFailed` | Turning records into a commit fails; the records stay journaled and are retried |
| `ReconcileFailed` | Reconciling a moved directory failed 5 times over at least 30 s, with both paths and the error: the move is committed as journaled, and files beneath the destination that recording the tree would have added, dropped or recorded again differ from the tree until each changes again or is recorded with `agentfs record` |
| `PushFailed` | A push attempt fails, with the remote, the kind (`unreachable`, `auth`, `untrusted_host`, `non_fast_forward`, `refused`, `local`, `too_large`), the stage (`git`, or `lfs` when uploading LFS objects failed and no ref was pushed; `too_large` at stage `lfs` is an object over the remote's `agentfsMaxLfsObjectSize`), the head, the last pushed commit, the consecutive failure count and the next retry; stage `queue` for the patch remote's status, applied and inbox refs, which never counts against the branch |
| `Diverged` | The local head was saved under a diverged ref |
| `StartBranchMoved` | A started volume's start branch holds commits it does not have: the start remote, the branch, the pinned commit and the tip, at every push to the start remote while it holds; an error, like `Diverged`; log event `agentfs.start_branch_moved` with `agentfs.remote`, `agentfs.start.ref`, `agentfs.start.pinned`, `agentfs.start.tip` |
| `PushLimited` | A push sent only part of the backlog because it was over the remote's `agentfsMaxPushSize`: the commit pushed, the head, the limit and the first-parent commits left; the next step follows at once; a warning |
| `RemoteMisconfigured` | A `[remote "<name>"]` section is invalid, so that remote is left out; raised when a volume opens and by `agentfs push` |
| `RepackFailed` | The periodic `git repack` failed, or refreshing the object database after it; tried again an interval later |
| `PushSucceeded` | A push attempt succeeded, including one with nothing new (not an alarm; sinks filter) |
| `PatchBlocked` | A patch could not be applied at mount: the inbox number, the patch, the reason and the queue depth; inbox 0 with reason `failed` when the queue could not run at all; the volume mounts anyway |
| `PatchHeld` | A patch was set aside whole, held (the reason) or not judged (the rule), while the queue went on: the inbox number, the patch, the verdict, the reason and the message; raised at every run it stays so |
| `PatchRuleFailed` | The acceptance rule could not judge a patch: the inbox number, the rule and why (a hook that did not start, failed, or ran out of `agentfs.patchAcceptTimeout`); the patch is not judged |
| `PatchFetchFailed` | Fetching the inbox from the patch remote failed or timed out; the queue runs with what was already fetched |
| `NotifyFailed` | Telling the integrator of the patch queue's verdict changes failed: the method (`hook`, the `patch-notify` hook; `webhook`, `agentfs.patchNotifyUrl`) and why; the same changes are sent again at the next queue run or apply |
| `PatchQueueDeferred` | A start did not land the patch queue because a workload may be attached: the reason (`unclean_stop`, `stale_mount`, `fronts_running`, `workload_attached`) and the patches queued; raised at each such start while any is queued; the queue was judged instead, and the patches land at the next start with nothing attached; a warning |
| `PatchIgnored` | An inbox entry the queue never applies: `stale` (at or below the last applied number, with another commit; deleted on the remote by the queue push) or `malformed`; a warning |
| `PatchApplied` | A patch was applied as a commit (not an alarm; sinks filter) |
| `PatchDuplicate` | A queued patch commit was already applied under an earlier inbox number, so it was consumed with a marker commit and not applied again (not an alarm) |
| `ForkFinished` | A fork wrote its finish commit, with the fork and its change (none when nothing changed); once (not an alarm, logged at INFO) |
| `ForkPublished` | A fork's fork remote holds its change and finish refs at exactly their ids: the fork and the remote; once, when the published mark is written (not an alarm, logged at INFO) |
| `ForkRefused` | A fork's fork remote refuses its publish for good: the fork, the remote, the reason (`another_writer`, `too_large`, `rejected`, `remote_gone`, `unsafe`) and the remote's message; once, when the refusal is recorded; nothing retries it until `agentfs fork finish --retry` ([fork-refused.md](runbooks/fork-refused.md)) |
| `ForkParentFinished` | A fork of a fork finished after its parent had, so its change could not roll up into the parent's queue and was published as a top-level fork's, at `refs/agentfs/<fork>/change`: the fork and its parent; once, before the published mark, which records it; again only at the next publish after a crash between the alarm and the mark, which leaves the fork finished. Nothing is lost and nothing lands it ([runbook](runbooks/patch-queue.md#9-forks-of-forks)) |
| `ForkInboxNotCarried` | A fork published to its fork remote, and its inbox there holds entries its finished history did not land, most often a fork of it that finished after its last mount: the fork, the remote and the inbox numbers; when the fork is published. Its change does not carry them and nothing lands them ([runbook](runbooks/patch-queue.md#9-forks-of-forks)) |
| `ForkAbandoned` | An operator abandoned a fork on record, with the reason (not an alarm, logged at INFO) |
| `HistoryServiceUnavailable` | A front's request to the history service failed (unreachable, timed out, lost or refused) after it last succeeded: raised once per outage; boundaries fail with `EIO` until it is back |
| `HistoryServiceRestored` | A front's request succeeded again after `HistoryServiceUnavailable` (not an alarm; sinks filter, the CLI prints it) |
| `IdentityFallback` | The history service could not render a journaled front origin's committer (the identity templates changed across a restart); that pod's records are committed with the service's own committer, keeping the pod trailers; once per pod |

A sink is called from the synchronous step, on the FUSE threads, so it must
never block. Backlog gauges (`unpushed_commits`, `oldest_unpushed_age`) come
from each remote's push state, not from events.

A sink is called from the synchronous step, on the FUSE threads, so it never
blocks. Backlog gauges (`unpushed_commits`, `oldest_unpushed_age`) come
from each remote's push state, not from events.

## Export

The binary reads the standard `OTEL_*` variables. A signal is
exported only when an OTLP endpoint is set for it
(`OTEL_EXPORTER_OTLP_<SIGNAL>_ENDPOINT`, else `OTEL_EXPORTER_OTLP_ENDPOINT`);
with none, with `OTEL_SDK_DISABLED=true`, or with
`OTEL_<SIGNAL>_EXPORTER=none`, telemetry is a no-op and nothing is built.
Each signal goes over the protocol `OTEL_EXPORTER_OTLP_<SIGNAL>_PROTOCOL`,
else `OTEL_EXPORTER_OTLP_PROTOCOL`, names: `http/protobuf` (the default),
`http/json`, or `grpc`. The HTTP exporters use a blocking client on the
SDK's batch threads; the gRPC exporter (tonic) needs a tokio runtime, so a
one-worker runtime (`agentfs-otlp`) is started only when a signal uses gRPC,
and the batch threads drive each export to completion on it. Headers
(`OTEL_EXPORTER_OTLP_HEADERS`) go out as HTTP headers or gRPC metadata, so
a bearer token is `Authorization=Bearer%20<token>`.

### TLS to the collector

(`https://` endpoints) is the `otlp-tls` cargo
feature, built into the image: rustls on ring for both transports (no
OpenSSL, no aws-lc; `ci/check-deps.sh` checks each feature set). A CA bundle
(`OTEL_EXPORTER_OTLP_CERTIFICATE`, per signal or general) is the only trust
when set, so a private collector CA is pinned; unset, the system store is
used. A client certificate and key (`OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE`,
`..._CLIENT_KEY`), set together, give mutual TLS; one without the other is a
misconfiguration. Every file is read and parsed when the exporter is built,
so a bad file turns its signal off with a warning naming it at start rather
than failing every export; rotating a certificate takes a restart. For the
HTTP exporters agentfs builds its own blocking client with that rustls
configuration and so applies `OTEL_EXPORTER_OTLP_TIMEOUT` itself; the gRPC
exporter gets tonic's TLS settings from the same files. Without the feature
an `https` endpoint is reported as needing it.

An unknown protocol, an `https` endpoint in a build without `otlp-tls`, an
`OTEL_EXPORTER_OTLP_TIMEOUT` that is not a number of milliseconds (checked
for every endpoint), or TLS settings that cannot be used are reported
with a warning and that
signal stays off: a
telemetry misconfiguration never stops a command, in keeping with telemetry
never affecting the filesystem. Telemetry is flushed at exit, waiting at
most 5 seconds.

## Alarms

Alarms are alert rules on these metrics in the consumer's monitoring stack.
agentfs emits the signals; it does not page anyone itself. Example rules
ship in `deploy/alerts`, as a Prometheus rules file and a generated
`PrometheusRule`, each alert linking its runbook
([deployment.md](deployment.md#alerts)). They name the metrics as
Prometheus's default OTLP translation does (`agentfs.record.failures` is
`agentfs_record_failures_total`, `service.instance.id` is `instance`), and
each counter rule also matches a counter's first sample, since a counter
appears at its first count and `increase()` alone needs two. Under the
CSI driver one `instance` (the node plugin's pod) runs every volume on
its node, so there `agentfs.volume` must be promoted to the label
`agentfs_volume`, and the rules join a volume's series on it. The
`alerts` workflow runs promtool's checks and unit tests on them and
checks them against the binary's metrics in a real Prometheus. The CSI driver
reports failed calls through the kubelet's events on the pod (FailedMount
with the call's error); it emits no Events of its own, having no API
access.

## Alerts

Example Prometheus alert rules on these metrics, each linking its
runbook, are in [`deploy/alerts`](../deploy/alerts), as a rules file and a
`PrometheusRule` ([deployment.md](deployment.md#alerts)); the CSI chart
installs them with `alerts.enabled=true` ([helm.md](helm.md#3-values)).
