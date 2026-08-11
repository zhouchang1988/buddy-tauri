//! Read-time validation, port of `src/main/buddy/schemas.ts`.
//!
//! In the Electron edition the zod schemas are used to VALIDATE ON READ (never
//! on write) for forward compatibility: they reject malformed data, apply
//! defaults for absent fields, and strip unknown keys. Here serde
//! deserialization provides the same accept/reject behavior (missing required
//! fields and unknown `status` enum values are rejected; unknown keys are
//! ignored), and the `parse_*` functions apply the zod `.default(...)` values
//! and the `custom_prompt` trim/normalization explicitly.

use crate::buddy::types::{Event, GlobalSettings, Launcher, TaskSettings, TaskState};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

/// A zod-style validation failure (malformed JSON or schema violation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError(pub String);

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "schema validation failed: {}", self.0)
    }
}

impl std::error::Error for SchemaError {}

impl From<serde_json::Error> for SchemaError {
    fn from(error: serde_json::Error) -> Self {
        SchemaError(error.to_string())
    }
}

/// Validates raw JSON into a `TaskState`, applying the zod defaults from
/// `taskStateSchema` (`rounds_in_window: 0`, `instruction_queue: []`,
/// `context_sent: {}`, `countdown.remaining: 0`).
pub fn parse_task_state(input: &serde_json::Value) -> Result<TaskState, SchemaError> {
    let mut state: TaskState = serde_json::from_value(input.clone())?;
    if state.rounds_in_window.is_none() {
        state.rounds_in_window = Some(0);
    }
    if state.instruction_queue.is_none() {
        state.instruction_queue = Some(Vec::new());
    }
    if state.context_sent.is_none() {
        state.context_sent = Some(HashMap::new());
    }
    if let Some(countdown) = state.countdown.as_mut() {
        if countdown.remaining.is_none() {
            countdown.remaining = Some(0);
        }
    }
    Ok(state)
}

fn default_protocol_version() -> String {
    "1".to_string()
}

fn default_flow_policy() -> String {
    "claude_then_codex".to_string()
}

fn default_role_mode() -> String {
    "claude_implements".to_string()
}

/// `launcherSchema`: `env` defaults to `{}`, `timeout_seconds` to 600.
fn default_launcher_timeout() -> u64 {
    600
}

#[derive(Debug, Deserialize)]
struct LauncherWire {
    command: String,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default = "default_launcher_timeout")]
    timeout_seconds: u64,
}

impl From<LauncherWire> for Launcher {
    fn from(wire: LauncherWire) -> Self {
        Launcher {
            command: wire.command,
            env: wire.env,
            timeout_seconds: wire.timeout_seconds,
        }
    }
}

/// `taskSettingsSchema`: string fields default like the zod schema; unknown
/// keys are stripped (zod non-strict behavior).
#[derive(Debug, Deserialize)]
struct TaskSettingsWire {
    #[serde(default = "default_protocol_version")]
    protocol_version: String,
    #[serde(default = "default_flow_policy")]
    flow_policy: String,
    #[serde(default = "default_role_mode")]
    role_mode: String,
    #[serde(default)]
    launchers: HashMap<String, LauncherWire>,
    implementer_actor: Option<String>,
    reviewer_actor: Option<String>,
    max_consecutive_failures: Option<u32>,
    seed_claude_session_id: Option<String>,
    seed_codex_thread_id: Option<String>,
    seed_cursor_session_id: Option<String>,
    seed_opencode_session_id: Option<String>,
    seed_kimi_session_id: Option<String>,
}

/// Validates raw JSON into `TaskSettings` with the zod schema defaults.
pub fn parse_task_settings(input: &serde_json::Value) -> Result<TaskSettings, SchemaError> {
    let wire: TaskSettingsWire = serde_json::from_value(input.clone())?;
    Ok(TaskSettings {
        protocol_version: wire.protocol_version,
        flow_policy: wire.flow_policy,
        role_mode: wire.role_mode,
        launchers: wire
            .launchers
            .into_iter()
            .map(|(actor, launcher)| (actor, launcher.into()))
            .collect(),
        implementer_actor: wire.implementer_actor,
        reviewer_actor: wire.reviewer_actor,
        max_consecutive_failures: wire.max_consecutive_failures,
        seed_claude_session_id: wire.seed_claude_session_id,
        seed_codex_thread_id: wire.seed_codex_thread_id,
        seed_cursor_session_id: wire.seed_cursor_session_id,
        seed_opencode_session_id: wire.seed_opencode_session_id,
        seed_kimi_session_id: wire.seed_kimi_session_id,
    })
}

/// `globalSettingsSchema` field defaults, applied to the lenient wire shape
/// before converting into the shared `GlobalSettings` type.
#[derive(Debug, Deserialize)]
struct GlobalSettingsWire {
    #[serde(default = "default_protocol_version")]
    protocol_version: String,
    #[serde(default = "default_countdown_seconds")]
    countdown_seconds: u64,
    #[serde(default = "default_max_rounds")]
    max_rounds: u32,
    #[serde(default = "default_max_consecutive_failures")]
    max_consecutive_failures: u32,
    #[serde(default)]
    launchers: HashMap<String, LauncherWire>,
    seed_claude_session_id: Option<String>,
    seed_codex_thread_id: Option<String>,
    seed_cursor_session_id: Option<String>,
    seed_opencode_session_id: Option<String>,
    seed_kimi_session_id: Option<String>,
    max_compact_retries: Option<u32>,
    #[serde(default = "default_true")]
    auto_generate_commit_message: bool,
    #[serde(default = "default_true")]
    system_notifications_enabled: bool,
    max_upgrade_retries: Option<u32>,
    custom_prompt: Option<String>,
}

fn default_countdown_seconds() -> u64 {
    30
}

fn default_max_rounds() -> u32 {
    9999
}

fn default_max_consecutive_failures() -> u32 {
    10
}

fn default_true() -> bool {
    true
}

/// Validates raw JSON into `GlobalSettings` with the zod schema defaults.
/// `custom_prompt` is trimmed and normalized to `None` when empty, mirroring
/// the zod `.trim().transform((v) => (v ? v : undefined))` chain.
pub fn parse_global_settings(input: &serde_json::Value) -> Result<GlobalSettings, SchemaError> {
    let wire: GlobalSettingsWire = serde_json::from_value(input.clone())?;
    Ok(GlobalSettings {
        protocol_version: Some(wire.protocol_version),
        countdown_seconds: Some(wire.countdown_seconds),
        max_rounds: Some(wire.max_rounds),
        max_consecutive_failures: Some(wire.max_consecutive_failures),
        launchers: Some(
            wire.launchers
                .into_iter()
                .map(|(actor, launcher)| (actor, launcher.into()))
                .collect(),
        ),
        seed_claude_session_id: wire.seed_claude_session_id,
        seed_codex_thread_id: wire.seed_codex_thread_id,
        seed_cursor_session_id: wire.seed_cursor_session_id,
        seed_opencode_session_id: wire.seed_opencode_session_id,
        seed_kimi_session_id: wire.seed_kimi_session_id,
        max_compact_retries: wire.max_compact_retries,
        auto_generate_commit_message: Some(wire.auto_generate_commit_message),
        system_notifications_enabled: Some(wire.system_notifications_enabled),
        max_upgrade_retries: wire.max_upgrade_retries,
        custom_prompt: wire
            .custom_prompt
            .map(|prompt| prompt.trim().to_string())
            .filter(|prompt| !prompt.is_empty()),
    })
}

/// `eventSchema`: `seq`, `type`, `ts` and `payload` are required; unknown keys
/// are stripped.
#[derive(Debug, Deserialize)]
struct EventWire {
    seq: u64,
    task_id: Option<String>,
    #[serde(rename = "type")]
    event_type: String,
    actor: Option<String>,
    ts: String,
    run_id: Option<String>,
    payload: serde_json::Map<String, serde_json::Value>,
}

/// Parses a single `events.jsonl` line into an `Event`
/// (`JSON.parse` + `eventSchema.parse` in the Electron edition).
pub fn parse_event_line(line: &str) -> Result<Event, SchemaError> {
    let value: serde_json::Value = serde_json::from_str(line)?;
    let wire: EventWire = serde_json::from_value(value)?;
    Ok(Event {
        seq: wire.seq,
        task_id: wire.task_id,
        event_type: wire.event_type,
        actor: wire.actor,
        ts: wire.ts,
        run_id: wire.run_id,
        payload: wire.payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::TaskStatus;
    use serde_json::json;

    #[test]
    fn parses_task_state_with_optional_fields() {
        let state = parse_task_state(&json!({
            "status": "READY",
            "round": 1,
            "next_actor": "claude",
            "active_run": null
        }))
        .unwrap();

        assert_eq!(state.status, TaskStatus::Ready);
        assert_eq!(state.round, 1);
    }

    #[test]
    fn parses_buddy_python_round_window_and_context_tracking_fields() {
        let state = parse_task_state(&json!({
            "status": "READY",
            "round": 0,
            "rounds_in_window": 0,
            "next_actor": "opencode",
            "context_hash": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "context_sent": {
                "claude": false,
                "codex": true,
                "opencode": false,
                "kimi": false
            },
            "active_run": null,
            "last_error": {
                "message": "boom",
                "actor": "codex",
                "run_id": "run-1",
                "ts": "2026-05-26T00:00:00.000Z",
                "output_file": "/tmp/out.md",
                "event_file": "/tmp/events.jsonl"
            }
        }))
        .unwrap();

        assert_eq!(state.rounds_in_window, Some(0));
        assert_eq!(
            state.context_sent.as_ref().and_then(|m| m.get("codex")),
            Some(&true)
        );
        assert_eq!(
            state.last_error.as_ref().and_then(|f| f.run_id.as_deref()),
            Some("run-1")
        );
    }

    #[test]
    fn accepts_legacy_nullable_state_fields() {
        let state = parse_task_state(&json!({
            "status": "PAUSED",
            "round": 0,
            "next_actor": "claude",
            "countdown": null,
            "active_run": null,
            "claude_session_id": null,
            "codex_thread_id": null,
            "opencode_session_id": null,
            "kimi_session_id": null
        }))
        .unwrap();

        assert_eq!(state.status, TaskStatus::Paused);
        assert!(state.countdown.is_none());
        assert!(state.claude_session_id.is_none());
    }

    #[test]
    fn preserves_buddy_python_state_fields() {
        let state = parse_task_state(&json!({
            "protocol_version": "1",
            "task_id": "demo",
            "repo_root": "/tmp/repo",
            "status": "READY",
            "round": 0,
            "rounds_in_window": 0,
            "next_actor": "claude",
            "claude_session_id": null,
            "codex_thread_id": null,
            "context_hash": "abc",
            "context_sent": { "claude": false, "codex": false },
            "active_run": null,
            "countdown": null,
            "last_error": null,
            "event_seq": 1,
            "transcript_seq": 0,
            "consecutive_failures": 0,
            "created_at": "2026-05-26T11:11:27Z",
            "updated_at": "2026-05-26T11:11:27Z"
        }))
        .unwrap();

        assert_eq!(state.task_id.as_deref(), Some("demo"));
        assert_eq!(state.rounds_in_window, Some(0));
        assert_eq!(
            state.context_sent.as_ref().and_then(|m| m.get("claude")),
            Some(&false)
        );
        assert_eq!(state.event_seq, Some(1));
        assert_eq!(state.transcript_seq, Some(0));
        assert!(state.last_error.is_none());
    }

    #[test]
    fn accepts_legacy_countdown_objects_without_remaining_seconds() {
        let state = parse_task_state(&json!({
            "status": "DONE",
            "round": 1,
            "next_actor": "claude",
            "active_run": null,
            "countdown": {
                "after_actor": "codex",
                "deadline": "2026-05-22T11:12:52Z",
                "default_next_actor": "claude",
                "started_at": "2026-05-22T11:12:22Z",
                "status": "elapsed"
            }
        }))
        .unwrap();

        let countdown = state.countdown.as_ref().unwrap();
        assert_eq!(countdown.remaining, Some(0));
        assert_eq!(countdown.default_next_actor, "claude");
    }

    #[test]
    fn parses_event_json_lines() {
        let event = parse_event_line(
            "{\"seq\":1,\"task_id\":\"demo\",\"type\":\"task.created\",\"ts\":\"2026-05-26T00:00:00.000Z\",\"payload\":{}}",
        )
        .unwrap();

        assert_eq!(event.seq, 1);
        assert_eq!(event.task_id.as_deref(), Some("demo"));
        assert_eq!(event.event_type, "task.created");
    }

    #[test]
    fn rejects_malformed_event_json_lines() {
        assert!(parse_event_line("{bad").is_err());
    }

    #[test]
    fn normalizes_empty_custom_prompt_to_none() {
        let settings = parse_global_settings(&json!({ "custom_prompt": "   " })).unwrap();
        assert_eq!(settings.custom_prompt, None);

        let populated =
            parse_global_settings(&json!({ "custom_prompt": "Always run tests." })).unwrap();
        assert_eq!(
            populated.custom_prompt.as_deref(),
            Some("Always run tests.")
        );
    }
}
