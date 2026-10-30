# shellcheck shell=bash
# The checks the Hermes suites share (design §16, deployment.md §10):
# ci/e2e-hermes.sh under Docker Compose and ci/e2e-hermes-kind.sh on kind.
# Source it after ci/lib.sh. The caller defines how to reach each piece:
#
#   hermes_exec ARGS...   a command in the Hermes container
#   hermes_logs           Hermes' container log
#   dind_exec ARGS...     a command in the docker-in-docker container
#   model_exec ARGS...    a command in the model server's container
#   backing_exec ARGS...  a command where the backing tree is at $BACKING
#   history ARGS...       git on the volume's history (its state area)
#   rgit ARGS...          git on the remote the volume pushes to
#   agentfs_status        `agentfs status` of the volume
#   run_producer          runs $PRODUCER_SUBMIT with MARKER, VOLUME and
#                         ORIGIN set, where agentfs and git are
#
# and sets BRANCH, VOLUME, BACKING and HERMES_DIR (Hermes' home in its
# container). Tool containers see Hermes' home at /opt/data, as the
# scenarios' commands (ci/hermes/model/scenarios.json) expect.

HERMES_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCENARIOS=$HERMES_LIB/model/scenarios.json
HERMES_CONFIG=$HERMES_LIB/hermes/config.yaml
HERMES_API_KEY=agentfs-ci-not-a-secret

in_hermes() { hermes_exec sh -c "$1"; }

# The requests the model server received, as JSON.
model_requests() {
  model_exec python3 -c "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8000/requests').read().decode())"
}

# The messages of <role> in the turn whose user message carries <nonce>:
# the system prompt of its first request, or every tool result of its last
# (a terminal result's output, when it is JSON with one).
turn_text() { # nonce role
  model_requests | python3 -c '
import json, sys
nonce, role = sys.argv[1], sys.argv[2]

def text(content):
    try:
        value = json.loads(content)
        if isinstance(value, dict) and "output" in value:
            return str(value["output"])
    except (TypeError, ValueError):
        pass
    return str(content)

found = None
for r in json.load(sys.stdin):
    ms = r["body"].get("messages") or []
    if any(nonce in str(m.get("content")) for m in ms if m.get("role") == "user"):
        found = ms
        if role == "system":
            break
print("\n".join(text(m.get("content")) for m in (found or []) if m.get("role") == role))' "$1" "$2"
}

# A field of a scenario's first step.
scenario_field() { # scenario python-expression-on-arguments
  python3 -c 'import json, sys; a = json.load(open(sys.argv[1]))[sys.argv[2]][0]["arguments"]; print(eval(sys.argv[3]))' \
    "$SCENARIOS" "$1" "$2"
}

# Sends Hermes one user turn naming a scenario of the model server,
# through its gateway's OpenAI-compatible API; a fresh nonce makes each
# turn a new request. Prints the nonce.
hermes_ask() { # scenario
  local nonce out
  nonce="$1-$(date +%s%N)"
  out=$(hermes_exec curl -sS --max-time 300 http://127.0.0.1:8642/v1/chat/completions \
    -H "Authorization: Bearer $HERMES_API_KEY" -H 'Content-Type: application/json' \
    -d "{\"model\": \"hermes-agent\", \"messages\": [{\"role\": \"user\", \"content\": \"agentfs-scenario:$1 $nonce\"}]}") ||
    fail "the request to Hermes failed: $out"
  echo "$out" >&2
  assert_contains "$out" "agentfs-scenario $1: done" "Hermes finished the $1 turn" >&2
  echo "$nonce"
}

# Waits until git function $1 has <path> on the branch with <content>.
wait_content() { # git-fn path content [seconds]
  local deadline=$((SECONDS + ${4:-120}))
  until [ "$("$1" show "$BRANCH:$2" 2>/dev/null)" = "$3" ]; do
    [ $SECONDS -lt $deadline ] || fail "$1 never had $2 as Hermes sees it"
    sleep 1
  done
  pass "$1 has $2"
}
author() { "$1" log -1 --format=%an "$BRANCH" -- "$2"; } # git-fn path

# Fails if git function $1 ever recorded a path matching the regex <re>.
never_recorded() { # git-fn regex what
  local paths found
  paths=$("$1" log --all --format= --name-only | sort -u)
  echo "$1: $(printf '%s\n' "$paths" | grep -c .) paths ever recorded"
  if found=$(printf '%s\n' "$paths" | grep -E "$2"); then
    fail "$1 recorded $3: $found"
  fi
  pass "$1 never recorded $3"
}

# Waits until the remote's branch is the history's: everything committed
# so far is pushed.
wait_pushed() {
  local deadline=$((SECONDS + 120))
  until [ "$(rgit rev-parse "$BRANCH" 2>/dev/null)" = "$(history rev-parse "$BRANCH")" ]; do
    [ $SECONDS -lt $deadline ] || fail "the remote never caught up with the history"
    sleep 1
  done
  pass "the remote has the history's head"
}

# 1. Hermes' root start-up on the mount; files keep their owners. With
# `migrated`, SOUL.md came from the initial import, not from Hermes.
hermes_check_startup() { # [migrated]
  local soul
  step "1. Hermes starts as upstream ships it (root init) on the mount"
  assert_contains "$(in_hermes "grep ' $HERMES_DIR ' /proc/self/mountinfo")" "fuse" "Hermes' $HERMES_DIR is the agentfs FUSE mount"
  assert_contains "$(hermes_logs)" "[stage2] Setup complete" "the image's root start-up ran to the end on the mount"
  assert_eq "hermes 640" "$(in_hermes "stat -c '%U %a' $HERMES_DIR/config.yaml")" "start-up chowned and chmodded config.yaml to hermes"
  assert_eq "hermes 600" "$(in_hermes "stat -c '%U %a' $HERMES_DIR/.env")" ".env is hermes', mode 600"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/SOUL.md")" "SOUL.md is hermes'"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/skills")" "skills/ is hermes'"
  assert_eq 10000 "$(backing_exec stat -c %u "$BACKING/SOUL.md")" "the backing tree keeps SOUL.md's owner"
  soul=$(in_hermes "cat $HERMES_DIR/SOUL.md")
  wait_content history SOUL.md "$soul"
  wait_content rgit SOUL.md "$soul"
  if [ "${1:-}" != migrated ]; then
    assert_eq uid-10000 "$(author rgit SOUL.md)" "SOUL.md is authored by Hermes' user"
  fi
}

# 2. A skill Hermes creates with its own skill tool is recorded, as its
# user, and pushed.
hermes_check_skill() { # scenario
  local name content
  name=$(scenario_field "$1" 'a["operations"][0]["name"]')
  content=$(scenario_field "$1" 'a["operations"][0]["content"]')
  hermes_ask "$1" >/dev/null
  assert_eq "$content" "$(in_hermes "cat $HERMES_DIR/skills/$name/SKILL.md")" "Hermes wrote skills/$name"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/skills/$name/SKILL.md")" "the skill is Hermes' user's"
  wait_content history "skills/$name/SKILL.md" "$content"
  wait_content rgit "skills/$name/SKILL.md" "$content"
  assert_eq uid-10000 "$(author rgit "skills/$name/SKILL.md")" "the skill's commit is authored by Hermes' user"
}

# The tool container a scenario's note names ("Written in tool container
# <id>."), checked to be one on docker-in-docker, not Hermes.
tool_container() { # note
  local tool
  tool=$(printf '%s\n' "$1" | sed -n 's/^Written in tool container \(.*\)\.$/\1/p' | head -n1)
  [ -n "$tool" ] || fail "the tool container's note names no container: $1"
  [ "$tool" != "$(hermes_exec hostname)" ] || fail "the note was written in the Hermes container"
  assert_contains "$(dind_exec docker ps --no-trunc --format '{{.ID}} {{.Image}}')" "$tool" \
    "the note was written in a tool container on docker-in-docker ($tool)" >&2
  echo "$tool"
}

# Pulls the tool image ahead, so the terminal tool's first call does not
# wait on it.
pull_tool_image() {
  dind_exec docker pull -q "$(sed -n 's/^ *docker_image: *//p' "$HERMES_CONFIG")"
}

# 5. A tool container's edit, through Hermes' terminal tool, reaches
# Hermes and is recorded, as root, and pushed.
hermes_check_tool_edit() {
  local note
  step "5. a tool container's edit reaches Hermes and is recorded and pushed"
  pull_tool_image
  hermes_ask sandbox-edit >/dev/null
  note=$(in_hermes "cat $HERMES_DIR/skills/agentfs-sandbox/SKILL.md")
  echo "$note"
  tool_container "$note" >/dev/null
  assert_eq root "$(in_hermes "stat -c %U $HERMES_DIR/skills/agentfs-sandbox/SKILL.md")" "the tool container's file is root's in Hermes too"
  wait_content history skills/agentfs-sandbox/SKILL.md "$note"
  wait_content rgit skills/agentfs-sandbox/SKILL.md "$note"
  assert_eq uid-0 "$(author rgit skills/agentfs-sandbox/SKILL.md)" "the tool container's commit is authored by root"
}

# 3. Secrets, sessions, logs and databases written during the run, by
# Hermes and by the tool container, are in the tree but never in the
# history, locally or on the remote. Run after 5.
hermes_check_exclusions() {
  local path excluded
  step "3. secrets, sessions, logs and databases are never recorded"
  # Hermes keeps its sessions in state.db (upstream docker.md, v0.22.5).
  excluded=(.env state.db response_store.db logs/agent.log skills/agentfs-sandbox/auth.json skills/agentfs-sandbox/logs/run.log)
  for path in "${excluded[@]}"; do
    in_hermes "test -e $HERMES_DIR/$path" || fail "$path was never written, so this check proves nothing"
    backing_exec test -e "$BACKING/$path" || fail "$path is not in the backing tree"
  done
  pass "written during the run: ${excluded[*]}"
  wait_pushed
  for git_fn in history rgit; do
    never_recorded "$git_fn" \
      '(^|/)(\.env|auth\.json)$|\.(db|db-wal|db-shm|db-journal)$|\.sqlite|(^|/)(sessions|logs|checkpoints)/' \
      "a secret, session, log or database"
  done
}

# The producer's submit, as iac runs it: a SOUL.md with $MARKER queued
# for $VOLUME on $ORIGIN with `agentfs patch submit`.
# shellcheck disable=SC2016
PRODUCER_SUBMIT='git init -q /tmp/producer && cd /tmp/producer
git config user.name "Patch Producer" && git config user.email producer@example.com
git remote add origin "$ORIGIN"
printf "# A persona from the patch queue\n\n%s\n" "$MARKER" >/tmp/SOUL.md
agentfs patch submit --volume "$VOLUME" --put SOUL.md=/tmp/SOUL.md -m "Set the persona"'

# Queues patch <n> setting SOUL.md to carry <marker>.
hermes_queue_patch() { # marker n
  local out
  out=$(run_producer "$1" 2>&1) || fail "patch submit failed: $out"
  echo "$out"
  assert_contains "$out" "queued patch $2" "patch $2 is queued on the remote"
}

# 4. Patch <n>, with <marker>, has landed: Hermes reads it, and its next
# model request carries the patched SOUL.md.
hermes_check_patch_landed() { # marker n
  local nonce
  assert_contains "$(agentfs_status)" "last applied $2" "agentfs applied patch $2"
  assert_contains "$(in_hermes "cat $HERMES_DIR/SOUL.md")" "$1" "Hermes' container reads the patched SOUL.md"
  assert_contains "$(history log -1 --format=%B "$BRANCH" -- SOUL.md)" "Agentfs-Trigger: patch" "the patch is its own commit in the history"
  nonce=$(hermes_ask hello)
  assert_contains "$(turn_text "$nonce" system)" "$1" "Hermes' next model request carries the patched SOUL.md"
}
