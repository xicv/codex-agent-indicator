# Codex app approval-resume fixture

This privacy-scrubbed JSONL fixture captures the `response_item` shapes observed
when Codex Desktop `0.146.0-alpha.9.2` resumes an approval-gated code-mode tool
call. The tool first returns a running cell and then completes through the
corresponding `wait` call.

The fixture deliberately contains no conversation text, personal paths,
credentials, or original commands. It is a pinned compatibility fixture for an
internal local journal format, not an OpenAI-published schema.
