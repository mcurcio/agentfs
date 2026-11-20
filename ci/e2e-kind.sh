#!/usr/bin/env bash
# End-to-end test of the sidecar deployment (docs/deployment.md) on the tree engine in a kind cluster: deploys ci/k8s (the CI
# overlay of deploy/sidecar) and checks, from real pods:
#
#   - init creates the volume and the sidecar mounts it; the remote of the
#     operator settings (the runner's git server) is logged as not built
#     (push) at every boot, and nothing reaches the server;
#   - root and uid 10000 write through the mount with normal permissions,
#     the uid 10000 writer through a subPath mount of the view at a fixed
#     path;
#   - the volume's history (state/history.git, branch root) holds their
#     files with their owners and the pod, namespace and node trailers
#     from the downward API;
#   - the operator rules from the ConfigMap, mounted under
#     history.git/info, keep an excluded file and a `-agentfs` file out of
#     history, and nothing of the state area is visible to the writers
#     (test 5);
#   - a replaced pod (Recreate) finds the volume, logs the remote as not
#     built again, and the old pod's last write is in history with its pod
#     trailer;
#   - the sidecar seals the bare mount point (root's, mode 0000; checked
#     from the node through a non-recursive bind of the emptyDir);
#   - a clean exit of the sidecar mid-pod: the drain unmounts the view and
#     the readiness probe follows (the pod turns NotReady), the root
#     writer's writes fail with EROFS (the read-only emptyDir) and reach
#     nothing; the sidecar waits for the subPath writer's bind until it is
#     killed (as the kubelet kills it at its grace); the subPath writer,
#     restarted inside the gap, gets EACCES from the sealed mount point
#     (or, if the sidecar was back first, writes through the new mount and
#     is recorded); its liveness probe restarts it onto the new mount, and
#     both write through the restarted sidecar's mount;
#   - after `kill -9` of the daemon from the node, writes fail with
#     ENOTCONN, the kubelet restarts the sidecar (a write that races the
#     restart and succeeds must be recorded), which stacks a new mount
#     over the stale one (`--on-stale-mount recover`) with no manual step;
#     both writers (HostToContainer and subPath) reach it without a
#     restart, their writes are recorded, and the failed write is nowhere.
#
# Needs docker, kind and kubectl (ci/install-kind.sh), a kind cluster
# $KIND_CLUSTER (default agentfs-e2e) with the image agentfs:ci loaded, and
# the git server from ci/git-http-server.sh started with
# GIT_SERVER_EXTRA_LISTEN set to the kind network's gateway
# (`ci/e2e-kind.sh gateway`), its output in the environment. Locally:
#
#   docker build -t agentfs:ci .
#   kind create cluster --name agentfs-e2e --config ci/kind/cluster.yaml --image "$KIND_NODE_IMAGE"
#   kind load docker-image agentfs:ci --name agentfs-e2e
#   eval "$(GIT_SERVER_EXTRA_LISTEN=$(ci/e2e-kind.sh gateway) ci/git-http-server.sh start /tmp/gs | sed 's/^/export /')"
#   ci/e2e-kind.sh            # ci/e2e-kind.sh diagnose after a failure
#
# The FUSE node module must be loaded on the host (modprobe fuse).

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-e2e}
NODE=$KIND_CLUSTER-control-plane
NS=agentfs-e2e
APP=app.kubernetes.io/name=agentfs-example
VOLUME=ci-kind
WS=/agentfs/workspace
NOT_BUILT='not built: remote.origin.url (push): '

k() { kubectl -n "$NS" "$@"; }

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | sed -n 1p
}

diagnose() {
  set +e
  step "diagnose"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  k get pvc,svc,endpointslices -o wide
  k describe pods
  k get events --sort-by=.lastTimestamp | tail -n 60
  for pod in $(k get pods -o name); do
    for c in agentfs-init agentfs writer-root writer-10000; do
      echo "---- $pod $c"
      k logs "$pod" -c "$c" --tail=100
      echo "---- $pod $c (previous)"
      k logs "$pod" -c "$c" --previous --tail=100 2>/dev/null
    done
    echo "---- $pod history"
    k exec "$pod" -c agentfs -- git --git-dir /data/state/history.git log --format='%h %s' -20 root 2>/dev/null
  done
  echo "---- node mounts"
  docker exec "$NODE" sh -c "grep -E 'agentfs|fuse' /proc/self/mountinfo"
  return 0
}

case "${1:-}" in
gateway) gateway; exit 0 ;;
diagnose) diagnose; exit 0 ;;
"") ;;
*) echo "usage: $0 [gateway|diagnose]" >&2; exit 2 ;;
esac

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}" "${GIT_SERVER_HTPASSWD:?}"
REMOTE=$GIT_SERVER_REPOS/history.git
rgit() { git --git-dir "$REMOTE" "$@"; }

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

# The pod of the Deployment that is not being deleted.
current_pod() {
  k get pod -l "$APP" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
}

wait_ready() { # -> POD
  local deadline=$((SECONDS + 300))
  POD=""
  until [ -n "$POD" ]; do
    [ $SECONDS -lt $deadline ] || fail "no pod appeared"
    sleep 1
    POD=$(current_pod)
  done
  k wait --for=condition=Ready "pod/$POD" --timeout=300s >/dev/null ||
    fail "pod $POD never became Ready"
  pass "pod $POD is Ready"
}

in_root() { k exec "$POD" -c writer-root -- sh -c "$1"; }
in_user() { k exec "$POD" -c writer-10000 -- sh -c "$1"; }
in_sidecar() { k exec "$POD" -c agentfs -- "$@"; }
# git on the volume's history, in the sidecar.
history() { in_sidecar git --git-dir /data/state/history.git "$@"; }

# History is built behind the calls (docs/trees.md): waits until the
# root's branch has <path> with <content>.
wait_history() { # path content [seconds]
  local deadline=$((SECONDS + ${3:-120}))
  until [ "$(history show "root:files/$(tree_git_fanned "$1")" 2>/dev/null)" = "$2" ]; do
    [ $SECONDS -lt $deadline ] || fail "the history never had $1 = '$2'"
    sleep 1
  done
  pass "the history has $1"
}
# The uid history records <path> with (its metadata in lines/).
owner() { history show "root:lines/$(tree_git_fanned "$1")" | grep -o '"uid":[0-9]*' | cut -d: -f2; }
# The message of the newest commit that touched <path>.
message() { history log -1 --format=%B root -- "files/$(tree_git_fanned "$1")"; }
# Fails if <path> was ever recorded, on any branch.
never_recorded() { # path what
  if history log --all --format= --name-only | grep -Fx "files/$(tree_git_fanned "$1")" >/dev/null; then
    fail "$1 was recorded ($2)"
  fi
  pass "$1 is not in history ($2)"
}
# The not built line, once per boot in this container's current log.
assert_not_built_logged() { # what
  assert_eq 1 "$(k logs "$POD" -c agentfs | grep -cF "$NOT_BUILT" || true)" "$1"
}

step "git server and cluster"
rm -rf "$REMOTE"
git init -q --bare "$REMOTE"
set_password agentfs ci-kind-password
GW=$(gateway)
[ -n "$GW" ] || fail "no IPv4 gateway on the kind network"
kubectl get nodes -o wide
NODE_NAME=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')

step "the example renders and passes server-side validation"
kubectl apply --dry-run=server -k deploy/sidecar >/dev/null
pass "kubectl apply --dry-run=server -k deploy/sidecar"

step "deploy ci/k8s"
kubectl apply -f ci/k8s/namespace.yaml
kubectl apply -f - <<EOF
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: git-server-runner
  namespace: $NS
  labels:
    kubernetes.io/service-name: git-server
    endpointslice.kubernetes.io/managed-by: ci.agentfs
addressType: IPv4
ports:
  - name: http
    port: 8080
    protocol: TCP
endpoints:
  - addresses: ["$GW"]
EOF
kubectl apply -k ci/k8s
wait_ready
FIRST_POD=$POD
assert_contains "$(k logs "$POD" -c agentfs-init)" "$VOLUME " "the init container ran"
assert_contains "$(k logs "$POD" -c agentfs-init)" " created" "the init container created the volume"
assert_contains "$(k logs "$POD" -c agentfs)" "mounted volume $VOLUME at $WS" "the sidecar mounted the volume"
assert_not_built_logged "the sidecar logged the remote as not built"
assert_eq "live agentfs mount (agentfs:$VOLUME)" "$(in_sidecar agentfs probe "$WS")" "probe sees the tree view"

step "root and uid 10000 write with normal permissions"
in_root "cd $WS && printf root > root.txt && chmod 600 root.txt && mkdir u10000 && chown 10000:10000 u10000"
in_user 'printf user > /workspace/u10000/user.txt'
assert_eq 10000 "$(in_root "stat -c %u $WS/u10000/user.txt")" "uid 10000's file is its own"
assert_eq 600 "$(in_root "stat -c %a $WS/root.txt")" "chmod 600 persists"
if out=$(in_user 'printf x >> /workspace/root.txt' 2>&1); then
  fail "uid 10000 wrote root's 0600 file"
fi
assert_contains "$out" "Permission denied" "uid 10000 cannot write root's 0600 file"
assert_eq user "$(in_root "cat $WS/u10000/user.txt")" "root reads uid 10000's file through the other mount"

step "operator rules and invisibility"
in_root "cd $WS && mkdir -p scratch secrets && printf x > scratch/x.log && printf k > secrets/k"
assert_eq "root.txt scratch secrets u10000" "$(in_root "ls -A $WS" | tr '\n' ' ' | sed 's/ $//')" "the writer sees only its files"
assert_eq "workspace" "$(in_root 'ls -A /agentfs')" "nothing else in the shared emptyDir"
assert_eq "root.txt scratch secrets u10000" "$(in_user 'ls -A /workspace' | tr '\n' ' ' | sed 's/ $//')" "the subPath writer sees the same"
assert_eq k "$(in_root "cat $WS/secrets/k")" "the -agentfs file reads back through the mount"
if in_root 'test -e /data'; then fail "the writer can reach /data"; fi
pass "the writers cannot reach /data"

step "the history holds the writes with their owners and the pod trailers"
wait_history root.txt root
wait_history u10000/user.txt user
assert_eq 0 "$(owner root.txt)" "history records root.txt as root's"
assert_eq 10000 "$(owner u10000/user.txt)" "history records u10000/user.txt as uid 10000's"
POD_UID=$(k get pod "$POD" -o jsonpath="{.metadata.uid}")
msg=$(message u10000/user.txt)
assert_contains "$msg" "Agentfs-Volume: $VOLUME" "volume trailer"
assert_contains "$msg" "Agentfs-Pod: $POD" "pod trailer"
assert_contains "$msg" "Agentfs-Pod-Uid: $POD_UID" "pod uid trailer"
assert_contains "$msg" "Agentfs-Namespace: $NS" "namespace trailer"
assert_contains "$msg" "Agentfs-Node: $NODE_NAME" "node trailer"
# A later save is built after them: history up to it holds neither.
in_root "printf later > $WS/later.txt"
wait_history later.txt later
never_recorded scratch/x.log "excluded by info/exclude"
never_recorded secrets/k "-agentfs in info/attributes"
assert_eq "" "$(rgit for-each-ref)" "nothing reached the git server: push is not built"

step "a replaced pod finds the volume and logs not built again"
in_root "printf final > $WS/final.txt"
OLD=$POD
# The subPath writer's bind holds the view until the pod's volumes are torn
# down, after the sidecar: the sidecar drains, then waits out the grace.
k delete pod "$POD" --wait --timeout=180s
wait_ready
[ "$POD" != "$OLD" ] || fail "the pod was not replaced"
assert_contains "$(k logs "$POD" -c agentfs-init)" "$VOLUME " "init ran in the new pod"
assert_contains "$(k logs "$POD" -c agentfs-init)" " found" "init found the volume"
assert_not_built_logged "the new pod's sidecar logged the remote as not built"
wait_history final.txt final
assert_contains "$(message final.txt)" "Agentfs-Pod: $OLD" "final.txt was recorded by the deleted pod"
assert_eq root "$(in_root "cat $WS/root.txt")" "the new pod serves the volume's files"

# Restart counts and pids on the node.
sidecar_restarts() {
  k get pod "$POD" -o jsonpath='{.status.initContainerStatuses[?(@.name=="agentfs")].restartCount}'
}
writer_restarts() { # container
  k get pod "$POD" -o jsonpath="{.status.containerStatuses[?(@.name==\"$1\")].restartCount}"
}
sidecar_pid() {
  local cid
  cid=$(k get pod "$POD" -o jsonpath='{.status.initContainerStatuses[?(@.name=="agentfs")].containerID}')
  docker exec "$NODE" crictl inspect --output go-template --template '{{.info.pid}}' "${cid#*://}"
}
container_pid() { # container
  local cid
  cid=$(k get pod "$POD" -o jsonpath="{.status.containerStatuses[?(@.name==\"$1\")].containerID}")
  docker exec "$NODE" crictl inspect --output go-template --template '{{.info.pid}}' "${cid#*://}"
}
# SIGKILLs <pid> from the node (PID 1 ignores SIGKILL from inside its own
# namespace) and waits until it is gone.
kill_from_node() { # pid
  docker exec "$NODE" kill -KILL "$1"
  local deadline=$((SECONDS + 60))
  while docker exec "$NODE" kill -0 "$1" 2>/dev/null; do
    [ $SECONDS -lt $deadline ] || fail "the sidecar (pid $1) did not exit after SIGKILL"
    sleep 0.2
  done
  pass "SIGKILL to the sidecar (pid $1 on the node); it is gone"
}
wait_sidecar_restarted() { # previous-count
  local deadline=$((SECONDS + 180))
  until [ "$(sidecar_restarts)" -gt "$1" ]; do
    [ $SECONDS -lt $deadline ] || fail "the kubelet never restarted the sidecar"
    sleep 1
  done
  k wait --for=condition=Ready "pod/$POD" --timeout=180s >/dev/null ||
    fail "the pod never became Ready again"
  pass "the kubelet restarted the sidecar and the pod is Ready, with no manual step"
}

step "the sidecar sealed the bare mount point"
# A non-recursive bind of the emptyDir shows the directory under the FUSE
# mount, whatever is mounted on it.
POD_UID=$(k get pod "$POD" -o jsonpath='{.metadata.uid}')
EMPTYDIR=/var/lib/kubelet/pods/$POD_UID/volumes/kubernetes.io~empty-dir/agentfs-mount
docker exec "$NODE" sh -c "mkdir -p /tmp/peek && mount --bind $EMPTYDIR /tmp/peek"
sealed=$(docker exec "$NODE" stat -c '%a %u' /tmp/peek/workspace)
out=$(docker exec -u 10000:10000 "$NODE" sh -c 'printf x > /tmp/peek/workspace/x' 2>&1) && sealout=wrote || sealout=$out
docker exec "$NODE" umount /tmp/peek
assert_eq "0 0" "$sealed" "the bare mount point is root's with mode 0000"
assert_contains "$sealout" "Permission denied" "uid 10000 cannot write to the bare mount point"

step "a clean exit mid-pod: the read-only emptyDir and the sealed mount point fence the gap"
restarts=$(sidecar_restarts)
pid=$(sidecar_pid)
[ -n "$pid" ] || fail "no pid for the sidecar"
docker exec "$NODE" kill -TERM "$pid"
# The drain detaches the view's client, then unmounts the view: the
# readiness probe follows the mount.
k wait --for=condition=Ready=false "pod/$POD" --timeout=120s >/dev/null ||
  fail "the pod never turned NotReady after the sidecar's stop"
pass "the readiness probe follows the mount: the pod is NotReady"
if out=$(in_root "printf gap > $WS/gap-0.txt" 2>&1); then
  fail "a write in the gap succeeded"
fi
assert_contains "$out" "Read-only file system" "a write in the gap fails with EROFS (the read-only emptyDir)"
# The subPath writer's bind still holds the drained view: the sidecar
# waits for it, as it does for the kubelet's teardown at a pod's stop.
docker exec "$NODE" kill -0 "$pid" || fail "the sidecar exited while the subPath bind held the view"
assert_eq "$restarts" "$(sidecar_restarts)" "the drained sidecar waits for the subPath bind"
# As the kubelet does at the end of its grace.
kill_from_node "$pid"
# Restart the subPath writer inside the gap (the sidecar's restart may wait
# out the kubelet's backoff): its subPath is bound to the bare, sealed
# directory.
user_restarts=$(writer_restarts writer-10000)
docker exec "$NODE" kill -KILL "$(container_pid writer-10000)"
n=1
sub=""
subgap() { # tries the restarted subPath writer once; sets sub
  [ -z "$sub" ] || return 0
  [ "$(writer_restarts writer-10000)" -gt "$user_restarts" ] || return 0
  local sout
  if sout=$(in_user 'printf subgap > /workspace/u10000/subgap.txt' 2>&1); then
    sub=wrote
  else
    case "$sout" in
    *"Permission denied"*) sub=refused ;;
    # Not running yet, or its first bind was the dead mount.
    *"Transport endpoint is not connected"* | *"container not found"* | *"not running"* | *"stopped"* | *"unable to upgrade"*) ;;
    *) fail "the restarted subPath writer failed unexpectedly: $sout" ;;
    esac
  fi
}
until out=$(in_root "printf gap > $WS/gap-$n.txt" 2>&1); do
  case "$out" in
  *"Read-only file system"*) ;;
  *) fail "a write in the gap failed unexpectedly: $out" ;;
  esac
  subgap
  n=$((n + 1))
  [ "$n" -lt 600 ] || fail "writes never worked again"
  sleep 0.5
done
echo "$n write(s) refused in the gap; gap-$n.txt went to the new mount"
wait_sidecar_restarted "$restarts"
assert_contains "$(k logs "$POD" -c agentfs)" "mounted volume $VOLUME at $WS" "the restarted sidecar mounted the volume"
assert_not_built_logged "the restarted sidecar logged the remote as not built again"
in_root "printf post-gap > $WS/post-gap.txt"
wait_history post-gap.txt post-gap
wait_history "gap-$n.txt" gap
i=0
while [ $i -lt "$n" ]; do
  never_recorded "gap-$i.txt" "refused in the gap"
  if in_root "test -e $WS/gap-$i.txt"; then fail "gap-$i.txt is in the volume"; fi
  i=$((i + 1))
done
pass "no refused write reached the volume or its history"
deadline=$((SECONDS + 120))
until [ -n "$sub" ]; do
  [ $SECONDS -lt $deadline ] || fail "the subPath writer never came back after its restart"
  sleep 1
  subgap
done
case "$sub" in
refused)
  pass "the subPath writer restarted in the gap was refused (EACCES on the sealed mount point)"
  never_recorded u10000/subgap.txt "refused in the gap"
  ;;
wrote)
  # The sidecar was back before the writer: the write went to the new mount.
  wait_history u10000/subgap.txt subgap
  pass "the subPath writer restarted after the gap and its write was recorded"
  ;;
esac
# Its liveness probe restarts it onto the new mount (if it is not on it).
deadline=$((SECONDS + 240))
until in_user 'printf post-gap-subpath > /workspace/u10000/post-gap-subpath.txt' 2>/dev/null; do
  [ $SECONDS -lt $deadline ] || fail "the subPath writer never reached the new mount"
  sleep 2
done
k wait --for=condition=Ready "pod/$POD" --timeout=120s >/dev/null || fail "the pod never became Ready"
wait_history u10000/post-gap-subpath.txt post-gap-subpath
pass "the subPath writer restarted onto the new mount"

step "kill -9 the daemon: the restarted sidecar stacks over the stale mount in place"
in_root "printf before > $WS/before-crash.txt"
wait_history before-crash.txt before
restarts=$(sidecar_restarts)
user_restarts=$(writer_restarts writer-10000)
root_restarts=$(writer_restarts writer-root)
kill_from_node "$(sidecar_pid)"
# The kubelet restarts the sidecar at once after a first exit, and the
# restart can win the race against this write: then it must be recorded.
if out=$(in_root "printf x > $WS/during.txt" 2>&1); then
  during=recorded
  echo "the sidecar was back before the write; during.txt must be recorded"
else
  during=refused
  assert_contains "$out" "Transport endpoint is not connected" "a write after the crash fails with ENOTCONN"
fi
wait_sidecar_restarted "$restarts"
recovered=$(k logs "$POD" -c agentfs)
echo "$recovered" | grep -E '^(stacked|mounted|not built) ' || true
assert_contains "$recovered" "stacked over a stale mount at $WS left by a crashed daemon" "the sidecar stacked over the stale mount"
assert_not_built_logged "the stacked sidecar logged the remote as not built again"
assert_eq "live agentfs mount (agentfs:$VOLUME)" "$(in_sidecar agentfs probe "$WS")" "probe sees the new mount"
in_root "printf after > $WS/after-crash.txt"
in_user 'printf after-subpath > /workspace/u10000/after-crash-subpath.txt'
assert_eq after "$(in_user 'cat /workspace/after-crash.txt')" "the subPath writer reaches the new mount"
assert_eq "$user_restarts" "$(writer_restarts writer-10000)" "the subPath writer was not restarted"
assert_eq "$root_restarts" "$(writer_restarts writer-root)" "the root writer was not restarted"
wait_history after-crash.txt after
wait_history u10000/after-crash-subpath.txt after-subpath
assert_contains "$(message after-crash.txt)" "Agentfs-Pod: $POD" "the stacked view records with the pod's trailers"
wait_history before-crash.txt before
if [ "$during" = refused ]; then
  never_recorded during.txt "refused after the crash"
  if in_root "test -e $WS/during.txt"; then fail "during.txt is in the volume"; fi
  pass "the write that failed is nowhere"
else
  wait_history during.txt x
fi
echo "first pod $FIRST_POD; replaced, stopped mid-pod and recovered in place: $POD"

step "e2e-kind passed"
