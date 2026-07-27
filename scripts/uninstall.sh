#!/bin/zsh

set -euo pipefail

readonly LABEL="com.codex-agent-indicator"
readonly BINARY="$HOME/.local/bin/codex-agent-indicator"
readonly CONFIG_DIR="$HOME/.config/codex-agent-indicator"
readonly HOOKS_FILE="$HOME/.codex/hooks.json"
readonly LAUNCH_AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"
readonly LOG_FILE="$HOME/Library/Logs/codex-agent-indicator.log"
readonly RUNTIME_DIR="$HOME/.cache/codex-agent-indicator"
readonly TEMP_DIR="$(mktemp -d)"

cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

if [[ "${1:-}" != "" && "${1:-}" != "--purge" ]]; then
    print -u2 "Usage: $0 [--purge]"
    exit 1
fi

launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true

if [[ -f "$HOOKS_FILE" ]]; then
    if ! command -v jq >/dev/null 2>&1; then
        print -u2 "jq is required to remove the hook safely."
        exit 1
    fi

    jq \
        --arg command "$BINARY hook" \
        '
        .hooks = (
            .hooks
            | with_entries(
                .value |= map(
                    select(
                        (
                            any(.hooks[]?; .command == $command)
                        ) | not
                    )
                )
            )
            | with_entries(select(.value | length > 0))
        )
        ' \
        "$HOOKS_FILE" > "$TEMP_DIR/hooks.json"
    jq -e . "$TEMP_DIR/hooks.json" >/dev/null
    install -m 0600 "$TEMP_DIR/hooks.json" "$HOOKS_FILE"
fi

rm -f "$LAUNCH_AGENT" "$BINARY" "$LOG_FILE"
rm -rf "$RUNTIME_DIR"

if [[ "${1:-}" == "--purge" ]]; then
    rm -rf "$CONFIG_DIR"
fi

print "Uninstalled codex-agent-indicator."
