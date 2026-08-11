//! Per-round prompt construction for implementer/reviewer actors.
//! Port of `src/main/buddy/prompts.ts` from the Electron edition.
//!
//! Byte-level fidelity of the generated prompts matters: the tests here are a
//! verbatim port of `tests/unit/main/buddy-prompts.test.ts`.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::buddy::types::{GlobalSettings, TranscriptEntry};

/// Transcript row as seen by the prompt builder: the typed entry plus the
/// optional `seq` the store assigns in transcript.jsonl. The TS original reads
/// it as `TranscriptEntry & { seq?: number }`; `types::TranscriptEntry`
/// carries the same optional `seq`, which `From<&TranscriptEntry>` passes
/// through. Rows without a `seq` sort by stable insertion order — the same
/// result the JS code produces when a row's `seqValue` is 0.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRow {
    pub role: String,
    pub content: String,
    pub ts: String,
    pub seq: Option<u64>,
}

impl From<&TranscriptEntry> for TranscriptRow {
    fn from(entry: &TranscriptEntry) -> Self {
        TranscriptRow {
            role: entry.role.clone(),
            content: entry.content.clone(),
            ts: entry.ts.clone(),
            seq: entry.seq,
        }
    }
}

pub const ACTOR_CLAUDE: &str = "claude";
pub const ACTOR_CODEX: &str = "codex";
pub const ACTOR_CURSOR: &str = "cursor";
pub const ACTOR_OPENCODE: &str = "opencode";
pub const ACTOR_KIMI: &str = "kimi";
pub const ROLE_MODE_CODEX_IMPL: &str = "codex_implements";

pub const BUDDY_MESSAGE_PROTOCOL: &str = r#"## Buddy Message Protocol

Your output is parsed by the buddy orchestrator. Wrap your response in the following JSON structure:

```json
{
  "type": "chat",
  "content": "your response text here"
}
```

- **type=chat**: Normal continuation. The loop proceeds to the next actor.
- **type=break**: Request to end the task. The other actor must also confirm with `type=break` before the task transitions to DONE.

Rules:
- Always output valid JSON matching this structure.
- Output the JSON as your **final text response** - do NOT use shell commands (echo, printf, etc.) to output it. The orchestrator reads your text output, not command output.
- Output raw JSON only - do NOT wrap it in a Markdown code block, and do NOT add any text before or after the JSON.
- Avoid unescaped double quotes inside `content`; use single quotes or escape them.
- Use `type=break` when: the task is fully completed, you are blocked and need human input, continuing would be counterproductive, or the other actor has failed repeatedly on the same issue across multiple rounds without meaningful progress.
- Use `type=chat` for all normal responses.
- The `content` field contains your actual response (markdown is fine).

## Dual confirmation

When one actor signals `type=break`, the task does NOT end immediately. The other actor must also confirm with `type=break` before the task transitions to DONE. If the other actor responds with `type=chat` instead, the break request is withdrawn and work continues."#;

#[derive(Debug, Clone, Default)]
pub struct BuildActorPromptInput {
    pub actor: String,
    pub round: u32,
    pub repo_root: String,
    pub task_text: String,
    pub context_text: String,
    pub transcript: Vec<TranscriptRow>,
    /// Loose `Partial<TaskSettings>` from the TS original; pass a JSON object.
    pub settings: Option<Value>,
    /// Loose `Partial<TaskState>` from the TS original; pass a JSON object.
    pub state: Option<Value>,
    pub global_settings: Option<GlobalSettings>,
    pub user_message: Option<String>,
}

pub fn build_ping_prompt(actor: &str) -> String {
    let parts = [
        "# buddy actor turn",
        "",
        "## Actor",
        actor,
        "",
        BUDDY_MESSAGE_PROTOCOL,
        "",
        "## Task",
        "Connectivity check — say hi to confirm you are ready.",
        "",
        "## Instruction",
        "This is a quick connectivity check before a collaborative task begins. Please respond with a brief greeting to confirm you are operational and ready to work. Use the buddy message protocol (JSON) to respond.",
    ];
    format!("{}\n", parts.join("\n").trim_end())
}

pub fn build_actor_prompt(input: &BuildActorPromptInput) -> String {
    let task_text = input.task_text.trim();
    let context_text = input.context_text.trim();
    let empty = Value::Object(Map::new());
    let state = input.state.as_ref().unwrap_or(&empty);
    let settings = input.settings.as_ref().unwrap_or(&empty);
    let context_hash = hash_text(&input.context_text);
    let context_sent = state.get("context_sent").and_then(Value::as_object);
    let pending_break = state.get("pending_break").filter(|v| js_truthy(v));
    let break_rejected_by = state.get("break_rejected_by").filter(|v| js_truthy(v));

    let mut parts: Vec<String> = vec![
        "# buddy actor turn".to_string(),
        String::new(),
        "## Actor".to_string(),
        input.actor.clone(),
        String::new(),
        BUDDY_MESSAGE_PROTOCOL.to_string(),
        String::new(),
        "## Task".to_string(),
        task_text.to_string(),
    ];

    let context_sent_to_actor = context_sent
        .and_then(|sent| sent.get(&input.actor))
        .map(js_truthy)
        .unwrap_or(false);
    if !context_text.is_empty()
        && (state.get("context_hash").and_then(Value::as_str) != Some(context_hash.as_str())
            || !context_sent_to_actor)
    {
        parts.push(String::new());
        parts.push("## Background context".to_string());
        parts.push(context_text.to_string());
    }

    if let Some(pending_break) = pending_break {
        let requester_label = actor_display_name(
            pending_break.get("actor").and_then(Value::as_str).unwrap_or(""),
        );
        parts.push(String::new());
        parts.push("## Break confirmation required".to_string());
        parts.push(format!(
            "{} has signaled `type=break` and believes the task is complete.",
            requester_label
        ));
        parts.push("You must decide:".to_string());
        parts.push("- If you also agree the task is complete, respond with `type=break` to confirm. The task will then end.".to_string());
        parts.push("- If you think work should continue, respond with `type=chat` and describe what still needs to be done. The break request will be withdrawn.".to_string());
        parts.push(String::new());
        parts.push("**Important**: This is a priority decision. Do NOT start new work or investigate new questions. Either confirm the break or reject it with a specific reason.".to_string());
    }

    let break_rejected_by_other = break_rejected_by.filter(|marker| {
        marker.get("actor").and_then(Value::as_str) != Some(input.actor.as_str())
    });
    if let Some(rejected) = break_rejected_by_other {
        let rejected_label =
            actor_display_name(rejected.get("actor").and_then(Value::as_str).unwrap_or(""));
        parts.push(String::new());
        parts.push("## Break request rejected — review required".to_string());
        parts.push(format!(
            "Your previous `type=break` request was rejected by {}, who made changes to the codebase.",
            rejected_label
        ));
        parts.push(format!(
            "You must review {}'s changes before confirming completion.",
            rejected_label
        ));
        parts.push("- Carefully examine the changes made by the other actor.".to_string());
        parts.push("- If the changes are correct and the task is truly complete, respond with `type=break`.".to_string());
        parts.push("- If you find issues with the changes or the task is not yet complete, respond with `type=chat` and describe what needs to be fixed.".to_string());
    }

    if let Some(user_message) = &input.user_message {
        if !user_message.is_empty() {
            parts.push(String::new());
            parts.push("## Human message".to_string());
            parts.push(user_message.clone());
        }
    }

    parts.push(String::new());
    parts.push("## Runtime settings".to_string());
    parts.extend(runtime_settings_lines(
        settings,
        state,
        input.global_settings.as_ref(),
        &input.actor,
        &input.repo_root,
    ));

    let recent = select_recent_transcript(&input.transcript, 6);
    if !recent.is_empty() {
        parts.push(String::new());
        parts.push("## Recent transcript".to_string());
        for item in &recent {
            parts.push(format!("### {}", item.role));
            parts.push(item.content.clone());
        }
    }

    parts.push(String::new());
    parts.push("## Instruction".to_string());
    let human_lang = detect_human_language(
        &input.transcript,
        input.user_message.as_deref().unwrap_or(""),
        task_text,
        context_text,
    );
    if let Some(pending_break) = pending_break {
        let requester_name = actor_display_name(
            pending_break.get("actor").and_then(Value::as_str).unwrap_or(""),
        );
        parts.push(format!(
            "{} has requested to end the task. Confirm with `type=break` or continue with `type=chat`.",
            requester_name
        ));
    } else if let Some(rejected) = break_rejected_by_other {
        let rejected_name =
            actor_display_name(rejected.get("actor").and_then(Value::as_str).unwrap_or(""));
        parts.push(format!(
            "Your previous break request was rejected by {}, who made changes. Review their changes carefully. Only confirm with `type=break` if you agree the changes are correct and the task is complete.",
            rejected_name
        ));
    } else {
        let implementer = implementer_actor(settings);
        if input.actor == implementer {
            parts.push("Continue the implementation work. Report changed files, what you did, and blockers.".to_string());
        } else {
            parts.push("Review the current task state. Report blocking findings first, then concise next action. If you detect that the other actor is making repeated errors or the task is stuck in a circular pattern without progress, signal `type=break` to stop and let a human decide.".to_string());
        }
    }

    if !human_lang.is_empty() {
        parts.push(format!(
            "默认使用最近 human message 的语言输出；当前任务使用{}。除 JSON 等编程语言外，所有自然语言内容都用{}输出。",
            human_lang, human_lang
        ));
    }

    // User-defined custom prompt, appended verbatim after the system prompt so
    // it applies to every actor on every round. Optional; omitted when empty.
    let custom_prompt = input
        .global_settings
        .as_ref()
        .and_then(|g| g.custom_prompt.as_deref())
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Some(custom_prompt) = custom_prompt {
        parts.push(String::new());
        parts.push("## Custom instructions".to_string());
        parts.push(custom_prompt.to_string());
    }

    format!("{}\n", parts.join("\n").trim_end())
}

pub fn runtime_settings_lines(
    settings: &Value,
    state: &Value,
    global_settings: Option<&GlobalSettings>,
    actor: &str,
    repo_root: &str,
) -> Vec<String> {
    let max_rounds = global_settings
        .and_then(|g| g.max_rounds)
        .map(|v| v as f64)
        .unwrap_or(9999.0);
    let rounds_in_window = number_value(state.get("rounds_in_window"), 0.0);
    let remaining = if max_rounds == -1.0 {
        "unlimited".to_string()
    } else if max_rounds > 0.0 {
        js_number_string((max_rounds - rounds_in_window).max(0.0))
    } else {
        "unlimited".to_string()
    };
    let mut lines = vec![
        format!(
            "- Current total round: {}",
            js_number_string(number_value(state.get("round"), 0.0))
        ),
        format!(
            "- Automatic rounds used in this window: {}/{}",
            js_number_string(rounds_in_window),
            if max_rounds == -1.0 {
                "unlimited".to_string()
            } else {
                js_number_string(max_rounds)
            }
        ),
        format!(
            "- Automatic rounds remaining in this window: {}",
            remaining
        ),
        format!("- Next actor after this turn: {}", next_actor(actor, settings)),
    ];
    if !repo_root.is_empty() {
        lines.push(format!("- Repository: {}", repo_root));
    }
    if let Some(deadline) = state
        .get("countdown")
        .and_then(|c| c.get("deadline"))
        .and_then(Value::as_str)
    {
        lines.push(format!("- Active countdown deadline: {}", deadline));
    }
    let consecutive_failures = number_value(state.get("consecutive_failures"), 0.0);
    if consecutive_failures > 0.0 {
        lines.push(format!(
            "- Consecutive failures: {}",
            js_number_string(consecutive_failures)
        ));
        if let Some(message) = state
            .get("latest_failure")
            .and_then(|f| f.get("message"))
            .and_then(Value::as_str)
        {
            let msg = if message.chars().count() > 200 {
                format!("{}...", message.chars().take(200).collect::<String>())
            } else {
                message.to_string()
            };
            lines.push(format!("- Latest failure: {}", msg));
        }
    }
    lines
}

pub fn select_recent_transcript(transcript: &[TranscriptRow], window: usize) -> Vec<TranscriptRow> {
    let split = transcript.len().saturating_sub(window);
    let mut recent: Vec<TranscriptRow> = transcript[split..].to_vec();
    let mut recent_keys: std::collections::HashSet<String> =
        recent.iter().map(row_key).collect();
    let earlier = &transcript[..split];

    for role in [
        "human",
        ACTOR_CLAUDE,
        ACTOR_CODEX,
        ACTOR_CURSOR,
        ACTOR_OPENCODE,
        ACTOR_KIMI,
    ] {
        if recent.iter().any(|item| item.role == role) {
            continue;
        }
        if let Some(last) = earlier.iter().rev().find(|item| item.role == role) {
            let key = row_key(last);
            if !recent_keys.contains(&key) {
                recent.insert(0, last.clone());
                recent_keys.insert(key);
            }
        }
    }

    // Stable sort, mirroring JS Array.prototype.sort.
    recent.sort_by_key(seq_value);
    recent
}

pub fn detect_human_language(
    transcript: &[TranscriptRow],
    user_message: &str,
    task_text: &str,
    context_text: &str,
) -> String {
    let mut text = user_message.trim();
    if text.is_empty() {
        text = transcript
            .iter()
            .rev()
            .find(|item| item.role == "human")
            .map(|item| item.content.trim())
            .unwrap_or("");
    }
    if text.is_empty() {
        text = task_text.trim();
    }
    if text.is_empty() {
        text = context_text.trim();
    }
    if text.is_empty() {
        return String::new();
    }

    let cjk_count = text
        .chars()
        .filter(|&ch| ('\u{4e00}'..='\u{9fff}').contains(&ch) || ('\u{3400}'..='\u{4dbf}').contains(&ch))
        .count();
    let non_space = text.chars().filter(|ch| !ch.is_whitespace()).count();
    if non_space > 0 && (cjk_count as f64 / non_space as f64) > 0.1 {
        "中文".to_string()
    } else {
        "English".to_string()
    }
}

pub fn next_actor(actor: &str, settings: &Value) -> String {
    let implementer = settings
        .get("implementer_actor")
        .and_then(Value::as_str)
        .unwrap_or(ACTOR_CLAUDE);
    let reviewer = settings
        .get("reviewer_actor")
        .and_then(Value::as_str)
        .unwrap_or(ACTOR_CODEX);
    if actor == implementer {
        reviewer.to_string()
    } else {
        implementer.to_string()
    }
}

pub fn implementer_actor(settings: &Value) -> String {
    settings
        .get("implementer_actor")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| {
            if settings.get("role_mode").and_then(Value::as_str) == Some(ROLE_MODE_CODEX_IMPL) {
                ACTOR_CODEX.to_string()
            } else {
                ACTOR_CLAUDE.to_string()
            }
        })
}

pub fn actor_display_name(actor: &str) -> String {
    match actor {
        ACTOR_CLAUDE => "Claude Code".to_string(),
        ACTOR_OPENCODE => "OpenCode".to_string(),
        ACTOR_KIMI => "Kimi Code".to_string(),
        ACTOR_CODEX => "Codex".to_string(),
        ACTOR_CURSOR => "Cursor".to_string(),
        other => {
            if other.is_empty() {
                "Codex".to_string()
            } else {
                other.to_string()
            }
        }
    }
}

pub fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

fn row_key(item: &TranscriptRow) -> String {
    let seq = seq_value(item);
    if seq != 0 {
        seq.to_string()
    } else {
        format!("{}:{}:{}", item.role, item.ts, item.content)
    }
}

fn seq_value(item: &TranscriptRow) -> u64 {
    item.seq.unwrap_or(0)
}

fn number_value(value: Option<&Value>, fallback: f64) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(fallback),
        _ => fallback,
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// Format a number the way JS `${number}` renders the integer values used in
/// runtime settings (no `.0` suffix).
fn js_number_string(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

// ---------------------------------------------------------------------------
// Tests (port of tests/unit/main/buddy-prompts.test.ts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_input(actor: &str) -> BuildActorPromptInput {
        BuildActorPromptInput {
            actor: actor.to_string(),
            round: 1,
            repo_root: "/tmp/repo".to_string(),
            task_text: "Build feature".to_string(),
            context_text: String::new(),
            transcript: Vec::new(),
            settings: None,
            state: None,
            global_settings: None,
            user_message: None,
        }
    }

    fn claude_implements_settings() -> Value {
        json!({
            "role_mode": "claude_implements",
            "flow_policy": "claude_then_codex",
            "launchers": {}
        })
    }

    #[test]
    fn includes_task_context_actor_round_and_repo_root() {
        let mut input = base_input("claude");
        input.context_text = "Use tests".to_string();
        let prompt = build_actor_prompt(&input);

        assert!(prompt.contains("claude"));
        assert!(prompt.contains("/tmp/repo"));
        assert!(prompt.contains("Build feature"));
        assert!(prompt.contains("Use tests"));
    }

    #[test]
    fn matches_buddy_python_prompt_sections_and_runtime_settings() {
        let mut input = base_input("claude");
        input.round = 4;
        input.task_text = "# Demo".to_string();
        input.context_text = "Use tests".to_string();
        input.settings = Some(claude_implements_settings());
        input.global_settings = Some(GlobalSettings {
            max_rounds: Some(10),
            ..Default::default()
        });
        input.state = Some(json!({
            "round": 4,
            "rounds_in_window": 3,
            "context_hash": "old",
            "context_sent": { "claude": false, "codex": false }
        }));
        let prompt = build_actor_prompt(&input);

        assert!(prompt.contains("# buddy actor turn"));
        assert!(prompt.contains("## Buddy Message Protocol"));
        assert!(prompt.contains("## Background context"));
        assert!(prompt.contains("## Runtime settings"));
        assert!(prompt.contains("Automatic rounds used in this window: 3/10"));
        assert!(prompt.contains("Automatic rounds remaining in this window: 7"));
        assert!(prompt.contains("Next actor after this turn: codex"));
        assert!(prompt.contains("Continue the implementation work"));
    }

    #[test]
    fn uses_reviewer_instructions_when_actor_is_not_the_configured_implementer() {
        let mut input = base_input("claude");
        input.settings = Some(json!({
            "role_mode": "codex_implements",
            "flow_policy": "claude_then_codex",
            "launchers": {}
        }));
        input.state = Some(json!({ "round": 0, "rounds_in_window": 0 }));
        let prompt = build_actor_prompt(&input);

        assert!(prompt.contains("Review the current task state"));
        assert!(!prompt.contains("Continue the implementation work"));
    }

    #[test]
    fn asks_the_second_actor_to_confirm_or_reject_pending_break() {
        let mut input = base_input("codex");
        input.round = 2;
        input.settings = Some(claude_implements_settings());
        input.state = Some(json!({
            "round": 1,
            "rounds_in_window": 1,
            "pending_break": { "actor": "claude", "round": 1 }
        }));
        let prompt = build_actor_prompt(&input);

        assert!(prompt.contains("## Break confirmation required"));
        assert!(prompt.contains("Claude Code has signaled `type=break`"));
        assert!(prompt.contains("Confirm with `type=break` or continue with `type=chat`"));
    }

    #[test]
    fn selects_recent_transcript_while_preserving_missing_actor_and_human_rows() {
        let mut transcript = vec![
            TranscriptRow {
                seq: Some(1),
                role: "claude".to_string(),
                content: "claude earlier".to_string(),
                ts: String::new(),
            },
            TranscriptRow {
                seq: Some(2),
                role: "human".to_string(),
                content: "human earlier".to_string(),
                ts: String::new(),
            },
        ];
        for index in 0..8u64 {
            transcript.push(TranscriptRow {
                seq: Some(index + 3),
                role: "codex".to_string(),
                content: format!("codex {}", index + 3),
                ts: String::new(),
            });
        }

        let mut input = base_input("codex");
        input.round = 10;
        input.transcript = transcript;
        input.settings = Some(claude_implements_settings());
        input.state = Some(json!({ "round": 9, "rounds_in_window": 9 }));
        let prompt = build_actor_prompt(&input);

        assert!(prompt.contains("## Recent transcript"));
        assert!(prompt.contains("claude earlier"));
        assert!(prompt.contains("human earlier"));
        assert!(prompt.contains("codex 10"));
        assert!(!prompt.contains("codex 3"));
    }

    #[test]
    fn places_the_detected_human_language_rule_as_the_last_instruction_line() {
        let mut input = base_input("codex");
        input.user_message = Some("请修复这个问题".to_string());
        input.settings = Some(claude_implements_settings());
        input.state = Some(json!({ "round": 0, "rounds_in_window": 0 }));
        let prompt = build_actor_prompt(&input);

        let last_line = prompt.trim().split('\n').next_back().unwrap();
        assert!(last_line.contains("中文"));
        assert!(last_line.contains("自然语言"));
    }

    // -----------------------------------------------------------------------
    // custom_prompt
    // -----------------------------------------------------------------------

    fn build_with_global_settings(global_settings: Option<GlobalSettings>) -> String {
        let mut input = base_input("claude");
        input.settings = Some(claude_implements_settings());
        input.state = Some(json!({ "round": 0, "rounds_in_window": 0 }));
        input.global_settings = global_settings;
        build_actor_prompt(&input)
    }

    #[test]
    fn does_not_add_a_custom_instructions_section_when_unset() {
        let prompt = build_with_global_settings(None);
        assert!(!prompt.contains("## Custom instructions"));
    }

    #[test]
    fn appends_the_custom_prompt_as_the_final_section_after_the_system_prompt() {
        let prompt = build_with_global_settings(Some(GlobalSettings {
            custom_prompt: Some("Always run pnpm test before reporting done.".to_string()),
            ..Default::default()
        }));

        assert!(prompt.contains("## Custom instructions"));
        assert!(prompt.contains("Always run pnpm test before reporting done."));

        // Custom instructions come after the built-in implementer instruction.
        let instruction_idx = prompt.find("Continue the implementation work").unwrap();
        let custom_idx = prompt
            .find("Always run pnpm test before reporting done.")
            .unwrap();
        assert!(custom_idx > instruction_idx);

        // And it is the last section of the assembled prompt.
        let last_line = prompt.trim().split('\n').next_back().unwrap();
        assert_eq!(last_line, "Always run pnpm test before reporting done.");
    }

    #[test]
    fn treats_whitespace_only_custom_prompt_as_unset() {
        let prompt = build_with_global_settings(Some(GlobalSettings {
            custom_prompt: Some("   ".to_string()),
            ..Default::default()
        }));
        assert!(!prompt.contains("## Custom instructions"));
    }
}
