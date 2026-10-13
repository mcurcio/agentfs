#!/usr/bin/env bash
# End-to-end test of the CSI driver (design §14.2, docs/csi.md) in a kind
# cluster: deploys ci/k8s-csi/driver (the CI overlay of deploy/csi) and
# ci/k8s-csi/apps, and checks, from real pods:
#
#   - deploy/csi passes server-side validation, the driver registers on
#     the node, and the RBAC inventory holds (`kubectl auth can-i`);
#   - two restricted pods (uid 10000 and 10001, one fsGroup) on one RWO
#     claim both write; the remote branch on the runner's git server gets
#     commits from both, with distinct committers, pod trailers and uid
#     authors; `agentfs status` in the node plugin shows the service and
#     two fronts;
#   - kill -9 of writer-a's front: writer-b is unaffected, writer-a's
#     front restarts in the same slot and its mount comes back live
#     (HostToContainer), and what it writes is pushed;
#   - kill -9 of the history service right after a write: the write is
#     committed and pushed after the restart, and writes resume;
#   - deleting the node plugin pod: the new one reconciles, writer-a
#     recovers live, writer-b after a container restart;
#   - an inline (ephemeral) volume and a ReadWriteMany claim are refused;
#   - deleting the pods unpublishes and unstages (no mounts left, the
#     socket gone, the final commit made), and deleting the claim removes
#     the volume directory.
#
# Needs docker, kind and kubectl (ci/install-kind.sh), a kind cluster
# $KIND_CLUSTER (default agentfs-csi) with the image agentfs:ci loaded,
# and the git server from ci/git-http-server.sh started with
# GIT_SERVER_EXTRA_LISTEN set to the kind network's gateway
# (`ci/e2e-csi.sh gateway`), its output in the environment. Locally:
#
#   docker build -t agentfs:ci .
#   kind create cluster --name agentfs-csi --config ci/kind/cluster.yaml --image "$KIND_NODE_IMAGE"
#   kind load docker-image agentfs:ci --name agentfs-csi
#   eval "$(GIT_SERVER_EXTRA_LISTEN=$(ci/e2e-csi.sh gateway) ci/git-http-server.sh start /tmp/gs | sed 's/^/export /')"
#   ci/e2e-csi.sh            # ci/e2e-csi.sh diagnose after a failure
#
# The FUSE module must be loaded on the host (modprobe fuse).

. "$(dirname "$0")/lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-csi}
NODE=$KIND_CLUSTER-control-plane
DNS=agentfs-csi
APPS=agentfs-csi-e2e-apps
DRIVER=agentfs.mcurcio.github.io
VOLUME=$APPS.shared
BRANCH=agentfs/$VOLUME
BASE=/var/lib/agentfs-csi/volumes
NODE_STATE=/var/lib/agentfs-csi/node
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
LOGS=${E2E_CSI_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-csi-kind-logs}
mkdir -p "$LOGS"

kd() { kubectl -n "$DNS" "$@"; }
ka() { kubectl -n "$APPS" "$@"; }

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | head -n1
}

diagnose() {
  set +e
  step "diagnose"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  kubectl get csidriver,csinode,volumeattachments,pv -o wide
  ka get pvc,pods -o wide
  ka describe pods
  kd describe pods
  ka get events --sort-by=.lastTimestamp | tail -n 60
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    for c in agentfs-csi csi-provisioner csi-attacher node-driver-registrar; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=100 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  echo "---- saved logs"
  tail -n 100 "$LOGS"/*.log 2>/dev/null
  echo "---- node mounts"
  docker exec "$NODE" sh -c "grep -E 'agentfs|fuse|kubernetes.io~csi' /proc/self/mountinfo"
  echo "---- node processes"
  docker exec "$NODE" sh -c "ps -eo pid,args | grep -E 'agentfs|tini' | grep -v grep"
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

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

# Waits until the remote branch has <path> with <content>.
wait_remote() { # path content [seconds]
  local deadline=$((SECONDS + ${3:-90}))
  until [ "$(rgit show "$BRANCH:$1" 2>/dev/null)" = "$2" ]; do
    [ $SECONDS -lt $deadline ] || fail "the remote never had $1 = '$2'"
    sleep 1
  done
  pass "remote has $1"
}

# Of the newest remote commit that touched <path>.
remote_committer() { rgit log -1 --format=%cn "$BRANCH" -- "$1"; }
remote_committer_email() { rgit log -1 --format=%ce "$BRANCH" -- "$1"; }
remote_author() { rgit log -1 --format=%an "$BRANCH" -- "$1"; }
remote_message() { rgit log -1 --format=%B "$BRANCH" -- "$1"; }

plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }
plugin_log() { kd logs "$(plugin_pod)" -c agentfs-csi; }

# Runs a shell command in a writer.
in_pod() { ka exec "$1" -- sh -c "$2"; }

# Waits up to <seconds> for <pod> to write <file> (the mount works).
wait_writable() { # pod file content seconds
  local deadline=$((SECONDS + $4)) out
  until out=$(in_pod "$1" "printf '$3' > /data/$2" 2>&1); do
    [ $SECONDS -lt $deadline ] || fail "$1 could not write $2 within $4 s: $out"
    sleep 1
  done
  pass "$1 wrote $2"
}

# Waits until no write of <pod> succeeds (its mount is broken).
wait_broken() { # pod seconds
  local deadline=$((SECONDS + $2))
  while in_pod "$1" "printf x > /data/probe-$1.txt" >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || fail "$1's writes still succeed"
    sleep 0.5
  done
  pass "$1's mount is broken"
}

# Waits for an event on <object> whose message contains <text>.
wait_event() { # object-name reason text seconds
  local deadline=$((SECONDS + $4)) events
  until events=$(ka get events --field-selector "involvedObject.name=$1,reason=$2" \
    -o jsonpath='{range .items[*]}{.message}{"\n"}{end}' 2>/dev/null) &&
    grep -qi -- "$3" <<<"$events"; do
    [ $SECONDS -lt $deadline ] || fail "no $2 event on $1 naming '$3' (got: $events)"
    sleep 2
  done
  pass "$1: $2 names '$3'"
}

# SIGKILLs every process on the node whose command line matches the glob
# <pattern>; fails if there is none. (The node image has no pkill.)
node_kill() { # pattern
  docker exec "$NODE" sh -c '
    found=1
    for d in /proc/[0-9]*; do
      [ "${d#/proc/}" = $$ ] && continue
      c=$(tr "\\0" " " <"$d/cmdline" 2>/dev/null) || continue
      case "$c" in
        $1) echo "kill -9 ${d#/proc/}: $c"; kill -9 "${d#/proc/}" && found=0 ;;
      esac
    done
    exit $found' sh "$1"
}

# `yes` or `no` for the service account <sa> of the driver's namespace.
can_i() { # sa verb resource [namespace]
  kubectl auth can-i "$2" "$3" --as "system:serviceaccount:$DNS:$1" -n "${4:-$DNS}" 2>/dev/null || true
}

step "git server and cluster"
rm -rf "$REMOTE"
git init -q --bare "$REMOTE"
set_password agentfs ci-csi-password
GW=$(gateway)
[ -n "$GW" ] || fail "no IPv4 gateway on the kind network"
kubectl get nodes -o wide
NODE_NAME=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')

step "(a) deploy/csi passes server-side validation"
kubectl apply -f deploy/csi/namespace.yaml
kubectl apply --dry-run=server -k deploy/csi >/dev/null
pass "kubectl apply --dry-run=server -k deploy/csi"

step "(b) deploy the driver"
kubectl apply -f - <<EOF
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: git-server-runner
  namespace: $DNS
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
kubectl apply -k ci/k8s-csi/driver
kd rollout status deployment/agentfs-csi-controller --timeout=300s
deadline=$((SECONDS + 300))
until [ -n "$(plugin_pod)" ]; do
  [ $SECONDS -lt $deadline ] || fail "no node plugin pod appeared"
  sleep 1
done
kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
  fail "the node plugin never became Ready"
pass "the node plugin is Ready"
deadline=$((SECONDS + 120))
until kubectl get csinode "$NODE_NAME" -o jsonpath='{.spec.drivers[*].name}' | grep -qw "$DRIVER"; do
  [ $SECONDS -lt $deadline ] || fail "the CSINode of $NODE_NAME never listed $DRIVER"
  sleep 1
done
pass "CSINode $NODE_NAME lists $DRIVER"
assert_eq "$NODE_NAME" "$(kubectl get csinode "$NODE_NAME" -o jsonpath="{.spec.drivers[?(@.name==\"$DRIVER\")].nodeID}")" \
  "the node id is the node's name"

step "(c) the RBAC inventory (design §14.2)"
for check in "get pods" "list secrets" "get persistentvolumes" "create events" "list nodes" "get volumeattachments.storage.k8s.io"; do
  # shellcheck disable=SC2086
  assert_eq no "$(can_i agentfs-csi-node $check)" "the node plugin's account cannot $check"
done
for check in "get secrets" "list secrets" "create pods" "get pods"; do
  # shellcheck disable=SC2086
  assert_eq no "$(can_i agentfs-csi-controller $check)" "the controller's account cannot $check"
  # shellcheck disable=SC2086
  assert_eq no "$(can_i agentfs-csi-controller $check $APPS)" "the controller's account cannot $check in $APPS"
done
assert_eq yes "$(can_i agentfs-csi-controller create persistentvolumes)" "the controller's account creates PVs"
assert_eq yes "$(can_i agentfs-csi-controller patch volumeattachments/status)" "the controller's account patches attachment status"
assert_eq yes "$(can_i agentfs-csi-controller update leases.coordination.k8s.io)" "the controller's account updates its leases"
assert_eq no "$(can_i agentfs-csi-controller update leases.coordination.k8s.io kube-system)" "but not elsewhere"

step "(d) two restricted pods on one claim"
kubectl apply -k ci/k8s-csi/apps
ka wait --for=condition=Ready pod/writer-a pod/writer-b --timeout=300s >/dev/null ||
  fail "the writers never became Ready"
pass "writer-a and writer-b are Ready (admitted under restricted)"
PV=$(ka get pvc shared -o jsonpath='{.spec.volumeName}')
[ -n "$PV" ] || fail "the claim is not bound"
STATE=$BASE/$PV/state
SOCK=$NODE_STATE/run/$PV/history.sock
UID_A=$(ka get pod writer-a -o jsonpath='{.metadata.uid}')
UID_B=$(ka get pod writer-b -o jsonpath='{.metadata.uid}')
echo "volume $PV, writer-a $UID_A, writer-b $UID_B"
assert_eq "$NODE_NAME" "$(kubectl get volumeattachments -o jsonpath="{.items[?(@.spec.source.persistentVolumeName==\"$PV\")].spec.nodeName}")" \
  "the volume is attached to the node"

step "(e) both pods write; commits from both reach the remote"
in_pod writer-a 'printf a > /data/a.txt && mkdir -p /data/shared && printf a > /data/shared/from-a.txt'
in_pod writer-b 'printf b > /data/b.txt && printf b > /data/shared/from-b.txt'
assert_eq a "$(in_pod writer-b 'cat /data/a.txt')" "writer-b reads writer-a's file"
assert_eq "10000 10000" "$(in_pod writer-a 'stat -c "%u %g" /data/a.txt')" "writer-a's file is uid 10000, fsGroup 10000"
assert_eq "10001 10000" "$(in_pod writer-b 'stat -c "%u %g" /data/b.txt')" "writer-b's file is uid 10001, fsGroup 10000"
wait_remote a.txt a
wait_remote b.txt b
wait_remote shared/from-a.txt a
wait_remote shared/from-b.txt b
assert_eq writer-a "$(remote_committer a.txt)" "a.txt committed by writer-a"
assert_eq writer-b "$(remote_committer b.txt)" "b.txt committed by writer-b"
email_a=$(remote_committer_email a.txt)
email_b=$(remote_committer_email b.txt)
[ "$email_a" != "$email_b" ] || fail "the committers share an email: $email_a"
pass "distinct committer emails ($email_a, $email_b)"
assert_contains "$email_a" "$UID_A" "writer-a's email carries its pod uid"
assert_eq uid-10000 "$(remote_author a.txt)" "a.txt authored by uid-10000"
assert_eq uid-10001 "$(remote_author b.txt)" "b.txt authored by uid-10001"
msg=$(remote_message b.txt)
assert_contains "$msg" "Agentfs-Pod: writer-b" "pod trailer"
assert_contains "$msg" "Agentfs-Namespace: $APPS" "namespace trailer"
assert_contains "$msg" "Agentfs-Node: $NODE_NAME" "node trailer"
status=$(in_plugin agentfs --state "$STATE" status)
echo "$status"
assert_contains "$status" "history:   serving fronts at unix:$SOCK" "status shows the history service"
assert_contains "$status" "pod=writer-a" "status lists writer-a's front"
assert_contains "$status" "pod=writer-b" "status lists writer-b's front"
SLOT_A=$(sed -n 's/^fronts: *slot \([0-9]*\): .*pod=writer-a .*/\1/p' <<<"$status")
[ -n "$SLOT_A" ] || fail "no slot for writer-a in: $status"

step "(f) kill -9 writer-a's front: it restarts in its slot, live"
node_kill "*--pod-uid $UID_A *" || fail "no front of writer-a to kill"
pass "killed writer-a's front"
wait_broken writer-a 30
in_pod writer-b 'printf during > /data/during-front-crash.txt'
pass "writer-b keeps working"
wait_writable writer-a after-crash.txt after 60
wait_remote after-crash.txt after
assert_eq writer-a "$(remote_committer after-crash.txt)" "after-crash.txt committed by writer-a"
wait_remote during-front-crash.txt during
log=$(plugin_log)
assert_contains "$log" "front for $APPS/writer-a exited" "the plugin saw the front exit"
assert_contains "$log" "restarted in slot $SLOT_A" "the front restarted in slot $SLOT_A"

step "(g) kill -9 the history service right after a write"
in_pod writer-b 'printf journaled > /data/before-service-crash.txt'
node_kill "*--state $STATE --root * serve-history *" || fail "no history service to kill"
pass "killed the history service"
wait_remote before-service-crash.txt journaled 120
assert_eq writer-b "$(remote_committer before-service-crash.txt)" "the journaled write keeps its committer"
wait_writable writer-a after-service-crash.txt again 60
wait_remote after-service-crash.txt again
assert_contains "$(plugin_log)" "service for $PV exited" "the plugin restarted the service"

step "(h) restart the node plugin: reconcile"
OLD_PLUGIN=$(plugin_pod)
kd logs -f "$OLD_PLUGIN" -c agentfs-csi >"$LOGS/old-plugin.log" 2>&1 &
FOLLOWER=$!
kd delete pod "$OLD_PLUGIN" --wait --timeout=120s
for _ in $(seq 30); do kill -0 "$FOLLOWER" 2>/dev/null || break; sleep 1; done
kill "$FOLLOWER" 2>/dev/null || true
deadline=$((SECONDS + 300))
until NEW_PLUGIN=$(plugin_pod) && [ -n "$NEW_PLUGIN" ] && [ "$NEW_PLUGIN" != "$OLD_PLUGIN" ]; do
  [ $SECONDS -lt $deadline ] || fail "the node plugin was not recreated"
  sleep 1
done
kd wait --for=condition=Ready "pod/$NEW_PLUGIN" --timeout=300s >/dev/null ||
  fail "the new node plugin never became Ready"
pass "new node plugin $NEW_PLUGIN"
log=$(plugin_log)
assert_contains "$log" "reconcile: started the history service for $PV" "reconcile restarted the service"
assert_contains "$log" "reconcile: recovering the front for $APPS/writer-a" "reconcile recovers writer-a's front"
assert_contains "$log" "reconcile: recovering the front for $APPS/writer-b" "reconcile recovers writer-b's front"
wait_writable writer-a after-restart-a.txt a2 90
ka exec writer-b -- sh -c 'kill 1' || true
deadline=$((SECONDS + 120))
until [ "$(ka get pod writer-b -o jsonpath='{.status.containerStatuses[0].restartCount}')" -ge 1 ]; do
  [ $SECONDS -lt $deadline ] || fail "writer-b's container never restarted"
  sleep 1
done
ka wait --for=condition=Ready pod/writer-b --timeout=120s >/dev/null || fail "writer-b never became Ready again"
wait_writable writer-b after-restart-b.txt b2 60
wait_remote after-restart-a.txt a2
wait_remote after-restart-b.txt b2
assert_eq writer-b "$(remote_committer after-restart-b.txt)" "writer-b still commits as itself"

step "(i) inline volumes and ReadWriteMany are refused"
kubectl apply -k ci/k8s-csi/negative
wait_event inline FailedMount Ephemeral 180
wait_event rwx ProvisioningFailed multi-node 180
kubectl delete -k ci/k8s-csi/negative --wait --timeout=120s

step "(j) teardown: unpublish, unstage, delete"
in_pod writer-a 'printf final > /data/final.txt'
ka delete pod writer-a writer-b --wait --timeout=180s
deadline=$((SECONDS + 180))
while docker exec "$NODE" grep -q "kubernetes.io~csi/$PV" /proc/self/mountinfo; do
  [ $SECONDS -lt $deadline ] || fail "mounts of $PV are left on the node"
  sleep 1
done
pass "no mounts of $PV left on the node"
deadline=$((SECONDS + 120))
while docker exec "$NODE" test -e "$SOCK"; do
  [ $SECONDS -lt $deadline ] || fail "the history socket $SOCK was not removed"
  sleep 1
done
pass "the history service stopped and removed its socket"
assert_eq final "$(in_plugin git --git-dir "$STATE/git" show "$BRANCH:final.txt")" "the final commit is in the volume's history"
assert_contains "$(plugin_log)" "unstaged volume $PV" "the volume was unstaged"
ka delete pvc shared --wait --timeout=180s
deadline=$((SECONDS + 180))
while docker exec "$NODE" test -e "$BASE/$PV"; do
  [ $SECONDS -lt $deadline ] || fail "the volume directory $BASE/$PV was not removed"
  sleep 1
done
pass "DeleteVolume removed $BASE/$PV (onDelete: delete)"

step "e2e-csi passed"
