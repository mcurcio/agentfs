#!/usr/bin/env bash
# End-to-end test of node-local volumes on a two-worker kind cluster
# (docs/csi.md). Moving a volume between nodes is not built on tree
# volumes: a start class (`startFrom`) is refused, by design, until its
# slice lands (docs/runbooks/moving.md says how an operator moves one by
# hand). The driver comes from charts/agentfs-csi in this tree, with the
# `ci` profile of ci/k8s-csi/driver, whose remote is logged as not built.
# Each line is one assertion:
#
#   a. a pod on worker A writes f: it is recorded in A's
#      <base>/<pv>/state, the PV's node affinity names A, and B holds no
#      directory of the volume;
#   b. a claim of a start class (`startFrom: origin:agentfs/<V1>@<commit>`)
#      stays Pending, with a ProvisioningFailed event naming the refusal
#      (`startFrom is not built on tree volumes`);
#   c. deleting A's pod and claim deletes the PV and keeps the volume's
#      directory on A (csi.retained.json), with f in its history.
#
# Needs docker, kind, kubectl and helm, a kind cluster $KIND_CLUSTER
# (default agentfs-move) made from ci/kind/two-workers.yaml with the image
# agentfs:ci loaded, and FUSE on the host. Locally:
#
#   docker build -t agentfs:ci .
#   kind create cluster --name agentfs-move --config ci/kind/two-workers.yaml --image "$KIND_NODE_IMAGE"
#   kind load docker-image agentfs:ci --name agentfs-move
#   ci/e2e-csi-move.sh            # ci/e2e-csi-move.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-move}
NODE_A=$KIND_CLUSTER-worker
NODE_B=$KIND_CLUSTER-worker2
DNS=agentfs-csi
APPS=agentfs-csi-e2e-move
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node

kd() { kubectl -n "$DNS" "$@"; }
ka() { kubectl -n "$APPS" "$@"; }

diagnose() {
  set +e
  step "diagnose"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  kubectl get csinode,pv,storageclass -o wide
  ka get pvc,pods -o wide
  ka describe pvc
  ka describe pods
  ka get events --sort-by=.lastTimestamp | tail -n 60
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    for c in agentfs-csi csi-provisioner; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=100 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  for node in "$NODE_A" "$NODE_B"; do
    echo "---- volumes on $node"
    docker exec "$node" sh -c "ls -la $BASE $BASE/*; cat $BASE/*/csi*.json" 2>&1
  done
  return 0
}

case "${1:-}" in
diagnose) diagnose; exit 0 ;;
"") ;;
*) echo "usage: $0 [diagnose]" >&2; exit 2 ;;
esac

W=$(mktemp -d "${RUNNER_TEMP:-/tmp}/e2e-move.XXXXXX")

plugin_on() { # node
  kd get pod -l "$PLUGIN_LABEL" --field-selector "spec.nodeName=$1" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
}
on() { # node command...
  local node=$1
  shift
  kd exec "$(plugin_on "$node")" -c agentfs-csi -- "$@"
}
plugin_log() { kd logs "$(plugin_on "$1")" -c agentfs-csi 2>/dev/null; }
in_pod() { ka exec "$1" -- sh -c "$2"; }
# git on V1's history, in A's node plugin.
hgit() { on "$NODE_A" git --git-dir "$BASE/$PV1/state/history.git" "$@"; }
# Where <path>'s bytes are in a tree commit (docs/trees.md).
file_rev() { echo "root:files/$(tree_git_fanned "$1")"; }

# Waits up to <seconds> for <command...> to succeed.
until_ok() { # seconds message command...
  local deadline=$((SECONDS + $1)) message=$2
  shift 2
  until "$@" >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || fail "$message"
    sleep 2
  done
  pass "$message"
}

# Checks for until_ok.
history_file_is() { [ "$(hgit show "$(file_rev "$1")" 2>/dev/null)" = "$2" ]; } # path content
plugin_ready() { kd wait --for=condition=Ready "pod/$(plugin_on "$1")" --timeout=5s; }
pv_gone() { ! kubectl get pv "$1" >/dev/null 2>&1; }

# A restricted pod using <claim>, on <node> when given.
writer() { # name claim [node]
  local selector=""
  [ -z "${3:-}" ] || selector="nodeSelector: { kubernetes.io/hostname: $3 }"
  ka apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
spec:
  $selector
  automountServiceAccountToken: false
  terminationGracePeriodSeconds: 5
  securityContext:
    runAsNonRoot: true
    runAsUser: 10000
    runAsGroup: 10000
    fsGroup: 10000
    fsGroupChangePolicy: OnRootMismatch
    seccompProfile: { type: RuntimeDefault }
  containers:
    - name: writer
      image: agentfs:ci
      imagePullPolicy: IfNotPresent
      command: [sh, -c, "trap 'exit 0' TERM; while :; do sleep 1; done"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: [ALL] }
      volumeMounts:
        - { name: data, mountPath: /data, mountPropagation: HostToContainer }
  volumes:
    - name: data
      persistentVolumeClaim: { claimName: $2 }
EOF
}

claim() { # name class
  ka apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $1
spec:
  storageClassName: $2
  accessModes: [ReadWriteOnce]
  resources: { requests: { storage: 1Gi } }
EOF
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

step "cluster and driver"
kubectl get nodes -o wide
for node in "$NODE_A" "$NODE_B"; do
  kubectl get node "$node" >/dev/null || fail "no node $node (ci/kind/two-workers.yaml)"
done
# The chart's values for this job: the image it built, the `ci` profile
# (set from ci/k8s-csi/driver below) and its credentials, and the
# ordinary class.
cat >"$W/values.yaml" <<'EOF'
image:
  repository: agentfs
  tag: ci
credentials:
  stringData:
    # A CI-only password the `ci` profile names; no server runs here.
    ci-origin: |
      username=agentfs
      password=ci-csi-password
storageClasses:
  - name: agentfs-move
    parameters:
      profile: ci
EOF
kubectl apply -f deploy/csi/namespace.yaml
helm install agentfs-csi charts/agentfs-csi --namespace "$DNS" \
  -f "$W/values.yaml" \
  --set-file profiles.ci.config=ci/k8s-csi/driver/profiles/ci.config \
  --wait --timeout 5m
for node in "$NODE_A" "$NODE_B"; do
  until_ok 180 "the node plugin on $node is Ready" plugin_ready "$node"
done
kubectl apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $APPS
  labels:
    pod-security.kubernetes.io/enforce: restricted
EOF

step "(a) a pod on A writes f: recorded on A's disk, the PV pinned to A"
claim v1 agentfs-move
writer writer-1 v1 "$NODE_A"
ka wait --for=condition=Ready pod/writer-1 --timeout=300s >/dev/null || fail "writer-1 never became Ready"
PV1=$(ka get pvc v1 -o jsonpath='{.spec.volumeName}')
[ -n "$PV1" ] || fail "the claim v1 is not bound"
created=$(plugin_log "$NODE_A" | grep -F "created volume $PV1 on node $NODE_A (agentfs volume " || true)
[ -n "$created" ] || fail "A's plugin never logged: created volume $PV1 on node $NODE_A"
V1=$(sed -n 's/.*(agentfs volume \([^,]*\), profile ci).*/\1/p' <<<"$created" | sed -n 1p)
[ -n "$V1" ] || fail "no agentfs volume in: $created"
echo "V1: $V1 ($PV1) on $NODE_A"
assert_eq "$NODE_A" "$(kubectl get pv "$PV1" -o jsonpath='{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}')" \
  "V1's PV is pinned to A, whose disk holds its volume"
in_pod writer-1 'printf f > /data/f.txt'
until_ok 120 "f is recorded in $BASE/$PV1/state on A" history_file_is f.txt f
docker exec "$NODE_B" test ! -e "$BASE/$PV1" || fail "V1's directory is on B's disk"
pass "V1's directory is on A's disk only"
F=$(hgit rev-parse root)
echo "f $F"

step "(b) a start class is refused: moving a volume is not built"
kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: agentfs-move-v1
provisioner: agentfs.mcurcio.github.io
parameters:
  profile: ci
  startFrom: "origin:agentfs/$V1@$F"
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
EOF
claim v2 agentfs-move-v1
writer writer-2 v2
wait_event v2 ProvisioningFailed "startFrom is not built on tree volumes" 180
assert_eq Pending "$(ka get pvc v2 -o jsonpath='{.status.phase}')" "the claim v2 stays Pending"
ka delete pod writer-2 --wait=false
ka delete pvc v2 --wait --timeout=120s
kubectl delete storageclass agentfs-move-v1

step "(c) deleting A's claim keeps its directory on A"
ka delete pod writer-1 --wait --timeout=180s
ka delete pvc v1 --wait --timeout=180s
until_ok 180 "V1's PV is deleted" pv_gone "$PV1"
docker exec "$NODE_A" test -f "$BASE/$PV1/csi.retained.json" ||
  fail "$BASE/$PV1/csi.retained.json is missing on A: the volume was not kept"
if docker exec "$NODE_A" test -e "$BASE/$PV1/csi.json"; then
  fail "$BASE/$PV1/csi.json is still on A: the volume is not marked kept"
fi
pass "V1's directory is kept on A ($BASE/$PV1/csi.retained.json)"
assert_eq f "$(hgit show "$(file_rev f.txt)")" "the kept history has f"
docker exec "$NODE_B" test ! -e "$BASE/$PV1" || fail "V1's directory appeared on B"
pass "B still holds no directory of V1"

step "e2e-csi-move passed"
