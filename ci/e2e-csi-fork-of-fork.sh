#!/usr/bin/env bash
# End-to-end test of forks of forks on the CSI driver (docs/csi.md) in a
# kind cluster. Forks of forks are not built on tree volumes: this job
# asserts the refusal, by design, until nested forks are built
# (docs/target-architecture/future/nested-forks.md). It deploys
# ci/k8s-csi/forks/driver (the e2e-csi overlay with the `cifork` profile)
# and ci/k8s-csi/forks/apps (a source claim kept in use by a writer), and
# checks, in order:
#
#   1. a fork claim of the source binds (the child), and its pod reads the
#      source's file;
#   2. a fork claim whose dataSource is the child's claim (the grandchild)
#      stays Pending, with a ProvisioningFailed event naming the refusal
#      (`forks of forks are not built`), while the child keeps working: its
#      save lands in the source;
#   3. the runner's git server never gets a ref: nothing pushes.
#
# Needs kind, kubectl, the image agentfs:ci loaded into the cluster
# $KIND_CLUSTER (default agentfs-fork-of-fork), and the git server from
# ci/git-http-server.sh with GIT_SERVER_EXTRA_LISTEN=$(this script gateway).
#
#   ci/e2e-csi-fork-of-fork.sh            # ci/e2e-csi-fork-of-fork.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-fork-of-fork}
NODE=$KIND_CLUSTER-control-plane
DNS=agentfs-csi
APPS=agentfs-csi-e2e-forks
DRIVER=agentfs.mcurcio.github.io
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node

kd() { kubectl -n "$DNS" "$@"; }
ka() { kubectl -n "$APPS" "$@"; }

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | sed -n 1p
}

diagnose() {
  set +e
  step "diagnose"
  kubectl get pods -A -o wide
  kubectl get pv,storageclass -o wide
  ka get pvc,pods -o wide
  ka describe pvc
  ka describe pods
  ka get events --sort-by=.lastTimestamp | tail -n 60
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    for c in agentfs-csi csi-provisioner; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=60 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  echo "---- volumes on the node"
  docker exec "$NODE" sh -c "ls -la $BASE $BASE/*; cat $BASE/*/csi*.json"
  echo "---- git server"
  git --git-dir "${GIT_SERVER_REPOS:-/nonexistent}/history.git" for-each-ref 2>/dev/null
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

plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
}
plugin_ready() {
  kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
    fail "the node plugin never became Ready"
}
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }
in_pod() { ka exec "$1" -- sh -c "$2"; }

# git on the source's history, in the node plugin.
hgit() { in_plugin git --git-dir "$BASE/$SOURCE_PV/state/history.git" "$@"; }
# Where <path>'s bytes are in a tree commit (docs/trees.md).
file_rev() { echo "root:files/$(tree_git_fanned "$1")"; }

# Checks for until_ok.
has_plugin_pod() { [ -n "$(plugin_pod)" ]; }
csinode_lists_driver() {
  local drivers
  drivers=$(kubectl get csinode "$1" -o jsonpath='{.spec.drivers[*].name}')
  grep -qw "$DRIVER" <<<"$drivers"
}
history_file_is() { [ "$(hgit show "$(file_rev "$1")" 2>/dev/null)" = "$2" ]; } # path content
pod_reads() { [ "$(in_pod "$1" "cat /data/$2" 2>/dev/null)" = "$3" ]; } # pod path content

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

# A fork class claim <name> of claim <source>, and a pod <name> using it
# on the writer's node (a fork is made in its source's volume).
fork_claim_and_pod() { # name source
  kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $1
  namespace: $APPS
spec:
  storageClassName: agentfs-ci-fork
  dataSource:
    kind: PersistentVolumeClaim
    name: $2
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: $1
  namespace: $APPS
spec:
  automountServiceAccountToken: false
  terminationGracePeriodSeconds: 5
  affinity:
    podAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        - labelSelector:
            matchLabels: { app.kubernetes.io/name: writer }
          topologyKey: kubernetes.io/hostname
  securityContext:
    runAsNonRoot: true
    runAsUser: 10000
    runAsGroup: 10000
    fsGroup: 10000
    fsGroupChangePolicy: OnRootMismatch
    seccompProfile: { type: RuntimeDefault }
  containers:
    - name: work
      image: agentfs:ci
      imagePullPolicy: Never
      command: [sh, -c, "trap 'exit 0' TERM; while :; do sleep 1; done"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: { drop: [ALL] }
      volumeMounts:
        - { name: data, mountPath: /data }
  volumes:
    - name: data
      persistentVolumeClaim: { claimName: $1 }
EOF
}

step "git server and driver"
rm -rf "$REMOTE"
git init -q --bare "$REMOTE"
{ echo "agentfs:$(openssl passwd -apr1 ci-csi-password)"; } >"$GIT_SERVER_HTPASSWD"
GW=$(gateway)
[ -n "$GW" ] || fail "no IPv4 gateway on the kind network"
kubectl apply -f deploy/csi/namespace.yaml
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
kubectl apply -k ci/k8s-csi/forks/driver
until_ok 300 "a node plugin pod exists" has_plugin_pod
plugin_ready
NODE_NAME=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')
until_ok 120 "the CSINode lists $DRIVER" csinode_lists_driver "$NODE_NAME"

step "(1) a fork claim of the source binds (the child)"
kubectl apply -k ci/k8s-csi/forks/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never became Ready"
SOURCE_PV=$(ka get pvc source -o jsonpath='{.spec.volumeName}')
[ -n "$SOURCE_PV" ] || fail "the source claim is not bound"
in_pod writer 'printf "from the source" > /data/shared.txt'
until_ok 120 "the source's history has shared.txt" history_file_is shared.txt "from the source"
fork_claim_and_pod child source
ka wait --for=condition=Ready pod/child --timeout=240s >/dev/null || fail "the child fork's pod never became Ready"
CHILD_PV=$(ka get pvc child -o jsonpath='{.spec.volumeName}')
echo "source $SOURCE_PV, child $CHILD_PV"
assert_eq "from the source" "$(in_pod child 'cat /data/shared.txt')" "the child reads the source's file"

step "(2) a fork claim of the child is refused: forks of forks are not built"
fork_claim_and_pod grandchild child
wait_event grandchild ProvisioningFailed "forks of forks are not built" 180
assert_eq Pending "$(ka get pvc grandchild -o jsonpath='{.status.phase}')" "the grandchild's claim stays Pending"
ka delete pod grandchild --wait=false
ka delete pvc grandchild --wait --timeout=120s
in_pod child 'printf "from the child" > /data/child.txt'
until_ok 120 "the child's save lands in the source: the writer reads child.txt" \
  pod_reads writer child.txt "from the child"
until_ok 120 "the source's history has child.txt" history_file_is child.txt "from the child"

step "(3) nothing pushes"
refs=$(git ls-remote "$REMOTE")
[ -z "$refs" ] || fail "the runner's git server got refs: $refs"
pass "the runner's git server has no ref"

step "e2e-csi-fork-of-fork passed"
