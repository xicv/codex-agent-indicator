use std::process::Command;

use anyhow::{Context, Result, bail};

const CODEX_BUNDLE_IDENTIFIER: &str = "com.openai.codex";

pub fn codex_thread_url(session_id: &str) -> Option<String> {
    if session_id.is_empty()
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(format!("codex://threads/{session_id}"))
}

fn codex_open_arguments(session_id: &str) -> Option<Vec<String>> {
    let url = codex_thread_url(session_id)?;
    Some(vec![
        "-b".to_owned(),
        CODEX_BUNDLE_IDENTIFIER.to_owned(),
        url,
    ])
}

pub fn open_codex_thread(session_id: &str) -> Result<()> {
    let arguments =
        codex_open_arguments(session_id).context("session ID is not safe for a Codex deep link")?;
    let url = &arguments[2];
    let status = Command::new("/usr/bin/open")
        .args(&arguments)
        .status()
        .with_context(|| format!("failed to open {url}"))?;
    if !status.success() {
        bail!("/usr/bin/open rejected {url} with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{codex_open_arguments, codex_thread_url};

    const LAUNCH_AGENT: &str =
        include_str!("../launchd/com.codex-agent-indicator.plist.template");

    #[test]
    fn builds_deep_links_only_for_safe_technical_thread_ids() {
        assert_eq!(
            codex_thread_url("test-thread_123").as_deref(),
            Some("codex://threads/test-thread_123")
        );
        assert_eq!(codex_thread_url(""), None);
        assert_eq!(codex_thread_url("../settings"), None);
        assert_eq!(codex_thread_url("task?prompt=surprise"), None);
    }

    #[test]
    fn opens_the_selected_thread_in_the_foreground_codex_app() {
        assert_eq!(
            codex_open_arguments("test-thread_123"),
            Some(vec![
                "-b".to_owned(),
                "com.openai.codex".to_owned(),
                "codex://threads/test-thread_123".to_owned(),
            ])
        );
        assert_eq!(codex_open_arguments("../settings"), None);
    }

    #[test]
    fn launch_agent_prioritizes_physical_g_key_navigation() {
        assert!(
            LAUNCH_AGENT.contains(
                "<key>ProcessType</key>\n    <string>Interactive</string>"
            ),
            "physical G-key input must not run at background scheduling priority"
        );
        assert!(!LAUNCH_AGENT.contains("<string>Background</string>"));
    }
}
