//! Detect the current model for an actor by reading its configuration file.
//! Port of `src/main/buddy/model-detect.ts` from the Electron edition.
//!
//! This serves as a fallback when the model cannot be determined from
//! streaming output events.
//!
//! Detection is driven by the launcher command's *kind* (the actual CLI being
//! invoked), not the actor name — so e.g. an actor named "kimi" whose launcher
//! is `opencode -m provider/kimi-k2.6` is detected as opencode, matching what
//! the runner will actually invoke.
//!
//! Precedence:
//! 1. `-m` / `--model` passed on the command line (always wins — it is exactly
//!    what the runner invokes, for any CLI).
//! 2. CLI-specific config file (see `detect_model_from_config`).
//!
//! NOTE: `commandKindFor` / `isWecodeClaudeCommand` / `isWecodeCodexCommand` /
//! `splitCommand` live in `launchers.ts` upstream; here they come from the
//! shared `launchers` module.

use regex::Regex;
use serde_json::Value;
use std::path::Path;

use crate::buddy::launchers::{
    command_kind_for, is_wecode_claude_command, is_wecode_codex_command, split_command,
    LauncherCommandKind,
};

/// Detect the configured model for an actor.
///
/// @param actor  Actor name (codex, opencode, kimi, claude)
/// @param command  Optional launcher command string. Used both to extract an
///                 explicit `-m`/`--model` override and to determine the CLI
///                 kind (e.g. distinguishing `wecode codex` from plain `codex`,
///                 or `opencode` invoked under a kimi/codex actor).
pub async fn detect_model_from_config(actor: &str, command: Option<&str>) -> Option<String> {
    let home = dirs::home_dir()?;
    detect_model_from_config_in(&home, actor, command).await
}

async fn detect_model_from_config_in(
    home: &Path,
    actor: &str,
    command: Option<&str>,
) -> Option<String> {
    // 1. An explicit -m / --model on the command line always wins, for any CLI.
    if let Some(from_command) = model_from_command_args(command) {
        return Some(from_command);
    }

    // 2. Otherwise branch on the actual CLI kind, not the actor name.
    let kind = command_kind_for(actor, command.unwrap_or(""));

    match kind {
        LauncherCommandKind::NativeOpencode => {
            read_json_model(&home.join(".config").join("opencode").join("opencode.json"), "model")
                .await
        }
        LauncherCommandKind::NativeCodex => {
            // When codex is launched via `wecode codex`, the effective model is
            // in ~/.wecode-cli/config.json (codex.model), NOT ~/.codex/config.toml
            // — wecode does not write back to config.toml.
            if is_wecode_codex_command(command.unwrap_or("")) {
                read_wecode_codex_model(home).await
            } else {
                read_toml_model(&home.join(".codex").join("config.toml"), "model").await
            }
        }
        LauncherCommandKind::NativeKimi => {
            // Kimi Code CLI reads ~/.kimi-code/config.toml; ~/.kimi is the legacy path
            if let Some(primary) =
                read_toml_model(&home.join(".kimi-code").join("config.toml"), "default_model").await
            {
                return Some(primary);
            }
            read_toml_model(&home.join(".kimi").join("config.toml"), "default_model").await
        }
        LauncherCommandKind::NativeClaude => {
            // WeCode Claude (`wecode`, optionally with flags like
            // --dangerously-skip-permissions) reads its own config and must NOT
            // fall back to ~/.claude/settings.json — otherwise a stale Claude model
            // would be displayed. Detection is by executable basename, not by any
            // permission flag, mirroring commandKindFor.
            if is_wecode_claude_command(command.unwrap_or("")) {
                read_wecode_claude_model(&home.join(".wecode-cli").join("config.json")).await
            } else {
                read_claude_model(&home.join(".claude").join("settings.json")).await
            }
        }
        // contract: model is not knowable before a run.
        _ => None,
    }
}

/// Extract the model from a launcher command's `-m` / `--model` argument,
/// e.g. `opencode -m agnes/agnes-2.0-flash` → `agnes/agnes-2.0-flash`,
/// or `codex -m gpt-5.6-luna` → `gpt-5.6-luna`. Applies to any CLI kind —
/// a command-line override is always what the runner actually invokes.
fn model_from_command_args(command: Option<&str>) -> Option<String> {
    let command = command?;
    if command.is_empty() {
        return None;
    }
    let parts = split_command(command);
    if parts.is_empty() {
        return None;
    }
    for i in 0..parts.len() {
        if let Some(rest) = parts[i].strip_prefix("--model=") {
            return if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            };
        }
        if (parts[i] == "-m" || parts[i] == "--model") && i + 1 < parts.len() {
            let next = &parts[i + 1];
            return if next.is_empty() {
                None
            } else {
                Some(next.clone())
            };
        }
    }
    None
}

/// Read the codex model from ~/.wecode-cli/config.json.
/// Structure: { codex: { model: "thudm-glm-5.2", forceModel: false } }
async fn read_wecode_codex_model(home: &Path) -> Option<String> {
    let raw = tokio::fs::read_to_string(home.join(".wecode-cli").join("config.json"))
        .await
        .ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    let model = obj.get("codex")?.get("model").and_then(Value::as_str)?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Read the effective WeCode Claude model from ~/.wecode-cli/config.json.
/// Structure: { env: { ANTHROPIC_MODEL: "weibo-glm-5.2[1m]" } }
///
/// WeCode does not write back to ~/.claude/settings.json, so when the launcher
/// is `wecode` this is the only source of truth. Any failure (missing file,
/// unreadable, malformed JSON, absent/non-string/empty ANTHROPIC_MODEL) yields
/// None — no fallback to the plain-Claude config.
async fn read_wecode_claude_model(file_path: &Path) -> Option<String> {
    let raw = tokio::fs::read_to_string(file_path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    let model = obj.get("env")?.get("ANTHROPIC_MODEL").and_then(Value::as_str)?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Read the effective Claude model from ~/.claude/settings.json.
///
/// Claude Code's `model` field is a tier alias (e.g. "sonnet[1m]", "opus").
/// The actual model the SDK invokes is `env.ANTHROPIC_MODEL` when set — it
/// overrides the tier at the SDK level. Prefer it so the displayed model
/// matches what the runner really invokes; fall back to the `model` alias.
async fn read_claude_model(file_path: &Path) -> Option<String> {
    let raw = tokio::fs::read_to_string(file_path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    if let Some(env) = obj.get("env").and_then(Value::as_object) {
        if let Some(override_model) = env.get("ANTHROPIC_MODEL").and_then(Value::as_str) {
            if !override_model.is_empty() {
                return Some(override_model.to_string());
            }
        }
    }
    let model = obj.get("model").and_then(Value::as_str)?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Read a model field from a JSON config file.
async fn read_json_model(file_path: &Path, field: &str) -> Option<String> {
    let raw = tokio::fs::read_to_string(file_path).await.ok()?;
    let obj = serde_json::from_str::<Value>(&raw).ok()?;
    let value = obj.get(field).and_then(Value::as_str)?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Extract a top-level string field from a TOML config file.
/// Uses a simple regex instead of a full TOML parser since we only
/// need a single top-level key.
///
/// Handles: key = "value", key = 'value', key = value
async fn read_toml_model(file_path: &Path, field: &str) -> Option<String> {
    let raw = tokio::fs::read_to_string(file_path).await.ok()?;
    // Match top-level field only: no leading whitespace, no dot in key path
    // Patterns: model = "gpt-5.5" | model = 'gpt-5.5' | model = gpt-5.5
    let re = Regex::new(&format!(
        r#"(?m)^{}\s*=\s*(?:"([^"]*)"|'([^']*)'|(\S+))"#,
        regex::escape(field)
    ))
    .ok()?;
    let caps = re.captures(&raw)?;
    let value = caps
        .get(1)
        .or_else(|| caps.get(2))
        .or_else(|| caps.get(3))
        .map(|m| m.as_str())?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests (port of tests/unit/main/buddy-model-detect.test.ts; the mocked
// homedir becomes an explicit `home` parameter on the internal function)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write(home: &Path, rel: &str, content: &str) {
        let path = home.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn reads_model_from_opencode_json_config() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".config/opencode/opencode.json",
            &serde_json::to_string(&json!({
                "model": "wecode/ali-deepseek-v4-pro",
                "provider": {}
            }))
            .unwrap(),
        );

        let model = detect_model_from_config_in(temp.path(), "opencode", None).await;
        assert_eq!(model.as_deref(), Some("wecode/ali-deepseek-v4-pro"));
    }

    #[tokio::test]
    async fn reads_model_from_codex_toml_config_quoted_value() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".codex/config.toml",
            &[
                "model_provider = \"cpa\"",
                "model = \"gpt-5.5\"",
                "disable_response_storage = true",
                "",
                "[model_providers.cpa]",
                "name = \"wecode openai\"",
            ]
            .join("\n"),
        );

        let model = detect_model_from_config_in(
            temp.path(),
            "codex",
            Some("codex -p --output-format stream-json"),
        )
        .await;
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
    }

    #[tokio::test]
    async fn reads_model_from_wecode_config_when_command_is_wecode_codex() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".codex/config.toml", "model = \"gpt-5.5\"\n");
        write(
            temp.path(),
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({
                "codex": { "model": "thudm-glm-5.2", "forceModel": false },
                "claude": { "forceModel": true }
            }))
            .unwrap(),
        );

        let model = detect_model_from_config_in(
            temp.path(),
            "codex",
            Some("wecode codex --output-format stream-json"),
        )
        .await;
        assert_eq!(model.as_deref(), Some("thudm-glm-5.2"));
    }

    #[tokio::test]
    async fn falls_back_to_codex_config_toml_when_wecode_config_has_no_codex_model() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".codex/config.toml", "model = \"gpt-5.5\"\n");
        write(
            temp.path(),
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({
                "codex": { "forceModel": false }
            }))
            .unwrap(),
        );

        let model = detect_model_from_config_in(temp.path(), "codex", Some("wecode codex")).await;
        // wecode config exists but has no codex.model → None (not fallback to config.toml)
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn returns_none_for_wecode_codex_when_wecode_config_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let model = detect_model_from_config_in(temp.path(), "codex", Some("wecode codex")).await;
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn uses_codex_config_toml_when_command_is_plain_codex() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".codex/config.toml", "model = \"gpt-5.5\"\n");

        let model = detect_model_from_config_in(
            temp.path(),
            "codex",
            Some("codex --output-format stream-json"),
        )
        .await;
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
    }

    #[tokio::test]
    async fn uses_codex_config_toml_when_command_is_none() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".codex/config.toml", "model = \"gpt-5.5\"\n");

        let model = detect_model_from_config_in(temp.path(), "codex", None).await;
        assert_eq!(model.as_deref(), Some("gpt-5.5"));
    }

    #[tokio::test]
    async fn reads_default_model_from_kimi_toml_config() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".kimi/config.toml",
            &[
                "default_model = \"kimi-latest\"",
                "default_thinking = false",
                "default_yolo = false",
            ]
            .join("\n"),
        );

        let model = detect_model_from_config_in(temp.path(), "kimi", None).await;
        assert_eq!(model.as_deref(), Some("kimi-latest"));
    }

    #[tokio::test]
    async fn prefers_kimi_code_config_toml_over_legacy_kimi() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".kimi-code/config.toml",
            "default_model = \"kimi-code/k3\"\n",
        );
        write(
            temp.path(),
            ".kimi/config.toml",
            "default_model = \"kimi-latest\"\n",
        );

        let model = detect_model_from_config_in(temp.path(), "kimi", None).await;
        assert_eq!(model.as_deref(), Some("kimi-code/k3"));
    }

    #[tokio::test]
    async fn reads_opencode_model_from_m_command_argument() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            detect_model_from_config_in(temp.path(), "opencode", Some("opencode -m agnes/agnes-2.0-flash"))
                .await
                .as_deref(),
            Some("agnes/agnes-2.0-flash")
        );
        assert_eq!(
            detect_model_from_config_in(temp.path(), "opencode", Some("opencode --model provider/kimi-k2.6"))
                .await
                .as_deref(),
            Some("provider/kimi-k2.6")
        );
        assert_eq!(
            detect_model_from_config_in(temp.path(), "opencode", Some("opencode --model=provider/kimi-k2.6"))
                .await
                .as_deref(),
            Some("provider/kimi-k2.6")
        );
    }

    #[tokio::test]
    async fn detects_model_from_m_override_regardless_of_actor_name() {
        let temp = tempfile::tempdir().unwrap();
        // actor is "kimi" but the launcher actually runs opencode with -m;
        // the runner invokes opencode with this model, so detection must match.
        assert_eq!(
            detect_model_from_config_in(temp.path(), "kimi", Some("opencode -m provider/kimi-k2.6"))
                .await
                .as_deref(),
            Some("provider/kimi-k2.6")
        );
        // Without -m, a kimi actor on the opencode CLI reads opencode's config.
        write(
            temp.path(),
            ".config/opencode/opencode.json",
            &serde_json::to_string(&json!({ "model": "wecode/ali-deepseek-v4-pro" })).unwrap(),
        );
        assert_eq!(
            detect_model_from_config_in(temp.path(), "kimi", Some("opencode"))
                .await
                .as_deref(),
            Some("wecode/ali-deepseek-v4-pro")
        );
    }

    #[tokio::test]
    async fn detects_model_from_codex_m_command_override() {
        let temp = tempfile::tempdir().unwrap();
        // Stale config.toml must NOT win over an explicit -m on the command line.
        write(temp.path(), ".codex/config.toml", "model = \"gpt-5.5\"\n");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "codex", Some("codex -m gpt-5.6-luna"))
                .await
                .as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            detect_model_from_config_in(temp.path(), "codex", Some("codex --model gpt-5.6-luna"))
                .await
                .as_deref(),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            detect_model_from_config_in(temp.path(), "codex", Some("codex --model=gpt-5.6-luna"))
                .await
                .as_deref(),
            Some("gpt-5.6-luna")
        );
    }

    #[tokio::test]
    async fn returns_none_for_unknown_actor() {
        let temp = tempfile::tempdir().unwrap();
        let model = detect_model_from_config_in(temp.path(), "unknown_actor", None).await;
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn returns_none_when_config_file_does_not_exist() {
        let temp = tempfile::tempdir().unwrap();
        let model = detect_model_from_config_in(temp.path(), "opencode", None).await;
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn returns_none_when_model_field_is_empty_string() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".kimi/config.toml",
            &["default_model = \"\"", "default_thinking = false"].join("\n"),
        );

        let model = detect_model_from_config_in(temp.path(), "kimi", None).await;
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn returns_none_for_claude_when_no_config_exists() {
        let temp = tempfile::tempdir().unwrap();
        let model = detect_model_from_config_in(temp.path(), "claude", None).await;
        assert_eq!(model, None);
    }

    #[tokio::test]
    async fn reads_the_selected_claude_model_from_settings_json() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".claude/settings.json",
            &serde_json::to_string(&json!({ "model": "sonnet[1m]" })).unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("claude"))
                .await
                .as_deref(),
            Some("sonnet[1m]")
        );
    }

    #[tokio::test]
    async fn prefers_an_explicit_claude_model_launcher_override() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".claude/settings.json",
            &serde_json::to_string(&json!({ "model": "sonnet[1m]" })).unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("claude --model opus"))
                .await
                .as_deref(),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn prefers_claude_env_anthropic_model_over_the_model_tier_alias() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".claude/settings.json",
            &serde_json::to_string(&json!({
                "model": "sonnet[1m]",
                "env": { "ANTHROPIC_MODEL": "weibo-glm-5.2" }
            }))
            .unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("claude"))
                .await
                .as_deref(),
            Some("weibo-glm-5.2")
        );
    }

    // -----------------------------------------------------------------------
    // WeCode Claude detection
    // -----------------------------------------------------------------------

    fn write_wecode_config(home: &Path, model: &str) {
        write(
            home,
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({
                "env": { "ANTHROPIC_MODEL": model }
            }))
            .unwrap(),
        );
    }

    fn write_claude_config(home: &Path, model: &str) {
        write(
            home,
            ".claude/settings.json",
            &serde_json::to_string(&json!({
                "model": model,
                "env": { "ANTHROPIC_MODEL": model }
            }))
            .unwrap(),
        );
    }

    #[tokio::test]
    async fn wecode_1_reads_model_from_wecode_config_when_command_is_bare_wecode() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode"))
                .await
                .as_deref(),
            Some("weibo-glm-5.2[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_2_ignores_dangerously_skip_permissions_when_detecting_wecode() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(
                temp.path(),
                "claude",
                Some("wecode --dangerously-skip-permissions")
            )
            .await
            .as_deref(),
            Some("weibo-glm-5.2[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_3_wecode_config_wins_over_stale_claude_config() {
        let temp = tempfile::tempdir().unwrap();
        write_claude_config(temp.path(), "stale-claude-model");
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        let model = detect_model_from_config_in(temp.path(), "claude", Some("wecode")).await;
        assert_eq!(model.as_deref(), Some("weibo-glm-5.2[1m]"));
        assert_ne!(model.as_deref(), Some("stale-claude-model"));
    }

    #[tokio::test]
    async fn wecode_4_returns_none_when_wecode_config_missing_no_fallback_to_claude() {
        let temp = tempfile::tempdir().unwrap();
        // Stale claude config exists, but no wecode config — must NOT fall back.
        write_claude_config(temp.path(), "stale-claude-model");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode")).await,
            None
        );
    }

    #[tokio::test]
    async fn wecode_5_detects_wecode_via_absolute_path() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("/usr/local/bin/wecode"))
                .await
                .as_deref(),
            Some("weibo-glm-5.2[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_5b_detects_wecode_via_a_quoted_path_with_spaces() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(
                temp.path(),
                "claude",
                Some("\"/path with spaces/wecode\" --dangerously-skip-permissions")
            )
            .await
            .as_deref(),
            Some("weibo-glm-5.2[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_6_command_line_model_override_wins_over_wecode_config() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode --model explicit-model"))
                .await
                .as_deref(),
            Some("explicit-model")
        );
    }

    #[tokio::test]
    async fn wecode_6b_command_line_m_override_wins_over_wecode_config() {
        let temp = tempfile::tempdir().unwrap();
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode -m explicit-model"))
                .await
                .as_deref(),
            Some("explicit-model")
        );
    }

    #[tokio::test]
    async fn wecode_7_plain_claude_still_reads_settings_json() {
        let temp = tempfile::tempdir().unwrap();
        write_claude_config(temp.path(), "sonnet[1m]");
        // No wecode config — plain claude must keep working.
        write_wecode_config(temp.path(), "weibo-glm-5.2[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("claude"))
                .await
                .as_deref(),
            Some("sonnet[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_7b_plain_claude_with_absolute_path_still_reads_settings_json() {
        let temp = tempfile::tempdir().unwrap();
        write_claude_config(temp.path(), "sonnet[1m]");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("/usr/local/bin/claude"))
                .await
                .as_deref(),
            Some("sonnet[1m]")
        );
    }

    #[tokio::test]
    async fn wecode_8_wecode_codex_does_not_enter_the_claude_branch() {
        let temp = tempfile::tempdir().unwrap();
        // Set up both configs to prove the codex branch is taken, not claude's.
        write(
            temp.path(),
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({
                "env": { "ANTHROPIC_MODEL": "weibo-glm-5.2[1m]" },
                "codex": { "model": "thudm-glm-5.2", "forceModel": false }
            }))
            .unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "codex", Some("wecode codex"))
                .await
                .as_deref(),
            Some("thudm-glm-5.2")
        );
    }

    #[tokio::test]
    async fn wecode_returns_none_when_config_has_no_env_anthropic_model() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({ "codex": { "model": "x" } })).unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode")).await,
            None
        );
    }

    #[tokio::test]
    async fn wecode_returns_none_when_config_is_malformed_json() {
        let temp = tempfile::tempdir().unwrap();
        write(temp.path(), ".wecode-cli/config.json", "{ not valid json");

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode")).await,
            None
        );
    }

    #[tokio::test]
    async fn wecode_returns_none_when_anthropic_model_is_empty_string() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            ".wecode-cli/config.json",
            &serde_json::to_string(&json!({ "env": { "ANTHROPIC_MODEL": "" } })).unwrap(),
        );

        assert_eq!(
            detect_model_from_config_in(temp.path(), "claude", Some("wecode")).await,
            None
        );
    }
}
