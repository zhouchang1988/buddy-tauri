use crate::buddy::agents::{AgentManagementService, LocalAgent};
use crate::buddy::launchers::split_command;
use crate::buddy::store::atomic_write_json;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryCapabilities {
    pub read_skills: bool,
    pub write_skills: bool,
    pub read_mcp: bool,
    pub write_mcp: bool,
    pub auth_probe: bool,
    pub diagnose: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryTarget {
    pub target_id: String,
    pub agent_id: String,
    pub display_name: String,
    pub config_context: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_physical_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_physical_key: Option<String>,
    pub mcp_format: String,
    pub capabilities: LibraryCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_init: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_skill_target_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by_tasks: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySkillRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub library_path: String,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub assigned_target_ids: Vec<String>,
    pub import_mode: String,
    pub managed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSkill {
    pub discovery_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub path: String,
    pub physical_key: String,
    pub target_id: String,
    pub agent_id: String,
    pub scope: String,
    pub is_symlink: bool,
    pub is_git_root: bool,
    pub already_imported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMcpServer {
    pub id: String,
    pub name: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub raw: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_target_id: Option<String>,
    pub scope: String,
    #[serde(default)]
    pub assigned_target_ids: Vec<String>,
    pub managed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSource {
    pub target_id: String,
    pub agent_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredMcpServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<McpSource>>,
    pub discovery_id: String,
    pub name: String,
    pub transport: String,
    pub summary: String,
    pub target_id: String,
    pub agent_id: String,
    pub scope: String,
    pub path: String,
    pub physical_key: String,
    pub already_imported: bool,
    #[serde(default)]
    pub raw: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetAuthStatus {
    pub target_id: String,
    pub server_name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySnapshot {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<String>,
    pub refreshing: bool,
    pub targets: Vec<LibraryTarget>,
    pub skills: Vec<LibrarySkillRecord>,
    pub discovered_skills: Vec<DiscoveredSkill>,
    pub servers: Vec<LibraryMcpServer>,
    pub discovered_servers: Vec<DiscoveredMcpServer>,
    pub auth: Vec<TargetAuthStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryOpResult {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected_target_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryBatchResult {
    pub results: Vec<LibraryOpResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedBinding {
    pub kind: String,
    pub fingerprint: String,
    pub path: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryMetaFile {
    pub schema_version: u32,
    #[serde(default)]
    pub skills: Vec<LibrarySkillRecord>,
    #[serde(default)]
    pub servers: Vec<LibraryMcpServer>,
    #[serde(default)]
    pub applied: HashMap<String, AppliedBinding>,
}

impl Default for LibraryMetaFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            skills: Vec::new(),
            servers: Vec::new(),
            applied: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureAcpHomeResult {
    pub home: String,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthGuidanceResult {
    pub title: String,
    pub steps: Vec<String>,
}

struct LibraryStore {
    data_root: PathBuf,
}

impl LibraryStore {
    fn new(data_root: &Path) -> Self {
        Self {
            data_root: data_root.to_path_buf(),
        }
    }
    fn root(&self) -> PathBuf {
        self.data_root.join("library")
    }
    fn skills_dir(&self) -> PathBuf {
        self.root().join("skills")
    }
    fn backups_dir(&self) -> PathBuf {
        self.root().join("backups")
    }
    fn meta_path(&self) -> PathBuf {
        self.root().join("library.json")
    }
    async fn ensure_layout(&self) -> Result<(), String> {
        tokio::fs::create_dir_all(self.skills_dir())
            .await
            .map_err(|error| error.to_string())?;
        tokio::fs::create_dir_all(self.backups_dir())
            .await
            .map_err(|error| error.to_string())
    }
    async fn read_meta(&self) -> Result<LibraryMetaFile, String> {
        self.ensure_layout().await?;
        match tokio::fs::read_to_string(self.meta_path()).await {
            Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(LibraryMetaFile::default())
            }
            Err(error) => Err(error.to_string()),
        }
    }
    async fn write_meta(&self, meta: &LibraryMetaFile) -> Result<(), String> {
        self.ensure_layout().await?;
        atomic_write_json(
            &self.meta_path(),
            &serde_json::to_value(meta).map_err(|error| error.to_string())?,
        )
        .await
        .map_err(|error| error.to_string())
    }
    async fn backup_tree(&self, source: &Path, label: &str) -> Result<PathBuf, String> {
        let safe = Regex::new(r"[^a-zA-Z0-9._-]+")
            .unwrap()
            .replace_all(label, "_");
        let dir = self.backups_dir().join(format!(
            "{}-{}",
            chrono::Utc::now().timestamp_millis(),
            safe.chars().take(40).collect::<String>()
        ));
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| error.to_string())?;
        let target = dir.join("content");
        copy_tree(source, &target).await?;
        Ok(target)
    }
}

pub struct LibraryService {
    store: LibraryStore,
    agents: Arc<AgentManagementService>,
    cache: RwLock<Option<LibrarySnapshot>>,
    operation: Mutex<()>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}
async fn physical(path: Option<&String>) -> Option<String> {
    let path = path?;
    Some(
        tokio::fs::canonicalize(path)
            .await
            .unwrap_or_else(|_| PathBuf::from(path))
            .to_string_lossy()
            .to_string(),
    )
}
fn capabilities(read: bool) -> LibraryCapabilities {
    LibraryCapabilities {
        read_skills: read,
        write_skills: read,
        read_mcp: read,
        write_mcp: read,
        auth_probe: false,
        diagnose: false,
        reason: None,
    }
}
fn env_of(agent: Option<&LocalAgent>) -> HashMap<String, String> {
    let mut env = agent
        .and_then(|agent| agent.cli.as_ref())
        .map(|launcher| launcher.env.clone())
        .unwrap_or_default();
    env.extend(
        agent
            .and_then(|agent| agent.acp.as_ref())
            .map(|launcher| launcher.env.clone())
            .unwrap_or_default(),
    );
    env
}
fn claude_home(env: &HashMap<String, String>) -> PathBuf {
    env.get("CLAUDE_CONFIG_DIR")
        .cloned()
        .or_else(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"))
}
fn codex_home(env: &HashMap<String, String>) -> PathBuf {
    env.get("CODEX_HOME")
        .cloned()
        .or_else(|| std::env::var("CODEX_HOME").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
}
fn opencode_home() -> PathBuf {
    std::env::var("OPENCODE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config/opencode"))
}
fn buddy_acp_home(data_root: &Path, actor: &str) -> PathBuf {
    data_root.join("acp").join(actor).join("codex-home")
}
fn dsh_profile(agent: Option<&LocalAgent>) -> String {
    let args = agent
        .and_then(|agent| {
            if agent.selected_protocol.as_deref() == Some("acp") {
                agent.acp.as_ref()
            } else {
                agent.cli.as_ref()
            }
        })
        .and_then(|launcher| launcher.args.clone())
        .unwrap_or_default();
    let explicit = args
        .iter()
        .position(|arg| arg == "--profile")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .or_else(|| {
            args.iter()
                .find_map(|arg| arg.strip_prefix("--profile=").map(String::from))
        });
    let name = explicit
        .or_else(|| {
            args.iter()
                .find(|arg| matches!(arg.as_str(), "headless" | "acp" | "desktop" | "web"))
                .cloned()
        })
        .unwrap_or_else(|| {
            if agent.and_then(|agent| agent.selected_protocol.as_deref()) == Some("acp") {
                "acp".to_string()
            } else {
                "headless".to_string()
            }
        });
    if Regex::new(r"^[A-Za-z0-9_-]+$").unwrap().is_match(&name) {
        name
    } else {
        "headless".to_string()
    }
}

async fn resolve_library_targets(agents: &[LocalAgent], data_root: &Path) -> Vec<LibraryTarget> {
    struct Spec {
        agent_id: String,
        display_name: String,
        context: String,
        skills_dir: Option<PathBuf>,
        mcp_path: Option<PathBuf>,
        format: String,
        capabilities: LibraryCapabilities,
        pending_init: Option<bool>,
        used_by_tasks: Option<bool>,
    }
    let by_id = agents
        .iter()
        .map(|agent| (agent.id.clone(), agent))
        .collect::<HashMap<_, _>>();
    let mut specs = vec![Spec {
        agent_id: "shared".to_string(),
        display_name: "~/.agents".to_string(),
        context: "native".to_string(),
        skills_dir: Some(home().join(".agents/skills")),
        mcp_path: Some(home().join(".agents/mcp.json")),
        format: "jsonc-mcpServers".to_string(),
        capabilities: capabilities(true),
        pending_init: None,
        used_by_tasks: None,
    }];
    let mut add_native =
        |id: &str, name: &str, skills_dir: PathBuf, mcp_path: PathBuf, format: &str| {
            let agent = by_id.get(id);
            let mut caps = capabilities(true);
            if id == "claude" {
                caps.auth_probe = true;
            }
            specs.push(Spec {
                agent_id: id.to_string(),
                display_name: name.to_string(),
                context: "native".to_string(),
                skills_dir: Some(skills_dir),
                mcp_path: Some(mcp_path),
                format: format.to_string(),
                capabilities: caps,
                pending_init: None,
                used_by_tasks: Some(agent.is_some_and(|agent| agent.enabled)),
            });
        };
    add_native(
        "claude",
        "Claude Code",
        claude_home(&env_of(by_id.get("claude").copied())).join("skills"),
        home().join(".claude.json"),
        "jsonc-mcpServers",
    );
    add_native(
        "codex",
        "Codex",
        codex_home(&env_of(by_id.get("codex").copied())).join("skills"),
        codex_home(&env_of(by_id.get("codex").copied())).join("config.toml"),
        "toml-mcp_servers",
    );
    add_native(
        "cursor",
        "Cursor",
        home().join(".cursor/skills"),
        home().join(".cursor/mcp.json"),
        "jsonc-mcpServers",
    );
    add_native(
        "opencode",
        "OpenCode",
        opencode_home().join("skills"),
        opencode_home().join("opencode.json"),
        "jsonc-mcpServers",
    );
    add_native(
        "agy",
        "Antigravity",
        home().join(".gemini/skills"),
        home().join(".gemini/mcp_config.json"),
        "jsonc-mcpServers",
    );
    add_native(
        "kimi",
        "Kimi Code",
        home().join(".kimi/skills"),
        home().join(".kimi/mcp.json"),
        "jsonc-mcpServers",
    );
    let codebuddy_root = std::env::var("CODEBUDDY_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".codebuddy"));
    add_native(
        "codebuddy",
        "CodeBuddy",
        codebuddy_root.join("skills"),
        home().join(".codebuddy.json"),
        "jsonc-mcpServers",
    );
    let dsh_root = env_of(by_id.get("dsh").copied())
        .get("DSH_HOME")
        .cloned()
        .or_else(|| std::env::var("DSH_HOME").ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".dsh"));
    add_native(
        "dsh",
        "DeepSeek Harness",
        dsh_root.join("skills"),
        dsh_root
            .join("profiles")
            .join(dsh_profile(by_id.get("dsh").copied()))
            .join("cordis.patch.yml"),
        "dsh-patch",
    );
    for (id, name, native) in [
        ("wecode_claude", "WeCode Claude", "claude"),
        ("wecode_opencode", "WeCode OpenCode", "opencode"),
    ] {
        let Some(agent) = by_id.get(id).copied() else {
            continue;
        };
        let env = env_of(Some(agent));
        let (skills, mcp) = if native == "claude" {
            (
                claude_home(&env).join("skills"),
                home().join(".claude.json"),
            )
        } else {
            let root = env
                .get("OPENCODE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(opencode_home);
            (root.join("skills"), root.join("opencode.json"))
        };
        specs.push(Spec {
            agent_id: id.to_string(),
            display_name: name.to_string(),
            context: "wecode".to_string(),
            skills_dir: Some(skills),
            mcp_path: Some(mcp),
            format: "jsonc-mcpServers".to_string(),
            capabilities: capabilities(true),
            pending_init: None,
            used_by_tasks: Some(agent.enabled),
        });
    }
    for actor in ["codex", "wecode_codex"] {
        let agent = by_id.get(actor).copied();
        if agent.is_none() && actor == "wecode_codex" {
            continue;
        }
        let acp_home = agent
            .and_then(|agent| env_of(Some(agent)).get("CODEX_HOME").cloned())
            .map(PathBuf::from)
            .unwrap_or_else(|| buddy_acp_home(data_root, actor));
        let ready = tokio::fs::metadata(&acp_home).await.is_ok();
        let mut caps = capabilities(ready);
        caps.write_mcp = true;
        if !ready {
            caps.reason = Some("Buddy ACP home not initialized yet".to_string());
        }
        specs.push(Spec {
            agent_id: actor.to_string(),
            display_name: if actor == "codex" {
                "Codex ACP".to_string()
            } else {
                "WeCode Codex ACP".to_string()
            },
            context: if actor.starts_with("wecode") {
                "wecode".to_string()
            } else {
                "buddy-acp".to_string()
            },
            skills_dir: Some(acp_home.join("skills")),
            mcp_path: Some(acp_home.join("config.toml")),
            format: "toml-mcp_servers".to_string(),
            capabilities: caps,
            pending_init: Some(!ready),
            used_by_tasks: Some(agent.is_some_and(|agent| {
                agent.enabled
                    && (agent.selected_protocol.as_deref() == Some("acp") || agent.cli.is_none())
            })),
        });
    }
    let mut targets = Vec::new();
    for spec in specs {
        let skills_dir = spec.skills_dir.map(|path| path_string(&path));
        let mcp_path = spec.mcp_path.map(|path| path_string(&path));
        targets.push(LibraryTarget {
            target_id: format!("{}:{}:user", spec.agent_id, spec.context),
            agent_id: spec.agent_id,
            display_name: spec.display_name,
            config_context: spec.context,
            scope: "user".to_string(),
            skills_physical_key: physical(skills_dir.as_ref()).await,
            mcp_physical_key: physical(mcp_path.as_ref()).await,
            skills_dir,
            mcp_path,
            mcp_format: spec.format,
            capabilities: spec.capabilities,
            pending_init: spec.pending_init,
            used_by_tasks: spec.used_by_tasks,
            ..Default::default()
        });
    }
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for target in &targets {
        if let Some(key) = &target.skills_physical_key {
            groups
                .entry(key.clone())
                .or_default()
                .push(target.target_id.clone());
        }
    }
    for target in &mut targets {
        if let Some(key) = &target.skills_physical_key {
            let linked = groups
                .get(key)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|id| id != &target.target_id)
                .collect::<Vec<_>>();
            if !linked.is_empty() {
                target.linked_skill_target_ids = Some(linked);
            }
        }
    }
    targets
}

fn parse_jsonc(text: &str) -> Result<Value, String> {
    jsonc_parser::parse_to_serde_value(
        text,
        &jsonc_parser::ParseOptions {
            allow_trailing_commas: true,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "JSONC document is empty".to_string())
}

async fn atomic_write_text(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    let temporary = path.with_file_name(format!(
        "{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    tokio::fs::write(&temporary, text)
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|error| error.to_string())
}

fn matching_brace(text: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut index = start;
    let mut string = false;
    let mut escape = false;
    let mut line_comment = false;
    let mut block_comment = false;
    while index < bytes.len() {
        let ch = bytes[index];
        let next = bytes.get(index + 1).copied();
        if line_comment {
            if ch == b'\n' {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_comment {
            if ch == b'*' && next == Some(b'/') {
                block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if string {
            if escape {
                escape = false;
            } else if ch == b'\\' {
                escape = true;
            } else if ch == b'"' {
                string = false;
            }
            index += 1;
            continue;
        }
        if ch == b'/' && next == Some(b'/') {
            line_comment = true;
            index += 2;
            continue;
        }
        if ch == b'/' && next == Some(b'*') {
            block_comment = true;
            index += 2;
            continue;
        }
        if ch == b'"' {
            string = true;
            index += 1;
            continue;
        }
        if ch == open {
            depth += 1;
        }
        if ch == close {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn value_start_after_key(text: &str, key: &str) -> Option<usize> {
    let key_pos = text.find(key)?;
    let colon = text[key_pos + key.len()..].find(':')? + key_pos + key.len();
    Some(colon + 1 + text[colon + 1..].find(|ch: char| !ch.is_whitespace())?)
}

fn indent_json(value: &Value, spaces: usize) -> Result<String, String> {
    let pretty = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    let indent = " ".repeat(spaces);
    Ok(pretty
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn set_mcp_server_in_jsonc(
    text: &str,
    name: &str,
    server: &Map<String, Value>,
) -> Result<String, String> {
    let root_key = "\"mcpServers\"";
    if let Some(root_start) = value_start_after_key(text, root_key) {
        if text.as_bytes().get(root_start) == Some(&b'{') {
            let root_end = matching_brace(text, root_start, b'{', b'}')
                .ok_or_else(|| "Invalid mcpServers object".to_string())?;
            let name_key = serde_json::to_string(name).map_err(|error| error.to_string())?;
            let inner = &text[root_start + 1..root_end];
            if let Some(relative) = inner.find(&name_key) {
                let absolute = root_start + 1 + relative;
                let value_start = value_start_after_key(&text[absolute..], &name_key)
                    .map(|start| absolute + start)
                    .ok_or_else(|| "Invalid MCP server entry".to_string())?;
                let end = matching_brace(text, value_start, b'{', b'}')
                    .ok_or_else(|| "Invalid MCP server object".to_string())?;
                let replacement =
                    serde_json::to_string_pretty(server).map_err(|error| error.to_string())?;
                return Ok(format!(
                    "{}{}{}",
                    &text[..value_start],
                    replacement,
                    &text[end + 1..]
                ));
            }
            let encoded = indent_json(&Value::Object(server.clone()), 4)?;
            let before = text[..root_end].trim_end();
            let separator = if before.ends_with('{') { "" } else { "," };
            let insertion = format!("{}\n  {}: {}\n", separator, name_key, encoded);
            return Ok(format!(
                "{}{}{}",
                &text[..root_end],
                insertion,
                &text[root_end..]
            ));
        }
    }
    let root_end = text
        .rfind('}')
        .ok_or_else(|| "Expected a JSONC object".to_string())?;
    let server = indent_json(&Value::Object(server.clone()), 4)?;
    let name_key = serde_json::to_string(name).map_err(|error| error.to_string())?;
    let before = text[..root_end].trim_end();
    let separator = if before.ends_with('{') { "" } else { "," };
    let insertion = format!(
        "{}\n  \"mcpServers\": {{\n    {}: {}\n  }}\n",
        separator, name_key, server
    );
    Ok(format!(
        "{}{}{}",
        &text[..root_end],
        insertion,
        &text[root_end..]
    ))
}

fn delete_mcp_server_in_jsonc(text: &str, name: &str) -> Result<String, String> {
    let Some(root_start) = value_start_after_key(text, "\"mcpServers\"") else {
        return Ok(text.to_string());
    };
    let root_end = matching_brace(text, root_start, b'{', b'}')
        .ok_or_else(|| "Invalid mcpServers object".to_string())?;
    let name_key = serde_json::to_string(name).map_err(|error| error.to_string())?;
    let inner = &text[root_start + 1..root_end];
    let Some(relative) = inner.find(&name_key) else {
        return Ok(text.to_string());
    };
    let key_start = root_start + 1 + relative;
    let value_start = value_start_after_key(&text[key_start..], &name_key)
        .map(|start| key_start + start)
        .ok_or_else(|| "Invalid MCP server entry".to_string())?;
    let value_end = matching_brace(text, value_start, b'{', b'}')
        .ok_or_else(|| "Invalid MCP server object".to_string())?
        + 1;
    let mut start = key_start;
    let mut end = value_end;
    while end < root_end && text.as_bytes()[end].is_ascii_whitespace() {
        end += 1;
    }
    if text.as_bytes().get(end) == Some(&b',') {
        end += 1;
        while end < root_end && text.as_bytes()[end].is_ascii_whitespace() {
            end += 1;
        }
    } else {
        let mut cursor = start;
        while cursor > root_start + 1 && text.as_bytes()[cursor - 1].is_ascii_whitespace() {
            cursor -= 1;
        }
        if cursor > root_start + 1 && text.as_bytes()[cursor - 1] == b',' {
            start = cursor - 1;
        }
    }
    Ok(format!("{}{}", &text[..start], &text[end..]))
}

async fn write_mcp_server_jsonc(
    path: &Path,
    name: &str,
    server: &Map<String, Value>,
) -> Result<(), String> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => "{\n}\n".to_string(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(error) => return Err(error.to_string()),
    };
    parse_jsonc(&text)?;
    atomic_write_text(path, &set_mcp_server_in_jsonc(&text, name, server)?).await
}

async fn remove_mcp_server_jsonc(path: &Path, name: &str) -> Result<(), String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| error.to_string())?;
    atomic_write_text(path, &delete_mcp_server_in_jsonc(&text, name)?).await
}

fn toml_servers(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: toml::Value = toml::from_str(text).map_err(|error| error.to_string())?;
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    Ok(value
        .get("mcp_servers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

fn parse_toml_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('[') || trimmed.starts_with("[[") || !trimmed.ends_with(']') {
        return None;
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escape = false;
    for ch in inner.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' && quote.is_some() {
            escape = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '.' && quote.is_none() {
            if !current.trim().is_empty() {
                parts.push(current.trim().to_string());
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    Some(parts.join("."))
}

fn quote_toml_key(key: &str) -> String {
    if Regex::new(r"^[A-Za-z0-9_-]+$").unwrap().is_match(key) {
        key.to_string()
    } else {
        serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
    }
}

fn format_toml_value(value: &Value) -> String {
    match value {
        Value::String(value) => serde_json::to_string(value).unwrap_or_default(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_toml_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{ {} }}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{} = {}",
                    quote_toml_key(key),
                    format_toml_value(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Null => "\"\"".to_string(),
    }
}

fn table_spans(lines: &[String]) -> Vec<(String, usize, usize)> {
    let headers = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| parse_toml_header(line).map(|name| (name, index)))
        .collect::<Vec<_>>();
    headers
        .iter()
        .enumerate()
        .map(|(position, (name, from))| {
            (
                name.clone(),
                *from,
                headers
                    .get(position + 1)
                    .map(|next| next.1)
                    .unwrap_or(lines.len()),
            )
        })
        .collect()
}

async fn set_mcp_server_toml(
    path: &Path,
    name: &str,
    values: &Map<String, Value>,
) -> Result<(), String> {
    let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let mut lines = text.lines().map(String::from).collect::<Vec<_>>();
    let table_name = format!("mcp_servers.{name}");
    let spans = table_spans(&lines);
    let mut keep = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if let Some(span) = spans.iter().find(|span| {
            span.1 == index
                && (span.0 == table_name || span.0.starts_with(&format!("{table_name}.")))
        }) {
            index = span.2;
        } else {
            keep.push(lines[index].clone());
            index += 1;
        }
    }
    while keep.last().is_some_and(|line| line.trim().is_empty()) {
        keep.pop();
    }
    if !keep.is_empty() {
        keep.push(String::new());
    }
    keep.push(format!("[mcp_servers.{}]", quote_toml_key(name)));
    for (key, value) in values {
        keep.push(format!(
            "{} = {}",
            quote_toml_key(key),
            format_toml_value(value)
        ));
    }
    lines = keep;
    atomic_write_text(path, &(lines.join("\n") + "\n")).await
}

async fn delete_mcp_server_toml(path: &Path, name: &str) -> Result<(), String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| error.to_string())?;
    let lines = text.lines().map(String::from).collect::<Vec<_>>();
    let table_name = format!("mcp_servers.{name}");
    let spans = table_spans(&lines);
    let mut keep = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if let Some(span) = spans.iter().find(|span| {
            span.1 == index
                && (span.0 == table_name || span.0.starts_with(&format!("{table_name}.")))
        }) {
            index = span.2;
        } else {
            keep.push(lines[index].clone());
            index += 1;
        }
    }
    atomic_write_text(path, &(keep.join("\n") + "\n")).await
}

fn transport_of(raw: &Map<String, Value>) -> String {
    let kind = raw
        .get("type")
        .or_else(|| raw.get("transport"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    if matches!(
        kind.as_str(),
        "http" | "streamable-http" | "streamable_http"
    ) {
        return "http".to_string();
    }
    if kind == "sse" {
        return "sse".to_string();
    }
    if raw.contains_key("command") {
        return "stdio".to_string();
    }
    if raw.contains_key("url") {
        return "http".to_string();
    }
    "unknown".to_string()
}

fn summary_of(raw: &Map<String, Value>) -> String {
    if let Some(command) = raw.get("command").and_then(Value::as_str) {
        let args = raw
            .get("args")
            .and_then(Value::as_array)
            .map(|args| {
                args.iter()
                    .map(|arg| {
                        arg.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| arg.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        return if args.is_empty() {
            command.to_string()
        } else {
            format!("{command} {args}")
        };
    }
    raw.get("url")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| transport_of(raw))
}

#[derive(Debug, Clone)]
struct CanonicalMcp {
    transport: String,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<Map<String, Value>>,
    url: Option<String>,
    headers: Option<Map<String, Value>>,
}

fn string_map(value: Option<&Value>) -> Option<Map<String, Value>> {
    let object = value?.as_object()?;
    let out = object
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| {
            (
                key.clone(),
                Value::String(
                    value
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| value.to_string()),
                ),
            )
        })
        .collect::<Map<_, _>>();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn to_canonical(raw: &Map<String, Value>) -> Result<CanonicalMcp, String> {
    let kind = raw
        .get("type")
        .or_else(|| raw.get("transport"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let url = ["url", "serverUrl", "httpUrl"]
        .iter()
        .find_map(|key| raw.get(*key).and_then(Value::as_str))
        .map(String::from);
    let command = raw.get("command").and_then(|value| {
        value.as_str().map(String::from).or_else(|| {
            value
                .as_array()
                .and_then(|items| items.first())
                .map(|item| {
                    item.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| item.to_string())
                })
        })
    });
    let args = raw
        .get("command")
        .and_then(Value::as_array)
        .filter(|items| items.len() > 1)
        .map(|items| {
            items[1..]
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| item.to_string())
                })
                .collect()
        })
        .or_else(|| {
            raw.get("args").and_then(Value::as_array).map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| item.to_string())
                    })
                    .collect()
            })
        });
    if url.is_some()
        || matches!(
            kind.as_str(),
            "http" | "sse" | "remote" | "streamable-http" | "streamable_http"
        )
    {
        let url = url.ok_or_else(|| "remote MCP server is missing url".to_string())?;
        return Ok(CanonicalMcp {
            transport: if kind == "sse" {
                "sse".to_string()
            } else {
                "http".to_string()
            },
            command: None,
            args: None,
            env: None,
            url: Some(url),
            headers: string_map(raw.get("headers"))
                .or_else(|| string_map(raw.get("http_headers")))
                .or_else(|| string_map(raw.get("httpHeaders"))),
        });
    }
    let command = command.ok_or_else(|| "stdio MCP server is missing command".to_string())?;
    Ok(CanonicalMcp {
        transport: "stdio".to_string(),
        command: Some(command),
        args,
        env: string_map(raw.get("env")).or_else(|| string_map(raw.get("environment"))),
        url: None,
        headers: None,
    })
}

fn convert_mcp_raw_for_target(
    raw: &Map<String, Value>,
    format: &str,
) -> Result<Map<String, Value>, String> {
    let canonical = to_canonical(raw)?;
    let mut out = Map::new();
    if format == "toml-mcp_servers" {
        if canonical.transport == "sse" {
            return Err(
                "Codex config.toml does not support SSE MCP servers; refuse assignment".to_string(),
            );
        }
        if canonical.transport == "http" {
            out.insert(
                "url".to_string(),
                Value::String(
                    canonical
                        .url
                        .ok_or_else(|| "HTTP MCP server missing url".to_string())?,
                ),
            );
            if let Some(headers) = canonical.headers {
                out.insert("http_headers".to_string(), Value::Object(headers));
            }
        } else {
            out.insert(
                "command".to_string(),
                Value::String(
                    canonical
                        .command
                        .ok_or_else(|| "stdio MCP server missing command".to_string())?,
                ),
            );
            if let Some(args) = canonical.args.filter(|args| !args.is_empty()) {
                out.insert(
                    "args".to_string(),
                    serde_json::to_value(args).map_err(|error| error.to_string())?,
                );
            }
            if let Some(env) = canonical.env {
                out.insert("env".to_string(), Value::Object(env));
            }
        }
    } else if matches!(canonical.transport.as_str(), "http" | "sse") {
        out.insert("type".to_string(), Value::String(canonical.transport));
        out.insert(
            "url".to_string(),
            Value::String(
                canonical
                    .url
                    .ok_or_else(|| "remote MCP server missing url".to_string())?,
            ),
        );
        if let Some(headers) = canonical.headers {
            out.insert("headers".to_string(), Value::Object(headers));
        }
    } else {
        out.insert(
            "command".to_string(),
            Value::String(
                canonical
                    .command
                    .ok_or_else(|| "stdio MCP server missing command".to_string())?,
            ),
        );
        if let Some(args) = canonical.args.filter(|args| !args.is_empty()) {
            out.insert(
                "args".to_string(),
                serde_json::to_value(args).map_err(|error| error.to_string())?,
            );
        }
        if let Some(env) = canonical.env {
            out.insert("env".to_string(), Value::Object(env));
        }
    }
    for key in ["cwd", "enabled", "startup_timeout_sec", "tool_timeout_sec"] {
        if let Some(value) = raw.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if format == "toml-mcp_servers" {
        if let Some(value) = raw.get("env_vars") {
            out.insert("env_vars".to_string(), value.clone());
        }
    }
    Ok(out)
}

async fn read_dsh_servers(path: &Path) -> Result<Vec<(String, Map<String, Value>)>, String> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "[]\n".to_string(),
        Err(error) => return Err(error.to_string()),
    };
    let document: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for patch in document.as_sequence().into_iter().flatten() {
        for entry in patch
            .get("insert")
            .and_then(serde_yaml::Value::as_sequence)
            .into_iter()
            .flatten()
        {
            if entry.get("name").and_then(serde_yaml::Value::as_str)
                != Some("@deepseek-ai/dsh-mcp-client")
            {
                continue;
            }
            let Some(config) = entry.get("config") else {
                continue;
            };
            let raw = serde_json::to_value(config).map_err(|error| error.to_string())?;
            let Some(mut raw) = raw.as_object().cloned() else {
                continue;
            };
            let Some(name) = raw
                .remove("serverName")
                .and_then(|value| value.as_str().map(String::from))
            else {
                continue;
            };
            out.push((name, raw));
        }
    }
    Ok(out)
}

async fn write_dsh_server(
    path: &Path,
    name: &str,
    raw: Option<&Map<String, Value>>,
) -> Result<(), String> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "[]\n".to_string(),
        Err(error) => return Err(error.to_string()),
    };
    let mut document: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|error| error.to_string())?;
    let patches = document
        .as_sequence_mut()
        .ok_or_else(|| "Unsupported dsh patch document".to_string())?;
    let id = format!("buddy-mcp-{name}");
    for patch in patches.iter_mut() {
        if let Some(insert) = patch
            .get_mut("insert")
            .and_then(serde_yaml::Value::as_sequence_mut)
        {
            let mut index = insert.len();
            while index > 0 {
                index -= 1;
                let entry = &insert[index];
                if entry.get("name").and_then(serde_yaml::Value::as_str)
                    != Some("@deepseek-ai/dsh-mcp-client")
                {
                    continue;
                }
                if entry
                    .get("config")
                    .and_then(|config| config.get("serverName"))
                    .and_then(serde_yaml::Value::as_str)
                    != Some(name)
                {
                    continue;
                }
                if entry.get("id").and_then(serde_yaml::Value::as_str) != Some(id.as_str()) {
                    return Err("Existing dsh MCP entry is not managed by Buddy".to_string());
                }
                insert.remove(index);
            }
        }
    }
    if let Some(raw) = raw {
        if !Regex::new(r"^[A-Za-z0-9_-]{1,32}$").unwrap().is_match(name) {
            return Err(
                "dsh MCP names require 1–32 letters, digits, hyphens or underscores".to_string(),
            );
        }
        let canonical = to_canonical(raw)?;
        if canonical.transport == "sse" {
            return Err("dsh does not support SSE MCP".to_string());
        }
        let mut config = serde_json::json!({ "serverName": name });
        let config = config.as_object_mut().unwrap();
        if let Some(command) = canonical.command {
            config.insert("command".to_string(), Value::String(command));
        }
        if let Some(args) = canonical.args {
            config.insert("args".to_string(), serde_json::to_value(args).unwrap());
        }
        if let Some(env) = canonical.env {
            config.insert("env".to_string(), Value::Object(env));
        }
        if let Some(url) = canonical.url {
            config.insert("url".to_string(), Value::String(url));
        }
        if let Some(headers) = canonical.headers {
            config.insert("headers".to_string(), Value::Object(headers));
        }
        config.insert(
            "transport".to_string(),
            Value::String(if canonical.transport == "http" {
                "streamable-http".to_string()
            } else {
                "stdio".to_string()
            }),
        );
        if let Some(cwd) = raw.get("cwd") {
            config.insert("cwd".to_string(), cwd.clone());
        }
        let entry = serde_json::json!({ "insert": [{ "id": id, "name": "@deepseek-ai/dsh-mcp-client", "config": config }] });
        patches.push(serde_yaml::to_value(entry).map_err(|error| error.to_string())?);
    }
    let encoded = serde_yaml::to_string(&document).map_err(|error| error.to_string())?;
    atomic_write_text(path, &encoded).await
}

fn stable_config(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(stable_config)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        stable_config(&values[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn mcp_config_key(name: &str, raw: &Map<String, Value>) -> String {
    let mut config = raw.clone();
    config.remove("__buddy");
    match to_canonical(&config) {
        Ok(canonical) => {
            let aliases = [
                "type",
                "transport",
                "command",
                "args",
                "env",
                "environment",
                "url",
                "serverUrl",
                "httpUrl",
                "headers",
                "http_headers",
                "httpHeaders",
                "note",
                "startup_timeout_sec",
                "tool_timeout_sec",
                "enabled",
                "env_vars",
            ];
            let mut value = serde_json::json!({ "name": name, "transport": canonical.transport, "command": canonical.command, "args": canonical.args.filter(|args| !args.is_empty()), "env": canonical.env, "url": canonical.url, "headers": canonical.headers });
            let object = value.as_object_mut().unwrap();
            for (key, value) in config {
                if !aliases.contains(&key.as_str())
                    && !(key == "cwd" && matches!(value.as_str(), Some("." | "")))
                {
                    object.insert(key, value);
                }
            }
            stable_config(&value)
        }
        Err(_) => stable_config(&serde_json::json!({ "name": name, "config": config })),
    }
}

fn same_local_mcp_launcher(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    match (to_canonical(a), to_canonical(b)) {
        (Ok(left), Ok(right)) => {
            left.transport == "stdio"
                && right.transport == "stdio"
                && left.command == right.command
                && left.args == right.args
                && a.get("cwd").and_then(Value::as_str).unwrap_or(".")
                    == b.get("cwd").and_then(Value::as_str).unwrap_or(".")
        }
        _ => false,
    }
}

async fn read_skill_meta(dir: &Path) -> Option<(String, Option<String>)> {
    let text = tokio::fs::read_to_string(dir.join("SKILL.md")).await.ok()?;
    let name = Regex::new(r"(?m)^name:\s*(.+)$")
        .unwrap()
        .captures(&text)
        .or_else(|| Regex::new(r"(?m)^#\s+(.+)$").unwrap().captures(&text))
        .map(|captures| captures[1].trim().to_string())
        .or_else(|| {
            dir.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "skill".to_string());
    let description = Regex::new(r"(?m)^description:\s*(.+)$")
        .unwrap()
        .captures(&text)
        .map(|captures| captures[1].trim().to_string());
    Some((name, description))
}

async fn discover_skills(
    targets: &[LibraryTarget],
    library: &[LibrarySkillRecord],
) -> Vec<DiscoveredSkill> {
    let imported = library
        .iter()
        .flat_map(|skill| [Some(skill.library_path.clone()), skill.origin_path.clone()])
        .flatten()
        .collect::<HashSet<_>>();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for target in targets {
        let Some(dir) = &target.skills_dir else {
            continue;
        };
        if target.pending_init == Some(true) || !target.capabilities.read_skills {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if !Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
                .unwrap()
                .is_match(&name)
            {
                continue;
            }
            let file_type = match entry.file_type().await {
                Ok(value) => value,
                Err(_) => continue,
            };
            if !file_type.is_dir() && !file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let Ok(physical_key) = tokio::fs::canonicalize(&path).await else {
                continue;
            };
            let physical_key = path_string(&physical_key);
            if seen.contains(&physical_key)
                || physical_key.split('/').any(|part| part == ".system")
                || Regex::new(r"/plugins/(cache|marketplaces)/")
                    .unwrap()
                    .is_match(&physical_key)
            {
                continue;
            }
            let Some((skill_name, description)) = read_skill_meta(&path).await else {
                continue;
            };
            seen.insert(physical_key.clone());
            let git = tokio::fs::metadata(path.join(".git")).await.is_ok();
            let already_imported = imported.contains(&physical_key)
                || library
                    .iter()
                    .any(|skill| skill.name == skill_name && skill.managed);
            out.push(DiscoveredSkill {
                discovery_id: physical_key.clone(),
                name: skill_name,
                description,
                path: path_string(&path),
                physical_key,
                target_id: target.target_id.clone(),
                agent_id: target.agent_id.clone(),
                scope: target.scope.clone(),
                is_symlink: file_type.is_symlink(),
                is_git_root: git,
                already_imported,
                import_blocked_reason: if git {
                    Some("git-root".to_string())
                } else if file_type.is_symlink() {
                    Some("symlink".to_string())
                } else {
                    None
                },
            });
        }
    }
    out
}

async fn read_servers_from_target(
    target: &LibraryTarget,
) -> Vec<(String, Map<String, Value>, String, String)> {
    let Some(path) = &target.mcp_path else {
        return Vec::new();
    };
    if target.pending_init == Some(true) || !target.capabilities.read_mcp {
        return Vec::new();
    }
    let physical_key = target
        .mcp_physical_key
        .clone()
        .unwrap_or_else(|| path.clone());
    match target.mcp_format.as_str() {
        "dsh-patch" => read_dsh_servers(Path::new(path))
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(name, raw)| (name, raw, path.clone(), physical_key.clone()))
            .collect(),
        "jsonc-mcpServers" => {
            let Ok(text) = tokio::fs::read_to_string(path).await else {
                return Vec::new();
            };
            let Ok(value) = parse_jsonc(&text) else {
                return Vec::new();
            };
            let mut servers = value
                .get("mcpServers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(mcp) = value.get("mcp").and_then(Value::as_object) {
                servers.extend(mcp.clone());
            }
            servers
                .into_iter()
                .filter_map(|(name, raw)| {
                    raw.as_object()
                        .cloned()
                        .map(|raw| (name, raw, path.clone(), physical_key.clone()))
                })
                .collect()
        }
        "toml-mcp_servers" => {
            let Ok(text) = tokio::fs::read_to_string(path).await else {
                return Vec::new();
            };
            toml_servers(&text)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(name, raw)| {
                    raw.as_object()
                        .cloned()
                        .map(|raw| (name, raw, path.clone(), physical_key.clone()))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

async fn discover_mcp_servers(
    targets: &[LibraryTarget],
    library: &[LibraryMcpServer],
) -> Vec<DiscoveredMcpServer> {
    let mut out: Vec<DiscoveredMcpServer> = Vec::new();
    let mut grouped: HashMap<String, usize> = HashMap::new();
    for target in targets {
        for (name, raw, path, physical_key) in read_servers_from_target(target).await {
            let key = mcp_config_key(&name, &raw);
            let source = McpSource {
                target_id: target.target_id.clone(),
                agent_id: target.agent_id.clone(),
                path: path.clone(),
                enabled: Some(raw.get("enabled").and_then(Value::as_bool) != Some(false)),
            };
            if let Some(index) = grouped.get(&key).copied() {
                let sources = out[index].sources.as_mut().unwrap();
                if !sources.iter().any(|existing| {
                    existing.target_id == source.target_id && existing.path == source.path
                }) {
                    sources.push(source);
                }
                continue;
            }
            let already_imported = library
                .iter()
                .any(|server| mcp_config_key(&server.name, &server.raw) == key);
            grouped.insert(key, out.len());
            out.push(DiscoveredMcpServer {
                sources: Some(vec![source]),
                discovery_id: format!("{}:{}:{}", target.target_id, name, physical_key),
                name: name.clone(),
                transport: transport_of(&raw),
                summary: summary_of(&raw),
                target_id: target.target_id.clone(),
                agent_id: target.agent_id.clone(),
                scope: target.scope.clone(),
                path,
                physical_key,
                already_imported,
                raw,
            });
        }
    }
    out
}

fn unimported_mcp_servers(
    entries: &[DiscoveredMcpServer],
    library: &[LibraryMcpServer],
) -> Vec<DiscoveredMcpServer> {
    let imported = library
        .iter()
        .map(|server| server.name.clone())
        .collect::<HashSet<_>>();
    let mut groups: HashMap<String, DiscoveredMcpServer> = HashMap::new();
    for entry in entries {
        if imported.contains(&entry.name) {
            continue;
        }
        if let Some(existing) = groups.get_mut(&entry.name) {
            for source in entry.sources.clone().unwrap_or_default() {
                if !existing.sources.as_ref().unwrap().iter().any(|current| {
                    current.target_id == source.target_id && current.path == source.path
                }) {
                    existing.sources.as_mut().unwrap().push(source);
                }
            }
        } else {
            groups.insert(entry.name.clone(), entry.clone());
        }
    }
    groups.into_values().collect()
}

fn copy_tree<'a>(
    source: &'a Path,
    target: &'a Path,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        let metadata = tokio::fs::symlink_metadata(source)
            .await
            .map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            let link = tokio::fs::read_link(source)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            tokio::fs::symlink(link, target)
                .await
                .map_err(|error| error.to_string())?;
        } else if metadata.is_dir() {
            tokio::fs::create_dir_all(target)
                .await
                .map_err(|error| error.to_string())?;
            let mut entries = tokio::fs::read_dir(source)
                .await
                .map_err(|error| error.to_string())?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| error.to_string())?
            {
                copy_tree(&entry.path(), &target.join(entry.file_name())).await?;
            }
        } else {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            tokio::fs::copy(source, target)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })
}

async fn move_dir(source: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| error.to_string())?;
    }
    if tokio::fs::rename(source, target).await.is_ok() {
        return Ok(());
    }
    copy_tree(source, target).await?;
    tokio::fs::remove_dir_all(source)
        .await
        .map_err(|error| error.to_string())
}

async fn path_kind(path: &Path) -> &'static str {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => "symlink",
        Ok(metadata) if metadata.is_dir() => "dir",
        Ok(_) => "file",
        Err(_) => "missing",
    }
}

async fn replace_with_symlink(path: &Path, target: &Path) -> Result<(), String> {
    match path_kind(path).await {
        "dir" => {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|error| error.to_string())?;
        }
        "missing" => {}
        _ => {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| error.to_string())?;
        }
    }
    tokio::fs::symlink(target, path)
        .await
        .map_err(|error| error.to_string())
}

fn hash_tree(root: &Path) -> Result<String, String> {
    fn walk(dir: &Path, prefix: &str, hash: &mut Sha256) -> Result<(), String> {
        let mut entries = std::fs::read_dir(dir)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".DS_Store" {
                continue;
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let path = entry.path();
            let file_type = entry.file_type().map_err(|error| error.to_string())?;
            if file_type.is_dir() {
                walk(&path, &relative, hash)?;
            } else if file_type.is_symlink() {
                hash.update(format!("link:{relative}\n"));
            } else if file_type.is_file() {
                let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
                hash.update(format!("file:{relative}:{}\n", bytes.len()));
                hash.update(bytes);
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    walk(root, "", &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn upsert_skill(meta: &mut LibraryMetaFile, skill: LibrarySkillRecord) {
    meta.skills
        .retain(|current| current.id != skill.id && current.name != skill.name);
    meta.skills.push(skill);
}
fn upsert_server(meta: &mut LibraryMetaFile, server: LibraryMcpServer) {
    meta.servers.retain(|current| {
        current.id != server.id
            && !(current.name == server.name && current.source_target_id == server.source_target_id)
    });
    meta.servers.push(server);
}
fn operation(status: &str) -> LibraryOpResult {
    LibraryOpResult {
        status: status.to_string(),
        ..Default::default()
    }
}

async fn import_discovered_skill(
    store: &LibraryStore,
    discovered: &DiscoveredSkill,
    targets: &[LibraryTarget],
) -> LibraryOpResult {
    let result = async {
        let mut meta = store.read_meta().await?;
        if meta
            .skills
            .iter()
            .any(|skill| skill.name == discovered.name && skill.managed)
        {
            return Ok(LibraryOpResult {
                status: "exists".to_string(),
                name: Some(discovered.name.clone()),
                message: Some("already in library".to_string()),
                ..Default::default()
            });
        }
        if discovered.is_git_root
            || tokio::fs::metadata(Path::new(&discovered.path).join(".git"))
                .await
                .is_ok()
        {
            let id = uuid::Uuid::new_v4().to_string();
            let record = LibrarySkillRecord {
                id: id.clone(),
                name: discovered.name.clone(),
                description: discovered.description.clone(),
                library_path: discovered.path.clone(),
                source_kind: "external".to_string(),
                origin_path: Some(discovered.path.clone()),
                assigned_target_ids: vec![discovered.target_id.clone()],
                import_mode: "external-ref".to_string(),
                managed: true,
                ..Default::default()
            };
            upsert_skill(&mut meta, record);
            store.write_meta(&meta).await?;
            return Ok(LibraryOpResult {
                status: "ok".to_string(),
                id: Some(id),
                name: Some(discovered.name.clone()),
                message: Some("imported as external reference (git root not moved)".to_string()),
                ..Default::default()
            });
        }
        if discovered.is_symlink || path_kind(Path::new(&discovered.path)).await == "symlink" {
            let id = uuid::Uuid::new_v4().to_string();
            let origin = tokio::fs::read_link(&discovered.path)
                .await
                .map(|path| path_string(&path))
                .unwrap_or_else(|_| discovered.path.clone());
            let record = LibrarySkillRecord {
                id: id.clone(),
                name: discovered.name.clone(),
                description: discovered.description.clone(),
                library_path: discovered.physical_key.clone(),
                source_kind: "external".to_string(),
                origin_path: Some(origin),
                assigned_target_ids: vec![discovered.target_id.clone()],
                import_mode: "external-ref".to_string(),
                managed: true,
                ..Default::default()
            };
            upsert_skill(&mut meta, record);
            store.write_meta(&meta).await?;
            return Ok(LibraryOpResult {
                status: "ok".to_string(),
                id: Some(id),
                name: Some(discovered.name.clone()),
                message: Some("imported as external symlink reference".to_string()),
                ..Default::default()
            });
        }
        store
            .backup_tree(
                Path::new(&discovered.path),
                &format!("import-{}", discovered.name),
            )
            .await?;
        let safe = Regex::new(r"[^a-zA-Z0-9._-]+")
            .unwrap()
            .replace_all(&discovered.name, "-")
            .trim_matches('-')
            .to_string();
        let destination = store.skills_dir().join(if safe.is_empty() {
            "skill".to_string()
        } else {
            safe
        });
        if path_kind(&destination).await != "missing" {
            return Ok(LibraryOpResult {
                status: "conflict".to_string(),
                name: Some(discovered.name.clone()),
                message: Some(format!(
                    "library path exists: {}",
                    path_string(&destination)
                )),
                ..Default::default()
            });
        }
        move_dir(Path::new(&discovered.path), &destination).await?;
        if let Err(error) = replace_with_symlink(Path::new(&discovered.path), &destination).await {
            let _ = move_dir(&destination, Path::new(&discovered.path)).await;
            return Ok(LibraryOpResult {
                status: "failed".to_string(),
                name: Some(discovered.name.clone()),
                message: Some(format!("link failed: {error}")),
                ..Default::default()
            });
        }
        let mut linked = HashSet::from([discovered.target_id.clone()]);
        if let Some(source) = targets
            .iter()
            .find(|target| target.target_id == discovered.target_id)
        {
            linked.extend(source.linked_skill_target_ids.clone().unwrap_or_default());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let record = LibrarySkillRecord {
            id: id.clone(),
            name: discovered.name.clone(),
            description: discovered.description.clone(),
            library_path: path_string(&destination),
            source_kind: "migrated".to_string(),
            origin_path: Some(discovered.path.clone()),
            content_hash: Some(hash_tree(&destination)?),
            assigned_target_ids: linked.iter().cloned().collect(),
            import_mode: "migrate-link".to_string(),
            managed: true,
            ..Default::default()
        };
        let mut meta = store.read_meta().await?;
        upsert_skill(&mut meta, record);
        store.write_meta(&meta).await?;
        Ok(LibraryOpResult {
            status: "ok".to_string(),
            id: Some(id),
            name: Some(discovered.name.clone()),
            affected_target_ids: Some(linked.into_iter().collect()),
            ..Default::default()
        })
    }
    .await;
    result.unwrap_or_else(|error: String| LibraryOpResult {
        status: "failed".to_string(),
        name: Some(discovered.name.clone()),
        message: Some(error),
        ..Default::default()
    })
}

async fn assign_skill_to_target(
    store: &LibraryStore,
    skill_id: &str,
    target: &LibraryTarget,
    targets: &[LibraryTarget],
    enabled: bool,
) -> LibraryOpResult {
    let result = async {
        let Some(skills_dir) = &target.skills_dir else {
            return Ok(operation("rejected"));
        };
        if target.pending_init == Some(true) {
            return Ok(operation("rejected"));
        }
        let mut meta = store.read_meta().await?;
        let Some(mut skill) = meta
            .skills
            .iter()
            .find(|skill| skill.id == skill_id)
            .cloned()
        else {
            return Ok(operation("failed"));
        };
        let affected = HashSet::from([target.target_id.clone()])
            .into_iter()
            .chain(target.linked_skill_target_ids.clone().unwrap_or_default())
            .collect::<HashSet<_>>();
        let link_name = Path::new(skills_dir).join(
            Path::new(&skill.library_path)
                .file_name()
                .unwrap_or_default(),
        );
        if enabled {
            if !matches!(skill.import_mode.as_str(), "external-ref" | "shared-ref") {
                tokio::fs::create_dir_all(skills_dir)
                    .await
                    .map_err(|error| error.to_string())?;
                match path_kind(&link_name).await {
                    "missing" | "symlink" => {
                        replace_with_symlink(&link_name, Path::new(&skill.library_path)).await?
                    }
                    _ => {
                        return Ok(LibraryOpResult {
                            status: "conflict".to_string(),
                            name: Some(skill.name),
                            message: Some(format!(
                                "blocked by existing path {}",
                                path_string(&link_name)
                            )),
                            ..Default::default()
                        })
                    }
                }
            }
            skill.assigned_target_ids = skill
                .assigned_target_ids
                .into_iter()
                .chain(affected.iter().cloned())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
        } else {
            for id in &affected {
                let dir = targets
                    .iter()
                    .find(|candidate| &candidate.target_id == id)
                    .and_then(|candidate| candidate.skills_dir.as_ref())
                    .unwrap_or(skills_dir);
                let path = Path::new(dir).join(
                    Path::new(&skill.library_path)
                        .file_name()
                        .unwrap_or_default(),
                );
                if let Ok(link) = tokio::fs::read_link(&path).await {
                    let link = path_string(&link);
                    if link == skill.library_path
                        || link.ends_with(
                            Path::new(&skill.library_path)
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .as_ref(),
                        )
                    {
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                }
            }
            skill
                .assigned_target_ids
                .retain(|id| !affected.contains(id));
        }
        upsert_skill(&mut meta, skill.clone());
        store.write_meta(&meta).await?;
        Ok(LibraryOpResult {
            status: "ok".to_string(),
            id: Some(skill.id),
            name: Some(skill.name),
            affected_target_ids: Some(affected.into_iter().collect()),
            ..Default::default()
        })
    }
    .await;
    result.unwrap_or_else(|error: String| LibraryOpResult {
        status: "failed".to_string(),
        message: Some(error),
        ..Default::default()
    })
}

async fn eject_skill(store: &LibraryStore, skill_id: &str) -> LibraryOpResult {
    let result = async {
        let mut meta = store.read_meta().await?;
        let Some(skill) = meta
            .skills
            .iter()
            .find(|skill| skill.id == skill_id)
            .cloned()
        else {
            return Ok(operation("failed"));
        };
        if skill.import_mode != "migrate-link" {
            meta.skills.retain(|current| current.id != skill_id);
            store.write_meta(&meta).await?;
            return Ok(LibraryOpResult {
                status: "ok".to_string(),
                id: Some(skill_id.to_string()),
                name: Some(skill.name),
                message: Some("unmanaged reference removed from library metadata".to_string()),
                ..Default::default()
            });
        }
        let origin = skill
            .origin_path
            .clone()
            .ok_or_else(|| "missing origin path".to_string())?;
        store
            .backup_tree(
                Path::new(&skill.library_path),
                &format!("eject-{}", skill.name),
            )
            .await?;
        match path_kind(Path::new(&origin)).await {
            "symlink" => {
                tokio::fs::remove_file(&origin)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            "missing" => {}
            _ => {
                return Ok(LibraryOpResult {
                    status: "conflict".to_string(),
                    name: Some(skill.name),
                    message: Some(format!("origin is not a managed symlink: {origin}")),
                    ..Default::default()
                })
            }
        }
        move_dir(Path::new(&skill.library_path), Path::new(&origin)).await?;
        meta.skills.retain(|current| current.id != skill_id);
        store.write_meta(&meta).await?;
        Ok(LibraryOpResult {
            status: "ok".to_string(),
            id: Some(skill_id.to_string()),
            name: Some(skill.name),
            message: Some("ejected to origin".to_string()),
            ..Default::default()
        })
    }
    .await;
    result.unwrap_or_else(|error: String| LibraryOpResult {
        status: "failed".to_string(),
        message: Some(error),
        ..Default::default()
    })
}

fn server_record(
    name: &str,
    raw: &Map<String, Value>,
    source_target_id: Option<String>,
    assigned_target_ids: Vec<String>,
) -> LibraryMcpServer {
    let mut stored = raw.clone();
    stored.remove("__buddy");
    LibraryMcpServer {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        transport: transport_of(raw),
        command: raw.get("command").and_then(Value::as_str).map(String::from),
        args: raw.get("args").and_then(Value::as_array).map(|args| {
            args.iter()
                .map(|arg| {
                    arg.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| arg.to_string())
                })
                .collect()
        }),
        url: raw.get("url").and_then(Value::as_str).map(String::from),
        note: raw.get("note").and_then(Value::as_str).map(String::from),
        summary: summary_of(raw),
        raw: stored,
        source_target_id,
        scope: "user".to_string(),
        assigned_target_ids,
        managed: true,
    }
}

async fn add_server(store: &LibraryStore, name: &str, raw: &Map<String, Value>) -> LibraryOpResult {
    let result = async {
        if !Regex::new(r"^[A-Za-z0-9_-]{1,64}$").unwrap().is_match(name) {
            return Ok(LibraryOpResult {
                status: "rejected".to_string(),
                message: Some(
                    "Use 1–64 letters, digits, hyphens or underscores for the name.".to_string(),
                ),
                ..Default::default()
            });
        }
        let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
        let mut config = Map::new();
        if kind == "stdio" {
            let (command, args) =
                if let Some(command_line) = raw.get("commandLine").and_then(Value::as_str) {
                    let tokens = split_command(command_line.trim());
                    (
                        tokens.first().cloned(),
                        tokens.into_iter().skip(1).collect::<Vec<_>>(),
                    )
                } else {
                    (
                        raw.get("command").and_then(Value::as_str).map(String::from),
                        raw.get("args")
                            .and_then(Value::as_array)
                            .map(|args| {
                                args.iter()
                                    .filter_map(|arg| arg.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                };
            let Some(command) = command.filter(|command| !command.trim().is_empty()) else {
                return Ok(LibraryOpResult {
                    status: "rejected".to_string(),
                    message: Some("Enter a command and string arguments.".to_string()),
                    ..Default::default()
                });
            };
            config.insert("type".to_string(), Value::String(kind.to_string()));
            config.insert("command".to_string(), Value::String(command));
            config.insert("args".to_string(), serde_json::to_value(args).unwrap());
            if let Some(env) = raw.get("env") {
                config.insert("env".to_string(), env.clone());
            }
        } else if matches!(kind, "http" | "sse") {
            let Some(url) = raw.get("url").and_then(Value::as_str) else {
                return Ok(operation("rejected"));
            };
            let parsed =
                reqwest::Url::parse(url).map_err(|_| "Enter an HTTP(S) URL".to_string())?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Ok(operation("rejected"));
            }
            config.insert("type".to_string(), Value::String(kind.to_string()));
            config.insert("url".to_string(), Value::String(parsed.to_string()));
            if let Some(headers) = raw.get("headers") {
                config.insert("headers".to_string(), headers.clone());
            }
        } else {
            return Ok(operation("rejected"));
        }
        if let Some(note) = raw
            .get("note")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|note| !note.is_empty())
        {
            config.insert(
                "note".to_string(),
                Value::String(note.chars().take(500).collect()),
            );
        }
        if let Some(timeout) = raw.get("startup_timeout_sec") {
            config.insert("startup_timeout_sec".to_string(), timeout.clone());
        }
        let mut meta = store.read_meta().await?;
        if meta.servers.iter().any(|server| server.name == name) {
            return Ok(operation("exists"));
        }
        let record = server_record(name, &config, None, Vec::new());
        let id = record.id.clone();
        upsert_server(&mut meta, record);
        store.write_meta(&meta).await?;
        Ok(LibraryOpResult {
            status: "ok".to_string(),
            id: Some(id),
            name: Some(name.to_string()),
            ..Default::default()
        })
    }
    .await;
    result.unwrap_or_else(|error: String| LibraryOpResult {
        status: "failed".to_string(),
        message: Some(error),
        ..Default::default()
    })
}

async fn write_server_to_target(
    target: &LibraryTarget,
    name: &str,
    raw: &Map<String, Value>,
) -> Result<(), String> {
    let path = target
        .mcp_path
        .as_ref()
        .ok_or_else(|| "target has no MCP path".to_string())?;
    match target.mcp_format.as_str() {
        "dsh-patch" => write_dsh_server(Path::new(path), name, Some(raw)).await,
        "jsonc-mcpServers" => {
            write_mcp_server_jsonc(
                Path::new(path),
                name,
                &convert_mcp_raw_for_target(raw, &target.mcp_format)?,
            )
            .await
        }
        "toml-mcp_servers" => {
            set_mcp_server_toml(
                Path::new(path),
                name,
                &convert_mcp_raw_for_target(raw, &target.mcp_format)?,
            )
            .await
        }
        _ => Err(target
            .capabilities
            .reason
            .clone()
            .unwrap_or_else(|| "unsupported MCP format".to_string())),
    }
}

async fn remove_server_from_target(target: &LibraryTarget, name: &str) -> Result<(), String> {
    let Some(path) = &target.mcp_path else {
        return Ok(());
    };
    match target.mcp_format.as_str() {
        "dsh-patch" => write_dsh_server(Path::new(path), name, None).await,
        "jsonc-mcpServers" => remove_mcp_server_jsonc(Path::new(path), name).await,
        "toml-mcp_servers" => delete_mcp_server_toml(Path::new(path), name).await,
        _ => Ok(()),
    }
}

async fn ensure_codex_acp_home(
    data_root: &Path,
    actor: &str,
) -> Result<(LibraryTarget, Vec<String>), String> {
    let source = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".codex"));
    let acp_home = buddy_acp_home(data_root, actor);
    tokio::fs::create_dir_all(&acp_home)
        .await
        .map_err(|error| error.to_string())?;
    let mut notes = Vec::new();
    for name in ["config.toml", "AGENTS.md", "AGENTS.override.md"] {
        let destination = acp_home.join(name);
        if tokio::fs::symlink_metadata(&destination).await.is_ok() {
            continue;
        }
        if tokio::fs::copy(source.join(name), &destination)
            .await
            .is_ok()
        {
            notes.push(format!("copied {name}"));
        } else if name == "config.toml" {
            tokio::fs::write(
                &destination,
                "# Created by Buddy library ensureCodexAcpHome\n",
            )
            .await
            .map_err(|error| error.to_string())?;
            notes.push("created empty config.toml".to_string());
        }
    }
    for name in ["skills", "rules"] {
        let destination = acp_home.join(name);
        if tokio::fs::symlink_metadata(&destination).await.is_ok() {
            continue;
        }
        match tokio::fs::symlink(source.join(name), &destination).await {
            Ok(()) => notes.push(format!("linked {name} → native")),
            Err(error) => notes.push(format!("could not link {name}: {error}")),
        }
    }
    notes.push("session import skipped; SQLite untouched".to_string());
    let target = LibraryTarget {
        target_id: format!(
            "{}:{}:user",
            actor,
            if actor.starts_with("wecode") {
                "wecode"
            } else {
                "buddy-acp"
            }
        ),
        agent_id: actor.to_string(),
        display_name: format!(
            "{} ACP",
            if actor == "codex" {
                "Codex"
            } else {
                "WeCode Codex"
            }
        ),
        config_context: (if actor.starts_with("wecode") {
            "wecode"
        } else {
            "buddy-acp"
        })
        .to_string(),
        scope: "user".to_string(),
        skills_dir: Some(path_string(&acp_home.join("skills"))),
        mcp_path: Some(path_string(&acp_home.join("config.toml"))),
        mcp_format: "toml-mcp_servers".to_string(),
        capabilities: capabilities(true),
        ..Default::default()
    };
    Ok((target, notes))
}

async fn assign_server_to_target(
    store: &LibraryStore,
    data_root: &Path,
    server_id: &str,
    target: &LibraryTarget,
    enabled: bool,
) -> LibraryOpResult {
    let result = async {
        if !target.capabilities.write_mcp && target.pending_init != Some(true) {
            return Ok(operation("rejected"));
        }
        let resolved = if target.pending_init == Some(true)
            && matches!(target.config_context.as_str(), "buddy-acp" | "wecode")
        {
            ensure_codex_acp_home(data_root, &target.agent_id).await?.0
        } else {
            target.clone()
        };
        let mut meta = store.read_meta().await?;
        let Some(mut server) = meta
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .cloned()
        else {
            return Ok(operation("failed"));
        };
        let mut applied_raw = server.raw.clone();
        if enabled {
            let existing = discover_mcp_servers(std::slice::from_ref(&resolved), &[]).await;
            let current = existing.iter().find(|entry| entry.name == server.name);
            if current.is_some_and(|current| {
                mcp_config_key(&current.name, &current.raw)
                    != mcp_config_key(&server.name, &server.raw)
                    && !same_local_mcp_launcher(&current.raw, &server.raw)
            }) {
                return Ok(LibraryOpResult {
                    status: "conflict".to_string(),
                    message: Some(
                        "This Agent already has a server with the same name.".to_string(),
                    ),
                    ..Default::default()
                });
            }
            applied_raw = current
                .map(|current| current.raw.clone())
                .unwrap_or_else(|| server.raw.clone());
            applied_raw.insert("enabled".to_string(), Value::Bool(true));
            write_server_to_target(&resolved, &server.name, &applied_raw).await?;
            server.assigned_target_ids = server
                .assigned_target_ids
                .into_iter()
                .chain([resolved.target_id.clone()])
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
        } else {
            remove_server_from_target(&resolved, &server.name).await?;
            server
                .assigned_target_ids
                .retain(|id| id != &resolved.target_id);
        }
        meta.applied.insert(
            format!("{}:mcp:{}", resolved.target_id, server.name),
            AppliedBinding {
                kind: "mcp".to_string(),
                fingerprint: serde_json::to_string(&applied_raw).unwrap(),
                path: resolved.mcp_path.clone().unwrap_or_default(),
                at: now(),
            },
        );
        upsert_server(&mut meta, server.clone());
        store.write_meta(&meta).await?;
        Ok(LibraryOpResult {
            status: "ok".to_string(),
            id: Some(server.id),
            name: Some(server.name),
            affected_target_ids: Some(vec![resolved.target_id]),
            ..Default::default()
        })
    }
    .await;
    result.unwrap_or_else(|error: String| LibraryOpResult {
        status: "failed".to_string(),
        message: Some(error),
        ..Default::default()
    })
}

impl LibraryService {
    pub fn new(data_root: &Path, agents: Arc<AgentManagementService>) -> Self {
        Self {
            store: LibraryStore::new(data_root),
            agents,
            cache: RwLock::new(None),
            operation: Mutex::new(()),
        }
    }

    pub async fn get_snapshot(&self, force: bool) -> LibrarySnapshot {
        if !force {
            if let Some(snapshot) = self.cache.read().await.clone() {
                return snapshot;
            }
        }
        self.refresh().await
    }

    pub async fn refresh(&self) -> LibrarySnapshot {
        let _operation = self.operation.lock().await;
        self.refresh_inner().await
    }

    async fn refresh_inner(&self) -> LibrarySnapshot {
        let result = async {
            self.store.ensure_layout().await?;
            let inventory = self.agents.get_inventory().await;
            let meta = self.store.read_meta().await?;
            let targets = resolve_library_targets(&inventory.agents, &self.store.data_root).await;
            let (discovered_skills, discovered_servers) = tokio::join!(
                discover_skills(&targets, &meta.skills),
                discover_mcp_servers(&targets, &meta.servers)
            );
            let mut servers = meta.servers.clone();
            for server in &mut servers {
                let assigned = discovered_servers
                    .iter()
                    .filter(|entry| entry.name == server.name)
                    .flat_map(|entry| {
                        entry.sources.clone().unwrap_or_else(|| {
                            vec![McpSource {
                                target_id: entry.target_id.clone(),
                                agent_id: entry.agent_id.clone(),
                                path: entry.path.clone(),
                                enabled: None,
                            }]
                        })
                    })
                    .filter(|source| {
                        if source.enabled == Some(false) {
                            return false;
                        }
                        if let Some(entry) = discovered_servers.iter().find(|entry| {
                            entry
                                .sources
                                .as_ref()
                                .is_some_and(|sources| sources.contains(source))
                        }) {
                            if mcp_config_key(&entry.name, &entry.raw)
                                == mcp_config_key(&server.name, &server.raw)
                            {
                                return true;
                            }
                        }
                        meta.applied
                            .get(&format!("{}:mcp:{}", source.target_id, server.name))
                            .is_some_and(|binding| {
                                binding.path == source.path
                                    && serde_json::from_str::<Map<String, Value>>(
                                        &binding.fingerprint,
                                    )
                                    .is_ok_and(|raw| {
                                        mcp_config_key(&server.name, &raw)
                                            == mcp_config_key(&server.name, &server.raw)
                                    })
                            })
                    })
                    .map(|source| source.target_id)
                    .collect::<HashSet<_>>();
                server.assigned_target_ids = assigned.into_iter().collect();
            }
            let mut auth = Vec::new();
            for server in &servers {
                for target_id in &server.assigned_target_ids {
                    if let Some(target) = targets.iter().find(|target| {
                        &target.target_id == target_id && target.capabilities.auth_probe
                    }) {
                        auth.push(probe_auth(target, &server.name).await);
                    }
                }
            }
            Ok::<LibrarySnapshot, String>(LibrarySnapshot {
                schema_version: 1,
                scanned_at: Some(now()),
                refreshing: false,
                targets,
                skills: meta.skills.clone(),
                discovered_skills: discovered_skills
                    .into_iter()
                    .filter(|skill| !skill.already_imported)
                    .collect(),
                servers,
                discovered_servers: unimported_mcp_servers(&discovered_servers, &meta.servers),
                auth,
                error: None,
            })
        }
        .await;
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => LibrarySnapshot {
                schema_version: 1,
                refreshing: false,
                error: Some(error),
                ..self.cache.read().await.clone().unwrap_or_default()
            },
        };
        *self.cache.write().await = Some(snapshot.clone());
        snapshot
    }

    pub async fn import_skills(&self, discovery_ids: Vec<String>) -> LibraryBatchResult {
        let _operation = self.operation.lock().await;
        let snapshot = self.refresh_inner().await;
        let all = discover_skills(&snapshot.targets, &snapshot.skills).await;
        let mut results = Vec::new();
        for id in discovery_ids {
            if let Some(discovered) = all.iter().find(|skill| skill.discovery_id == id) {
                results.push(
                    import_discovered_skill(&self.store, discovered, &snapshot.targets).await,
                );
            } else {
                results.push(LibraryOpResult {
                    status: "failed".to_string(),
                    message: Some(format!("not found: {id}")),
                    ..Default::default()
                });
            }
        }
        self.refresh_inner().await;
        LibraryBatchResult { results }
    }

    pub async fn set_skill_assignment(
        &self,
        skill_id: &str,
        target_id: &str,
        enabled: bool,
    ) -> LibraryOpResult {
        let _operation = self.operation.lock().await;
        let snapshot = self.refresh_inner().await;
        let Some(target) = snapshot
            .targets
            .iter()
            .find(|target| target.target_id == target_id)
        else {
            return operation("failed");
        };
        let result =
            assign_skill_to_target(&self.store, skill_id, target, &snapshot.targets, enabled).await;
        self.refresh_inner().await;
        result
    }

    pub async fn eject_skill(&self, skill_id: &str) -> LibraryOpResult {
        let _operation = self.operation.lock().await;
        let result = eject_skill(&self.store, skill_id).await;
        self.refresh_inner().await;
        result
    }

    pub async fn add_server(&self, name: &str, raw: Map<String, Value>) -> LibraryOpResult {
        let _operation = self.operation.lock().await;
        let result = add_server(&self.store, name, &raw).await;
        if result.status == "ok" {
            self.refresh_inner().await;
        }
        result
    }

    pub async fn eject_server(&self, server_id: &str) -> LibraryOpResult {
        let _operation = self.operation.lock().await;
        let result = async {
            let mut meta = self.store.read_meta().await?;
            let Some(server) = meta
                .servers
                .iter()
                .find(|server| server.id == server_id)
                .cloned()
            else {
                return Ok(operation("failed"));
            };
            meta.servers.retain(|entry| entry.id != server_id);
            meta.applied
                .retain(|key, _| !key.ends_with(&format!(":mcp:{}", server.name)));
            self.store.write_meta(&meta).await?;
            Ok::<LibraryOpResult, String>(LibraryOpResult {
                status: "ok".to_string(),
                id: Some(server_id.to_string()),
                name: Some(server.name),
                ..Default::default()
            })
        }
        .await
        .unwrap_or_else(|error| LibraryOpResult {
            status: "failed".to_string(),
            message: Some(error),
            ..Default::default()
        });
        self.refresh_inner().await;
        result
    }

    pub async fn import_servers(&self, discovery_ids: Vec<String>) -> LibraryBatchResult {
        let _operation = self.operation.lock().await;
        let snapshot = self.refresh_inner().await;
        let all = discover_mcp_servers(&snapshot.targets, &snapshot.servers).await;
        let mut results = Vec::new();
        for id in discovery_ids {
            let Some(discovered) = all.iter().find(|server| server.discovery_id == id) else {
                results.push(operation("failed"));
                continue;
            };
            let result = async {
                let mut meta = self.store.read_meta().await?;
                if meta
                    .servers
                    .iter()
                    .any(|server| server.name == discovered.name)
                {
                    return Ok(operation("exists"));
                }
                let record = server_record(
                    &discovered.name,
                    &discovered.raw,
                    Some(discovered.target_id.clone()),
                    discovered
                        .sources
                        .clone()
                        .unwrap_or_else(|| {
                            vec![McpSource {
                                target_id: discovered.target_id.clone(),
                                agent_id: discovered.agent_id.clone(),
                                path: discovered.path.clone(),
                                enabled: None,
                            }]
                        })
                        .into_iter()
                        .map(|source| source.target_id)
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect(),
                );
                let id = record.id.clone();
                upsert_server(&mut meta, record);
                self.store.write_meta(&meta).await?;
                Ok::<LibraryOpResult, String>(LibraryOpResult {
                    status: "ok".to_string(),
                    id: Some(id),
                    name: Some(discovered.name.clone()),
                    ..Default::default()
                })
            }
            .await
            .unwrap_or_else(|error| LibraryOpResult {
                status: "failed".to_string(),
                message: Some(error),
                ..Default::default()
            });
            results.push(result);
        }
        self.refresh_inner().await;
        LibraryBatchResult { results }
    }

    pub async fn set_server_assignment(
        &self,
        server_id: &str,
        target_id: &str,
        enabled: bool,
    ) -> LibraryOpResult {
        let _operation = self.operation.lock().await;
        let snapshot = self.refresh_inner().await;
        let Some(target) = snapshot
            .targets
            .iter()
            .find(|target| target.target_id == target_id)
        else {
            return operation("failed");
        };
        let result = assign_server_to_target(
            &self.store,
            &self.store.data_root,
            server_id,
            target,
            enabled,
        )
        .await;
        self.refresh_inner().await;
        result
    }

    pub async fn open_auth_guidance(
        &self,
        target_id: &str,
        server_name: &str,
    ) -> AuthGuidanceResult {
        let snapshot = self.get_snapshot(false).await;
        let Some(target) = snapshot
            .targets
            .iter()
            .find(|target| target.target_id == target_id)
        else {
            return AuthGuidanceResult {
                title: "未知目标".to_string(),
                steps: vec!["目标不存在".to_string()],
            };
        };
        let guide = auth_guidance(target, server_name);
        open_terminal_guidance(
            target
                .mcp_path
                .as_ref()
                .and_then(|path| Path::new(path).parent()),
            &format!("{}\n{}", guide.title, guide.steps.join("\n")),
        );
        guide
    }

    pub async fn open_library_dir(&self) -> Result<String, String> {
        self.store.ensure_layout().await?;
        Ok(path_string(&self.store.root()))
    }

    pub async fn open_backups_dir(&self) -> Result<String, String> {
        self.store.ensure_layout().await?;
        Ok(path_string(&self.store.backups_dir()))
    }

    pub async fn ensure_acp_home(&self, agent_id: &str) -> Result<EnsureAcpHomeResult, String> {
        let _operation = self.operation.lock().await;
        let (_, notes) = ensure_codex_acp_home(&self.store.data_root, agent_id).await?;
        self.refresh_inner().await;
        Ok(EnsureAcpHomeResult {
            home: path_string(&buddy_acp_home(&self.store.data_root, agent_id)),
            notes,
        })
    }
}

async fn probe_auth(target: &LibraryTarget, server_name: &str) -> TargetAuthStatus {
    if target.agent_id != "claude" || target.config_context != "native" {
        return TargetAuthStatus {
            target_id: target.target_id.clone(),
            server_name: server_name.to_string(),
            status: "unknown".to_string(),
            detail: Some("请在 Agent 中确认（无可靠静态探测接口）".to_string()),
        };
    }
    let path = home().join(".claude/.credentials.json");
    match tokio::fs::read_to_string(path).await {
        Ok(text) => {
            let status = parse_jsonc(&text)
                .ok()
                .and_then(|value| value.get("mcpOAuth").and_then(Value::as_object).cloned())
                .map(|oauth| {
                    if oauth.keys().any(|key| {
                        key == server_name || key.starts_with(&format!("{server_name}|"))
                    }) {
                        "signed-in"
                    } else {
                        "not-found"
                    }
                })
                .unwrap_or("not-found");
            TargetAuthStatus {
                target_id: target.target_id.clone(),
                server_name: server_name.to_string(),
                status: status.to_string(),
                detail: Some(if status == "signed-in" {
                    "mcpOAuth entry present (token not read into Buddy)".to_string()
                } else {
                    "no matching mcpOAuth entry".to_string()
                }),
            }
        }
        Err(_) => TargetAuthStatus {
            target_id: target.target_id.clone(),
            server_name: server_name.to_string(),
            status: "not-found".to_string(),
            detail: Some("credentials file missing".to_string()),
        },
    }
}

fn auth_guidance(target: &LibraryTarget, server_name: &str) -> AuthGuidanceResult {
    if target.agent_id == "claude" {
        return AuthGuidanceResult {
            title: "在 Claude Code 中完成 MCP 登录".to_string(),
            steps: vec![
                "打开终端并进入 Claude Code".to_string(),
                format!("运行 /mcp 或在设置中找到服务器「{server_name}」"),
                "按原生提示完成登录；Buddy 不会代跑登录进程".to_string(),
            ],
        };
    }
    if matches!(target.agent_id.as_str(), "codex" | "wecode_codex") {
        return AuthGuidanceResult {
            title: "在 Codex 中确认 MCP 认证".to_string(),
            steps: vec![
                format!(
                    "确认写入目标：{}（{}）",
                    target.display_name, target.config_context
                ),
                format!(
                    "配置文件：{}",
                    target
                        .mcp_path
                        .clone()
                        .unwrap_or_else(|| "（尚未初始化）".to_string())
                ),
                "在 Codex 原生界面或 CLI 中按该服务器要求登录；Buddy 只提供指引".to_string(),
            ],
        };
    }
    AuthGuidanceResult {
        title: format!("在 {} 中确认 MCP 认证", target.display_name),
        steps: vec![
            format!("服务器：{server_name}"),
            "打开对应 Agent，按其原生 MCP / 设置流程操作".to_string(),
            "Buddy 不托管登录、不显示虚假进度".to_string(),
        ],
    }
}

fn open_terminal_guidance(cwd: Option<&Path>, hint: &str) {
    let dir = cwd.map(path_string).unwrap_or_else(|| path_string(&home()));
    let script = format!(
        "cd {}; clear; echo {}; exec \"$SHELL\" -l",
        serde_json::to_string(&dir).unwrap(),
        serde_json::to_string(hint).unwrap()
    );
    let applescript = format!(
        "tell application \"Terminal\" to do script {}",
        serde_json::to_string(&script).unwrap()
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &applescript])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonc_local_edit_preserves_comments_and_other_servers() {
        let source = "{\n  // keep this comment\n  \"theme\": \"dark\",\n  \"mcpServers\": {\n    \"existing\": { \"command\": \"old\" }\n  }\n}\n";
        let server = serde_json::json!({ "command": "new", "args": ["--stdio"] })
            .as_object()
            .unwrap()
            .clone();
        let updated = set_mcp_server_in_jsonc(source, "docs", &server).unwrap();
        assert!(updated.contains("// keep this comment"));
        assert!(updated.contains("\"theme\": \"dark\""));
        assert!(updated.contains("\"existing\": { \"command\": \"old\" }"));
        assert!(updated.contains("\"docs\""));
        let removed = delete_mcp_server_in_jsonc(&updated, "docs").unwrap();
        assert!(!removed.contains("\"docs\""));
        assert!(removed.contains("// keep this comment"));
    }

    #[test]
    fn toml_local_edit_preserves_unrelated_text_and_complex_values() {
        let source =
            "# keep\n[ui]\ntheme = \"dark\"\n\n[mcp_servers.existing]\ncommand = \"old\"\n";
        let server =
            serde_json::json!({ "command": "new", "args": ["a", "b"], "env": {"TOKEN": "x"} })
                .as_object()
                .unwrap()
                .clone();
        let mut lines = source.lines().map(String::from).collect::<Vec<_>>();
        let table_name = "mcp_servers.docs";
        let spans = table_spans(&lines);
        assert!(spans.iter().all(|span| span.0 != table_name));
        lines.push("[mcp_servers.docs]".to_string());
        for (key, value) in &server {
            lines.push(format!(
                "{} = {}",
                quote_toml_key(key),
                format_toml_value(value)
            ));
        }
        let parsed = toml_servers(&lines.join("\n")).unwrap();
        assert_eq!(parsed["docs"]["args"], serde_json::json!(["a", "b"]));
        assert_eq!(parsed["docs"]["env"]["TOKEN"], "x");
        assert!(source.contains("# keep"));
    }

    #[test]
    fn canonical_conversion_preserves_launch_context() {
        let raw = serde_json::json!({ "type": "stdio", "command": "server", "args": ["a"], "cwd": "/tmp/repo", "enabled": true }).as_object().unwrap().clone();
        let converted = convert_mcp_raw_for_target(&raw, "toml-mcp_servers").unwrap();
        assert_eq!(converted["command"], "server");
        assert_eq!(converted["cwd"], "/tmp/repo");
        assert_eq!(converted["enabled"], true);
    }

    #[tokio::test]
    async fn dsh_mcp_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cordis.patch.yml");
        let raw = serde_json::json!({ "type": "stdio", "command": "server", "args": ["run"] })
            .as_object()
            .unwrap()
            .clone();
        write_dsh_server(&path, "docs", Some(&raw)).await.unwrap();
        let entries = read_dsh_servers(&path).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "docs");
        assert_eq!(entries[0].1["command"], "server");
        write_dsh_server(&path, "docs", None).await.unwrap();
        assert!(read_dsh_servers(&path).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn skill_import_and_eject_restores_origin() {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("agent/skills/demo");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::write(origin.join("SKILL.md"), "name: demo\ndescription: test\n").unwrap();
        let store = LibraryStore::new(temp.path());
        store.ensure_layout().await.unwrap();
        let discovered = DiscoveredSkill {
            discovery_id: path_string(&origin),
            name: "demo".to_string(),
            path: path_string(&origin),
            physical_key: path_string(&origin),
            target_id: "claude:native:user".to_string(),
            ..Default::default()
        };
        let result = import_discovered_skill(&store, &discovered, &[]).await;
        assert_eq!(result.status, "ok");
        assert_eq!(path_kind(&origin).await, "symlink");
        let meta = store.read_meta().await.unwrap();
        let result = eject_skill(&store, &meta.skills[0].id).await;
        assert_eq!(result.status, "ok");
        assert_eq!(path_kind(&origin).await, "dir");
        assert!(origin.join("SKILL.md").exists());
    }
}
