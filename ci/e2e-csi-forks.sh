#!/usr/bin/env bash
# End-to-end test of fork claims on the CSI driver (docs/csi.md) on the
# tree engine in a kind cluster: deploys ci/k8s-csi/forks/driver (the
# e2e-csi overlay with a `cifork` profile) and ci/k8s-csi/forks/apps (a
# source claim kept in use by a writer), and checks, in order:
#
#   1. a fork pod's ephemeral claim on a fork class, whose dataSource is the
#      source claim the writer keeps in use, binds (if it does not, the
#      test fails with "premise refuted"); the fork lives in the source's
#      volume (the fork claim's directory holds only csi.json), and the
#      fork pod reads the source's file;
#   2. the fork pod writes f.txt: it lands in the source automatically, so
#      the writer's own mount shows it, and the source's root history has
#      it with the fork pod's Agentfs-Pod trailer;
#   3. the class admission policy denies a fork class with reclaim policy
#      Retain, and admits the shipped agentfs-fork class;
#   4. fork claims on a `link: unlinked` class and on a `landing: approval`
#      class stay Pending, each with a ProvisioningFailed event naming the
#      refusal (`unlinked forks are not built`, `landing approvals are not
#      built`);
#   5. deleting the fork pod deletes its claim: the fork ends, its PV goes
#      and its directory on the node is removed, while the writer still
#      reads f.txt and the source's history keeps it.
#
# Throughout, the profile's remote is logged as not built at each host's
# boot and the runner's git server never gets a ref: nothing pushes.
#
# Needs what ci/e2e-csi.sh needs: kind, kubectl, the image agentfs:ci
# loaded into the cluster $KIND_CLUSTER (default agentfs-forks), and the
# git server from ci/git-http-server.sh with
# GIT_SERVER_EXTRA_LISTEN=$(ci/e2e-csi-forks.sh gateway).
#
#   ci/e2e-csi-forks.sh            # ci/e2e-csi-forks.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/tree-git-test-lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-forks}
NODE=$KIND_CLUSTER-control-plane
DNS=agentfs-csi
APPS=agentfs-csi-e2e-forks
DRIVER=agentfs.mcurcio.github.io
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
HERE=$(cd "$(dirname "$0")" && pwd)

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
  echo "---- node mounts"
  docker exec "$NODE" sh -c "grep -E 'agentfs|kubernetes.io~csi' /proc/self/mountinfo"
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
plugin_log() { kd logs "$(plugin_pod)" -c agentfs-csi; }
provisioner_log() { kd logs "$(plugin_pod)" -c csi-provisioner; }
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }
in_pod() { ka exec "$1" -- sh -c "$2"; }

# git on the source's history, in the node plugin.
hgit() { in_plugin git --git-dir "$BASE/$SOURCE_PV/state/history.git" "$@"; }
# Where <path>'s bytes are in a tree commit (docs/trees.md).
file_rev() { echo "root:files/$(tree_git_fanned "$1")"; }
# The message of the newest root commit that recorded <path>.
history_message() { hgit log -1 --format=%B root -- "files/$(tree_git_fanned "$1")"; }

# Checks for until_ok.
has_plugin_pod() { [ -n "$(plugin_pod)" ]; }
csinode_lists_driver() {
  local drivers
  drivers=$(kubectl get csinode "$1" -o jsonpath='{.spec.drivers[*].name}')
  grep -qw "$DRIVER" <<<"$drivers"
}
history_file_is() { [ "$(hgit show "$(file_rev "$1")" 2>/dev/null)" = "$2" ]; } # path content
pod_reads() { [ "$(in_pod "$1" "cat /data/$2" 2>/dev/null)" = "$3" ]; } # pod path content
pv_gone() { ! kubectl get pv "$1" >/dev/null 2>&1; }
node_path_gone() { ! docker exec "$NODE" test -e "$1"; }
not_built_logged() { # pv
  local said # captured first: grep -q closing a pipe would fail it (pipefail)
  said=$(plugin_log 2>/dev/null)
  grep -qF "[host $1] not built: remote.origin.url (push): " <<<"$said"
}

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

no_remote_refs() {
  local refs
  refs=$(git ls-remote "$REMOTE")
  [ -z "$refs" ] || fail "the runner's git server got refs: $refs"
  pass "the runner's git server has no ref: nothing pushes"
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

step "(1) a fork claim of a source in use binds, in the source's volume"
kubectl apply -k ci/k8s-csi/forks/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never became Ready"
SOURCE_PV=$(ka get pvc source -o jsonpath='{.spec.volumeName}')
[ -n "$SOURCE_PV" ] || fail "the source claim is not bound"
in_pod writer 'printf "from the source" > /data/shared.txt'
until_ok 120 "the source's history has shared.txt" history_file_is shared.txt "from the source"
until_ok 60 "the source's host logs its remote as not built" not_built_logged "$SOURCE_PV"
kubectl apply -f "$HERE/k8s-csi/forks/fork.yaml"
if ! ka wait --for=condition=Ready pod/fork --timeout=240s >/dev/null; then
  ka get events --field-selector involvedObject.name=fork-data -o wide || true
  provisioner_log | grep -i -E "fork-data|dataSource" | tail -n 20 || true
  fail "premise refuted: the fork claim of the source claim, in use by the writer, never became usable"
fi
pass "the fork pod is Ready: a fork claim of a claim in use binds"
FORK_PV=$(ka get pvc fork-data -o jsonpath='{.spec.volumeName}')
[ -n "$FORK_PV" ] || fail "the fork claim is not bound"
echo "source $SOURCE_PV, fork $FORK_PV"
docker exec "$NODE" test -f "$BASE/$FORK_PV/csi.json" || fail "no $BASE/$FORK_PV/csi.json on the node"
if docker exec "$NODE" test -e "$BASE/$FORK_PV/state"; then
  fail "$BASE/$FORK_PV/state exists: the fork is a volume of its own, not a fork in its source's volume"
fi
pass "the fork claim's directory holds only csi.json: its fork lives in the source's volume"
assert_eq "from the source" "$(in_pod fork 'cat /data/shared.txt')" "the fork pod reads the source's file"
ka get pod writer -o jsonpath='{.status.phase}' | grep Running >/dev/null || fail "the source's pod stopped"
pass "the writer kept the source in use"

step "(2) the fork pod's save lands in the source"
in_pod fork 'printf "from the fork" > /data/f.txt'
# Through the writer's own mount: only a landing puts it there.
until_ok 120 "the writer's mount shows f.txt" pod_reads writer f.txt "from the fork"
until_ok 120 "the source's root history has f.txt" history_file_is f.txt "from the fork"
msg=$(history_message f.txt)
echo "$msg"
assert_contains "$msg" "Agentfs-Pod: fork" "the landed commit names the fork pod (Agentfs-Pod trailer)"

step "(3) admission: every class, a fork class too, must be reclaim Delete"
denied=
deadline=$((SECONDS + 90))
while [ $SECONDS -lt $deadline ]; do
  if out=$(kubectl apply -f "$HERE/k8s-csi/forks/retain-class.yaml" 2>&1); then
    kubectl delete storageclass agentfs-ci-fork-retain --ignore-not-found >/dev/null
    sleep 3
    continue
  fi
  denied=$out
  break
done
[ -n "$denied" ] || fail "a fork class with reclaimPolicy Retain was admitted"
assert_contains "$denied" "agentfs-class" "the Retain fork class is denied by the policy"
kubectl apply --dry-run=server -f deploy/csi/example/fork-storageclass.yaml >/dev/null ||
  fail "the shipped agentfs-fork class is not admitted"
pass "the shipped agentfs-fork class is admitted"

step "(4) refused: unlinked forks, landing approvals"
kubectl apply -f "$HERE/k8s-csi/forks/refused.yaml"
wait_event unlinked ProvisioningFailed "unlinked forks are not built" 180
wait_event approval ProvisioningFailed "landing approvals are not built" 180
for claim in unlinked approval; do
  assert_eq Pending "$(ka get pvc "$claim" -o jsonpath='{.status.phase}')" "the claim $claim stays Pending"
done
kubectl delete -f "$HERE/k8s-csi/forks/refused.yaml" --wait --timeout=120s

step "(5) deleting the fork pod ends the fork; the source keeps what landed"
ka delete pod fork --wait --timeout=180s
until_ok 180 "the fork's PV is gone" pv_gone "$FORK_PV"
until_ok 60 "the fork claim's directory is removed" node_path_gone "$BASE/$FORK_PV"
assert_eq "from the fork" "$(in_pod writer 'cat /data/f.txt')" "the writer still reads f.txt"
assert_eq "from the fork" "$(hgit show "$(file_rev f.txt)")" "the source's history keeps f.txt"
in_pod writer 'printf after > /data/after-fork.txt'
until_ok 120 "the source still records" history_file_is after-fork.txt after
ka get pod writer -o jsonpath='{.status.phase}' | grep Running >/dev/null || fail "the source's pod stopped"
pass "the source stayed in use throughout"
no_remote_refs

step "e2e-csi-forks passed"
