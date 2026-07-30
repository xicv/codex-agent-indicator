use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, Paths};
use crate::device::G915;
use crate::journal::{JournalPoll, JournalTracker, SessionJournalTransition};
use crate::navigation::open_codex_thread;
use crate::state::{Engine, LightingChange, RestoredSlot};
use crate::wire::{EventMessage, LifecycleTracker};

const SOCKET_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const G_KEY_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STATUS_WRITE_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_LOOP_DELAY_THRESHOLD: Duration = Duration::from_millis(250);
const EVENT_LOOP_REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusWriteRequest {
    None,
    Deferred,
    Immediate,
}

struct StatusPersistence {
    deadline: Option<Instant>,
}

impl StatusPersistence {
    fn new() -> Self {
        Self { deadline: None }
    }

    fn note(&mut self, now: Instant, request: StatusWriteRequest) -> bool {
        match request {
            StatusWriteRequest::None => false,
            StatusWriteRequest::Deferred => {
                self.deadline
                    .get_or_insert(now + STATUS_WRITE_DEBOUNCE_INTERVAL);
                false
            }
            StatusWriteRequest::Immediate => {
                self.deadline = None;
                true
            }
        }
    }

    fn poll_interval(&self, now: Instant) -> Duration {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(SOCKET_POLL_INTERVAL)
            .clamp(Duration::from_millis(1), SOCKET_POLL_INTERVAL)
    }

    fn take_if_due(&mut self, now: Instant) -> bool {
        if self.deadline.is_none_or(|deadline| now < deadline) {
            return false;
        }
        self.deadline = None;
        true
    }

    fn mark_flushed(&mut self) {
        self.deadline = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ObservedFailure {
    occurred_at: u64,
    message: String,
}

struct StatusPublisher {
    active_error: Option<String>,
    failure_count: u64,
    last_failure: Option<ObservedFailure>,
    event_loop: EventLoopHealth,
}

impl StatusPublisher {
    fn new() -> Self {
        Self {
            active_error: None,
            failure_count: 0,
            last_failure: None,
            event_loop: EventLoopHealth::new(),
        }
    }

    fn publish(
        &mut self,
        paths: &Paths,
        config: &AppConfig,
        engine: &Engine,
        hardware: &Hardware,
        journals: &JournalTracker,
    ) {
        match write_status(paths, config, engine, hardware, journals, self) {
            Ok(()) => {
                if let Some(error) = self.active_error.take() {
                    log_event(
                        "status-write-recovered",
                        &format!("status cache is writable again after {error}"),
                    );
                }
            }
            Err(error) => self.observe_failure_at(format!("{error:#}"), epoch_seconds()),
        }
    }

    fn observe_failure_at(&mut self, message: String, occurred_at: u64) {
        let changed = self.active_error.as_deref() != Some(message.as_str());
        self.failure_count = self.failure_count.saturating_add(1);
        self.last_failure = Some(ObservedFailure {
            occurred_at,
            message: message.clone(),
        });
        self.active_error = Some(message.clone());
        if changed {
            log_event("status-write-failed", &message);
        }
    }
}

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

struct LightingWatchdog {
    last_reassert: Instant,
}

impl LightingWatchdog {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self { last_reassert: now }
    }

    fn take_if_due(&mut self, now: Instant, interval: Duration) -> bool {
        if now.saturating_duration_since(self.last_reassert) < interval {
            return false;
        }
        self.last_reassert = now;
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ObservedDelay {
    occurred_at: u64,
    delay_ms: u64,
}

struct EventLoopHealth {
    wait_deadline: Option<Instant>,
    last_reported_at: Option<Instant>,
    delay_count: u64,
    last_delay: Option<ObservedDelay>,
    max_delay_ms: u64,
}

impl EventLoopHealth {
    fn new() -> Self {
        Self {
            wait_deadline: None,
            last_reported_at: None,
            delay_count: 0,
            last_delay: None,
            max_delay_ms: 0,
        }
    }

    fn begin_wait(&mut self, now: Instant, interval: Duration) {
        self.wait_deadline = Some(now + interval);
    }

    fn observe_at(&mut self, now: Instant, occurred_at: u64) -> bool {
        let Some(deadline) = self.wait_deadline.take() else {
            return false;
        };
        let delay = now.saturating_duration_since(deadline);
        if delay < EVENT_LOOP_DELAY_THRESHOLD {
            return false;
        }

        let delay_ms = duration_millis(delay);
        self.delay_count = self.delay_count.saturating_add(1);
        self.max_delay_ms = self.max_delay_ms.max(delay_ms);
        self.last_delay = Some(ObservedDelay {
            occurred_at,
            delay_ms,
        });
        true
    }

    fn take_report_if_due(&mut self, now: Instant) -> bool {
        if self.last_reported_at.is_some_and(|last_reported| {
            now.saturating_duration_since(last_reported) < EVENT_LOOP_REPORT_INTERVAL
        }) {
            return false;
        }
        self.last_reported_at = Some(now);
        true
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
    let mut engine = restore_engine(&paths, config.behavior.max_sessions);
    let mut journals = JournalTracker::new(paths.codex_sessions.clone());
    let restored_sessions = engine
        .snapshot(&config)
        .into_iter()
        .map(|slot| slot.session_id)
        .collect::<Vec<_>>();
    let restored = journals.restore(restored_sessions, &config);
    prune_unadmitted_sessions(
        &mut engine,
        &restored.admitted_sessions,
        &config,
    );
    for transition in restored.transitions {
        engine.reconcile(
            &transition.session_id,
            transition.state,
            transition.occurred_at,
            &config,
        );
    }
    let mut lifecycle = LifecycleTracker::default();
    let mut flash = FlashController::new();
    let mut lighting_watchdog = LightingWatchdog::new();
    let mut hardware = Hardware::new();
    let mut status_publisher = StatusPublisher::new();
    log_event(
        "daemon-started",
        &format!("pid={}", std::process::id()),
    );
    hardware.connect(&config, &engine);
    status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
    let mut status_persistence = StatusPersistence::new();

    let mut buffer = [0_u8; 8_192];
    loop {
        let observed_at = Instant::now();
        let event_loop_report = if status_publisher
            .event_loop
            .observe_at(observed_at, epoch_seconds())
            && status_publisher
                .event_loop
                .take_report_if_due(observed_at)
        {
            let delay = status_publisher
                .event_loop
                .last_delay
                .as_ref()
                .expect("delay was just observed");
            Some(format!(
                "delay_ms={} count={} max_delay_ms={}",
                delay.delay_ms,
                status_publisher.event_loop.delay_count,
                status_publisher.event_loop.max_delay_ms
            ))
        } else {
            None
        };
        if let Some(report) = event_loop_report {
            log_event("event-loop-delay", &report);
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        let wait_started = Instant::now();
        let flash_poll = flash.poll_interval(wait_started, &config, &engine);
        let status_poll = status_persistence.poll_interval(wait_started);
        let journal_poll = journals.poll_interval(wait_started);
        let poll_interval = hardware.poll_interval(flash_poll.min(status_poll).min(journal_poll));
        socket.set_read_timeout(Some(poll_interval))?;
        status_publisher
            .event_loop
            .begin_wait(wait_started, poll_interval);
        match socket.recv(&mut buffer) {
            Ok(length) => match serde_json::from_slice::<EventMessage>(&buffer[..length]) {
                Ok(message) => {
                    let status_write = handle_message(
                        message,
                        ReloadContext {
                            paths: &paths,
                            config_modified: &mut config_modified,
                            journals: &mut journals,
                        },
                        &mut config,
                        &mut engine,
                        &mut lifecycle,
                        &mut hardware,
                        &mut flash,
                    );
                    if status_persistence.note(Instant::now(), status_write) {
                        status_publisher
                            .publish(&paths, &config, &engine, &hardware, &journals);
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
            journals.retain_sessions(
                engine
                    .snapshot(&config)
                    .into_iter()
                    .map(|slot| slot.session_id),
            );
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        let previous_journal_error = journals.last_error().clone();
        let poll = journals.poll_if_due(Instant::now(), &config);
        let status_write = apply_journal_poll(
            poll,
            &config,
            &mut engine,
            &mut lifecycle,
            &mut hardware,
            &mut flash,
        );
        if status_persistence.note(Instant::now(), status_write) {
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
        }
        if previous_journal_error != *journals.last_error() {
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
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
                        lifecycle.clear(None);
                        journals.retain_sessions(
                            engine
                                .snapshot(&config)
                                .into_iter()
                                .map(|slot| slot.session_id),
                        );
                        status_publisher
                            .publish(&paths, &config, &engine, &hardware, &journals);
                        status_persistence.mark_flushed();
                    }
                    Err(error) => eprintln!("configuration reload rejected: {error:#}"),
                }
            }
        }

        if hardware.retry_due() {
            hardware.connect(&config, &engine);
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        if let Some(frame) = flash.frame_if_due(Instant::now(), &config, &engine) {
            hardware.apply(&config, &engine, &frame);
        }

        let reassert_interval =
            Duration::from_millis(config.lighting.reassert_interval_ms);
        if lighting_watchdog.take_if_due(Instant::now(), reassert_interval)
            && hardware.reassert_direct_lighting(&config, &engine)
        {
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        if status_persistence.take_if_due(Instant::now()) {
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
        }
    }
}

struct ReloadContext<'a> {
    paths: &'a Paths,
    config_modified: &'a mut Option<SystemTime>,
    journals: &'a mut JournalTracker,
}

fn handle_message(
    message: EventMessage,
    reload: ReloadContext<'_>,
    config: &mut AppConfig,
    engine: &mut Engine,
    lifecycle: &mut LifecycleTracker,
    hardware: &mut Hardware,
    flash: &mut FlashController,
) -> StatusWriteRequest {
    if let EventMessage::Hook {
        session_id,
        hook_event_name,
        transcript_path,
        ..
    } = &message
    {
        let admitted = match transcript_path {
            Some(transcript_path) => match reload
                .journals
                .register_live(session_id, Path::new(transcript_path))
            {
                Ok(admitted) => admitted,
                Err(error) => {
                    reload.journals.record_error(format!("{error:#}"));
                    false
                }
            },
            None => false,
        };
        if !admitted {
            lifecycle.clear(Some(session_id));
            if hook_event_name == "SessionEnd" {
                reload.journals.clear(Some(session_id));
            }
            return StatusWriteRequest::None;
        }
    }
    let hook_state = lifecycle.state_for_event(&message, config);
    match message {
        EventMessage::Hook {
            session_id,
            ..
        } => {
            let Some(state) = hook_state else {
                retain_journals_for_slots(reload.journals, engine, config);
                return StatusWriteRequest::None;
            };
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(config, engine, hardware, flash, &changes);
            retain_journals_for_slots(reload.journals, engine, config);
            if changes.is_empty() {
                StatusWriteRequest::Deferred
            } else {
                StatusWriteRequest::Immediate
            }
        }
        EventMessage::Set { session_id, state } => {
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(config, engine, hardware, flash, &changes);
            retain_journals_for_slots(reload.journals, engine, config);
            if changes.is_empty() {
                StatusWriteRequest::Deferred
            } else {
                StatusWriteRequest::Immediate
            }
        }
        EventMessage::Clear { session_id } => {
            lifecycle.clear(session_id.as_deref());
            reload.journals.clear(session_id.as_deref());
            let changes = match session_id {
                Some(session_id) => engine.clear_session(&session_id, config),
                None => engine.clear_all(config),
            };
            apply_state_change(config, engine, hardware, flash, &changes);
            if changes.is_empty() {
                StatusWriteRequest::None
            } else {
                StatusWriteRequest::Immediate
            }
        }
        EventMessage::Reload => {
            if let Err(error) =
                reload_config(reload.paths, config, engine, hardware, flash)
            {
                hardware.last_error = Some(format!("configuration reload failed: {error:#}"));
            } else {
                lifecycle.clear(None);
                *reload.config_modified = modified_at(&reload.paths.config);
            }
            retain_journals_for_slots(reload.journals, engine, config);
            StatusWriteRequest::Immediate
        }
        EventMessage::Snapshot => StatusWriteRequest::Immediate,
    }
}

fn apply_journal_transitions(
    transitions: Vec<SessionJournalTransition>,
    config: &AppConfig,
    engine: &mut Engine,
    hardware: &mut Hardware,
    flash: &mut FlashController,
) -> StatusWriteRequest {
    if transitions.is_empty() {
        return StatusWriteRequest::None;
    }
    let mut changes = Vec::new();
    for transition in transitions {
        changes.extend(engine.reconcile(
            &transition.session_id,
            transition.state,
            transition.occurred_at,
            config,
        ));
    }
    apply_state_change(config, engine, hardware, flash, &changes);
    if changes.is_empty() {
        StatusWriteRequest::Deferred
    } else {
        StatusWriteRequest::Immediate
    }
}

fn apply_journal_poll(
    poll: JournalPoll,
    config: &AppConfig,
    engine: &mut Engine,
    lifecycle: &mut LifecycleTracker,
    hardware: &mut Hardware,
    flash: &mut FlashController,
) -> StatusWriteRequest {
    for transition in &poll.transitions {
        lifecycle.clear(Some(&transition.session_id));
    }
    let transition_request =
        apply_journal_transitions(poll.transitions, config, engine, hardware, flash);

    if poll.removed_sessions.is_empty() {
        return transition_request;
    }

    let changes =
        clear_removed_journal_sessions(poll.removed_sessions, config, engine, lifecycle);
    apply_state_change(config, engine, hardware, flash, &changes);
    StatusWriteRequest::Immediate
}

fn clear_removed_journal_sessions(
    session_ids: Vec<String>,
    config: &AppConfig,
    engine: &mut Engine,
    lifecycle: &mut LifecycleTracker,
) -> Vec<LightingChange> {
    let mut changes = Vec::new();
    for session_id in session_ids {
        lifecycle.clear(Some(&session_id));
        changes.extend(engine.clear_session(&session_id, config));
    }
    changes
}

fn retain_journals_for_slots(
    journals: &mut JournalTracker,
    engine: &Engine,
    config: &AppConfig,
) {
    journals.retain_sessions(
        engine
            .snapshot(config)
            .into_iter()
            .map(|slot| slot.session_id),
    );
}

fn prune_unadmitted_sessions(
    engine: &mut Engine,
    admitted_sessions: &HashSet<String>,
    config: &AppConfig,
) {
    let stale_sessions = engine
        .snapshot(config)
        .into_iter()
        .map(|slot| slot.session_id)
        .filter(|session_id| !admitted_sessions.contains(session_id))
        .collect::<Vec<_>>();
    for session_id in stale_sessions {
        engine.clear_session(&session_id, config);
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
    failure_count: u64,
    connection_count: u64,
    last_failure: Option<ObservedFailure>,
    last_connected_at: Option<u64>,
    lighting_reassertion_count: u64,
    last_lighting_reasserted_at: Option<u64>,
    last_lighting_reassert_duration_ms: Option<u64>,
    max_lighting_reassert_duration_ms: u64,
    next_retry: Instant,
}

impl Hardware {
    fn new() -> Self {
        Self {
            keyboard: None,
            last_error: None,
            last_navigation_error: None,
            failure_count: 0,
            connection_count: 0,
            last_failure: None,
            last_connected_at: None,
            lighting_reassertion_count: 0,
            last_lighting_reasserted_at: None,
            last_lighting_reassert_duration_ms: None,
            max_lighting_reassert_duration_ms: 0,
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
                    self.schedule_retry(format!("G915 initialization failed: {error:#}"));
                    return;
                }
                let active = engine.active_lighting(100, config);
                let active: Vec<_> = active
                    .iter()
                    .map(|change| (change.key, change.color))
                    .collect();
                if let Err(error) = keyboard.set_keys(&active) {
                    self.schedule_retry(format!(
                        "G915 task-light initialization failed: {error:#}"
                    ));
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
                self.record_connected_at(epoch_seconds());
                log_event(
                    "g915-connected",
                    &format!(
                        "{} {:04x}:{:04x}, {} lighting zones, per-key feature 0x{:02x}, G-key navigation {}",
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
                    ),
                );
                self.keyboard = Some(keyboard);
            }
            Err(error) => {
                self.schedule_retry(format!("{error:#}"));
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
            self.schedule_retry(format!("G915 lighting write failed: {error:#}"));
            return;
        }
        self.last_error = None;
    }

    fn reassert_direct_lighting(&mut self, config: &AppConfig, engine: &Engine) -> bool {
        let previous_error = self.last_error.clone();
        if self.keyboard.is_none() {
            self.connect(config, engine);
            return self.last_error != previous_error;
        }

        let started_at = Instant::now();
        let keys: Vec<_> = engine
            .active_lighting(100, config)
            .iter()
            .map(|change| (change.key, change.color))
            .collect();
        let result = {
            let keyboard = self.keyboard.as_mut().expect("keyboard checked above");
            keyboard
                .reassert_direct_mode()
                .and_then(|()| keyboard.set_background(config.lighting.background))
                .and_then(|()| keyboard.set_keys(&keys))
                .and_then(|()| {
                    if config.navigation.enabled {
                        keyboard.reassert_g_key_navigation().map(|_| ())
                    } else {
                        Ok(())
                    }
                })
        };
        if let Err(error) = result {
            self.schedule_retry(format!("G915 direct-lighting watchdog failed: {error:#}"));
        } else {
            self.last_error = None;
            self.record_lighting_reassertion_at(epoch_seconds(), started_at.elapsed());
        }
        self.last_error != previous_error
    }

    fn record_lighting_reassertion_at(&mut self, occurred_at: u64, duration: Duration) {
        let duration_ms = duration_millis(duration);
        self.lighting_reassertion_count = self.lighting_reassertion_count.saturating_add(1);
        self.last_lighting_reasserted_at = Some(occurred_at);
        self.last_lighting_reassert_duration_ms = Some(duration_ms);
        self.max_lighting_reassert_duration_ms =
            self.max_lighting_reassert_duration_ms.max(duration_ms);
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
            self.schedule_retry(format!("G915 background refresh failed: {error:#}"));
            return;
        }

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
        self.last_error = None;
        self.apply(config, engine, &engine.active_lighting(100, config));
    }

    fn reset_background(&mut self, config: &AppConfig) {
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(error) = keyboard.set_background(config.lighting.background)
        {
            self.schedule_retry(format!("failed to reset keyboard background: {error:#}"));
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
                self.schedule_retry(format!("G915 input read failed: {error:#}"));
                Vec::new()
            }
        }
    }

    fn schedule_retry(&mut self, message: String) {
        self.record_failure_at(message, epoch_seconds());
        self.keyboard = None;
        self.next_retry = Instant::now() + DEVICE_RETRY_INTERVAL;
    }

    fn record_failure_at(&mut self, message: String, occurred_at: u64) {
        let changed = self.last_error.as_deref() != Some(message.as_str());
        self.failure_count = self.failure_count.saturating_add(1);
        self.last_failure = Some(ObservedFailure {
            occurred_at,
            message: message.clone(),
        });
        self.last_error = Some(message.clone());
        if changed {
            log_event("g915-failure", &message);
        }
    }

    fn record_connected_at(&mut self, connected_at: u64) {
        let recovered_from = self.last_error.take();
        self.connection_count = self.connection_count.saturating_add(1);
        self.last_connected_at = Some(connected_at);
        if let Some(error) = recovered_from {
            log_event(
                "g915-recovered",
                &format!("connection restored after {error}"),
            );
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
    hardware_connection_count: u64,
    hardware_failure_count: u64,
    last_hardware_connected_at: Option<u64>,
    last_hardware_failure: &'a Option<ObservedFailure>,
    lighting_reassertion_count: u64,
    last_lighting_reasserted_at: Option<u64>,
    last_lighting_reassert_duration_ms: Option<u64>,
    max_lighting_reassert_duration_ms: u64,
    status_write_failure_count: u64,
    last_status_write_failure: &'a Option<ObservedFailure>,
    event_loop_delay_count: u64,
    last_event_loop_delay: &'a Option<ObservedDelay>,
    max_event_loop_delay_ms: u64,
    g_key_navigation_enabled: bool,
    g_key_navigation_active: bool,
    last_navigation_error: &'a Option<String>,
    lifecycle_sources: usize,
    last_lifecycle_error: &'a Option<String>,
    slots: Vec<crate::state::SlotSnapshot>,
}

#[derive(Deserialize)]
struct RestorableStatus {
    #[serde(default)]
    slots: Vec<RestoredSlot>,
}

fn restore_engine(paths: &Paths, max_sessions: usize) -> Engine {
    let status = match fs::read(&paths.status) {
        Ok(content) => serde_json::from_slice::<RestorableStatus>(&content),
        Err(error) if error.kind() == ErrorKind::NotFound => return Engine::new(max_sessions),
        Err(error) => {
            eprintln!(
                "could not restore indicator state from {}: {error}",
                paths.status.display()
            );
            return Engine::new(max_sessions);
        }
    };
    match status {
        Ok(status) => Engine::restore(max_sessions, status.slots),
        Err(error) => {
            eprintln!(
                "ignored invalid indicator state in {}: {error}",
                paths.status.display()
            );
            Engine::new(max_sessions)
        }
    }
}

fn write_status(
    paths: &Paths,
    config: &AppConfig,
    engine: &Engine,
    hardware: &Hardware,
    journals: &JournalTracker,
    status_publisher: &StatusPublisher,
) -> Result<()> {
    let status = DaemonStatus {
        pid: std::process::id(),
        updated_at: epoch_seconds(),
        config_path: paths.config.display().to_string(),
        device_connected: hardware.keyboard.is_some(),
        last_error: &hardware.last_error,
        hardware_connection_count: hardware.connection_count,
        hardware_failure_count: hardware.failure_count,
        last_hardware_connected_at: hardware.last_connected_at,
        last_hardware_failure: &hardware.last_failure,
        lighting_reassertion_count: hardware.lighting_reassertion_count,
        last_lighting_reasserted_at: hardware.last_lighting_reasserted_at,
        last_lighting_reassert_duration_ms: hardware.last_lighting_reassert_duration_ms,
        max_lighting_reassert_duration_ms: hardware.max_lighting_reassert_duration_ms,
        status_write_failure_count: status_publisher.failure_count,
        last_status_write_failure: &status_publisher.last_failure,
        event_loop_delay_count: status_publisher.event_loop.delay_count,
        last_event_loop_delay: &status_publisher.event_loop.last_delay,
        max_event_loop_delay_ms: status_publisher.event_loop.max_delay_ms,
        g_key_navigation_enabled: config.navigation.enabled,
        g_key_navigation_active: hardware
            .keyboard
            .as_ref()
            .is_some_and(G915::g_key_navigation_active),
        last_navigation_error: &hardware.last_navigation_error,
        lifecycle_sources: journals.source_count(),
        last_lifecycle_error: journals.last_error(),
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
    let probe = UnixDatagram::unbound().context("failed to create daemon socket probe")?;
    probe
        .set_nonblocking(true)
        .context("failed to configure daemon socket probe")?;
    let snapshot =
        serde_json::to_vec(&EventMessage::Snapshot).context("failed to encode daemon probe")?;
    match probe.send_to(&snapshot, path) {
        Ok(_) => bail!(
            "codex-agent-indicator daemon is already running at {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::WouldBlock => bail!(
            "codex-agent-indicator daemon is already running at {}",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to probe daemon socket {}", path.display()));
        }
    }

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

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn log_event(event: &str, message: &str) {
    eprintln!("ts={} event={event} message={message:?}", epoch_seconds());
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::os::unix::net::UnixDatagram;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::config::{AppConfig, Paths};
    use crate::journal::JournalTracker;
    use crate::state::{Engine, RestoredSlot, StateKind};
    use crate::wire::LifecycleTracker;

    use super::{
        EventLoopHealth, FlashController, Hardware, LightingWatchdog, StatusPersistence,
        StatusPublisher, StatusWriteRequest, clear_removed_journal_sessions,
        prune_unadmitted_sessions, remove_stale_socket,
    };

    fn temporary_root(label: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::path::Path::new("/tmp").join(format!(
            "cai-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn active_daemon_socket_is_never_removed() {
        let root = temporary_root("active");
        fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("indicator.sock");
        let socket = UnixDatagram::bind(&socket_path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        let error = remove_stale_socket(&socket_path).unwrap_err();

        assert!(error.to_string().contains("daemon is already running"));
        assert!(socket_path.exists());
        let mut buffer = [0_u8; 128];
        let received = socket.recv(&mut buffer).unwrap();
        let message: crate::wire::EventMessage =
            serde_json::from_slice(&buffer[..received]).unwrap();
        assert!(matches!(message, crate::wire::EventMessage::Snapshot));

        drop(socket);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abandoned_daemon_socket_is_removed_before_binding() {
        let root = temporary_root("stale");
        fs::create_dir_all(&root).unwrap();
        let socket_path = root.join("indicator.sock");
        let socket = UnixDatagram::bind(&socket_path).unwrap();
        drop(socket);
        assert!(socket_path.exists());

        remove_stale_socket(&socket_path).unwrap();

        assert!(!socket_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_prunes_ghost_and_non_app_slots_before_lighting_them() {
        let config = AppConfig::default();
        let mut engine = Engine::restore(
            config.behavior.max_sessions,
            vec![
                RestoredSlot {
                    slot: 1,
                    session_id: "desktop-task".to_owned(),
                    state: StateKind::Done,
                    updated_at: 10,
                },
                RestoredSlot {
                    slot: 2,
                    session_id: "missing-ghost".to_owned(),
                    state: StateKind::Working,
                    updated_at: 11,
                },
                RestoredSlot {
                    slot: 3,
                    session_id: "cli-task".to_owned(),
                    state: StateKind::Approval,
                    updated_at: 12,
                },
            ],
        );

        prune_unadmitted_sessions(
            &mut engine,
            &HashSet::from(["desktop-task".to_owned()]),
            &config,
        );

        let snapshot = engine.snapshot(&config);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].session_id, "desktop-task");
        assert_eq!(snapshot[0].state, StateKind::Done);
    }

    #[test]
    fn archived_journal_removes_its_indicator_slot() {
        let config = AppConfig::default();
        let mut engine = Engine::new(config.behavior.max_sessions);
        engine.transition("archived-session", StateKind::Working, 10, &config);
        let mut lifecycle = LifecycleTracker::default();

        let changes = clear_removed_journal_sessions(
            vec!["archived-session".to_owned()],
            &config,
            &mut engine,
            &mut lifecycle,
        );

        assert!(engine.snapshot(&config).is_empty());
        assert_eq!(
            changes,
            [crate::state::LightingChange {
                key: config.device.slot_keys[0],
                color: config.lighting.background,
            }]
        );
    }

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

    #[test]
    fn periodically_reasserts_direct_lighting_mode() {
        let start = Instant::now();
        let mut watchdog = LightingWatchdog::new_at(start);
        let interval = Duration::from_secs(5);

        assert!(!watchdog.take_if_due(start + interval - Duration::from_millis(1), interval));
        assert!(watchdog.take_if_due(start + interval, interval));
        assert!(!watchdog.take_if_due(start + interval + Duration::from_secs(1), interval));
        assert!(watchdog.take_if_due(start + interval * 2, interval));
    }

    #[test]
    fn records_only_actionable_event_loop_delay() {
        let start = Instant::now();
        let mut health = EventLoopHealth::new();

        health.begin_wait(start, Duration::from_millis(25));
        assert!(!health.observe_at(start + Duration::from_millis(274), 10));
        assert_eq!(health.delay_count, 0);

        health.begin_wait(start + Duration::from_secs(1), Duration::from_millis(25));
        assert!(health.observe_at(start + Duration::from_millis(1_275), 20));
        assert_eq!(health.delay_count, 1);
        assert_eq!(health.max_delay_ms, 250);
        assert_eq!(
            health
                .last_delay
                .as_ref()
                .map(|delay| (delay.occurred_at, delay.delay_ms)),
            Some((20, 250))
        );
        let observed_at = start + Duration::from_millis(1_275);
        assert!(health.take_report_if_due(observed_at));
        assert!(!health.take_report_if_due(observed_at + Duration::from_secs(29)));
        assert!(health.take_report_if_due(observed_at + Duration::from_secs(30)));
    }

    #[test]
    fn records_successful_lighting_reassertions_without_clearing_failure_history() {
        let mut hardware = Hardware::new();
        hardware.record_failure_at("earlier HID failure".to_owned(), 5);

        hardware.record_lighting_reassertion_at(10, Duration::from_millis(14));
        hardware.record_lighting_reassertion_at(20, Duration::from_millis(9));

        assert_eq!(hardware.lighting_reassertion_count, 2);
        assert_eq!(hardware.last_lighting_reasserted_at, Some(20));
        assert_eq!(hardware.last_lighting_reassert_duration_ms, Some(9));
        assert_eq!(hardware.max_lighting_reassert_duration_ms, 14);
        assert_eq!(hardware.failure_count, 1);
        assert_eq!(
            hardware
                .last_failure
                .as_ref()
                .map(|failure| failure.message.as_str()),
            Some("earlier HID failure")
        );
    }

    #[test]
    fn debounces_repeated_non_visible_status_updates() {
        let start = Instant::now();
        let mut persistence = StatusPersistence::new();

        assert!(!persistence.note(start, StatusWriteRequest::Deferred));
        assert!(!persistence.note(
            start + Duration::from_millis(100),
            StatusWriteRequest::Deferred,
        ));
        assert!(
            !persistence.take_if_due(start + Duration::from_millis(249))
        );
        assert!(persistence.take_if_due(start + Duration::from_millis(250)));
        assert!(
            !persistence.take_if_due(start + Duration::from_millis(500))
        );
    }

    #[test]
    fn visible_status_update_flushes_immediately_and_cancels_debounce() {
        let start = Instant::now();
        let mut persistence = StatusPersistence::new();

        assert!(!persistence.note(start, StatusWriteRequest::Deferred));
        assert!(persistence.note(
            start + Duration::from_millis(50),
            StatusWriteRequest::Immediate,
        ));
        assert!(
            !persistence.take_if_due(start + Duration::from_millis(500))
        );
    }

    #[test]
    fn status_write_failure_is_recorded_without_escaping_the_daemon() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "codex-agent-indicator-status-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block status path").unwrap();
        let mut paths = Paths {
            config: root.join("config.toml"),
            codex_sessions: root.join("sessions"),
            runtime_dir: root.clone(),
            socket: root.join("indicator.sock"),
            status: blocker.join("status.json"),
        };
        let config = AppConfig::default();
        let engine = Engine::new(config.behavior.max_sessions);
        let mut hardware = Hardware::new();
        hardware.record_lighting_reassertion_at(30, Duration::from_millis(12));
        let journals = JournalTracker::new(paths.codex_sessions.clone());
        let loop_start = Instant::now();
        let mut publisher = StatusPublisher::new();
        publisher
            .event_loop
            .begin_wait(loop_start, Duration::from_millis(25));
        assert!(
            publisher
                .event_loop
                .observe_at(loop_start + Duration::from_millis(325), 40)
        );

        publisher.publish(&paths, &config, &engine, &hardware, &journals);

        assert_eq!(publisher.failure_count, 1);
        assert!(
            publisher
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.message.contains("failed to write"))
        );

        paths.status = root.join("status.json");
        publisher.publish(&paths, &config, &engine, &hardware, &journals);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.status).unwrap()).unwrap();
        assert_eq!(persisted["status_write_failure_count"], 1);
        assert_eq!(persisted["lighting_reassertion_count"], 1);
        assert_eq!(persisted["last_lighting_reasserted_at"], 30);
        assert_eq!(persisted["last_lighting_reassert_duration_ms"], 12);
        assert_eq!(persisted["max_lighting_reassert_duration_ms"], 12);
        assert_eq!(persisted["event_loop_delay_count"], 1);
        assert_eq!(persisted["last_event_loop_delay"]["occurred_at"], 40);
        assert_eq!(persisted["last_event_loop_delay"]["delay_ms"], 300);
        assert_eq!(persisted["max_event_loop_delay_ms"], 300);
        assert!(
            persisted["last_status_write_failure"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("failed to write"))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_hardware_failure_remains_available_for_diagnosis() {
        let mut hardware = Hardware::new();

        hardware.record_failure_at("G915 input read failed".to_owned(), 10);
        hardware.record_connected_at(20);

        assert!(hardware.last_error.is_none());
        assert_eq!(hardware.failure_count, 1);
        assert_eq!(hardware.connection_count, 1);
        assert_eq!(hardware.last_connected_at, Some(20));
        assert_eq!(
            hardware
                .last_failure
                .as_ref()
                .map(|failure| (failure.occurred_at, failure.message.as_str())),
            Some((10, "G915 input read failed"))
        );
    }
}
