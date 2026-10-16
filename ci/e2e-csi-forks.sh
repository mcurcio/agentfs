#!/usr/bin/env bash
# End-to-end test of forked volumes on the CSI driver (design §11.5,
# docs/csi.md §14) in a kind cluster: deploys ci/k8s-csi/forks/driver (the
# e2e-csi overlay with a `cifork` profile whose fork remote is forks.git)
# and ci/k8s-csi/forks/apps, and checks, in order:
#
#   1. a sandbox pod's ephemeral claim on a fork class clones a source
#      claim that a writing pod keeps in use (the premise of design §19.2
#      question 8: if Kubernetes refuses, the test fails with "premise
#      refuted"), and the sandbox sees the source's files;
#   2. the sandbox's writes reach refs/heads/agentfs/<fork> on the fork
#      remote by its interval pushes, and nothing of the fork reaches the
#      source's remote or branch;
#   3. the admission policy denies a fork class with reclaim policy
#      Retain, and admits the shipped agentfs-fork class;
#   4. with the fork remote blocked, deleting the sandbox pod leaves the PV
#      Released and DeleteVolume UNAVAILABLE, across a restart of the node
#      plugin pod; restoring the remote publishes the change and finish
#      refs and the PV goes;
#   5. the change submitted twice at once (`agentfs patch submit`), then a
#      restart of the source's pod, gives one commit applying it and one
#      duplicate marker in the source's history.
#
# Needs what ci/e2e-csi.sh needs (kind, kubectl, the image agentfs:ci
# loaded into the cluster $KIND_CLUSTER, default agentfs-forks, and the git
# server from ci/git-http-server.sh with GIT_SERVER_EXTRA_LISTEN), and the
# image in the runner's docker too (`agentfs patch submit` runs from it).
#
#   ci/e2e-csi-forks.sh            # ci/e2e-csi-forks.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"

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
    for c in agentfs-csi csi-provisioner csi-attacher; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=60 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  echo "---- volumes on the node"
  docker exec "$NODE" sh -c "ls -la $BASE; cat $BASE/*/csi.json"
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
sgit() { git --git-dir "$SOURCE_REMOTE" "$@"; }
fgit() { git --git-dir "$FORKS_REMOTE" "$@"; }
W=$(mktemp -d "${RUNNER_TEMP:-/tmp}/e2e-forks.XXXXXX")

plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
plugin_ready() {
  kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
    fail "the node plugin never became Ready"
}
provisioner_log() { kd logs deployment/agentfs-csi-controller -c csi-provisioner; }
in_pod() { ka exec "$1" -- sh -c "$2"; }

# Checks for until_ok.
has_plugin_pod() { [ -n "$(plugin_pod)" ]; }
csinode_lists_driver() {
  kubectl get csinode "$1" -o jsonpath='{.spec.drivers[*].name}' | grep -qw "$DRIVER"
}
remote_file_is() { [ "$(git --git-dir "$1" show "$2:$3" 2>/dev/null)" = "$4" ]; } # remote ref path content
pv_phase_is() { [ "$(kubectl get pv "$1" -o jsonpath='{.status.phase}' 2>/dev/null)" = "$2" ]; }
pv_gone() { ! kubectl get pv "$1" >/dev/null 2>&1; }
node_path_gone() { ! docker exec "$NODE" test -e "$1"; }
delete_unavailable() { # pv
  { provisioner_log; kubectl get events --field-selector "involvedObject.name=$1" \
    -o jsonpath='{range .items[*]}{.message}{"\n"}{end}'; } 2>/dev/null |
    grep -q "Unavailable.*fork $1 is"
}
ref_exists() { git --git-dir "$1" rev-parse -q --verify "$2" >/dev/null 2>&1; }
inbox_commits_are() { # n
  [ "$(sgit log --first-parent --format=%B "$SOURCE_BRANCH" | grep -c "^Agentfs-Patch: $CHANGE\$")" = "$1" ]
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
kd rollout status deployment/agentfs-csi-controller --timeout=300s
until_ok 300 "a node plugin pod exists" has_plugin_pod
plugin_ready
NODE_NAME=$(kubectl get nodes -o jsonpath='{.items[0].metadata.name}')
until_ok 120 "the CSINode lists $DRIVER" csinode_lists_driver "$NODE_NAME"

step "(1) a sandbox forks a source that is in use"
kubectl apply -k ci/k8s-csi/forks/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never became Ready"
SOURCE_PV=$(ka get pvc source -o jsonpath='{.spec.volumeName}')
SOURCE_VOLUME=$APPS.source.$SOURCE_PV
SOURCE_BRANCH=refs/heads/agentfs/$SOURCE_VOLUME
in_pod writer 'printf "from the source" > /data/shared.txt'
until_ok 120 "the source's branch on its remote has shared.txt" \
  remote_file_is "$SOURCE_REMOTE" "$SOURCE_BRANCH" shared.txt "from the source"
kubectl apply -f "$HERE/k8s-csi/forks/sandbox.yaml"
if ! ka wait --for=condition=Ready pod/sandbox --timeout=240s >/dev/null; then
  ka get events --field-selector involvedObject.name=sandbox-data -o wide || true
  provisioner_log | grep -i -E "sandbox-data|clone|dataSource" | tail -n 20 || true
  fail "premise refuted: the sandbox's clone of the source claim, in use by the writer, never became usable (design §19.2 question 8)"
fi
pass "the sandbox is Ready: Kubernetes cloned a claim in use"
FORK_PV=$(ka get pvc sandbox-data -o jsonpath='{.spec.volumeName}')
FORK=$APPS.sandbox-data.$FORK_PV
FORK_BRANCH=refs/heads/agentfs/$FORK
echo "source $SOURCE_PV ($SOURCE_VOLUME), fork $FORK_PV ($FORK)"
assert_eq "from the source" "$(in_pod sandbox 'cat /data/shared.txt')" "the sandbox sees the source's files"
assert_contains "$(docker exec "$NODE" cat "$BASE/$FORK_PV/state/git/config")" \
  "source = $SOURCE_VOLUME" "the fork's identity names its source"

step "(2) the sandbox's interval pushes reach the fork's branch only"
in_pod sandbox 'printf "from the sandbox" > /data/sandbox.txt'
until_ok 120 "the fork remote has $FORK_BRANCH with sandbox.txt" \
  remote_file_is "$FORKS_REMOTE" "$FORK_BRANCH" sandbox.txt "from the sandbox"
sgit rev-parse -q --verify "$FORK_BRANCH" >/dev/null && fail "the fork's branch reached the source's remote"
pass "the source's remote has no branch of the fork"
sgit show "$SOURCE_BRANCH:sandbox.txt" >/dev/null 2>&1 && fail "the source's branch has the sandbox's file"
pass "the source's branch is unchanged by the fork"
fgit rev-parse -q --verify "$SOURCE_BRANCH" >/dev/null && fail "the source pushed to its fork remote"
pass "the source never pushes to its fork remote"

step "(3) admission: a fork class must be reclaim Delete"
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
assert_contains "$denied" "agentfs-fork-class" "the Retain fork class is denied by the policy"
kubectl apply --dry-run=server -f deploy/csi/example/fork-storageclass.yaml >/dev/null ||
  fail "the shipped agentfs-fork class is not admitted"
pass "the shipped agentfs-fork class is admitted"

step "(4) DeleteVolume waits until the fork is published"
mode=$(stat -c %a "$FORKS_REMOTE")
chmod 000 "$FORKS_REMOTE"
ka delete pod sandbox --wait --timeout=180s
until_ok 180 "the fork's PV is Released" pv_phase_is "$FORK_PV" Released
until_ok 180 "DeleteVolume answers UNAVAILABLE while the fork is unpublished" \
  delete_unavailable "$FORK_PV"
assert_contains "$(docker exec "$NODE" cat "$BASE/$FORK_PV/csi.json")" '"finish_requested": true' \
  "csi.json records the request to finish"
old=$(plugin_pod)
kd delete pod "$old" --wait --timeout=120s
until_ok 300 "a new node plugin pod" has_plugin_pod
plugin_ready
sleep 10
assert_eq Released "$(kubectl get pv "$FORK_PV" -o jsonpath='{.status.phase}')" \
  "across the node plugin's restart, the PV stays Released"
fgit rev-parse -q --verify "refs/agentfs/$FORK/finish" >/dev/null 2>&1 &&
  fail "the finish ref reached a blocked remote"
chmod "$mode" "$FORKS_REMOTE"
until_ok 300 "the fork remote has the finish ref" ref_exists "$FORKS_REMOTE" "refs/agentfs/$FORK/finish"
CHANGE=$(fgit rev-parse "refs/agentfs/$FORK/change") || fail "no change ref"
sgit merge-base --is-ancestor "$(fgit rev-parse "$CHANGE^")" "$SOURCE_BRANCH" ||
  fail "the change's parent is not on the source's branch"
pass "the change's parent is base, a commit of the source's branch"
assert_eq "from the sandbox" "$(fgit show "$CHANGE:sandbox.txt")" "the change holds the sandbox's file"
assert_eq "$(fgit rev-parse "refs/agentfs/$FORK/finish")" "$(fgit rev-parse "$FORK_BRANCH")" \
  "the finish commit is the fork branch's head"
until_ok 600 "the fork's PV is gone once published" pv_gone "$FORK_PV"
until_ok 60 "the fork's volume directory is removed" node_path_gone "$BASE/$FORK_PV"
ka get pod writer -o jsonpath='{.status.phase}' | grep -q Running || fail "the source's pod stopped"
pass "the source stayed in use throughout"

step "(5) the change, submitted twice at once, applies once"
git clone -q "$SOURCE_REMOTE" "$W/producer"
git -C "$W/producer" fetch -q "$FORKS_REMOTE" "refs/agentfs/$FORK/change"
submit() {
  docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -e GIT_AUTHOR_NAME=receiver -e GIT_AUTHOR_EMAIL=receiver@example.invalid \
    -e GIT_COMMITTER_NAME=receiver -e GIT_COMMITTER_EMAIL=receiver@example.invalid \
    -v "$W:$W" -v "$GIT_SERVER_REPOS:$GIT_SERVER_REPOS" \
    --entrypoint agentfs agentfs:ci \
    patch submit "$CHANGE" --volume "$SOURCE_VOLUME" --remote "$SOURCE_REMOTE" \
    --repo "$W/producer" --attempts 10
}
submit >"$W/submit-1.log" 2>&1 &
p1=$!
submit >"$W/submit-2.log" 2>&1 &
p2=$!
wait "$p1" || fail "the first submit failed: $(cat "$W/submit-1.log")"
wait "$p2" || fail "the second submit failed: $(cat "$W/submit-2.log")"
pass "both submits queued the change"
ka delete pod writer --wait --timeout=120s
kubectl apply -k ci/k8s-csi/forks/apps
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never came back"
until_ok 240 "the source's branch has both inbox commits of the change" inbox_commits_are 2
messages=$(sgit log --first-parent --format='%s' --grep="^Agentfs-Patch: $CHANGE\$" "$SOURCE_BRANCH")
echo "$messages"
assert_eq 1 "$(grep -c '^duplicate of patch ' <<<"$messages")" "one is a duplicate marker"
assert_eq 1 "$(grep -vc '^duplicate of patch ' <<<"$messages")" "one applies the change"
assert_eq "from the sandbox" "$(sgit show "$SOURCE_BRANCH:sandbox.txt")" "the source has the sandbox's file"
assert_eq "from the sandbox" "$(in_pod writer 'cat /data/sandbox.txt')" "the source's pod sees it"

step "e2e-csi-forks passed"
