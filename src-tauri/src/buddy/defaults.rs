//! Defaults & normalization, port of `src/shared/defaults.ts`.

use crate::buddy::types::{GlobalSettings, Launcher};
use std::collections::HashMap;

pub const DEFAULT_LAUNCHER_ORDER: [&str; 5] = ["claude", "codex", "cursor", "opencode", "kimi"];

pub const DEFAULT_LAUNCHER_TIMEOUT_SECONDS: u64 = 7200;

pub fn default_launcher_command(actor: &str) -> &str {
    match actor {
        "claude" => "claude",
        "codex" => "codex",
        "cursor" => "cursor-agent",
        "opencode" => "opencode",
        "kimi" => "kimi",
        other => other,
    }
}

pub fn default_launcher_for(actor: &str) -> Launcher {
    Launcher {
        command: default_launcher_command(actor).to_string(),
        env: HashMap::new(),
        timeout_seconds: DEFAULT_LAUNCHER_TIMEOUT_SECONDS,
    }
}

pub fn normalize_launcher(actor: &str, launcher: Option<&Launcher>) -> Launcher {
    let fallback = default_launcher_for(actor);
    match launcher {
        Some(l) => Launcher {
            command: if !l.command.trim().is_empty() {
                l.command.clone()
            } else {
                fallback.command
            },
            env: l.env.clone(),
            timeout_seconds: l.timeout_seconds,
        },
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
        seed_opencode_session_id: Some(s.seed_opencode_session_id.clone().unwrap_or_default()),
        seed_kimi_session_id: Some(s.seed_kimi_session_id.clone().unwrap_or_default()),
        max_compact_retries: Some(s.max_compact_retries.unwrap_or(3)),
        auto_generate_commit_message: Some(s.auto_generate_commit_message.unwrap_or(true)),
        system_notifications_enabled: Some(s.system_notifications_enabled.unwrap_or(true)),
        max_upgrade_retries: Some(s.max_upgrade_retries.unwrap_or(3)),
        custom_prompt_implementer: s
            .custom_prompt_implementer
            .clone()
            .filter(|p| !p.is_empty()),
        custom_prompt_reviewer: s.custom_prompt_reviewer.clone().filter(|p| !p.is_empty()),
    }
}
