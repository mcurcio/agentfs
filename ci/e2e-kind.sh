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
#   - deleting the pod makes the final commit;
#   - after `kill -9` of the daemon, writes fail with ENOTCONN, the
#     restarted sidecar holds and the pod goes NotReady, `kubectl delete
#     pod` completes, and the new pod recovers and pushes what was written
#     before the kill (test 2 in a pod); the failed write is nowhere.
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
    for c in agentfs-init agentfs writer-root writer-10000; do
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
OLD=$POD
wait_ready
[ "$POD" != "$OLD" ] || fail "the pod was not replaced"
wait_remote final.txt final
assert_contains "$(remote_message final.txt)" "Agentfs-Pod: $OLD" "final.txt was committed by the deleted pod"
assert_contains "$(k logs "$POD" -c agentfs-init)" "volume $VOLUME already initialised" "init accepted the existing state area"

step "kill -9 the daemon: the pod restart recovers (design §18 test 2)"
in_root "printf before > $WS/before-crash.txt"
cid=$(k get pod "$POD" -o jsonpath='{.status.initContainerStatuses[?(@.name=="agentfs")].containerID}')
cid=${cid#*://}
pid=$(docker exec "$NODE" crictl inspect --output go-template --template '{{.info.pid}}' "$cid")
[ -n "$pid" ] || fail "no pid for the sidecar container $cid"
# SIGKILL from the node: PID 1 ignores it from inside its own namespace.
docker exec "$NODE" kill -9 "$pid"
pass "killed the daemon (pid $pid on the node)"
deadline=$((SECONDS + 30))
until out=$(in_root "printf x > $WS/during.txt" 2>&1; echo "exit=$?") && [ "${out##*exit=}" != 0 ]; do
  [ $SECONDS -lt $deadline ] || fail "writes still succeed after the kill: $out"
  sleep 0.5
done
assert_contains "$out" "Transport endpoint is not connected" "a write after the crash fails with ENOTCONN"
deadline=$((SECONDS + 120))
until [ "$(k get pod "$POD" -o jsonpath='{.status.initContainerStatuses[?(@.name=="agentfs")].restartCount}')" -ge 1 ] &&
  k logs "$POD" -c agentfs 2>/dev/null | grep -q 'Holding until SIGTERM'; do
  [ $SECONDS -lt $deadline ] || fail "the sidecar never restarted into hold"
  sleep 1
done
pass "the restarted sidecar holds"
deadline=$((SECONDS + 60))
until [ "$(k get pod "$POD" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = False ]; do
  [ $SECONDS -lt $deadline ] || fail "the pod never went NotReady"
  sleep 1
done
pass "the pod is NotReady"
assert_eq "stale agentfs mount (daemon gone)" "$(in_sidecar agentfs probe "$WS" || true)" "probe reports the stale mount"
out=$(in_root "cat $WS/root.txt" 2>&1 || true)
assert_contains "$out" "Transport endpoint is not connected" "reads fail with ENOTCONN too"
follow_log "$POD" agentfs hold
CRASHED=$POD
CRASHED_UID=$(k get pod "$POD" -o jsonpath='{.metadata.uid}')
if ! k delete pod "$POD" --wait --timeout=120s; then
  docker exec "$NODE" sh -c "grep -E 'agentfs|fuse' /proc/self/mountinfo" || true
  fail "the crashed pod could not be deleted"
fi
pass "kubectl delete pod completed"
end_follow
assert_contains "$(cat "$LOGS/hold.log")" "detached stale mount $WS" "the holding sidecar detached the stale mount"
if docker exec "$NODE" grep "/pods/$CRASHED_UID/" /proc/self/mountinfo; then
  fail "mounts of the deleted pod were left on the node"
fi
pass "nothing of the deleted pod is left mounted on the node"
wait_ready
[ "$POD" != "$CRASHED" ] || fail "the pod was not replaced"
recovered=$(k logs "$POD" -c agentfs | grep '^mounted ')
echo "sidecar: $recovered"
assert_contains "$recovered" "uncommitted record(s) recovered" "the new sidecar ran recovery"
wait_remote before-crash.txt before
if rgit cat-file -e "$BRANCH:during.txt" 2>/dev/null; then fail "during.txt is in history"; fi
if in_sidecar test -e /data/backing/during.txt; then fail "during.txt reached the backing tree"; fi
pass "the write that failed is nowhere"
in_root "printf after > $WS/after-crash.txt"
wait_remote after-crash.txt after
assert_contains "$(remote_message after-crash.txt)" "Agentfs-Pod: $POD" "the new pod records and pushes"
echo "first pod $FIRST_POD, crashed $CRASHED, now $POD"

step "e2e-kind passed"
