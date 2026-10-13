# Runbook: the history service is down

With several mounts of one volume, every front (`agentfs mount
--history-service`) hands each change boundary to the volume's history
service (`agentfs serve-history`) and waits for it. While the service is
down, hung or refusing a front, changes through that front are not
recorded. Design: [§14](../design.md#14-volumes-mounted-by-several-pods-csi-phase-4),
failures: [§3.5](../design.md#35-when-recording-fails), the operator
guide: [§16](../operator-guide.md#16-several-mounts-of-one-volume-history-service).

## Symptoms

- Workloads see `EIO` from `close` (and from `rename`, `unlink` and the
  other namespace operations) on a front's mount. Many programs ignore
  errors from `close`, so the alarms are the reliable signal. Reads and
  writes themselves keep working: the backing tree has the data.
- A front's stderr (and log event `agentfs.history_service_unavailable`):

  ```text
  alarm: history service unix:/data/state/meta/history/history.sock is unavailable: history service ... is unreachable: Connection refused (os error 111); changes fail with EIO until it is back
  alarm: record failed (close) for notes/plan.md: history service ... is unreachable: ...
  ```

  `HistoryServiceUnavailable` is raised once per outage per front;
  `RecordFailed` once per failed boundary.
- Metrics: `agentfs.front.service_failures{stage}` grows on the fronts
  (`connect`: nothing listens; `timeout`: no answer within
  `--history-timeout`; `request`: the connection broke; `refused`;
  `service`: the service answered with an error);
  `agentfs.history_service.fronts` stops being reported by the service.
- `agentfs --state "$STATE" status` (safe at any time):
  - no `history:` line, and `lock:` free or held by another command: the
    service is not running;
  - `history:   serving fronts at ...` but fronts still fail: the service
    is hung or refuses them (below);
  - `fronts:` lists every front's slot, pid and pod.

## Diagnose

1. Is the service running? `status` shows `lock: held by pid=N
   command=serve-history`. If the lock is free, the service exited or
   crashed: read its log (its last lines say why; a crash leaves
   nothing).
2. Is it answering? A front waits `--history-timeout` (10 s) per
   request. `timeout` failures with a running service mean it is stuck:
   a full or very slow state area (the journal `fsync`), or a commit
   loop blocked. Check the state area's disk (`df`), and the service's
   `CommitFailed` alarms.
3. Does it refuse the front? The front's alarm or exit message says
   `history service ... refused this front: <reason>`:

   | Reason | Fix |
   |---|---|
   | `this history service serves volume "x", not "y"` | The front and the service use different state areas: fix `--state` on one of them |
   | `uid N may not use this history service (root or uid M only)` | Run the front as root or as the service's user |
   | `the volume's identity templates do not render for pod "p"` | The front's pod flags do not fill `[agentfs "identity"]` (an empty `{pod}` in a template, for example): set the flags (`AGENTFS_POD`, ...) or fix the templates ([configuration](../configuration.md#agentfs-identity)) |
   | `slot N is not leased` | The front lost its lease file (`meta/fronts/` was removed): restart the front |
   | `protocol version N is not supported` | The front and the service are different agentfs versions: run one version |
   | `the socket is served by uid N, not root or this front's uid M` | Another user is listening at the address (a shared, writable listen directory): move `--listen` to a directory only the service's user can write, and find who bound the socket |

   A front that says `history service ... is busy: ... already serves 1024
   connections` is not refused and raises no `HistoryServiceUnavailable`:
   too many fronts are busy at once (design §14). Spread the fronts over
   more volumes, or lower their `--threads`.

## Restart the service

Start it again the way it was started, with the same `--state`, `--root`
and `--listen`:

```sh
agentfs --state "$STATE" --root "$BACKING" serve-history
```

- A crashed service leaves its socket behind; the new one replaces it.
  If another process is still listening there, the new one refuses to
  start (`another process is listening on ...`): stop that one first.
- It recovers the journal (`N uncommitted record(s) recovered`) and
  commits what fronts had journaled, each with its own pod's committer.
- With fronts running it does not apply the patch queue
  (`patches: N front(s) are running ...`); the queue is applied at a
  later start with no fronts.
- A hung service: SIGTERM, then a second signal if it does not stop
  within `--shutdown-grace` plus `--push-grace`. As a last resort
  `kill -9` is safe: what it acknowledged is in the journal.

Do not restart the fronts: they reconnect by themselves at their next
boundary.

## Check that the fronts caught up

1. Each front prints `history service ... is reachable again` at its first
   successful request.
2. Paths whose boundaries failed are recorded again from the backing
   tree: a file still open by its next close or the sweep (every
   `agentfs.sweepInterval`), a closed one or a namespace change by the
   front's repair queue, retried at every sweep. `status` lists a
   `repairs:` line only for a slot no live front drains.
3. Compare history with the tree for the paths the `RecordFailed` alarms
   named:

   ```sh
   git --git-dir "$STATE/git" log -3 --format='%h %cn %s' refs/heads/agentfs/<volume> -- <path>
   git --git-dir "$STATE/git" show refs/heads/agentfs/<volume>:<path> | cmp - "$BACKING/<path>"
   ```

   The commit's committer is the pod of the front that made the change.

## What is lost

Only the states in between: if a file changed several times during the
outage, history gets its final state, recorded when the service is back,
not each version. Nothing acknowledged before the outage is lost: a
boundary that returned success was in the service's journal, and a
restarted service commits it. A path that changed during the outage and
whose front stopped before the service came back is recorded by the next
front in that slot (its repair queue, committed as the pod that made the
change), or by any later change to it.
