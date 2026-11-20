#!/usr/bin/env bash
# End-to-end test of upstream Hermes (NousResearch/hermes-agent, deployment.md) on an agentfs volume of the
# tree engine under Docker Compose: examples/docker/compose.yaml +
# ci/hermes/compose.yaml, with the Hermes settings and rules of
# deploy/examples/hermes. Hermes runs its published image as shipped, root
# start-up included, with the mount at its data path; its docker terminal
# backend runs tool containers on a docker-in-docker service that has the
# mount too; a scripted model server (ci/hermes/model) stands in for the
# model API, so every turn is deterministic and needs no key. Hermes is
# driven through its gateway's OpenAI-compatible API. The checks are
# ci/hermes/lib.sh's, shared with the kind suite (ci/e2e-hermes-kind.sh).
# Each fails loudly:
#
#   1. Hermes starts as upstream ships it (root init) on the mount, and
#      files keep their owners, in Hermes and in the history;
#   2. a skill Hermes creates with its own skill tool is recorded as its
#      user;
#   3. secrets, sessions, logs and databases written during the run, by
#      Hermes and by the tool container, are in the tree but never in the
#      history (run after 5, so it covers the tool container's writes too);
#   4. the remote and the patch source of the Hermes settings are logged
#      as not built at every boot of agentfs, which serves anyway, and
#      Hermes' files are as it left them after agentfs restarts with
#      nothing attached;
#   5. a tool container's edit, made through Hermes' terminal tool, reaches
#      the Hermes container and is recorded as root's;
#   6. with `terminal.docker_volumes` empty (upstream's default), a tool
#      container's write to /opt/data stays in the tool container, never
#      in Hermes' files nor the history; upstream still binds Hermes'
#      skills read-only and the tool container's persistent home from
#      Hermes' home (sandboxes/, never recorded); Hermes' own edits are
#      still recorded.
#
#   ci/e2e-hermes.sh            run the checks
#   ci/e2e-hermes.sh diagnose   print Hermes' own logs, the tool containers
#                               and the model requests (after a failure)
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, and a host whose / is a shared mount (GitHub's runners).

ENGINE=tree
. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/hermes/lib.sh"

EX=examples/docker
export AGENTFS_IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
export VOLUME=hermes
files=(-f "$EX/compose.yaml" -f ci/hermes/compose.yaml)
dc() { docker compose "${files[@]}" "$@"; }
HERMES_DIR=/opt/data

hermes_exec() { dc exec -T hermes "$@"; }
hermes_logs() { dc logs --no-color --no-log-prefix hermes; }
dind_exec() { dc exec -T dind "$@"; }
model_exec() { dc exec -T model "$@"; }
history() { dc exec -T agentfs git --git-dir /data/state/history.git "$@"; }
# A restarted container keeps its log: every boot's lines.
agentfs_logs() { dc logs --no-color --no-log-prefix agentfs; }

if [ "${1:-}" = diagnose ]; then
  set +e
  step "diagnose: Hermes"
  dc ps -a
  in_hermes 'grep -E " /opt/data " /proc/self/mountinfo; ls -la /opt/data; tail -n 60 /opt/data/logs/agent.log /opt/data/logs/errors.log'
  step "diagnose: tool containers"
  dc exec -T dind docker ps -a
  step "diagnose: model requests (last message of each)"
  model_requests | python3 -c '
import json, sys
for r in json.load(sys.stdin):
    m = (r["body"].get("messages") or [{}])[-1]
    print(r["path"], m.get("role"), str(m.get("content"))[:300])'
  step "diagnose: agentfs"
  agentfs_logs | tail -n 60
  history log --stat -5 root
  exit 0
fi

# Waits until <service>'s container is healthy.
wait_healthy() { # service seconds
  local deadline=$((SECONDS + $2)) state
  while :; do
    state=$(docker inspect -f '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{end}}' "$(dc ps -aq "$1")" 2>/dev/null)
    case "$state" in
    "running healthy") break ;;
    exited* | dead* | *unhealthy) fail "$1 is $state" ;;
    esac
    [ $SECONDS -lt $deadline ] || fail "$1 never became healthy: $state"
    sleep 1
  done
  pass "$1 is healthy"
}

step "start agentfs, the model server, docker-in-docker and Hermes"
docker version --format 'docker {{.Server.Version}}'
docker compose version
# Hermes and dind bind the mount point itself, which Docker requires to
# exist when it creates them; the FUSE mount covers it before they start.
mkdir -p "$EX/mnt/workspace"
dc build --quiet
dc up -d --quiet-pull
wait_healthy agentfs 300
wait_healthy hermes 600

hermes_check_startup
hermes_check_not_built 1

step "2. a skill Hermes creates with its own skill tool is recorded"
hermes_check_skill create-skill

hermes_check_tool_edit

hermes_check_exclusions

step "4. a restart with nothing attached: not built is logged again"
soul=$(in_hermes "cat $HERMES_DIR/SOUL.md")
# A pod restart (deployment.md): everything on the mount stops, then
# agentfs, which drains, and starts again.
dc stop hermes dind
dc restart agentfs
wait_healthy agentfs 300
hermes_check_not_built 2
# Only what was stopped: agentfs-init refuses (exit 75) a mounted volume.
dc up -d --no-deps dind hermes
wait_healthy hermes 600
assert_eq "$soul" "$(in_hermes "cat $HERMES_DIR/SOUL.md")" "Hermes reads the SOUL.md it had"

step "6. tool containers without the mount (terminal.docker_volumes empty)"
# The operator's edit, through the mount: upstream's default, an empty
# list. Hermes reads its config at start.
in_hermes "sed -i -e 's|^  docker_volumes:\$|  docker_volumes: []|' -e '\\|^    - /opt/data:/opt/data\$|d' $HERMES_DIR/config.yaml"
assert_contains "$(in_hermes "grep docker_volumes $HERMES_DIR/config.yaml")" "docker_volumes: []" "docker_volumes is empty"
dc restart hermes
wait_healthy hermes 600
nonce=$(hermes_ask sandbox-unmounted)
result=$(turn_text "$nonce" tool)
echo "$result"
tool=$(tool_container "$result")
mounts=$(dind_exec docker inspect -f '{{range .Mounts}}{{.Source}}:{{.Destination}}:{{.RW}} {{end}}' "$tool")
echo "tool container $tool mounts: $mounts"
case " $mounts" in *":/opt/data:"*) fail "the tool container has /opt/data mounted: $mounts" ;; esac
pass "the tool container has nothing mounted at /opt/data"
assert_contains "$(dind_exec docker exec "$tool" cat /opt/data/skills/agentfs-unmounted/SKILL.md)" \
  "Written in tool container $tool." "the write stayed in the tool container's own /opt/data"
if in_hermes "test -e $HERMES_DIR/skills/agentfs-unmounted"; then fail "the tool container's write reached Hermes"; fi
pass "the tool container's write did not reach Hermes"
# Upstream binds Hermes' skills read-only into every tool container, and
# its persistent home (container_persistent, default true) from
# $HERMES_HOME/sandboxes: both from the daemon's view of Hermes' home.
assert_contains "$result" "skills: read-only" "the tool container sees Hermes' skills read-only"
home=$(in_hermes "find $HERMES_DIR/sandboxes -name agentfs-home.txt")
[ -n "$home" ] || fail "the tool container's home file is not under Hermes' sandboxes/: $mounts"
assert_eq "Written in tool container $tool." "$(in_hermes "cat $home")" \
  "the tool container's home is in Hermes' home (${home#"$HERMES_DIR"/})"
hermes_check_skill recovered-skill
never_recorded history '^skills/agentfs-unmounted/|^sandboxes/' "the tool container's writes"

step "clean up"
dc down -v
pass "Hermes on agentfs passed"
