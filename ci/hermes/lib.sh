# shellcheck shell=bash
# The checks the Hermes suites share (deployment.md):
# ci/e2e-hermes.sh under Docker Compose and ci/e2e-hermes-kind.sh on kind,
# every configuration on the tree engine (docs/trees.md). Source it after
# ci/lib.sh. The caller defines how to reach each piece:
#
#   hermes_exec ARGS...   a command in the Hermes container
#   hermes_logs           Hermes' container log
#   dind_exec ARGS...     a command in the docker-in-docker container
#   model_exec ARGS...    a command in the model server's container
#   history ARGS...       git on the volume's history (its state area)
#   agentfs_logs          the log of every boot of the volume's agentfs
#                         process: the sidecar's container, or the CSI node
#                         plugin's container, which carries its hosts'
#
# and sets HERMES_DIR (Hermes' home in its container). Tool containers see
# Hermes' home at /opt/data, as the scenarios' commands
# (ci/hermes/model/scenarios.json) expect.

HERMES_LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
. "$HERMES_LIB/../tree-git-test-lib.sh"
SCENARIOS=$HERMES_LIB/model/scenarios.json
HERMES_CONFIG=$HERMES_LIB/hermes/config.yaml
HERMES_API_KEY=agentfs-ci-not-a-secret

in_hermes() { hermes_exec sh -c "$1"; }

# The requests the model server received, as JSON.
model_requests() {
  model_exec python3 -c "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8000/requests').read().decode())"
}

# The messages of <role> in the turn whose user message carries <nonce>:
# the system prompt of its first request, or every tool result of the
# last that has any (a terminal result's output, when it is JSON with one).
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

# Hermes sends more requests per turn than the agent loop (a title, after
# the reply): the last one with messages of the role.
found = None
for r in json.load(sys.stdin):
    ms = r["body"].get("messages") or []
    if any(nonce in str(m.get("content")) for m in ms if m.get("role") == "user"):
        if role == "system":
            found = ms
            break
        if any(m.get("role") == role for m in ms):
            found = ms
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

# Where a file's bytes are in a commit: the root's files/<aa>/<bb>/<path>
# (docs/trees.md).
file_rev() { # path
  echo "root:files/$(tree_git_fanned "$1")"
}

# History is built behind the calls: waits until git function $1 has
# <path> with <content>.
wait_content() { # git-fn path content [seconds]
  local deadline=$((SECONDS + ${4:-120}))
  until [ "$("$1" show "$(file_rev "$2")" 2>/dev/null)" = "$3" ]; do
    [ $SECONDS -lt $deadline ] || fail "$1 never had $2 as Hermes sees it"
    sleep 1
  done
  pass "$1 has $2"
}
# The uid the history records <path> with (lines/ metadata).
owner_uid() { # path
  history show "root:lines/$(tree_git_fanned "$1")" | grep -o '"uid":[0-9]*' | cut -d: -f2
}
# Who wrote <path>, as the history says: its owner's uid.
assert_written_by() { # uid path what
  assert_eq "$1" "$(owner_uid "$2")" "$3"
}

# Every path git function $1 ever recorded: the paths under
# files/<aa>/<bb>/.
recorded_paths() { # git-fn
  "$1" log --all --format= --name-only | sed -nE 's#^files/[0-9a-f]{2}/[0-9a-f]{2}/##p' | sort -u
}

# Fails if git function $1 ever recorded a path matching the regex <re>.
never_recorded() { # git-fn regex what
  local paths found
  paths=$(recorded_paths "$1")
  echo "$1: $(printf '%s\n' "$paths" | grep -c .) paths ever recorded"
  if found=$(printf '%s\n' "$paths" | grep -E "$2"); then
    fail "$1 recorded $3: $found"
  fi
  pass "$1 never recorded $3"
}

# 1. Hermes' root start-up on the mount; files keep their owners.
hermes_check_startup() {
  local soul
  step "1. Hermes starts as upstream ships it (root init) on the mount"
  assert_contains "$(in_hermes "grep ' $HERMES_DIR ' /proc/self/mountinfo")" "fuse" "Hermes' $HERMES_DIR is the agentfs FUSE mount"
  assert_contains "$(hermes_logs)" "[stage2] Setup complete" "the image's root start-up ran to the end on the mount"
  assert_eq "hermes 640" "$(in_hermes "stat -c '%U %a' $HERMES_DIR/config.yaml")" "start-up chowned and chmodded config.yaml to hermes"
  assert_eq "hermes 600" "$(in_hermes "stat -c '%U %a' $HERMES_DIR/.env")" ".env is hermes', mode 600"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/SOUL.md")" "SOUL.md is hermes'"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/skills")" "skills/ is hermes'"
  soul=$(in_hermes "cat $HERMES_DIR/SOUL.md")
  wait_content history SOUL.md "$soul"
  assert_written_by 10000 SOUL.md "history records SOUL.md as Hermes' user's"
}

# 2. A skill Hermes creates with its own skill tool is recorded, as its
# user.
hermes_check_skill() { # scenario
  local name content
  name=$(scenario_field "$1" 'a["operations"][0]["name"]')
  content=$(scenario_field "$1" 'a["operations"][0]["content"]')
  hermes_ask "$1" >/dev/null
  assert_eq "$content" "$(in_hermes "cat $HERMES_DIR/skills/$name/SKILL.md")" "Hermes wrote skills/$name"
  assert_eq hermes "$(in_hermes "stat -c %U $HERMES_DIR/skills/$name/SKILL.md")" "the skill is Hermes' user's"
  wait_content history "skills/$name/SKILL.md" "$content"
  assert_written_by 10000 "skills/$name/SKILL.md" "the skill is recorded as Hermes' user's"
}

# The tool container a scenario's note names ("Written in tool container
# <id>."), checked to be one on docker-in-docker, not Hermes.
tool_container() { # note
  local tool
  tool=$(printf '%s\n' "$1" | sed -n 's/^Written in tool container \(.*\)\.$/\1/p' | sed -n 1p)
  [ -n "$tool" ] || fail "the tool container's note names no container: $1"
  [ "$tool" != "$(hermes_exec hostname)" ] || fail "the note was written in the Hermes container"
  assert_contains "$(dind_exec docker ps --no-trunc --format '{{.ID}} {{.Image}}')" "$tool" \
    "the note was written in a tool container on docker-in-docker ($tool)" >&2
  echo "$tool"
}

# Pulls the tool image ahead, so the terminal tool's first call does not
# wait on it, through the mirror CI's daemon settings name
# (ci/docker/daemon.json, mounted into docker-in-docker).
pull_tool_image() {
  assert_contains "$(dind_exec docker info --format '{{json .RegistryConfig.Mirrors}}')" \
    "$(jq -er '."registry-mirrors"[0]' "$HERMES_LIB/../docker/daemon.json")" \
    "docker-in-docker pulls Docker Hub through the mirror"
  dind_exec docker pull -q "$(sed -n 's/^ *docker_image: *//p' "$HERMES_CONFIG")"
}

# 5. A tool container's edit, through Hermes' terminal tool, reaches
# Hermes and is recorded, as root.
hermes_check_tool_edit() {
  local note
  step "5. a tool container's edit reaches Hermes and is recorded"
  pull_tool_image
  hermes_ask sandbox-edit >/dev/null
  note=$(in_hermes "cat $HERMES_DIR/skills/agentfs-sandbox/SKILL.md")
  echo "$note"
  tool_container "$note" >/dev/null
  assert_eq root "$(in_hermes "stat -c %U $HERMES_DIR/skills/agentfs-sandbox/SKILL.md")" "the tool container's file is root's in Hermes too"
  wait_content history skills/agentfs-sandbox/SKILL.md "$note"
  assert_written_by 0 skills/agentfs-sandbox/SKILL.md "the tool container's file is recorded as root's"
}

# 3. Secrets, sessions, logs and databases written during the run, by
# Hermes and by the tool container, are in the tree but never in the
# history. Run after 5.
hermes_check_exclusions() {
  local path excluded
  step "3. secrets, sessions, logs and databases are never recorded"
  # Hermes keeps its sessions in state.db (upstream docker.md, v0.22.5).
  excluded=(.env state.db response_store.db logs/agent.log skills/agentfs-sandbox/auth.json skills/agentfs-sandbox/logs/run.log)
  for path in "${excluded[@]}"; do
    in_hermes "test -e $HERMES_DIR/$path" || fail "$path was never written, so this check proves nothing"
  done
  pass "written during the run: ${excluded[*]}"
  # Everything Hermes wrote before this is in the history: history is
  # built behind the calls, and the skill of 5 was written after them.
  wait_content history skills/agentfs-sandbox/SKILL.md "$(in_hermes "cat $HERMES_DIR/skills/agentfs-sandbox/SKILL.md")"
  never_recorded history \
    '(^|/)(\.env|auth\.json)$|\.(db|db-wal|db-shm|db-journal)$|\.sqlite|(^|/)(sessions|logs|checkpoints)/' \
    "a secret, session, log or database"
}

# On the tree engine, push and the patch queue are not built: every boot
# of the volume's agentfs process (the sidecar, or the CSI driver's host
# of the volume) logs the remote and the patch source the operator
# settings name (deploy/examples/hermes/config), one line per key, and
# serves anyway. <boots> is how many times it has started.
hermes_check_not_built() { # boots
  local logs key n
  logs=$(agentfs_logs)
  for key in 'remote.origin.url (push)' 'agentfs.patchremote (patches)'; do
    n=$(printf '%s\n' "$logs" | grep -cF "not built: $key: " || true)
    [ "$n" -ge "$1" ] || fail "not built: $key was logged $n time(s) in $1 boot(s)"
    pass "not built: $key, logged at each of $1 boot(s)"
  done
}
