# Codex hook replay snapshot

These are synthetic, privacy-scrubbed lifecycle fixtures for Codex CLI
`0.145.0`. They are pinned to the official OpenAI `rust-v0.145.0` tag at commit
`1635de866c61d1b76e50b31928ee6d61482435a8`, captured on 2026-07-28.

The `schema/` files preserve the command-input schema content used by these
scenarios:

<https://github.com/openai/codex/tree/rust-v0.145.0/codex-rs/hooks/schema/generated>

The scenarios cover:

- a normal parent turn from prompt to completion;
- an approval request until the approved tool resumes;
- user input requested during a tool and again at final response;
- parallel subagents with changing turn IDs and stable agent IDs;
- an interrupted observable stream that ends without inventing a terminal
  `Stop` hook.

Every replay validates its input against the pinned schema, deserializes through
`HookInput`, applies `LifecycleTracker`, and compares the resulting G-key slot
snapshot after each event. A separate test verifies that the shipped hook
configuration registers every replayed event.

All IDs, paths, prompts, tool calls, and responses are generic test data. The
fixtures do not contain copied transcripts, user content, usernames, home
directories, credentials, or real task identifiers.

When Codex changes its hook wire format, add a new version-labelled directory
with the matching official schemas. Keep this snapshot unchanged so old and new
behavior remain independently replayable.
