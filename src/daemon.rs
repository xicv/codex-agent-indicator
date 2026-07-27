use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{AppConfig, Paths};
use crate::device::G915;
use crate::navigation::open_codex_thread;
use crate::state::{Engine, LightingChange};
use crate::wire::{EventMessage, state_for_hook};

const SOCKET_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const G_KEY_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);

struct FlashController {
    bright: bool,
    last_toggle: Instant,
}

impl FlashController {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            bright: true,
            last_toggle: now,
        }
    }

    fn reset(&mut self) {
        self.reset_at(Instant::now());
    }

    fn reset_at(&mut self, now: Instant) {
        self.bright = true;
        self.last_toggle = now;
    }

    fn frame_if_due(
        &mut self,
        now: Instant,
        config: &AppConfig,
        engine: &Engine,
    ) -> Option<Vec<LightingChange>> {
        if !config.lighting.flash_enabled || !engine.has_active_slots() {
            if !self.bright {
                self.reset_at(now);
            }
            return None;
        }

        let interval = Duration::from_millis(config.lighting.flash_interval_ms);
        if now.saturating_duration_since(self.last_toggle) < interval {
            return None;
        }

        self.bright = !self.bright;
        self.last_toggle = now;
        let brightness = if self.bright {
            100
        } else {
            config.lighting.flash_dim_percent
        };
        Some(engine.active_lighting(brightness, config))
    }

    fn poll_interval(&self, now: Instant, config: &AppConfig, engine: &Engine) -> Duration {
        if !config.lighting.flash_enabled || !engine.has_active_slots() {
            return SOCKET_POLL_INTERVAL;
        }

        let interval = Duration::from_millis(config.lighting.flash_interval_ms);
        interval
            .saturating_sub(now.saturating_duration_since(self.last_toggle))
            .clamp(Duration::from_millis(1), SOCKET_POLL_INTERVAL)
    }
}

pub fn run(paths: Paths) -> Result<()> {
    fs::create_dir_all(&paths.runtime_dir).with_context(|| {
        format!(
            "failed to create runtime directory {}",
            paths.runtime_dir.display()
        )
    })?;
    fs::set_permissions(&paths.runtime_dir, fs::Permissions::from_mode(0o700))?;
    remove_stale_socket(&paths.socket)?;

    let socket = UnixDatagram::bind(&paths.socket)
        .with_context(|| format!("failed to bind socket {}", paths.socket.display()))?;
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    socket.set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;

    let mut config = AppConfig::load(&paths.config)?;
    config.validate()?;
    let mut config_modified = modified_at(&paths.config);
    let mut last_config_check = Instant::now();
    let mut engine = Engine::new(config.behavior.max_sessions);
    let mut flash = FlashController::new();
    let mut hardware = Hardware::new();
    hardware.connect(&config, &engine);
    write_status(&paths, &config, &engine, &hardware)?;

    let mut buffer = [0_u8; 8_192];
    loop {
        let flash_poll = flash.poll_interval(Instant::now(), &config, &engine);
        socket.set_read_timeout(Some(hardware.poll_interval(flash_poll)))?;
        match socket.recv(&mut buffer) {
            Ok(length) => match serde_json::from_slice::<EventMessage>(&buffer[..length]) {
                Ok(message) => {
                    let changed = handle_message(
                        message,
                        &paths,
                        &mut config,
                        &mut config_modified,
                        &mut engine,
                        &mut hardware,
                        &mut flash,
                    );
                    if changed {
                        write_status(&paths, &config, &engine, &hardware)?;
                    }
                }
                Err(error) => {
                    eprintln!("ignored malformed indicator event: {error}");
                }
            },
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("indicator socket receive failed"),
        }

        let pressed_g_keys = hardware.poll_g_key_presses();
        let mut navigation_status_changed = false;
        for g_key in pressed_g_keys {
            let Some(session_id) = engine.session_for_g_key(g_key).map(str::to_owned) else {
                continue;
            };
            navigation_status_changed = true;
            if let Err(error) = open_codex_thread(&session_id) {
                hardware.last_navigation_error =
                    Some(format!("failed to open G{g_key} task: {error:#}"));
                eprintln!(
                    "failed to open Codex task for G{g_key}: {error:#}"
                );
            } else {
                hardware.last_navigation_error = None;
                let changes = engine.acknowledge_g_key(g_key, &config);
                apply_state_change(
                    &config,
                    &engine,
                    &mut hardware,
                    &mut flash,
                    &changes,
                );
                eprintln!("opened Codex task mapped to G{g_key}");
            }
        }
        if navigation_status_changed {
            write_status(&paths, &config, &engine, &hardware)?;
        }

        if last_config_check.elapsed() >= CONFIG_CHECK_INTERVAL {
            last_config_check = Instant::now();
            let current_modified = modified_at(&paths.config);
            if current_modified != config_modified {
                match reload_config(
                    &paths,
                    &mut config,
                    &mut engine,
                    &mut hardware,
                    &mut flash,
                ) {
                    Ok(()) => {
                        config_modified = current_modified;
                        write_status(&paths, &config, &engine, &hardware)?;
                    }
                    Err(error) => eprintln!("configuration reload rejected: {error:#}"),
                }
            }
        }

        if hardware.retry_due() {
            hardware.connect(&config, &engine);
            write_status(&paths, &config, &engine, &hardware)?;
        }

        if let Some(frame) = flash.frame_if_due(Instant::now(), &config, &engine) {
            hardware.apply(&config, &engine, &frame);
        }
    }
}

fn handle_message(
    message: EventMessage,
    paths: &Paths,
    config: &mut AppConfig,
    config_modified: &mut Option<SystemTime>,
    engine: &mut Engine,
    hardware: &mut Hardware,
    flash: &mut FlashController,
) -> bool {
    match message {
        EventMessage::Hook {
            session_id,
            hook_event_name,
            last_assistant_message,
            tool_failed,
        } => {
            let Some(state) = state_for_hook(
                &hook_event_name,
                last_assistant_message.as_deref(),
                tool_failed,
                config,
            ) else {
                return false;
            };
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(config, engine, hardware, flash, &changes);
            true
        }
        EventMessage::Set { session_id, state } => {
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(config, engine, hardware, flash, &changes);
            true
        }
        EventMessage::Clear { session_id } => {
            let changes = match session_id {
                Some(session_id) => engine.clear_session(&session_id, config),
                None => engine.clear_all(config),
            };
            apply_state_change(config, engine, hardware, flash, &changes);
            true
        }
        EventMessage::Reload => {
            if let Err(error) = reload_config(paths, config, engine, hardware, flash) {
                hardware.last_error = Some(format!("configuration reload failed: {error:#}"));
            } else {
                *config_modified = modified_at(&paths.config);
            }
            true
        }
    }
}

fn apply_state_change(
    config: &AppConfig,
    engine: &Engine,
    hardware: &mut Hardware,
    flash: &mut FlashController,
    changes: &[LightingChange],
) {
    if changes.is_empty() {
        return;
    }
    flash.reset();
    hardware.apply(config, engine, &engine.repaint(config));
}

fn reload_config(
    paths: &Paths,
    config: &mut AppConfig,
    engine: &mut Engine,
    hardware: &mut Hardware,
    flash: &mut FlashController,
) -> Result<()> {
    let updated = AppConfig::load(&paths.config)?;
    updated.validate()?;
    let shape_changed = updated.behavior.max_sessions != config.behavior.max_sessions
        || updated.device.slot_keys != config.device.slot_keys
        || updated.device.vendor_id != config.device.vendor_id
        || updated.device.product_id != config.device.product_id
        || updated.device.usage_page != config.device.usage_page
        || updated.device.usage != config.device.usage;

    if shape_changed {
        hardware.reset_background(config);
        *engine = Engine::new(updated.behavior.max_sessions);
        hardware.disconnect();
    }
    *config = updated;
    flash.reset();
    hardware.refresh(config, engine);
    Ok(())
}

struct Hardware {
    keyboard: Option<G915>,
    last_error: Option<String>,
    last_navigation_error: Option<String>,
    next_retry: Instant,
}

impl Hardware {
    fn new() -> Self {
        Self {
            keyboard: None,
            last_error: None,
            last_navigation_error: None,
            next_retry: Instant::now(),
        }
    }

    fn connect(&mut self, config: &AppConfig, engine: &Engine) {
        if self.keyboard.is_some() || !self.retry_due() {
            return;
        }

        match G915::connect(&config.device) {
            Ok((mut keyboard, summary)) => {
                if let Err(error) = keyboard.set_background(config.lighting.background) {
                    self.last_error = Some(format!("G915 initialization failed: {error:#}"));
                    self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
                    return;
                }
                let active = engine.active_lighting(100, config);
                let active: Vec<_> = active
                    .iter()
                    .map(|change| (change.key, change.color))
                    .collect();
                if let Err(error) = keyboard.set_keys(&active) {
                    self.last_error =
                        Some(format!("G915 task-light initialization failed: {error:#}"));
                    self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
                    return;
                }
                match keyboard.set_g_key_navigation(config.navigation.enabled) {
                    Ok(true) => self.last_navigation_error = None,
                    Ok(false) if config.navigation.enabled => {
                        self.last_navigation_error =
                            Some("G915 does not expose HID++ G-key notifications".to_string());
                    }
                    Ok(false) => self.last_navigation_error = None,
                    Err(error) => {
                        self.last_navigation_error =
                            Some(format!("G-key navigation initialization failed: {error:#}"));
                    }
                }
                self.last_error = None;
                eprintln!(
                    "connected {} {:04x}:{:04x}, {} lighting zones, per-key feature 0x{:02x}, G-key navigation {}",
                    summary.product,
                    summary.vendor_id,
                    summary.product_id,
                    summary.zone_count,
                    summary.feature_indices.per_key_lighting,
                    if keyboard.g_key_navigation_active() {
                        "active"
                    } else {
                        "unavailable"
                    }
                );
                self.keyboard = Some(keyboard);
            }
            Err(error) => {
                self.last_error = Some(format!("{error:#}"));
                self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
            }
        }
    }

    fn apply(&mut self, config: &AppConfig, engine: &Engine, changes: &[LightingChange]) {
        if changes.is_empty() {
            return;
        }
        if self.keyboard.is_none() {
            self.connect(config, engine);
        }
        let Some(keyboard) = self.keyboard.as_mut() else {
            return;
        };

        let keys: Vec<_> = changes
            .iter()
            .map(|change| (change.key, change.color))
            .collect();
        if let Err(error) = keyboard.set_keys(&keys) {
            self.last_error = Some(format!("G915 lighting write failed: {error:#}"));
            self.keyboard = None;
            self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
            return;
        }
        self.last_error = None;
    }

    fn refresh(&mut self, config: &AppConfig, engine: &Engine) {
        if self.keyboard.is_none() {
            self.connect(config, engine);
            return;
        }

        let result = self
            .keyboard
            .as_mut()
            .expect("keyboard checked above")
            .set_background(config.lighting.background);
        if let Err(error) = result {
            self.last_error = Some(format!("G915 background refresh failed: {error:#}"));
            self.keyboard = None;
            self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
            return;
        }

        self.last_error = None;
        let navigation_result = self
            .keyboard
            .as_mut()
            .expect("keyboard checked above")
            .set_g_key_navigation(config.navigation.enabled);
        match navigation_result {
            Ok(true) => self.last_navigation_error = None,
            Ok(false) if config.navigation.enabled => {
                self.last_navigation_error =
                    Some("G915 does not expose HID++ G-key notifications".to_string());
            }
            Ok(false) => self.last_navigation_error = None,
            Err(error) => {
                self.last_navigation_error =
                    Some(format!("G-key navigation refresh failed: {error:#}"));
            }
        }
        self.apply(config, engine, &engine.active_lighting(100, config));
    }

    fn reset_background(&mut self, config: &AppConfig) {
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(error) = keyboard.set_background(config.lighting.background)
        {
            self.last_error = Some(format!("failed to reset keyboard background: {error:#}"));
        }
    }

    fn disconnect(&mut self) {
        self.keyboard = None;
        self.next_retry = Instant::now();
    }

    fn retry_due(&self) -> bool {
        self.keyboard.is_none() && Instant::now() >= self.next_retry
    }

    fn poll_interval(&self, default: Duration) -> Duration {
        if self
            .keyboard
            .as_ref()
            .is_some_and(G915::g_key_navigation_active)
        {
            default.min(G_KEY_INPUT_POLL_INTERVAL)
        } else {
            default
        }
    }

    fn poll_g_key_presses(&mut self) -> Vec<usize> {
        let Some(keyboard) = self.keyboard.as_mut() else {
            return Vec::new();
        };
        match keyboard.poll_g_key_presses() {
            Ok(pressed) => pressed,
            Err(error) => {
                self.last_error = Some(format!("G915 input read failed: {error:#}"));
                self.keyboard = None;
                self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
                Vec::new()
            }
        }
    }
}

#[derive(Serialize)]
struct DaemonStatus<'a> {
    pid: u32,
    updated_at: u64,
    config_path: String,
    device_connected: bool,
    last_error: &'a Option<String>,
    g_key_navigation_enabled: bool,
    g_key_navigation_active: bool,
    last_navigation_error: &'a Option<String>,
    slots: Vec<crate::state::SlotSnapshot>,
}

fn write_status(
    paths: &Paths,
    config: &AppConfig,
    engine: &Engine,
    hardware: &Hardware,
) -> Result<()> {
    let status = DaemonStatus {
        pid: std::process::id(),
        updated_at: epoch_seconds(),
        config_path: paths.config.display().to_string(),
        device_connected: hardware.keyboard.is_some(),
        last_error: &hardware.last_error,
        g_key_navigation_enabled: config.navigation.enabled,
        g_key_navigation_active: hardware
            .keyboard
            .as_ref()
            .is_some_and(G915::g_key_navigation_active),
        last_navigation_error: &hardware.last_navigation_error,
        slots: engine.snapshot(config),
    };
    let content = serde_json::to_vec_pretty(&status)?;
    let temporary = paths.status.with_extension("json.tmp");
    fs::write(&temporary, content)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, &paths.status)
        .with_context(|| format!("failed to replace {}", paths.status.display()))?;
    Ok(())
}

fn modified_at(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|metadata| metadata.modified()).ok()
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove stale socket {}", path.display()))
        }
    }
}

pub fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::config::AppConfig;
    use crate::state::{Engine, StateKind};

    use super::FlashController;

    #[test]
    fn alternates_active_g_keys_between_dim_and_full_status_colours() {
        let config = AppConfig::default();
        let mut engine = Engine::new(config.behavior.max_sessions);
        engine.transition("task", StateKind::Approval, 10, &config);
        let start = Instant::now();
        let mut flash = FlashController::new_at(start);

        assert!(
            flash
                .frame_if_due(start + Duration::from_millis(499), &config, &engine)
                .is_none()
        );

        let dim = flash
            .frame_if_due(start + Duration::from_millis(500), &config, &engine)
            .expect("dim frame");
        assert_eq!(dim.len(), 1);
        assert_eq!(
            dim[0].color,
            config
                .colors
                .approval
                .scale_percent(config.lighting.flash_dim_percent)
        );

        let bright = flash
            .frame_if_due(start + Duration::from_millis(1_000), &config, &engine)
            .expect("bright frame");
        assert_eq!(bright[0].color, config.colors.approval);
    }
}
