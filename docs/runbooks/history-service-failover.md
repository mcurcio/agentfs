# Runbook: history service failover across nodes

With a volume mounted on several nodes (CSI `--multi-node`, a shared
filesystem such as NFS), each node that stages the volume runs a
`serve-history --standby`. One holds the state lock and is active; the
others wait. Fronts given `--history-service state` find the active one
through `meta/history/endpoint.json`. Design:
[§14.2](../design.md#142-csi-driver-built), the lock and its fence:
[§2.3](../design.md#23-single-writer-of-history).

## Symptoms

- A handover in progress: fronts' boundaries wait up to
  `--history-timeout` (nothing fails if the next service is active by
  then). A longer gap fails boundaries with `EIO` and raises
  `HistoryServiceUnavailable` on the fronts, as in
  [history-service-down.md](history-service-down.md).
- `StateLockLost` (log event `agentfs.state_lock_lost`) from a service:

  ```text
  alarm: lost the state lock (...); another node may hold it: this history service stops writing and exits, and a standby takes over
  lost the state lock (...): exiting without a final commit; another node's standby takes over
  ```

  The service exited 75 at once. Its node could not renew the lock on the
  shared filesystem (a network partition, the NFS server restarting, a
  node too loaded to renew within `--lock-fence`).
- A standby's log: `standby for volume V: the active history service is
  pid=N node=X ...; waiting to take over`, then `took over the state
  lock` and `serving history ...` when it becomes active.
- `agentfs --state "$STATE" status`: `lock:` names the active service's
  pid and node; `endpoint:` names the node, pid and addresses fronts use;
  `endpoint:  stale (...)` means no service holds the lock.

## Diagnose

1. Which node is active? `status`'s `lock:` line (`node=...`) and
   `endpoint:` line agree on a healthy volume.
2. No active service, but standbys running: they poll the lock every
   second. On NFS a node that vanished keeps its lock until the server's
   lease expires (`lease-time`, Linux's default 90 s, plus the grace
   period after a server restart). Wait that long before acting.
3. A service exits 75 again and again (`StateLockLost`): its node cannot
   keep the lock fresh. Check the NFS mount (`hard`, never `soft` or
   `nolock`), the server's `lease-time` (at least twice
   `--lock-fence`, default 10 s), and the node's network.
4. Fronts fail with `the active service ... is on another node and serves
   no TLS address`: the active service was started without
   `--listen-tls` (the CSI driver passes it only with `--multi-node`).

## Fix

- A planned move (draining a node): stop its plugin pod or unstage the
  volume; the service makes its final commit and exits, and a standby on
  another node takes over within about a second plus its recovery.
- A lost lock: nothing to do by hand. The service exited without
  committing; the next active service replays the journal (every
  acknowledged record is in it) and replaces the journal file, so a
  fenced writer still holding the old one cannot append to the live
  journal.
- A standby that never takes over while `status` says the lock is free:
  check its log for errors opening the volume (recovery), and restart it.

## Residual risk

A service stuck inside one NFS call when the partition began can finish
that call after another node took over. The fence stops every new
journal write and commit, the journal file is replaced at takeover and
branch updates are compare-and-swap, so the known cases are harmless;
this is reviewed, not proven ([§19.2](../design.md#192-open-questions)).
