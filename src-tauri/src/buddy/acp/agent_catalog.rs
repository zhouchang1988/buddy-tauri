//! ACP agent lookup, environment and binary resolution, port of
//! `src/main/buddy/acp/agent-catalog.ts`.

use crate::buddy::acp::codex_home::{codex_isolation_wrapper, prepare_codex_home};
use crate::buddy::acp::types::AcpError;
use crate::buddy::launchers::split_command;
use crate::buddy::types::{GlobalAcpAdapterItem, GlobalAcpAdaptersStatus};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const CLAUDE_ADAPTER: &str = "claude-agent-acp";
const CODEX_ADAPTER: &str = "codex-acp";
const CLAUDE_INSTALL: &str = "npm install -g @agentclientprotocol/claude-agent-acp";
const CODEX_INSTALL: &str = "npm install -g @agentclientprotocol/codex-acp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAcpBinary {
    pub command: String,
    pub args: Vec<String>,
    pub is_global: bool,
    pub adapter_name: Option<String>,
}

/// Locate the wecode executable on the system.
pub fn find_wecode_path() -> String {
    if let Some(home) = dirs::home_dir() {
        let default_wecode = home.join(".wecode-cli").join("bin").join("wecode");
        if default_wecode.is_file() {
            return default_wecode.to_string_lossy().into_owned();
        }
    }
    let path_env = std::env::var("PATH").unwrap_or_default();
    for dir in path_env.split(':').filter(|dir| !dir.is_empty()) {
        let candidate = Path::new(dir).join("wecode");
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "wecode".to_string()
}

/// Ensure an executable wrapper script exists for codex-acp to drive `wecode codex`.
pub async fn ensure_wecode_codex_wrapper(data_root: Option<&Path>) -> Result<String, AcpError> {
    let bin_dir = match data_root {
        Some(root) => root.join("bin"),
        None => std::env::temp_dir().join("bin"),
    };
    tokio::fs::create_dir_all(&bin_dir).await?;
    let wrapper_path = bin_dir.join("wecode-codex");
    let script = "#!/bin/bash\nWECODE_BIN=\"${WECODE_BIN:-$(which wecode 2>/dev/null || echo \"$HOME/.wecode-cli/bin/wecode\")}\"\nexec \"$WECODE_BIN\" codex \"$@\"\n";
    tokio::fs::write(&wrapper_path, script).await?;
    set_mode(&wrapper_path, 0o755)?;
    Ok(wrapper_path.to_string_lossy().into_owned())
}

/// Prepare environment variables required by specific ACP agents.
pub async fn prepare_acp_environment(
    actor: &str,
    base_env: HashMap<String, String>,
    data_root: Option<&Path>,
    session_id: Option<&str>,
) -> Result<HashMap<String, String>, AcpError> {
    let mut env = base_env;
    let codex_data_root = match data_root {
        Some(root) => root.to_path_buf(),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".buddy"),
    };

    if actor == "codex" || actor == "wecode_codex" {
        let home = prepare_codex_home(actor, &codex_data_root, &env, session_id, None).await?;
        env.insert("CODEX_HOME".to_string(), home);
        // Desktop can export a separate SQLite root. Do not inherit that shared
        // index even after isolating the rollout files with CODEX_HOME.
        if non_empty(&env, "CODEX_SQLITE_HOME").is_none() {
            let home = env.get("CODEX_HOME").cloned().unwrap_or_default();
            env.insert("CODEX_SQLITE_HOME".to_string(), home);
        }
    }

    if actor == "wecode_claude" {
        if non_empty(&env, "CLAUDE_CODE_EXECUTABLE").is_none() {
            env.insert("CLAUDE_CODE_EXECUTABLE".to_string(), find_wecode_path());
        }
    } else if actor == "wecode_codex" && non_empty(&env, "CODEX_PATH").is_none() {
        env.insert(
            "CODEX_PATH".to_string(),
            ensure_wecode_codex_wrapper(data_root).await?,
        );
    }

    if actor == "codex" || actor == "wecode_codex" {
        let executable = non_empty(&env, "CODEX_PATH")
            .or_else(|| {
                std::env::var("CODEX_PATH")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "codex".to_string());
        let sqlite_home = env.get("CODEX_SQLITE_HOME").cloned().unwrap_or_default();
        env.insert("BUDDY_CODEX_EXECUTABLE".to_string(), executable);
        env.insert(
            "BUDDY_CODEX_SQLITE_CONFIG".to_string(),
            format!("sqlite_home={}", serde_json::to_string(&sqlite_home)?),
        );
        env.insert(
            "CODEX_PATH".to_string(),
            codex_isolation_wrapper(&codex_data_root).await?,
        );
    }

    Ok(env)
}

/// Return default ACP arguments for known CLI commands if not explicitly provided.
pub fn default_acp_args(command: &str, actor: Option<&str>) -> Vec<String> {
    let raw_tokens = split_command(command);
    let exe = basename(raw_tokens.first().map(String::as_str).unwrap_or("")).to_lowercase();
    let second_token = raw_tokens.get(1).map(|token| token.to_lowercase());

    if raw_tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case("acp"))
    {
        return Vec::new();
    }
    if exe.contains("codex-acp") || exe.contains("claude-agent-acp") {
        return Vec::new();
    }
    if exe == "wecode" {
        if second_token.as_deref() == Some("opencode") {
            return vec!["acp".to_string()];
        }
        if actor == Some("opencode") || actor == Some("wecode_opencode") {
            return vec!["opencode".to_string(), "acp".to_string()];
        }
        return vec!["acp".to_string()];
    }
    vec!["acp".to_string()]
}

/// Locate a globally installed executable across PATH and standard user bin directories.
pub fn find_global_executable(name: &str) -> Option<String> {
    let path_env = std::env::var("PATH").unwrap_or_default();
    find_in_dirs(name, &search_dirs(&path_env))
}

pub fn resolve_acp_binary(command: &str, args: &[String]) -> ResolvedAcpBinary {
    resolve_acp_binary_with(command, args, find_global_executable)
}

pub fn check_global_acp_adapters() -> GlobalAcpAdaptersStatus {
    adapter_status(find_global_executable)
}

fn resolve_acp_binary_with(
    command: &str,
    args: &[String],
    find: impl Fn(&str) -> Option<String>,
) -> ResolvedAcpBinary {
    let joined = args.join(" ");
    let is_npx = command == "npx" || command.ends_with("/npx");

    if is_npx && joined.contains(CLAUDE_ADAPTER) {
        return prefer_global(command, args, CLAUDE_ADAPTER, &find);
    }
    if is_npx && joined.contains(CODEX_ADAPTER) {
        return prefer_global(command, args, CODEX_ADAPTER, &find);
    }
    if command == CLAUDE_ADAPTER || command.ends_with(&format!("/{CLAUDE_ADAPTER}")) {
        return direct_or_npx(
            command,
            args,
            CLAUDE_ADAPTER,
            "@agentclientprotocol/claude-agent-acp",
            &find,
        );
    }
    if command == CODEX_ADAPTER || command.ends_with(&format!("/{CODEX_ADAPTER}")) {
        return direct_or_npx(
            command,
            args,
            CODEX_ADAPTER,
            "@agentclientprotocol/codex-acp",
            &find,
        );
    }
    ResolvedAcpBinary {
        command: command.to_string(),
        args: args.to_vec(),
        is_global: false,
        adapter_name: None,
    }
}

fn prefer_global(
    command: &str,
    args: &[String],
    adapter: &str,
    find: &impl Fn(&str) -> Option<String>,
) -> ResolvedAcpBinary {
    if let Some(global_bin) = find(adapter) {
        ResolvedAcpBinary {
            command: global_bin,
            args: Vec::new(),
            is_global: true,
            adapter_name: Some(adapter.to_string()),
        }
    } else {
        ResolvedAcpBinary {
            command: command.to_string(),
            args: args.to_vec(),
            is_global: false,
            adapter_name: Some(adapter.to_string()),
        }
    }
}

fn direct_or_npx(
    _command: &str,
    args: &[String],
    adapter: &str,
    package: &str,
    find: &impl Fn(&str) -> Option<String>,
) -> ResolvedAcpBinary {
    if let Some(global_bin) = find(adapter) {
        ResolvedAcpBinary {
            command: global_bin,
            args: args.to_vec(),
            is_global: true,
            adapter_name: Some(adapter.to_string()),
        }
    } else {
        let mut npx_args = vec!["-y".to_string(), package.to_string()];
        npx_args.extend(args.iter().cloned());
        ResolvedAcpBinary {
            command: "npx".to_string(),
            args: npx_args,
            is_global: false,
            adapter_name: Some(adapter.to_string()),
        }
    }
}

fn adapter_status(find: impl Fn(&str) -> Option<String>) -> GlobalAcpAdaptersStatus {
    let claude_bin = find(CLAUDE_ADAPTER);
    let codex_bin = find(CODEX_ADAPTER);
    GlobalAcpAdaptersStatus {
        claude: GlobalAcpAdapterItem {
            installed: claude_bin.is_some(),
            binary_path: claude_bin,
            install_command: CLAUDE_INSTALL.to_string(),
        },
        codex: GlobalAcpAdapterItem {
            installed: codex_bin.is_some(),
            binary_path: codex_bin,
            install_command: CODEX_INSTALL.to_string(),
        },
    }
}

fn search_dirs(path_env: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for dir in path_env.split(':').filter(|dir| !dir.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = dirs::home_dir() {
        // Electron checks only `process.version`. There is no Node process
        // here, so scan installed nvm versions instead.
        let nvm = home.join(".nvm").join("versions").join("node");
        if let Ok(entries) = std::fs::read_dir(&nvm) {
            let mut versions: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            versions.sort();
            for version in versions {
                dirs.push(version.join("bin"));
            }
        }
        dirs.push(home.join(".npm-global").join("bin"));
        dirs.push(home.join("bin"));
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".wecode-cli").join("bin"));
    }
    dirs
}

fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<String> {
    let mut seen = HashSet::new();
    for dir in dirs {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let candidate = dir.join(name);
        match candidate.try_exists() {
            Ok(true) => return Some(candidate.to_string_lossy().into_owned()),
            _ => continue,
        }
    }
    None
}

fn basename(token: &str) -> &str {
    Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
}

fn non_empty(env: &HashMap<String, String>, key: &str) -> Option<String> {
    env.get(key).filter(|value| !value.is_empty()).cloned()
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::acp::codex_home::prepare_codex_home;
    use std::os::unix::fs::PermissionsExt;

    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        &LOCK
    }

    struct EnvVar {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVar {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    async fn seed_desktop(root: &Path) -> std::path::PathBuf {
        let source = root.join("desktop");
        tokio::fs::create_dir_all(&source).await.unwrap();
        tokio::fs::write(source.join("config.toml"), "model = \"test-model\"\n")
            .await
            .unwrap();
        tokio::fs::write(source.join("auth.json"), "{\"token\":\"test-only\"}")
            .await
            .unwrap();
        std::fs::set_permissions(
            source.join("auth.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        tokio::fs::write(source.join("state_5.sqlite"), "not a real database")
            .await
            .unwrap();
        source
    }

    #[tokio::test]
    async fn isolates_both_actors_and_keeps_private_bootstrap_config() {
        let _guard = env_lock().lock().await;
        let root = tempfile::tempdir().unwrap();
        let source = seed_desktop(root.path()).await;
        let _codex_home = EnvVar::set("CODEX_HOME", source.to_str().unwrap());
        let _sqlite = EnvVar::set("CODEX_SQLITE_HOME", source.to_str().unwrap());

        for actor in ["codex", "wecode_codex"] {
            let mut base = HashMap::new();
            base.insert("CUSTOM".to_string(), "kept".to_string());
            let env = prepare_acp_environment(actor, base, Some(root.path()), None)
                .await
                .unwrap();
            let expected = root.path().join("acp").join(actor).join("codex-home");
            assert_eq!(
                env.get("CODEX_HOME").map(String::as_str),
                Some(expected.to_str().unwrap())
            );
            assert_eq!(env.get("CODEX_SQLITE_HOME"), env.get("CODEX_HOME"));
            assert_eq!(env.get("CUSTOM").map(String::as_str), Some("kept"));
            let config = tokio::fs::read_to_string(expected.join("config.toml"))
                .await
                .unwrap();
            assert!(config.contains("test-model"));
            let auth_mode = std::fs::symlink_metadata(expected.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(auth_mode, 0o600);
            assert!(!std::fs::symlink_metadata(expected.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink());
            let names = std::fs::read_dir(&expected)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert!(!names.iter().any(|name| name == "state_5.sqlite"));
            tokio::fs::write(
                expected.join("config.toml"),
                "private provider configuration",
            )
            .await
            .unwrap();
            prepare_acp_environment(actor, HashMap::new(), Some(root.path()), None)
                .await
                .unwrap();
            let kept = tokio::fs::read_to_string(expected.join("config.toml"))
                .await
                .unwrap();
            assert_eq!(kept, "private provider configuration");
        }
        let source_config = tokio::fs::read_to_string(source.join("config.toml"))
            .await
            .unwrap();
        assert_eq!(source_config, "model = \"test-model\"\n");
    }

    #[tokio::test]
    async fn copies_only_a_selected_legacy_rollout() {
        let root = tempfile::tempdir().unwrap();
        let source = seed_desktop(root.path()).await;
        let relative = Path::new("sessions").join("2026").join("09").join("18");
        tokio::fs::create_dir_all(source.join(&relative))
            .await
            .unwrap();
        let filename = "rollout-2026-09-18T10-00-00-legacy-123.jsonl";
        tokio::fs::write(source.join(&relative).join(filename), "legacy history")
            .await
            .unwrap();
        tokio::fs::write(
            source.join(&relative).join("rollout-other.jsonl"),
            "unrelated history",
        )
        .await
        .unwrap();
        let home = prepare_codex_home(
            "codex",
            root.path(),
            &HashMap::new(),
            Some("legacy-123"),
            Some(&source),
        )
        .await
        .unwrap();
        let copied = PathBuf::from(&home).join(&relative);
        let mut names = std::fs::read_dir(&copied)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, vec![filename.to_string()]);
        assert_eq!(
            tokio::fs::read_to_string(copied.join(filename))
                .await
                .unwrap(),
            "legacy history"
        );
        tokio::fs::write(copied.join(filename), "resumed progress")
            .await
            .unwrap();
        prepare_codex_home(
            "codex",
            root.path(),
            &HashMap::new(),
            Some("legacy-123"),
            Some(&source),
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(copied.join(filename))
                .await
                .unwrap(),
            "resumed progress"
        );
        assert_eq!(
            tokio::fs::read_to_string(source.join(&relative).join(filename))
                .await
                .unwrap(),
            "legacy history"
        );
    }

    #[tokio::test]
    async fn serializes_concurrent_initialization_without_partial_files() {
        let root = tempfile::tempdir().unwrap();
        let source = seed_desktop(root.path()).await;
        let mut tasks = Vec::new();
        for _ in 0..6 {
            let root_path = root.path().to_path_buf();
            let source_path = source.clone();
            tasks.push(tokio::spawn(async move {
                prepare_codex_home(
                    "codex",
                    &root_path,
                    &HashMap::new(),
                    None,
                    Some(&source_path),
                )
                .await
            }));
        }
        let mut homes = Vec::new();
        for task in tasks {
            homes.push(task.await.unwrap().unwrap());
        }
        assert_eq!(homes.iter().collect::<HashSet<_>>().len(), 1);
        let auth = tokio::fs::read_to_string(Path::new(&homes[0]).join("auth.json"))
            .await
            .unwrap();
        assert_eq!(auth, "{\"token\":\"test-only\"}");
        let leftover = std::fs::read_dir(&homes[0]).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        });
        assert!(!leftover);
    }

    #[tokio::test]
    async fn honors_an_explicit_launcher_home() {
        let root = tempfile::tempdir().unwrap();
        let custom = root.path().join("custom");
        let mut base = HashMap::new();
        base.insert(
            "CODEX_HOME".to_string(),
            custom.to_string_lossy().into_owned(),
        );
        let env = prepare_acp_environment("codex", base, Some(root.path()), None)
            .await
            .unwrap();
        assert_eq!(env.get("CODEX_HOME").map(Path::new), Some(custom.as_path()));
        assert_eq!(env.get("CODEX_SQLITE_HOME"), env.get("CODEX_HOME"));
        let mut opencode = HashMap::new();
        opencode.insert("CUSTOM".to_string(), "kept".to_string());
        let untouched =
            prepare_acp_environment("opencode", opencode.clone(), Some(root.path()), None)
                .await
                .unwrap();
        assert_eq!(untouched, opencode);
    }

    #[tokio::test]
    async fn wraps_a_custom_executable_and_passes_the_db_override_literally() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fake codex");
        tokio::fs::write(&executable, "#!/bin/bash\nprintf \"%s\\n\" \"$@\"\n")
            .await
            .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let custom_home = root.path().join("home with \"quotes\" and $literal");
        let mut base = HashMap::new();
        base.insert(
            "CODEX_HOME".to_string(),
            custom_home.to_string_lossy().into_owned(),
        );
        base.insert(
            "CODEX_PATH".to_string(),
            executable.to_string_lossy().into_owned(),
        );
        let env = prepare_acp_environment("codex", base, Some(root.path()), None)
            .await
            .unwrap();
        let output = tokio::process::Command::new(env.get("CODEX_PATH").unwrap())
            .arg("app-server")
            .env_clear()
            .envs(&env)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let expected_config = format!(
            "sqlite_home={}",
            serde_json::to_string(&custom_home.to_string_lossy().into_owned()).unwrap()
        );
        assert_eq!(
            stdout.split('\n').collect::<Vec<_>>(),
            vec!["-c", expected_config.as_str(), "app-server", ""]
        );
    }

    #[test]
    fn default_args_follow_the_command_and_actor() {
        assert_eq!(
            default_acp_args("wecode", Some("wecode_claude")),
            vec!["acp".to_string()]
        );
        assert_eq!(
            default_acp_args("wecode", Some("wecode_opencode")),
            vec!["opencode".to_string(), "acp".to_string()]
        );
        assert_eq!(
            default_acp_args("wecode opencode", Some("wecode_opencode")),
            vec!["acp".to_string()]
        );
        assert_eq!(default_acp_args("wecode acp", None), Vec::<String>::new());
        assert_eq!(
            default_acp_args("/usr/local/bin/codex-acp", Some("codex")),
            Vec::<String>::new()
        );
        assert_eq!(default_acp_args("opencode", None), vec!["acp".to_string()]);
        assert_eq!(
            default_acp_args("cursor-agent", None),
            vec!["acp".to_string()]
        );
    }

    #[test]
    fn resolve_prefers_a_global_adapter_and_falls_back_to_npx() {
        let args = vec![
            "-y".to_string(),
            "@agentclientprotocol/claude-agent-acp".to_string(),
        ];
        let found = resolve_acp_binary_with("npx", &args, |name| {
            (name == CLAUDE_ADAPTER).then(|| "/opt/claude-agent-acp".to_string())
        });
        assert_eq!(found.command, "/opt/claude-agent-acp");
        assert!(found.args.is_empty());
        assert!(found.is_global);

        let missing = resolve_acp_binary_with("codex-acp", &[], |_| None);
        assert_eq!(missing.command, "npx");
        assert_eq!(
            missing.args,
            vec![
                "-y".to_string(),
                "@agentclientprotocol/codex-acp".to_string()
            ]
        );
        assert!(!missing.is_global);
        assert_eq!(missing.adapter_name.as_deref(), Some(CODEX_ADAPTER));
    }

    #[test]
    fn check_reports_install_commands_when_adapters_are_absent() {
        let status = adapter_status(|_| None);
        assert!(!status.claude.installed);
        assert_eq!(status.claude.binary_path, None);
        assert_eq!(status.claude.install_command, CLAUDE_INSTALL);
        assert!(!status.codex.installed);
        assert_eq!(status.codex.install_command, CODEX_INSTALL);
    }
}
