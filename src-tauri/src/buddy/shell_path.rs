//! PATH fix-up when launched from Finder, port of `src/main/buddy/shell-path.ts`.

use std::path::PathBuf;
use std::process::Command;

pub fn install_hint_for(command: &str) -> Option<&'static str> {
    match command {
        "kimi" => Some("curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash"),
        "claude" => Some("npm install -g @anthropic-ai/claude-code"),
        "codex" => Some("npm install -g @openai/codex"),
        "cursor-agent" | "agent" => Some("curl -fsS https://cursor.com/install | bash"),
        "opencode" => Some("go install github.com/sst/opencode@latest"),
        _ => None,
    }
}

/// On macOS, GUI apps inherit a minimal PATH. Merge the login-shell PATH plus
/// well-known tool install locations so actor CLIs are discoverable.
pub fn fix_shell_path() {
    if std::env::consts::OS != "macos" {
        return;
    }
    if std::env::var("NODE_ENV").map(|v| v == "test").unwrap_or(false) {
        return;
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let extras = [
        home.join(".kimi-code/bin"),
        home.join(".local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".npm-global/bin"),
        home.join(".cargo/bin"),
    ];

    if let Ok(shell) = std::env::var("SHELL").or_else(|_| Ok::<String, std::env::VarError>("/bin/zsh".to_string())) {
        let output = Command::new(shell)
            .args(["-il", "-c", "echo \"$PATH\""])
            .output();
        if let Ok(out) = output {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() && Some(&path) != std::env::var("PATH").ok().as_ref() {
                std::env::set_var("PATH", &path);
            }
        }
    }

    // Always merge common tool paths — the login shell PATH may be incomplete
    // when the app is launched from Finder (no terminal context).
    let current: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|s| s.to_string())
        .collect();
    let mut merged: Vec<String> = Vec::new();
    for extra in &extras {
        let s = extra.to_string_lossy().to_string();
        if !merged.contains(&s) {
            merged.push(s);
        }
    }
    for c in current {
        if !merged.contains(&c) {
            merged.push(c);
        }
    }
    std::env::set_var("PATH", merged.join(":"));
}
