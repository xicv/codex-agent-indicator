#!/bin/zsh

set -euo pipefail

readonly ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
readonly LABEL="com.codex-agent-indicator"
readonly BINARY_DIR="$HOME/.local/bin"
readonly BINARY="$BINARY_DIR/codex-agent-indicator"
readonly CONFIG_DIR="$HOME/.config/codex-agent-indicator"
readonly HOOKS_FILE="$HOME/.codex/hooks.json"
readonly LAUNCH_AGENT="$HOME/Library/LaunchAgents/$LABEL.plist"
readonly LOG_FILE="$HOME/Library/Logs/codex-agent-indicator.log"
readonly TEMP_DIR="$(mktemp -d)"

cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

for command in cargo jq; do
    if ! command -v "$command" >/dev/null 2>&1; then
        print -u2 "Missing required command: $command"
        exit 1
    fi
done

print "Building codex-agent-indicator..."
cargo build --locked --release --manifest-path "$ROOT_DIR/Cargo.toml"

mkdir -p \
    "$BINARY_DIR" \
    "$CONFIG_DIR" \
    "$HOME/.codex" \
    "$HOME/Library/LaunchAgents" \
    "$HOME/Library/Logs"

install -m 0755 "$ROOT_DIR/target/release/codex-agent-indicator" "$BINARY"

if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
    install -m 0600 "$ROOT_DIR/config.example.toml" "$CONFIG_DIR/config.toml"
fi

sed "s|__HOME__|$HOME|g" \
    "$ROOT_DIR/launchd/com.codex-agent-indicator.plist.template" \
    > "$TEMP_DIR/launch-agent.plist"
plutil -lint "$TEMP_DIR/launch-agent.plist" >/dev/null
install -m 0600 "$TEMP_DIR/launch-agent.plist" "$LAUNCH_AGENT"

sed "s|__HOME__|$HOME|g" \
    "$ROOT_DIR/integrations/codex-hooks.json" \
    > "$TEMP_DIR/indicator-hooks.json"
jq -e . "$TEMP_DIR/indicator-hooks.json" >/dev/null

if [[ -f "$HOOKS_FILE" ]]; then
    jq \
        --arg command "$BINARY hook" \
        --slurpfile indicator "$TEMP_DIR/indicator-hooks.json" \
        '
        reduce ($indicator[0].hooks | keys[]) as $event (.;
            .hooks[$event] = (
                (
                    (.hooks[$event] // [])
                    | map(
                        select(
                            (
                                any(.hooks[]?; .command == $command)
                            ) | not
                        )
                    )
                ) + $indicator[0].hooks[$event]
            )
        )
        ' \
        "$HOOKS_FILE" > "$TEMP_DIR/hooks.json"
else
    cp "$TEMP_DIR/indicator-hooks.json" "$TEMP_DIR/hooks.json"
fi

jq -e . "$TEMP_DIR/hooks.json" >/dev/null
install -m 0600 "$TEMP_DIR/hooks.json" "$HOOKS_FILE"

launchctl bootout "gui/$(id -u)/$LABEL" >/dev/null 2>&1 || true
launchctl bootstrap "gui/$(id -u)" "$LAUNCH_AGENT"
launchctl kickstart -k "gui/$(id -u)/$LABEL"

print
print "Installed successfully."
print "Binary: $BINARY"
print "Config: $CONFIG_DIR/config.toml"
print "Log:    $LOG_FILE"
print
print "Open /hooks in Codex once and trust:"
print "$BINARY hook"
