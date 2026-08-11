//! Parsers for the streaming JSON output of the five actor CLIs
//! (claude / codex / cursor / opencode / kimi). Port of
//! `src/main/buddy/parsers.ts` from the Electron edition.

use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedActorLine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_type: Option<String>,
    /// True for noise events (e.g. system/hook) that carry no actor content.
    #[serde(default, skip_serializing_if = "is_false")]
    pub noise: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BuddyMessage {
    Break {
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Message {
        text: String,
    },
}

// ---------------------------------------------------------------------------
// Regexes (OnceLock, matching the TS module-level patterns)
// ---------------------------------------------------------------------------

fn buddy_json_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\{\s*"type"\s*:\s*"(chat|break)""#).unwrap())
}

fn fenced_json_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)```json\s*(\{[\s\S]*\})\s*```").unwrap())
}

fn type_field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""type"\s*:\s*"(chat|break)""#).unwrap())
}

fn content_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""content"\s*:\s*""#).unwrap())
}

// ---------------------------------------------------------------------------
// Small JS-semantics helpers
// ---------------------------------------------------------------------------

fn text_value(value: &Value) -> Option<String> {
    value.as_str().map(String::from)
}

fn object_value(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|obj| obj.get(key))
}

fn get_text(value: &Value, key: &str) -> Option<String> {
    get(value, key).and_then(text_value)
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

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{}…", head.trim_end())
    }
}

fn stringify_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => serde_json::to_string(other).ok(),
    }
}

fn text_from_content_part(part: &Value) -> String {
    let Some(candidate) = object_value(part) else {
        return String::new();
    };
    let part_type = candidate.get("type").and_then(Value::as_str);
    if part_type == Some("text") || part_type == Some("output_text") {
        candidate
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------------
// Per-actor line parsers
// ---------------------------------------------------------------------------

pub fn parse_claude_stream_line(line: &str) -> serde_json::Result<ParsedActorLine> {
    let json: Value = serde_json::from_str(line)?;
    let text = get(&json, "message")
        .and_then(|m| get(m, "content"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|part| {
                    get(part, "type").and_then(Value::as_str) == Some("text")
                        && get_text(part, "text").map(|t| !t.is_empty()).unwrap_or(false)
                })
                .filter_map(|part| get_text(part, "text"))
                .collect::<Vec<_>>()
                .join("")
        });

    let is_hook = get(&json, "type").and_then(Value::as_str) == Some("system")
        && get(&json, "subtype")
            .and_then(Value::as_str)
            .map(|s| s.starts_with("hook_"))
            .unwrap_or(false);

    Ok(ParsedActorLine {
        text,
        session_id: claude_session_id_from_event(&json),
        raw_type: get_text(&json, "type"),
        noise: is_hook,
        ..Default::default()
    })
}

pub fn parse_codex_json_line(line: &str) -> serde_json::Result<ParsedActorLine> {
    let json: Value = serde_json::from_str(line)?;
    let mut text: Option<String> = None;

    if let Some(content) = get(&json, "content").and_then(Value::as_array) {
        let mut parts: Vec<String> = Vec::new();
        for part in content {
            let part_type = get(part, "type").and_then(Value::as_str);
            if (part_type == Some("text") || part_type == Some("output_text"))
                && get_text(part, "text").is_some()
            {
                parts.push(get_text(part, "text").unwrap());
            } else if part_type == Some("tool_call") && get_text(part, "name").is_some() {
                let name = get_text(part, "name").unwrap();
                let detail = codex_tool_detail(&name, get(part, "input"));
                parts.push(match detail {
                    Some(d) => format!("🔧 {} {}", name, d),
                    None => format!("🔧 {}", name),
                });
            }
        }
        let joined = parts.join("");
        if !joined.is_empty() {
            text = Some(joined);
        }
    }

    if text.is_none() {
        let item_text = get(&json, "item")
            .filter(|item| item.is_object())
            .and_then(|item| get_text(item, "text"));
        if let Some(item_text) = item_text {
            text = Some(item_text);
        } else if let Some(message) = get(&json, "message")
            .filter(|v| js_truthy(v))
            .and_then(text_value)
        {
            text = Some(message);
        }
    }

    Ok(ParsedActorLine {
        text,
        session_id: stable_session_id_from_event("codex", &json),
        thread_id: stable_thread_id_from_event("codex", &json).or_else(|| get_text(&json, "thread_id")),
        raw_type: get_text(&json, "type"),
        ..Default::default()
    })
}

/// Parse Cursor Agent CLI's --output-format stream-json events.
pub fn parse_cursor_stream_line(line: &str) -> serde_json::Result<ParsedActorLine> {
    let json: Value = serde_json::from_str(line)?;
    let content = get(&json, "message").and_then(|m| get(m, "content"));
    let mut text: Option<String> = None;

    if let Some(content) = content.and_then(Value::as_array) {
        let joined = content
            .iter()
            .map(text_from_content_part)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            text = Some(joined);
        }
    }

    if text.is_none() && get(&json, "type").and_then(Value::as_str) == Some("result") {
        text = get_text(&json, "result");
    }

    Ok(ParsedActorLine {
        text,
        session_id: cursor_session_id_from_event(&json),
        raw_type: get_text(&json, "type"),
        ..Default::default()
    })
}

pub fn parse_opencode_json_line(line: &str) -> serde_json::Result<ParsedActorLine> {
    let json: Value = serde_json::from_str(line)?;
    let part = get(&json, "part").and_then(object_value);
    let event_type = get(&json, "type").and_then(Value::as_str);
    let mut text: Option<String> = None;

    let session_id = || {
        stable_session_id_from_event("opencode", &json).or_else(|| get_text(&json, "sessionID"))
    };

    match event_type {
        Some("text") => {
            text = part.and_then(|p| text_value(p.get("text")?));
        }
        Some("error") => {
            text = get(&json, "error").and_then(stringify_value);
        }
        Some("step_start") => {
            // step_start is a lifecycle event, not actor content.
            // Mark as noise so downstream logic doesn't treat the placeholder "..."
            // as a valid buddy message (which would cause infinite loops when the
            // actor's context is exhausted and only step_start events are emitted).
            return Ok(ParsedActorLine {
                text: Some("...".to_string()),
                session_id: session_id(),
                raw_type: get_text(&json, "type"),
                noise: true,
                ..Default::default()
            });
        }
        Some("step_finish") => {
            return Ok(ParsedActorLine {
                session_id: session_id(),
                raw_type: get_text(&json, "type"),
                noise: true,
                ..Default::default()
            });
        }
        Some("tool_use") => {
            let tool_name = part
                .and_then(|p| p.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let state = part.and_then(|p| p.get("state")).and_then(object_value);
            let input = state
                .and_then(|s| s.get("input"))
                .filter(|v| !v.is_null())
                .or_else(|| part.and_then(|p| p.get("input")));
            let output = state.and_then(|s| text_value(s.get("output")?));
            // If tool output contains a buddy message, show it directly (e.g. echo commands)
            if let Some(output) = output {
                if buddy_json_re().is_match(&output) {
                    text = Some(output.trim().to_string());
                    return Ok(ParsedActorLine {
                        text,
                        session_id: session_id(),
                        raw_type: get_text(&json, "type"),
                        ..Default::default()
                    });
                }
            }
            let detail = input.and_then(|i| opencode_tool_detail(&tool_name, i));
            text = Some(match detail {
                Some(d) => format!("🔧 {} {}", tool_name, d),
                None => format!("🔧 {}", tool_name),
            });
        }
        _ => {}
    }

    Ok(ParsedActorLine {
        text,
        session_id: session_id(),
        raw_type: get_text(&json, "type"),
        ..Default::default()
    })
}

pub fn parse_kimi_json_line(line: &str) -> serde_json::Result<ParsedActorLine> {
    let json: Value = serde_json::from_str(line)?;
    let part = get(&json, "part").and_then(object_value);
    let event_type = get(&json, "type").and_then(Value::as_str);
    let mut text: Option<String> = None;

    let resume_hint_session = || {
        if get(&json, "role").and_then(Value::as_str) == Some("meta")
            && event_type == Some("session.resume_hint")
        {
            get_text(&json, "session_id")
        } else {
            None
        }
    };

    // New stream-json format (mirrors OpenCode event types)
    match event_type {
        Some("text") => {
            text = part.and_then(|p| text_value(p.get("text")?));
        }
        Some("error") => {
            text = get(&json, "error").and_then(stringify_value);
        }
        Some("step_start") => {
            return Ok(ParsedActorLine {
                text: Some("...".to_string()),
                session_id: stable_session_id_from_event("kimi", &json)
                    .or_else(resume_hint_session)
                    .or_else(|| get_text(&json, "sessionID")),
                raw_type: get_text(&json, "type"),
                noise: true,
                ..Default::default()
            });
        }
        Some("step_finish") => {
            return Ok(ParsedActorLine {
                session_id: stable_session_id_from_event("kimi", &json)
                    .or_else(|| get_text(&json, "sessionID")),
                raw_type: get_text(&json, "type"),
                noise: true,
                ..Default::default()
            });
        }
        Some("tool_use") => {
            let tool_name = part
                .and_then(|p| p.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let state = part.and_then(|p| p.get("state")).and_then(object_value);
            let input = state
                .and_then(|s| s.get("input"))
                .filter(|v| !v.is_null())
                .or_else(|| part.and_then(|p| p.get("input")));
            let output = state.and_then(|s| text_value(s.get("output")?));
            if let Some(output) = output {
                if buddy_json_re().is_match(&output) {
                    text = Some(output.trim().to_string());
                    return Ok(ParsedActorLine {
                        text,
                        session_id: kimi_session_id(&json, resume_hint_session),
                        raw_type: kimi_raw_type(&json),
                        ..Default::default()
                    });
                }
            }
            let detail = kimi_tool_detail(&tool_name, input);
            text = Some(match detail {
                Some(d) => format!("🔧 {} {}", tool_name, d),
                None => format!("🔧 {}", tool_name),
            });
        }
        _ => {
            if get(&json, "role").and_then(Value::as_str) == Some("assistant") {
                // Legacy OpenAI-compatible format
                text = get_text(&json, "content");
                if let Some(tool_calls) = get(&json, "tool_calls").and_then(Value::as_array) {
                    let tool_texts: Vec<String> = tool_calls
                        .iter()
                        .map(|tc| {
                            let function = get(tc, "function").and_then(object_value);
                            let name = function
                                .and_then(|f| text_value(f.get("name")?))
                                .or_else(|| get_text(tc, "name"))
                                .unwrap_or_else(|| "tool".to_string());
                            let args = function.and_then(|f| f.get("arguments"));
                            let detail = kimi_tool_detail(&name, args);
                            match detail {
                                Some(d) => format!("🔧 {} {}", name, d),
                                None => format!("🔧 {}", name),
                            }
                        })
                        .collect();
                    if !tool_texts.is_empty() {
                        text = Some(tool_texts.join(" "));
                    }
                }
            }
        }
    }

    Ok(ParsedActorLine {
        text,
        session_id: kimi_session_id(&json, resume_hint_session),
        raw_type: kimi_raw_type(&json),
        ..Default::default()
    })
}

fn kimi_session_id(
    json: &Value,
    resume_hint_session: impl FnOnce() -> Option<String>,
) -> Option<String> {
    stable_session_id_from_event("kimi", json)
        .or_else(resume_hint_session)
        .or_else(|| get_text(json, "sessionID"))
}

fn kimi_raw_type(json: &Value) -> Option<String> {
    get_text(json, "type").or_else(|| get_text(json, "role"))
}

pub fn parse_actor_line(actor: &str, line: &str) -> serde_json::Result<ParsedActorLine> {
    match actor {
        "claude" => parse_claude_stream_line(line),
        "codex" => parse_codex_json_line(line),
        "cursor" => parse_cursor_stream_line(line),
        "opencode" => parse_opencode_json_line(line),
        "kimi" => parse_kimi_json_line(line),
        _ => parse_codex_json_line(line),
    }
}

pub fn parse_actor_events(actor: &str, raw_events: &str) -> Vec<ParsedActorLine> {
    let mut out = Vec::new();
    for raw in raw_events.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        match parse_actor_line(actor, raw) {
            Ok(parsed) => out.push(parsed),
            Err(_) => out.push(ParsedActorLine {
                text: Some(raw.to_string()),
                ..Default::default()
            }),
        }
    }
    out
}

pub fn extract_actor_output(actor: &str, raw_events: &str) -> String {
    match actor {
        "claude" => extract_claude_output(raw_events),
        "cursor" => extract_cursor_output(raw_events),
        "opencode" => extract_opencode_output(raw_events),
        "kimi" => extract_kimi_output(raw_events),
        _ => extract_generic_json_output(raw_events),
    }
}

// ---------------------------------------------------------------------------
// Buddy message protocol parsing
// ---------------------------------------------------------------------------

pub fn parse_buddy_message(text: &str) -> BuddyMessage {
    let trimmed = text.trim();
    if let Some(json_message) = parse_buddy_json_message(trimmed) {
        return json_message;
    }

    let mut fields: Map<String, Value> = Map::new();
    for line in trimmed.lines() {
        if let Some(index) = line.find('=') {
            fields.insert(
                line[..index].to_string(),
                Value::String(line[index + 1..].to_string()),
            );
        }
    }

    if fields.get("type").and_then(Value::as_str) == Some("break") {
        let reason = fields.get("reason").and_then(Value::as_str).map(String::from);
        return BuddyMessage::Break {
            content: reason.clone().unwrap_or_else(|| text.to_string()),
            reason,
        };
    }

    BuddyMessage::Message {
        text: text.to_string(),
    }
}

fn parse_buddy_json_message(text: &str) -> Option<BuddyMessage> {
    if let Some(fenced) = fenced_json_re().captures(text) {
        let inner = fenced.get(1).unwrap().as_str();
        if let Some(parsed) = parse_buddy_json_candidate(inner) {
            return Some(parsed);
        }
        if let Some(loose) = loose_extract_buddy_message(inner) {
            return Some(loose);
        }
    }

    if let Some(parsed) = parse_buddy_json_candidate(text) {
        return Some(parsed);
    }

    if let Some(loose) = loose_extract_buddy_message(text) {
        return Some(loose);
    }

    if let Some(obj) = find_buddy_json_object(text) {
        if let Some(parsed) = parse_buddy_json_candidate(obj) {
            return Some(parsed);
        }
        if let Some(loose) = loose_extract_buddy_message(obj) {
            return Some(loose);
        }
    }

    if let Some(unescaped) = try_unescape_json(text) {
        let uobj = find_buddy_json_object(&unescaped).unwrap_or(&unescaped);
        if let Some(parsed) = parse_buddy_json_candidate(uobj) {
            return Some(parsed);
        }
        if let Some(loose) = loose_extract_buddy_message(uobj) {
            return Some(loose);
        }
    }

    None
}

fn find_buddy_json_object(text: &str) -> Option<&str> {
    let m = buddy_json_re().find(text)?;
    let start = m.start();
    let bytes = text.as_bytes();

    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut in_backtick = false;
    let mut escape = false;

    let mut i = start;
    while i < bytes.len() {
        let ch = bytes[i];
        if in_backtick {
            if ch == b'`' {
                in_backtick = false;
            }
            i += 1;
            continue;
        }
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if ch == b'\\' {
            escape = true;
            i += 1;
            continue;
        }
        if ch == b'`' && !in_string {
            in_backtick = true;
            i += 1;
            continue;
        }
        if ch == b'"' {
            in_string = !in_string;
            i += 1;
            continue;
        }
        if in_string {
            i += 1;
            continue;
        }
        if ch == b'{' {
            depth += 1;
        }
        if ch == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(&text[start..=i]);
            }
        }
        i += 1;
    }

    None
}

fn try_unescape_json(text: &str) -> Option<String> {
    if !text.contains("\\\"") {
        return None;
    }
    let unescaped = text.replace("\\\"", "\"");
    if !buddy_json_re().is_match(&unescaped) {
        return None;
    }
    Some(unescaped)
}

fn find_closing_content_quote(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut pos = bytes.len() as isize - 1;

    while pos >= 0 && matches!(bytes[pos as usize], b' ' | b'\t' | b'\n' | b'\r') {
        pos -= 1;
    }
    if pos < 0 || bytes[pos as usize] != b'}' {
        return None;
    }
    pos -= 1;
    while pos >= 0 && matches!(bytes[pos as usize], b' ' | b'\t' | b'\n' | b'\r') {
        pos -= 1;
    }
    if pos < 0 || bytes[pos as usize] != b'"' {
        return None;
    }

    Some(pos as usize)
}

fn parse_buddy_json_candidate(text: &str) -> Option<BuddyMessage> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let obj = parsed.as_object()?;
    let msg_type = obj.get("type").and_then(Value::as_str);
    let content = obj.get("content").and_then(Value::as_str);
    match (msg_type, content) {
        (Some("break"), Some(content)) => Some(BuddyMessage::Break {
            content: content.to_string(),
            reason: None,
        }),
        (Some("chat"), Some(content)) => Some(BuddyMessage::Message {
            text: content.to_string(),
        }),
        _ => None,
    }
}

fn loose_extract_buddy_message(text: &str) -> Option<BuddyMessage> {
    let type_match = type_field_re().captures(text)?;
    let kind = type_match.get(1).unwrap().as_str();
    let type_start = type_match.get(0).unwrap().start();

    // Search for "content":" AFTER the "type" match to avoid picking up
    // "content" keys from unrelated JSON structures (e.g. tool_result)
    // that appear before the buddy JSON in the text.
    let after_type = &text[type_start..];
    let content_key_match = content_key_re().find(after_type)?;

    let content_start = type_start + content_key_match.end();
    let closing_quote = find_closing_content_quote(text);
    let raw = match closing_quote {
        Some(pos) if pos > content_start => &text[content_start..pos],
        _ => &text[content_start..],
    };
    let content = unescape_json_string(raw);

    Some(if kind == "break" {
        BuddyMessage::Break {
            content,
            reason: None,
        }
    } else {
        BuddyMessage::Message { text: content }
    })
}

fn unescape_json_string(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r")
        .replace("\\\\", "\\")
        .replace("\\\"", "\"")
}

// ---------------------------------------------------------------------------
// Output extraction
// ---------------------------------------------------------------------------

fn extract_claude_output(raw_events: &str) -> String {
    let mut result = String::new();
    let mut chunks: Vec<String> = Vec::new();
    for event in parse_jsonl_buffer(raw_events) {
        if get(&event, "type").and_then(Value::as_str) == Some("result") {
            if let Some(event_result) = get_text(&event, "result") {
                if !event_result.is_empty() {
                    result = event_result;
                }
            }
        }
        if let Some(text) = get_text(&event, "text") {
            if !text.is_empty() {
                chunks.push(text);
            }
        }
        let content = get(&event, "message").and_then(|m| get(m, "content"));
        if let Some(content) = content.and_then(Value::as_array) {
            chunks.extend(
                content
                    .iter()
                    .map(text_from_content_part)
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    (if !result.is_empty() {
        result
    } else {
        chunks.join("\n")
    })
    .trim()
    .to_string()
}

fn extract_cursor_output(raw_events: &str) -> String {
    let mut result = String::new();
    let mut chunks: Vec<String> = Vec::new();
    for event in parse_jsonl_buffer(raw_events) {
        if get(&event, "type").and_then(Value::as_str) == Some("result") {
            if let Some(final_text) = get_text(&event, "result") {
                if !final_text.is_empty() {
                    result = final_text;
                }
            }
        }
        let content = get(&event, "message").and_then(|m| get(m, "content"));
        if let Some(content) = content.and_then(Value::as_array) {
            chunks.extend(
                content
                    .iter()
                    .map(text_from_content_part)
                    .filter(|s| !s.is_empty()),
            );
        }
    }
    (if !result.is_empty() {
        result
    } else {
        chunks.join("")
    })
    .trim()
    .to_string()
}

fn extract_opencode_output(raw_events: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for event in parse_jsonl_buffer(raw_events) {
        match get(&event, "type").and_then(Value::as_str) {
            Some("text") => {
                let text = get(&event, "part")
                    .and_then(object_value)
                    .and_then(|p| text_value(p.get("text")?));
                if let Some(text) = text {
                    chunks.push(text);
                }
            }
            Some("error") => {
                if let Some(error) = get(&event, "error").and_then(stringify_value) {
                    chunks.push(error);
                }
            }
            Some("tool_use") => {
                // Some models (e.g. DeepSeek) output buddy JSON via echo/bash commands.
                // The buddy message appears in part.state.output of tool_use events.
                let output = get(&event, "part")
                    .and_then(object_value)
                    .and_then(|p| p.get("state"))
                    .and_then(object_value)
                    .and_then(|s| text_value(s.get("output")?));
                if let Some(output) = output {
                    if buddy_json_re().is_match(&output) {
                        chunks.push(format!("\n{}", output.trim()));
                    }
                }
            }
            _ => {}
        }
    }
    chunks.join("").trim().to_string()
}

fn extract_kimi_output(raw_events: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut legacy_last = String::new();
    for event in parse_jsonl_buffer(raw_events) {
        match get(&event, "type").and_then(Value::as_str) {
            Some("text") => {
                let text = get(&event, "part")
                    .and_then(object_value)
                    .and_then(|p| text_value(p.get("text")?));
                if let Some(text) = text {
                    chunks.push(text);
                }
            }
            Some("error") => {
                if let Some(error) = get(&event, "error").and_then(stringify_value) {
                    chunks.push(error);
                }
            }
            Some("tool_use") => {
                let output = get(&event, "part")
                    .and_then(object_value)
                    .and_then(|p| p.get("state"))
                    .and_then(object_value)
                    .and_then(|s| text_value(s.get("output")?));
                if let Some(output) = output {
                    if buddy_json_re().is_match(&output) {
                        chunks.push(format!("\n{}", output.trim()));
                    }
                }
            }
            _ => {
                if get(&event, "role").and_then(Value::as_str) == Some("assistant") {
                    // Legacy OpenAI-compatible format: each event is a full message, keep last
                    if let Some(content) = get_text(&event, "content") {
                        legacy_last = content;
                    }
                }
            }
        }
    }
    let stream_text = chunks.join("").trim().to_string();
    if !stream_text.is_empty() {
        stream_text
    } else {
        legacy_last.trim().to_string()
    }
}

fn extract_generic_json_output(raw_events: &str) -> String {
    let mut chunks: Vec<String> = Vec::new();
    for event in parse_jsonl_buffer(raw_events) {
        let item_text = get(&event, "item")
            .and_then(object_value)
            .and_then(|i| text_value(i.get("text")?));
        let message = get_text(&event, "message");
        let content = get(&event, "content");
        if let Some(content) = content.and_then(Value::as_array) {
            chunks.extend(
                content
                    .iter()
                    .map(text_from_content_part)
                    .filter(|s| !s.is_empty()),
            );
        } else if let Some(item_text) = item_text.filter(|t| !t.is_empty()) {
            chunks.push(item_text);
        } else if let Some(message) = message.filter(|m| !m.is_empty()) {
            chunks.push(message);
        }
    }
    chunks.join("\n").trim().to_string()
}

// ---------------------------------------------------------------------------
// JSONL buffer with broken-event recovery
// ---------------------------------------------------------------------------

pub fn parse_jsonl_buffer(raw: &str) -> Vec<Value> {
    let mut results: Vec<Value> = Vec::new();
    let mut buffer = String::new();

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // If we have a stale buffer from a previous broken event and this line
        // starts a new JSON object, try to salvage the buffer first, then
        // discard it if it can't be parsed — don't let one broken event
        // swallow all subsequent events.
        if !buffer.is_empty() && line.starts_with('{') {
            if let Ok(obj) = serde_json::from_str::<Value>(&buffer) {
                if obj.is_object() {
                    results.push(obj);
                }
            }
            buffer.clear();
        }

        buffer = if buffer.is_empty() {
            line.to_string()
        } else {
            format!("{}\n{}", buffer, line)
        };
        match serde_json::from_str::<Value>(&buffer) {
            Ok(obj) => {
                if obj.is_object() {
                    results.push(obj);
                }
                buffer.clear();
            }
            Err(_) => {
                // incomplete JSON, keep accumulating
            }
        }
    }

    if !buffer.trim().is_empty() {
        if let Ok(obj) = serde_json::from_str::<Value>(&buffer) {
            if obj.is_object() {
                results.push(obj);
            }
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Session-id extraction
// ---------------------------------------------------------------------------

fn claude_session_id_from_event(event: &Value) -> Option<String> {
    let session_id = get_text(event, "session_id")?;
    if session_id.is_empty() {
        return None;
    }

    let event_type = get(event, "type").and_then(Value::as_str);
    let subtype = get(event, "subtype").and_then(Value::as_str).unwrap_or("");
    if event_type == Some("system") {
        if subtype == "init" {
            return Some(session_id);
        }
        if subtype.starts_with("hook_")
            || get(event, "hook_event").map(js_truthy).unwrap_or(false)
        {
            return None;
        }
    }
    if matches!(event_type, Some("result") | Some("assistant") | Some("user")) {
        return Some(session_id);
    }
    if event_type != Some("system") {
        return Some(session_id);
    }
    None
}

fn cursor_session_id_from_event(event: &Value) -> Option<String> {
    let session_id = get_text(event, "session_id");
    session_id.filter(|s| !s.is_empty())
}

fn stable_session_id_from_event(actor: &str, event: &Value) -> Option<String> {
    if get(event, "type").and_then(Value::as_str) != Some("buddy.session") {
        return None;
    }
    if get(event, "actor").and_then(Value::as_str) != Some(actor) {
        return None;
    }
    if actor == "codex" {
        return None;
    }
    get_text(event, "session_id")
}

fn stable_thread_id_from_event(actor: &str, event: &Value) -> Option<String> {
    let event_type = get(event, "type").and_then(Value::as_str);
    let event_actor = get(event, "actor").and_then(Value::as_str);
    if actor == "codex" && event_type == Some("buddy.session") && event_actor == Some("codex") {
        return get_text(event, "thread_id").or_else(|| get_text(event, "session_id"));
    }
    if actor == "codex" && event_type == Some("thread.started") {
        return get_text(event, "thread_id");
    }
    None
}

// ---------------------------------------------------------------------------
// Tool-call detail helpers
// ---------------------------------------------------------------------------

fn codex_tool_detail(tool_name: &str, input: Option<&Value>) -> Option<String> {
    tool_detail(tool_name, input)
}

fn opencode_tool_detail(tool_name: &str, input: &Value) -> Option<String> {
    tool_detail(tool_name, Some(input))
}

fn kimi_tool_detail(tool_name: &str, args: Option<&Value>) -> Option<String> {
    let args = args?;
    // args may be a JSON string or an object
    if let Value::String(s) = args {
        return match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(obj)) => tool_detail_from_obj(tool_name, &obj),
            // Parsed to a non-object (number, bool, ...): property lookups all
            // yield undefined in JS, so no detail — mirroring the TS fall-through.
            Ok(_) => None,
            Err(_) => Some(truncate(s, 80)),
        };
    }
    if args.is_object() || args.is_array() {
        return tool_detail(tool_name, Some(args));
    }
    None
}

fn tool_detail(tool_name: &str, input: Option<&Value>) -> Option<String> {
    match input? {
        Value::Object(obj) => tool_detail_from_obj(tool_name, obj),
        // JS `typeof [] === 'object'`: Object.values(array) yields its elements.
        Value::Array(arr) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    return Some(truncate(s, 80));
                }
            }
            None
        }
        _ => None,
    }
}

fn tool_detail_from_obj(tool_name: &str, obj: &Map<String, Value>) -> Option<String> {
    // bash/shell: show command
    if tool_name == "bash" || tool_name == "shell" {
        let cmd = obj
            .get("command")
            .and_then(Value::as_str)
            .or_else(|| obj.get("cmd").and_then(Value::as_str));
        if let Some(cmd) = cmd {
            return Some(truncate(cmd, 80));
        }
    }
    // file operations: show path
    let path = obj
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| obj.get("file_path").and_then(Value::as_str))
        .or_else(|| obj.get("file").and_then(Value::as_str));
    if let Some(path) = path {
        return Some(truncate(path, 80));
    }
    // generic: show first string value
    for v in obj.values() {
        if let Some(s) = v.as_str() {
            return Some(truncate(s, 80));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests (port of tests/unit/main/buddy-parsers.test.ts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stringify(v: Value) -> String {
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn extracts_text_from_claude_stream_json_content_blocks() {
        let event = parse_claude_stream_line(&stringify(json!({
            "type": "assistant",
            "message": {
                "content": [{ "type": "text", "text": "hello" }]
            },
            "session_id": "claude-session"
        })))
        .unwrap();

        assert_eq!(event.text.as_deref(), Some("hello"));
        assert_eq!(event.session_id.as_deref(), Some("claude-session"));
    }

    #[test]
    fn extracts_text_from_codex_json_lines() {
        let event = parse_codex_json_line(&stringify(json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "done" }],
            "thread_id": "codex-thread"
        })))
        .unwrap();

        assert_eq!(event.text.as_deref(), Some("done"));
        assert_eq!(event.thread_id.as_deref(), Some("codex-thread"));
    }

    #[test]
    fn extracts_text_from_current_codex_item_completed_agent_messages() {
        let event = parse_codex_json_line(&stringify(json!({
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "text": "{\"type\":\"chat\",\"content\":\"review complete\"}"
            }
        })))
        .unwrap();

        assert_eq!(
            event.text.as_deref(),
            Some("{\"type\":\"chat\",\"content\":\"review complete\"}")
        );
    }

    #[test]
    fn extracts_cursor_stream_json_text_and_preserves_session_id() {
        let event = parse_cursor_stream_line(&stringify(json!({
            "type": "assistant",
            "session_id": "cursor-chat",
            "message": {
                "role": "assistant",
                "content": [{ "type": "text", "text": "{\"type\":\"chat\",\"content\":\"done\"}" }]
            }
        })))
        .unwrap();

        assert_eq!(
            event.text.as_deref(),
            Some("{\"type\":\"chat\",\"content\":\"done\"}")
        );
        assert_eq!(event.session_id.as_deref(), Some("cursor-chat"));
    }

    #[test]
    fn uses_cursor_result_events_as_final_output() {
        let output = extract_actor_output(
            "cursor",
            &[
                stringify(json!({ "type": "assistant", "session_id": "cursor-chat", "message": { "content": [{ "type": "text", "text": "partial " }] } })),
                stringify(json!({ "type": "assistant", "session_id": "cursor-chat", "message": { "content": [{ "type": "text", "text": "output" }] } })),
                stringify(json!({ "type": "result", "subtype": "success", "session_id": "cursor-chat", "result": "{\"type\":\"chat\",\"content\":\"final output\"}" })),
            ]
            .join("\n"),
        );

        assert_eq!(output, "{\"type\":\"chat\",\"content\":\"final output\"}");
    }

    #[test]
    fn extracts_opencode_session_ids_and_text_chunks_while_ignoring_reasoning() {
        let session = parse_actor_line(
            "opencode",
            &stringify(json!({
                "type": "step_start",
                "sessionID": "opencode-session",
                "part": { "type": "step-start" }
            })),
        )
        .unwrap();
        let text = parse_actor_line(
            "opencode",
            &stringify(json!({
                "type": "text",
                "sessionID": "opencode-session",
                "part": { "type": "text", "text": "Hello" }
            })),
        )
        .unwrap();
        let reasoning = extract_actor_output(
            "opencode",
            &stringify(json!({
                "type": "reasoning",
                "sessionID": "opencode-session",
                "part": { "type": "reasoning", "text": "hidden" }
            })),
        );

        assert_eq!(session.session_id.as_deref(), Some("opencode-session"));
        assert_eq!(text.session_id.as_deref(), Some("opencode-session"));
        assert_eq!(text.text.as_deref(), Some("Hello"));
        assert_eq!(reasoning, "");
    }

    #[test]
    fn extracts_buddy_messages_from_opencode_tool_use_output_echo_commands() {
        let output = extract_actor_output(
            "opencode",
            &[
                stringify(json!({ "type": "text", "sessionID": "s1", "part": { "type": "text", "text": "Task done." } })),
                stringify(json!({ "type": "tool_use", "sessionID": "s1", "part": { "type": "tool", "tool": "bash", "callID": "c1", "state": { "status": "completed", "input": { "command": "echo '{\"type\": \"break\", \"content\": \"All done.\"}'" }, "output": "{\"type\": \"break\", \"content\": \"All done.\"}" } } })),
            ]
            .join("\n"),
        );
        let message = parse_buddy_message(&output);

        assert_eq!(
            message,
            BuddyMessage::Break {
                content: "All done.".to_string(),
                reason: None
            }
        );
    }

    #[test]
    fn extracts_buddy_messages_from_opencode_tool_use_when_text_has_no_buddy_json() {
        let output = extract_actor_output(
            "opencode",
            &[
                stringify(json!({ "type": "text", "sessionID": "s1", "part": { "type": "text", "text": "I will signal completion." } })),
                stringify(json!({ "type": "tool_use", "sessionID": "s1", "part": { "type": "tool", "tool": "bash", "callID": "c1", "state": { "status": "completed", "input": { "command": "echo hi" }, "output": "{\"type\": \"break\", \"content\": \"Done.\"}" } } })),
                stringify(json!({ "type": "text", "sessionID": "s1", "part": { "type": "text", "text": "Echo command executed." } })),
            ]
            .join("\n"),
        );
        let message = parse_buddy_message(&output);

        assert!(matches!(message, BuddyMessage::Break { .. }));
    }

    #[test]
    fn streams_buddy_messages_from_opencode_tool_use_events() {
        let event = parse_actor_line(
            "opencode",
            &stringify(json!({
                "type": "tool_use",
                "sessionID": "s1",
                "part": { "type": "tool", "tool": "bash", "callID": "c1", "state": { "status": "completed", "input": { "command": "echo break" }, "output": "{\"type\": \"break\", \"content\": \"Done.\"}" } }
            })),
        )
        .unwrap();

        assert!(event.text.unwrap().contains("\"type\": \"break\""));
    }

    #[test]
    fn keeps_opencode_json_chunks_adjacent_when_extracting_output() {
        let output = extract_actor_output(
            "opencode",
            &[
                stringify(json!({ "type": "text", "sessionID": "s1", "part": { "type": "text", "text": "{\"type\":\"ch" } })),
                stringify(json!({ "type": "text", "sessionID": "s1", "part": { "type": "text", "text": "at\",\"content\":\"hi\"}" } })),
            ]
            .join("\n"),
        );

        assert_eq!(output, "{\"type\":\"chat\",\"content\":\"hi\"}");
    }

    #[test]
    fn keeps_only_last_kimi_assistant_content_when_extracting_output() {
        let output = extract_actor_output(
            "kimi",
            &[
                stringify(json!({ "role": "assistant", "content": "{\"type\":\"chat\",\"content\":\"Part one\"}" })),
                stringify(json!({ "role": "tool", "content": "tool result" })),
                stringify(json!({ "role": "assistant", "content": "{\"type\":\"chat\",\"content\":\"Part two\"}" })),
            ]
            .join("\n"),
        );

        assert_eq!(output, "{\"type\":\"chat\",\"content\":\"Part two\"}");
    }

    #[test]
    fn extracts_stable_sessions_from_buddy_session_events() {
        let event = parse_actor_line(
            "kimi",
            &stringify(json!({
                "type": "buddy.session",
                "actor": "kimi",
                "session_id": "kimi-session"
            })),
        )
        .unwrap();
        assert_eq!(event.session_id.as_deref(), Some("kimi-session"));
    }

    #[test]
    fn extracts_session_id_from_kimi_session_resume_hint_meta_event() {
        let event = parse_actor_line(
            "kimi",
            &stringify(json!({
                "role": "meta",
                "type": "session.resume_hint",
                "session_id": "session_f811580a-d17e-4e01-b900-92048f4b1455",
                "command": "kimi -r session_f811580a-d17e-4e01-b900-92048f4b1455",
                "content": "To resume this session: kimi -r session_f811580a-d17e-4e01-b900-92048f4b1455"
            })),
        )
        .unwrap();
        assert_eq!(
            event.session_id.as_deref(),
            Some("session_f811580a-d17e-4e01-b900-92048f4b1455")
        );
        assert_eq!(event.text, None);
    }

    #[test]
    fn detects_break_messages() {
        match parse_buddy_message("type=break\nreason=done") {
            BuddyMessage::Break { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("done"));
            }
            other => panic!("expected break, got {:?}", other),
        }
    }

    #[test]
    fn unwraps_buddy_json_chat_and_break_envelopes() {
        assert_eq!(
            parse_buddy_message("```json\n{\"type\":\"chat\",\"content\":\"hello\"}\n```"),
            BuddyMessage::Message {
                text: "hello".to_string()
            }
        );
        assert_eq!(
            parse_buddy_message("{\"type\":\"break\",\"content\":\"done\"}"),
            BuddyMessage::Break {
                content: "done".to_string(),
                reason: None
            }
        );
    }

    #[test]
    fn preserves_markdown_content_from_buddy_json_envelopes() {
        let markdown = "## Summary\n\n- Updated `src/main`\n- Kept transcript JSONL compatible\n";
        let message = parse_buddy_message(&stringify(json!({ "type": "chat", "content": markdown })));

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: markdown.to_string()
            }
        );
    }

    #[test]
    fn loosely_extracts_markdown_content_with_unescaped_quotes() {
        let message = parse_buddy_message(
            "```json\n{\"type\": \"chat\", \"content\": \"## 结果\n\n这是一段包含\"引号\"的 markdown\"}\n```",
        );

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "## 结果\n\n这是一段包含\"引号\"的 markdown".to_string()
            }
        );
    }

    #[test]
    fn extracts_buddy_json_preceded_by_preamble_text() {
        let text = "所有测试通过，类型检查通过。让我总结一下所做的更改。\n{\"type\": \"chat\", \"content\": \"## Changes Made\n\n### Root Cause\nThe error was fixed.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "## Changes Made\n\n### Root Cause\nThe error was fixed.".to_string()
            }
        );
    }

    #[test]
    fn extracts_buddy_json_with_unescaped_content_after_preamble() {
        let text = "Preamble text here. {\"type\": \"chat\", \"content\": \"Hello world\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "Hello world".to_string()
            }
        );
    }

    #[test]
    fn extracts_break_message_from_json_with_preamble() {
        let text = "Task complete. {\"type\": \"break\", \"content\": \"All done\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Break {
                content: "All done".to_string(),
                reason: None
            }
        );
    }

    #[test]
    fn extracts_content_with_inline_code_containing_brace_like_patterns() {
        let text = "{\"type\": \"chat\", \"content\": \"The fix uses `spawn kimi ENOENT` error handling in launchers.ts\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "The fix uses `spawn kimi ENOENT` error handling in launchers.ts".to_string()
            }
        );
    }

    #[test]
    fn extracts_multiline_content_with_code_blocks_containing_braces() {
        let text = "{\"type\": \"chat\", \"content\": \"## Changes\\n\\n```json\\n{\\\"key\\\": \\\"value\\\"}\\n```\\n\\nAll done.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "## Changes\n\n```json\n{\"key\": \"value\"}\n```\n\nAll done.".to_string()
            }
        );
    }

    #[test]
    fn extracts_content_containing_quote_brace_pattern_inside_backticks() {
        let text = "{\"type\": \"chat\", \"content\": \"Example closing: `\"} at the end\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "Example closing: `\"} at the end".to_string()
            }
        );
    }

    #[test]
    fn extracts_buddy_json_from_escaped_json_in_preamble() {
        let text = "Preamble \\\"type\\\": \\\"chat\\\" but real message: {\"type\": \"chat\", \"content\": \"Working fix\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "Working fix".to_string()
            }
        );
    }

    #[test]
    fn handles_fully_escaped_json_embedded_in_text() {
        let text = "Summary: {\\\"type\\\": \\\"chat\\\", \\\"content\\\": \\\"All tests pass\\\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "All tests pass".to_string()
            }
        );
    }

    #[test]
    fn extracts_long_multiline_content_with_preamble_intact() {
        let text = "Summary text here.\\n{\"type\": \"chat\", \"content\": \"## Changes Made\\n\\n### Root Cause\\nThe `spawn kimi ENOENT` error occurs.\\n\\n### Files Changed\\n- file1.ts\\n- file2.ts\\n\\n### Verification\\nAll tests pass.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "## Changes Made\n\n### Root Cause\nThe `spawn kimi ENOENT` error occurs.\n\n### Files Changed\n- file1.ts\n- file2.ts\n\n### Verification\nAll tests pass.".to_string()
            }
        );
    }

    #[test]
    fn handles_valid_json_after_preamble_without_quotes_in_preamble() {
        let text = "Done. Here is the summary.\\n\\n{\"type\": \"chat\", \"content\": \"Summary text.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "Summary text.".to_string()
            }
        );
    }

    #[test]
    fn extracts_content_when_buddy_json_has_actual_newlines_in_content_value() {
        let text = "所有修改看起来都正确。现在我将输出伙伴协议消息。\n{\"type\": \"chat\", \"content\": \"## Changes Summary\n\n### Fix 1: Kimi ENOENT error\n\nThe error was fixed.\n- file1.ts\n- file2.ts\n\nAll tests pass.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "## Changes Summary\n\n### Fix 1: Kimi ENOENT error\n\nThe error was fixed.\n- file1.ts\n- file2.ts\n\nAll tests pass.".to_string()
            }
        );
    }

    #[test]
    fn extracts_content_with_actual_newlines_and_inline_code_backticks() {
        let text = "Preamble.\n{\"type\": \"chat\", \"content\": \"The `kimi` binary was not found in PATH.\n\nInstall with `pip install kimi-cli`.\"}";
        let message = parse_buddy_message(text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "The `kimi` binary was not found in PATH.\n\nInstall with `pip install kimi-cli`.".to_string()
            }
        );
    }

    fn claude_stream_with_text(text: &str) -> String {
        stringify(json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": text
                }]
            }
        }))
    }

    #[test]
    fn end_to_end_claude_stream_json_with_preamble_plus_buddy_json() {
        let stream_events = claude_stream_with_text(
            "所有修改看起来都正确。现在我将输出伙伴协议消息。\n\n{\"type\": \"chat\", \"content\": \"## Changes Summary\\n\\n### Fix 1: Kimi ENOENT error\\n\\nAll tests pass.\"}",
        );
        let output_text = extract_actor_output("claude", &stream_events);
        let message = parse_buddy_message(&output_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
                assert!(text.contains("All tests pass."));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_claude_output_with_actual_newlines_in_buddy_json_content() {
        let stream_events = claude_stream_with_text(
            "所有修改看起来都正确。现在我将输出伙伴协议消息。\n\n{\"type\": \"chat\", \"content\": \"## Changes Summary\n\n### Fix 1: Kimi ENOENT error\n\nAll tests pass.\"}",
        );
        let output_text = extract_actor_output("claude", &stream_events);
        let message = parse_buddy_message(&output_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn end_to_end_claude_output_with_preamble_on_same_line_as_json() {
        let stream_events = claude_stream_with_text(
            "所有修改看起来都正确。现在我将输出伙伴协议消息。{\"type\": \"chat\", \"content\": \"## Changes Summary\\n\\nAll tests pass.\"}",
        );
        let output_text = extract_actor_output("claude", &stream_events);
        let message = parse_buddy_message(&output_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn extracts_content_with_unescaped_quotes_in_content_actual_newlines() {
        let text = "所有修改看起来都正确。\n{\"type\": \"chat\", \"content\": \"Run `echo \"$PATH\"` to check.\n\nAll done.\"}";
        let message = parse_buddy_message(text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(text.contains("Run"));
                assert!(text.contains("All done."));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn real_world_preamble_plus_buddy_json_with_long_markdown_content() {
        let content = "## Changes Summary\\n\\n### Fix 1: Kimi ENOENT error\\n\\n**Root cause**: macOS GUI apps don't inherit the user's shell PATH.\\n\\n**Files changed:**\\n- **`src/main/buddy/shell-path.ts`** (new)\\n- **`src/main/index.ts`**\\n- **`src/main/buddy/launchers.ts`**\\n\\n### Tests\\n- All 151 tests pass, typecheck clean, build succeeds";
        let stream_events = claude_stream_with_text(&format!(
            "所有修改看起来都正确。现在我将输出伙伴协议消息。\n\n{{\"type\": \"chat\", \"content\": \"{}\"}}",
            content
        ));
        let output_text = extract_actor_output("claude", &stream_events);
        let message = parse_buddy_message(&output_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
                assert!(text.contains("shell-path.ts"));
                assert!(text.contains("151 tests pass"));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn real_world_preamble_with_buddy_json_actual_newlines_in_raw_text() {
        let raw_text = "所有修改看起来都正确。现在我将输出伙伴协议消息。\n{\"type\": \"chat\", \"content\": \"## Changes Summary\\n\\nFix 1: Kimi ENOENT error (original task)\"}";
        let message = parse_buddy_message(raw_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
                assert!(text.contains("Kimi ENOENT"));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    #[test]
    fn real_world_chinese_preamble_with_buddy_json_escaped() {
        let raw_text = "所有修改看起来都正确。现在我将输出伙伴协议消息。\\n\\n{\\\"type\\\": \\\"chat\\\", \\\"content\\\": \\\"## Changes Summary\\n\\nFix 1: Kimi ENOENT error (original task)\\\"}";
        let message = parse_buddy_message(raw_text);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("\"type\""));
                assert!(!text.contains("\"content\""));
                assert!(text.contains("## Changes Summary"));
                assert!(text.contains("Kimi ENOENT"));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // parseJsonlBuffer recovery from broken events
    // -----------------------------------------------------------------------

    #[test]
    fn recovers_valid_events_after_a_broken_json_event() {
        let broken = "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"diff with\ttab\"}]}}";
        let valid1 = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done!\"}]}}";
        let valid2 = "{\"type\":\"result\",\"result\":\"Done!\",\"session_id\":\"s1\"}";

        let raw = [broken, valid1, valid2].join("\n");
        let events = parse_jsonl_buffer(&raw);

        assert!(events.len() >= 2);
        assert!(events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("assistant")));
        assert!(events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("result")));
    }

    #[test]
    fn recovers_result_event_when_tool_result_has_raw_newlines() {
        let broken_event = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"tool_use_id\":\"tu_1\",\"type\":\"tool_result\",\"content\":\"diff --git a/file.ts b/file.ts\nindex abc..def\n--- a/file.ts\n+++ b/file.ts\n@@ -1,3 +1,4 @@\n line1\n+line2\n line3\"}]}}";
        let assistant_event = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Based on the diff, here is my conclusion.\"}]}}";
        let result_event = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"Based on the diff, here is my conclusion.\",\"session_id\":\"sess1\"}";

        let raw = format!("{}\n{}\n{}", broken_event, assistant_event, result_event);
        let events = parse_jsonl_buffer(&raw);

        assert!(events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("assistant")));
        assert!(events
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some("result")));

        let result_event_parsed = events
            .iter()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("result"))
            .unwrap();
        assert_eq!(
            result_event_parsed.get("result").and_then(Value::as_str),
            Some("Based on the diff, here is my conclusion.")
        );
    }

    #[test]
    fn preserves_normal_jsonl_parsing_when_no_broken_events() {
        let raw = [
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}",
            "{\"type\":\"result\",\"result\":\"hello\"}",
        ]
        .join("\n");
        let events = parse_jsonl_buffer(&raw);

        assert_eq!(events.len(), 3);
        assert_eq!(
            events[0].get("type").and_then(Value::as_str),
            Some("system")
        );
        assert_eq!(
            events[1].get("type").and_then(Value::as_str),
            Some("assistant")
        );
        assert_eq!(
            events[2].get("type").and_then(Value::as_str),
            Some("result")
        );
    }

    // -----------------------------------------------------------------------
    // parseBuddyMessage with preamble containing tool_result content
    // -----------------------------------------------------------------------

    #[test]
    fn extracts_buddy_json_when_tool_result_content_appears_before_it() {
        let tool_result_content = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"tool_use_id\":\"tu_1\",\"type\":\"tool_result\",\"content\":\"diff output here\"}]}}";
        let buddy_json = "{\"type\":\"chat\",\"content\":\"My conclusion after review.\"}";
        let text = format!("{}\n{}", tool_result_content, buddy_json);

        let message = parse_buddy_message(&text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "My conclusion after review.".to_string()
            }
        );
    }

    #[test]
    fn extracts_buddy_json_from_fallback_parsed_text_with_raw_tool_result() {
        let broken_line1 = "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"diff --git a/parsers.ts b/parsers.ts\"}";
        let broken_line2 = "more diff content with \"content\":\" keys inside";
        let assistant_text = "Here is my analysis.\n{\"type\":\"chat\",\"content\":\"Fixed the parsing bug.\"}";

        let text = [broken_line1, broken_line2, assistant_text].join("\n");
        let message = parse_buddy_message(&text);

        assert_eq!(
            message,
            BuddyMessage::Message {
                text: "Fixed the parsing bug.".to_string()
            }
        );
    }

    #[test]
    fn end_to_end_claude_stream_with_broken_tool_result_event() {
        let broken_event = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"tool_use_id\":\"tu_1\",\"type\":\"tool_result\",\"content\":\"diff --git a/file.ts b/file.ts\nindex abc..def\n--- a/file.ts\n+++ b/file.ts\n@@ -1,3 +1,4 @@\n line1\n+added\n line3\"}]}}";
        let assistant_event = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Based on the diff:\\n\\n{\"type\":\"chat\",\"content\":\"## Changes\\n\\n1. Added new line to file.ts\\n\\nAll tests pass.\"}\"}]}}";
        let result_event = stringify(json!({
            "type": "result",
            "subtype": "success",
            "result": "Based on the diff:\n\n{\"type\":\"chat\",\"content\":\"## Changes\\n\\n1. Added new line to file.ts\\n\\nAll tests pass.\"}",
            "session_id": "sess1"
        }));

        let raw = [
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess1\"}".to_string(),
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"Bash\",\"input\":{\"command\":\"git diff\"}}]}}".to_string(),
            broken_event.to_string(),
            assistant_event.to_string(),
            result_event,
        ]
        .join("\n");

        let extracted = extract_actor_output("claude", &raw);
        let message = parse_buddy_message(&extracted);

        match message {
            BuddyMessage::Message { text } => {
                assert!(!text.contains("diff --git"));
                assert!(!text.contains("\"type\""));
                assert!(text.contains("## Changes"));
                assert!(text.contains("All tests pass."));
            }
            other => panic!("expected message, got {:?}", other),
        }
    }
}
