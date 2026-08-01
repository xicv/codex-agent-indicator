use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::mem::MaybeUninit;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, LightingMode, Paths};
use crate::device::G915;
use crate::journal::{JournalPoll, JournalTracker, SessionJournalTransition};
use crate::navigation::open_codex_thread;
use crate::state::{Engine, LightingChange, RestoredQueuedSession, RestoredSlot};
use crate::wire::{EventMessage, LifecycleTracker};

const SOCKET_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const DEVICE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const G_KEY_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const STATUS_WRITE_DEBOUNCE_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_LOOP_DELAY_THRESHOLD: Duration = Duration::from_millis(250);
const EVENT_LOOP_REPORT_INTERVAL: Duration = Duration::from_secs(30);
const LIGHTING_MODE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const LOG_ROTATION_CHECK_INTERVAL: Duration = Duration::from_secs(60);
const LOG_ROTATION_MAX_BYTES: u64 = 5 * 1024 * 1024;
const LOG_ROTATION_RETAINED_FILES: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusWriteRequest {
    None,
    Deferred,
    Immediate,
}

struct StatusPersistence {
    deadline: Option<Instant>,
}

struct LogRotator {
    last_checked: Instant,
    max_bytes: u64,
    retained_files: usize,
}

impl LogRotator {
    fn new() -> Self {
        Self::new_at(
            Instant::now(),
            LOG_ROTATION_MAX_BYTES,
            LOG_ROTATION_RETAINED_FILES,
        )
    }

    fn new_at(now: Instant, max_bytes: u64, retained_files: usize) -> Self {
        Self {
            last_checked: now,
            max_bytes,
            retained_files,
        }
    }

    fn rotate_if_due(&mut self, now: Instant, path: &Path) -> Result<Option<bool>> {
        if now.saturating_duration_since(self.last_checked) < LOG_ROTATION_CHECK_INTERVAL {
            return Ok(None);
        }
        self.last_checked = now;
        rotate_log_file(path, self.max_bytes, self.retained_files).map(Some)
    }
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

    fn frame_if_due_for_mode(
        &mut self,
        now: Instant,
        mode: LightingMode,
        config: &AppConfig,
        engine: &Engine,
    ) -> Option<Vec<LightingChange>> {
        if mode == LightingMode::Night
            || !config.lighting.flash_enabled
            || !engine.has_active_slots()
        {
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

    fn poll_interval_for_mode(
        &self,
        now: Instant,
        mode: LightingMode,
        config: &AppConfig,
        engine: &Engine,
    ) -> Duration {
        if mode == LightingMode::Night
            || !config.lighting.flash_enabled
            || !engine.has_active_slots()
        {
            return SOCKET_POLL_INTERVAL;
        }

        let interval = Duration::from_millis(config.lighting.flash_interval_ms);
        interval
            .saturating_sub(now.saturating_duration_since(self.last_toggle))
            .clamp(Duration::from_millis(1), SOCKET_POLL_INTERVAL)
    }
}

struct LightingModeController {
    mode: LightingMode,
    last_checked: Instant,
}

impl LightingModeController {
    fn new(config: &AppConfig) -> Result<Self> {
        Ok(Self::new_at_minute(config, local_minute_of_day()?))
    }

    fn new_at_minute(config: &AppConfig, minute_of_day: u16) -> Self {
        Self {
            mode: config.lighting.mode_at_minute(minute_of_day),
            last_checked: Instant::now(),
        }
    }

    fn mode(&self) -> LightingMode {
        self.mode
    }

    fn update_at_minute(&mut self, config: &AppConfig, minute_of_day: u16) -> bool {
        let updated = config.lighting.mode_at_minute(minute_of_day);
        if updated == self.mode {
            return false;
        }
        self.mode = updated;
        true
    }

    fn update_now(&mut self, config: &AppConfig) -> Result<bool> {
        self.last_checked = Instant::now();
        Ok(self.update_at_minute(config, local_minute_of_day()?))
    }

    fn update_if_due(&mut self, now: Instant, config: &AppConfig) -> Result<bool> {
        if now.saturating_duration_since(self.last_checked) < LIGHTING_MODE_CHECK_INTERVAL {
            return Ok(false);
        }
        self.last_checked = now;
        Ok(self.update_at_minute(config, local_minute_of_day()?))
    }
}

fn local_minute_of_day() -> Result<u16> {
    let epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let epoch_seconds: libc::time_t = epoch_seconds
        .try_into()
        .context("current time does not fit time_t")?;
    let mut local = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers remain valid for the call, and localtime_r writes one
    // complete tm value into the caller-owned output buffer on success.
    let result = unsafe { libc::localtime_r(&epoch_seconds, local.as_mut_ptr()) };
    if result.is_null() {
        bail!("localtime_r could not convert the current local time");
    }
    // SAFETY: a non-null localtime_r result confirms that the output was initialized.
    let local = unsafe { local.assume_init() };
    if !(0..=23).contains(&local.tm_hour) || !(0..=59).contains(&local.tm_min) {
        bail!("localtime_r returned an invalid hour or minute");
    }
    Ok((local.tm_hour as u16) * 60 + local.tm_min as u16)
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
    let mut lighting_mode = LightingModeController::new(&config)?;
    let mut config_modified = modified_at(&paths.config);
    let mut last_config_check = Instant::now();
    let mut engine = restore_engine(&paths, config.behavior.max_sessions);
    let mut journals = JournalTracker::new(paths.codex_sessions.clone());
    let restored_sessions = engine.tracked_session_ids();
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
    let mut log_rotator = LogRotator::new();
    let mut hardware = Hardware::new();
    let mut status_publisher = StatusPublisher::new();
    log_event(
        "daemon-started",
        &format!("pid={}", std::process::id()),
    );
    hardware.connect(&config, &engine, lighting_mode.mode());
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
        let flash_poll = flash.poll_interval_for_mode(
            wait_started,
            lighting_mode.mode(),
            &config,
            &engine,
        );
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
                            lighting_mode: &mut lighting_mode,
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
                    lighting_mode.mode(),
                    &changes,
                );
                eprintln!("opened Codex task mapped to G{g_key}");
            }
        }
        if navigation_status_changed {
            journals.retain_sessions(engine.tracked_session_ids());
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
            lighting_mode.mode(),
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
                    &mut lighting_mode,
                ) {
                    Ok(()) => {
                        config_modified = current_modified;
                        lifecycle.clear(None);
                        journals.retain_sessions(engine.tracked_session_ids());
                        status_publisher
                            .publish(&paths, &config, &engine, &hardware, &journals);
                        status_persistence.mark_flushed();
                    }
                    Err(error) => eprintln!("configuration reload rejected: {error:#}"),
                }
            }
        }

        if hardware.retry_due() {
            hardware.connect(&config, &engine, lighting_mode.mode());
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        match lighting_mode.update_if_due(Instant::now(), &config) {
            Ok(true) => {
                flash.reset();
                hardware.refresh(&config, &engine, lighting_mode.mode());
                status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
                status_persistence.mark_flushed();
            }
            Ok(false) => {}
            Err(error) => eprintln!("could not update day/night lighting mode: {error:#}"),
        }

        if let Some(frame) = flash.frame_if_due_for_mode(
            Instant::now(),
            lighting_mode.mode(),
            &config,
            &engine,
        ) {
            hardware.apply(&config, &engine, lighting_mode.mode(), &frame);
        }

        let reassert_interval =
            Duration::from_millis(config.lighting.reassert_interval_ms);
        if lighting_watchdog.take_if_due(Instant::now(), reassert_interval)
            && hardware.reassert_direct_lighting(&config, &engine, lighting_mode.mode())
        {
            status_publisher.publish(&paths, &config, &engine, &hardware, &journals);
            status_persistence.mark_flushed();
        }

        match log_rotator.rotate_if_due(Instant::now(), &paths.log) {
            Ok(Some(true)) => log_event(
                "log-rotated",
                &format!(
                    "max_bytes={LOG_ROTATION_MAX_BYTES} retained_files={LOG_ROTATION_RETAINED_FILES}"
                ),
            ),
            Ok(Some(false) | None) => {}
            Err(error) => log_event("log-rotation-failed", &format!("{error:#}")),
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
    lighting_mode: &'a mut LightingModeController,
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
                retain_journals_for_engine(reload.journals, engine);
                return StatusWriteRequest::None;
            };
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(
                config,
                engine,
                hardware,
                flash,
                reload.lighting_mode.mode(),
                &changes,
            );
            retain_journals_for_engine(reload.journals, engine);
            if changes.is_empty() {
                StatusWriteRequest::Deferred
            } else {
                StatusWriteRequest::Immediate
            }
        }
        EventMessage::Set { session_id, state } => {
            let changes = engine.transition(&session_id, state, epoch_seconds(), config);
            apply_state_change(
                config,
                engine,
                hardware,
                flash,
                reload.lighting_mode.mode(),
                &changes,
            );
            retain_journals_for_engine(reload.journals, engine);
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
            apply_state_change(
                config,
                engine,
                hardware,
                flash,
                reload.lighting_mode.mode(),
                &changes,
            );
            if changes.is_empty() {
                StatusWriteRequest::None
            } else {
                StatusWriteRequest::Immediate
            }
        }
        EventMessage::Reload => {
            if let Err(error) =
                reload_config(
                    reload.paths,
                    config,
                    engine,
                    hardware,
                    flash,
                    reload.lighting_mode,
                )
            {
                hardware.last_error = Some(format!("configuration reload failed: {error:#}"));
            } else {
                lifecycle.clear(None);
                *reload.config_modified = modified_at(&reload.paths.config);
            }
            retain_journals_for_engine(reload.journals, engine);
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
    lighting_mode: LightingMode,
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
    apply_state_change(
        config,
        engine,
        hardware,
        flash,
        lighting_mode,
        &changes,
    );
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
    lighting_mode: LightingMode,
) -> StatusWriteRequest {
    for transition in &poll.transitions {
        lifecycle.clear(Some(&transition.session_id));
    }
    let transition_request =
        apply_journal_transitions(
            poll.transitions,
            config,
            engine,
            hardware,
            flash,
            lighting_mode,
        );

    if poll.removed_sessions.is_empty() {
        return transition_request;
    }

    let changes =
        clear_removed_journal_sessions(poll.removed_sessions, config, engine, lifecycle);
    apply_state_change(
        config,
        engine,
        hardware,
        flash,
        lighting_mode,
        &changes,
    );
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

fn retain_journals_for_engine(journals: &mut JournalTracker, engine: &Engine) {
    journals.retain_sessions(engine.tracked_session_ids());
}

fn prune_unadmitted_sessions(
    engine: &mut Engine,
    admitted_sessions: &HashSet<String>,
    config: &AppConfig,
) {
    let stale_sessions = engine
        .tracked_session_ids()
        .into_iter()
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
    lighting_mode: LightingMode,
    changes: &[LightingChange],
) {
    if changes.is_empty() {
        return;
    }
    flash.reset();
    hardware.apply(
        config,
        engine,
        lighting_mode,
        &engine.repaint_for_mode(lighting_mode, config),
    );
}

fn reload_config(
    paths: &Paths,
    config: &mut AppConfig,
    engine: &mut Engine,
    hardware: &mut Hardware,
    flash: &mut FlashController,
    lighting_mode: &mut LightingModeController,
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
        hardware.reset_background(config, lighting_mode.mode());
        *engine = Engine::new(updated.behavior.max_sessions);
        hardware.disconnect();
    }
    *config = updated;
    lighting_mode.update_now(config)?;
    flash.reset();
    hardware.refresh(config, engine, lighting_mode.mode());
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

    fn connect(&mut self, config: &AppConfig, engine: &Engine, lighting_mode: LightingMode) {
        if self.keyboard.is_some() || !self.retry_due() {
            return;
        }

        match G915::connect(&config.device) {
            Ok((mut keyboard, summary)) => {
                let background = config.lighting.background_for_mode(lighting_mode);
                if let Err(error) = keyboard.set_background(background) {
                    self.schedule_retry(format!("G915 initialization failed: {error:#}"));
                    return;
                }
                let active = engine.active_lighting_for_mode(lighting_mode, config);
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

    fn apply(
        &mut self,
        config: &AppConfig,
        engine: &Engine,
        lighting_mode: LightingMode,
        changes: &[LightingChange],
    ) {
        if changes.is_empty() {
            return;
        }
        if self.keyboard.is_none() {
            self.connect(config, engine, lighting_mode);
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

    fn reassert_direct_lighting(
        &mut self,
        config: &AppConfig,
        engine: &Engine,
        lighting_mode: LightingMode,
    ) -> bool {
        let previous_error = self.last_error.clone();
        if self.keyboard.is_none() {
            self.connect(config, engine, lighting_mode);
            return self.last_error != previous_error;
        }

        let started_at = Instant::now();
        let keys: Vec<_> = engine
            .active_lighting_for_mode(lighting_mode, config)
            .iter()
            .map(|change| (change.key, change.color))
            .collect();
        let result = {
            let keyboard = self.keyboard.as_mut().expect("keyboard checked above");
            keyboard
                .reassert_direct_mode()
                .and_then(|()| {
                    keyboard.set_background(config.lighting.background_for_mode(lighting_mode))
                })
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

    fn refresh(&mut self, config: &AppConfig, engine: &Engine, lighting_mode: LightingMode) {
        if self.keyboard.is_none() {
            self.connect(config, engine, lighting_mode);
            return;
        }

        let result = self
            .keyboard
            .as_mut()
            .expect("keyboard checked above")
            .set_background(config.lighting.background_for_mode(lighting_mode));
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
        self.apply(
            config,
            engine,
            lighting_mode,
            &engine.active_lighting_for_mode(lighting_mode, config),
        );
    }

    fn reset_background(&mut self, config: &AppConfig, lighting_mode: LightingMode) {
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(error) =
                keyboard.set_background(config.lighting.background_for_mode(lighting_mode))
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
    lighting_mode: LightingMode,
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
    queued_sessions: Vec<RestoredQueuedSession>,
}

#[derive(Deserialize)]
struct RestorableStatus {
    #[serde(default)]
    slots: Vec<RestoredSlot>,
    #[serde(default)]
    queued_sessions: Vec<RestoredQueuedSession>,
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
        Ok(status) => {
            Engine::restore(max_sessions, status.slots, status.queued_sessions)
        }
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
        lighting_mode: config
            .lighting
            .mode_at_minute(local_minute_of_day()?),
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
        queued_sessions: engine.queued_snapshot(),
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

fn log_path_with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut suffixed = path.as_os_str().to_os_string();
    suffixed.push(suffix);
    suffixed.into()
}

fn rotate_log_file(path: &Path, max_bytes: u64, retained_files: usize) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()));
        }
    };
    if metadata.len() < max_bytes {
        return Ok(false);
    }

    if retained_files > 0 {
        let oldest = log_path_with_suffix(path, &format!(".{retained_files}"));
        if oldest.exists() {
            fs::remove_file(&oldest)
                .with_context(|| format!("failed to remove {}", oldest.display()))?;
        }
        for index in (1..retained_files).rev() {
            let source = log_path_with_suffix(path, &format!(".{index}"));
            if !source.exists() {
                continue;
            }
            let destination = log_path_with_suffix(path, &format!(".{}", index + 1));
            fs::rename(&source, &destination).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        }

        let temporary = log_path_with_suffix(path, ".rotate.tmp");
        if temporary.exists() {
            fs::remove_file(&temporary)
                .with_context(|| format!("failed to remove {}", temporary.display()))?;
        }
        fs::copy(path, &temporary)
            .with_context(|| format!("failed to archive {}", path.display()))?;
        let first_archive = log_path_with_suffix(path, ".1");
        fs::rename(&temporary, &first_archive).with_context(|| {
            format!(
                "failed to move {} to {}",
                temporary.display(),
                first_archive.display()
            )
        })?;
    }

    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {} for truncation", path.display()))?
        .set_len(0)
        .with_context(|| format!("failed to truncate {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::net::UnixDatagram;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::config::{AppConfig, LightingMode, Paths};
    use crate::journal::JournalTracker;
    use crate::state::{Engine, RestoredSlot, StateKind};
    use crate::wire::LifecycleTracker;

    use super::{
        EventLoopHealth, FlashController, Hardware, LightingModeController, LightingWatchdog,
        LogRotator, StatusPersistence, StatusPublisher, StatusWriteRequest,
        clear_removed_journal_sessions, prune_unadmitted_sessions, remove_stale_socket,
        restore_engine, rotate_log_file, write_status,
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
    fn rotates_a_launchd_log_without_breaking_its_open_append_handle() {
        let root = temporary_root("rotate-log");
        fs::create_dir_all(&root).unwrap();
        let log = root.join("codex-agent-indicator.log");
        let first_archive = log.with_extension("log.1");
        let second_archive = log.with_extension("log.2");
        let mut launchd_handle = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();

        launchd_handle.write_all(b"first-generation\n").unwrap();
        launchd_handle.flush().unwrap();
        assert!(rotate_log_file(&log, 8, 2).unwrap());
        assert_eq!(fs::read(&first_archive).unwrap(), b"first-generation\n");
        assert_eq!(fs::read(&log).unwrap(), b"");

        launchd_handle.write_all(b"second-generation\n").unwrap();
        launchd_handle.flush().unwrap();
        assert!(rotate_log_file(&log, 8, 2).unwrap());
        assert_eq!(fs::read(&first_archive).unwrap(), b"second-generation\n");
        assert_eq!(fs::read(&second_archive).unwrap(), b"first-generation\n");

        launchd_handle.write_all(b"still-live\n").unwrap();
        launchd_handle.flush().unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"still-live\n");

        assert!(rotate_log_file(&log, 8, 2).unwrap());
        assert_eq!(fs::read(&first_archive).unwrap(), b"still-live\n");
        assert_eq!(fs::read(&second_archive).unwrap(), b"second-generation\n");
        assert!(!log.with_extension("log.3").exists());

        launchd_handle.write_all(b"after-third-rotation\n").unwrap();
        launchd_handle.flush().unwrap();
        assert_eq!(fs::read(&log).unwrap(), b"after-third-rotation\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leaves_a_log_below_the_rotation_limit_untouched() {
        let root = temporary_root("small-log");
        fs::create_dir_all(&root).unwrap();
        let log = root.join("codex-agent-indicator.log");
        fs::write(&log, b"small\n").unwrap();

        assert!(!rotate_log_file(&log, 1024, 2).unwrap());
        assert_eq!(fs::read(&log).unwrap(), b"small\n");
        assert!(!log.with_extension("log.1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checks_log_size_only_when_the_rotation_interval_is_due() {
        let root = temporary_root("rotation-interval");
        fs::create_dir_all(&root).unwrap();
        let log = root.join("codex-agent-indicator.log");
        fs::write(&log, b"large-enough\n").unwrap();
        let started = Instant::now();
        let mut rotator = LogRotator::new_at(started, 8, 2);

        assert_eq!(
            rotator
                .rotate_if_due(started + Duration::from_secs(59), &log)
                .unwrap(),
            None
        );
        assert!(!log.with_extension("log.1").exists());
        assert_eq!(
            rotator
                .rotate_if_due(started + Duration::from_secs(60), &log)
                .unwrap(),
            Some(true)
        );
        assert!(log.with_extension("log.1").exists());
        fs::remove_dir_all(root).unwrap();
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
            Vec::new(),
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
    fn startup_prunes_unadmitted_waiting_tasks_before_promotion() {
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::new(1);
        engine.transition("visible", StateKind::Working, 10, &config);
        engine.transition("missing-ghost", StateKind::Working, 20, &config);
        engine.transition("desktop-waiting", StateKind::Working, 30, &config);

        prune_unadmitted_sessions(
            &mut engine,
            &HashSet::from([
                "visible".to_owned(),
                "desktop-waiting".to_owned(),
            ]),
            &config,
        );
        engine.clear_session("visible", &config);

        assert_eq!(
            engine.session_for_g_key(1),
            Some("desktop-waiting")
        );
    }

    #[test]
    fn startup_restores_waiting_tasks_without_remapping_visible_g_keys() {
        let root = temporary_root("restore-waiting");
        fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            config: root.join("config.toml"),
            codex_sessions: root.join("sessions"),
            log: root.join("codex-agent-indicator.log"),
            runtime_dir: root.clone(),
            socket: root.join("indicator.sock"),
            status: root.join("status.json"),
        };
        fs::write(
            &paths.status,
            serde_json::to_vec(&serde_json::json!({
                "slots": [{
                    "slot": 1,
                    "session_id": "visible",
                    "state": "working",
                    "updated_at": 10
                }],
                "queued_sessions": [{
                    "session_id": "waiting",
                    "state": "requested",
                    "updated_at": 20
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let config = AppConfig::default();

        let mut engine = restore_engine(&paths, 1);
        engine.clear_session("visible", &config);

        assert_eq!(engine.session_for_g_key(1), Some("waiting"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_persists_waiting_tasks_for_restart() {
        let root = temporary_root("persist-waiting");
        fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            config: root.join("config.toml"),
            codex_sessions: root.join("sessions"),
            log: root.join("codex-agent-indicator.log"),
            runtime_dir: root.clone(),
            socket: root.join("indicator.sock"),
            status: root.join("status.json"),
        };
        let mut config = AppConfig::default();
        config.behavior.max_sessions = 1;
        config.device.slot_keys.truncate(1);
        let mut engine = Engine::new(1);
        engine.transition("visible", StateKind::Working, 10, &config);
        engine.transition("waiting", StateKind::Requested, 20, &config);
        let hardware = Hardware::new();
        let journals = JournalTracker::new(paths.codex_sessions.clone());
        let publisher = StatusPublisher::new();

        write_status(
            &paths,
            &config,
            &engine,
            &hardware,
            &journals,
            &publisher,
        )
        .unwrap();

        let status: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.status).unwrap()).unwrap();
        assert!(matches!(status["lighting_mode"].as_str(), Some("day" | "night")));
        assert_eq!(
            status["queued_sessions"],
            serde_json::json!([{
                "session_id": "waiting",
                "state": "requested",
                "updated_at": 20
            }])
        );
        fs::remove_dir_all(root).unwrap();
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
                .frame_if_due_for_mode(
                    start + Duration::from_millis(499),
                    LightingMode::Day,
                    &config,
                    &engine,
                )
                .is_none()
        );

        let dim = flash
            .frame_if_due_for_mode(
                start + Duration::from_millis(500),
                LightingMode::Day,
                &config,
                &engine,
            )
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
            .frame_if_due_for_mode(
                start + Duration::from_millis(1_000),
                LightingMode::Day,
                &config,
                &engine,
            )
            .expect("bright frame");
        assert_eq!(bright[0].color, config.colors.approval);
    }

    #[test]
    fn night_mode_never_emits_a_flash_frame() {
        let config = AppConfig::default();
        let mut engine = Engine::new(config.behavior.max_sessions);
        engine.transition("task", StateKind::Working, 10, &config);
        let start = Instant::now();
        let mut flash = FlashController::new_at(start);

        assert!(
            flash
                .frame_if_due_for_mode(
                    start + Duration::from_secs(5),
                    LightingMode::Night,
                    &config,
                    &engine,
                )
                .is_none()
        );
    }

    #[test]
    fn lighting_mode_controller_changes_only_when_a_schedule_boundary_is_crossed() {
        let config = AppConfig::default();
        let mut controller = LightingModeController::new_at_minute(&config, 16 * 60 + 59);

        assert_eq!(controller.mode(), LightingMode::Day);
        assert!(!controller.update_at_minute(&config, 16 * 60 + 59));
        assert!(controller.update_at_minute(&config, 17 * 60));
        assert_eq!(controller.mode(), LightingMode::Night);
        assert!(!controller.update_at_minute(&config, 17 * 60 + 1));
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
            log: root.join("codex-agent-indicator.log"),
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
