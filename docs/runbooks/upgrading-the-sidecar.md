# Runbook: upgrading the phase 1 sidecar

Moving a workload's agentfs sidecar ([deployment.md](../deployment.md),
[`deploy/sidecar`](../../deploy/sidecar)) to another image, and back.
An upgrade is a pod replacement: the old sidecar stops after the
workload, makes its final commit and pushes once, then the new pod mounts
the same volume. Nothing is migrated by hand. The CSI driver is upgraded
differently ([csi.md](../csi.md)).

```sh
POD=$(kubectl get pod -l app.kubernetes.io/name=agentfs-example -o name | head -n1)
```

## Before

1. **Pick the image by commit.** Tags are 40-character commit ids of
   `main` (and `latest`, which moves): use `ghcr.io/mcurcio/agentfs:<sha>`
   or a digest, never `latest`
   ([deployment.md](../deployment.md#1-the-image)). The commits between
   the running one and the new one (`git log --oneline <old>..<new>` in a
   clone of agentfs) say what changed; design §19.1 lists decisions by
   date.
2. **Note where the volume is:**

   ```sh
   kubectl exec "$POD" -c agentfs -- agentfs --version
   kubectl exec "$POD" -c agentfs -- agentfs status
   ```

   Keep the `head:` line, and check `backlog:` and each `remote:` line.
   Unpushed commits are kept across the upgrade, but pushing first
   (`kubectl exec "$POD" -c agentfs -- agentfs push`) means a remote holds
   the history if the upgrade goes wrong.
3. **No recording failures open.** A `RecordFailed` or `CommitFailed`
   alarm, or the alerts `AgentfsRecordFailed` and `AgentfsCommitsFailing`,
   should be resolved first ([recording-failures.md](recording-failures.md)),
   so the final commit can empty the journal.

## Upgrade

Change the image of **both** the `agentfs-init` and `agentfs` containers
(with kustomize, one `images:` entry for `ghcr.io/mcurcio/agentfs`
covers both), and apply. The Deployment's `Recreate` strategy stops the
old pod before the new one starts, so only one daemon ever holds the
`ReadWriteOncePod` claim and the state area's lock.

The old sidecar's stop, within `terminationGracePeriodSeconds` (60 s in
the example; keep it longer than the unmount grace of 10 s, the push
grace of 5 s and the final commit):

1. the workload containers stop first, then the sidecar gets SIGTERM;
2. it unmounts, commits what is journaled and pushes each remote that
   lacks commits once, within the push grace (design §11.2);
3. commits that did not get out are pushed by the new pod.

If the old pod is killed before its final commit (the grace ran out, or
the node died), nothing is lost: the new daemon replays the journal at
start (`... uncommitted record(s) recovered`).

## Verify

```sh
kubectl rollout status deployment/agentfs-example
kubectl exec "$POD" -c agentfs -- agentfs --version
kubectl exec "$POD" -c agentfs -- agentfs status
kubectl logs "$POD" -c agentfs | head -n 20
```

(`POD` again after the rollout: the pod is new.)

- The pod is Ready: the readiness probe sees a live mount.
- `head:` is the head you noted, or a descendant of it
  (`git --git-dir /data/state/git merge-base --is-ancestor <old> <new>`
  in the sidecar); `backlog:` drains.
- No `remote: <name>: misconfigured` line: a newer version may refuse a
  setting an older one accepted (`RemoteMisconfigured`,
  [stuck-pushes.md](stuck-pushes.md#diagnose-by-kind)).
- The startup log has no `path(s) a previous mount could not record`
  with paths `still failing` ([recording-failures.md](recording-failures.md#4-paths-a-mount-could-not-record-metarepairs)).
- A change by the workload appears in `git log` and reaches the remotes
  (`agentfs status`, or the `AgentfsPushBacklogOld` alert staying quiet).

## Rolling back

A newer version can journal changes an older one cannot read (a
directory move): the older one then refuses to open the state area, with
an error naming `agentfs flush` (design
[§3.3](../design.md#33-journal-and-replay)). Nothing is discarded.

- **The newer pod stopped normally:** its final commit emptied the
  journal. Set the older image and apply.
- **It did not** (a crash, a kill), or the older pod refuses to start:
  empty the journal with the newer image first, as below, then set the
  older image and apply.

Name the newer image, then scale the Deployment to 0 and run
`agentfs flush` with it against the claim. The flush Pod mounts the
Deployment's ConfigMap, read from the Deployment because kustomize adds
a hash of the content to its name (`agentfs-config-<hash>`):

```sh
NEWER=ghcr.io/mcurcio/agentfs:<newer sha>
```

<!-- ci: rollback-flush -->
```sh
kubectl scale deployment/agentfs-example --replicas=0
kubectl wait --for=delete "$POD" --timeout=120s
CONFIG=$(kubectl get deployment agentfs-example \
  -o jsonpath='{.spec.template.spec.volumes[?(@.name=="config")].configMap.name}')
kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: agentfs-flush
spec:
  restartPolicy: Never
  containers:
    - name: flush
      image: $NEWER
      args: [flush]
      env:
        - { name: AGENTFS_STATE, value: /data/state }
        - { name: AGENTFS_ROOT, value: /data/backing }
      securityContext:
        runAsUser: 0
        runAsGroup: 0
      volumeMounts:
        - { name: data, mountPath: /data }
        - { name: config, mountPath: /data/state/git/info/exclude, subPath: exclude, readOnly: true }
        - { name: config, mountPath: /data/state/git/info/attributes, subPath: attributes, readOnly: true }
        - { name: config, mountPath: /etc/agentfs, readOnly: true }
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: agentfs-data
    - name: config
      configMap:
        name: $CONFIG
EOF
kubectl wait --for=jsonpath='{.status.phase}'=Succeeded pod/agentfs-flush --timeout=300s
kubectl logs agentfs-flush
kubectl delete pod agentfs-flush
```

The log says `<n> record(s) into <m> commit(s)`. Then set the older
image and scale back to 1
(`kubectl scale deployment/agentfs-example --replicas=1`).

Commits the newer version made stay in history: a rollback changes the
binary, never the history.

## What is tested

The kind job of the `image` workflow checks the stop this relies on: the
final commit and push when the pod is deleted, and recovery after the
daemon is killed ([deployment.md](../deployment.md#8-how-it-is-tested)).
The same job runs the flush block above as written (`ci: rollback-flush`),
with the image under test as `NEWER`: the Pod starts with the
Deployment's ConfigMap, the flush succeeds, and the volume mounts again
after the scale back. A rollout from one released image to another, and
an older image reading the flushed state area, are not run in CI.
