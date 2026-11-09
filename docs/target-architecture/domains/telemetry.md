# agentfs target architecture: the Telemetry domain

**Proposed, not built.** This page proposes the Telemetry domain (metrics, traces and alarms), one part of the system around the git model ([the system](../system.md)). The git model ([git-model.md](../git-model.md)) wins wherever the two disagree. Rust sketches show surfaces only. The commit that lands the slice that builds this domain deletes this page and moves the behaviour into the library or operator docs.

## 1. What Telemetry owns, and what it is not

Telemetry turns the events every domain publishes into metrics, traces, log records and alarms, and sends them to the
operator's monitoring. It is the one home of alarms. It never blocks a caller, never fails one, and nothing in agentfs
reads anything back from it.

The owner's guidance that applies here is in `OWNER-GUIDANCE.md` at the repository root. Everything else on this page is a design choice.

**Owns:** the catalogue (every metric, span, log and alarm name), the alarm board, trace identity, export. **Is not:**
a judge of any fact, a source of events, dashboards or alert thresholds, persistence.

## 2. Model and contract

```rust
pub trait Telemetry {
    fn emit(&self, e: &Event); // returns nothing: no caller can branch on telemetry
}
pub enum Event {                    // closed: a new event cannot skip the catalogue (T3)
    Clients(ClientsEvent),          // Attached, AttachmentEnded, LastDetached
    Tree(TreeEvent),                // TreeMoved (cause: save, landing, sync, transaction; collisions named), TakeWaiting, TakeReleased, OrphanDropped, SaveFailed
    Views(ViewsEvent),              // ViewCaughtUp
    Trees(TreesEvent),              // ForkEnded, ForkDropped
    Volume(VolumeEvent),            // VolumeDraining, VolumeStaged, VolumeReleased, StartLineChanged
    Replication(ReplicationEvent),  // PushSettled
    Service(ServiceEvent),          // ServiceReady
    Health(HealthEvent),            // LivenessChanged
    Diagnostic(Diagnostic),         // failures that are replies: SpaceLimit, MountFailed, OtherFamily, RulesError, ...
}
pub enum AlarmEffect {
    None,
    Raise { alarm: AlarmId, subject: Subject },
    Clear { alarm: AlarmId, subject: Subject },
    Incident { alarm: AlarmId },    // counted and logged, never on the board
}
pub enum Subject { Tree(TreeRef), Remote(RemoteName), Component(Component), Fork(ForkId) }
pub trait LabelValue: sealed::Bounded {} // only closed enums: cause kinds, reason codes
pub trait AttrValue: sealed::Shown {}    // ids, commit ids, paths; never content or credentials
```

`TakeWaiting{tree, files}` and `TakeReleased{tree}` (design choice: the tree's actor reports a take waiting for open
files, git-model §6) exist only so the operator sees a stalled queue; nothing reads them.

| Alarm | Kind | Raised by | Cleared by | Judge |
|---|---|---|---|---|
| `CollisionRecorded` | incident, with writer, lost and won lines as attributes | `TreeMoved` naming a collision | — | the tree's actor |
| `TakeWaiting` | condition, per tree | `TakeWaiting` | `TakeReleased` | the tree's actor |
| `SaveFailed` | condition, per open | the tree's actor: a save at `RELEASE` failed; its calls stay journaled (git-model §3) | that save succeeding | the tree's actor |
| `VersionMismatch` | incident, both versions shown | `OtherFamily`, `OtherMajor`, `FormatsOf` | — | the refusing domain |
| `StartLineMoved` | condition, per volume | `StartLineChanged{Moved}` | `StartLineChanged{InStep}` | Volume |
| `PushFailing` / `PushDiverged` | condition, per remote | `PushSettled{failed}` / `{diverged}` | `PushSettled{ok}` | Replication |
| `ComponentUnresponsive` | condition, per component | `LivenessChanged{Unresponsive, Gone}` | `LivenessChanged{Live}` | Health |
| `NoSpace`, `ForkSpaceLimit`, `ViewLagged`, `RulesError`, `MountFailed` | incident | its diagnostic | — | the emitting domain |

## 3. Invariants

| # | Invariant | Enforced by | Strength |
|---|---|---|---|
| T1 | `emit` never fails a caller and never blocks | returns `()`; metrics are atomic updates; spans and logs go to a bounded queue with `try_push`, drops counted | compile time + test |
| T2 | Nothing depends on Telemetry | no query; `cargo deny`; the port is always bound, export `Off` a value | build + boot time |
| T3 | Every event kind has exactly one catalogue entry | `Event` is closed; one exhaustive `match`, wildcards denied | compile time |
| T4 | No identity or path is ever a metric label | labels take only sealed `LabelValue` | compile time |
| T5 | No file content and no credential reaches any signal | attributes take only sealed `AttrValue` | compile time |
| T6 | Each condition alarm is raised and cleared only by its judge's events, never by silence or elapsed time | tested over the catalogue; clippy bans timers | construction + test |
| T7 | Telemetry persists nothing; its board is rebuilt from the judges' re-published state before `ServiceReady` | no repository port | construction + test |

**Paths are log and span attributes, never metric labels** (design choice), so an operator sees which file a
collision or a waiting take is about. Cost: whoever reads the collector reads paths, hidden ones too.

**Who judges "too long"** (design choice). A take waiting on a held-open file can last days (git-model §16). agentfs
exports each condition alarm's age; the operator's alert rules hold the thresholds, so agentfs keeps no timer.

## 4. Ports, policies and tests

Ports: `SignalOut` (OpenTelemetry SDK, OTLP from the standard `OTEL_*` variables), `AlarmOut` (one stderr line per
alarm change, whatever the export policy), `Clock` (stamps only).

| Setting | Current setting |
|---|---|
| Export | `Off` unless an `OTEL_*` endpoint is set |
| Trace sampling | ratio on the trace id, 1.0 (operator, `OTEL_TRACES_SAMPLER`) |
| Metric export interval | 60 s (the SDK's default; a listed operator schedule) |
| Resource attributes | volume, process role, `service.version` (full semantic version), pod and node |

The catalogue is the one home of every signal name: the metric and alarm reference and example alert rules are
generated from it, and CI checks they match.

| Test | Rules out |
|---|---|
| Compile-fail: a `ClientId` or path as a label; content or a credential as an attribute; a wildcard catalogue match | series explosion; leaks; an untracked event |
| The collector accepts and never answers; 1,000 forks save: save latency as with export `Off` | a slow collector slowing file calls |
| A collision: one `CollisionRecorded` with its writer and lines; a take waiting on an open file raises and clears `TakeWaiting` | an unseen loss or stall |
| Hours of fake idle time raise nothing | alarms from elapsed time (T6) |
| A generated reference that differs from the catalogue fails CI | docs drifting from code |

## 5. Costs

1. **Lossy by design:** overload drops spans and logs (counted); a crash loses the queue.
2. **The caller pays the translation:** a catalogue lookup and a few atomic updates on its thread.
3. **Paths in logs** put path names in the operator's collector.

## Where it meets the existing code

- `EventSink` becomes the port with a closed `Event`; free-form fields become per-domain diagnostics.
- Telemetry stops being optional: the port is always bound, `Off` an export value; the once-a-minute stderr warning becomes one line per export-link change.
- Alarm gauges set by each subsystem (`patch.blocked`, `push.diverged`) move onto the board. Cost: every name is regenerated from the catalogue in one release, with no aliases.

