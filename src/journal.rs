use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::AppConfig;
use crate::state::StateKind;
use crate::wire::{message_tail, state_for_hook};

const JOURNAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RECOVERY_TAIL_BYTES: u64 = 8 * 1_024 * 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalTransition {
    pub state: StateKind,
    pub occurred_at: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionJournalTransition {
    pub session_id: String,
    pub state: StateKind,
    pub occurred_at: u64,
}

pub struct JournalReader {
    path: PathBuf,
    file: File,
    offset: u64,
    skip_partial_line: bool,
    active_turn: Option<String>,
    completed_turn: Option<String>,
}

pub struct JournalTracker {
    sessions_root: PathBuf,
    readers: HashMap<String, JournalReader>,
    next_poll: Instant,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct JournalRecord {
    #[serde(rename = "type")]
    record_type: String,
    payload: JournalPayload,
}

#[derive(Deserialize)]
struct JournalPayload {
    #[serde(rename = "type")]
    event_type: String,
    turn_id: String,
    #[serde(default)]
    started_at: Option<u64>,
    #[serde(default)]
    completed_at: Option<u64>,
    #[serde(default)]
    last_agent_message: Option<String>,
}

impl JournalReader {
    pub fn recover(path: &Path, config: &AppConfig) -> Result<(Self, Option<JournalTransition>)> {
        let file = File::open(path)
            .with_context(|| format!("failed to open Codex lifecycle journal {}", path.display()))?;
        let length = file.metadata()?.len();
        let offset = length.saturating_sub(RECOVERY_TAIL_BYTES);
        let skip_partial_line = if offset == 0 {
            false
        } else {
            let mut probe = file.try_clone()?;
            probe.seek(SeekFrom::Start(offset - 1))?;
            let mut previous = [0_u8; 1];
            probe.read_exact(&mut previous)?;
            previous[0] != b'\n'
        };
        let mut reader = Self {
            path: path.to_path_buf(),
            file,
            offset,
            skip_partial_line,
            active_turn: None,
            completed_turn: None,
        };
        let latest = reader.poll(config)?;
        Ok((reader, latest))
    }

    pub fn poll(&mut self, config: &AppConfig) -> Result<Option<JournalTransition>> {
        let length = self.file.metadata()?.len();
        if length < self.offset {
            self.offset = 0;
            self.skip_partial_line = false;
            self.active_turn = None;
            self.completed_turn = None;
        }
        if length == self.offset {
            return Ok(None);
        }

        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut source = BufReader::new(file);
        let mut line = Vec::new();
        let mut latest = None;

        loop {
            line.clear();
            let line_start = self.offset;
            let bytes_read = source.read_until(b'\n', &mut line)?;
            if bytes_read == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                self.offset = line_start;
                break;
            }
            self.offset += bytes_read as u64;
            if self.skip_partial_line {
                self.skip_partial_line = false;
                continue;
            }

            if !is_lifecycle_candidate(&line) {
                continue;
            }
            let record: JournalRecord = serde_json::from_slice(&line)
                .context("invalid Codex lifecycle journal record")?;
            if let Some(transition) = self.observe(record, config) {
                latest = Some(transition);
            }
        }

        Ok(latest)
    }

    fn follow(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open Codex lifecycle journal {}", path.display()))?;
        let offset = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offset,
            skip_partial_line: false,
            active_turn: None,
            completed_turn: None,
        })
    }

    fn observe(
        &mut self,
        record: JournalRecord,
        config: &AppConfig,
    ) -> Option<JournalTransition> {
        if record.record_type != "event_msg" {
            return None;
        }

        match record.payload.event_type.as_str() {
            "task_started" => {
                if self.completed_turn.as_deref() == Some(record.payload.turn_id.as_str()) {
                    return None;
                }
                self.active_turn = Some(record.payload.turn_id);
                self.completed_turn = None;
                Some(JournalTransition {
                    state: config.events.user_prompt_submit,
                    occurred_at: record.payload.started_at?,
                })
            }
            "task_complete"
                if self.active_turn.as_deref().is_none()
                    || self.active_turn.as_deref() == Some(record.payload.turn_id.as_str()) =>
            {
                let last_message = record
                    .payload
                    .last_agent_message
                    .as_deref()
                    .map(message_tail);
                self.active_turn = None;
                self.completed_turn = Some(record.payload.turn_id);
                Some(JournalTransition {
                    state: state_for_hook(
                        "Stop",
                        last_message.as_deref(),
                        false,
                        config,
                    )
                    .unwrap_or(config.events.stop_complete),
                    occurred_at: record.payload.completed_at?,
                })
            }
            _ => None,
        }
    }
}

impl JournalTracker {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            readers: HashMap::new(),
            next_poll: Instant::now() + JOURNAL_POLL_INTERVAL,
            last_error: None,
        }
    }

    pub fn restore<I, S>(
        &mut self,
        session_ids: I,
        config: &AppConfig,
    ) -> Vec<SessionJournalTransition>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let targets = session_ids
            .into_iter()
            .map(|session_id| session_id.as_ref().to_owned())
            .collect::<HashSet<_>>();
        if targets.is_empty() {
            return Vec::new();
        }

        let paths = match discover_transcripts(&self.sessions_root, &targets) {
            Ok(paths) => paths,
            Err(error) => {
                self.record_error(format!("{error:#}"));
                return Vec::new();
            }
        };

        let mut restored = Vec::new();
        let mut errors = Vec::new();
        for session_id in targets {
            let Some(path) = paths.get(&session_id) else {
                continue;
            };
            match JournalReader::recover(path, config) {
                Ok((reader, transition)) => {
                    self.readers.insert(session_id.clone(), reader);
                    if let Some(transition) = transition {
                        restored.push(SessionJournalTransition {
                            session_id,
                            state: transition.state,
                            occurred_at: transition.occurred_at,
                        });
                    }
                }
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        self.finish_operation(errors);
        restored
    }

    pub fn register_live(&mut self, session_id: &str, path: &Path) -> Result<()> {
        let path = validate_transcript_path(&self.sessions_root, session_id, path)?;
        if self
            .readers
            .get(session_id)
            .is_some_and(|reader| reader.path == path)
        {
            return Ok(());
        }
        self.readers
            .insert(session_id.to_owned(), JournalReader::follow(&path)?);
        Ok(())
    }

    pub fn poll_interval(&self, now: Instant) -> Duration {
        self.next_poll
            .saturating_duration_since(now)
            .clamp(Duration::from_millis(1), JOURNAL_POLL_INTERVAL)
    }

    pub fn poll_if_due(
        &mut self,
        now: Instant,
        config: &AppConfig,
    ) -> Vec<SessionJournalTransition> {
        if now < self.next_poll {
            return Vec::new();
        }
        self.next_poll = now + JOURNAL_POLL_INTERVAL;

        let mut transitions = Vec::new();
        let mut errors = Vec::new();
        for (session_id, reader) in &mut self.readers {
            match reader.poll(config) {
                Ok(Some(transition)) => transitions.push(SessionJournalTransition {
                    session_id: session_id.clone(),
                    state: transition.state,
                    occurred_at: transition.occurred_at,
                }),
                Ok(None) => {}
                Err(error) => errors.push(format!(
                    "failed to reconcile Codex lifecycle journal {}: {error:#}",
                    reader.path.display()
                )),
            }
        }
        self.finish_operation(errors);
        transitions
    }

    pub fn retain_sessions<I, S>(&mut self, session_ids: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let keep = session_ids
            .into_iter()
            .map(|session_id| session_id.as_ref().to_owned())
            .collect::<HashSet<_>>();
        self.readers
            .retain(|session_id, _| keep.contains(session_id));
    }

    pub fn clear(&mut self, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            self.readers.remove(session_id);
        } else {
            self.readers.clear();
        }
    }

    pub fn source_count(&self) -> usize {
        self.readers.len()
    }

    pub fn last_error(&self) -> &Option<String> {
        &self.last_error
    }

    pub fn record_error(&mut self, error: String) {
        if self.last_error.as_deref() != Some(error.as_str()) {
            eprintln!("Codex lifecycle reconciliation warning: {error}");
        }
        self.last_error = Some(error);
    }

    fn finish_operation(&mut self, errors: Vec<String>) {
        if errors.is_empty() {
            self.last_error = None;
        } else {
            self.record_error(errors.join("; "));
        }
    }
}

fn validate_transcript_path(
    sessions_root: &Path,
    session_id: &str,
    path: &Path,
) -> Result<PathBuf> {
    let root = fs::canonicalize(sessions_root).with_context(|| {
        format!(
            "failed to resolve Codex sessions directory {}",
            sessions_root.display()
        )
    })?;
    let path = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve transcript path {}", path.display()))?;
    if !path.starts_with(&root) {
        bail!(
            "transcript path {} is outside the Codex sessions directory",
            path.display()
        );
    }
    let expected_suffix = format!("-{session_id}.jsonl");
    let matches_session = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(&expected_suffix));
    if !matches_session {
        bail!(
            "transcript path {} does not match session {session_id}",
            path.display()
        );
    }
    Ok(path)
}

fn discover_transcripts(
    sessions_root: &Path,
    targets: &HashSet<String>,
) -> Result<HashMap<String, PathBuf>> {
    if !sessions_root.exists() {
        return Ok(HashMap::new());
    }

    let mut found = HashMap::<String, (SystemTime, PathBuf)>::new();
    let mut directories = vec![sessions_root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("failed to inspect {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(session_id) = targets
                .iter()
                .find(|session_id| name.ends_with(&format!("-{session_id}.jsonl")))
            else {
                continue;
            };
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let replace = found
                .get(session_id)
                .is_none_or(|(current, _)| modified > *current);
            if replace {
                found.insert(session_id.clone(), (modified, entry.path()));
            }
        }
    }

    Ok(found
        .into_iter()
        .map(|(session_id, (_, path))| (session_id, path))
        .collect())
}

fn is_lifecycle_candidate(line: &[u8]) -> bool {
    let Some(payload_start) = find_bytes(line, br#","payload":"#) else {
        return false;
    };
    let header = &line[..payload_start];
    let payload_end = line.len().min(payload_start + 256);
    let payload_prefix = &line[payload_start..payload_end];
    contains_bytes(header, br#""type":"event_msg""#)
        && (contains_bytes(payload_prefix, br#""type":"task_started""#)
            || contains_bytes(payload_prefix, br#""type":"task_complete""#))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use crate::config::AppConfig;
    use crate::state::StateKind;

    use super::{JournalReader, JournalTracker};

    const LIFECYCLE: &str =
        include_str!("journal/fixtures/codex-app-0.146.0-alpha.3.1/lifecycle.jsonl");
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn replay_uses_the_newest_native_turn_instead_of_an_older_completion() {
        let path = temporary_journal(LIFECYCLE);
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert_eq!(latest.unwrap().state, StateKind::Working);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn appended_native_completion_reconciles_a_missed_stop_hook_once() {
        let path = temporary_journal(LIFECYCLE);
        let config = AppConfig::default();
        let (mut reader, _) = JournalReader::recover(&path, &config).unwrap();
        let completion = concat!(
            "{\"timestamp\":\"2026-07-28T00:00:30.000Z\",",
            "\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",",
            "\"turn_id\":\"turn-current\",\"last_agent_message\":\"All done.\",",
            "\"started_at\":120,\"completed_at\":130,\"duration_ms\":10000,",
            "\"time_to_first_token_ms\":100}}\n"
        );
        append(&path, completion);

        let transition = reader.poll(&config).unwrap().unwrap();

        assert_eq!(transition.state, StateKind::Done);
        assert_eq!(transition.occurred_at, 130);
        assert!(reader.poll(&config).unwrap().is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn incomplete_jsonl_record_waits_for_its_newline_before_reconciliation() {
        let path = temporary_journal(LIFECYCLE);
        let config = AppConfig::default();
        let (mut reader, _) = JournalReader::recover(&path, &config).unwrap();
        let completion = concat!(
            "{\"timestamp\":\"2026-07-28T00:00:30.000Z\",",
            "\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",",
            "\"turn_id\":\"turn-current\",\"last_agent_message\":\"All done.\",",
            "\"started_at\":120,\"completed_at\":130}}\n"
        );
        let split = completion.len() - 1;
        append(&path, &completion[..split]);

        assert!(reader.poll(&config).unwrap().is_none());

        append(&path, &completion[split..]);
        assert_eq!(
            reader.poll(&config).unwrap().unwrap().state,
            StateKind::Done
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn native_fixture_is_privacy_scrubbed() {
        for forbidden in ["/Users/", "/home/", "file://", "github_pat_", "PRIVATE KEY"] {
            assert!(!LIFECYCLE.contains(forbidden));
        }
    }

    #[test]
    fn lifecycle_shaped_tool_output_is_not_mistaken_for_a_top_level_event() {
        let content = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "timestamp": "2026-07-28T00:00:00.000Z",
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call_output",
                    "output": {
                        "type": "event_msg",
                        "payload": {"type": "task_complete"}
                    }
                }
            }))
            .unwrap()
        );
        let path = temporary_journal(&content);
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert!(latest.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn startup_recovery_reads_only_a_bounded_recent_tail() {
        let mut content = LIFECYCLE.to_owned();
        content.push_str(
            "{\"timestamp\":\"2026-07-28T00:00:30.000Z\",\"type\":\"event_msg\",\
             \"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-current\",\
             \"last_agent_message\":\"All done.\",\"started_at\":120,\
             \"completed_at\":130}}\n",
        );
        content.push_str(&format!(
            "{{\"timestamp\":\"2026-07-28T00:00:31.000Z\",\"type\":\"response_item\",\
             \"payload\":{{\"type\":\"function_call_output\",\"output\":\"{}\"}}}}\n",
            "x".repeat(8 * 1_024 * 1_024)
        ));
        let path = temporary_journal(&content);
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert!(latest.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn restore_discovers_only_the_requested_working_session() {
        let root = temporary_directory("restore-root");
        let nested = root.join("2026").join("07").join("28");
        fs::create_dir_all(&nested).unwrap();
        let path = nested.join("rollout-scrubbed-session-a.jsonl");
        fs::write(&path, LIFECYCLE).unwrap();
        fs::write(
            nested.join("rollout-scrubbed-untracked-session.jsonl"),
            LIFECYCLE,
        )
        .unwrap();
        let config = AppConfig::default();
        let mut tracker = JournalTracker::new(root.clone());

        let restored = tracker.restore(["session-a"], &config);

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].session_id, "session-a");
        assert_eq!(restored[0].state, StateKind::Working);
        assert_eq!(tracker.source_count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_registration_rejects_a_journal_outside_the_codex_sessions_root() {
        let root = temporary_directory("trusted-root");
        let outside = temporary_directory("outside-root");
        let path = outside.join("rollout-scrubbed-session-a.jsonl");
        fs::write(&path, LIFECYCLE).unwrap();
        let mut tracker = JournalTracker::new(root.clone());

        let error = tracker.register_live("session-a", &path).unwrap_err();

        assert!(error.to_string().contains("outside the Codex sessions directory"));
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    fn temporary_journal(content: &str) -> std::path::PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "codex-agent-indicator-journal-{}-{sequence}.jsonl",
            std::process::id()
        ));
        fs::write(&path, content).unwrap();
        path
    }

    fn temporary_directory(label: &str) -> std::path::PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "codex-agent-indicator-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn append(path: &std::path::Path, content: &str) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
    }
}
