#!/usr/bin/env bash
# The CSI driver on several nodes (design §14.2, §18 test 16): a kind
# cluster of a control plane and two workers whose basePath is one NFS
# export on the runner, the driver deployed with --multi-node
# (ci/k8s-csi/multinode), and checks, from real pods:
#
#   a. the driver registers on both workers; the RBAC inventory is the
#      single-node one (no Service, no API reads for TLS);
#   b. two restricted pods of one ReadWriteMany claim, on two nodes,
#      write; their commits reach the runner's git server with distinct
#      committers, pod and node trailers; a file made on one node is
#      renamed and removed through the other;
#   c. `agentfs status` names the active service's node, and the other
#      node runs a standby;
#   d. the round trip: ping-history from the standby's node (TLS over the
#      pod network) and from the active node (Unix socket), into the step
#      summary (close latencies through NFS are measured by e2e-nfs);
#   e. kill -9 of the active service: the standby on the other node takes
#      over, writes resume, every acknowledged close is in history;
#   f. a node reboot (`docker restart` of the active service's worker):
#      the other pod's failed closes are confined to the failover window
#      (measured and printed); the node comes back with NFS mounted, its
#      pod is re-staged and records again; every acknowledged close is in
#      history;
#   g. the node plugin upgrade procedure (docs/csi.md §9): a new image tag,
#      then per worker cordon, evict its writer, delete its plugin pod,
#      wait Ready, uncordon; no close fails on the other worker
#      meanwhile; the handover time is printed;
#   h. an RWO claim used by two pods on two nodes: the second is refused
#      (Multi-Attach);
#   i. teardown: unpublish, unstage on both nodes, the final push, then
#      DeleteVolume.
#
# Needs docker, kind and kubectl (ci/install-kind.sh), the NFS server of
# ci/nfs-lib.sh exporting $NFS_EXPORT on the kind network's gateway, a
# cluster $KIND_CLUSTER (default agentfs-mn) from ci/kind/multinode.yaml
# with the image agentfs:ci loaded and ci/kind/nfs-node-mount.sh run on
# each worker, and the git server of ci/git-http-server.sh. The
# e2e-csi-multinode job in .github/workflows/image.yml does all of that.

. "$(dirname "$0")/lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-mn}
DNS=agentfs-csi
APPS=agentfs-csi-e2e-apps
DRIVER=agentfs.mcurcio.github.io
BASE=/var/lib/agentfs-csi/volumes
PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
LOGS=${E2E_CSI_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-csi-mn-logs}
mkdir -p "$LOGS"
SUMMARY=$LOGS/summary.md
: >"$SUMMARY"
OUT_LOG=$LOGS/e2e-csi-multinode.out.log
if [ -z "${1:-}" ]; then
  exec > >(tee "$OUT_LOG") 2>&1
  trap 'status=$?; [ $status -eq 0 ] || annotate_log "$OUT_LOG" 30' EXIT
fi

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
  ka describe pods | tail -n 120
  ka get events --sort-by=.lastTimestamp | tail -n 60
  kd get events --sort-by=.lastTimestamp | tail -n 40
  for pod in $(kd get pods -o name); do
    kd logs "$pod" -c agentfs-csi --tail=150 2>/dev/null | sed "s|^|$pod: |"
    kd logs "$pod" -c agentfs-csi --previous --tail=60 2>/dev/null | sed "s|^|$pod (previous): |"
  done
  for n in $(kind get nodes --name "$KIND_CLUSTER"); do
    echo "---- $n"
    docker exec "$n" findmnt -t nfs4
    docker exec "$n" sh -c "ps -eo pid,args | grep -E 'agentfs|tini' | grep -v grep"
  done
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

plugin_on() { # node
  kd get pod -l "$PLUGIN_LABEL" --field-selector "spec.nodeName=$1" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
in_plugin_on() { # node args...
  local node=$1; shift
  kd exec "$(plugin_on "$node")" -c agentfs-csi -- "$@"
}
writer_on() { # node
  ka get pod -l app=writer --field-selector "spec.nodeName=$1,status.phase=Running" \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null
}

# Every name the writers acknowledged (closes that returned 0), over the
# writers' ledgers, now; and the failed ones with their times.
ledgers() { # acked|failed
  local pod
  for pod in $(ka get pod -l app=writer -o jsonpath='{.items[*].metadata.name}'); do
    ka exec "$pod" -- cat "/ledger/$1" 2>/dev/null || true
  done
}
ACKED_FILE=$LOGS/acked
FAILED_FILE=$LOGS/failed
: >"$ACKED_FILE"
: >"$FAILED_FILE"
# Ledgers live in emptyDirs: save them before a pod goes.
save_ledgers() {
  ledgers acked >>"$ACKED_FILE"
  ledgers failed >>"$FAILED_FILE"
  sort -u -o "$ACKED_FILE" "$ACKED_FILE"
  sort -u -o "$FAILED_FILE" "$FAILED_FILE"
}
writers_run() { # on|off
  local pod
  for pod in $(ka get pod -l app=writer -o jsonpath='{.items[*].metadata.name}'); do
    if [ "$1" = on ]; then
      ka exec "$pod" -- touch /ledger/run
    else
      ka exec "$pod" -- rm -f /ledger/run
    fi
  done
}

# Every acknowledged name is in the volume's history (on the remote),
# waiting up to two minutes for the commits and pushes.
check_acked() { # what
  local missing tree=$LOGS/tree deadline=$((SECONDS + 120))
  save_ledgers
  while :; do
    rgit ls-tree -r --name-only "$BRANCH" -- stream/ 2>/dev/null | sed 's|^stream/||' | sort -u >"$tree"
    missing=$(comm -23 "$ACKED_FILE" "$tree" | wc -l)
    [ "$missing" -eq 0 ] || [ $SECONDS -ge $deadline ] && break
    sleep 2
  done
  comm -23 "$ACKED_FILE" "$tree" | head -n 20
  assert_eq 0 "$missing" "$1: all $(wc -l <"$ACKED_FILE") acknowledged closes are in history"
}

active_node() {
  local any
  any=$(kubectl get nodes -l agentfs-ci/role -o jsonpath='{.items[0].metadata.name}')
  in_plugin_on "$any" sh -c "cat $STATE/meta/history/endpoint.json" 2>/dev/null |
    sed -n 's/.*"node": *"\([^"]*\)".*/\1/p'
}

step "git server and cluster"
rm -rf "$REMOTE"
git init -q --bare "$REMOTE"
set_password agentfs ci-csi-password
GW=$(gateway)
[ -n "$GW" ] || fail "no IPv4 gateway on the kind network"
mapfile -t WORKERS < <(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ "${#WORKERS[@]}" = 2 ] || fail "expected two workers, got ${WORKERS[*]}"
W1=${WORKERS[0]}
W2=${WORKERS[1]}
kubectl label node "$W1" agentfs-ci/role=worker-a --overwrite >/dev/null
kubectl label node "$W2" agentfs-ci/role=worker-b --overwrite >/dev/null
for n in "$W1" "$W2"; do
  assert_eq nfs4 "$(docker exec "$n" findmnt -no FSTYPE "$BASE")" "$n mounts basePath over NFS"
done
# The controller runs on the control plane: it needs basePath too.
CP=$(kubectl get nodes -l node-role.kubernetes.io/control-plane -o jsonpath='{.items[0].metadata.name}')
assert_eq nfs4 "$(docker exec "$CP" findmnt -no FSTYPE "$BASE")" "$CP mounts basePath over NFS"

step "(a) the driver with --multi-node"
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
kubectl apply -k ci/k8s-csi/multinode
kd rollout status deployment/agentfs-csi-controller --timeout=300s
# The DaemonSet is OnDelete: rollout status does not apply.
wait_for 300 sh -c "[ \"\$(kubectl -n $DNS get daemonset agentfs-csi-node -o jsonpath='{.status.numberReady}')\" = 3 ]" ||
  fail "the node plugins never became Ready on all three nodes"
for n in "$W1" "$W2"; do
  wait_for 120 sh -c "kubectl get csinode $n -o jsonpath='{.spec.drivers[*].name}' | grep -qw $DRIVER" ||
    fail "the driver never registered on $n"
  pass "the driver is registered on $n"
  assert_contains "$(kd logs "$(plugin_on "$n")" -c agentfs-csi)" "multi-node volumes accepted" \
    "the plugin on $n runs with --multi-node"
done
for check in "get pods" "list secrets" "create services" "list endpoints"; do
  # shellcheck disable=SC2086
  assert_eq no "$(kubectl auth can-i $check --as system:serviceaccount:$DNS:agentfs-csi-node -n $DNS 2>/dev/null || true)" \
    "the node plugin's account cannot $check"
done

step "(b) two pods on two nodes write one ReadWriteMany volume"
kubectl apply -k ci/k8s-csi/multinode-apps
ka rollout status deployment/writer --timeout=300s
PV=$(ka get pvc shared-rwx -o jsonpath='{.spec.volumeName}')
[ -n "$PV" ] || fail "the RWX claim is not bound"
VOLUME=$APPS.shared-rwx.$PV
BRANCH=agentfs/$VOLUME
STATE=$BASE/$PV/state
A=$(writer_on "$W1")
B=$(writer_on "$W2")
[ -n "$A" ] && [ -n "$B" ] || fail "the writers are not on both workers ($A, $B)"
echo "volume $PV; $A on $W1, $B on $W2"
ATTACHED=$(kubectl get volumeattachments -o jsonpath="{range .items[?(@.spec.source.persistentVolumeName==\"$PV\")]}{.spec.nodeName}{\"\\n\"}{end}" | sort | tr '\n' ' ')
assert_eq "$(printf '%s\n' "$W1" "$W2" | sort | tr '\n' ' ')" "$ATTACHED" "attached to both workers"
ka exec "$A" -- sh -c 'mkdir -p /data/stream && chmod 2775 /data/stream'
ka exec "$A" -- sh -c 'printf a > /data/from-a.txt'
ka exec "$B" -- sh -c 'printf b > /data/from-b.txt'
wait_for 90 sh -c "[ \"\$(git --git-dir '$REMOTE' show '$BRANCH:from-a.txt' 2>/dev/null)\" = a ]" ||
  fail "from-a.txt never reached the remote"
wait_for 90 sh -c "[ \"\$(git --git-dir '$REMOTE' show '$BRANCH:from-b.txt' 2>/dev/null)\" = b ]" ||
  fail "from-b.txt never reached the remote"
assert_eq "$A" "$(rgit log -1 --format=%cn "$BRANCH" -- from-a.txt)" "from-a.txt is $A's"
assert_eq "$B" "$(rgit log -1 --format=%cn "$BRANCH" -- from-b.txt)" "from-b.txt is $B's"
assert_contains "$(rgit log -1 --format=%B "$BRANCH" -- from-a.txt)" "Agentfs-Node: $W1" "with $W1's node trailer"
assert_contains "$(rgit log -1 --format=%B "$BRANCH" -- from-b.txt)" "Agentfs-Node: $W2" "with $W2's node trailer"
# Made on one node, renamed and removed through the other.
ka exec "$A" -- sh -c 'printf moved > /data/cross.txt'
wait_for 30 ka exec "$B" -- test -e /data/cross.txt || fail "$B never saw cross.txt"
ka exec "$B" -- mv /data/cross.txt /data/crossed.txt
wait_for 90 sh -c "[ \"\$(git --git-dir '$REMOTE' show '$BRANCH:crossed.txt' 2>/dev/null)\" = moved ]" ||
  fail "the rename never reached the remote"
assert_eq "$B" "$(rgit log -1 --format=%cn "$BRANCH" -- crossed.txt)" "the rename is $B's"
ka exec "$B" -- rm /data/crossed.txt
wait_for 90 sh -c "! git --git-dir '$REMOTE' cat-file -e '$BRANCH:crossed.txt' 2>/dev/null" ||
  fail "the removal never reached the remote"
pass "renames and removals across nodes are recorded"

step "(c) one active service, one standby"
ACTIVE=$(active_node)
[ -n "$ACTIVE" ] || fail "no endpoint names an active node"
if [ "$ACTIVE" = "$W1" ]; then STANDBY=$W2; else STANDBY=$W1; fi
STATUS=$(in_plugin_on "$STANDBY" agentfs --state "$STATE" status)
echo "$STATUS"
assert_contains "$STATUS" "endpoint:  node $ACTIVE" "status names the active node $ACTIVE"
assert_contains "$STATUS" "tls:" "with its TLS address"
assert_contains "$(kd logs "$(plugin_on "$STANDBY")" -c agentfs-csi)" "standby for volume" \
  "$STANDBY's service waits as a standby"

step "(d) the round trip between nodes"
# As the plugin's fronts do: --node is the Kubernetes node.
ping_from() { # node
  in_plugin_on "$1" sh -c "agentfs --state $STATE ping-history --count 2000 --node \"\$NODE_NAME\""
}
PING_REMOTE=$(ping_from "$STANDBY")
PING_LOCAL=$(ping_from "$ACTIVE")
echo "$PING_REMOTE"
echo "$PING_LOCAL"
assert_contains "$PING_REMOTE" "from node $STANDBY" "ping-history from the standby's node"
{
  echo "### CSI on two nodes over NFS (kind)"
  echo
  echo "Kernel $(uname -r). Fronts on the active service's node use its Unix socket; on the other node, TLS over the pod network."
  echo
  echo "- ping-history from $ACTIVE (unix): $PING_LOCAL"
  echo "- ping-history from $STANDBY (TLS): $PING_REMOTE"
} | tee -a "$SUMMARY"

step "(e) kill -9 of the active service"
writers_run on
sleep 5
# The service is the agentfs process whose arguments are
# `... --state <basePath>/<pv>/state ... serve-history`; never this shell.
docker exec "$ACTIVE" sh -c '
  for d in /proc/[0-9]*; do
    [ "${d#/proc/}" = $$ ] && continue
    c=$(tr "\0" " " <"$d/cmdline" 2>/dev/null) || continue
    case "$c" in
      */agentfs\ --state\ *"$1"/state\ *serve-history*) echo "kill -9 ${d#/proc/}: $c"; kill -9 "${d#/proc/}" ;;
    esac
  done' sh "$PV"
T0=$SECONDS
NEW=
while [ $((SECONDS - T0)) -lt 120 ]; do
  NEW=$(active_node)
  STATUS=$(in_plugin_on "$STANDBY" agentfs --state "$STATE" status 2>/dev/null || true)
  case "$STATUS" in *"lock:      held by"*serve-history*) [ -n "$NEW" ] && break ;; esac
  sleep 1
done
TAKEOVER=$((SECONDS - T0))
sleep 10
writers_run off
NEW=$(active_node)
echo "active after the kill: $NEW (within $TAKEOVER s)"
check_acked "after kill -9 of the service"
echo "- kill -9 of the active service on $ACTIVE: a service on $NEW active within $TAKEOVER s; $(wc -l <"$FAILED_FILE") failed close(s) in total so far" | tee -a "$SUMMARY"

step "(f) a node reboot: docker restart of the active service's worker"
ACTIVE=$(active_node)
if [ "$ACTIVE" = "$W1" ]; then OTHER=$W2; else OTHER=$W1; fi
OTHER_POD=$(writer_on "$OTHER")
save_ledgers
ka exec "$OTHER_POD" -- sh -c ': > /ledger/failed'
writers_run on
sleep 3
REBOOT=$(date -u +%s)
docker restart "$ACTIVE" >/dev/null
wait_for 300 sh -c "kubectl get node $ACTIVE -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].status}' | grep -q True" ||
  fail "$ACTIVE never became Ready after the restart"
pass "$ACTIVE is Ready again"
# The unit (ci/kind/nfs-node-mount.sh) mounts it at boot, before the
# kubelet; kind's node entrypoint may start units late.
if ! wait_for 60 sh -c "[ \"\$(docker exec $ACTIVE findmnt -no FSTYPE $BASE)\" = nfs4 ]"; then
  docker exec "$ACTIVE" systemctl status agentfs-nfs.service --no-pager 2>&1 | tail -n 15
  fail "basePath is not mounted again after the reboot"
fi
pass "basePath is mounted again after the reboot"
wait_for 300 sh -c "kubectl -n $APPS get pod -l app=writer --field-selector spec.nodeName=$ACTIVE,status.phase=Running -o name | grep -q ." ||
  fail "no writer runs on $ACTIVE again"
sleep 15
NEWPOD=$(writer_on "$ACTIVE")
ka exec "$NEWPOD" -- touch /ledger/run
sleep 10
writers_run off
FAILED_OTHER=$(ka exec "$OTHER_POD" -- cat /ledger/failed 2>/dev/null || true)
N_FAILED=$(printf '%s' "$FAILED_OTHER" | grep -c . || true)
if [ "$N_FAILED" -gt 0 ]; then
  FIRST=$(printf '%s\n' "$FAILED_OTHER" | awk '{print $2}' | sort -n | head -n1)
  LAST=$(printf '%s\n' "$FAILED_OTHER" | awk '{print $2}' | sort -n | tail -n1)
  WINDOW="$((FIRST - REBOOT))..$((LAST - REBOOT)) s after the restart"
else
  WINDOW="none"
fi
echo "- node reboot of $ACTIVE: $N_FAILED failed close(s) on $OTHER, window $WINDOW; the other node's standby took over" | tee -a "$SUMMARY"
check_acked "after the node reboot"
ka exec "$NEWPOD" -- sh -c 'printf back > /data/after-reboot.txt'
wait_for 90 sh -c "[ \"\$(git --git-dir '$REMOTE' show '$BRANCH:after-reboot.txt' 2>/dev/null)\" = back ]" ||
  fail "the rebooted node's pod never recorded again"
pass "the rebooted node's pod records again"

step "(g) the node plugin upgrade procedure (docs/csi.md §9)"
for n in "$W1" "$W2" "$CP"; do
  docker exec "$n" ctr -n k8s.io images tag docker.io/library/agentfs:ci docker.io/library/agentfs:ci-next >/dev/null ||
    fail "could not tag the image on $n"
done
kd set image daemonset/agentfs-csi-node agentfs-csi=agentfs:ci-next
save_ledgers
for pod in $(ka get pod -l app=writer -o jsonpath='{.items[*].metadata.name}'); do
  ka exec "$pod" -- sh -c ': > /ledger/failed'
done
writers_run on
UPGRADE_FAILED=0
for n in "$W1" "$W2"; do
  if [ "$n" = "$W1" ]; then other=$W2; else other=$W1; fi
  other_pod=$(writer_on "$other")
  T0=$SECONDS
  kubectl cordon "$n" >/dev/null
  save_ledgers
  ka delete pod --field-selector "spec.nodeName=$n" -l app=writer --wait --timeout=120s >/dev/null
  kd delete pod "$(plugin_on "$n")" --wait --timeout=120s >/dev/null
  wait_for 300 sh -c "[ -n \"\$(kubectl -n $DNS get pod -l $PLUGIN_LABEL --field-selector spec.nodeName=$n -o jsonpath='{.items[?(@.status.phase==\"Running\")].metadata.name}')\" ]" ||
    fail "the plugin on $n never came back"
  kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --field-selector "spec.nodeName=$n" --timeout=300s >/dev/null
  assert_eq "agentfs:ci-next" "$(kd get pod "$(plugin_on "$n")" -o jsonpath='{.spec.containers[0].image}')" \
    "the plugin on $n runs the new image"
  kubectl uncordon "$n" >/dev/null
  wait_for 300 sh -c "kubectl -n $APPS get pod -l app=writer --field-selector spec.nodeName=$n,status.phase=Running -o name | grep -q ." ||
    fail "the writer never came back on $n"
  sleep 10
  ka exec "$(writer_on "$n")" -- touch /ledger/run
  f=$(ka exec "$other_pod" -- sh -c 'grep -c . /ledger/failed || true')
  UPGRADE_FAILED=$((UPGRADE_FAILED + f))
  echo "- upgrade of $n: $((SECONDS - T0)) s from cordon to its writer running again; $f failed close(s) on $other meanwhile" | tee -a "$SUMMARY"
done
sleep 5
writers_run off
assert_eq 0 "$UPGRADE_FAILED" "no close failed on the other node during the upgrade"
check_acked "after the upgrade"

step "(h) ReadWriteOnce stays on one node (Multi-Attach)"
kubectl apply -f "$(dirname "$0")/k8s-csi/multinode-apps/rwo.yaml"
# Whichever pod's node gets the volume first runs; the other is refused.
wait_for 300 sh -c "kubectl -n $APPS get pod rwo-first rwo-second -o jsonpath='{.items[*].status.phase}' | grep -q Running" ||
  fail "neither RWO pod ever ran"
RUNNING=$(ka get pod rwo-first rwo-second -o jsonpath='{range .items[?(@.status.phase=="Running")]}{.metadata.name}{"\n"}{end}')
assert_eq 1 "$(printf '%s\n' "$RUNNING" | grep -c .)" "exactly one RWO pod runs"
if [ "$RUNNING" = rwo-first ]; then REFUSED=rwo-second; else REFUSED=rwo-first; fi
wait_for 180 sh -c "kubectl -n $APPS get events --field-selector involvedObject.name=$REFUSED -o jsonpath='{.items[*].message}' | grep -qi 'multi-attach'" ||
  fail "$REFUSED was not refused with a Multi-Attach error"
pass "the RWO claim's second node is refused (Multi-Attach)"
ka delete pod rwo-first rwo-second --wait --timeout=120s >/dev/null

step "(i) teardown"
ka exec "$(writer_on "$W1")" -- sh -c "printf final > /data/final.txt"
save_ledgers
ka delete deployment writer --wait --timeout=180s
wait_for 180 sh -c "! kubectl get volumeattachments -o jsonpath='{.items[*].spec.source.persistentVolumeName}' | grep -q $PV" ||
  fail "the RWX volume is still attached"
pass "unpublished and unstaged on both nodes"
wait_for 60 sh -c "[ \"\$(git --git-dir '$REMOTE' show '$BRANCH:final.txt' 2>/dev/null)\" = final ]" ||
  fail "the final commit never reached the remote"
pass "the final commit reached the remote"
ka delete pvc shared-rwx only-rwo --wait --timeout=180s
wait_for 180 sh -c "! docker exec $W1 test -e $BASE/$PV" || fail "the volume directory was not removed"
pass "DeleteVolume removed $BASE/$PV"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$SUMMARY" >>"$GITHUB_STEP_SUMMARY"; fi
step "e2e-csi-multinode passed"
