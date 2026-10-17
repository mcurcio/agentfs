#!/usr/bin/env bash
# End-to-end test of one branch per worktree on a two-worker kind cluster
# (F8, design §14.2, docs/runbooks/moving.md): node-local worktrees, a
# volume continued on another node by a new volume started from the old
# branch, the old node's late commits reaching the old branch and alarming
# on the new volume, and DeleteVolume removing nothing a remote lacks.
# The driver comes from charts/agentfs-csi in this tree; the remote is the
# git server image (examples/docker/git-server) as a pod pinned to worker
# B, its repositories on B's disk. Each line is one assertion:
#
#   a. a pod on worker A writes f (pushed to agentfs/V1); the server is
#      stopped; the pod writes g;
#   b. A's node container stops; the pod is force-deleted and its claim
#      deleted; the PV stays Released;
#   c. the server restarts; a start class pinned to origin:agentfs/V1@f
#      and a new claim: the pod runs on B, reads f, not g, and writes h;
#      agentfs/V2 holds h with f as an ancestor; B's plugin log has no
#      start-branch alarm;
#   d. the server stops; A restarts; A's plugin log shows PushFailed for
#      V1 at least twice (the release scan), and the PV still exists
#      (DeleteVolume answers UNAVAILABLE naming the remote);
#   e. the server starts: agentfs/V1 gets g; the PV is then deleted and,
#      with onDelete: delete, V1's directory on A is gone;
#   f. B's next push raises StartBranchMoved naming agentfs/V1, f and g;
#      `agentfs status` in B's plugin shows it; `agentfs start accept
#      --tip g` there; the next push raises none, and the remote holds
#      refs/agentfs/V2/accepted/g;
#   g. a CLI volume with V1's volume name, pushing to the server:
#      diverged, agentfs/V1 unchanged, a refs/agentfs/<V1>/diverged/* ref
#      saved;
#   h. a class whose startFrom names refs/agentfs/<V1>/diverged/...: the
#      claim stays Pending, with a ProvisioningFailed event naming "not a
#      branch";
#   i. with B's worktree directory removed, a pod on the same claim
#      waits; its events name the missing worktree.
#
# Needs docker, kind, kubectl and helm, a kind cluster $KIND_CLUSTER
# (default agentfs-move) made from ci/kind/two-workers.yaml with the images
# agentfs:ci and agentfs-git-server:ci loaded, and FUSE on the host.
# Locally:
#
#   docker build -t agentfs:ci . && docker build -t agentfs-git-server:ci examples/docker/git-server
#   kind create cluster --name agentfs-move --config ci/kind/two-workers.yaml --image "$KIND_NODE_IMAGE"
#   kind load docker-image agentfs:ci agentfs-git-server:ci --name agentfs-move
#   ci/e2e-csi-move.sh            # ci/e2e-csi-move.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-move}
NODE_A=$KIND_CLUSTER-worker
NODE_B=$KIND_CLUSTER-worker2
DNS=agentfs-csi
APPS=agentfs-csi-e2e-move
BASE=/var/lib/agentfs-csi/volumes
NODE_STATE=/var/lib/agentfs-csi/node
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
REPO=/srv/git/history.git
PROFILE=/etc/agentfs-csi/profiles/ci.config

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
    for c in agentfs-csi csi-provisioner git-server; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=100 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  for node in "$NODE_A" "$NODE_B"; do
    echo "---- worktrees on $node"
    docker exec "$node" sh -c "ls -la $BASE $NODE_STATE/staged; cat $BASE/*/csi*.json" 2>&1
  done
  return 0
}

case "${1:-}" in
diagnose) diagnose; exit 0 ;;
"") ;;
*) echo "usage: $0 [diagnose]" >&2; exit 2 ;;
esac

plugin_on() { # node
  kd get pod -l "$PLUGIN_LABEL" --field-selector "spec.nodeName=$1" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
on() { # node command...
  local node=$1
  shift
  kd exec "$(plugin_on "$node")" -c agentfs-csi -- "$@"
}
plugin_log() { kd logs "$(plugin_on "$1")" -c agentfs-csi 2>/dev/null; }
provisioner_log() { kd logs "$(plugin_on "$1")" -c csi-provisioner 2>/dev/null; }
# The remote, while the server runs.
sgit() { kd exec deploy/git-server -c git-server -- git --git-dir "$REPO" "$@"; }
in_pod() { ka exec "$1" -- sh -c "$2"; }

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
remote_file_is() { [ "$(sgit show "$1:$2" 2>/dev/null)" = "$3" ]; } # ref path content
remote_ref_is() { [ "$(sgit rev-parse -q --verify "$1" 2>/dev/null)" = "$2" ]; } # ref oid
local_file_is() { [ "$(on "$1" git --git-dir "$2" show "$3:$4" 2>/dev/null)" = "$5" ]; } # node git ref path content
node_ready_is() { [ "$(kubectl get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = "$2" ]; }
plugin_ready() { kd wait --for=condition=Ready "pod/$(plugin_on "$1")" --timeout=5s; }
pv_phase_is() { [ "$(kubectl get pv "$1" -o jsonpath='{.status.phase}' 2>/dev/null)" = "$2" ]; }
pv_gone() { ! kubectl get pv "$1" >/dev/null 2>&1; }
node_path_gone() { ! docker exec "$1" test -e "$2"; }
server_down() { [ -z "$(kd get pods -l app=git-server -o name)" ]; }
push_failures_at_least() { # node pv n
  local said # captured first: grep -c closing a pipe would fail it (pipefail)
  said=$(plugin_log "$1")
  [ "$(grep -c "\[push $2\] alarm: push to origin failed" <<<"$said")" -ge "$3" ]
}
delete_unavailable() { # node pv
  local said
  said=$(provisioner_log "$1")
  grep "$2" <<<"$said" | grep -q "Unavailable.*remote origin could not be listed"
}
log_has() { # node text
  local said
  said=$(plugin_log "$1")
  grep -qF -- "$2" <<<"$said"
}
status_lacks() { # node state text
  local said
  said=$(on "$1" agentfs --state "$2" status 2>&1) || return 1
  ! grep -qF -- "$3" <<<"$said"
}

server() { # replicas
  kd scale deployment/git-server --replicas="$1"
  if [ "$1" = 0 ]; then
    until_ok 120 "the git server is stopped" server_down
  else
    kd rollout status deployment/git-server --timeout=180s
    pass "the git server runs on $NODE_B"
  fi
}

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

start_class() { # name startFrom
  kubectl apply -f - <<EOF
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: $1
provisioner: agentfs.mcurcio.github.io
parameters:
  profile: ci
  onDelete: delete
  startFrom: "$2"
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
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

step "cluster, git server and driver"
kubectl get nodes -o wide
for node in "$NODE_A" "$NODE_B"; do
  kubectl get node "$node" >/dev/null || fail "no node $node (ci/kind/two-workers.yaml)"
done
kubectl apply -f deploy/csi/namespace.yaml
kubectl apply -f - <<EOF
apiVersion: v1
kind: Secret
metadata:
  name: git-server-origin
  namespace: $DNS
stringData:
  # The CI-only password the profile's credentials carry (values.yaml).
  origin: |
    username=agentfs
    password=ci-csi-password
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: git-server
  namespace: $DNS
spec:
  replicas: 1
  strategy: { type: Recreate }
  selector:
    matchLabels: { app: git-server }
  template:
    metadata:
      labels: { app: git-server }
    spec:
      # Worker B: its disk keeps the repositories while the server is
      # stopped, and A can be stopped without taking the remote down.
      nodeSelector: { kubernetes.io/hostname: $NODE_B }
      terminationGracePeriodSeconds: 2
      containers:
        - name: git-server
          image: agentfs-git-server:ci
          imagePullPolicy: IfNotPresent
          ports: [{ name: http, containerPort: 8080 }]
          readinessProbe:
            tcpSocket: { port: 8080 }
            periodSeconds: 2
          volumeMounts:
            - { name: repos, mountPath: /srv/git }
            - { name: origin, mountPath: /run/secrets, readOnly: true }
      volumes:
        - name: repos
          hostPath: { path: /var/lib/ci-git-server, type: DirectoryOrCreate }
        - name: origin
          secret: { secretName: git-server-origin }
---
apiVersion: v1
kind: Service
metadata:
  name: git-server
  namespace: $DNS
spec:
  selector: { app: git-server }
  ports: [{ name: http, port: 8080, targetPort: http }]
EOF
kd rollout status deployment/git-server --timeout=180s
helm install agentfs-csi charts/agentfs-csi --namespace "$DNS" \
  -f ci/k8s-csi/move/values.yaml \
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

step "(a) a pod on A writes f, pushed; the server stops; the pod writes g"
claim v1 agentfs-move
writer writer-1 v1 "$NODE_A"
ka wait --for=condition=Ready pod/writer-1 --timeout=300s >/dev/null || fail "writer-1 never became Ready"
PV1=$(ka get pvc v1 -o jsonpath='{.spec.volumeName}')
V1=$APPS.v1.$PV1
BRANCH1=refs/heads/agentfs/$V1
GIT1=$BASE/$PV1/state/git
echo "V1: $V1 ($PV1) on $NODE_A"
assert_eq "$NODE_A" "$(kubectl get pv "$PV1" -o jsonpath='{.spec.nodeAffinity.required.nodeSelectorTerms[0].matchExpressions[0].values[0]}')" \
  "V1's PV is pinned to A, whose disk holds its worktree"
docker exec "$NODE_A" test -d "$BASE/$PV1/state" || fail "V1's worktree is not on A's disk"
docker exec "$NODE_B" test ! -e "$BASE/$PV1" || fail "V1's worktree is on B's disk"
pass "V1's worktree is on A's disk only"
in_pod writer-1 'printf f > /data/f.txt'
until_ok 120 "agentfs/V1 on the remote has f.txt" remote_file_is "$BRANCH1" f.txt f
F=$(sgit rev-parse "$BRANCH1")
server 0
in_pod writer-1 'printf g > /data/g.txt'
until_ok 60 "g is committed on A, on no remote" local_file_is "$NODE_A" "$GIT1" "$BRANCH1" g.txt g
G=$(on "$NODE_A" git --git-dir "$GIT1" rev-parse "$BRANCH1")
echo "f $F, g $G"

step "(b) A stops; the pod and its claim are deleted; the PV stays Released"
docker stop "$NODE_A" >/dev/null
until_ok 180 "$NODE_A is NotReady" node_ready_is "$NODE_A" Unknown
ka delete pod writer-1 --force --grace-period=0 >/dev/null 2>&1 || true
ka delete pvc v1 --wait=false
until_ok 120 "V1's PV is Released" pv_phase_is "$PV1" Released
sleep 10
assert_eq Released "$(kubectl get pv "$PV1" -o jsonpath='{.status.phase}')" \
  "with A's provisioner gone, nothing deletes V1's PV"

step "(c) a start class pinned at f; the new claim's pod runs on B"
server 1
start_class agentfs-move-v1 "origin:agentfs/$V1@$F"
claim v2 agentfs-move-v1
writer writer-2 v2
ka wait --for=condition=Ready pod/writer-2 --timeout=300s >/dev/null || fail "writer-2 never became Ready"
assert_eq "$NODE_B" "$(ka get pod writer-2 -o jsonpath='{.spec.nodeName}')" "the pod runs on B"
PV2=$(ka get pvc v2 -o jsonpath='{.spec.volumeName}')
V2=$APPS.v2.$PV2
BRANCH2=refs/heads/agentfs/$V2
STATE2=$BASE/$PV2/state
echo "V2: $V2 ($PV2) on $NODE_B"
assert_eq f "$(in_pod writer-2 'cat /data/f.txt')" "V2 reads f"
in_pod writer-2 'test ! -e /data/g.txt' || fail "V2 reads g, which is after the pin"
pass "V2 does not have g"
in_pod writer-2 'printf h > /data/h.txt'
until_ok 120 "agentfs/V2 on the remote has h.txt" remote_file_is "$BRANCH2" h.txt h
sgit merge-base --is-ancestor "$F" "$BRANCH2" || fail "f is not an ancestor of agentfs/V2"
pass "agentfs/V2 holds h with f as an ancestor"
assert_eq "$F" "$(sgit rev-parse "$BRANCH1")" "agentfs/V1 is still at f"
if log_has "$NODE_B" "start branch moved"; then
  fail "B raised the start-branch alarm while agentfs/V1 is at the pin"
fi
pass "B's plugin log has no start-branch alarm"
kubectl delete storageclass agentfs-move-v1

step "(d) the server stops; A comes back: its release scan keeps failing; the PV stays"
server 0
docker start "$NODE_A" >/dev/null
until_ok 300 "$NODE_A is Ready again" node_ready_is "$NODE_A" True
until_ok 300 "the node plugin on A is Ready again" plugin_ready "$NODE_A"
until_ok 300 "A's release scan alarms PushFailed for V1 at least twice" \
  push_failures_at_least "$NODE_A" "$PV1" 2
kubectl get pv "$PV1" >/dev/null || fail "V1's PV was deleted while A held commits no remote has"
pass "V1's PV still exists"
until_ok 300 "DeleteVolume answers UNAVAILABLE naming the remote" delete_unavailable "$NODE_A" "$PV1"
docker exec "$NODE_A" test -d "$BASE/$PV1/state" || fail "V1's worktree left A's disk"
pass "V1's worktree is still on A"

step "(e) the server starts: agentfs/V1 gets g; then the PV goes"
server 1
until_ok 300 "agentfs/V1 on the remote gets g" remote_ref_is "$BRANCH1" "$G"
until_ok 300 "V1's PV is deleted" pv_gone "$PV1"
until_ok 60 "V1's directory on A is gone (onDelete: delete)" node_path_gone "$NODE_A" "$BASE/$PV1"

step "(f) B's next push raises StartBranchMoved; accepting g clears it"
until_ok 120 "B alarms that agentfs/V1 moved past the pin" \
  log_has "$NODE_B" "start branch moved: $BRANCH1 on origin, which this volume started from at $F, is now at $G"
status=$(on "$NODE_B" agentfs --state "$STATE2" status)
echo "$status"
assert_contains "$status" "start:     origin:agentfs/$V1@$F" "status names the start point"
assert_contains "$status" "start branch moved: $F..$G (agentfs start accept --tip $G)" \
  "status shows the start-branch alarm"
on "$NODE_B" agentfs --state "$STATE2" start accept --tip "$G"
until_ok 120 "the next push raises no alarm" status_lacks "$NODE_B" "$STATE2" "start branch moved"
until_ok 120 "the remote holds refs/agentfs/V2/accepted/g" \
  remote_ref_is "refs/agentfs/$V2/accepted/$G" "$G"

step "(g) a CLI volume named like V1 pushes: diverged, agentfs/V1 unchanged"
on "$NODE_B" sh -c "rm -rf /tmp/cli && mkdir -p /tmp/cli/tree && printf cli > /tmp/cli/tree/cli.txt &&
  agentfs --state /tmp/cli/state --root /tmp/cli/tree init --volume '$V1' --include $PROFILE &&
  AGENTFS_NAMESPACE=ci AGENTFS_POD=cli AGENTFS_POD_UID=cli \
    agentfs --state /tmp/cli/state --root /tmp/cli/tree record cli.txt"
if out=$(on "$NODE_B" agentfs --state /tmp/cli/state push 2>&1); then
  fail "a second writer of agentfs/V1 pushed: $out"
fi
echo "$out"
assert_contains "$out" "diverged" "the CLI volume's push is diverged"
assert_eq "$G" "$(sgit rev-parse "$BRANCH1")" "agentfs/V1 is unchanged"
DIVERGED=$(sgit for-each-ref --format='%(refname)' "refs/agentfs/$V1/diverged/" | head -n1)
[ -n "$DIVERGED" ] || fail "no refs/agentfs/$V1/diverged/* ref was saved"
pass "the CLI volume's head is saved as $DIVERGED"

step "(h) a class starting from a diverged/* ref is refused: not a branch"
start_class agentfs-move-diverged "origin:$DIVERGED@$(sgit rev-parse "$DIVERGED")"
claim v3 agentfs-move-diverged
writer writer-3 v3
wait_event v3 ProvisioningFailed "not a branch" 180
assert_eq Pending "$(ka get pvc v3 -o jsonpath='{.status.phase}')" "the claim stays Pending"
ka delete pod writer-3 --wait=false
ka delete pvc v3 --wait=false
kubectl delete storageclass agentfs-move-diverged

step "(i) B's worktree removed: a pod on the claim waits, naming it"
ka delete pod writer-2 --wait --timeout=120s
until_ok 120 "V2 is unstaged on B" node_path_gone "$NODE_B" "$NODE_STATE/staged/$PV2.json"
docker exec "$NODE_B" rm -rf "$BASE/$PV2"
writer writer-2b v2
wait_event writer-2b FailedMount "no worktree on node $NODE_B" 180
assert_eq Pending "$(ka get pod writer-2b -o jsonpath='{.status.phase}')" "the pod waits"

step "e2e-csi-move passed"
