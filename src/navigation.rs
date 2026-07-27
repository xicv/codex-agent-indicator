use std::process::Command;

use anyhow::{Context, Result, bail};

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

pub fn open_codex_thread(session_id: &str) -> Result<()> {
    let url = codex_thread_url(session_id).context("session ID is not safe for a Codex deep link")?;
    let status = Command::new("/usr/bin/open")
        .arg(&url)
        .status()
        .with_context(|| format!("failed to open {url}"))?;
    if !status.success() {
        bail!("/usr/bin/open rejected {url} with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::codex_thread_url;

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
}
