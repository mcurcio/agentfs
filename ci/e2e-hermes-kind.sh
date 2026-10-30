#!/usr/bin/env bash
# End-to-end tests of upstream Hermes (NousResearch/hermes-agent, design
# §16) on agentfs in a kind cluster, one configuration per run, with the
# checks of the Compose suite (ci/hermes/lib.sh): Hermes' root start-up
# on the mount (1), a skill Hermes writes with its own tool recorded and
# pushed (2), secrets, sessions, logs and databases never recorded (3), a
# queued patch landing at the next start with nothing attached and
# reaching Hermes' next model request (4), and a tool container's edit
# through Hermes' terminal tool recorded and pushed (5). The model is the
# scripted stand-in (ci/hermes/model); tool containers run on a
# docker-in-docker sidecar in Hermes' pod.
#
#   sidecar   the pod of docs/deployment.md §10 (ci/hermes/k8s/sidecar):
#             Hermes and dind with the mount at /opt/data through subPath
#             and the liveness probe; first Hermes on the plain volume
#             (ci/hermes/k8s/before), then the migration Job of §10 as
#             written there (its initial import recorded and pushed,
#             excluded files left out), then 1-5; then a clean exit of
#             only the sidecar under running Hermes: Hermes' liveness
#             probe restarts it onto the new mount and it records again,
#             and a queued patch is not applied (PatchQueueDeferred); it
#             lands at the next pod restart (4), over the mark the old
#             pod's stop left while the kubelet's subPath binds held the
#             mount (design §12 "Only with nothing attached").
#   fence     the read-only fence alternative of §10 (ci/hermes/k8s/fence):
#             the emptyDir read-only at /agentfs and
#             HERMES_HOME=/agentfs/workspace, on a fresh volume; 1-5.
#   csi       Hermes and dind in one pod on an agentfs claim of the CSI
#             driver (ci/hermes/k8s/csi-driver, ci/hermes/k8s/csi); 1-5,
#             the patch landing at the next NodeStage.
#
#   ci/e2e-hermes-kind.sh <sidecar|fence|csi>
#   ci/e2e-hermes-kind.sh gateway             the kind network's gateway
#   ci/e2e-hermes-kind.sh diagnose <mode>     after a failure
#
# Needs docker, kind and kubectl (ci/install-kind.sh), a kind cluster
# $KIND_CLUSTER (default agentfs-hermes) with the image agentfs:ci loaded,
# the image in the runner's docker too (`agentfs patch submit` runs from
# it), and the git server from ci/git-http-server.sh started with
# GIT_SERVER_EXTRA_LISTEN set to the gateway, its output in the
# environment (as ci/e2e-kind.sh). The FUSE module must be loaded.

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/hermes/lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-hermes}
NODE=$KIND_CLUSTER-control-plane
K8S=ci/hermes/k8s
LOGS=${E2E_HERMES_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-hermes-kind-logs}
mkdir -p "$LOGS"

gateway() {
  docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' |
    tr ' ' '\n' | grep -E '^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$' | head -n1
}
render() { kubectl kustomize --load-restrictor LoadRestrictionsNone "$1"; }
apply_dir() { render "$1" | kubectl apply -f -; }

MODE=${1:-}
case "$MODE" in
gateway) gateway; exit 0 ;;
diagnose) MODE=${2:?diagnose <mode>} ;;
sidecar | fence | csi) ;;
*) echo "usage: $0 <sidecar|fence|csi> | gateway | diagnose <mode>" >&2; exit 2 ;;
esac

case "$MODE" in
csi)
  NS=hermes-csi
  APP=app.kubernetes.io/name=hermes
  DEPLOYMENT=hermes
  DNS=agentfs-csi
  BASE=/var/lib/agentfs-csi/volumes
  PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
  PASSWORD=ci-csi-password
  HERMES_DIR=/opt/data
  ;;
*)
  NS=agentfs-e2e
  APP=app.kubernetes.io/name=agentfs-example
  DEPLOYMENT=agentfs-example
  VOLUME=hermes
  BRANCH=agentfs/$VOLUME
  BACKING=/data/backing
  PASSWORD=ci-kind-password
  HERMES_DIR=/opt/data
  [ "$MODE" = fence ] && HERMES_DIR=/agentfs/workspace
  ;;
esac

k() { kubectl -n "$NS" "$@"; }
kd() { kubectl -n "$DNS" "$@"; }
plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }

# The hooks of ci/hermes/lib.sh. POD is the pod under test.
hermes_exec() { k exec "$POD" -c hermes -- "$@"; }
hermes_logs() { k logs "$POD" -c hermes; }
dind_exec() { k exec "$POD" -c dind -- "$@"; }
model_exec() { k exec deploy/model -- "$@"; }
if [ "$MODE" = csi ]; then
  backing_exec() { in_plugin "$@"; }
  history() { in_plugin git --git-dir "$STATE/git" "$@"; }
  agentfs_status() { in_plugin agentfs --state "$STATE" status; }
else
  backing_exec() { k exec "$POD" -c agentfs -- "$@"; }
  history() { k exec "$POD" -c agentfs -- git --git-dir /data/state/git "$@"; }
  agentfs_status() { k exec "$POD" -c agentfs -- agentfs status; }
fi
run_producer() { # marker
  docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -e MARKER="$1" -e VOLUME="$VOLUME" -e ORIGIN="$REMOTE" \
    -v "$GIT_SERVER_REPOS:$GIT_SERVER_REPOS" \
    --entrypoint sh agentfs:ci -ec "$PRODUCER_SUBMIT"
}

if [ "${1:-}" = diagnose ]; then
  set +e
  step "diagnose ($MODE)"
  kubectl get nodes -o wide
  kubectl get pods -A -o wide
  k get pvc,pods,jobs -o wide
  k describe pods
  k get events --sort-by=.lastTimestamp | tail -n 80
  for pod in $(k get pods -o name); do
    for c in agentfs-init agentfs hermes-config dind hermes model migrate; do
      k logs "$pod" -c "$c" --tail=120 2>/dev/null | sed "s|^|$pod/$c: |"
      k logs "$pod" -c "$c" --previous --tail=60 2>/dev/null | sed "s|^|$pod/$c (previous): |"
    done
  done
  if [ "$MODE" = csi ]; then
    kd describe pods
    for pod in $(kd get pods -o name); do
      kd logs "$pod" -c agentfs-csi --tail=200 2>/dev/null | sed "s|^|$pod/agentfs-csi: |"
    done
  fi
  POD=$(k get pod -l "$APP" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
  if [ -n "$POD" ]; then
    step "diagnose: Hermes' own logs"
    in_hermes "grep -E ' $HERMES_DIR ' /proc/self/mountinfo; ls -la $HERMES_DIR; tail -n 60 $HERMES_DIR/logs/agent.log $HERMES_DIR/logs/errors.log"
    step "diagnose: tool containers"
    dind_exec docker ps -a
  fi
  step "diagnose: model requests (last message of each)"
  model_requests | python3 -c '
import json, sys
for r in json.load(sys.stdin):
    m = (r["body"].get("messages") or [{}])[-1]
    print(r["path"], m.get("role"), str(m.get("content"))[:300])'
  echo "---- saved logs"
  tail -n 80 "$LOGS"/*.log 2>/dev/null
  echo "---- node mounts"
  docker exec "$NODE" sh -c "grep -E 'agentfs|fuse' /proc/self/mountinfo"
  echo "---- git server"
  tail -n 40 "${GIT_SERVER_DIR:-/nonexistent}"/logs/*.log 2>/dev/null
  exit 0
fi

: "${GIT_SERVER_REPOS:?start ci/git-http-server.sh first}" "${GIT_SERVER_HTPASSWD:?}"
REMOTE=$GIT_SERVER_REPOS/history.git
rgit() { git --git-dir "$REMOTE" "$@"; }

set_password() { # user password
  local others
  others=$(grep -v "^$1:" "$GIT_SERVER_HTPASSWD" || true)
  { [ -n "$others" ] && echo "$others"; echo "$1:$(openssl passwd -apr1 "$2")"; } \
    >"$GIT_SERVER_HTPASSWD"
}

# The runner's git server as the Service git-server of <namespace>.
git_server_endpoint() { # namespace
  kubectl apply -f - <<EOF
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata:
  name: git-server-runner
  namespace: $1
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
}

# The pod of <label> that is not being deleted.
current_pod() { # label
  k get pod -l "$1" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    head -n1
}
# Waits for a pod of <label> (default: the one under test) to be Ready;
# sets POD.
wait_ready() { # [label]
  local deadline=$((SECONDS + 600)) label=${1:-$APP}
  POD=""
  until [ -n "$POD" ]; do
    [ $SECONDS -lt $deadline ] || fail "no pod of $label appeared"
    sleep 1
    POD=$(current_pod "$label")
  done
  k wait --for=condition=Ready "pod/$POD" --timeout=600s >/dev/null ||
    fail "pod $POD never became Ready"
  pass "pod $POD is Ready"
}
# Waits until no pod of <label> is left.
wait_gone() { # label
  local deadline=$((SECONDS + 180))
  while [ -n "$(k get pod -l "$1" -o name)" ]; do
    [ $SECONDS -lt $deadline ] || fail "the pods of $1 were not deleted"
    sleep 1
  done
}
restarts() { # init|containers name
  k get pod "$POD" -o jsonpath="{.status.${1}Statuses[?(@.name==\"$2\")].restartCount}"
}

pins_agree() {
  local digests digest
  mapfile -t digests < <(grep -E '^ *image: \$\{(HERMES|DIND)_IMAGE' ci/hermes/compose.yaml | grep -oE 'sha256:[0-9a-f]{64}')
  [ "${#digests[@]}" = 2 ] || fail "ci/hermes/compose.yaml should pin Hermes and dind: ${digests[*]}"
  for digest in "${digests[@]}"; do
    grep -q "digest: $digest" "$K8S/common/kustomization.yaml" ||
      fail "ci/hermes/compose.yaml pins $digest, $K8S/common does not"
  done
  pass "the kind scenarios pin the images the Compose suite does"
}

step "git server and cluster"
pins_agree
rm -rf "$REMOTE"
git init -q --bare "$REMOTE"
set_password agentfs "$PASSWORD"
GW=$(gateway)
[ -n "$GW" ] || fail "no IPv4 gateway on the kind network"
kubectl get nodes -o wide

if [ "$MODE" = csi ]; then
  step "the CSI driver with a hermes profile (deploy/examples/hermes rules)"
  kubectl apply -f deploy/csi/namespace.yaml
  git_server_endpoint "$DNS"
  apply_dir "$K8S/csi-driver"
  deadline=$((SECONDS + 300))
  until [ -n "$(plugin_pod)" ]; do
    [ $SECONDS -lt $deadline ] || fail "no node plugin pod appeared"
    sleep 1
  done
  kd wait --for=condition=Ready pod -l "$PLUGIN_LABEL" --timeout=300s >/dev/null ||
    fail "the node plugin never became Ready"
  pass "the node plugin is Ready"

  step "Hermes and docker-in-docker in one pod on an agentfs claim"
  apply_dir "$K8S/csi"
  wait_ready
  PV=$(k get pvc hermes -o jsonpath='{.spec.volumeName}')
  [ -n "$PV" ] || fail "the claim is not bound"
  VOLUME=$NS.hermes.$PV
  BRANCH=agentfs/$VOLUME
  STATE=$BASE/$PV/state
  BACKING=$BASE/$PV/backing
  SOCK=/var/lib/agentfs-csi/node/run/$PV/history.sock
  echo "volume $VOLUME"
  hermes_check_startup
else
  step "deploy ci/hermes/k8s/$MODE"
  render "$K8S/$MODE" >"$LOGS/$MODE.yaml"
  grep -q 'name: agentfs-example' "$LOGS/$MODE.yaml" || fail "the overlay renders no Deployment"
  kubectl apply -f ci/k8s/namespace.yaml
  git_server_endpoint "$NS"
  apply_dir "$K8S/$MODE"
  k rollout status deployment/model --timeout=600s
fi

if [ "$MODE" = sidecar ]; then
  step "2a. Hermes before agentfs: its files at the root of the volume"
  apply_dir "$K8S/before"
  wait_ready app.kubernetes.io/name=hermes-before
  hermes_ask before-skill >/dev/null
  BEFORE_SKILL=$(scenario_field before-skill 'a["operations"][0]["content"]')
  # A nested repository: a skill cloned with git (agentfs.nested = exclude).
  in_hermes "d=$HERMES_DIR/skills/agentfs-cloned && mkdir -p \$d && cd \$d && git init -q &&
    printf 'cloned\n' > SKILL.md && chown -R 10000:10000 \$d"
  # Names xargs would split or read as quotes: a space, ' and ".
  in_hermes "d=$HERMES_DIR/skills/agentfs-names && mkdir -p \$d && cd \$d &&
    printf 'space\n' > 'a space.md' && printf 'single\n' > \"it's.md\" &&
    printf 'double\n' > 'say \"hi\".md' && chown -R 10000:10000 \$d"
  for path in SOUL.md config.yaml .env state.db response_store.db logs/agent.log \
    skills/agentfs-before/SKILL.md skills/agentfs-cloned/SKILL.md; do
    in_hermes "test -e $HERMES_DIR/$path" || fail "Hermes left no $path before agentfs"
  done
  pass "Hermes' files are at the root of the volume"
  k scale deployment/hermes-before --replicas=0
  wait_gone app.kubernetes.io/name=hermes-before
  pass "Hermes is scaled to zero"

  step "2a. the one-time migration Job of docs/deployment.md §10, as written"
  CONFIG=$(k get deployment agentfs-example -o jsonpath='{.spec.template.spec.volumes[?(@.name=="config")].configMap.name}')
  script=$(runbook_block ../deployment.md hermes-migration)
  [ -n "$script" ] || fail "no hermes-migration block in docs/deployment.md"
  k create configmap hermes-migration --from-literal=migrate.sh="$script"
  k apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: hermes-migration
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      automountServiceAccountToken: false
      containers:
        - name: migrate
          image: agentfs:ci
          command: [sh, /migration/migrate.sh]
          securityContext: { runAsUser: 0, runAsGroup: 0 }
          volumeMounts:
            - { name: data, mountPath: /data }
            - { name: config, mountPath: /etc/agentfs, readOnly: true }
            - { name: credentials, mountPath: /run/secrets/agentfs, readOnly: true }
            - { name: migration, mountPath: /migration, readOnly: true }
      volumes:
        - { name: data, persistentVolumeClaim: { claimName: agentfs-data } }
        - { name: config, configMap: { name: $CONFIG } }
        - { name: credentials, secret: { secretName: agentfs-credentials, defaultMode: 0400 } }
        - { name: migration, configMap: { name: hermes-migration } }
EOF
  if ! k wait --for=condition=complete job/hermes-migration --timeout=300s; then
    k logs job/hermes-migration || true
    fail "the migration Job did not complete"
  fi
  k logs job/hermes-migration >"$LOGS/migration.log" 2>&1
  migration=$(cat "$LOGS/migration.log")
  grep -E '^(recorded|skipped) ' "$LOGS/migration.log" | grep -vE '^recorded skills/[^/]+/' || true
  assert_contains "$migration" "recorded SOUL.md as seq" "the initial import recorded SOUL.md"
  assert_contains "$migration" "recorded skills/agentfs-before/SKILL.md as seq" "and the skill Hermes wrote before agentfs"
  assert_contains "$migration" "skipped .env: " "it reports .env as skipped"
  assert_contains "$migration" "skipped state.db: " "it reports state.db as skipped"
  assert_eq "$BEFORE_SKILL" "$(rgit show "$BRANCH:skills/agentfs-before/SKILL.md")" "the import is pushed: the remote has the skill"
  for path in SOUL.md config.yaml; do
    rgit cat-file -e "$BRANCH:$path" || fail "the remote lacks $path"
  done
  pass "the remote has SOUL.md and config.yaml"
  for name in 'space:a space.md' "single:it's.md" 'double:say "hi".md'; do
    assert_eq "${name%%:*}" "$(rgit show "$BRANCH:skills/agentfs-names/${name#*:}")" \
      "the import recorded and pushed skills/agentfs-names/${name#*:}"
  done
  never_recorded rgit \
    '(^|/)(\.env|auth\.json)$|\.(db|db-wal|db-shm|db-journal)$|(^|/)(sessions|logs)/|^skills/agentfs-cloned/' \
    "a secret, database, log or nested repository of the existing data"

  step "the Hermes pod of docs/deployment.md §10"
  k scale deployment/agentfs-example --replicas=1
  wait_ready
  assert_contains "$(k logs "$POD" -c agentfs-init)" "volume hermes already initialised" "agentfs-init accepted the migrated volume"
  backing_exec test -e "$BACKING/skills/agentfs-cloned/SKILL.md" || fail "the nested repository's files left the tree"
  pass "excluded files stay in the tree"
  hermes_check_startup migrated
elif [ "$MODE" = fence ]; then
  wait_ready
  hermes_check_startup
  # The image's entrypoint sets HOME=/opt/data whatever HERMES_HOME is.
  echo "the image's /opt/data: $(in_hermes 'ls -A /opt/data 2>&1' | tr '\n' ' ')"
  for path in config.yaml SOUL.md .env state.db skills; do
    if in_hermes "test -e /opt/data/$path"; then fail "Hermes wrote $path to /opt/data, not its home"; fi
  done
  pass "Hermes keeps its home on the mount, none of it in /opt/data"
fi

step "2. a skill Hermes creates with its own skill tool is recorded and pushed"
hermes_check_skill create-skill
hermes_check_tool_edit
hermes_check_exclusions

marker="Patched by agentfs CI $(date +%s%N)"
step "4. queue a patch to SOUL.md"
hermes_queue_patch "$marker" 1

if [ "$MODE" = sidecar ]; then
  step "a clean exit of only the sidecar under running Hermes"
  sidecar_restarts=$(restarts initContainer agentfs)
  hermes_restarts=$(restarts container hermes)
  k exec "$POD" -c agentfs -- sh -c 'kill -TERM 1'
  deadline=$((SECONDS + 300))
  until [ "$(restarts initContainer agentfs)" -gt "$sidecar_restarts" ]; do
    [ $SECONDS -lt $deadline ] || fail "the kubelet never restarted the sidecar"
    sleep 1
  done
  pass "the kubelet restarted the sidecar"
  # Hermes' subPath is the dead mount until its liveness probe restarts it.
  deadline=$((SECONDS + 300))
  until [ "$(restarts container hermes)" -gt "$hermes_restarts" ]; do
    [ $SECONDS -lt $deadline ] || fail "Hermes' liveness probe never restarted it onto the new mount"
    sleep 2
  done
  k wait --for=condition=Ready "pod/$POD" --timeout=600s >/dev/null || fail "the pod never became Ready again"
  pass "Hermes' liveness probe restarted it onto the new mount, and the pod is Ready"
  k logs "$POD" -c agentfs --previous >"$LOGS/sidecar-stopped.log" 2>&1 || true
  k logs "$POD" -c agentfs >"$LOGS/sidecar-restarted.log" 2>&1
  grep -E 'signal|unmount|attached|elsewhere|final' "$LOGS/sidecar-stopped.log" || true
  grep -E '^(mounted|stacked|patches|alarm)' "$LOGS/sidecar-restarted.log" || true
  assert_contains "$(cat "$LOGS/sidecar-restarted.log")" "patches: not applied at this start" "the restarted sidecar did not apply the queue"
  assert_contains "$(cat "$LOGS/sidecar-restarted.log")" "alarm: patch queue deferred (" "it raised PatchQueueDeferred"
  assert_contains "$(agentfs_status)" "1 queued" "the patch stays queued"
  case "$(in_hermes "cat $HERMES_DIR/SOUL.md")" in
  *"$marker"*) fail "the patch landed under running Hermes" ;;
  esac
  pass "Hermes reads the SOUL.md it had"
  step "Hermes records through the new mount"
  hermes_check_skill recovered-skill

  step "4. the queued patch lands at the next pod restart"
  OLD=$POD
  k logs -f "$POD" -c agentfs >"$LOGS/sidecar-pod-stop.log" 2>&1 &
  follower=$!
  k delete pod "$POD" --wait --timeout=180s
  for _ in $(seq 30); do kill -0 "$follower" 2>/dev/null || break; sleep 1; done
  kill "$follower" 2>/dev/null || true
  grep -E 'signal|unmount|attached|elsewhere|final' "$LOGS/sidecar-pod-stop.log" || true
  wait_ready
  [ "$POD" != "$OLD" ] || fail "the pod was not replaced"
  k logs "$POD" -c agentfs >"$LOGS/sidecar-next-pod.log" 2>&1
  grep -E '^(mounted|patches|alarm)' "$LOGS/sidecar-next-pod.log" || true
  if grep -q 'patches: not applied at this start' "$LOGS/sidecar-next-pod.log"; then
    fail "the pod restart did not land the queue: the new pod deferred it ($(grep -m1 'patches: not applied' "$LOGS/sidecar-next-pod.log"))"
  fi
elif [ "$MODE" = fence ]; then
  step "4. the queued patch lands at the next pod restart"
  OLD=$POD
  k delete pod "$POD" --wait --timeout=180s
  wait_ready
  [ "$POD" != "$OLD" ] || fail "the pod was not replaced"
else
  step "4. the queued patch lands at the next NodeStage"
  k scale "deployment/$DEPLOYMENT" --replicas=0
  wait_gone "$APP"
  deadline=$((SECONDS + 180))
  while docker exec "$NODE" test -e "$SOCK"; do
    [ $SECONDS -lt $deadline ] || fail "the volume was never unstaged"
    sleep 1
  done
  pass "the volume is unstaged"
  k scale "deployment/$DEPLOYMENT" --replicas=1
  wait_ready
  kd logs "$(plugin_pod)" -c agentfs-csi | grep -E 'patch|stag' | tail -n 20 || true
fi
hermes_check_patch_landed "$marker" 1

step "e2e-hermes-kind $MODE passed"
