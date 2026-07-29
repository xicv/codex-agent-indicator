mod config;
mod daemon;
mod device;
mod journal;
mod navigation;
mod protocol;
mod state;
mod update;
mod wire;

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use config::{AppConfig, DEFAULT_CONFIG, Paths};
use state::StateKind;
use wire::{EventMessage, HookInput};

fn main() -> ExitCode {
    let command = env::args().nth(1).unwrap_or_else(|| "help".to_string());
    match run_command(&command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("codex-agent-indicator: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_command(command: &str) -> Result<()> {
    match command {
        "daemon" => daemon::run(Paths::discover()?),
        "hook" => forward_hook(),
        "init-config" => init_config(env::args().any(|argument| argument == "--force")),
        "set" => set_state(),
        "clear" => clear_state(),
        "reload" => send_event(&Paths::discover()?, &EventMessage::Reload),
        "status" => print_status(),
        "doctor" => doctor(),
        "update" => update::run(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "--version" | "-V" | "version" => {
            println!("codex-agent-indicator {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown command {other:?}; run with --help"),
    }
}

fn forward_hook() -> Result<()> {
    let mut input = String::new();
    io::stdin()
        .take(1_048_576)
        .read_to_string(&mut input)
        .context("failed to read Codex hook input")?;
    let hook: HookInput = serde_json::from_str(&input).context("invalid Codex hook input")?;
    send_event(&Paths::discover()?, &hook.into_event())
}

fn set_state() -> Result<()> {
    let mut arguments = env::args().skip(2);
    let state = arguments
        .next()
        .context("set requires a state")?
        .parse::<StateKind>()?;
    let session_id = arguments
        .next()
        .unwrap_or_else(|| "manual-preview".to_string());
    send_event(
        &Paths::discover()?,
        &EventMessage::Set { session_id, state },
    )
}

fn clear_state() -> Result<()> {
    let session_id = env::args().nth(2);
    send_event(&Paths::discover()?, &EventMessage::Clear { session_id })
}

fn send_event(paths: &Paths, message: &EventMessage) -> Result<()> {
    let content = serde_json::to_vec(message)?;
    let socket = UnixDatagram::unbound().context("failed to create indicator event socket")?;
    socket
        .send_to(&content, &paths.socket)
        .with_context(|| {
            format!(
                "indicator daemon is not reachable at {}; start or reinstall its LaunchAgent",
                paths.socket.display()
            )
        })?;
    Ok(())
}

fn init_config(force: bool) -> Result<()> {
    let paths = Paths::discover()?;
    if paths.config.exists() && !force {
        println!(
            "configuration already exists at {}; left unchanged",
            paths.config.display()
        );
        return Ok(());
    }

    if let Some(parent) = paths.config.parent() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(&paths.config, DEFAULT_CONFIG)
        .with_context(|| format!("failed to write {}", paths.config.display()))?;
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600))?;
    println!("wrote {}", paths.config.display());
    Ok(())
}

fn print_status() -> Result<()> {
    let paths = Paths::discover()?;
    let previous_modified = modified_at(&paths.status);
    if send_event(&paths, &EventMessage::Snapshot).is_ok() {
        wait_for_status_refresh(&paths.status, previous_modified);
    }
    let content = fs::read_to_string(&paths.status).with_context(|| {
        format!(
            "no daemon status at {}; the daemon may not be running",
            paths.status.display()
        )
    })?;
    println!("{content}");
    Ok(())
}

fn wait_for_status_refresh(path: &std::path::Path, previous_modified: Option<SystemTime>) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if modified_at(path) != previous_modified {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn modified_at(path: &std::path::Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn doctor() -> Result<()> {
    let paths = Paths::discover()?;
    let config = AppConfig::load(&paths.config)?;
    config.validate()?;
    println!("Configuration: {} (valid)", paths.config.display());
    println!(
        "Daemon socket: {} ({})",
        paths.socket.display(),
        if paths.socket.exists() {
            "present"
        } else {
            "missing"
        }
    );

    let ghub_running = Command::new("/usr/bin/pgrep")
        .args(["-f", "/Applications/lghub.app"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    println!(
        "Logitech G HUB: {}",
        if ghub_running {
            "running; shared HID access is enabled, but G HUB may overwrite colours"
        } else {
            "not running"
        }
    );

    let summary = device::G915::probe(&config.device)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if !summary.feature_indices_queried {
        println!(
            "Warning: feature-index discovery did not fully respond; configured G915 fallbacks will be used."
        );
    }
    Ok(())
}

fn print_help() {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "\
codex-agent-indicator {}

USAGE:
    codex-agent-indicator daemon
    codex-agent-indicator hook                 # reads Codex hook JSON on stdin
    codex-agent-indicator set STATE [SESSION]
    codex-agent-indicator clear [SESSION]
    codex-agent-indicator reload
    codex-agent-indicator status
    codex-agent-indicator doctor
    codex-agent-indicator update
    codex-agent-indicator init-config [--force]

STATES:
    idle, working, approval, requested, done, error
",
        env!("CARGO_PKG_VERSION")
    );
}
