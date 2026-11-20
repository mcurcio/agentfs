#!/usr/bin/env bash
# End-to-end test of the CSI driver (docs/csi.md) on the tree engine in a
# kind cluster: deploys ci/k8s-csi/driver (the CI overlay of deploy/csi)
# and ci/k8s-csi/apps, and checks, from real pods:
#
#   (a) deploy/csi passes server-side validation;
#   (b) the driver registers on the node with its topology;
#   (c) the RBAC inventory holds (`kubectl auth can-i`: the node plugin's
#       account is csi-provisioner's, and its token reaches no other
#       container), the class admission policy refuses a class with reclaim
#       policy Retain, and the provisioner policy refuses that account
#       another driver's PV, and this driver's without a token naming the
#       PV's node (every claim below is provisioned and deleted through it,
#       with the real token);
#   (d) one restricted writer (uid 10000, primary group 3000, fsGroup 3001)
#       on a RWO claim, its PV pinned to the node by the driver's topology;
#       one `agentfs host` runs the volume, and the writer's /data is a view
#       of it (source agentfs:<volume>) at the kubelet's target;
#   (e) the writer's files, created through its fsGroup, are in the
#       volume's history (`<base>/<pv>/state/history.git`, ref root), with
#       Agentfs-Pod and Agentfs-Namespace trailers naming the pod;
#   (f) the profile's remote is logged as not built at the host's boot, and
#       the runner's git server never gets a ref;
#   (g) a second pod on the claim gets a FailedMount event: several pods on
#       one volume are not built;
#   (h) kill -9 of the volume's host: the plugin starts a new one, the
#       writer's mount comes back live (HostToContainer), and a new write
#       is recorded;
#   (i) deleting the node plugin pod: the new one reconciles, the writer's
#       mount comes back live, and a new write is recorded;
#   (j) an inline (ephemeral) volume and a ReadWriteMany claim are refused,
#       and claims of classes with onDelete: delete and startFrom stay
#       Pending, each with a ProvisioningFailed event naming the refusal;
#   (k) deleting the writer leaves no view of the volume on the node and no
#       host; deleting the claim deletes the PV and keeps the volume's
#       directory (csi.retained.json) and its history.
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
# CSI_INSTALL=chart tests a driver installed beforehand from
# charts/agentfs-csi (ci/helm-csi-install.sh, the helm workflow's e2e
# job) instead of deploying ci/k8s-csi/driver; every check is the same.
#
# The FUSE module must be loaded on the host (modprobe fuse).

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-csi}
NODE=$KIND_CLUSTER-control-plane
DNS=agentfs-csi
APPS=agentfs-csi-e2e-apps
DRIVER=agentfs.mcurcio.github.io
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
LOGS=${E2E_CSI_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-csi-kind-logs}
mkdir -p "$LOGS"

kd() { kubectl -n "$DNS" "$@"; }
ka() { kubectl -n "$APPS" "$@"; }

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | sed -n 1p
}

diagnose() {
  set +e
  step "diagnose"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  kubectl get csidriver,csinode,pv -o wide
  ka get pvc,pods -o wide
  ka describe pods
  ka describe pvc
  kd describe pods
  ka get events --sort-by=.lastTimestamp | tail -n 60
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    for c in agentfs-csi csi-provisioner node-driver-registrar; do
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
  echo "---- volumes on the node"
  docker exec "$NODE" sh -c "ls -la $BASE $BASE/* 2>&1"
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

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
}
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }
plugin_log() { kd logs "$(plugin_pod)" -c agentfs-csi; }

# git on the volume's history, in the node plugin.
hgit() { in_plugin git --git-dir "$BASE/$PV/state/history.git" "$@"; }
# Where <path>'s bytes are in a tree commit (docs/trees.md).
file_rev() { echo "root:files/$(tree_git_fanned "$1")"; }

# History is built behind the calls: waits until the root's history has
# <path> with <content>.
wait_history() { # path content [seconds]
  local deadline=$((SECONDS + ${3:-90}))
  until [ "$(hgit show "$(file_rev "$1")" 2>/dev/null)" = "$2" ]; do
    [ $SECONDS -lt $deadline ] || fail "the volume's history never had $1 = '$2'"
    sleep 1
  done
  pass "the history has $1"
}
# The message of the newest root commit that recorded <path>.
history_message() { hgit log -1 --format=%B root -- "files/$(tree_git_fanned "$1")"; }
# The uid the history records <path> with (lines/ metadata).
history_uid() {
  hgit show "root:lines/$(tree_git_fanned "$1")" | grep -o '"uid":[0-9]*' | cut -d: -f2
}

# Runs a shell command in a pod of the apps namespace.
in_pod() { ka exec "$1" -- sh -c "$2"; }

# Waits until the writer reads a.txt, which only a live view of the volume
# shows: its mount is back.
wait_live() { # seconds
  local deadline=$((SECONDS + $1))
  until [ "$(in_pod writer 'cat /data/a.txt' 2>/dev/null)" = a ]; do
    [ $SECONDS -lt $deadline ] || fail "the writer's view never came back within $1 s"
    sleep 1
  done
  pass "the writer's view is live"
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

# The pids and command lines of the processes on the node whose command
# line matches the glob <pattern>, one per line. (The node image has no
# pgrep.)
node_procs() { # pattern
  docker exec "$NODE" sh -c '
    for d in /proc/[0-9]*; do
      [ "${d#/proc/}" = $$ ] && continue
      c=$(tr "\\0" " " <"$d/cmdline" 2>/dev/null) || continue
      case "$c" in
        $1) echo "${d#/proc/} $c" ;;
      esac
    done' sh "$1"
}

# SIGKILLs every process on the node whose command line matches the glob
# <pattern>; fails if there is none.
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

# The volume's host: `agentfs --state <base>/<pv>/state host ...`.
host_pattern() { echo "*--state $BASE/$PV/state host *"; }
host_pids() { node_procs "$(host_pattern)" | cut -d' ' -f1; }
# Waits until exactly one host of the volume runs, other than pid <old>;
# sets HOST_PID.
wait_host() { # [old-pid] [seconds]
  local deadline=$((SECONDS + ${2:-120})) pids
  while :; do
    pids=$(host_pids)
    if [ "$(printf '%s' "$pids" | grep -c .)" = 1 ] && [ "$pids" != "${1:-}" ]; then
      HOST_PID=$pids
      break
    fi
    [ $SECONDS -lt $deadline ] || fail "no single new host of $PV (old: ${1:-none}; now: $(node_procs "$(host_pattern)"))"
    sleep 1
  done
  pass "one host of $PV runs (pid $HOST_PID)"
}

# How many times the plugin's log has the host's not-built line for the
# profile's remote: once per boot of the host under this plugin.
not_built_lines() {
  plugin_log | grep -cF "[host $PV] not built: remote.origin.url (push): " || true
}

# The node's mounts whose source is the volume's view (agentfs:<volume>).
view_mounts() { docker exec "$NODE" grep -F " agentfs:$VOLUME" /proc/self/mountinfo || true; }

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
case "${CSI_INSTALL:-kustomize}" in
kustomize) kubectl apply -k ci/k8s-csi/driver ;;
chart) helm status agentfs-csi -n "$DNS" >/dev/null || fail "no agentfs-csi release (ci/helm-csi-install.sh)" ;;
*) fail "CSI_INSTALL is kustomize or chart, not $CSI_INSTALL" ;;
esac
deadline=$((SECONDS + 300))
until [ -n "$(plugin_pod)" ]; do
  [ $SECONDS -lt $deadline ] || fail "no node plugin pod appeared"
  sleep 1
done
kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
  fail "the node plugin never became Ready"
pass "the node plugin is Ready"
deadline=$((SECONDS + 120))
until kubectl get csinode "$NODE_NAME" -o jsonpath='{.spec.drivers[*].name}' | grep -w "$DRIVER" >/dev/null; do
  [ $SECONDS -lt $deadline ] || fail "the CSINode of $NODE_NAME never listed $DRIVER"
  sleep 1
done
pass "CSINode $NODE_NAME lists $DRIVER"
assert_eq "$NODE_NAME" "$(kubectl get csinode "$NODE_NAME" -o jsonpath="{.spec.drivers[?(@.name==\"$DRIVER\")].nodeID}")" \
  "the node id is the node's name"
TOPOLOGY=topology.agentfs.mcurcio.github.io/node
assert_eq "$TOPOLOGY" "$(kubectl get csinode "$NODE_NAME" -o jsonpath="{.spec.drivers[?(@.name==\"$DRIVER\")].topologyKeys[0]}")" \
  "the driver's topology key"
deadline=$((SECONDS + 60))
until [ "$(kubectl get node "$NODE_NAME" -o go-template="{{index .metadata.labels \"$TOPOLOGY\"}}")" = "$NODE_NAME" ]; do
  [ $SECONDS -lt $deadline ] || fail "the node never got the label $TOPOLOGY=$NODE_NAME"
  sleep 1
done
pass "the node is labelled $TOPOLOGY=$NODE_NAME"

step "(c) the RBAC inventory"
for check in "get secrets" "list secrets" "create pods" "get pods" "get volumeattachments.storage.k8s.io" "update leases.coordination.k8s.io"; do
  # shellcheck disable=SC2086
  assert_eq no "$(can_i agentfs-csi-node $check)" "the node plugin's account cannot $check"
  # shellcheck disable=SC2086
  assert_eq no "$(can_i agentfs-csi-node $check $APPS)" "the node plugin's account cannot $check in $APPS"
done
assert_eq yes "$(can_i agentfs-csi-node create persistentvolumes)" "csi-provisioner (the node plugin's account) creates PVs"
assert_eq yes "$(can_i agentfs-csi-node list nodes)" "and reads the nodes' topology"
kd get serviceaccount agentfs-csi-controller >/dev/null 2>&1 &&
  fail "an agentfs-csi-controller account exists: there is no controller Deployment"
pass "no controller account"
in_plugin test ! -e /var/run/secrets/kubernetes.io/serviceaccount/token ||
  fail "the agentfs-csi container holds an API token"
pass "the agentfs-csi container holds no API token"
denied=
deadline=$((SECONDS + 90))
while [ $SECONDS -lt $deadline ]; do
  if out=$(kubectl apply -f - 2>&1 <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: agentfs-ci-retain
provisioner: $DRIVER
parameters: { profile: ci }
reclaimPolicy: Retain
EOF
  ); then
    kubectl delete storageclass agentfs-ci-retain --ignore-not-found >/dev/null
    sleep 3
    continue
  fi
  denied=$out
  break
done
[ -n "$denied" ] || fail "a class with reclaimPolicy Retain was admitted"
assert_contains "$denied" "agentfs-class" "a class with reclaimPolicy Retain is denied by the class policy"
# RBAC lets the node plugin's account delete any PV; the provisioner
# policy narrows it to this driver's PVs on the token's own node. As that
# account (impersonated: no node in the token), a server-side dry run.
pv_as_node() { # driver
  kubectl create --dry-run=server --as "system:serviceaccount:$DNS:agentfs-csi-node" -f - 2>&1 <<EOF
apiVersion: v1
kind: PersistentVolume
metadata:
  name: agentfs-ci-foreign
spec:
  capacity: { storage: 1Gi }
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  nodeAffinity:
    required:
      nodeSelectorTerms:
        - matchExpressions:
            - { key: $TOPOLOGY, operator: In, values: [$NODE_NAME] }
  csi: { driver: $1, volumeHandle: agentfs-ci-foreign }
EOF
}
denied=
deadline=$((SECONDS + 90))
while [ $SECONDS -lt $deadline ]; do
  if ! out=$(pv_as_node other.example.com); then
    denied=$out
    break
  fi
  sleep 3
done
[ -n "$denied" ] || fail "the node plugin's account may create another driver's PV"
assert_contains "$denied" "agentfs-provisioner" "another driver's PV is denied to the node plugin's account"
assert_contains "$denied" "PVs of the agentfs driver" "named as another driver's"
out=$(pv_as_node "$DRIVER") && fail "a PV created with a token naming no node was admitted: $out"
assert_contains "$out" "pinned to its own node" "this driver's PV needs a token bound to a pod on its node"

step "(d) one restricted writer on a claim"
kubectl apply -k ci/k8s-csi/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null ||
  fail "the writer never became Ready"
pass "the writer is Ready (admitted under restricted)"
PV=$(ka get pvc shared -o jsonpath='{.spec.volumeName}')
[ -n "$PV" ] || fail "the claim is not bound"
WRITER_UID=$(ka get pod writer -o jsonpath='{.metadata.uid}')
assert_eq "$TOPOLOGY $NODE_NAME" \
  "$(kubectl get pv "$PV" -o jsonpath='{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].key} {.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}')" \
  "the PV is pinned to the node that holds its volume"
created=$(plugin_log | grep -F "created volume $PV on node $NODE_NAME (agentfs volume " || true)
[ -n "$created" ] || fail "the plugin never logged: created volume $PV on node $NODE_NAME"
VOLUME=$(sed -n 's/.*(agentfs volume \([^,]*\), profile ci).*/\1/p' <<<"$created" | sed -n 1p)
[ -n "$VOLUME" ] || fail "no agentfs volume or profile ci in: $created"
pass "created volume $PV (agentfs volume $VOLUME, profile ci)"
echo "volume $PV ($VOLUME), writer $WRITER_UID"
docker exec "$NODE" test -f "$BASE/$PV/csi.json" || fail "no $BASE/$PV/csi.json on the node"
docker exec "$NODE" test -d "$BASE/$PV/state/history.git" || fail "no $BASE/$PV/state/history.git on the node"
pass "the volume is at $BASE/$PV (csi.json, state/history.git)"
wait_host
assert_contains "$(node_procs "$(host_pattern)")" "host --launcher csi --volume $VOLUME" \
  "the host runs under launcher csi, guarded by the volume's name"
mounts=$(view_mounts)
echo "$mounts"
assert_contains "$mounts" "kubernetes.io~csi/$PV/mount" "the view is mounted at the kubelet's target"
assert_contains "$mounts" "fuse.agentfs agentfs:$VOLUME " "the view is an agentfs FUSE mount of $VOLUME"

step "(e) the writer's files are in the volume's history, naming the pod"
assert_eq 3001 "$(in_pod writer 'stat -c %g /data')" "the kubelet gave the volume's root the fsGroup"
# Not the writer's own directory, nor its primary group: the creates go
# through the supplementary fsGroup.
[ "$(in_pod writer 'stat -c %u /data')" != 10000 ] || fail "the volume's root is the writer's own"
in_pod writer 'umask 002 && printf a > /data/a.txt && mkdir /data/notes && printf b > /data/notes/b.txt'
assert_eq "10000 3001" "$(in_pod writer 'stat -c "%u %g" /data/a.txt')" "a.txt is uid 10000, fsGroup 3001"
assert_eq 3001 "$(in_pod writer 'stat -c %g /data/notes/b.txt')" "notes/b.txt carries the fsGroup"
wait_history a.txt a
wait_history notes/b.txt b
assert_eq 10000 "$(history_uid a.txt)" "the history records a.txt as uid 10000's"
msg=$(history_message a.txt)
echo "$msg"
assert_contains "$msg" "Agentfs-Pod: writer" "pod trailer"
assert_contains "$msg" "Agentfs-Namespace: $APPS" "namespace trailer"
assert_contains "$msg" "Agentfs-Pod-Uid: $WRITER_UID" "pod uid trailer"
assert_contains "$msg" "Agentfs-Volume: $VOLUME" "volume trailer"
if plugin_log | grep "process id is 0" >/dev/null; then
  fail "the host could not see a pod's process (hostPID): $(plugin_log | grep 'process id is 0')"
fi
pass "the host saw its callers' groups"

step "(f) the profile's remote is logged as not built, and never reached"
BOOTS=$(not_built_lines)
[ "$BOOTS" -ge 1 ] || fail "the host never logged: not built: remote.origin.url (push)"
pass "the host's boot logged: not built: remote.origin.url (push)"
refs=$(git ls-remote "$REMOTE")
[ -z "$refs" ] || fail "the runner's git server got refs: $refs"
pass "the runner's git server has no ref"

step "(g) a second pod on the claim is refused"
kubectl apply -f ci/k8s-csi/apps/second.yaml
wait_event second FailedMount "several pods on one volume are not built" 180
in_pod writer 'printf during > /data/during-second.txt'
wait_history during-second.txt during
ka delete pod second --wait --timeout=120s
assert_eq 1 "$(printf '%s\n' "$(host_pids)" | grep -c .)" "one host still runs the volume"

step "(h) kill -9 the volume's host: a new one serves the writer, live"
OLD_HOST=$HOST_PID
node_kill "$(host_pattern)" || fail "no host of $PV to kill"
pass "killed the host (pid $OLD_HOST)"
wait_host "$OLD_HOST"
deadline=$((SECONDS + 120))
until [ "$(not_built_lines)" -gt "$BOOTS" ]; do
  [ $SECONDS -lt $deadline ] || fail "the new host never booted (one not-built line)"
  sleep 1
done
pass "the new host booted (its not-built line logged again)"
wait_live 120
in_pod writer 'printf after > /data/after-host-crash.txt' || fail "the writer could not write after the host's restart"
wait_history after-host-crash.txt after
assert_eq 10000 "$(history_uid after-host-crash.txt)" "the write after the crash is recorded as the writer's"

step "(i) restart the node plugin: reconcile"
OLD_PLUGIN=$(plugin_pod)
OLD_HOST=$HOST_PID
kd logs -f "$OLD_PLUGIN" -c agentfs-csi >"$LOGS/old-plugin.log" 2>&1 &
FOLLOWER=$!
kd delete pod "$OLD_PLUGIN" --wait --timeout=120s
for _ in $(seq 30); do kill -0 "$FOLLOWER" 2>/dev/null || break; sleep 1; done
kill "$FOLLOWER" 2>/dev/null || true
if docker exec "$NODE" kill -0 "$OLD_HOST" 2>/dev/null; then
  fail "the old plugin's host (pid $OLD_HOST) outlived it"
fi
pass "the old plugin's host exited with it"
deadline=$((SECONDS + 300))
until NEW_PLUGIN=$(plugin_pod) && [ -n "$NEW_PLUGIN" ] && [ "$NEW_PLUGIN" != "$OLD_PLUGIN" ]; do
  [ $SECONDS -lt $deadline ] || fail "the node plugin was not recreated"
  sleep 1
done
kd wait --for=condition=Ready "pod/$NEW_PLUGIN" --timeout=300s >/dev/null ||
  fail "the new node plugin never became Ready"
pass "new node plugin $NEW_PLUGIN"
wait_host "$OLD_HOST" 300
wait_live 180
in_pod writer 'printf again > /data/after-plugin-restart.txt' || fail "the writer could not write after the plugin's restart"
wait_history after-plugin-restart.txt again
[ "$(not_built_lines)" -ge 1 ] || fail "the new plugin's host never logged its not-built line"
pass "the new plugin's host logged its not-built line"

step "(j) refused: inline volumes, ReadWriteMany, onDelete: delete, startFrom"
kubectl apply -k ci/k8s-csi/negative
wait_event inline FailedMount Ephemeral 180
wait_event rwx ProvisioningFailed multi-node 180
wait_event ondelete-delete ProvisioningFailed "onDelete: delete is not built on tree volumes" 180
wait_event start-from ProvisioningFailed "startFrom is not built on tree volumes" 180
for claim in ondelete-delete start-from; do
  assert_eq Pending "$(ka get pvc "$claim" -o jsonpath='{.status.phase}')" "the claim $claim stays Pending"
done
kubectl delete -k ci/k8s-csi/negative --wait --timeout=120s

step "(k) teardown: the writer goes, then the claim; the volume is kept"
in_pod writer 'printf final > /data/final.txt'
ka delete pod writer --wait --timeout=180s
deadline=$((SECONDS + 180))
while [ -n "$(view_mounts)" ]; do
  [ $SECONDS -lt $deadline ] || fail "views of $VOLUME are left on the node: $(view_mounts)"
  sleep 1
done
pass "no agentfs:$VOLUME mount left on the node"
deadline=$((SECONDS + 120))
while [ -n "$(host_pids)" ]; do
  [ $SECONDS -lt $deadline ] || fail "the host of $PV still runs: $(node_procs "$(host_pattern)")"
  sleep 1
done
pass "no host of $PV runs"
assert_eq final "$(hgit show "$(file_rev final.txt)")" "the final write is in the volume's history"
ka delete pvc shared --wait --timeout=180s
deadline=$((SECONDS + 180))
while kubectl get pv "$PV" >/dev/null 2>&1; do
  [ $SECONDS -lt $deadline ] || fail "the PV $PV was never deleted"
  sleep 1
done
pass "DeleteVolume succeeded: the PV $PV is gone"
docker exec "$NODE" test -f "$BASE/$PV/csi.retained.json" ||
  fail "$BASE/$PV/csi.retained.json is missing: the volume was not kept"
if docker exec "$NODE" test -e "$BASE/$PV/csi.json"; then
  fail "$BASE/$PV/csi.json is still there: the volume is not marked kept"
fi
pass "the volume's directory is kept ($BASE/$PV/csi.retained.json)"
assert_eq final "$(hgit show "$(file_rev final.txt)")" "the kept history has the final write"
assert_contains "$(plugin_log)" "kept volume $PV at " "the plugin logged the kept volume"
refs=$(git ls-remote "$REMOTE")
[ -z "$refs" ] || fail "the runner's git server got refs: $refs"
pass "the runner's git server still has no ref"

step "e2e-csi passed"
