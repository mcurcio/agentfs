#!/bin/sh
# Installs the plugins this repository turns on in .claude/settings.json.
#
# A cloud session never shows the workspace trust dialog, so it ignores
# extraKnownMarketplaces and enabledPlugins in a checked-in settings file
# (https://code.claude.com/docs/en/plugins/loading.md). Without this, the
# workflow:* agents are missing. Plugins load when Claude Code starts, so an
# install takes effect in the next session, not the one running this script.
#
# It installs at user scope and never edits .claude/settings.json. (Undoing it
# with `claude plugin marketplace remove` does: it empties that file's lists.)
#
# Run from anywhere. As a SessionStart hook (--hook) it does nothing outside
# cloud sessions, and tells the session when it installed something.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
settings="$repo/.claude/settings.json"

if [ "${1:-}" = --hook ] && [ "${CLAUDE_CODE_REMOTE:-}" != true ]; then
	exit 0
fi
# A SessionStart hook gets JSON on stdin; nothing here reads it.
[ -t 0 ] || cat >/dev/null

claude=$(command -v claude || echo /opt/claude-code/bin/claude)
installed=$("$claude" plugin list 2>/dev/null || true)

# Marketplaces first: a plugin is installed by its marketplace's name.
jq -r '.extraKnownMarketplaces // {} | to_entries[]
	| select(.value.source.source == "github") | "\(.key) \(.value.source.repo)"' "$settings" |
	while read -r name repo_slug; do
		"$claude" plugin marketplace list 2>/dev/null | grep -q "> $name\$" ||
			"$claude" plugin marketplace add "$repo_slug" >&2
	done

added=
for plugin in $(jq -r '.enabledPlugins // {} | to_entries[] | select(.value == true) | .key' "$settings"); do
	if ! printf '%s\n' "$installed" | grep -q "> $plugin\$"; then
		"$claude" plugin install "$plugin" >&2
		added="$added $plugin"
	fi
done

if [ -n "$added" ] && [ "${1:-}" = --hook ]; then
	echo "Installed plugins this session could not load:$added. They load in the next session." \
		"Their agents (workflow:*) are missing from this one: tell the user before relying on them."
fi
