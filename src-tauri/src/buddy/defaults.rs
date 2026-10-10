//! Defaults & normalization, port of `src/shared/defaults.ts`.

use crate::buddy::types::{AcpPreset, GlobalSettings, Launcher};
use std::collections::HashMap;

pub const DEFAULT_LAUNCHER_ORDER: [&str; 9] = [
    "claude",
    "codex",
    "cursor",
    "agy",
    "opencode",
    "kimi",
    "wecode_claude",
    "wecode_codex",
    "wecode_opencode",
];

pub const DEFAULT_LAUNCHER_TIMEOUT_SECONDS: u64 = 7200;

/// Port of `KNOWN_ACP_PRESETS` in `src/shared/defaults.ts`.
pub fn known_acp_presets() -> Vec<AcpPreset> {
    vec![
        AcpPreset {
            id: "opencode-acp".to_string(),
            name: "OpenCode ACP".to_string(),
            actor: "opencode".to_string(),
            command: "opencode".to_string(),
            args: vec!["acp".to_string()],
            description: "OpenCode CLI with native ACP server".to_string(),
        },
        AcpPreset {
            id: "wecode-opencode-acp".to_string(),
            name: "WeCode OpenCode ACP".to_string(),
            actor: "wecode_opencode".to_string(),
            command: "wecode".to_string(),
            args: vec!["opencode".to_string(), "acp".to_string()],
            description: "WeCode OpenCode native ACP server".to_string(),
        },
        AcpPreset {
            id: "codex-acp".to_string(),
            name: "Codex ACP".to_string(),
            actor: "codex".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@agentclientprotocol/codex-acp".to_string()],
            description: "Codex ACP adapter".to_string(),
        },
        AcpPreset {
            id: "claude-acp".to_string(),
            name: "Claude ACP".to_string(),
            actor: "claude".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string(),
            ],
            description: "Claude Agent ACP adapter".to_string(),
        },
        AcpPreset {
            id: "cursor-acp".to_string(),
            name: "Cursor ACP".to_string(),
            actor: "cursor".to_string(),
            command: "cursor-agent".to_string(),
            args: vec!["acp".to_string()],
            description: "Cursor Agent native ACP mode".to_string(),
        },
        AcpPreset {
            id: "wecode-claude-acp".to_string(),
            name: "WeCode Claude ACP".to_string(),
            actor: "wecode_claude".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/claude-agent-acp".to_string(),
            ],
            description: "WeCode Claude via ACP adapter".to_string(),
        },
        AcpPreset {
            id: "wecode-codex-acp".to_string(),
            name: "WeCode Codex ACP".to_string(),
            actor: "wecode_codex".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@agentclientprotocol/codex-acp".to_string()],
            description: "WeCode Codex via ACP adapter".to_string(),
        },
    ]
}

pub fn default_launcher_command(actor: &str) -> &str {
    match actor {
        "claude" => "claude",
        "codex" => "codex",
        "cursor" => "cursor-agent",
        "agy" => "agy",
        "opencode" => "opencode",
        "kimi" => "kimi",
        "wecode_claude" => "wecode",
        "wecode_codex" => "wecode codex",
        "wecode_opencode" => "wecode opencode",
        other => other,
    }
}

pub fn default_launcher_for(actor: &str) -> Launcher {
    Launcher {
        protocol: None,
        command: default_launcher_command(actor).to_string(),
        args: None,
        env: HashMap::new(),
        timeout_seconds: DEFAULT_LAUNCHER_TIMEOUT_SECONDS,
        model: None,
    }
}

pub fn normalize_launcher(actor: &str, launcher: Option<&Launcher>) -> Launcher {
    let fallback = default_launcher_for(actor);
    match launcher {
        Some(l) => {
            let mut command = if !l.command.trim().is_empty() {
                l.command.clone()
            } else {
                fallback.command.clone()
            };
            if l.protocol.as_deref() != Some("acp") && command == "npx" {
                command = fallback.command.clone();
            }
            let mut result = Launcher {
                protocol: None,
                command,
                args: None,
                env: l.env.clone(),
                timeout_seconds: l.timeout_seconds,
                model: None,
            };
            if l.protocol.as_deref() == Some("acp") {
                result.protocol = Some("acp".to_string());
                if let Some(args) = &l.args {
                    result.args = Some(args.clone());
                }
            }
            if let Some(model) = l.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                result.model = Some(model.to_string());
            }
            result
        }
        None => fallback,
    }
}

pub fn normalize_launchers(
    launchers: Option<&HashMap<String, Launcher>>,
) -> HashMap<String, Launcher> {
    let mut normalized: HashMap<String, Launcher> = HashMap::new();

    for actor in DEFAULT_LAUNCHER_ORDER {
        normalized.insert(
            actor.to_string(),
            normalize_launcher(actor, launchers.and_then(|m| m.get(actor))),
        );
    }

    if let Some(map) = launchers {
        for (actor, launcher) in map {
            if !normalized.contains_key(actor) {
                normalized.insert(actor.clone(), normalize_launcher(actor, Some(launcher)));
            }
        }
    }

    normalized
}

pub fn normalize_global_settings(settings: Option<&GlobalSettings>) -> GlobalSettings {
    let empty = GlobalSettings::default();
    let s = settings.unwrap_or(&empty);
    GlobalSettings {
        protocol_version: Some(s.protocol_version.clone().unwrap_or_else(|| "1".to_string())),
        countdown_seconds: Some(s.countdown_seconds.unwrap_or(30)),
        max_rounds: Some(s.max_rounds.unwrap_or(9999)),
        max_consecutive_failures: Some(s.max_consecutive_failures.unwrap_or(10)),
        launchers: Some(normalize_launchers(s.launchers.as_ref())),
        seed_claude_session_id: Some(s.seed_claude_session_id.clone().unwrap_or_default()),
        seed_codex_thread_id: Some(s.seed_codex_thread_id.clone().unwrap_or_default()),
        seed_cursor_session_id: Some(s.seed_cursor_session_id.clone().unwrap_or_default()),
        seed_agy_session_id: Some(s.seed_agy_session_id.clone().unwrap_or_default()),
        seed_opencode_session_id: Some(s.seed_opencode_session_id.clone().unwrap_or_default()),
        seed_kimi_session_id: Some(s.seed_kimi_session_id.clone().unwrap_or_default()),
        seed_wecode_claude_session_id: Some(
            s.seed_wecode_claude_session_id.clone().unwrap_or_default(),
        ),
        seed_wecode_codex_thread_id: Some(
            s.seed_wecode_codex_thread_id.clone().unwrap_or_default(),
        ),
        seed_wecode_opencode_session_id: Some(
            s.seed_wecode_opencode_session_id.clone().unwrap_or_default(),
        ),
        max_compact_retries: Some(s.max_compact_retries.unwrap_or(3)),
        auto_generate_commit_message: Some(s.auto_generate_commit_message.unwrap_or(true)),
        system_notifications_enabled: Some(s.system_notifications_enabled.unwrap_or(true)),
        max_upgrade_retries: Some(s.max_upgrade_retries.unwrap_or(3)),
        custom_prompt_implementer: s
            .custom_prompt_implementer
            .clone()
            .filter(|p| !p.is_empty()),
        custom_prompt_reviewer: s.custom_prompt_reviewer.clone().filter(|p| !p.is_empty()),
        custom_prompt: s.custom_prompt.clone().filter(|p| !p.is_empty()),
    }
}
