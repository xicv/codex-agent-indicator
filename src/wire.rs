use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::AppConfig;
use crate::state::StateKind;

const MAX_MESSAGE_TAIL_BYTES: usize = 2_048;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventMessage {
    Hook {
        session_id: String,
        run_id: String,
        hook_event_name: String,
        is_subagent: bool,
        tool_name: Option<String>,
        last_assistant_message: Option<String>,
        tool_failed: bool,
        transcript_path: Option<String>,
    },
    Set {
        session_id: String,
        state: StateKind,
    },
    Clear {
        session_id: Option<String>,
    },
    Reload,
    Snapshot,
}

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub tool_response: Option<Value>,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

impl HookInput {
    pub fn into_event(self) -> EventMessage {
        let is_subagent = matches!(
            self.hook_event_name.as_str(),
            "SubagentStart" | "SubagentStop"
        ) || non_empty(self.agent_id.as_deref()).is_some()
            || non_empty(self.agent_type.as_deref()).is_some();
        let run_id = if is_subagent {
            non_empty(self.agent_id.as_deref())
                .or_else(|| non_empty(self.turn_id.as_deref()))
        } else {
            non_empty(self.turn_id.as_deref())
                .or_else(|| non_empty(self.agent_id.as_deref()))
        }
            .unwrap_or(&self.session_id)
            .to_owned();
        EventMessage::Hook {
            session_id: self.session_id,
            run_id,
            hook_event_name: self.hook_event_name,
            is_subagent,
            tool_name: self.tool_name,
            last_assistant_message: self
                .last_assistant_message
                .as_deref()
                .map(message_tail),
            tool_failed: self.tool_response.as_ref().is_some_and(tool_failed),
            transcript_path: self.transcript_path,
        }
    }
}

#[derive(Debug, Default)]
pub struct LifecycleTracker {
    sessions: HashMap<String, SessionActivity>,
}

#[derive(Debug, Default)]
struct SessionActivity {
    active_runs: BTreeSet<String>,
    waiting_runs: BTreeMap<String, StateKind>,
}

impl LifecycleTracker {
    pub fn state_for_event(
        &mut self,
        event: &EventMessage,
        config: &AppConfig,
    ) -> Option<StateKind> {
        let EventMessage::Hook {
            session_id,
            run_id,
            hook_event_name,
            is_subagent,
            tool_name,
            last_assistant_message,
            tool_failed,
            ..
        } = event
        else {
            return None;
        };

        if hook_event_name == "Stop" && !is_subagent {
            self.sessions.remove(session_id);
            return state_for_hook(
                hook_event_name,
                last_assistant_message.as_deref(),
                *tool_failed,
                config,
            );
        }
        if hook_event_name == "SessionEnd" {
            self.sessions.remove(session_id);
            return None;
        }

        match hook_event_name.as_str() {
            "UserPromptSubmit" if !is_subagent => {
                let activity = self.sessions.entry(session_id.clone()).or_default();
                activity.active_runs.clear();
                activity.waiting_runs.clear();
                activity.active_runs.insert(run_id.clone());
                Some(config.events.user_prompt_submit)
            }
            "PermissionRequest" => {
                let activity = self.sessions.entry(session_id.clone()).or_default();
                activity.active_runs.insert(run_id.clone());
                activity
                    .waiting_runs
                    .insert(run_id.clone(), config.events.permission_request);
                Some(activity.state_or(config.events.permission_request))
            }
            "PreToolUse" => {
                let activity = self.sessions.entry(session_id.clone()).or_default();
                activity.active_runs.insert(run_id.clone());
                if tool_name.as_deref().is_some_and(tool_requests_user_input) {
                    activity
                        .waiting_runs
                        .insert(run_id.clone(), config.events.stop_question);
                } else {
                    activity.waiting_runs.remove(run_id);
                }
                Some(activity.state_or(config.events.user_prompt_submit))
            }
            "PostToolUse" => {
                let activity = self.sessions.entry(session_id.clone()).or_default();
                activity.active_runs.insert(run_id.clone());
                activity.waiting_runs.remove(run_id);
                let fallback = if *tool_failed {
                    config.events.post_tool_failure
                } else {
                    config.events.post_tool_success
                };
                Some(activity.state_or(fallback))
            }
            "SubagentStart" => {
                let activity = self.sessions.entry(session_id.clone()).or_default();
                activity.active_runs.insert(run_id.clone());
                Some(activity.state_or(config.events.user_prompt_submit))
            }
            "SubagentStop" | "Stop" if *is_subagent => {
                let activity = self.sessions.get_mut(session_id)?;
                activity.active_runs.remove(run_id);
                activity.waiting_runs.remove(run_id);
                let state = activity.state_or(config.events.user_prompt_submit);
                if activity.active_runs.is_empty() && activity.waiting_runs.is_empty() {
                    self.sessions.remove(session_id);
                }
                Some(state)
            }
            _ => state_for_hook(
                hook_event_name,
                last_assistant_message.as_deref(),
                *tool_failed,
                config,
            ),
        }
    }

    pub fn clear(&mut self, session_id: Option<&str>) {
        if let Some(session_id) = session_id {
            self.sessions.remove(session_id);
        } else {
            self.sessions.clear();
        }
    }
}

impl SessionActivity {
    fn state_or(&self, fallback: StateKind) -> StateKind {
        if self.waiting_runs.values().any(|state| *state == StateKind::Approval) {
            StateKind::Approval
        } else if self
            .waiting_runs
            .values()
            .any(|state| *state == StateKind::Requested)
        {
            StateKind::Requested
        } else {
            self.waiting_runs
                .values()
                .next()
                .copied()
                .unwrap_or(fallback)
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

fn tool_requests_user_input(tool_name: &str) -> bool {
    tool_name
        .to_ascii_lowercase()
        .ends_with("request_user_input")
}

pub fn state_for_hook(
    event_name: &str,
    last_assistant_message: Option<&str>,
    failed: bool,
    config: &AppConfig,
) -> Option<StateKind> {
    match event_name {
        "UserPromptSubmit" => Some(config.events.user_prompt_submit),
        "PermissionRequest" => Some(config.events.permission_request),
        "PostToolUse" if failed => Some(config.events.post_tool_failure),
        "PostToolUse" => Some(config.events.post_tool_success),
        "Stop"
            if last_assistant_message
                .is_some_and(assistant_message_reports_failure) =>
        {
            Some(config.events.stop_failure)
        }
        "Stop"
            if config.behavior.detect_questions
                && last_assistant_message.is_some_and(assistant_message_requests_input) =>
        {
            Some(config.events.stop_question)
        }
        "Stop" => Some(config.events.stop_complete),
        _ => None,
    }
}

pub fn assistant_message_requests_input(message: &str) -> bool {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return false;
    }

    let final_paragraph = trimmed
        .rsplit("\n\n")
        .find(|paragraph| !paragraph.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();
    if final_paragraph.ends_with('?') {
        return true;
    }

    let lowercase = final_paragraph.to_ascii_lowercase();
    [
        "please provide",
        "please choose",
        "please confirm",
        "i need your input",
        "i need you to",
        "waiting for your",
        "let me know which",
        "tell me which",
    ]
    .iter()
    .any(|phrase| lowercase.contains(phrase))
}

pub fn assistant_message_reports_failure(message: &str) -> bool {
    let lowercase = message.trim().to_ascii_lowercase();
    [
        "i couldn't complete",
        "i could not complete",
        "i wasn't able to complete",
        "i was unable to complete",
        "the task is blocked",
        "implementation failed",
        "build failed and",
    ]
    .iter()
    .any(|phrase| lowercase.contains(phrase))
}

pub(crate) fn message_tail(message: &str) -> String {
    if message.len() <= MAX_MESSAGE_TAIL_BYTES {
        return message.to_string();
    }

    let mut start = message.len() - MAX_MESSAGE_TAIL_BYTES;
    while !message.is_char_boundary(start) {
        start += 1;
    }
    message[start..].to_string()
}

fn tool_failed(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(tool_failed),
        Value::Object(object) => {
            object.get("is_error") == Some(&Value::Bool(true))
                || object.get("success") == Some(&Value::Bool(false))
                || object
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0)
                || object
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| {
                        matches!(
                            status.to_ascii_lowercase().as_str(),
                            "error" | "failed" | "failure"
                        )
                    })
                || object.values().any(tool_failed)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

#[cfg(test)]
mod replay_tests;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::AppConfig;

    use super::{
        EventMessage, HookInput, LifecycleTracker, assistant_message_reports_failure,
        assistant_message_requests_input, state_for_hook, tool_failed,
    };
    use crate::state::StateKind;

    fn hook(run_id: &str, event: &str, is_subagent: bool) -> EventMessage {
        EventMessage::Hook {
            session_id: "task".to_owned(),
            run_id: run_id.to_owned(),
            hook_event_name: event.to_owned(),
            is_subagent,
            tool_name: None,
            last_assistant_message: None,
            tool_failed: false,
            transcript_path: None,
        }
    }

    #[test]
    fn recognizes_input_request_without_transcript_scraping() {
        assert!(assistant_message_requests_input(
            "I found two devices. Which one should I use?"
        ));
        assert!(assistant_message_requests_input(
            "The setup is ready.\n\nPlease confirm the final colour."
        ));
        assert!(!assistant_message_requests_input(
            "Installed and verified successfully."
        ));
    }

    #[test]
    fn recognizes_clear_failure_language() {
        assert!(assistant_message_reports_failure(
            "I couldn't complete the hardware test because the keyboard is disconnected."
        ));
        assert!(!assistant_message_reports_failure(
            "Two expected failure-path tests passed."
        ));
    }

    #[test]
    fn recognizes_structured_tool_failure() {
        assert!(tool_failed(&json!({"exit_code": 1, "output": "bad"})));
        assert!(tool_failed(&json!({"content": [{"is_error": true}]})));
        assert!(!tool_failed(&json!({"exit_code": 0, "output": "ok"})));
    }

    #[test]
    fn maps_stop_to_requested_or_done() {
        let config = AppConfig::default();
        assert_eq!(
            state_for_hook("Stop", Some("Choose A or B?"), false, &config),
            Some(StateKind::Requested)
        );
        assert_eq!(
            state_for_hook("Stop", Some("All done."), false, &config),
            Some(StateKind::Done)
        );
    }

    #[test]
    fn extracts_turn_and_subagent_identity_without_transcript_content() {
        let input: HookInput = serde_json::from_value(json!({
            "session_id": "task",
            "turn_id": "child-turn",
            "hook_event_name": "SubagentStart",
            "agent_id": "child",
            "agent_type": "worker",
            "tool_name": "exec"
        }))
        .unwrap();

        assert!(matches!(
            input.into_event(),
            EventMessage::Hook {
                run_id,
                is_subagent: true,
                tool_name: Some(tool_name),
                ..
            } if run_id == "child" && tool_name == "exec"
        ));
    }

    #[test]
    fn preserves_the_official_transcript_path_for_lifecycle_reconciliation() {
        let input: HookInput = serde_json::from_value(json!({
            "session_id": "task",
            "turn_id": "root-turn",
            "hook_event_name": "UserPromptSubmit",
            "transcript_path": "/tmp/privacy-scrubbed-task.jsonl"
        }))
        .unwrap();

        assert!(matches!(
            input.into_event(),
            EventMessage::Hook {
                transcript_path: Some(path),
                ..
            } if path == "/tmp/privacy-scrubbed-task.jsonl"
        ));
    }

    #[test]
    fn correlates_subagent_events_by_stable_agent_id_across_turn_ids() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();
        let event = |event: &str, turn_id: &str, tool_name: Option<&str>| {
            let input: HookInput = serde_json::from_value(json!({
                "session_id": "parent-task",
                "turn_id": turn_id,
                "hook_event_name": event,
                "agent_id": "stable-child",
                "agent_type": "worker",
                "tool_name": tool_name
            }))
            .unwrap();
            input.into_event()
        };

        tracker.state_for_event(&hook("root-turn", "UserPromptSubmit", false), &config);
        tracker.state_for_event(
            &event("SubagentStart", "spawn-turn", None),
            &config,
        );
        assert_eq!(
            tracker.state_for_event(
                &event(
                    "PreToolUse",
                    "tool-turn",
                    Some("functions.request_user_input"),
                ),
                &config,
            ),
            Some(StateKind::Requested)
        );
        assert_eq!(
            tracker.state_for_event(
                &event("SubagentStop", "completion-turn", None),
                &config,
            ),
            Some(StateKind::Working)
        );
    }

    #[test]
    fn ordinary_tool_failure_does_not_claim_the_whole_task_stopped() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();
        let mut failure = hook("root-turn", "PostToolUse", false);
        if let EventMessage::Hook { tool_failed, .. } = &mut failure {
            *tool_failed = true;
        }

        assert_eq!(
            tracker.state_for_event(&failure, &config),
            Some(StateKind::Working)
        );
    }

    #[test]
    fn unrelated_tool_failure_does_not_hide_an_approval_request() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();
        tracker.state_for_event(
            &hook("approval-turn", "PermissionRequest", false),
            &config,
        );
        let mut failure = hook("tool-turn", "PostToolUse", true);
        if let EventMessage::Hook { tool_failed, .. } = &mut failure {
            *tool_failed = true;
        }

        assert_eq!(
            tracker.state_for_event(&failure, &config),
            Some(StateKind::Approval)
        );
    }

    #[test]
    fn subagent_completion_cannot_finish_a_working_parent() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();

        assert_eq!(
            tracker.state_for_event(&hook("root-turn", "UserPromptSubmit", false), &config),
            Some(StateKind::Working)
        );
        assert_eq!(
            tracker.state_for_event(&hook("child-turn", "SubagentStart", true), &config),
            Some(StateKind::Working)
        );
        assert_eq!(
            tracker.state_for_event(&hook("child-turn", "SubagentStop", true), &config),
            Some(StateKind::Working)
        );
        assert_eq!(
            tracker.state_for_event(&hook("root-turn", "Stop", false), &config),
            Some(StateKind::Done)
        );
    }

    #[test]
    fn attention_state_outlives_unrelated_subagent_activity() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();

        tracker.state_for_event(&hook("root-turn", "UserPromptSubmit", false), &config);
        tracker.state_for_event(&hook("child-turn", "SubagentStart", true), &config);
        assert_eq!(
            tracker.state_for_event(&hook("root-turn", "PermissionRequest", false), &config),
            Some(StateKind::Approval)
        );
        assert_eq!(
            tracker.state_for_event(&hook("child-turn", "SubagentStop", true), &config),
            Some(StateKind::Approval)
        );
    }

    #[test]
    fn request_user_input_is_detected_before_the_turn_stops() {
        let config = AppConfig::default();
        let mut tracker = LifecycleTracker::default();
        let mut request = hook("root-turn", "PreToolUse", false);
        if let EventMessage::Hook { tool_name, .. } = &mut request {
            *tool_name = Some("functions.request_user_input".to_owned());
        }

        assert_eq!(
            tracker.state_for_event(&request, &config),
            Some(StateKind::Requested)
        );
        assert_eq!(
            tracker.state_for_event(&hook("root-turn", "PostToolUse", false), &config),
            Some(StateKind::Working)
        );
    }

    #[test]
    fn ignores_session_end_to_preserve_unacknowledged_state() {
        let config = AppConfig::default();
        assert_eq!(state_for_hook("SessionEnd", None, false, &config), None);
    }
}
