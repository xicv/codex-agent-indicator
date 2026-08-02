use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::AppConfig;
use crate::state::StateKind;
use crate::wire::{message_tail, state_for_hook};

const JOURNAL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const RECOVERY_TAIL_BYTES: u64 = 8 * 1_024 * 1_024;
const SESSION_META_MAX_BYTES: u64 = 256 * 1_024;
const IGNORED_SESSION_CACHE_CAPACITY: usize = 256;

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

#[derive(Debug, Default)]
pub struct JournalRestore {
    pub admitted_sessions: HashSet<String>,
    pub transitions: Vec<SessionJournalTransition>,
}

#[derive(Debug, Default)]
pub struct JournalPoll {
    pub transitions: Vec<SessionJournalTransition>,
    pub removed_sessions: Vec<String>,
}

pub struct JournalReader {
    path: PathBuf,
    file: File,
    offset: u64,
    skip_partial_line: bool,
    active_turn: Option<String>,
    completed_turn: Option<String>,
    pending_tool_calls: HashSet<String>,
}

pub struct JournalTracker {
    sessions_root: PathBuf,
    readers: HashMap<String, JournalReader>,
    ignored_sessions: HashSet<String>,
    ignored_order: VecDeque<String>,
    next_poll: Instant,
    last_error: Option<String>,
}

#[derive(Deserialize)]
struct SessionMetaRecord {
    #[serde(rename = "type")]
    record_type: String,
    payload: SessionMetaPayload,
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    #[serde(default)]
    id: String,
    #[serde(default)]
    source: serde_json::Value,
    #[serde(default)]
    originator: String,
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
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    internal_chat_message_metadata_passthrough: JournalMessageMetadata,
    #[serde(default)]
    started_at: Option<u64>,
    #[serde(default)]
    completed_at: Option<u64>,
    #[serde(default)]
    last_agent_message: Option<String>,
}

#[derive(Default, Deserialize)]
struct JournalMessageMetadata {
    #[serde(default)]
    turn_id: Option<String>,
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
            pending_tool_calls: HashSet::new(),
        };
        let latest = reader.poll(config)?;
        Ok((reader, latest))
    }

    pub fn poll(&mut self, config: &AppConfig) -> Result<Option<JournalTransition>> {
        let metadata = self.file.metadata()?;
        let length = metadata.len();
        let journal_modified_at = metadata
            .modified()
            .unwrap_or_else(|_| SystemTime::now())
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if length < self.offset {
            self.offset = 0;
            self.skip_partial_line = false;
            self.active_turn = None;
            self.completed_turn = None;
            self.pending_tool_calls.clear();
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
            if let Some(transition) = self.observe(record, config, journal_modified_at) {
                latest = Some(transition);
            }
        }

        if self.pending_tool_calls.is_empty() {
            Ok(latest)
        } else {
            Ok(None)
        }
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
            pending_tool_calls: HashSet::new(),
        })
    }

    fn observe(
        &mut self,
        record: JournalRecord,
        config: &AppConfig,
        journal_modified_at: u64,
    ) -> Option<JournalTransition> {
        if record.record_type == "response_item" {
            let call_id = correlated_tool_call_id(&record.payload)?;
            return match record.payload.event_type.as_str() {
                "custom_tool_call" | "function_call" => {
                    self.pending_tool_calls.insert(call_id);
                    None
                }
                "custom_tool_call_output" | "function_call_output" => {
                    self.pending_tool_calls.remove(&call_id);
                    self.pending_tool_calls
                        .is_empty()
                        .then_some(JournalTransition {
                            state: config.events.post_tool_success,
                            occurred_at: journal_modified_at,
                        })
                }
                _ => None,
            };
        }

        if record.record_type != "event_msg" {
            return None;
        }

        let turn_id = record.payload.turn_id?;
        match record.payload.event_type.as_str() {
            "task_started" => {
                if self.completed_turn.as_deref() == Some(turn_id.as_str()) {
                    return None;
                }
                self.active_turn = Some(turn_id);
                self.completed_turn = None;
                self.pending_tool_calls.clear();
                Some(JournalTransition {
                    state: config.events.user_prompt_submit,
                    occurred_at: record.payload.started_at?,
                })
            }
            "task_complete"
                if self.active_turn.as_deref().is_none()
                    || self.active_turn.as_deref() == Some(turn_id.as_str()) =>
            {
                let last_message = record
                    .payload
                    .last_agent_message
                    .as_deref()
                    .map(message_tail);
                self.active_turn = None;
                self.completed_turn = Some(turn_id);
                self.pending_tool_calls.clear();
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

fn correlated_tool_call_id(payload: &JournalPayload) -> Option<String> {
    let call_id = payload.call_id.as_deref()?.trim();
    let turn_id = payload
        .internal_chat_message_metadata_passthrough
        .turn_id
        .as_deref()?
        .trim();
    if call_id.is_empty() || turn_id.is_empty() {
        return None;
    }
    Some(call_id.to_owned())
}

impl JournalTracker {
    pub fn new(sessions_root: PathBuf) -> Self {
        Self {
            sessions_root,
            readers: HashMap::new(),
            ignored_sessions: HashSet::new(),
            ignored_order: VecDeque::new(),
            next_poll: Instant::now() + JOURNAL_POLL_INTERVAL,
            last_error: None,
        }
    }

    pub fn restore<I, S>(
        &mut self,
        session_ids: I,
        config: &AppConfig,
    ) -> JournalRestore
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let targets = session_ids
            .into_iter()
            .map(|session_id| session_id.as_ref().to_owned())
            .collect::<HashSet<_>>();
        if targets.is_empty() {
            return JournalRestore::default();
        }

        let paths = match discover_transcripts(&self.sessions_root, &targets) {
            Ok(paths) => paths,
            Err(error) => {
                self.record_error(format!("{error:#}"));
                return JournalRestore::default();
            }
        };

        let mut restored = JournalRestore::default();
        let mut errors = Vec::new();
        for session_id in targets {
            let Some(path) = paths.get(&session_id) else {
                continue;
            };
            match is_top_level_codex_desktop_journal(path, &session_id) {
                Ok(true) => {
                    restored.admitted_sessions.insert(session_id.clone());
                }
                Ok(false) => {
                    self.remember_ignored(session_id);
                    continue;
                }
                Err(error) => {
                    errors.push(format!("{error:#}"));
                    continue;
                }
            }
            match JournalReader::recover(path, config) {
                Ok((reader, transition)) => {
                    self.readers.insert(session_id.clone(), reader);
                    if let Some(transition) = transition {
                        restored.transitions.push(SessionJournalTransition {
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

    pub fn register_live(&mut self, session_id: &str, path: &Path) -> Result<bool> {
        if self.ignored_sessions.contains(session_id) {
            return Ok(false);
        }
        let path = validate_transcript_path(&self.sessions_root, session_id, path)?;
        if self
            .readers
            .get(session_id)
            .is_some_and(|reader| reader.path == path)
        {
            return Ok(true);
        }
        if !is_top_level_codex_desktop_journal(&path, session_id)? {
            self.remember_ignored(session_id.to_owned());
            return Ok(false);
        }
        self.readers
            .insert(session_id.to_owned(), JournalReader::follow(&path)?);
        Ok(true)
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
    ) -> JournalPoll {
        if now < self.next_poll {
            return JournalPoll::default();
        }
        self.next_poll = now + JOURNAL_POLL_INTERVAL;

        let mut transitions = Vec::new();
        let mut removed_sessions = Vec::new();
        let mut errors = Vec::new();
        for (session_id, reader) in &mut self.readers {
            match fs::metadata(&reader.path) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    removed_sessions.push(session_id.clone());
                    continue;
                }
                Err(error) => {
                    errors.push(format!(
                        "failed to inspect Codex lifecycle journal {}: {error:#}",
                        reader.path.display()
                    ));
                    continue;
                }
            }
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
        for session_id in &removed_sessions {
            self.readers.remove(session_id);
        }
        self.finish_operation(errors);
        JournalPoll {
            transitions,
            removed_sessions,
        }
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
            self.ignored_sessions.remove(session_id);
            self.ignored_order.retain(|ignored| ignored != session_id);
        } else {
            self.readers.clear();
            self.ignored_sessions.clear();
            self.ignored_order.clear();
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

    fn remember_ignored(&mut self, session_id: String) {
        if !self.ignored_sessions.insert(session_id.clone()) {
            return;
        }
        self.ignored_order.push_back(session_id);
        while self.ignored_order.len() > IGNORED_SESSION_CACHE_CAPACITY {
            if let Some(expired) = self.ignored_order.pop_front() {
                self.ignored_sessions.remove(&expired);
            }
        }
    }
}

fn is_top_level_codex_desktop_journal(path: &Path, expected_session_id: &str) -> Result<bool> {
    let file = File::open(path)
        .with_context(|| format!("failed to inspect Codex session metadata {}", path.display()))?;
    let mut source = BufReader::new(file).take(SESSION_META_MAX_BYTES);
    let mut line = Vec::new();
    let bytes_read = source.read_until(b'\n', &mut line)?;
    if bytes_read == 0 {
        return Ok(false);
    }
    if !line.ends_with(b"\n") {
        bail!(
            "Codex session metadata exceeds {} bytes in {}",
            SESSION_META_MAX_BYTES,
            path.display()
        );
    }

    let record: SessionMetaRecord =
        serde_json::from_slice(&line).context("invalid Codex session metadata record")?;
    Ok(record.record_type == "session_meta"
        && record.payload.id == expected_session_id
        && record.payload.source.as_str() == Some("vscode")
        && record.payload.originator == "Codex Desktop")
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
    let lifecycle_event = contains_bytes(header, br#""type":"event_msg""#)
        && (contains_bytes(payload_prefix, br#""type":"task_started""#)
            || contains_bytes(payload_prefix, br#""type":"task_complete""#));
    let tool_lifecycle = contains_bytes(header, br#""type":"response_item""#)
        && [
            br#""type":"custom_tool_call""#.as_slice(),
            br#""type":"custom_tool_call_output""#.as_slice(),
            br#""type":"function_call""#.as_slice(),
            br#""type":"function_call_output""#.as_slice(),
        ]
        .into_iter()
        .any(|event_type| contains_bytes(payload_prefix, event_type));
    lifecycle_event || tool_lifecycle
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
    use std::collections::HashSet;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::config::AppConfig;
    use crate::state::StateKind;

    use super::{JournalReader, JournalTracker};

    const LIFECYCLE: &str =
        include_str!("journal/fixtures/codex-app-0.146.0-alpha.3.1/lifecycle.jsonl");
    const APPROVAL_RESUME: &str = include_str!(
        "journal/fixtures/codex-app-0.146.0-alpha.9.2/approval-resume.jsonl"
    );
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
    fn completed_tool_output_repairs_a_missed_approval_resume_on_startup() {
        let path = temporary_journal(APPROVAL_RESUME);
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert_eq!(latest.unwrap().state, StateKind::Working);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn appended_tool_output_repairs_a_missed_approval_resume_once() {
        let (pending_call, completed_calls) = APPROVAL_RESUME
            .split_once('\n')
            .expect("fixture contains a pending call and its completion");
        let path = temporary_journal(&format!("{pending_call}\n"));
        let config = AppConfig::default();
        let (mut reader, latest) = JournalReader::recover(&path, &config).unwrap();
        assert!(latest.is_none());

        append(&path, completed_calls);
        let transition = reader.poll(&config).unwrap().unwrap();

        assert_eq!(transition.state, StateKind::Working);
        assert!(reader.poll(&config).unwrap().is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn pending_tool_call_does_not_clear_a_real_approval() {
        let pending_call = APPROVAL_RESUME
            .lines()
            .next()
            .expect("fixture begins with the approval-gated call");
        let path = temporary_journal(&format!("{pending_call}\n"));
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert!(latest.is_none());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn one_completed_call_does_not_clear_another_pending_approval() {
        let records = APPROVAL_RESUME.lines().collect::<Vec<_>>();
        let path = temporary_journal(&format!("{}\n{}\n", records[0], records[2]));
        let config = AppConfig::default();
        let (mut reader, latest) = JournalReader::recover(&path, &config).unwrap();
        assert!(latest.is_none());

        append(&path, &format!("{}\n", records[1]));
        assert!(reader.poll(&config).unwrap().is_none());

        append(&path, &format!("{}\n", records[3]));
        assert_eq!(
            reader.poll(&config).unwrap().unwrap().state,
            StateKind::Working
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn newer_pending_call_suppresses_an_older_resume_during_recovery() {
        let records = APPROVAL_RESUME.lines().collect::<Vec<_>>();
        let path = temporary_journal(&format!(
            "{}\n{}\n{}\n",
            records[0], records[1], records[2]
        ));
        let config = AppConfig::default();

        let (_, latest) = JournalReader::recover(&path, &config).unwrap();

        assert!(latest.is_none());
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
        for fixture in [LIFECYCLE, APPROVAL_RESUME] {
            for forbidden in ["/Users/", "/home/", "file://", "github_pat_", "PRIVATE KEY"] {
                assert!(!fixture.contains(forbidden));
            }
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
        fs::write(&path, desktop_journal("session-a")).unwrap();
        fs::write(
            nested.join("rollout-scrubbed-untracked-session.jsonl"),
            desktop_journal("untracked-session"),
        )
        .unwrap();
        let config = AppConfig::default();
        let mut tracker = JournalTracker::new(root.clone());

        let restored = tracker.restore(["session-a"], &config);

        assert_eq!(restored.transitions.len(), 1);
        assert_eq!(restored.transitions[0].session_id, "session-a");
        assert_eq!(restored.transitions[0].state, StateKind::Working);
        assert_eq!(
            restored.admitted_sessions,
            HashSet::from(["session-a".to_owned()])
        );
        assert_eq!(tracker.source_count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_registration_admits_only_top_level_codex_desktop_journals() {
        let root = temporary_directory("origin-root");
        let app = named_journal(
            &root,
            "app-session",
            &desktop_journal("app-session"),
        );
        let cli = named_journal(
            &root,
            "cli-session",
            &journal_with_origin("cli-session", json!("cli"), "codex-tui"),
        );
        let claude = named_journal(
            &root,
            "claude-session",
            &journal_with_origin("claude-session", json!("vscode"), "Claude Code"),
        );
        let subagent = named_journal(
            &root,
            "subagent-session",
            &journal_with_origin(
                "subagent-session",
                json!({"subagent": {"thread_spawn": {"parent_thread_id": "parent"}}}),
                "Codex Desktop",
            ),
        );
        let mut tracker = JournalTracker::new(root.clone());

        assert!(tracker.register_live("app-session", &app).unwrap());
        assert!(!tracker.register_live("cli-session", &cli).unwrap());
        assert!(!tracker.register_live("claude-session", &claude).unwrap());
        assert!(!tracker.register_live("subagent-session", &subagent).unwrap());
        assert_eq!(tracker.source_count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn polling_removes_a_session_moved_out_of_active_sessions() {
        let root = temporary_directory("active-root");
        let archive = temporary_directory("archive-root");
        let path = named_journal(
            &root,
            "archived-session",
            &desktop_journal("archived-session"),
        );
        let mut tracker = JournalTracker::new(root.clone());
        let config = AppConfig::default();
        assert!(
            tracker
                .register_live("archived-session", &path)
                .unwrap()
        );

        fs::rename(
            &path,
            archive.join(path.file_name().expect("journal file name")),
        )
        .unwrap();
        let poll = tracker.poll_if_due(Instant::now() + Duration::from_secs(1), &config);

        assert!(poll.transitions.is_empty());
        assert_eq!(poll.removed_sessions, ["archived-session"]);
        assert_eq!(tracker.source_count(), 0);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(archive).unwrap();
    }

    #[test]
    fn live_registration_fails_closed_without_matching_session_metadata() {
        let root = temporary_directory("metadata-root");
        let missing_metadata = named_journal(&root, "missing-meta", LIFECYCLE);
        let mismatched_metadata = named_journal(
            &root,
            "expected-session",
            &desktop_journal("different-session"),
        );
        let mut tracker = JournalTracker::new(root.clone());

        assert!(
            !tracker
                .register_live("missing-meta", &missing_metadata)
                .unwrap()
        );
        assert!(
            !tracker
                .register_live("expected-session", &mismatched_metadata)
                .unwrap()
        );
        assert_eq!(tracker.source_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_restore_reports_only_reopenable_codex_desktop_sessions() {
        let root = temporary_directory("restore-origin-root");
        named_journal(&root, "app-session", &desktop_journal("app-session"));
        named_journal(
            &root,
            "cli-session",
            &journal_with_origin("cli-session", json!("cli"), "codex-tui"),
        );
        let config = AppConfig::default();
        let mut tracker = JournalTracker::new(root.clone());

        let restored =
            tracker.restore(["app-session", "cli-session", "missing-session"], &config);

        assert_eq!(
            restored.admitted_sessions,
            HashSet::from(["app-session".to_owned()])
        );
        assert_eq!(restored.transitions.len(), 1);
        assert_eq!(restored.transitions[0].session_id, "app-session");
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

    fn desktop_journal(session_id: &str) -> String {
        journal_with_origin(session_id, json!("vscode"), "Codex Desktop")
    }

    fn journal_with_origin(
        session_id: &str,
        source: serde_json::Value,
        originator: &str,
    ) -> String {
        format!(
            "{}\n{LIFECYCLE}",
            serde_json::to_string(&json!({
                "timestamp": "2026-07-28T00:00:00.000Z",
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "source": source,
                    "originator": originator,
                    "cli_version": "0.146.0-alpha.3.1",
                    "cwd": "/workspace"
                }
            }))
            .unwrap()
        )
    }

    fn named_journal(
        root: &std::path::Path,
        session_id: &str,
        content: &str,
    ) -> std::path::PathBuf {
        let path = root.join(format!("rollout-scrubbed-{session_id}.jsonl"));
        fs::write(&path, content).unwrap();
        path
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
