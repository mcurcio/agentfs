#!/usr/bin/env bash
# End-to-end test of the CSI chart's git server (server.enabled, F8, design
# §14.2 "The server", docs/helm.md "The git server") on a one-node kind
# cluster: the chart from this tree with charts/agentfs-csi/ci/server.yaml,
# its server on the cluster's default StorageClass with the chart's own
# random password, and a profile pushing to it with the credentials the
# chart wires into the node plugin. Each line is one assertion:
#
#   a. rendering: with the server off, no server object and the plain
#      credentials Secret; with it on, a Secret, Service and StatefulSet,
#      and the node plugin projects the `token` key of the very Secret
#      whose `htpasswd` key the server reads;
#   b. the install's notes name the server's URL and the credentials key;
#   c. a pod writes a file: `git ls-remote` through `kubectl port-forward`
#      to the Service shows agentfs/<volume> at the worktree's head, and
#      that commit holds the file;
#   d. a wrong password is refused, and the server logs the mismatch;
#   e. the server pod is deleted and comes back with the repository intact
#      (its claim), and the next write reaches it;
#   f. `helm upgrade` keeps the password (the chart's lookup).
#
# Needs docker, kind, kubectl, helm and git, a kind cluster $KIND_CLUSTER
# (default agentfs-server) made from ci/kind/cluster.yaml with the images
# agentfs:ci and agentfs-git-server:ci loaded, and FUSE on the host.
# Locally:
#
#   docker build -t agentfs:ci . && docker build -t agentfs-git-server:ci examples/docker/git-server
#   kind create cluster --name agentfs-server --config ci/kind/cluster.yaml --image "$KIND_NODE_IMAGE"
#   kind load docker-image agentfs:ci agentfs-git-server:ci --name agentfs-server
#   ci/e2e-chart-server.sh        # ci/e2e-chart-server.sh diagnose after a failure

. "$(dirname "$0")/lib.sh"

DNS=agentfs-csi
APPS=agentfs-csi-e2e-server
CHART=charts/agentfs-csi
VALUES=$CHART/ci/server.yaml
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
SERVER=agentfs-git-server
PORT=18080
WORK=$(mktemp -d)
PF=
# A refused password fails git at once instead of prompting.
export GIT_TERMINAL_PROMPT=0

kd() { kubectl -n "$DNS" "$@"; }
ka() { kubectl -n "$APPS" "$@"; }

diagnose() {
  set +e
  step "diagnose"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  kubectl get pv,storageclass -o wide
  kd get statefulset,pvc,secret -o wide
  ka get pvc,pods -o wide
  ka describe pvc
  ka describe pods
  kd describe pod "$SERVER-0"
  ka get events --sort-by=.lastTimestamp | tail -n 40
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    for c in agentfs-csi csi-provisioner init git-server; do
      kd logs "$pod" -c "$c" --tail=200 2>/dev/null | sed "s|^|$pod/$c: |"
      kd logs "$pod" -c "$c" --previous --tail=100 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  return 0
}

case "${1:-}" in
diagnose) diagnose; exit 0 ;;
"") ;;
*) echo "usage: $0 [diagnose]" >&2; exit 2 ;;
esac

cleanup() {
  [ -z "$PF" ] || kill "$PF" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

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

plugin() { kd get pod -l "$PLUGIN_LABEL" -o jsonpath='{.items[0].metadata.name}'; }
local_head() { # pv volume
  kd exec "$(plugin)" -c agentfs-csi -- git --git-dir "$BASE/$1/state/git" rev-parse "refs/heads/agentfs/$2"
}
# The token as the chart made it: `username=` and `password=` lines.
token() { kd get secret "$SERVER" -o jsonpath='{.data.token}' | base64 -d; }
url() { # password
  echo "http://agentfs:$1@127.0.0.1:$PORT/git/history.git"
}
remote_ref() { # ref
  local said
  said=$(git ls-remote "$(url "$PASSWORD")" "$1") || return 1
  awk '{print $1}' <<<"$said"
}
remote_ref_is() { [ "$(remote_ref "$1")" = "$2" ]; } # ref oid
pushed() { # pv volume: the remote's branch is at the worktree's head
  local head
  head=$(local_head "$1" "$2") || return 1
  remote_ref_is "refs/heads/agentfs/$2" "$head"
}
port_forward() {
  [ -z "$PF" ] || kill "$PF" 2>/dev/null || true
  kd port-forward "svc/$SERVER" "$PORT:8080" >"$WORK/port-forward.log" 2>&1 &
  PF=$!
  until_ok 60 "kubectl port-forward reaches the Service" git ls-remote "$(url "$PASSWORD")"
}
in_pod() { ka exec writer -- sh -c "$1"; }

step "(a) rendering"
helm lint --strict "$CHART" -f "$VALUES"
off=$(helm template agentfs-csi "$CHART" --namespace "$DNS")
if grep -q "name: $SERVER" <<<"$off"; then
  fail "with server.enabled false the chart renders a server object"
fi
pass "the server is off by default: no server object"
grep -q "secret: { secretName: agentfs-csi-credentials, defaultMode: 0400 }" <<<"$off" ||
  fail "with the server off, the node plugin's credentials are not the plain Secret"
pass "with the server off, the node plugin mounts the credentials Secret as before"
on=$(helm template agentfs-csi "$CHART" --namespace "$DNS" -f "$VALUES" --show-only templates/server.yaml)
for kind in Secret Service StatefulSet; do
  grep -q "^kind: $kind$" <<<"$on" || fail "server.yaml renders no $kind"
done
pass "server.yaml renders a Secret, a Service and a StatefulSet"
# The Secret the node plugin projects `token` from, and the one the server
# reads `htpasswd` from (each captured first: grep -q closing a pipe would
# fail it under pipefail).
projected() { # rendered node.yaml
  local said
  said=$(grep -A2 -- "- secret:$" <<<"$1")
  grep -A1 "name: " <<<"$said"
}
render() { helm template agentfs-csi "$CHART" --namespace "$DNS" -f "$VALUES" "$@"; }
node=$(render --show-only templates/node.yaml)
token_from=$(projected "$node")
grep -qx "  *name: $SERVER" <<<"$token_from" || fail "the node plugin does not project Secret $SERVER"
grep -q "items: \[{ key: token, path: agentfs-server }\]" <<<"$token_from" ||
  fail "the node plugin does not mount key token of $SERVER as agentfs-server"
htpasswd_from=$(grep -A1 "secretName: " <<<"$on")
if ! grep -q "secretName: $SERVER$" <<<"$htpasswd_from" ||
  ! grep -q "items: \[{ key: htpasswd, path: htpasswd }\]" <<<"$htpasswd_from"; then
  fail "the server does not read key htpasswd of Secret $SERVER"
fi
pass "one Secret: the server reads its htpasswd, the node plugin its token as agentfs-server"
on=$(render --show-only templates/server.yaml --set server.credentials.existingSecret=mine)
if grep -q "^kind: Secret$" <<<"$on"; then
  fail "with server.credentials.existingSecret the chart still makes its Secret"
fi
grep -q "secretName: mine$" <<<"$on" || fail "with server.credentials.existingSecret the server reads another Secret"
token_from=$(projected "$(render --show-only templates/node.yaml --set server.credentials.existingSecret=mine)")
grep -qx "  *name: mine" <<<"$token_from" ||
  fail "with server.credentials.existingSecret the node plugin projects another Secret"
pass "server.credentials.existingSecret replaces the chart's Secret for both"

step "(b) install"
kubectl apply -f deploy/csi/namespace.yaml
helm install agentfs-csi "$CHART" --namespace "$DNS" -f "$VALUES" --wait --timeout 5m
kd rollout status "statefulset/$SERVER" --timeout=180s
kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=180s
notes=$(helm get notes agentfs-csi --namespace "$DNS")
assert_contains "$notes" "url = http://$SERVER.$DNS.svc:8080/git/history.git" "the notes name the server's URL"
assert_contains "$notes" "agentfsCredentials = /etc/agentfs-csi/credentials/agentfs-server" \
  "the notes name the credentials key"
PASSWORD=$(token | sed -n 's/^password=//p')
[ ${#PASSWORD} = 32 ] || fail "the chart's password is not 32 characters"
pass "the chart made a random password"
kubectl apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $APPS
  labels:
    pod-security.kubernetes.io/enforce: restricted
EOF

step "(c) a pod writes; the server holds agentfs/<volume> at the worktree's head"
ka apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: data
spec:
  storageClassName: agentfs-server
  accessModes: [ReadWriteOnce]
  resources: { requests: { storage: 1Gi } }
---
apiVersion: v1
kind: Pod
metadata:
  name: writer
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
      persistentVolumeClaim: { claimName: data }
EOF
ka wait --for=condition=Ready pod/writer --timeout=300s >/dev/null || fail "the writer never became Ready"
PV=$(ka get pvc data -o jsonpath='{.spec.volumeName}')
V=$APPS.data.$PV
echo "volume $V ($PV)"
port_forward
in_pod 'printf one > /data/one.txt'
until_ok 120 "agentfs/$V on the server is at the worktree's head" pushed "$PV" "$V"
ONE=$(remote_ref "refs/heads/agentfs/$V")
git init -q --bare "$WORK/clone.git"
git --git-dir "$WORK/clone.git" fetch -q "$(url "$PASSWORD")" "refs/heads/agentfs/$V"
assert_eq one "$(git --git-dir "$WORK/clone.git" show "$ONE:one.txt")" "the pushed commit $ONE holds one.txt"

step "(d) a wrong password is refused"
if git ls-remote "$(url wrong-password)" >"$WORK/wrong.log" 2>&1; then
  fail "the server accepted a wrong password"
fi
pass "git ls-remote with a wrong password fails"
until_ok 30 "the server logs the password mismatch (401)" \
  sh -c "kubectl -n $DNS logs $SERVER-0 -c git-server | grep -q 'user \"agentfs\": password mismatch'"

step "(e) the server pod is deleted and comes back with the repository"
uid=$(kd get pod "$SERVER-0" -o jsonpath='{.metadata.uid}')
kd delete pod "$SERVER-0" --wait=true
until_ok 180 "a new server pod is Ready" sh -c \
  "[ \"\$(kubectl -n $DNS get pod $SERVER-0 -o jsonpath='{.metadata.uid}')\" != '$uid' ] &&
   kubectl -n $DNS wait --for=condition=Ready pod/$SERVER-0 --timeout=5s"
port_forward
assert_eq "$ONE" "$(remote_ref "refs/heads/agentfs/$V")" "the repository survived the pod (its claim)"
in_pod 'printf two > /data/two.txt'
until_ok 120 "the next write reaches the new server pod" pushed "$PV" "$V"
TWO=$(remote_ref "refs/heads/agentfs/$V")
[ "$TWO" != "$ONE" ] || fail "agentfs/$V did not move"
git --git-dir "$WORK/clone.git" fetch -q "$(url "$PASSWORD")" "refs/heads/agentfs/$V"
git --git-dir "$WORK/clone.git" merge-base --is-ancestor "$ONE" "$TWO" ||
  fail "the second commit does not follow the first"
pass "agentfs/$V moved from $ONE to $TWO"

step "(f) helm upgrade keeps the password"
before=$(token)
helm upgrade agentfs-csi "$CHART" --namespace "$DNS" -f "$VALUES" --wait --timeout 5m
assert_eq "$before" "$(token)" "the token is unchanged after helm upgrade"
git ls-remote "$(url "$PASSWORD")" >/dev/null || fail "the password no longer works after helm upgrade"
pass "the password still works after helm upgrade"

step "clean up"
ka delete pod writer --wait=true
ka delete pvc data --wait=true
pass "the chart's git server works"
