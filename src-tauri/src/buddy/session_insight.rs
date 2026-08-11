//! Read per-run model / token usage that actor CLIs do NOT include in their
//! stdout stream, from their on-disk session state. Port of
//! `src/main/buddy/session-insight.ts` from the Electron edition.
//!
//! - kimi (Kimi Code CLI): stream-json stdout carries only assistant/tool/meta
//!   messages. Token usage and the actual model live in the session wire file:
//!   ~/.kimi-code/sessions/<wd>/<sessionId>/agents/<agent>/wire.jsonl
//!   entries of type "usage.record" → usage.{inputOther, output, inputCacheRead}
//!
//! - opencode: stdout JSON events carry tokens in step_finish but no model.
//!   The model lives in per-session storage:
//!   - newer versions: ~/.local/share/opencode/opencode.db (SQLite, message table)
//!   - older versions: ~/.local/share/opencode/storage/message/<sessionId>/*.json

use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KimiUsageRecord {
    pub time_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KimiSessionInsight {
    pub records: Vec<KimiUsageRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn as_number(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|f| f.is_finite() && *f > 0.0)
            .map(|f| f as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

/// Locate every wire.jsonl (main agent + subagents) for a kimi session id.
fn find_kimi_wire_files(home: &Path, session_id: &str) -> Vec<PathBuf> {
    let base = home.join(".kimi-code").join("sessions");
    let mut results = Vec::new();
    let Ok(wd_dirs) = std::fs::read_dir(&base) else {
        return results;
    };
    for wd in wd_dirs.flatten() {
        let agents_dir = base.join(wd.file_name()).join(session_id).join("agents");
        let Ok(agents) = std::fs::read_dir(&agents_dir) else {
            continue;
        };
        for agent in agents.flatten() {
            results.push(agents_dir.join(agent.file_name()).join("wire.jsonl"));
        }
    }
    results
}

/// Parse a kimi session's wire files into usage records (one per LLM step)
/// plus the latest model seen. Returns None when nothing was found.
pub async fn read_kimi_session_insight(session_id: &str) -> Option<KimiSessionInsight> {
    let home = dirs::home_dir()?;
    read_kimi_session_insight_in(&home, session_id).await
}

async fn read_kimi_session_insight_in(home: &Path, session_id: &str) -> Option<KimiSessionInsight> {
    if session_id.is_empty() {
        return None;
    }
    let mut records: Vec<KimiUsageRecord> = Vec::new();
    let mut model: Option<String> = None;
    for file in find_kimi_wire_files(home, session_id) {
        let Ok(raw) = tokio::fs::read_to_string(&file).await else {
            continue;
        };
        for line in raw.split('\n') {
            if !line.contains("\"usage.record\"") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue; // Malformed line — skip
            };
            if entry.get("type").and_then(Value::as_str) != Some("usage.record") {
                continue;
            }
            let usage = entry.get("usage").filter(|u| !u.is_null());
            let Some(usage) = usage else {
                continue;
            };
            records.push(KimiUsageRecord {
                time_ms: as_number(entry.get("time")),
                input_tokens: as_number(usage.get("inputOther")),
                output_tokens: as_number(usage.get("output")),
                cache_read_tokens: as_number(usage.get("inputCacheRead")),
            });
            if let Some(entry_model) = entry.get("model").and_then(Value::as_str) {
                if !entry_model.is_empty() {
                    model = Some(entry_model.to_string());
                }
            }
        }
    }
    if records.is_empty() && model.is_none() {
        return None;
    }
    Some(KimiSessionInsight { records, model })
}

fn is_safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// opencode ≥ new storage: model from the SQLite message table via the sqlite3 CLI.
async fn read_opencode_model_from_db(home: &Path, session_id: &str) -> Option<String> {
    if !is_safe_session_id(session_id) {
        return None;
    }
    let db_path = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db");
    let query = format!(
        "SELECT json_extract(data,'$.providerID') || '/' || json_extract(data,'$.modelID') FROM message WHERE session_id='{}' AND json_extract(data,'$.role')='assistant' AND json_extract(data,'$.modelID') IS NOT NULL ORDER BY time_created DESC LIMIT 1;",
        session_id
    );
    let mut command = tokio::process::Command::new("sqlite3");
    command
        .arg("-readonly")
        .arg(&db_path)
        .arg(&query)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Dropping the child on timeout kills it (the TS original SIGTERMs it
        // after 5s and resolves undefined either way).
        .kill_on_drop(true);
    let child = command.spawn().ok()?;
    match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !out.is_empty() && out != "/" {
                Some(out)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// opencode old storage: JSON files under storage/message/<sessionId>/.
async fn read_opencode_model_from_files(home: &Path, session_id: &str) -> Option<String> {
    let dir = home
        .join(".local")
        .join("share")
        .join("opencode")
        .join("storage")
        .join("message")
        .join(session_id);
    let mut entries = tokio::fs::read_dir(&dir).await.ok()?;
    let mut latest_ms: i64 = -1;
    let mut model: Option<String> = None;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") {
            continue;
        }
        let parsed = (|| {
            let raw = std::fs::read_to_string(entry.path()).ok()?;
            let msg = serde_json::from_str::<Value>(&raw).ok()?;
            let m = msg.get("model").and_then(Value::as_object)?;
            let provider_id = m.get("providerID").and_then(Value::as_str)?;
            let model_id = m.get("modelID").and_then(Value::as_str)?;
            if model_id.is_empty() {
                return None;
            }
            let created = msg
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite())
                .unwrap_or(0.0) as i64;
            Some((
                created,
                if provider_id.is_empty() {
                    model_id.to_string()
                } else {
                    format!("{}/{}", provider_id, model_id)
                },
            ))
        })();
        let Some((created, candidate)) = parsed else {
            continue; // Unreadable file — skip
        };
        if created >= latest_ms {
            latest_ms = created;
            model = Some(candidate);
        }
    }
    model
}

async fn read_opencode_session_model_in(home: &Path, session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    if let Some(from_files) = read_opencode_model_from_files(home, session_id).await {
        return Some(from_files);
    }
    read_opencode_model_from_db(home, session_id).await
}

/// Detect the model an opencode session actually used, from its local session
/// storage. Tries the legacy JSON-file storage first, then the SQLite database.
pub async fn read_opencode_session_model(session_id: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    read_opencode_session_model_in(&home, session_id).await
}

// ---------------------------------------------------------------------------
// Tests (port of tests/unit/main/buddy-session-insight.test.ts; the mocked
// homedir becomes an explicit `home` parameter on the internal functions)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SESSION_ID: &str = "session_aaaa-bbbb";

    fn usage_record_line(time: u64, usage: Value, model: &str) -> String {
        serde_json::to_string(&json!({
            "type": "usage.record",
            "model": model,
            "usage": usage,
            "usageScope": "turn",
            "time": time
        }))
        .unwrap()
    }

    fn write_kimi_wire(home: &Path, session_id: &str, agent: &str, lines: &[String]) {
        let dir = home
            .join(".kimi-code")
            .join("sessions")
            .join("wd_repo_abc123")
            .join(session_id)
            .join("agents")
            .join(agent);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("wire.jsonl"), lines.join("\n")).unwrap();
    }

    #[tokio::test]
    async fn aggregates_usage_records_and_latest_model_from_main_agent_wire_file() {
        let temp = tempfile::tempdir().unwrap();
        write_kimi_wire(
            temp.path(),
            SESSION_ID,
            "main",
            &[
                serde_json::to_string(&json!({ "type": "session.init", "time": 100 })).unwrap(),
                usage_record_line(
                    1_000,
                    json!({ "inputOther": 100, "output": 10, "inputCacheRead": 900, "inputCacheCreation": 0 }),
                    "kimi-code/k3",
                ),
                usage_record_line(
                    2_000,
                    json!({ "inputOther": 200, "output": 20, "inputCacheRead": 800, "inputCacheCreation": 0 }),
                    "kimi-code/k4",
                ),
            ],
        );

        let insight = read_kimi_session_insight_in(temp.path(), SESSION_ID)
            .await
            .unwrap();
        assert_eq!(insight.records.len(), 2);
        assert_eq!(
            insight.records[0],
            KimiUsageRecord {
                time_ms: 1_000,
                input_tokens: 100,
                output_tokens: 10,
                cache_read_tokens: 900
            }
        );
        assert_eq!(insight.records[1].input_tokens, 200);
        assert_eq!(insight.model.as_deref(), Some("kimi-code/k4"));
    }

    #[tokio::test]
    async fn includes_subagent_wire_files() {
        let temp = tempfile::tempdir().unwrap();
        write_kimi_wire(
            temp.path(),
            SESSION_ID,
            "main",
            &[usage_record_line(
                1_000,
                json!({ "inputOther": 100, "output": 10, "inputCacheRead": 0, "inputCacheCreation": 0 }),
                "kimi-code/k3",
            )],
        );
        write_kimi_wire(
            temp.path(),
            SESSION_ID,
            "coder-1",
            &[usage_record_line(
                1_500,
                json!({ "inputOther": 50, "output": 5, "inputCacheRead": 500, "inputCacheCreation": 0 }),
                "kimi-code/k3",
            )],
        );

        let insight = read_kimi_session_insight_in(temp.path(), SESSION_ID)
            .await
            .unwrap();
        assert_eq!(insight.records.len(), 2);
    }

    #[tokio::test]
    async fn returns_none_when_the_session_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_kimi_session_insight_in(temp.path(), "session_nonexistent")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn skips_malformed_lines() {
        let temp = tempfile::tempdir().unwrap();
        write_kimi_wire(
            temp.path(),
            SESSION_ID,
            "main",
            &[
                "not json at all".to_string(),
                "{\"type\":\"usage.record\",\"usage\":".to_string(),
                usage_record_line(
                    3_000,
                    json!({ "inputOther": 1, "output": 2, "inputCacheRead": 3, "inputCacheCreation": 0 }),
                    "kimi-code/k3",
                ),
            ],
        );

        let insight = read_kimi_session_insight_in(temp.path(), SESSION_ID)
            .await
            .unwrap();
        assert_eq!(insight.records.len(), 1);
    }

    #[tokio::test]
    async fn reads_the_latest_assistant_model_from_legacy_json_file_storage() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp
            .path()
            .join(".local")
            .join("share")
            .join("opencode")
            .join("storage")
            .join("message")
            .join("ses_test123");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("msg_old.json"),
            serde_json::to_string(&json!({
                "role": "assistant",
                "time": { "created": 100 },
                "model": { "providerID": "agnes", "modelID": "agnes-2.0-flash" }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("msg_new.json"),
            serde_json::to_string(&json!({
                "role": "assistant",
                "time": { "created": 200 },
                "model": { "providerID": "opencode", "modelID": "deepseek-v4-flash-free" }
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            read_opencode_session_model_in(temp.path(), "ses_test123").await,
            Some("opencode/deepseek-v4-flash-free".to_string())
        );
    }

    #[tokio::test]
    async fn returns_none_when_neither_storage_exists() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_opencode_session_model_in(temp.path(), "ses_missing")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn rejects_unsafe_session_ids_without_touching_the_db() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            read_opencode_session_model_in(temp.path(), "x'; DROP TABLE message; --")
                .await
                .is_none()
        );
    }
}
