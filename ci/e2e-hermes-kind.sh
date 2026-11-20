#!/usr/bin/env bash
# End-to-end tests of upstream Hermes (NousResearch/hermes-agent, design) on agentfs in a kind cluster, one configuration per run, with the
# checks of the Compose suite (ci/hermes/lib.sh): Hermes' root start-up
# on the mount (1), a skill Hermes writes with its own tool recorded (2),
# secrets, sessions, logs and databases never recorded (3), and a tool
# container's edit through Hermes' terminal tool recorded (5). The model is
# the scripted stand-in (ci/hermes/model); tool containers run on a
# docker-in-docker sidecar in Hermes' pod.
#
#   sidecar   the pod of docs/deployment.md (ci/hermes/k8s/sidecar) on the
#             tree engine: Hermes and dind with the mount at /opt/data
#             through subPath and the liveness probe; first Hermes on the
#             plain volume (ci/hermes/k8s/before), then the migration Job
#             of §10 as written there (the import records Hermes' files
#             with their owners; .env, the databases, the logs and a
#             nested repository are in the volume, not in history), then
#             1, 2, 5 and 3; the remote and the patch source logged as not
#             built (4); then a clean exit of only the sidecar under
#             running Hermes: it drains and waits for the kubelet's subPath
#             binds, the pod turns NotReady, it is killed as the kubelet
#             kills it at its grace, the restarted sidecar logs the not
#             built lines again, and Hermes' liveness probe restarts it
#             onto the new mount, where it records again.
#   fence     the read-only fence alternative of §10 (ci/hermes/k8s/fence)
#             on the tree engine: the emptyDir read-only at /agentfs and
#             HERMES_HOME=/agentfs/workspace, on a fresh volume; 1, 2, 5,
#             3, the not built lines (4), and again after a pod restart.
#   csi       Hermes and dind in one pod on an agentfs claim of the CSI
#             driver (ci/hermes/k8s/csi-driver, ci/hermes/k8s/csi), on the
#             tree engine: the volume's `agentfs host` in the node plugin;
#             1, 2, 5 and 3; the remote and the patch source logged as not
#             built (4); then a pod restart: the plugin starts a new host,
#             which logs the not built lines again, Hermes reads the
#             SOUL.md it had, and a new skill is recorded.
#
#   ci/e2e-hermes-kind.sh <sidecar|fence|csi>
#   ci/e2e-hermes-kind.sh diagnose <mode>     after a failure
#
# Needs docker, kind and kubectl (ci/install-kind.sh), a kind cluster
# $KIND_CLUSTER (default agentfs-hermes) with the image agentfs:ci loaded.
# No configuration reaches a remote: push is not built on tree volumes.
# The FUSE module must be loaded.

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/hermes/lib.sh"

KIND_CLUSTER=${KIND_CLUSTER:-agentfs-hermes}
NODE=$KIND_CLUSTER-control-plane
K8S=ci/hermes/k8s
LOGS=${E2E_HERMES_LOGS:-${RUNNER_TEMP:-/tmp}/agentfs-hermes-kind-logs}
mkdir -p "$LOGS"

render() { kubectl kustomize --load-restrictor LoadRestrictionsNone "$1"; }
apply_dir() { render "$1" | kubectl apply -f -; }

MODE=${1:-}
case "$MODE" in
diagnose) MODE=${2:?diagnose <mode>} ;;
sidecar | fence | csi) ;;
*) echo "usage: $0 <sidecar|fence|csi> | diagnose <mode>" >&2; exit 2 ;;
esac

case "$MODE" in
csi)
  NS=hermes-csi
  APP=app.kubernetes.io/name=hermes
  DNS=agentfs-csi
  BASE=/var/lib/agentfs-csi/volumes
  PLUGIN_LABEL=app.kubernetes.io/name=agentfs-csi-node
  HERMES_DIR=/opt/data
  ;;
*)
  NS=agentfs-e2e
  APP=app.kubernetes.io/name=agentfs-example
  HERMES_DIR=/opt/data
  [ "$MODE" = fence ] && HERMES_DIR=/agentfs/workspace
  ;;
esac

k() { kubectl -n "$NS" "$@"; }
kd() { kubectl -n "$DNS" "$@"; }
plugin_pod() {
  kd get pod -l "$PLUGIN_LABEL" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
}
in_plugin() { kd exec "$(plugin_pod)" -c agentfs-csi -- "$@"; }

# The hooks of ci/hermes/lib.sh. POD is the pod under test.
hermes_exec() { k exec "$POD" -c hermes -- "$@"; }
hermes_logs() { k logs "$POD" -c hermes; }
dind_exec() { k exec "$POD" -c dind -- "$@"; }
model_exec() { k exec deploy/model -- "$@"; }
if [ "$MODE" = csi ]; then
  # The volume's state area on the node, and the plugin's log, which
  # carries its hosts' output (each line `[host <pv>] ...`): its previous
  # container's, if any, then this one's.
  history() { in_plugin git --git-dir "$BASE/$PV/state/history.git" "$@"; }
  agentfs_logs() {
    kd logs "$(plugin_pod)" -c agentfs-csi --previous 2>/dev/null || true
    kd logs "$(plugin_pod)" -c agentfs-csi
  }
else
  history() { k exec "$POD" -c agentfs -- git --git-dir /data/state/history.git "$@"; }
  # The sidecar's previous boot in this pod, if any, then this one.
  agentfs_logs() {
    k logs "$POD" -c agentfs --previous 2>/dev/null || true
    k logs "$POD" -c agentfs
  }
fi

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
  [ "$MODE" != csi ] || PV=$(k get pvc hermes -o jsonpath='{.spec.volumeName}' 2>/dev/null)
  if [ -n "$POD" ]; then
    step "diagnose: the volume's history"
    history log --stat -5 root
  fi
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
  echo "---- node processes"
  docker exec "$NODE" sh -c "ps -eo pid,args | grep -E 'agentfs|tini' | grep -v grep"
  exit 0
fi

# The pod of <label> that is not being deleted.
current_pod() { # label
  k get pod -l "$1" -o go-template \
    --template '{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}' |
    sed -n 1p
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
# The sidecar's pid on the node.
sidecar_pid() {
  local cid
  cid=$(k get pod "$POD" -o jsonpath='{.status.initContainerStatuses[?(@.name=="agentfs")].containerID}')
  docker exec "$NODE" crictl inspect --output go-template --template '{{.info.pid}}' "${cid#*://}"
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

step "cluster"
pins_agree
kubectl get nodes -o wide

if [ "$MODE" = csi ]; then
  step "the CSI driver with a hermes profile (deploy/examples/hermes settings and rules)"
  kubectl apply -f deploy/csi/namespace.yaml
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
  echo "volume $PV, history $BASE/$PV/state/history.git"
  hermes_check_startup
else
  step "deploy ci/hermes/k8s/$MODE"
  render "$K8S/$MODE" >"$LOGS/$MODE.yaml"
  grep -q 'name: agentfs-example' "$LOGS/$MODE.yaml" || fail "the overlay renders no Deployment"
  kubectl apply -f ci/k8s/namespace.yaml
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
  KEPT_EXISTING=(.env state.db response_store.db logs/agent.log skills/agentfs-cloned/SKILL.md)
  for path in SOUL.md config.yaml skills/agentfs-before/SKILL.md "${KEPT_EXISTING[@]}"; do
    in_hermes "test -e $HERMES_DIR/$path" || fail "Hermes left no $path before agentfs"
  done
  pass "Hermes' files are at the root of the volume"
  k scale deployment/hermes-before --replicas=0
  wait_gone app.kubernetes.io/name=hermes-before
  pass "Hermes is scaled to zero"

  step "2a. the one-time migration Job of docs/deployment.md, as written"
  CONFIG=$(k get deployment agentfs-example -o jsonpath='{.spec.template.spec.volumes[?(@.name=="config")].configMap.name}')
  script=$(runbook_block ../deployment.md hermes-migration)
  [ -n "$script" ] || fail "no hermes-migration block in docs/deployment.md"
  k create configmap hermes-migration --from-literal=migrate.sh="$script"
  # As §10 says: the agentfs image, privileged (FUSE), as root, with the
  # Hermes ConfigMap at /etc/agentfs.
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
          securityContext: { privileged: true, runAsUser: 0, runAsGroup: 0 }
          volumeMounts:
            - { name: data, mountPath: /data }
            - { name: config, mountPath: /etc/agentfs, readOnly: true }
            - { name: migration, mountPath: /migration, readOnly: true }
      volumes:
        - { name: data, persistentVolumeClaim: { claimName: agentfs-data } }
        - { name: config, configMap: { name: $CONFIG } }
        - { name: migration, configMap: { name: hermes-migration } }
EOF
  if ! k wait --for=condition=complete job/hermes-migration --timeout=300s; then
    k logs job/hermes-migration || true
    fail "the migration Job did not complete"
  fi
  k logs job/hermes-migration >"$LOGS/migration.log" 2>&1
  migration=$(cat "$LOGS/migration.log")
  grep -vE '^tree ' "$LOGS/migration.log" || true
  assert_contains "$migration" "hermes " "init created the volume"
  assert_contains "$migration" "mounted volume hermes at /mnt/hermes" "the Job mounted the volume and copied through it"
  assert_contains "$migration" "not built: remote.origin.url (push): " "the Job's mount logged the remote as not built"

  step "the Hermes pod of docs/deployment.md"
  k scale deployment/agentfs-example --replicas=1
  wait_ready
  assert_contains "$(k logs "$POD" -c agentfs-init)" " found" "agentfs-init found the migrated volume"
  step "2a. the import recorded Hermes' files with their owners"
  wait_content history skills/agentfs-before/SKILL.md "$BEFORE_SKILL"
  assert_written_by 10000 skills/agentfs-before/SKILL.md "the skill Hermes wrote before agentfs is recorded as its user's"
  for path in SOUL.md config.yaml; do
    history cat-file -e "$(file_rev "$path")" || fail "the history lacks $path"
  done
  pass "the history has SOUL.md and config.yaml"
  never_recorded history \
    '(^|/)(\.env|auth\.json)$|\.(db|db-wal|db-shm|db-journal)$|(^|/)(sessions|logs)/|^skills/agentfs-cloned/' \
    "a secret, database, log or nested repository of the existing data"
  for path in "${KEPT_EXISTING[@]}"; do
    in_hermes "test -e $HERMES_DIR/$path" || fail "$path did not reach the volume"
  done
  pass "excluded files and the nested repository are in the volume: ${KEPT_EXISTING[*]}"
  hermes_check_startup
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

step "2. a skill Hermes creates with its own skill tool is recorded"
hermes_check_skill create-skill
hermes_check_tool_edit
hermes_check_exclusions

step "4. the remote and the patch source are logged as not built"
hermes_check_not_built 1

if [ "$MODE" = sidecar ]; then
  step "a clean exit of only the sidecar under running Hermes"
  sidecar_restarts=$(restarts initContainer agentfs)
  hermes_restarts=$(restarts container hermes)
  pid=$(sidecar_pid)
  [ -n "$pid" ] || fail "no pid for the sidecar"
  docker exec "$NODE" kill -TERM "$pid"
  # The drain unmounts the view, so the readiness probe fails; the
  # kubelet's subPath binds for Hermes and dind still hold the mount, so
  # the process waits for them (docs/deployment.md §7).
  k wait --for=condition=Ready=false "pod/$POD" --timeout=120s >/dev/null ||
    fail "the pod never turned NotReady after the sidecar's stop"
  pass "the sidecar drained and unmounted: the pod is NotReady"
  docker exec "$NODE" kill -0 "$pid" || fail "the sidecar exited while Hermes' subPath binds held the mount"
  assert_eq "$sidecar_restarts" "$(restarts initContainer agentfs)" "the sidecar is still running, held by the subPath binds"
  # As the kubelet does at the end of its grace.
  docker exec "$NODE" kill -KILL "$pid"
  deadline=$((SECONDS + 300))
  until [ "$(restarts initContainer agentfs)" -gt "$sidecar_restarts" ]; do
    [ $SECONDS -lt $deadline ] || fail "the kubelet never restarted the sidecar"
    sleep 1
  done
  pass "SIGKILL ended it, and the kubelet restarted the sidecar"
  # Hermes' subPath is the dead mount until its liveness probe restarts it.
  deadline=$((SECONDS + 300))
  until [ "$(restarts container hermes)" -gt "$hermes_restarts" ]; do
    [ $SECONDS -lt $deadline ] || fail "Hermes' liveness probe never restarted it onto the new mount"
    sleep 2
  done
  k wait --for=condition=Ready "pod/$POD" --timeout=600s >/dev/null || fail "the pod never became Ready again"
  pass "Hermes' liveness probe restarted it onto the new mount, and the pod is Ready"
  k logs "$POD" -c agentfs >"$LOGS/sidecar-restarted.log" 2>&1
  grep -E '^(mounted|stacked|not built)' "$LOGS/sidecar-restarted.log" || true
  assert_contains "$(cat "$LOGS/sidecar-restarted.log")" "mounted volume hermes at /agentfs/workspace" "the restarted sidecar mounted the volume"
  hermes_check_not_built 2
  step "Hermes records through the new mount"
  hermes_check_skill recovered-skill
else
  step "a pod restart: not built is logged again, and Hermes' files stay"
  soul=$(in_hermes "cat $HERMES_DIR/SOUL.md")
  OLD=$POD
  k delete pod "$POD" --wait --timeout=180s
  wait_ready
  [ "$POD" != "$OLD" ] || fail "the pod was not replaced"
  if [ "$MODE" = csi ]; then
    # The plugin's log holds every boot of the volume's host: the old
    # pod's went with it, and the new pod's view is a new host's.
    hermes_check_not_built 2
  else
    # A new pod's agentfs container has booted once.
    hermes_check_not_built 1
  fi
  assert_eq "$soul" "$(in_hermes "cat $HERMES_DIR/SOUL.md")" "Hermes reads the SOUL.md it had"
  hermes_check_skill recovered-skill
fi

step "e2e-hermes-kind $MODE passed"
