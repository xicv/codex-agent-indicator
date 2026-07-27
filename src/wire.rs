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
        hook_event_name: String,
        last_assistant_message: Option<String>,
        tool_failed: bool,
    },
    Set {
        session_id: String,
        state: StateKind,
    },
    Clear {
        session_id: Option<String>,
    },
    Reload,
}

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub tool_response: Option<Value>,
}

impl HookInput {
    pub fn into_event(self) -> EventMessage {
        EventMessage::Hook {
            session_id: self.session_id,
            hook_event_name: self.hook_event_name,
            last_assistant_message: self
                .last_assistant_message
                .as_deref()
                .map(message_tail),
            tool_failed: self.tool_response.as_ref().is_some_and(tool_failed),
        }
    }
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

fn message_tail(message: &str) -> String {
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
mod tests {
    use serde_json::json;

    use crate::config::AppConfig;

    use super::{
        assistant_message_reports_failure, assistant_message_requests_input, state_for_hook,
        tool_failed,
    };
    use crate::state::StateKind;

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
    fn ignores_session_end_to_preserve_unacknowledged_state() {
        let config = AppConfig::default();
        assert_eq!(state_for_hook("SessionEnd", None, false, &config), None);
    }
}
