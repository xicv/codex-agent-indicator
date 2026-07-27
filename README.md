# Codex Agent Indicator

Use the five G-keys on a Logitech G915 keyboard as a live Codex task monitor.

![macOS](https://img.shields.io/badge/macOS-supported-black)
![Rust](https://img.shields.io/badge/built_with-Rust-orange)

## What the lights mean

| Colour | Status |
| --- | --- |
| Blue | Codex is working |
| Amber | Codex needs approval |
| Purple | Codex needs your input |
| Green | Codex finished successfully |
| Red | Codex stopped with an error |

G1 through G5 each represent one Codex task. Active keys flash brightly while
the rest of the keyboard stays on with a dim, steady background.

Green, red, purple, and amber stay visible until you press their G-key. The key
opens the matching task in Codex and then clears the acknowledged light. A blue
key stays blue because its task is still working.

## Requirements

- macOS
- Logitech G915 connected through its wired USB HID interface
- [Rust](https://rustup.rs/) 1.91 or newer
- [`jq`](https://jqlang.github.io/jq/) for safely merging Codex hooks

If `jq` is missing and you use Homebrew:

```sh
brew install jq
```

## Install

Clone or download this repository, open Terminal in its folder, and run:

```sh
./scripts/install.sh
```

The installer:

1. builds the optimized Rust binary;
2. installs it in `~/.local/bin`;
3. creates a private user configuration;
4. safely adds the required hooks to `~/.codex/hooks.json`;
5. starts a lightweight macOS LaunchAgent.

It preserves unrelated hooks already present in your Codex configuration.

After the first install, open `/hooks` in Codex and trust this command:

```text
~/.local/bin/codex-agent-indicator hook
```

You do not need to restart Codex.

## Uninstall

From the repository folder, run:

```sh
./scripts/uninstall.sh
```

This removes the daemon, binary, and only this project's hook entries. Your
custom configuration is kept. To remove that too:

```sh
./scripts/uninstall.sh --purge
```

## Use it

Press an illuminated G-key to open its matching Codex task.

Useful commands:

```sh
codex-agent-indicator status
codex-agent-indicator doctor
codex-agent-indicator reload
```

You can also preview or clear states manually:

```sh
codex-agent-indicator set done demo-task
codex-agent-indicator set approval demo-task
codex-agent-indicator clear demo-task
codex-agent-indicator clear
```

If your shell cannot find the command, add this to your shell profile:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

## Customize the lights

Edit:

```text
~/.config/codex-agent-indicator/config.toml
```

The daemon automatically reloads valid changes. The main settings are:

```toml
[lighting]
background = "#101820"
flash_enabled = true
flash_interval_ms = 500
flash_dim_percent = 5

[colors]
working = "#007aff"
approval = "#ff9500"
requested = "#af52de"
done = "#34c759"
error = "#ff3b30"
```

- Increase `flash_interval_ms` for slower flashing.
- Increase `flash_dim_percent` for a brighter dim phase.
- Set `flash_enabled = false` for steady status colours.
- Set `navigation.enabled = false` if G-keys should show status without opening
  tasks.
- Set `detect_questions = false` to treat every stopped turn as completed.

## Behaviour

- The first five active Codex tasks use G1 through G5.
- Finished and attention states are never removed by a timer.
- A newer task cannot displace an unacknowledged green, red, purple, or amber
  state.
- If all five keys need acknowledgement, open one before another task can be
  assigned.
- The oldest blue working slot may be reused when all keys are occupied.
- A task is acknowledged only after its Codex deep link opens successfully.
- Merely ending a Codex process does not clear an unacknowledged result.

## Troubleshooting

Run:

```sh
codex-agent-indicator doctor
codex-agent-indicator status
```

Common causes:

- The G915 must expose the expected wired USB HID interface.
- Logitech G HUB may overwrite the indicator colours if it is running an active
  lighting effect.
- Codex may ask you to trust the hook command after installation.
- The daemon log is stored at
  `~/Library/Logs/codex-agent-indicator.log`.

## Privacy and performance

- Everything runs locally on your Mac.
- There is no telemetry, analytics, cloud service, or network server.
- Hook messages travel through a private user-only Unix socket.
- The daemon does not read task transcripts.
- Only the tail of a final assistant message is inspected to distinguish a
  question from a completed response.
- No Accessibility permission, screen recording, browser control, MCP server,
  Codex plugin, or extra background app is required.
- Repository templates contain placeholders rather than usernames or personal
  computer paths.

## How it works

The project is one small native Rust binary. It serves as:

- the background daemon;
- the fast Codex hook forwarder;
- the G-key task switcher;
- the configuration and diagnostic command.

Hooks send a small Unix datagram and exit. The daemon batches lighting changes
into one HID++ frame and sleeps between events. G-key presses use Logitech's
HID++ `0x8010` feature. Task switching uses Codex's
`codex://threads/<thread-id>` deep link.

Normal keyboard lighting, macros, profiles, key assignments, and onboard memory
are not modified.

## Development

```sh
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

The project intentionally does not require a formatter pass for validation.

## References

- [OpenAI Codex lifecycle hooks](https://learn.chatgpt.com/docs/hooks)
- [OpenAI Codex desktop commands and deep links](https://learn.chatgpt.com/docs/reference/commands.md)
- [Logitech G915](https://www.logitechg.com/en-us/products/gaming-keyboards/g915-low-profile-wireless-mechanical-gaming-keyboard.html)
- [OpenLogi](https://github.com/AprilNEA/OpenLogi)
- [OpenRGB G915 controller](https://github.com/CalcProgrammer1/OpenRGB/tree/master/Controllers/LogitechController/LogitechG915Controller)
- [hidapi 2.6.6](https://docs.rs/hidapi/2.6.6/hidapi/)
