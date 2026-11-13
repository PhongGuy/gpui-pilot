# shellcheck shell=sh
# Shared AI-attribution check for the global git hooks. Sourced by commit-msg
# and pre-push; like _identity-lib.sh it is never executed as a hook itself.
# Tests: ../hook-tests/attribution-test.sh

# AI agents, matched as whole words in trailers and footers. Extend as new tools appear.
ATTRIBUTION_AGENTS='claude|anthropic|openai|chatgpt|gpt-?[0-9]|codex|copilot|cursor|cursoragent|gemini|devin|aider|windsurf|codeium|cline|tabnine|amp|jules|junie|kiro|qodo'
ATTRIBUTION_AGENT_WORD="(^|[^[:alnum:]])(${ATTRIBUTION_AGENTS})([^[:alnum:]]|\$)"

# Print every line of a commit message (read from stdin) that credits an AI
# agent or bot, and return 0 when there is at least one. A person may be credited
# with "Co-authored-by: Name <email>"; any trailer naming an agent or a [bot]
# account, an AI-tool trailer, a "Generated with <agent>" footer, an agent
# noreply address or the robot emoji is rejected. Prose mentioning an agent,
# and "claude" as a commit scope, stay allowed.
find_attribution() {
	msg=$(cat)
	# Only trailer, footer and address lines can carry credit; the rest skip the per-line checks.
	hits=$(printf '%s\n' "$msg" | grep -iE '^[[:space:]]*[a-z-]+:|^[^[:alnum:]]*(generated|created|written|authored|made|built|coded) |noreply@anthropic' |
		while IFS= read -r line; do
		lower=$(printf '%s' "$line" | tr '[:upper:]' '[:lower:]')
		case $lower in
		*noreply@anthropic.com*) printf '%s\n' "$line"; continue ;;
		esac
		if printf '%s' "$lower" | grep -qE '^[[:space:]]*(claude-session|assisted-by|generated-by|ai-assisted|ai-generated):'; then
			printf '%s\n' "$line"
		elif printf '%s' "$lower" | grep -qE '^[[:space:]]*[a-z-]+-by:'; then
			# A credit trailer (Co-authored-by, Signed-off-by, ...): never an agent or a bot,
			# except Dependabot, which is the dependency bot and not an AI agent.
			if printf '%s' "$lower" | grep -qE '^[[:space:]]*[a-z-]+-by:[[:space:]]*dependabot\[bot\] <'; then
				:
			elif printf '%s' "$lower" | grep -qE "$ATTRIBUTION_AGENT_WORD|\[bot\]"; then
				printf '%s\n' "$line"
			elif printf '%s' "$lower" | grep -qE '^[[:space:]]*co-authored-by:' &&
				! printf '%s' "$lower" | grep -qE '^[[:space:]]*co-authored-by:[[:space:]]*[^<>[:space:]][^<>]*<[^<>@[:space:]]+@[^<>[:space:]]+>[[:space:]]*$'; then
				# A co-author must be a named person with an address.
				printf '%s\n' "$line"
			fi
		elif printf '%s' "$lower" | grep -qE "^[^[:alnum:]]*(generated|created|written|authored|made|built|coded) (with|by|using) " &&
			printf '%s' "$lower" | grep -qE "$ATTRIBUTION_AGENT_WORD"; then
			printf '%s\n' "$line"
		fi
	done)
	# The robot emoji is matched as a fixed byte string so it works in any locale.
	emoji=$(printf '%s\n' "$msg" | grep -F "$(printf '\360\237\244\226')")
	hits=$(printf '%s\n%s\n' "$hits" "$emoji" | sed '/^$/d')
	[ -n "$hits" ] || return 1
	printf '%s\n' "$hits" | sed 's/^/    /' >&2
	return 0
}
