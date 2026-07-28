use serde::Deserialize;
use serde_json::Value;

use crate::config::AppConfig;
use crate::state::{Engine, StateKind};

use super::{EventMessage, HookInput, LifecycleTracker};

const CODEX_VERSION: &str = "0.145.0";
const SCHEMA_TAG: &str = "rust-v0.145.0";
const SCHEMA_COMMIT: &str = "1635de866c61d1b76e50b31928ee6d61482435a8";
const SCHEMA_SNAPSHOT_DATE: &str = "2026-07-28";
const SCHEMA_SOURCE: &str =
    "https://github.com/openai/codex/tree/rust-v0.145.0/codex-rs/hooks/schema/generated";
const PARENT_COMPLETES: &str =
    include_str!("fixtures/codex-hooks-0.145.0/parent-completes.json");
const APPROVAL_REQUEST: &str =
    include_str!("fixtures/codex-hooks-0.145.0/approval-request.json");
const INPUT_REQUEST: &str = include_str!("fixtures/codex-hooks-0.145.0/input-request.json");
const PARALLEL_SUBAGENTS: &str =
    include_str!("fixtures/codex-hooks-0.145.0/parallel-subagents.json");
const INTERRUPTED_WITHOUT_STOP: &str =
    include_str!("fixtures/codex-hooks-0.145.0/interrupted-without-stop.json");
const REPLAY_FIXTURES: &[&str] = &[
    PARENT_COMPLETES,
    APPROVAL_REQUEST,
    INPUT_REQUEST,
    PARALLEL_SUBAGENTS,
    INTERRUPTED_WITHOUT_STOP,
];

#[test]
fn replays_parent_task_from_hook_json_to_slot_snapshots() {
    replay_fixture(PARENT_COMPLETES);
}

#[test]
fn replays_approval_request_until_the_tool_resumes() {
    replay_fixture(APPROVAL_REQUEST);
}

#[test]
fn replays_user_input_request_and_final_question() {
    replay_fixture(INPUT_REQUEST);
}

#[test]
fn replays_parallel_subagents_without_losing_parent_or_approval_state() {
    replay_fixture(PARALLEL_SUBAGENTS);
}

#[test]
fn replays_interruption_without_inventing_a_terminal_hook() {
    let fixture = parse_fixture(INTERRUPTED_WITHOUT_STOP);
    assert!(
        fixture.events.iter().all(|step| {
            step.input.get("hook_event_name").and_then(Value::as_str) != Some("Stop")
        }),
        "interruption fixture must preserve the observed absence of Stop"
    );
    replay_fixture(INTERRUPTED_WITHOUT_STOP);
}

#[test]
fn shipped_hook_config_covers_every_replayed_event() {
    let integration: Value = serde_json::from_str(include_str!("../../integrations/codex-hooks.json"))
        .expect("integration hook config must be valid JSON");
    let configured = integration
        .get("hooks")
        .and_then(Value::as_object)
        .expect("integration hook config must contain hooks");

    for source in REPLAY_FIXTURES {
        let fixture = parse_fixture(source);
        for step in fixture.events {
            let event_name = step
                .input
                .get("hook_event_name")
                .and_then(Value::as_str)
                .expect("fixture hook event name must be a string");
            assert!(
                configured.contains_key(event_name),
                "scenario {:?} replays unconfigured hook event {event_name}",
                fixture.scenario
            );
        }
    }
}

#[test]
fn replay_fixtures_are_privacy_scrubbed() {
    let forbidden = [
        "/Users/",
        "/home/",
        "file://",
        "gho_",
        "github_pat_",
        "PRIVATE KEY",
    ];
    for source in REPLAY_FIXTURES {
        let fixture = parse_fixture(source);
        for pattern in forbidden {
            assert!(
                !source.contains(pattern),
                "scenario {:?} contains forbidden privacy pattern {pattern:?}",
                fixture.scenario
            );
        }
    }
}

#[test]
#[should_panic(expected = "fixture is missing required field \"turn_id\"")]
fn pinned_schema_rejects_a_missing_required_hook_field() {
    let mut fixture = parse_fixture(PARENT_COMPLETES);
    fixture.events[0]
        .input
        .as_object_mut()
        .expect("fixture hook input must be an object")
        .remove("turn_id");
    validate_hook_input(&fixture.events[0].input);
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayFixture {
    schema: FixtureSchema,
    scenario: String,
    events: Vec<ReplayStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureSchema {
    codex_version: String,
    schema_tag: String,
    schema_commit: String,
    snapshot_date: String,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayStep {
    input: Value,
    slots: Vec<ExpectedSlot>,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ExpectedSlot {
    slot: usize,
    key: String,
    session_id: String,
    state: StateKind,
}

fn replay_fixture(source: &str) {
    let fixture = parse_fixture(source);
    assert_fixture_provenance(&fixture.schema);
    assert!(!fixture.scenario.trim().is_empty(), "scenario must be named");
    assert!(!fixture.events.is_empty(), "scenario must contain events");

    let config = AppConfig::default();
    let mut lifecycle = LifecycleTracker::default();
    let mut engine = Engine::new(config.behavior.max_sessions);

    for (index, step) in fixture.events.into_iter().enumerate() {
        validate_hook_input(&step.input);
        let hook: HookInput =
            serde_json::from_value(step.input).expect("fixture must deserialize as HookInput");
        let message = hook.into_event();
        let EventMessage::Hook { session_id, .. } = &message else {
            panic!("HookInput must produce a hook event");
        };
        if let Some(state) = lifecycle.state_for_event(&message, &config) {
            engine.transition(session_id, state, (index + 1) as u64, &config);
        }

        let actual = engine
            .snapshot(&config)
            .into_iter()
            .map(|slot| ExpectedSlot {
                slot: slot.slot,
                key: slot.key,
                session_id: slot.session_id,
                state: slot.state,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            step.slots,
            "scenario {:?}, event {}",
            fixture.scenario,
            index + 1
        );
    }
}

fn parse_fixture(source: &str) -> ReplayFixture {
    serde_json::from_str(source).expect("replay fixture must be valid JSON")
}

fn assert_fixture_provenance(schema: &FixtureSchema) {
    assert_eq!(schema.codex_version, CODEX_VERSION);
    assert_eq!(schema.schema_tag, SCHEMA_TAG);
    assert_eq!(schema.schema_commit, SCHEMA_COMMIT);
    assert_eq!(schema.snapshot_date, SCHEMA_SNAPSHOT_DATE);
    assert_eq!(schema.source, SCHEMA_SOURCE);
}

fn validate_hook_input(input: &Value) {
    let object = input
        .as_object()
        .expect("each hook input must be a JSON object");
    let event_name = object
        .get("hook_event_name")
        .and_then(Value::as_str)
        .expect("hook_event_name must be a string");
    let schema_source = match event_name {
        "UserPromptSubmit" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/user-prompt-submit.command.input.schema.json"
        ),
        "PreToolUse" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/pre-tool-use.command.input.schema.json"
        ),
        "PermissionRequest" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/permission-request.command.input.schema.json"
        ),
        "PostToolUse" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/post-tool-use.command.input.schema.json"
        ),
        "SubagentStart" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/subagent-start.command.input.schema.json"
        ),
        "SubagentStop" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/subagent-stop.command.input.schema.json"
        ),
        "Stop" => include_str!(
            "fixtures/codex-hooks-0.145.0/schema/stop.command.input.schema.json"
        ),
        other => panic!("no pinned hook schema for {other}"),
    };
    let schema: Value =
        serde_json::from_str(schema_source).expect("vendored hook schema must be valid JSON");
    let schema_object = schema
        .as_object()
        .expect("pinned hook schema must be a JSON object");
    for keyword in schema_object.keys() {
        assert!(
            matches!(
                keyword.as_str(),
                "$schema"
                    | "additionalProperties"
                    | "definitions"
                    | "properties"
                    | "required"
                    | "title"
                    | "type"
            ),
            "pinned schema uses unsupported root keyword {keyword:?}"
        );
    }
    assert_eq!(
        schema.get("type").and_then(Value::as_str),
        Some("object"),
        "pinned hook schema must describe an object"
    );
    assert_eq!(
        schema.get("additionalProperties"),
        Some(&Value::Bool(false)),
        "pinned schema must reject unknown fields"
    );
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("pinned schema must define properties");
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("pinned schema must define required fields");

    for field in required {
        let field = field
            .as_str()
            .expect("required schema entries must be strings");
        assert!(
            object.contains_key(field),
            "{event_name} fixture is missing required field {field:?}"
        );
    }
    for (field, value) in object {
        let rule = properties
            .get(field)
            .unwrap_or_else(|| panic!("{event_name} fixture has unknown field {field:?}"));
        validate_schema_rule(&schema, event_name, field, rule, value);
    }
}

fn validate_schema_rule(
    schema: &Value,
    event_name: &str,
    field: &str,
    rule: &Value,
    value: &Value,
) {
    if rule == &Value::Bool(true) {
        return;
    }
    assert_ne!(
        rule,
        &Value::Bool(false),
        "{event_name}.{field} is rejected by its schema"
    );
    let rule_object = rule
        .as_object()
        .unwrap_or_else(|| panic!("{event_name}.{field} has an unsupported schema rule"));
    for keyword in rule_object.keys() {
        assert!(
            matches!(
                keyword.as_str(),
                "$ref" | "const" | "description" | "enum" | "type"
            ),
            "{event_name}.{field} uses unsupported schema keyword {keyword:?}"
        );
    }

    if let Some(reference) = rule.get("$ref").and_then(Value::as_str) {
        let pointer = reference
            .strip_prefix('#')
            .expect("only local schema references are supported");
        let resolved = schema
            .pointer(pointer)
            .unwrap_or_else(|| panic!("schema reference {reference:?} must resolve"));
        validate_schema_rule(schema, event_name, field, resolved, value);
        return;
    }
    if let Some(expected) = rule.get("const") {
        assert_eq!(
            value, expected,
            "{event_name}.{field} must match the schema constant"
        );
    }
    if let Some(allowed) = rule.get("enum").and_then(Value::as_array) {
        assert!(
            allowed.contains(value),
            "{event_name}.{field} is not in the schema enum"
        );
    }
    if let Some(types) = rule.get("type") {
        assert!(
            schema_type_matches(types, value),
            "{event_name}.{field} has the wrong JSON type"
        );
    }
}

fn schema_type_matches(types: &Value, value: &Value) -> bool {
    match types {
        Value::String(expected) => json_type_matches(expected, value),
        Value::Array(expected) => expected.iter().any(|expected| {
            expected
                .as_str()
                .is_some_and(|expected| json_type_matches(expected, value))
        }),
        _ => false,
    }
}

fn json_type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "array" => value.is_array(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "object" => value.is_object(),
        "string" => value.is_string(),
        _ => false,
    }
}
