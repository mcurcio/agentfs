#!/usr/bin/env bash
# End-to-end test of the phase 1 deployment (design §17.1,
# docs/deployment.md) in a kind cluster: deploys ci/k8s (the CI overlay of
# deploy/sidecar) and checks, from real pods:
#
#   - root and uid 10000 write through the mount with normal permissions
#     (design §18 test 4), the uid 10000 writer through a subPath mount of
#     the FUSE root at a fixed path;
#   - the operator rules from the ConfigMap apply, and nothing of the state
#     area is visible to the writers (test 5);
#   - commits carry the writers' uids as authors and the pod, namespace and
#     node trailers from the downward API, and reach a bare repository on
#     the runner's git server over HTTP with the credential Secret (test 8);
#   - `kubectl exec ... agentfs restore` through the control socket (test
#     11);
#   - deleting the pod makes the final commit and pushes it;
#   - after `kill -9` of the daemon, writes fail with ENOTCONN, the kubelet
#     restarts the sidecar (a write that races the recovery and succeeds
#     must be recorded), which stacks a new mount over the stale one
#     (`--on-stale-mount recover`) with no manual step; both writers
#     (HostToContainer and subPath) reach it without a restart, and
#     everything written before and after the kill reaches the remote
#     (test 2 in a pod); the failed write is nowhere;
#   - `agentfs push` in the sidecar asks the running daemon to push now;
#   - the sidecar seals the bare mount point (root's, mode 0000; checked
#     from the node through a non-recursive bind of the emptyDir);
#   - a clean exit of the sidecar mid-pod: writes in the gap fail with
#     EROFS (the workload's read-only emptyDir) and reach nothing; the
#     subPath writer, restarted inside the gap, gets EACCES from the sealed
#     mount point (or, if the sidecar was back first, writes through the
#     new mount and is recorded), its liveness probe restarts it onto the
#     new mount, and both write through the restarted sidecar's mount;
#   - deleting the recovered pod leaves nothing mounted on the node;
#   - the rollback's flush Pod of docs/runbooks/upgrading-the-sidecar.md
#     (`ci: rollback-flush`), run as written: it finds the Deployment's
#     hashed ConfigMap, flushes, and the volume mounts again after it.
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

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-e2e}
NODE=$KIND_CLUSTER-control-plane
NS=agentfs-e2e
APP=app.kubernetes.io/name=agentfs-example
VOLUME=ci-kind
BRANCH=agentfs/$VOLUME
WS=/agentfs/workspace
LOGS=${E2E_KIND_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-kind-logs}
mkdir -p "$LOGS"

k() { kubectl -n "$NS" "$@"; }

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | head -n1
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
    for c in agentfs-init agentfs writer-root writer-10000 flush; do
      echo "---- $pod $c"
      k logs "$pod" -c "$c" --tail=100
      echo "---- $pod $c (previous)"
      k logs "$pod" -c "$c" --previous --tail=100 2>/dev/null
    done
  done
  echo "---- saved logs"
  tail -n 50 "$LOGS"/*.log 2>/dev/null
  echo "---- node mounts"
  docker exec "$NODE" sh -c "grep -E 'agentfs|fuse' /proc/self/mountinfo"
  echo "---- git server"
  tail -n 40 "${GIT_SERVER_DIR:-/nonexistent}"/logs/*.log 2>/dev/null
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

# The pod of the Deployment that is not being deleted.
current_pod() {
  k get pod -l "$APP" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
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

# Waits until the remote branch has <path> with <content>.
wait_remote() { # path content [seconds]
  local deadline=$((SECONDS + ${3:-60}))
  until [ "$(rgit show "$BRANCH:$1" 2>/dev/null)" = "$2" ]; do
    [ $SECONDS -lt $deadline ] || fail "the remote never had $1 = '$2'"
    sleep 1
  done
  pass "remote has $1"
}

# The message of the newest remote commit that touched <path>.
remote_message() { rgit log -1 --format=%B "$BRANCH" -- "$1"; }
remote_author() { rgit log -1 --format=%an "$BRANCH" -- "$1"; }
remote_ident() { rgit log -1 --format="$2" "$BRANCH" -- "$1"; } # path format

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

# Follows a container's log into $LOGS/<name>.log in the background;
# FOLLOWER is the follower's pid. end_follow waits up to 30 s for it.
follow_log() { # pod container name
  k logs -f "$1" -c "$2" >"$LOGS/$3.log" 2>&1 &
  FOLLOWER=$!
}
end_follow() {
  local i
  for i in $(seq 30); do kill -0 "$FOLLOWER" 2>/dev/null || break; sleep 1; done
  kill "$FOLLOWER" 2>/dev/null || true
  wait "$FOLLOWER" 2>/dev/null || true
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
startup=$(k logs "$POD" -c agentfs | grep '^mounted ' || true)
echo "sidecar: $startup"
assert_contains "$startup" "volume $VOLUME" "the sidecar mounted the volume"
assert_contains "$(k logs "$POD" -c agentfs-init)" "initialised volume $VOLUME" "the init container created the state area"

step "root and uid 10000 write with normal permissions (design §18 test 4)"
in_root "cd $WS && printf root > root.txt && chmod 600 root.txt && mkdir u10000 && chown 10000:10000 u10000"
sleep 1
in_user 'printf user > /workspace/u10000/user.txt'
assert_eq 10000 "$(in_root "stat -c %u $WS/u10000/user.txt")" "uid 10000's file is its own"
assert_eq 600 "$(in_root "stat -c %a $WS/root.txt")" "chmod 600 persists"
if out=$(in_user 'printf x >> /workspace/root.txt' 2>&1); then
  fail "uid 10000 wrote root's 0600 file"
fi
assert_contains "$out" "Permission denied" "uid 10000 cannot write root's 0600 file"
assert_eq user "$(in_root "cat $WS/u10000/user.txt")" "root reads uid 10000's file through the other mount"

step "operator rules and invisibility (design §18 test 5)"
in_root "cd $WS && mkdir -p scratch secrets && printf x > scratch/x.log && printf k > secrets/k"
assert_eq "root.txt scratch secrets u10000" "$(in_root "ls -A $WS" | tr '\n' ' ' | sed 's/ $//')" "the writer sees only its files"
assert_eq "workspace" "$(in_root 'ls -A /agentfs')" "nothing else in the shared emptyDir"
assert_eq "root.txt scratch secrets u10000" "$(in_user 'ls -A /workspace' | tr '\n' ' ' | sed 's/ $//')" "the subPath writer sees the same"
if in_root 'test -e /data'; then fail "the writer can reach /data"; fi
pass "the writers cannot reach /data"

step "commits reach the remote with authors and trailers (design §18 test 8)"
wait_remote root.txt root
wait_remote u10000/user.txt user
assert_eq uid-0 "$(remote_author root.txt)" "root.txt authored by uid-0"
assert_eq uid-10000 "$(remote_author u10000/user.txt)" "u10000/user.txt authored by uid-10000"
# The identity templates of ci/k8s/config/config, rendered exactly.
POD_UID=$(k get pod "$POD" -o jsonpath="{.metadata.uid}")
assert_eq "$POD on $NODE_NAME" "$(remote_ident u10000/user.txt %cn)" "committer name from {pod} on {node}"
assert_eq "$POD_UID@$NS.$VOLUME.ci.invalid" "$(remote_ident u10000/user.txt %ce)" "committer email from {podUid}@{namespace}.{volume}"
assert_eq "uid-10000.gid-10000@$VOLUME.ci.invalid" "$(remote_ident u10000/user.txt %ae)" "author email from uid-{uid}.gid-{gid}"
assert_eq "uid-0.gid-0@$VOLUME.ci.invalid" "$(remote_ident root.txt %ae)" "root's author email"
msg=$(remote_message u10000/user.txt)
assert_contains "$msg" "Agentfs-Pod: $POD" "pod trailer"
assert_contains "$msg" "Agentfs-Namespace: $NS" "namespace trailer"
assert_contains "$msg" "Agentfs-Node: $NODE_NAME" "node trailer"
assert_contains "$msg" "Agentfs-Uid: 10000" "uid trailer"
sleep 3
for path in scratch/x.log secrets/k; do
  if rgit cat-file -e "$BRANCH:$path" 2>/dev/null; then fail "$path was recorded"; fi
done
pass "excluded and -agentfs paths are not in history"

step "restore through the sidecar's control socket (design §18 test 11)"
FIRST=$(rgit log --format=%H --diff-filter=A "$BRANCH" -- root.txt | tail -n1)
in_root "printf changed > $WS/root.txt"
wait_remote root.txt changed
in_sidecar agentfs restore root.txt "$FIRST"
assert_eq root "$(in_root "cat $WS/root.txt")" "the writer reads the restored content"
wait_remote root.txt root
assert_contains "$(remote_message root.txt)" "Agentfs-Restore: $FIRST" "the restore is its own commit"

step "deleting the pod makes the final commit"
follow_log "$POD" agentfs graceful
in_root "printf final > $WS/final.txt"
k delete pod "$POD" --wait --timeout=120s
end_follow
graceful=$(cat "$LOGS/graceful.log")
assert_contains "$graceful" "signal 15: unmounting" "the sidecar got SIGTERM"
assert_contains "$graceful" "final commit:" "the sidecar made its final commit"
assert_contains "$graceful" "final push: " "the sidecar pushed its final commit at shutdown"
OLD=$POD
wait_ready
[ "$POD" != "$OLD" ] || fail "the pod was not replaced"
wait_remote final.txt final
assert_contains "$(remote_message final.txt)" "Agentfs-Pod: $OLD" "final.txt was committed by the deleted pod"
assert_contains "$(k logs "$POD" -c agentfs-init)" "volume $VOLUME already initialised" "init accepted the existing state area"

# Restart counts and the sidecar's pid on the node.
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
# Signals the sidecar from the node (PID 1 ignores SIGKILL from inside its
# own namespace) and waits until it is gone.
signal_sidecar() { # signal
  local pid
  pid=$(sidecar_pid)
  [ -n "$pid" ] || fail "no pid for the sidecar"
  docker exec "$NODE" kill "-$1" "$pid"
  local deadline=$((SECONDS + 60))
  while docker exec "$NODE" kill -0 "$pid" 2>/dev/null; do
    [ $SECONDS -lt $deadline ] || fail "the sidecar (pid $pid) did not exit after SIG$1"
    sleep 0.2
  done
  pass "SIG$1 to the sidecar (pid $pid on the node); it is gone"
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

step "the sidecar sealed the bare mount point (design §17.1)"
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

step "kill -9 the daemon: the restarted sidecar recovers in place (design §18 test 2)"
in_root "printf before > $WS/before-crash.txt"
wait_remote before-crash.txt before
restarts=$(sidecar_restarts)
user_restarts=$(writer_restarts writer-10000)
signal_sidecar KILL
# The kubelet restarts the sidecar at once after a first exit, and recovery
# can win the race against this write: then it must be recorded.
if out=$(in_root "printf x > $WS/during.txt" 2>&1); then
  during=recorded
  echo "the sidecar recovered before the write; during.txt must be recorded"
else
  during=refused
  assert_contains "$out" "Transport endpoint is not connected" "a write after the crash fails with ENOTCONN"
fi
wait_sidecar_restarted "$restarts"
recovered=$(k logs "$POD" -c agentfs)
echo "$recovered" | grep -E '^(stacked|mounted) ' || true
assert_contains "$recovered" "stacked over a stale mount left by a crashed daemon at $WS" "the sidecar stacked over the stale mount"
assert_eq "live agentfs mount (agentfs:$VOLUME)" "$(in_sidecar agentfs probe "$WS")" "probe sees the new mount"
in_root "printf after > $WS/after-crash.txt"
in_user 'printf after-subpath > /workspace/u10000/after-crash-subpath.txt'
assert_eq after "$(in_user 'cat /workspace/after-crash.txt')" "the subPath writer reaches the new mount"
assert_eq "$user_restarts" "$(writer_restarts writer-10000)" "the subPath writer was not restarted"
out=$(in_sidecar agentfs push --timeout 60s) || fail "agentfs push through the daemon failed: $out"
echo "$out"
assert_contains "$out" "through the running daemon (pid " "agentfs push asked the running daemon"
assert_contains "$out" "origin: " "agentfs push reported the remote"
wait_remote after-crash.txt after
wait_remote u10000/after-crash-subpath.txt after-subpath
assert_contains "$(remote_message after-crash.txt)" "Agentfs-Pod: $POD" "the recovered daemon records and pushes"
rgit cat-file -e "$BRANCH:before-crash.txt" || fail "before-crash.txt left history"
if [ "$during" = refused ]; then
  if rgit cat-file -e "$BRANCH:during.txt" 2>/dev/null; then fail "during.txt is in history"; fi
  if in_sidecar test -e /data/backing/during.txt; then fail "during.txt reached the backing tree"; fi
  pass "the write that failed is nowhere"
else
  wait_remote during.txt x
fi

step "a clean exit mid-pod: the read-only emptyDir and the sealed mount point fence the gap"
restarts=$(sidecar_restarts)
user_restarts=$(writer_restarts writer-10000)
signal_sidecar TERM
# Restart the subPath writer inside the gap (the sidecar's second restart
# waits out the kubelet's backoff): its subPath is bound to the bare,
# sealed directory.
docker exec "$NODE" kill -KILL "$(container_pid writer-10000)"
gaps=0
n=0
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
  *"Read-only file system"* | *"Transport endpoint is not connected"*) ;;
  *) fail "a write in the gap failed unexpectedly: $out" ;;
  esac
  subgap
  gaps=$((gaps + 1))
  n=$((n + 1))
  [ $gaps -lt 600 ] || fail "writes never worked again"
  sleep 0.5
done
echo "$gaps write(s) refused in the gap; gap-$n.txt went to the new mount"
[ "$gaps" -ge 1 ] || fail "no write was refused in the gap"
pass "writes in the gap fail (EROFS) instead of landing unrecorded"
wait_sidecar_restarted "$restarts"
previous=$(k logs "$POD" -c agentfs --previous)
assert_contains "$previous" "final commit:" "the stopped sidecar made its final commit"
assert_contains "$previous" "detached 1 stale layer(s) at $WS" "it detached the layer the crash left"
in_root "printf post-gap > $WS/post-gap.txt"
wait_remote post-gap.txt post-gap
wait_remote "gap-$n.txt" gap
i=0
while [ $i -lt "$n" ]; do
  if rgit cat-file -e "$BRANCH:gap-$i.txt" 2>/dev/null; then fail "gap-$i.txt is in history"; fi
  if in_sidecar test -e "/data/backing/gap-$i.txt"; then fail "gap-$i.txt reached the backing tree"; fi
  i=$((i + 1))
done
pass "no refused write reached history or the backing tree"
deadline=$((SECONDS + 120))
until [ -n "$sub" ]; do
  [ $SECONDS -lt $deadline ] || fail "the subPath writer never came back after its restart"
  sleep 1
  subgap
done
case "$sub" in
refused)
  pass "the subPath writer restarted in the gap was refused (EACCES on the sealed mount point)"
  if in_sidecar test -e /data/backing/u10000/subgap.txt; then fail "subgap.txt reached the backing tree"; fi
  if rgit cat-file -e "$BRANCH:u10000/subgap.txt" 2>/dev/null; then fail "subgap.txt is in history"; fi
  ;;
wrote)
  # The sidecar was back before the writer: the write went to the new mount.
  wait_remote u10000/subgap.txt subgap
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
wait_remote u10000/post-gap-subpath.txt post-gap-subpath
pass "the subPath writer restarted onto the new mount"

step "deleting the recovered pod leaves nothing mounted"
follow_log "$POD" agentfs last
LAST_UID=$(k get pod "$POD" -o jsonpath='{.metadata.uid}')
k delete pod "$POD" --wait --timeout=120s || fail "the pod could not be deleted"
end_follow
assert_contains "$(cat "$LOGS/last.log")" "final push: " "the last sidecar pushed at shutdown"
if docker exec "$NODE" grep "/pods/$LAST_UID/" /proc/self/mountinfo; then
  fail "mounts of the deleted pod were left on the node"
fi
pass "nothing of the deleted pod is left mounted on the node"
echo "first pod $FIRST_POD, recovered in place: $POD"

step "the rollback's flush Pod runs as written (docs/runbooks/upgrading-the-sidecar.md)"
wait_ready
CONFIG=$(k get deployment agentfs-example -o jsonpath='{.spec.template.spec.volumes[?(@.name=="config")].configMap.name}')
case "$CONFIG" in
agentfs-config-?*) pass "the ConfigMap's name carries kustomize's hash ($CONFIG)" ;;
*) fail "the ConfigMap is named '$CONFIG', not agentfs-config-<hash>" ;;
esac
# The runbook's kubectl, in this namespace, with the image under test. A
# plain subshell, so set -e stops at the block's first failure (diagnose
# prints the saved log and describes the flush Pod).
(
  kubectl() { command kubectl -n "$NS" "$@"; }
  POD=pod/$POD
  NEWER=agentfs:ci
  run_block upgrading-the-sidecar.md rollback-flush
) >"$LOGS/rollback-flush.log" 2>&1
cat "$LOGS/rollback-flush.log"
assert_contains "$(cat "$LOGS/rollback-flush.log")" "record(s) into" "agentfs flush ran in the flush Pod"
k scale deployment/agentfs-example --replicas=1
wait_ready
in_root "printf after-flush > $WS/after-flush.txt"
wait_remote after-flush.txt after-flush

step "e2e-kind passed"
