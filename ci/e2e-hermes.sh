#!/usr/bin/env bash
# End-to-end test of upstream Hermes (NousResearch/hermes-agent, design
# §16, deployment.md §10) on an agentfs volume under Docker Compose:
# examples/docker/compose.yaml + compose.remote.yaml + ci/hermes/compose.yaml.
# Hermes runs its published image as shipped, root start-up included, with
# the mount at its data path; its docker terminal backend runs tool
# containers on a docker-in-docker service that has the mount too; a
# scripted model server (ci/hermes/model) stands in for the model API, so
# every turn is deterministic and needs no key. Hermes is driven through
# its gateway's OpenAI-compatible API. Each check fails loudly:
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
#      the Hermes container and is committed, as root, and pushed.
#
#   ci/e2e-hermes.sh            run the checks
#   ci/e2e-hermes.sh diagnose   print Hermes' own logs, the tool containers
#                               and the model requests (after a failure)
#
# Needs the agentfs image as $AGENTFS_IMAGE (default agentfs:ci), the fuse
# module loaded, and a host whose / is a shared mount (GitHub's runners).

. "$(dirname "$0")/lib.sh"

EX=examples/docker
export AGENTFS_IMAGE=${AGENTFS_IMAGE:-agentfs:ci}
export VOLUME=hermes
files=(-f "$EX/compose.yaml" -f "$EX/compose.remote.yaml" -f ci/hermes/compose.yaml)
dc() { docker compose "${files[@]}" "$@"; }
BRANCH=agentfs/$VOLUME
SCENARIOS=ci/hermes/model/scenarios.json
API_KEY=agentfs-ci-not-a-secret

in_hermes() { dc exec -T hermes sh -c "$1"; }
history() { dc exec -T agentfs git --git-dir /data/state/git "$@"; }
rgit() { dc exec -T -u www-data git-server git --git-dir /srv/git/history.git "$@"; }

# The requests the model server received, as JSON.
model_requests() {
  dc exec -T model python3 -c "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8000/requests').read().decode())"
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

# Waits until history function $1 has <path> on the branch with <content>.
wait_content() { # git-fn path content [seconds]
  local deadline=$((SECONDS + ${4:-120}))
  until [ "$("$1" show "$BRANCH:$2" 2>/dev/null)" = "$3" ]; do
    [ $SECONDS -lt $deadline ] || fail "$1 never had $2 as Hermes sees it"
    sleep 1
  done
  pass "$1 has $2"
}
author() { "$1" log -1 --format=%an "$BRANCH" -- "$2"; } # git-fn path

# Sends Hermes one user turn naming a scenario of the model server; a
# fresh nonce makes each turn a new request to the gateway. Prints the
# nonce.
ask() { # scenario
  local nonce out
  nonce="$1-$(date +%s%N)"
  out=$(dc exec -T hermes curl -sS --max-time 300 http://127.0.0.1:8642/v1/chat/completions \
    -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
    -d "{\"model\": \"hermes-agent\", \"messages\": [{\"role\": \"user\", \"content\": \"agentfs-scenario:$1 $nonce\"}]}") ||
    fail "the request to Hermes failed: $out"
  echo "$out" >&2
  assert_contains "$out" "agentfs-scenario $1: done" "Hermes finished the $1 turn" >&2
  echo "$nonce"
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

step "1. Hermes starts as upstream ships it (root init) on the mount"
assert_contains "$(in_hermes 'grep " /opt/data " /proc/self/mountinfo')" "fuse" "Hermes' /opt/data is the agentfs FUSE mount"
logs=$(dc logs --no-color --no-log-prefix hermes)
assert_contains "$logs" "[stage2] Setup complete" "the image's root start-up ran to the end on the mount"
assert_eq "hermes 640" "$(in_hermes 'stat -c "%U %a" /opt/data/config.yaml')" "start-up chowned and chmodded config.yaml to hermes"
assert_eq "hermes 600" "$(in_hermes 'stat -c "%U %a" /opt/data/.env')" "start-up created .env as hermes, mode 600"
assert_eq hermes "$(in_hermes 'stat -c %U /opt/data/SOUL.md')" "start-up seeded SOUL.md as hermes"
assert_eq hermes "$(in_hermes 'stat -c %U /opt/data/skills')" "start-up created skills/ as hermes"
assert_eq 10000 "$(dc exec -T agentfs stat -c %u /data/backing/SOUL.md)" "the backing tree keeps SOUL.md's owner"
soul=$(in_hermes 'cat /opt/data/SOUL.md')
wait_content history SOUL.md "$soul"
wait_content rgit SOUL.md "$soul"
assert_eq uid-10000 "$(author rgit SOUL.md)" "SOUL.md is authored by Hermes' user"

step "2. a skill Hermes creates with its own skill tool is recorded and pushed"
skill=$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["create-skill"][0]["arguments"]["operations"][0]["content"])' "$SCENARIOS")
ask create-skill >/dev/null
assert_eq "$skill" "$(in_hermes 'cat /opt/data/skills/agentfs-recorded/SKILL.md')" "Hermes wrote the skill"
assert_eq hermes "$(in_hermes 'stat -c %U /opt/data/skills/agentfs-recorded/SKILL.md')" "the skill is Hermes' user's"
wait_content history skills/agentfs-recorded/SKILL.md "$skill"
wait_content rgit skills/agentfs-recorded/SKILL.md "$skill"
assert_eq uid-10000 "$(author rgit skills/agentfs-recorded/SKILL.md)" "the skill's commit is authored by Hermes' user"

step "5. a tool container's edit reaches Hermes and is recorded and pushed"
sandbox_image=$(sed -n 's/^ *docker_image: *//p' ci/hermes/hermes/config.yaml)
# Pulled ahead, so the terminal tool's first call does not wait on it.
dc exec -T dind docker pull -q "$sandbox_image"
ask sandbox-edit >/dev/null
note=$(in_hermes 'cat /opt/data/skills/agentfs-sandbox/SKILL.md')
echo "$note"
tool=$(printf '%s\n' "$note" | sed -n 's/^Written in tool container \(.*\)\.$/\1/p')
[ -n "$tool" ] || fail "the tool container's note names no container"
[ "$tool" != "$(in_hermes hostname)" ] || fail "the note was written in the Hermes container"
assert_contains "$(dc exec -T dind docker ps --no-trunc --format '{{.ID}} {{.Image}}')" "$tool" \
  "the note was written in a tool container on docker-in-docker ($tool)"
assert_eq root "$(in_hermes 'stat -c %U /opt/data/skills/agentfs-sandbox/SKILL.md')" "the tool container's file is root's in Hermes too"
wait_content history skills/agentfs-sandbox/SKILL.md "$note"
wait_content rgit skills/agentfs-sandbox/SKILL.md "$note"
assert_eq uid-0 "$(author rgit skills/agentfs-sandbox/SKILL.md)" "the tool container's commit is authored by root"

step "3. secrets, sessions, logs and databases are never recorded"
# Hermes keeps its sessions in state.db (upstream docker.md); its
# sessions/ directory stays empty in this version.
excluded=(.env state.db response_store.db logs/agent.log skills/agentfs-sandbox/auth.json skills/agentfs-sandbox/logs/run.log)
for path in "${excluded[@]}"; do
  in_hermes "test -e /opt/data/$path" || fail "$path was never written, so this check proves nothing"
  dc exec -T agentfs test -e "/data/backing/$path" || fail "$path is not in the backing tree"
done
pass "written during the run: ${excluded[*]}"
# Everything Hermes and the tool container wrote so far is committed and
# pushed once the remote's branch is the history's.
deadline=$((SECONDS + 120))
until [ "$(rgit rev-parse "$BRANCH" 2>/dev/null)" = "$(history rev-parse "$BRANCH")" ]; do
  [ $SECONDS -lt $deadline ] || fail "the remote never caught up with the history"
  sleep 1
done
pass "the remote has the history's head"
unwanted='(^|/)(\.env|auth\.json)$|\.(db|db-wal|db-shm|db-journal)$|\.sqlite|(^|/)(sessions|logs|checkpoints)/'
for git_fn in history rgit; do
  paths=$("$git_fn" log --all --format= --name-only | sort -u)
  echo "$git_fn: $(printf '%s\n' "$paths" | grep -c .) paths ever recorded"
  if found=$(printf '%s\n' "$paths" | grep -E "$unwanted"); then
    fail "$git_fn recorded excluded paths: $found"
  fi
  pass "$git_fn never recorded a secret, session, log or database"
done

step "4. a queued patch lands at restart and Hermes reads it"
marker="Patched by agentfs CI $(date +%s%N)"
out=$(dc run --rm -T -e MARKER="$marker" producer '
  user=$(sed -n "s/^username=//p" /run/secrets/origin)
  password=$(sed -n "s/^password=//p" /run/secrets/origin)
  git init -q /tmp/producer && cd /tmp/producer
  git config user.name "Patch Producer" && git config user.email producer@example.com
  git remote add origin "http://$user:$password@git-server:8080/git/history.git"
  printf "# A persona from the patch queue\n\n%s\n" "$MARKER" >/tmp/SOUL.md
  agentfs patch submit --volume hermes --put SOUL.md=/tmp/SOUL.md -m "Set the persona"' 2>&1) ||
  fail "patch submit failed: $out"
echo "$out"
assert_contains "$out" "queued patch 1" "the patch is queued on the remote"
# A pod restart (deployment.md §10): everything on the mount stops, then
# the recorder, which lands the queue at mount with nothing attached.
dc stop hermes dind
dc restart agentfs
wait_healthy agentfs 300
# Only what was stopped: agentfs-init refuses (exit 75) a mounted volume.
dc up -d --no-deps dind hermes
wait_healthy hermes 600
assert_contains "$(dc exec -T agentfs agentfs status)" "last applied 1" "agentfs applied patch 1"
assert_contains "$(in_hermes 'cat /opt/data/SOUL.md')" "$marker" "Hermes' container reads the patched SOUL.md"
assert_contains "$(history log -1 --format=%B "$BRANCH" -- SOUL.md)" "Agentfs-Trigger: patch" "the patch is its own commit in the history"
nonce=$(ask hello)
system=$(model_requests | python3 -c '
import json, sys
for r in json.load(sys.stdin):
    ms = r["body"].get("messages") or []
    if any(sys.argv[1] in str(m.get("content")) for m in ms if m.get("role") == "user"):
        print(" ".join(str(m.get("content")) for m in ms if m.get("role") == "system"))
        break' "$nonce")
assert_contains "$system" "$marker" "Hermes' next model request carries the patched SOUL.md"

step "clean up"
dc --profile producer down -v
pass "Hermes on agentfs passed"
