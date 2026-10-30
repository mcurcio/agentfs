#!/usr/bin/env bash
# End-to-end test of upstream Hermes (NousResearch/hermes-agent, design
# §16, deployment.md §10) on an agentfs volume under Docker Compose:
# examples/docker/compose.yaml + compose.remote.yaml + ci/hermes/compose.yaml.
# Hermes runs its published image as shipped, root start-up included, with
# the mount at its data path; its docker terminal backend runs tool
# containers on a docker-in-docker service that has the mount too; a
# scripted model server (ci/hermes/model) stands in for the model API, so
# every turn is deterministic and needs no key. Hermes is driven through
# its gateway's OpenAI-compatible API. The checks are ci/hermes/lib.sh's,
# shared with the kind suite (ci/e2e-hermes-kind.sh). Each fails loudly:
#
#   1. Hermes starts as upstream ships it (root init) on the mount, and
#      files keep their owners, in Hermes and in the backing tree;
#   2. a skill Hermes creates with its own skill tool is committed, as its
#      user, and pushed to the git server;
#   3. secrets, sessions, logs and databases written during the run, by
#      Hermes and by the tool container, are in the tree but never in the
#      history, locally or on the remote (run after 5, so it covers the
#      tool container's writes too);
#   4. a patch queued on the remote with `agentfs patch submit` lands when
#      the volume is restarted with nothing attached, and Hermes reads the
#      new content: its next model request carries the patched SOUL.md;
#   5. a tool container's edit, made through Hermes' terminal tool, reaches
#      the Hermes container and is committed, as root, and pushed;
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

. "$(dirname "$0")/lib.sh"
. "$(dirname "$0")/hermes/lib.sh"

EX=examples/docker
export AGENTFS_IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
export VOLUME=hermes
files=(-f "$EX/compose.yaml" -f "$EX/compose.remote.yaml" -f ci/hermes/compose.yaml)
dc() { docker compose "${files[@]}" "$@"; }
BRANCH=agentfs/$VOLUME
HERMES_DIR=/opt/data
BACKING=/data/backing

hermes_exec() { dc exec -T hermes "$@"; }
hermes_logs() { dc logs --no-color --no-log-prefix hermes; }
dind_exec() { dc exec -T dind "$@"; }
model_exec() { dc exec -T model "$@"; }
backing_exec() { dc exec -T agentfs "$@"; }
history() { dc exec -T agentfs git --git-dir /data/state/git "$@"; }
rgit() { dc exec -T -u www-data git-server git --git-dir /srv/git/history.git "$@"; }
agentfs_status() { dc exec -T agentfs agentfs status; }
run_producer() { # marker
  dc run --rm -T -e MARKER="$1" -e VOLUME="$VOLUME" producer "
    user=\$(sed -n 's/^username=//p' /run/secrets/origin)
    password=\$(sed -n 's/^password=//p' /run/secrets/origin)
    ORIGIN=http://\$user:\$password@git-server:8080/git/history.git
    $PRODUCER_SUBMIT"
}

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
  dc exec -T agentfs agentfs status
  history log --stat -5 "$BRANCH"
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

step "start agentfs, the git server, the model server, docker-in-docker and Hermes"
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

step "2. a skill Hermes creates with its own skill tool is recorded and pushed"
hermes_check_skill create-skill

hermes_check_tool_edit

hermes_check_exclusions

step "4. a queued patch lands at restart and Hermes reads it"
marker="Patched by agentfs CI $(date +%s%N)"
hermes_queue_patch "$marker" 1
# A pod restart (deployment.md §10): everything on the mount stops, then
# the recorder, which lands the queue at mount with nothing attached.
dc stop hermes dind
dc restart agentfs
wait_healthy agentfs 300
# Only what was stopped: agentfs-init refuses (exit 75) a mounted volume.
dc up -d --no-deps dind hermes
wait_healthy hermes 600
hermes_check_patch_landed "$marker" 1

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
if backing_exec test -e "$BACKING/skills/agentfs-unmounted"; then fail "the tool container's write reached the backing tree"; fi
pass "the tool container's write reached neither Hermes nor the backing tree"
# Upstream binds Hermes' skills read-only into every tool container, and
# its persistent home (container_persistent, default true) from
# $HERMES_HOME/sandboxes: both from the daemon's view of Hermes' home.
assert_contains "$result" "skills: read-only" "the tool container sees Hermes' skills read-only"
home=$(in_hermes "find $HERMES_DIR/sandboxes -name agentfs-home.txt")
[ -n "$home" ] || fail "the tool container's home file is not under Hermes' sandboxes/: $mounts"
assert_eq "Written in tool container $tool." "$(in_hermes "cat $home")" \
  "the tool container's home is in Hermes' home (${home#"$HERMES_DIR"/})"
hermes_check_skill recovered-skill
wait_pushed
for git_fn in history rgit; do
  never_recorded "$git_fn" '^skills/agentfs-unmounted/|^sandboxes/' "the tool container's writes"
done

step "clean up"
dc --profile producer down -v
pass "Hermes on agentfs passed"
