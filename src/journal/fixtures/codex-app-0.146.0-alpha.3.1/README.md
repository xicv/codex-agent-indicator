# Codex app lifecycle fixture

This privacy-scrubbed JSONL fixture captures the `task_started` and
`task_complete` records observed in Codex app `0.146.0-alpha.3.1` on
2026-07-28. It intentionally ends with a newer active turn so replay tests
cannot mistake an older completion for the current task state.

This is a pinned compatibility fixture for an internal local journal format,
not an OpenAI-published schema.
