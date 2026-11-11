#!/bin/sh
# SessionStart hook: in a cloud session, says which plugins .claude/settings.json turns on are
# not installed. A cloud session ignores that file's plugins (it never shows the trust dialog);
# the environment's setup script installs them before Claude Code starts, as the README of
# https://github.com/mcurcio/skills says under "Cloud sessions". Installing here would be too
# late: plugins load only at start. Prints nothing when all are installed.
[ "${CLAUDE_CODE_REMOTE:-}" = true ] || exit 0
[ -t 0 ] || cat >/dev/null
settings="$(dirname "$0")/settings.json"
installed=$(claude plugin list 2>/dev/null || true)
missing=
for p in $(jq -r '.enabledPlugins // {} | to_entries[] | select(.value == true) | .key' "$settings"); do
  printf '%s\n' "$installed" | grep -q "> $p\$" || missing="$missing $p"
done
[ -z "$missing" ] || echo "Plugins .claude/settings.json turns on are not installed:$missing." \
  "This environment's setup script does not install them (see AGENTS.md), so their" \
  "skills and agents (workflow:*) are missing from this session. Tell the user before relying on them."
