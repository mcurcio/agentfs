#!/usr/bin/env bash
# End-to-end test of forks of forks on the CSI driver (bead agentfs-mia.7,
# design §11.5) in a kind cluster: deploys ci/k8s-csi/forks/driver (the
# e2e-csi overlay with the `cifork` profile, whose fork remote forks.git
# needs a password: agentfsCredentials, nginx auth_basic) and
# ci/k8s-csi/forks/apps (a source claim kept in use by a writer), and
# checks, in order:
#
#   1. a fork class claim on the source binds (the child), and a fork class
#      claim whose dataSource is that fork's claim binds too (the
#      grandchild): its identity names the child and records that its
#      source is a fork;
#   2. a pod in the grandchild writes g.txt, which its interval pushes
#      take to its branch on the fork remote;
#   3. deleting the grandchild's pod and claim finishes it: the node
#      plugin's release scan rolls its change up into the child's queue on
#      the fork remote (refs/agentfs/<child>/inbox/0000000001 holds g.txt),
#      logging in with the fork remote's credentials, publishes no change
#      ref of the grandchild's but its finish ref (its own forks read it as
#      finished), and the PV goes;
#   4. the child's next pod sees g.txt: its queue landed the patch when its
#      history service started with nothing attached, and its applied ref
#      reaches the fork remote.
#
# Step 3 is the one that fails if the roll-up does not log in through the
# fork remote's credentials (remote_auth.rs): the git program reads none of
# agentfs's credential keys.
#
# Needs kind, kubectl, the image agentfs:ci loaded into the cluster
# $KIND_CLUSTER (default agentfs-fork-of-fork), and the git server from
# ci/git-http-server.sh with GIT_SERVER_EXTRA_LISTEN=$(this script gateway).
#
#   ci/e2e-csi-fork-of-fork.sh            # ci/e2e-csi-fork-of-fork.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"

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
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | head -n1
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
  docker exec "$NODE" sh -c "ls -la $BASE; cat $BASE/*/csi.json; cat $BASE/*/state/meta/fork.json"
  echo "---- remotes"
  for r in history forks; do
    git --git-dir "${GIT_SERVER_REPOS:-/nonexistent}/$r.git" for-each-ref 2>/dev/null | sed "s|^|$r: |"
  done
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
SOURCE_REMOTE=$GIT_SERVER_REPOS/history.git
FORKS_REMOTE=$GIT_SERVER_REPOS/forks.git
fgit() { git --git-dir "$FORKS_REMOTE" "$@"; }

plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
plugin_ready() {
  kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
    fail "the node plugin never became Ready"
}
in_pod() { ka exec "$1" -- sh -c "$2"; }

# Checks for until_ok.
has_plugin_pod() { [ -n "$(plugin_pod)" ]; }
csinode_lists_driver() {
  local drivers
  drivers=$(kubectl get csinode "$1" -o jsonpath='{.spec.drivers[*].name}')
  grep -qw "$DRIVER" <<<"$drivers"
}
remote_file_is() { [ "$(git --git-dir "$1" show "$2:$3" 2>/dev/null)" = "$4" ]; } # remote ref path content
ref_exists() { git --git-dir "$1" rev-parse -q --verify "$2" >/dev/null 2>&1; }
pv_gone() { ! kubectl get pv "$1" >/dev/null 2>&1; }

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

# A fork class claim <name> cloning claim <source>, and a pod <name> using
# it, as the writer runs.
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
EOF
  pod "$1"
}

# A pod <name> using claim <name>.
pod() { # name
  kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
  namespace: $APPS
spec:
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
for r in "$SOURCE_REMOTE" "$FORKS_REMOTE"; do
  rm -rf "$r"
  git init -q --bare "$r"
done
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
# Nothing reaches the fork remote without the password.
if GIT_TERMINAL_PROMPT=0 git -c credential.helper= ls-remote "http://127.0.0.1:8080/git/forks.git" >/dev/null 2>&1; then
  fail "the fork remote answers without credentials"
fi
pass "the fork remote refuses a client with no credentials"

step "(1) a fork of a fork binds"
kubectl apply -k ci/k8s-csi/forks/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never became Ready"
SOURCE_PV=$(ka get pvc source -o jsonpath='{.spec.volumeName}')
SOURCE_BRANCH=refs/heads/agentfs/$APPS.source.$SOURCE_PV
in_pod writer 'printf "from the source" > /data/shared.txt'
until_ok 120 "the source's branch on its remote has shared.txt" \
  remote_file_is "$SOURCE_REMOTE" "$SOURCE_BRANCH" shared.txt "from the source"
fork_claim_and_pod child source
ka wait --for=condition=Ready pod/child --timeout=240s >/dev/null || fail "the child fork's pod never became Ready"
CHILD_PV=$(ka get pvc child -o jsonpath='{.spec.volumeName}')
CHILD=$APPS.child.$CHILD_PV
in_pod child 'printf "from the child" > /data/child.txt'
until_ok 120 "the fork remote has the child's branch with child.txt" \
  remote_file_is "$FORKS_REMOTE" "refs/heads/agentfs/$CHILD" child.txt "from the child"
fork_claim_and_pod grandchild child
ka wait --for=condition=Ready pod/grandchild --timeout=240s >/dev/null ||
  fail "the grandchild's pod never became Ready: a fork class claim on a fork's claim did not bind"
pass "a fork class claim whose source is a fork's claim binds"
GC_PV=$(ka get pvc grandchild -o jsonpath='{.spec.volumeName}')
GC=$APPS.grandchild.$GC_PV
echo "source $SOURCE_PV, child $CHILD_PV ($CHILD), grandchild $GC_PV ($GC)"
config=$(docker exec "$NODE" cat "$BASE/$GC_PV/state/git/config")
assert_contains "$config" "source = $CHILD" "the grandchild's identity names the child"
assert_contains "$config" "sourceIsFork = true" "the grandchild records that its source is a fork"
assert_eq "from the child" "$(in_pod grandchild 'cat /data/child.txt')" "the grandchild sees the child's files"

step "(2) the grandchild's writes reach its branch"
in_pod grandchild 'printf "from the grandchild" > /data/g.txt'
until_ok 120 "the fork remote has the grandchild's branch with g.txt" \
  remote_file_is "$FORKS_REMOTE" "refs/heads/agentfs/$GC" g.txt "from the grandchild"

step "(3) deleting the grandchild rolls its change up into the child's queue"
ka delete pod grandchild --wait --timeout=180s
ka delete pvc grandchild --wait=false
INBOX=refs/agentfs/$CHILD/inbox/0000000001
until_ok 600 "the release scan queues the change as the child's patch 1 on the fork remote" \
  ref_exists "$FORKS_REMOTE" "$INBOX"
assert_eq "from the grandchild" "$(fgit show "$INBOX:g.txt")" "the child's patch 1 holds g.txt"
fgit merge-base --is-ancestor "$(fgit rev-parse "$INBOX^")" "refs/heads/agentfs/$CHILD" ||
  fail "the patch's parent is not on the child's branch"
pass "the patch's parent is a commit of the child's branch, the base the grandchild forked"
ref_exists "$FORKS_REMOTE" "refs/agentfs/$GC/change" && fail "the grandchild published a change ref"
pass "a rolled-up fork publishes no change ref of its own"
until_ok 120 "the grandchild's finish ref is on the fork remote" \
  ref_exists "$FORKS_REMOTE" "refs/agentfs/$GC/finish"
until_ok 600 "the grandchild's PV is gone once its change is queued" pv_gone "$GC_PV"

step "(4) the child's next pod sees the grandchild's file"
ka delete pod child --wait --timeout=180s
pod child
ka wait --for=condition=Ready pod/child --timeout=240s >/dev/null || fail "the child's next pod never became Ready"
assert_eq "from the grandchild" "$(in_pod child 'cat /data/g.txt')" \
  "the child's queue landed the grandchild's change at its next mount"
until_ok 120 "the child's applied ref for patch 1 reaches the fork remote" \
  ref_exists "$FORKS_REMOTE" "refs/agentfs/$CHILD/applied/0000000001"
ka get pod writer -o jsonpath='{.status.phase}' | grep -q Running || fail "the source's pod stopped"
pass "the source stayed in use throughout"

step "e2e-csi-fork-of-fork passed"
