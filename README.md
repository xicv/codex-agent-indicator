# Codex Agent Indicator

Use the five G-keys on a Logitech G915 keyboard as a live Codex task monitor.

![macOS](https://img.shields.io/badge/macOS-supported-black)
![Rust](https://img.shields.io/badge/built_with-Rust-orange)
[![crates.io](https://img.shields.io/crates/v/codex-agent-indicator.svg)](https://crates.io/crates/codex-agent-indicator)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

## What the lights mean

| Colour | Status |
| --- | --- |
| 🔵 Blue | Codex is working |
| 🟠 Amber | Codex needs approval |
| 🟣 Purple | Codex needs your input |
| 🟢 Green | Codex finished successfully |
| 🔴 Red | Codex stopped with an error |

G1 through G5 each represent one Codex task. Active keys flash brightly while
the rest of the keyboard stays on with a dim, steady background.

The indicator intentionally uses only G1 through G5. It leaves F1 through F12,
M1 through M3, MR, media controls, macros, and onboard profiles untouched.

Green, red, purple, and amber stay visible until you press their G-key. The key
brings Codex to the foreground, selects the matching task in its sidebar, and
then clears the acknowledged light. A blue key stays blue because its task is
still working.

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

### Install the CLI from crates.io

Install the published command-line binary:

```sh
cargo install codex-agent-indicator --version 0.4.4 --locked
```

Cargo installs the command in `~/.cargo/bin`. This gives you the CLI, but it
does not add Codex hooks or create the macOS LaunchAgent.

After installation, update to the latest published release with:

```sh
codex-agent-indicator update
```

The updater uses Cargo to replace the binary in its current install root. This
means a complete `~/.local/bin` setup stays in `~/.local/bin` instead of gaining
a second copy in `~/.cargo/bin`. It preserves your configuration and hooks, and
restarts the keyboard indicator service when it was installed by the complete
setup below.

To reinstall a specific version instead:

```sh
cargo install codex-agent-indicator --version 0.4.4 --locked --force
```

### Complete keyboard-monitor setup

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

The complete installer builds the same version from the checked-out source and
is recommended for first-time setup because it also configures the Codex hooks
and macOS LaunchAgent.

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

Press an illuminated G-key to bring Codex forward and select its matching task
in the sidebar.

Useful commands:

```sh
codex-agent-indicator status
codex-agent-indicator doctor
codex-agent-indicator reload
codex-agent-indicator update
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
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
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
- Increase `reassert_interval_ms` if you want less frequent direct-lighting
  watchdog refreshes.
- Set `navigation.enabled = false` if G-keys should show status without opening
  tasks.
- Set `detect_questions = false` to treat every stopped turn as completed.

## Behaviour

- The first five active Codex tasks use G1 through G5.
- Parent turns and subagents are tracked separately, so a child finishing
  cannot turn the parent task green early.
- Fast hooks are reconciled against the known task's local `task_started` and
  `task_complete` journal records. A missed terminal hook therefore corrects
  itself within about 250 ms instead of leaving a stale blue key.
- Permission and user-input states take priority over unrelated subagent
  activity.
- A failed individual tool remains blue while Codex handles it; red is reserved
  for a terminal turn failure.
- Finished and attention states are never removed by a timer.
- Current task lights are restored after the daemon or Mac restarts.
- A low-rate watchdog reasserts direct lighting mode after keyboard sleep or
  another lighting app takes control.
- A newer task cannot displace an unacknowledged green, red, purple, or amber
  state.
- If all five keys need acknowledgement, open one before another task can be
  assigned.
- The oldest blue working slot may be reused when all keys are occupied.
- G-key navigation explicitly targets the Codex app, brings it to the
  foreground, and selects the task through its local deep link.
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
- A terminal `task_complete` record repairs a missed `Stop` hook. An app-level
  failure that writes neither record can still leave the last blue state; use
  `codex-agent-indicator set error TASK_ID` to correct that rare case manually.
- The daemon log is stored at
  `~/Library/Logs/codex-agent-indicator.log`.

## Privacy and performance

- Everything runs locally on your Mac.
- There is no telemetry, analytics, cloud service, or network server.
- Hook messages travel through a private user-only Unix socket.
- The daemon follows only the local journals for tasks currently assigned to
  G1 through G5. It scans at most the latest 8 MiB once after a restart, then
  checks only for appended bytes every 250 ms.
- It ignores non-lifecycle journal records and never stores journal content in
  its status file or logs.
- The daemon does not poll Codex databases or processes and does not start a
  second app-server.
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

Hooks send a small Unix datagram and exit. The daemon tracks task, turn, and
subagent identity, batches lighting changes into one HID++ frame, debounces
unchanged status-file writes, restores its last state after restarts, and sleeps
between events. A bounded local-journal adapter reconciles native turn start and
completion records when a hook is missed. A low-rate watchdog reclaims Logitech
direct-lighting mode. G-key presses use Logitech's HID++ `0x8010` feature. Task
switching uses Codex's `codex://threads/<thread-id>` deep link.

Only live RGB output and G-key notification diversion are controlled while the
daemon runs. The program does not edit G HUB macros, profiles, key assignments,
or onboard memory.

## Development

```sh
cargo check --all-targets
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Lifecycle replay tests use privacy-scrubbed, version-labelled Codex hook JSON
and the matching official input schemas. A separate pinned Codex app fixture
exercises native `task_started`/`task_complete` reconciliation, including a
missed `Stop` and a newer active turn. Together they exercise the complete
adapter → lifecycle tracker → G-key slot path:

```sh
cargo test wire::replay_tests
cargo test journal::tests
```

When the Codex hook schema changes, add a new fixture snapshot instead of
rewriting the historical one.

The project intentionally does not require a formatter pass for validation.

## License

Licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.

## References

- [OpenAI Codex lifecycle hooks](https://learn.chatgpt.com/docs/hooks)
- [OpenAI Codex desktop commands and deep links](https://learn.chatgpt.com/docs/reference/commands.md)
- [Logitech G915](https://www.logitechg.com/en-us/products/gaming-keyboards/g915-low-profile-wireless-mechanical-gaming-keyboard.html)
- [OpenLogi](https://github.com/AprilNEA/OpenLogi)
- [Workmux Codex status tracking](https://github.com/raine/workmux/blob/main/src/state/codex_status.rs)
- [LED Cube Agent Monitor](https://github.com/pirate/led-cube-agent-monitor)
- [OpenRGB G915 controller](https://github.com/CalcProgrammer1/OpenRGB/tree/master/Controllers/LogitechController/LogitechG915Controller)
- [hidapi 2.6.6](https://docs.rs/hidapi/2.6.6/hidapi/)
