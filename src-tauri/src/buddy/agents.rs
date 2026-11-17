use crate::buddy::acp::{
    default_acp_args, prepare_acp_environment, AcpClient, AcpClientOptions, AcpNewSessionParams,
    AcpStdioTransport, AcpTransport, StdioTransportOptions,
};
use crate::buddy::defaults::{default_launcher_for, DEFAULT_LAUNCHER_TIMEOUT_SECONDS};
use crate::buddy::launchers::{command_kind_for_tokens, split_command, LauncherCommandKind};
use crate::buddy::redact::redact_sensitive_text;
use crate::buddy::shell_path::merge_child_env;
use crate::buddy::store::{atomic_write_json, BuddyStore};
use crate::buddy::types::{AcpModelInfo, AcpModelList, GlobalSettings, Launcher};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex, RwLock};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfiguration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli: Option<Launcher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<Launcher>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInstallation {
    pub id: String,
    pub path: String,
    pub real_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shim: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAgentUpdate {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub version_args: Vec<String>,
    pub version: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalAgent {
    pub id: String,
    pub installation_id: String,
    pub name: String,
    pub path: String,
    pub real_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_protocol: Option<String>,
    pub runnable: bool,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli: Option<Launcher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp: Option<Launcher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installations: Option<Vec<AgentInstallation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_cli: Option<Launcher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_acp: Option<Launcher>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_update: Option<NativeAgentUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_failure_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_launch_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_acp_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInventorySnapshot {
    pub revision: u64,
    pub agents: Vec<LocalAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<String>,
    pub refreshing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateJob {
    pub id: String,
    pub agent_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentUpdateProcess {
    pub pid: u32,
    pub started: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentUpdateItem {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<AgentUpdateProcess>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentUpdateBatch {
    pub id: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub status: String,
    pub cancel_requested: bool,
    pub items: Vec<AgentUpdateItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUpdateSnapshot {
    pub schema_version: u32,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_batch: Option<AgentUpdateBatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_completed_batch: Option<AgentUpdateBatch>,
}

impl Default for AgentUpdateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: 2,
            revision: 0,
            active_batch: None,
            last_completed_batch: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct AgentRecipe {
    id: &'static str,
    name: &'static str,
    command: &'static str,
    aliases: Vec<&'static str>,
    npm: Option<&'static str>,
    brew: Option<&'static str>,
    app: Option<&'static str>,
    app_cli: Vec<&'static str>,
    acp: Vec<&'static str>,
    adapter: Option<&'static str>,
    model_flag: Option<&'static str>,
}

fn recipe(id: &'static str, name: &'static str, command: &'static str) -> AgentRecipe {
    AgentRecipe {
        id,
        name,
        command,
        model_flag: Some("--model"),
        ..Default::default()
    }
}

fn agent_recipes() -> Vec<AgentRecipe> {
    vec![
        AgentRecipe {
            npm: Some("@anthropic-ai/claude-code"),
            brew: Some("claude-code"),
            adapter: Some("claude-agent-acp"),
            ..recipe("claude", "Claude Code", "claude")
        },
        AgentRecipe {
            npm: Some("@openai/codex"),
            brew: Some("codex"),
            app: Some("Codex"),
            app_cli: vec!["Contents/Resources/codex", "Contents/MacOS/codex"],
            adapter: Some("codex-acp"),
            ..recipe("codex", "Codex", "codex")
        },
        AgentRecipe {
            aliases: vec!["agent"],
            acp: vec!["acp"],
            ..recipe("cursor", "Cursor CLI", "cursor-agent")
        },
        AgentRecipe {
            aliases: vec!["antigravity"],
            ..recipe("agy", "Antigravity CLI", "agy")
        },
        AgentRecipe {
            npm: Some("opencode-ai"),
            brew: Some("opencode"),
            acp: vec!["acp"],
            ..recipe("opencode", "OpenCode", "opencode")
        },
        recipe("kimi", "Kimi Code", "kimi"),
        AgentRecipe {
            npm: Some("@deepseek-ai/dsh"),
            app: Some("DeepSeek Harness"),
            app_cli: vec!["Contents/Resources/dsh", "Contents/MacOS/dsh"],
            acp: vec!["acp"],
            ..recipe("dsh", "DeepSeek Harness", "dsh")
        },
        AgentRecipe {
            adapter: Some("claude-agent-acp"),
            ..recipe("wecode_claude", "WeCode Claude", "wecode")
        },
        AgentRecipe {
            adapter: Some("codex-acp"),
            ..recipe("wecode_codex", "WeCode Codex", "wecode codex")
        },
        AgentRecipe {
            acp: vec!["opencode", "acp"],
            ..recipe("wecode_opencode", "WeCode OpenCode", "wecode opencode")
        },
        AgentRecipe {
            aliases: vec!["cbc"],
            acp: vec!["--acp"],
            npm: Some("@tencent-ai/codebuddy-code"),
            ..recipe("codebuddy", "CodeBuddy", "codebuddy")
        },
        AgentRecipe {
            npm: Some("@google/gemini-cli"),
            brew: Some("gemini-cli"),
            acp: vec!["--experimental-acp"],
            ..recipe("gemini", "Gemini CLI", "gemini")
        },
        AgentRecipe {
            acp: vec!["--acp"],
            ..recipe("qwen", "Qwen Code", "qwen")
        },
        AgentRecipe {
            acp: vec!["--acp", "--stdio"],
            model_flag: None,
            ..recipe("copilot", "GitHub Copilot CLI", "copilot")
        },
        AgentRecipe {
            acp: vec!["acp"],
            model_flag: None,
            ..recipe("goose", "Goose", "goose")
        },
        AgentRecipe {
            acp: vec!["exec", "--output-format", "acp"],
            ..recipe("droid", "Factory Droid", "droid")
        },
        AgentRecipe {
            npm: Some("@earendil-works/pi-coding-agent"),
            adapter: Some("pi-acp"),
            ..recipe("pi", "Pi", "pi")
        },
        AgentRecipe {
            npm: Some("@oh-my-pi/pi-coding-agent"),
            acp: vec!["acp"],
            ..recipe("omp", "Oh My Pi", "omp")
        },
        AgentRecipe {
            aliases: vec!["cline-cli"],
            npm: Some("cline"),
            brew: Some("cline"),
            acp: vec!["--acp"],
            ..recipe("cline", "Cline CLI", "cline")
        },
        AgentRecipe {
            npm: Some("@charmland/crush"),
            brew: Some("crush"),
            adapter: Some("crush-acp"),
            acp: vec!["acp"],
            model_flag: None,
            ..recipe("crush", "Crush", "crush")
        },
        AgentRecipe {
            aliases: vec!["qodercli"],
            acp: vec!["--acp"],
            model_flag: None,
            ..recipe("qoder", "Qoder", "qoder")
        },
        AgentRecipe {
            aliases: vec!["qoderclicn"],
            acp: vec!["--acp"],
            model_flag: None,
            ..recipe("qoder_cn", "Qoder CN", "qodercn")
        },
        AgentRecipe {
            adapter: Some("hermes-acp"),
            acp: vec!["acp"],
            model_flag: None,
            ..recipe("hermes", "Hermes Agent", "hermes")
        },
        AgentRecipe {
            aliases: vec!["mimocode"],
            acp: vec!["acp"],
            ..recipe("mimo", "MiMo Code", "mimo")
        },
        AgentRecipe {
            acp: vec!["acp"],
            ..recipe("mcode", "MiniMax Code", "mcode")
        },
        AgentRecipe {
            acp: vec!["acp"],
            ..recipe("deveco", "DevEco Code", "deveco")
        },
        AgentRecipe {
            model_flag: None,
            ..recipe("stepcode", "StepCode", "stepcode")
        },
        AgentRecipe {
            aliases: vec!["commandcode"],
            model_flag: None,
            ..recipe("commandcode", "Command Code", "command-code")
        },
        AgentRecipe {
            npm: Some("@atomgit.com/atomcode"),
            brew: Some("atomcode"),
            model_flag: None,
            ..recipe("atomcode", "AtomCode", "atomcode")
        },
        AgentRecipe {
            aliases: vec!["snow-cli"],
            npm: Some("snow-ai"),
            model_flag: None,
            ..recipe("snow", "Snow CLI", "snow")
        },
        AgentRecipe {
            model_flag: None,
            ..recipe("jcode", "jcode", "jcode")
        },
        AgentRecipe {
            model_flag: None,
            ..recipe("devin", "Devin", "devin")
        },
        AgentRecipe {
            npm: Some("mothx-installer"),
            model_flag: None,
            ..recipe("mothx", "MothX", "mothx")
        },
        AgentRecipe {
            npm: Some("@prismshadow/penguin-cli"),
            model_flag: None,
            ..recipe("penguin", "PenguinHarness", "penguin")
        },
        AgentRecipe {
            aliases: vec!["omo-ai"],
            npm: Some("omo-ai"),
            model_flag: None,
            ..recipe("omo", "OmO", "omo")
        },
        AgentRecipe {
            model_flag: None,
            ..recipe("prime_agent", "Prime Agent", "prime-agent")
        },
        AgentRecipe {
            aliases: vec!["soulforge"],
            model_flag: None,
            ..recipe("empryo", "Empryo", "empryo")
        },
        AgentRecipe {
            model_flag: None,
            ..recipe("ante", "Ante", "ante")
        },
        AgentRecipe {
            command: "",
            app: Some("Claude"),
            model_flag: None,
            ..recipe("claude_desktop", "Claude Desktop", "")
        },
        AgentRecipe {
            command: "",
            app: Some("Zed"),
            model_flag: None,
            ..recipe("zed", "Zed", "")
        },
        AgentRecipe {
            command: "",
            app: Some("Visual Studio Code"),
            model_flag: None,
            ..recipe("vscode", "VS Code", "")
        },
        AgentRecipe {
            command: "",
            app: Some("WorkBuddy"),
            model_flag: None,
            ..recipe("workbuddy", "WorkBuddy", "")
        },
    ]
}

#[derive(Default)]
struct InventoryState {
    agents: Vec<LocalAgent>,
    scanned_at: Option<String>,
    revision: u64,
    refreshing: bool,
    error: Option<String>,
}

pub struct AgentManagementService {
    store: Arc<BuddyStore>,
    inventory: RwLock<InventoryState>,
    cache_loaded: Mutex<bool>,
    operation: Mutex<()>,
    inventory_events: broadcast::Sender<AgentInventorySnapshot>,
    update_events: broadcast::Sender<AgentUpdateSnapshot>,
    updates: RwLock<AgentUpdateSnapshot>,
    updates_loaded: Mutex<bool>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}
fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn installation_identity(path: &str) -> String {
    sha256(path)[..20].to_string()
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}
fn launcher(
    protocol: &str,
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
) -> Launcher {
    Launcher {
        protocol: Some(protocol.to_string()),
        command,
        args: Some(args),
        env,
        timeout_seconds: DEFAULT_LAUNCHER_TIMEOUT_SECONDS,
        model: None,
    }
}
fn acp_fingerprint(launcher: Option<&Launcher>) -> String {
    launcher
        .map(|launcher| {
            serde_json::to_string(&json!([launcher.command, launcher.args, launcher.env]))
                .unwrap_or_default()
        })
        .unwrap_or_default()
}
pub fn is_newer_stable_version(a: &str, b: &str) -> bool {
    let parse = |value: &str| {
        let captures = Regex::new(r"^v?(\d+)\.(\d+)\.(\d+)$")
            .ok()?
            .captures(value)?;
        Some((
            captures[1].parse::<u64>().ok()?,
            captures[2].parse::<u64>().ok()?,
            captures[3].parse::<u64>().ok()?,
        ))
    };
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}
pub fn agent_update_group_key(agent: &LocalAgent) -> String {
    match &agent.native_update {
        Some(update) if update.target == "wecode" => format!(
            "wecode:{}",
            update
                .shared_key
                .clone()
                .unwrap_or_else(|| update.command.clone())
        ),
        _ => agent.id.clone(),
    }
}
pub fn resolve_agent_protocol(agent: &LocalAgent) -> String {
    if let Some(protocol) = &agent.selected_protocol {
        return protocol.clone();
    }
    if agent.runnable {
        "cli".to_string()
    } else if agent.acp.is_some() {
        "acp".to_string()
    } else if agent.cli.is_some() {
        "cli".to_string()
    } else {
        "acp".to_string()
    }
}
pub fn agent_protocol_ready(agent: &LocalAgent, protocol: &str) -> bool {
    if protocol == "cli" {
        agent.runnable
    } else {
        agent.acp_status.as_deref() == Some("verified")
    }
}
pub fn resolve_protocol_launcher(
    protocol: &str,
    agent: Option<&LocalAgent>,
    settings: &GlobalSettings,
    id: &str,
) -> Option<Launcher> {
    let config = settings
        .agent_management
        .as_ref()
        .and_then(|items| items.get(id));
    let saved = settings.launchers.as_ref().and_then(|items| items.get(id));
    let configured = config.and_then(|config| {
        if protocol == "acp" {
            config.acp.clone()
        } else {
            config.cli.clone()
        }
    });
    let discovered = agent.and_then(|agent| {
        if protocol == "acp" {
            agent.acp.clone()
        } else {
            agent.cli.clone()
        }
    });
    let saved = saved
        .filter(|saved| saved.protocol.as_deref().unwrap_or("cli") == protocol)
        .cloned();
    configured.or(discovered).or(saved).map(|mut launcher| {
        launcher.protocol = Some(protocol.to_string());
        launcher
    })
}
pub fn resolve_commit_agent_launch(
    agent: &LocalAgent,
    settings: &GlobalSettings,
    task: Option<&crate::buddy::types::TaskSettings>,
) -> Option<Launcher> {
    let protocol = settings
        .agent_management
        .as_ref()
        .and_then(|items| items.get(&agent.id))
        .and_then(|config| config.protocol.clone())
        .or_else(|| {
            settings
                .launchers
                .as_ref()
                .and_then(|items| items.get(&agent.id))
                .and_then(|launcher| launcher.protocol.clone())
        })
        .unwrap_or_else(|| resolve_agent_protocol(agent));
    let mut launcher = resolve_protocol_launcher(&protocol, Some(agent), settings, &agent.id)?;
    if !agent.enabled || launcher.command.is_empty() || !agent_protocol_ready(agent, &protocol) {
        return None;
    }
    if let Some(previous) = task.and_then(|task| task.launchers.get(&agent.id)) {
        if previous.protocol.as_deref().unwrap_or("cli") == protocol
            && previous.command == launcher.command
            && previous.args == launcher.args
            && previous.env == launcher.env
        {
            launcher.model = previous.model.clone().or(launcher.model);
        }
    }
    Some(launcher)
}

impl AgentManagementService {
    pub fn new(store: Arc<BuddyStore>) -> Self {
        let (inventory_events, _) = broadcast::channel(64);
        let (update_events, _) = broadcast::channel(64);
        Self {
            store,
            inventory: RwLock::new(InventoryState::default()),
            cache_loaded: Mutex::new(false),
            operation: Mutex::new(()),
            inventory_events,
            update_events,
            updates: RwLock::new(AgentUpdateSnapshot::default()),
            updates_loaded: Mutex::new(false),
        }
    }

    pub fn subscribe_inventory(&self) -> broadcast::Receiver<AgentInventorySnapshot> {
        self.inventory_events.subscribe()
    }
    pub fn subscribe_updates(&self) -> broadcast::Receiver<AgentUpdateSnapshot> {
        self.update_events.subscribe()
    }

    async fn configuration_hash(&self) -> Result<String, String> {
        let settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|error| error.to_string())?;
        Ok(sha256(
            &serde_json::to_string(&json!([settings.launchers, settings.agent_management]))
                .map_err(|error| error.to_string())?,
        ))
    }

    fn snapshot_from(state: &InventoryState) -> AgentInventorySnapshot {
        AgentInventorySnapshot {
            revision: state.revision,
            agents: state.agents.clone(),
            scanned_at: state.scanned_at.clone(),
            refreshing: state.refreshing,
            error: state.error.clone(),
        }
    }

    async fn publish_inventory(&self) {
        let snapshot = {
            let mut state = self.inventory.write().await;
            state.revision += 1;
            Self::snapshot_from(&state)
        };
        let _ = self.inventory_events.send(snapshot);
    }

    pub async fn get_inventory(&self) -> AgentInventorySnapshot {
        if let Err(error) = self.load_cache().await {
            let mut state = self.inventory.write().await;
            state.error = Some(error);
        }
        let state = self.inventory.read().await;
        Self::snapshot_from(&state)
    }

    async fn load_cache(&self) -> Result<(), String> {
        let mut loaded = self.cache_loaded.lock().await;
        if *loaded {
            return Ok(());
        }
        *loaded = true;
        let path = self
            .store
            .data_root
            .join("global")
            .join("agent-inventory-cache.json");
        let Ok(text) = tokio::fs::read_to_string(path).await else {
            return Ok(());
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if value.get("schemaVersion").and_then(Value::as_u64) != Some(2) {
            return Ok(());
        }
        let configuration_matches = value.get("configurationHash").and_then(Value::as_str)
            == Some(self.configuration_hash().await?.as_str());
        let settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|error| error.to_string())?;
        let recipes = agent_recipes();
        let mut agents = Vec::new();
        for mut agent in serde_json::from_value::<Vec<LocalAgent>>(
            value.get("agents").cloned().unwrap_or_else(|| json!([])),
        )
        .map_err(|error| error.to_string())?
        {
            let recipe = recipes.iter().find(|recipe| recipe.id == agent.id);
            let config = settings
                .agent_management
                .as_ref()
                .and_then(|items| items.get(&agent.id));
            let saved = settings
                .launchers
                .as_ref()
                .and_then(|items| items.get(&agent.id));
            let automatic_cli = if agent.path.is_empty() {
                None
            } else {
                Some(launcher(
                    "cli",
                    agent.path.clone(),
                    recipe
                        .map(|recipe| split_command(recipe.command).into_iter().skip(1).collect())
                        .unwrap_or_default(),
                    HashMap::new(),
                ))
            };
            let automatic_acp = if let Some(adapter) = recipe.and_then(|recipe| recipe.adapter) {
                find_executable(adapter)
                    .await
                    .map(|path| launcher("acp", path, Vec::new(), HashMap::new()))
            } else if !agent.path.is_empty() {
                agent
                    .native_acp_args
                    .clone()
                    .or_else(|| {
                        recipe.map(|recipe| recipe.acp.iter().map(|arg| arg.to_string()).collect())
                    })
                    .filter(|args: &Vec<String>| !args.is_empty())
                    .map(|args| launcher("acp", agent.path.clone(), args, HashMap::new()))
            } else {
                None
            };
            let saved_cli = config.and_then(|config| config.cli.clone()).or_else(|| {
                saved
                    .filter(|launcher| launcher.protocol.as_deref() != Some("acp"))
                    .cloned()
            });
            let saved_acp = config.and_then(|config| config.acp.clone()).or_else(|| {
                saved
                    .filter(|launcher| launcher.protocol.as_deref() == Some("acp"))
                    .cloned()
            });
            agent.automatic_cli = automatic_cli.clone();
            agent.automatic_acp = automatic_acp.clone();
            agent.cli = saved_cli.or(automatic_cli);
            agent.acp = saved_acp.or(automatic_acp);
            agent.enabled = config.and_then(|config| config.enabled).unwrap_or(true);
            agent.selected_protocol = config
                .and_then(|config| config.protocol.clone())
                .or_else(|| saved.and_then(|launcher| launcher.protocol.clone()))
                .or_else(|| {
                    Some(if agent.runnable {
                        "cli".to_string()
                    } else if agent.acp.is_some() {
                        "acp".to_string()
                    } else {
                        "cli".to_string()
                    })
                });
            if !configuration_matches {
                agent.acp_status = Some(if agent.acp.is_some() {
                    "unverified".to_string()
                } else {
                    "missing".to_string()
                });
                agent.acp_error = None;
                agent.acp_failure_stage = None;
            }
            agents.push(agent);
        }
        let mut state = self.inventory.write().await;
        state.agents = agents;
        state.scanned_at = if configuration_matches {
            value
                .get("scannedAt")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            None
        };
        drop(state);
        Ok(())
    }

    pub async fn initialize(&self) -> Result<(), String> {
        self.get_inventory().await;
        self.get_update_state().await?;
        Ok(())
    }

    pub fn needs_startup_refresh(&self, state: &AgentInventorySnapshot) -> bool {
        state
            .scanned_at
            .as_ref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|scanned| Utc::now().signed_duration_since(scanned).num_seconds() > 600)
            .unwrap_or(true)
    }

    pub async fn list_local_agents(&self, force: bool) -> Result<Vec<LocalAgent>, String> {
        let snapshot = self.get_inventory().await;
        if force || snapshot.agents.is_empty() {
            return Ok(self.refresh().await?.agents);
        }
        Ok(snapshot.agents)
    }

    async fn persist_inventory(&self) -> Result<(), String> {
        let settings_hash = self.configuration_hash().await?;
        let state = self.inventory.read().await;
        let agents = state
            .agents
            .iter()
            .map(|agent| {
                let mut metadata = agent.clone();
                metadata.native_acp_args = agent.automatic_acp.as_ref().and_then(|launcher| {
                    if launcher.command == agent.path {
                        launcher.args.clone()
                    } else {
                        None
                    }
                });
                metadata.cli = None;
                metadata.acp = None;
                metadata.automatic_cli = None;
                metadata.automatic_acp = None;
                metadata
            })
            .collect::<Vec<_>>();
        let value = json!({
            "schemaVersion": 2,
            "scannedAt": state.scanned_at,
            "configurationHash": settings_hash,
            "agents": agents,
        });
        drop(state);
        let path = self
            .store
            .data_root
            .join("global")
            .join("agent-inventory-cache.json");
        atomic_write_json(&path, &value)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn refresh(&self) -> Result<AgentInventorySnapshot, String> {
        let _operation = self.operation.lock().await;
        self.load_cache().await?;
        {
            let mut state = self.inventory.write().await;
            state.refreshing = true;
            state.error = None;
        }
        self.publish_inventory().await;
        let result = self.discover().await;
        match result {
            Ok(mut agents) => {
                let previous = self.inventory.read().await.agents.clone();
                for agent in &mut agents {
                    if let Some(cached) = previous.iter().find(|cached| {
                        cached.id == agent.id && cached.installation_id == agent.installation_id
                    }) {
                        agent.latest = cached.latest.clone();
                        agent.checked_at = cached.checked_at.clone();
                        agent.update_error = cached.update_error.clone();
                        if cached.acp_status.as_deref() == Some("failed")
                            && cached.acp_launch_fingerprint == agent.acp_launch_fingerprint
                        {
                            agent.acp_status = cached.acp_status.clone();
                            agent.acp_error = cached.acp_error.clone();
                            agent.acp_failure_stage = cached.acp_failure_stage.clone();
                        }
                    }
                }
                agents.sort_by(|a, b| a.name.cmp(&b.name));
                {
                    let mut state = self.inventory.write().await;
                    state.agents = agents;
                    state.scanned_at = Some(now());
                    state.refreshing = false;
                }
                self.persist_inventory().await?;
                self.publish_inventory().await;
            }
            Err(error) => {
                let mut state = self.inventory.write().await;
                state.refreshing = false;
                state.error = Some(redact_sensitive_text(&error));
            }
        }
        self.publish_inventory().await;
        Ok(self.get_inventory().await)
    }

    async fn discover(&self) -> Result<Vec<LocalAgent>, String> {
        let settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|error| error.to_string())?;
        let timeout = settings
            .launchers
            .as_ref()
            .and_then(|items| items.values().next())
            .map(|launcher| launcher.timeout_seconds)
            .unwrap_or(DEFAULT_LAUNCHER_TIMEOUT_SECONDS);
        let mut out = Vec::new();
        for recipe in agent_recipes() {
            let config = settings
                .agent_management
                .as_ref()
                .and_then(|items| items.get(recipe.id));
            let saved = settings
                .launchers
                .as_ref()
                .and_then(|items| items.get(recipe.id));
            let saved_cli = config.and_then(|config| config.cli.clone()).or_else(|| {
                saved
                    .filter(|launcher| launcher.protocol.as_deref() != Some("acp"))
                    .cloned()
            });
            let saved_acp = config.and_then(|config| config.acp.clone()).or_else(|| {
                saved
                    .filter(|launcher| launcher.protocol.as_deref() == Some("acp"))
                    .cloned()
            });
            if recipe.command.is_empty() && saved_cli.is_none() && saved_acp.is_none() {
                continue;
            }
            let command = saved_cli
                .as_ref()
                .map(|launcher| launcher.command.clone())
                .unwrap_or_else(|| recipe.command.to_string());
            let tokens = split_command(&command);
            let mut path = find_executable(tokens.first().map(String::as_str).unwrap_or("")).await;
            let mut identity_error = None;
            if path.is_none() && saved_cli.is_none() {
                for alias in &recipe.aliases {
                    let Some(candidate) = find_executable(alias).await else {
                        continue;
                    };
                    if ["cbc", "agent", "snow"].contains(alias) {
                        let real = tokio::fs::canonicalize(&candidate)
                            .await
                            .unwrap_or_else(|_| PathBuf::from(&candidate));
                        let primary = split_command(recipe.command)
                            .first()
                            .cloned()
                            .unwrap_or_default();
                        let target_name = real
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let package_ok = recipe.npm.is_some_and(|package| {
                            real.to_string_lossy()
                                .contains(&format!("/node_modules/{package}/"))
                        });
                        if target_name != primary && !package_ok {
                            identity_error =
                                Some(format!("Alias '{alias}' does not identify {}", recipe.name));
                            continue;
                        }
                    }
                    path = Some(candidate);
                    break;
                }
            }
            let app_version = if let Some(app) = recipe.app {
                let mut found = None;
                for root in [PathBuf::from("/Applications"), home().join("Applications")] {
                    let plist = root.join(format!("{app}.app/Contents/Info.plist"));
                    if let Ok(version) = run_file(
                        "/usr/libexec/PlistBuddy",
                        &[
                            "-c".to_string(),
                            "Print :CFBundleShortVersionString".to_string(),
                            plist.to_string_lossy().to_string(),
                        ],
                        3000,
                        None,
                    )
                    .await
                    {
                        found = Some(version);
                        break;
                    }
                }
                found
            } else {
                None
            };
            if path.is_none() && saved_cli.is_none() {
                if let (Some(app), app_cli) = (recipe.app, &recipe.app_cli) {
                    'roots: for root in
                        [PathBuf::from("/Applications"), home().join("Applications")]
                    {
                        for relative in app_cli {
                            let candidate = root.join(format!("{app}.app")).join(relative);
                            if executable(&candidate).await {
                                path = Some(candidate.to_string_lossy().to_string());
                                break 'roots;
                            }
                        }
                    }
                }
            }
            let adapter = match recipe.adapter {
                Some(adapter) => find_executable(adapter).await,
                None => None,
            };
            if path.is_none()
                && app_version.is_none()
                && config.is_none()
                && adapter.is_none()
                && saved_acp.is_none()
            {
                continue;
            }
            let real_path = match &path {
                Some(path) => tokio::fs::canonicalize(path)
                    .await
                    .unwrap_or_else(|_| PathBuf::from(path))
                    .to_string_lossy()
                    .to_string(),
                None => String::new(),
            };
            let mut args = tokens.iter().skip(1).cloned().collect::<Vec<_>>();
            if let Some(saved) = &saved_cli {
                args.extend(saved.args.clone().unwrap_or_default());
            }
            let env = saved_cli
                .as_ref()
                .map(|launcher| launcher.env.clone())
                .unwrap_or_default();
            let mut version = None;
            let mut error = None;
            if let Some(path) = &path {
                let mut version_args = args.clone();
                version_args.push("--version".to_string());
                match run_file(path, &version_args, 3000, Some(&env)).await {
                    Ok(raw) => {
                        version =
                            extract_version(&raw).or_else(|| Some(raw.chars().take(100).collect()))
                    }
                    Err(message) => error = Some(message),
                }
            }
            let kind = command_kind_for_tokens(recipe.id, &tokens);
            let native = kind != LauncherCommandKind::Contract;
            let cli = path.clone().map(|path| {
                let mut launcher = launcher("cli", path, args.clone(), env.clone());
                launcher.timeout_seconds = timeout;
                launcher.model = saved_cli
                    .as_ref()
                    .and_then(|launcher| launcher.model.clone());
                launcher
            });
            let mut automatic_acp =
                adapter.map(|path| launcher("acp", path, Vec::new(), HashMap::new()));
            if automatic_acp.is_none() {
                if let (Some(path), acp_args) = (&path, &recipe.acp) {
                    if !acp_args.is_empty() {
                        automatic_acp = Some(launcher(
                            "acp",
                            path.clone(),
                            acp_args.iter().map(|arg| arg.to_string()).collect(),
                            HashMap::new(),
                        ));
                    }
                }
            }
            if automatic_acp.is_none() && kind == LauncherCommandKind::NativeKimi {
                if let Some(cli) = &cli {
                    if let Ok(help) = run_file(
                        &cli.command,
                        &[
                            cli.args.clone().unwrap_or_default(),
                            vec!["--help".to_string()],
                        ]
                        .concat(),
                        3000,
                        Some(&cli.env),
                    )
                    .await
                    {
                        if let Some(acp_args) = kimi_acp_args_from_help(&help) {
                            automatic_acp = Some(launcher(
                                "acp",
                                cli.command.clone(),
                                [cli.args.clone().unwrap_or_default(), acp_args].concat(),
                                cli.env.clone(),
                            ));
                        }
                    }
                }
            }
            let automatic_cli = path.clone().map(|path| {
                launcher(
                    "cli",
                    path,
                    split_command(recipe.command).into_iter().skip(1).collect(),
                    HashMap::new(),
                )
            });
            let mut acp = saved_acp.or_else(|| automatic_acp.clone());
            if let Some(launcher) = &mut acp {
                if launcher.args.as_ref().is_none_or(|args| args.is_empty()) {
                    launcher.args = Some(default_acp_args(&launcher.command, Some(recipe.id)));
                }
                launcher.timeout_seconds = timeout;
            }
            if cli.is_none() && acp.is_none() {
                continue;
            }
            let runnable = cli.is_some()
                && error.is_none()
                && (native || config.and_then(|config| config.contract).unwrap_or(false));
            let native_update = if let Some(cli) = &cli {
                detect_native_update(recipe.id, cli, native, &real_path).await
            } else {
                None
            };
            let source_info = detect_source(recipe.npm, &real_path, path.as_deref()).await;
            let package = source_info.3.or_else(|| recipe.brew.map(String::from));
            let installations = match &path {
                Some(path) => Some(vec![AgentInstallation {
                    id: installation_identity(path),
                    path: path.clone(),
                    real_path: real_path.clone(),
                    version: version.clone(),
                    ..Default::default()
                }]),
                None => None,
            };
            let acp_status = Some(if acp.is_some() {
                "unverified".to_string()
            } else {
                "missing".to_string()
            });
            let acp_launch_fingerprint =
                acp.as_ref().map(|launcher| acp_fingerprint(Some(launcher)));
            out.push(LocalAgent {
                id: recipe.id.to_string(),
                installation_id: installation_identity(path.as_deref().unwrap_or(recipe.id)),
                name: recipe.name.to_string(),
                path: path.clone().unwrap_or_default(),
                real_path,
                version,
                app_version,
                source: source_info.0,
                prefix: source_info.1,
                manager: source_info.2,
                package,
                selected_protocol: config
                    .and_then(|config| config.protocol.clone())
                    .or_else(|| saved.and_then(|launcher| launcher.protocol.clone()))
                    .or_else(|| {
                        Some(if runnable {
                            "cli".to_string()
                        } else if acp.is_some() {
                            "acp".to_string()
                        } else {
                            "cli".to_string()
                        })
                    }),
                runnable,
                enabled: config.and_then(|config| config.enabled).unwrap_or(true),
                error: error.or(identity_error),
                cli,
                acp,
                model_flag: recipe.model_flag.map(String::from),
                installations,
                automatic_cli,
                automatic_acp,
                native_update,
                acp_status,
                acp_launch_fingerprint,
                ..Default::default()
            });
        }
        for agent in &mut out {
            if agent.acp.is_some() && agent.acp_status.as_deref() != Some("failed") {
                let _ = self.probe_target(agent, false).await;
            }
        }
        Ok(out)
    }

    pub async fn configure(&self, id: &str, config: AgentConfiguration) -> Result<(), String> {
        if !valid_id(id) {
            return Err("Invalid agent ID".to_string());
        }
        if self.get_update_state().await?.active_batch.is_some() {
            return Err("Agent updates are running".to_string());
        }
        let mut settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|error| error.to_string())?;
        let mut management = settings.agent_management.clone().unwrap_or_default();
        let mut next = management.get(id).cloned().unwrap_or_default();
        if config.contract.is_some() {
            next.contract = config.contract;
        }
        if config.enabled.is_some() {
            next.enabled = config.enabled;
        }
        if config.protocol.is_some() {
            next.protocol = config.protocol;
        }
        if config.cli.is_some() {
            next.cli = config.cli;
        }
        if config.acp.is_some() {
            next.acp = config.acp;
        }
        let agent = self
            .get_inventory()
            .await
            .agents
            .into_iter()
            .find(|agent| agent.id == id);
        let protocol = next
            .protocol
            .clone()
            .or_else(|| agent.as_ref().map(resolve_agent_protocol))
            .unwrap_or_else(|| "cli".to_string());
        let candidate = if protocol == "acp" {
            next.acp
                .clone()
                .or_else(|| agent.as_ref().and_then(|agent| agent.acp.clone()))
        } else {
            next.cli
                .clone()
                .or_else(|| agent.as_ref().and_then(|agent| agent.cli.clone()))
                .or_else(|| Some(default_launcher_for(id)))
        };
        let mut resolved = candidate.ok_or_else(|| format!("No configured ACP entry for {id}"))?;
        if resolved.command.trim().is_empty() {
            return Err(format!("No configured ACP entry for {id}"));
        }
        resolved.protocol = Some(protocol.clone());
        next.protocol = Some(protocol);
        management.insert(id.to_string(), next);
        settings.agent_management = Some(management);
        let mut launchers = settings.launchers.clone().unwrap_or_default();
        launchers.insert(id.to_string(), resolved);
        settings.launchers = Some(launchers);
        self.store
            .update_global_settings(&settings)
            .await
            .map_err(|error| error.to_string())?;
        {
            let mut state = self.inventory.write().await;
            if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == id) {
                if let Some(config) = settings
                    .agent_management
                    .as_ref()
                    .and_then(|items| items.get(id))
                {
                    agent.enabled = config.enabled.unwrap_or(agent.enabled);
                    agent.selected_protocol = config
                        .protocol
                        .clone()
                        .or_else(|| agent.selected_protocol.clone());
                    if config.cli.is_some() {
                        agent.cli = config.cli.clone();
                    }
                    if config.acp.is_some() {
                        agent.acp = config.acp.clone();
                    }
                }
                agent.acp_status = Some(if agent.acp.is_some() {
                    "unverified".to_string()
                } else {
                    "missing".to_string()
                });
                agent.acp_error = None;
                agent.acp_launch_fingerprint = agent
                    .acp
                    .as_ref()
                    .map(|launcher| acp_fingerprint(Some(launcher)));
            }
        }
        self.persist_inventory().await?;
        self.publish_inventory().await;
        Ok(())
    }

    pub async fn probe(&self, id: &str) -> Result<LocalAgent, String> {
        let _operation = self.operation.lock().await;
        self.load_cache().await?;
        let mut agent = self
            .inventory
            .read()
            .await
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .cloned()
            .ok_or_else(|| "ACP entry unavailable".to_string())?;
        self.probe_target(&mut agent, true).await?;
        {
            let mut state = self.inventory.write().await;
            if let Some(row) = state.agents.iter_mut().find(|row| row.id == id) {
                *row = agent.clone();
            }
        }
        self.persist_inventory().await?;
        self.publish_inventory().await;
        Ok(agent)
    }

    async fn probe_target(&self, agent: &mut LocalAgent, session: bool) -> Result<(), String> {
        let Some(acp) = agent.acp.clone() else {
            return Ok(());
        };
        if acp.command.contains("npx") {
            return Err("Install a local ACP adapter before scanning".to_string());
        }
        let tokens = split_command(&acp.command);
        let cwd = home().to_string_lossy().to_string();
        let env = prepare_acp_environment(
            &agent.id,
            acp.env.clone(),
            Some(&self.store.data_root),
            None,
        )
        .await
        .map_err(|error| error.to_string())?;
        let transport = Arc::new(AcpStdioTransport::new(StdioTransportOptions {
            command: tokens.first().cloned().unwrap_or_default(),
            args: [
                tokens.iter().skip(1).cloned().collect::<Vec<_>>(),
                acp.args.clone().unwrap_or_default(),
            ]
            .concat(),
            cwd: Some(cwd.clone()),
            env: Some(env),
            on_stderr: None,
        }));
        let client = AcpClient::new(
            transport.clone() as Arc<dyn AcpTransport>,
            AcpClientOptions {
                timeout_ms: Some(10_000),
                client_info: None,
            },
        );
        transport.start().await;
        let result = async {
            let initialized = client
                .initialize(None)
                .await
                .map_err(|error| error.to_string())?;
            if initialized.protocol_version.as_u64() != Some(1) {
                return Err("Incompatible ACP protocol".to_string());
            }
            if session {
                client
                    .new_session(&AcpNewSessionParams {
                        cwd,
                        ..Default::default()
                    })
                    .await
                    .map_err(|error| error.to_string())?;
            }
            Ok::<(), String>(())
        }
        .await;
        client.close().await;
        match result {
            Ok(()) => {
                agent.acp_status = Some("verified".to_string());
                agent.acp_error = None;
                agent.acp_failure_stage = None;
            }
            Err(error) => {
                let message = redact_sensitive_text(&error);
                let auth = parse_acp_auth_failure(&message).is_some();
                agent.acp_status = Some(if message.contains("Incompatible") {
                    "incompatible".to_string()
                } else {
                    "failed".to_string()
                });
                agent.acp_failure_stage = Some(if session || auth {
                    "session".to_string()
                } else {
                    "handshake".to_string()
                });
                agent.acp_error = Some(message);
            }
        }
        agent.acp_launch_fingerprint = Some(acp_fingerprint(Some(&acp)));
        Ok(())
    }

    pub async fn record_acp_probe_async(&self, id: &str, result: Result<(), String>) {
        let mut state = self.inventory.write().await;
        if let Some(agent) = state.agents.iter_mut().find(|agent| agent.id == id) {
            match result {
                Ok(()) => {
                    agent.acp_status = Some("verified".to_string());
                    agent.acp_error = None;
                    agent.acp_failure_stage = None;
                }
                Err(error) => {
                    let error = redact_sensitive_text(&error);
                    agent.acp_status = Some("failed".to_string());
                    agent.acp_failure_stage = Some(if parse_acp_auth_failure(&error).is_some() {
                        "session".to_string()
                    } else {
                        "handshake".to_string()
                    });
                    agent.acp_error = Some(error);
                }
            }
            agent.acp_launch_fingerprint = agent
                .acp
                .as_ref()
                .map(|launcher| acp_fingerprint(Some(launcher)));
        }
        drop(state);
        let _ = self.persist_inventory().await;
        self.publish_inventory().await;
    }

    pub async fn models(
        &self,
        id: &str,
        override_launcher: Option<Launcher>,
    ) -> Result<AcpModelList, String> {
        let agent = self
            .get_inventory()
            .await
            .agents
            .into_iter()
            .find(|agent| agent.id == id);
        let cli = override_launcher
            .filter(|launcher| !launcher.command.is_empty())
            .or_else(|| agent.and_then(|agent| agent.cli));
        let Some(cli) = cli else {
            return Ok(AcpModelList {
                models: Vec::new(),
                current_model_id: None,
                error: None,
            });
        };
        let tokens = split_command(&cli.command);
        let command = tokens.first().cloned().unwrap_or_default();
        let args = [
            tokens.iter().skip(1).cloned().collect::<Vec<_>>(),
            cli.args.clone().unwrap_or_default(),
        ]
        .concat();
        let kind = command_kind_for_tokens(id, &tokens);
        let (extra, timeout) = match kind {
            LauncherCommandKind::NativeCursor | LauncherCommandKind::NativeAgy => {
                (vec!["models".to_string()], 30_000)
            }
            LauncherCommandKind::NativeKimi => (
                vec![
                    "provider".to_string(),
                    "list".to_string(),
                    "--json".to_string(),
                ],
                10_000,
            ),
            LauncherCommandKind::NativeOpencode => (vec!["models".to_string()], 15_000),
            _ if id == "codebuddy" => (vec!["--help".to_string()], 3_000),
            _ => {
                return Ok(AcpModelList {
                    models: Vec::new(),
                    current_model_id: cli.model,
                    error: None,
                })
            }
        };
        let raw = run_file(&command, &[args, extra].concat(), timeout, Some(&cli.env)).await?;
        let models = match kind {
            LauncherCommandKind::NativeCursor => parse_cursor_models(&raw),
            LauncherCommandKind::NativeAgy => parse_agy_models(&raw),
            LauncherCommandKind::NativeKimi => parse_kimi_models(&raw)?,
            LauncherCommandKind::NativeOpencode => raw
                .lines()
                .map(str::trim)
                .filter(|id| Regex::new(r"^[^\s]+/[^\s]+$").unwrap().is_match(id))
                .map(|id| AcpModelInfo {
                    id: id.to_string(),
                    name: id.to_string(),
                })
                .collect(),
            _ => Regex::new(r"Currently supported: \(([^)]+)\)")
                .unwrap()
                .captures(&raw)
                .map(|captures| {
                    captures[1]
                        .split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(|id| AcpModelInfo {
                            id: id.to_string(),
                            name: id.to_string(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        Ok(AcpModelList {
            models,
            current_model_id: cli.model,
            error: None,
        })
    }

    pub async fn check_updates(&self, force: bool) -> Result<Vec<LocalAgent>, String> {
        let _operation = self.operation.lock().await;
        self.load_cache().await?;
        let mut agents = self.inventory.read().await.agents.clone();
        for agent in &mut agents {
            if !force
                && agent.update_error.is_none()
                && agent
                    .checked_at
                    .as_ref()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .is_some_and(|checked| {
                        Utc::now().signed_duration_since(checked).num_seconds() < 3600
                    })
            {
                continue;
            }
            agent.checked_at = Some(now());
            agent.latest = None;
            agent.update_error = None;
            let result = latest_agent_version(agent).await;
            match result {
                Ok(Some(version)) => agent.latest = Some(version),
                Ok(None) if agent.native_update.is_none() => {
                    agent.update_error =
                        Some("Installation source does not expose a verified updater".to_string())
                }
                Ok(None) => {}
                Err(error) => agent.update_error = Some(redact_sensitive_text(&error)),
            }
        }
        {
            let mut state = self.inventory.write().await;
            for current in &mut state.agents {
                if let Some(checked) = agents.iter().find(|agent| {
                    agent.id == current.id && agent.installation_id == current.installation_id
                }) {
                    current.latest = checked.latest.clone();
                    current.checked_at = checked.checked_at.clone();
                    current.update_error = checked.update_error.clone();
                }
            }
        }
        self.persist_inventory().await?;
        self.publish_inventory().await;
        Ok(self.inventory.read().await.agents.clone())
    }

    pub async fn get_update_state(&self) -> Result<AgentUpdateSnapshot, String> {
        let mut loaded = self.updates_loaded.lock().await;
        if !*loaded {
            *loaded = true;
            let path = self
                .store
                .data_root
                .join("global")
                .join("agent-update-jobs.json");
            if let Ok(text) = tokio::fs::read_to_string(path).await {
                let value: Value = serde_json::from_str(&text)
                    .map_err(|error| format!("Cannot read Agent update history: {error}"))?;
                if value.is_array() {
                    let jobs: Vec<AgentUpdateJob> =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    if !jobs.is_empty() {
                        let items = jobs
                            .into_iter()
                            .map(|job| AgentUpdateItem {
                                id: job.id,
                                name: job.agent_id.clone(),
                                agent_id: job.agent_id,
                                status: match job.status.as_str() {
                                    "success" => "updated".to_string(),
                                    "queued" | "updating" | "verifying" => {
                                        "interrupted".to_string()
                                    }
                                    other => other.to_string(),
                                },
                                before: job.before,
                                after: job.after,
                                error: job.error,
                                ..Default::default()
                            })
                            .collect();
                        let mut state = self.updates.write().await;
                        state.last_completed_batch = Some(AgentUpdateBatch {
                            id: uuid::Uuid::new_v4().to_string(),
                            request_id: "legacy".to_string(),
                            status: "completed".to_string(),
                            items,
                            ..Default::default()
                        });
                    }
                } else {
                    let mut state: AgentUpdateSnapshot =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    if let Some(mut batch) = state.active_batch.take() {
                        batch.status = "interrupted".to_string();
                        batch.finished_at = Some(now());
                        for item in &mut batch.items {
                            if !update_terminal(&item.status) {
                                item.status = "interrupted".to_string();
                                item.finished_at = Some(now());
                            }
                        }
                        state.last_completed_batch = Some(batch);
                    }
                    *self.updates.write().await = state;
                }
                self.persist_updates().await?;
            }
        }
        drop(loaded);
        Ok(self.updates.read().await.clone())
    }

    async fn persist_updates(&self) -> Result<(), String> {
        let snapshot = {
            let mut state = self.updates.write().await;
            state.revision += 1;
            state.clone()
        };
        let path = self
            .store
            .data_root
            .join("global")
            .join("agent-update-jobs.json");
        atomic_write_json(
            &path,
            &serde_json::to_value(&snapshot).map_err(|error| error.to_string())?,
        )
        .await
        .map_err(|error| error.to_string())?;
        let _ = self.update_events.send(snapshot);
        Ok(())
    }

    async fn selection_keys(&self, ids: &[String]) -> Result<HashMap<String, String>, String> {
        let settings = self
            .store
            .read_global_settings()
            .await
            .map_err(|error| error.to_string())?;
        let recipes = agent_recipes();
        Ok(ids
            .iter()
            .map(|id| {
                let saved = settings.launchers.as_ref().and_then(|items| items.get(id));
                let configured = settings
                    .agent_management
                    .as_ref()
                    .and_then(|items| items.get(id))
                    .and_then(|config| config.cli.clone())
                    .or_else(|| {
                        saved
                            .filter(|launcher| launcher.protocol.as_deref() != Some("acp"))
                            .cloned()
                    });
                let recipe = recipes.iter().find(|recipe| recipe.id == id);
                let tokens = split_command(
                    &configured
                        .as_ref()
                        .map(|launcher| launcher.command.clone())
                        .or_else(|| recipe.map(|recipe| recipe.command.to_string()))
                        .unwrap_or_default(),
                );
                let selection_args = [
                    tokens.iter().skip(1).cloned().collect::<Vec<_>>(),
                    configured
                        .as_ref()
                        .and_then(|launcher| launcher.args.clone())
                        .unwrap_or_default(),
                ]
                .concat();
                let selection_env = configured
                    .as_ref()
                    .map(|launcher| launcher.env.clone())
                    .unwrap_or_default();
                let selection = json!([tokens.first(), selection_args, selection_env]);
                (id.clone(), sha256(&selection.to_string()))
            })
            .collect())
    }

    pub async fn start_updates(
        self: &Arc<Self>,
        ids: Vec<String>,
        request_id: String,
    ) -> Result<AgentUpdateSnapshot, String> {
        self.get_update_state().await?;
        if self.updates.read().await.active_batch.is_some()
            || self
                .updates
                .read()
                .await
                .last_completed_batch
                .as_ref()
                .is_some_and(|batch| batch.request_id == request_id)
        {
            return Ok(self.updates.read().await.clone());
        }
        if ids.is_empty() || ids.iter().any(|id| !valid_id(id)) || request_id.is_empty() {
            return Err("Invalid update request".to_string());
        }
        let targets = self.get_inventory().await.agents;
        let selection_keys = self.selection_keys(&ids).await?;
        let mut shared = HashSet::new();
        let ids = ids
            .into_iter()
            .filter(|id| {
                let agent = targets.iter().find(|agent| &agent.id == id);
                if agent
                    .and_then(|agent| agent.native_update.as_ref())
                    .is_some_and(|update| update.target == "wecode")
                {
                    shared.insert(agent_update_group_key(agent.unwrap()))
                } else {
                    true
                }
            })
            .collect::<Vec<_>>();
        let batch = AgentUpdateBatch {
            id: uuid::Uuid::new_v4().to_string(),
            request_id,
            created_at: Some(now()),
            status: "queued".to_string(),
            cancel_requested: false,
            items: ids
                .into_iter()
                .map(|id| {
                    let agent = targets.iter().find(|agent| agent.id == id);
                    AgentUpdateItem {
                        id: uuid::Uuid::new_v4().to_string(),
                        agent_id: id.clone(),
                        name: agent
                            .filter(|agent| {
                                agent
                                    .native_update
                                    .as_ref()
                                    .is_some_and(|update| update.target == "wecode")
                            })
                            .map(|_| "WeCode".to_string())
                            .or_else(|| agent.map(|agent| agent.name.clone()))
                            .unwrap_or_else(|| id.clone()),
                        installation_id: agent.map(|agent| agent.installation_id.clone()),
                        update_key: agent.map(agent_update_group_key),
                        target_key: agent.map(update_target_key),
                        selection_key: selection_keys.get(&id).cloned(),
                        target: agent
                            .and_then(|agent| agent.native_update.as_ref())
                            .map(|update| update.target.clone()),
                        source: agent.map(|agent| agent.source.clone()),
                        status: "queued".to_string(),
                        ..Default::default()
                    }
                })
                .collect(),
            error: None,
            finished_at: None,
        };
        let batch_id = batch.id.clone();
        self.updates.write().await.active_batch = Some(batch);
        self.persist_updates().await?;
        let accepted = self.updates.read().await.clone();
        let service = self.clone();
        tauri::async_runtime::spawn(async move {
            service.run_update_batch(batch_id).await;
        });
        Ok(accepted)
    }

    pub async fn cancel_updates(&self) -> Result<(), String> {
        self.get_update_state().await?;
        if let Some(batch) = &mut self.updates.write().await.active_batch {
            batch.cancel_requested = true;
            batch.status = "cancelling".to_string();
            self.persist_updates().await?;
        }
        Ok(())
    }

    pub async fn update_local_agents(
        self: &Arc<Self>,
        ids: Vec<String>,
    ) -> Result<Vec<AgentUpdateJob>, String> {
        self.start_updates(ids, sha256(&format!("{}{}", now(), uuid::Uuid::new_v4())))
            .await?;
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.updates.read().await.active_batch.is_none() {
                break;
            }
        }
        self.get_update_jobs().await
    }

    pub async fn get_update_jobs(&self) -> Result<Vec<AgentUpdateJob>, String> {
        let state = self.get_update_state().await?;
        Ok(state
            .active_batch
            .or(state.last_completed_batch)
            .map(|batch| {
                batch
                    .items
                    .into_iter()
                    .map(|item| AgentUpdateJob {
                        id: item.id,
                        agent_id: item.agent_id,
                        status: match item.status.as_str() {
                            "updated" | "up_to_date" => "success".to_string(),
                            "installing" => "updating".to_string(),
                            "preparing" => "queued".to_string(),
                            other => other.to_string(),
                        },
                        before: item.before,
                        after: item.after,
                        error: item.error,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn run_update_batch(self: Arc<Self>, batch_id: String) {
        if let Some(batch) = &mut self.updates.write().await.active_batch {
            if batch.id == batch_id {
                batch.status = if batch.cancel_requested {
                    "cancelling".to_string()
                } else {
                    "preparing".to_string()
                };
            }
        }
        let _ = self.persist_updates().await;
        let cancelled = self
            .updates
            .read()
            .await
            .active_batch
            .as_ref()
            .is_none_or(|batch| batch.cancel_requested);
        if !cancelled {
            let _ = self.refresh().await;
            let _ = self.check_updates(true).await;
        }
        let ids = self
            .updates
            .read()
            .await
            .active_batch
            .as_ref()
            .map(|batch| {
                batch
                    .items
                    .iter()
                    .map(|item| item.agent_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in ids {
            let cancelled = self
                .updates
                .read()
                .await
                .active_batch
                .as_ref()
                .is_none_or(|batch| batch.cancel_requested);
            if cancelled {
                break;
            }
            let selection_keys = self.selection_keys(&[id.clone()]).await.unwrap_or_default();
            let agent = self
                .get_inventory()
                .await
                .agents
                .into_iter()
                .find(|agent| agent.id == id);
            let (expected_selection, expected_target) = {
                let state = self.updates.read().await;
                let item = state
                    .active_batch
                    .as_ref()
                    .and_then(|batch| batch.items.iter().find(|item| item.agent_id == id));
                (
                    item.and_then(|item| item.selection_key.clone()),
                    item.and_then(|item| item.target_key.clone()),
                )
            };
            self.update_item(&id, |item| {
                item.started_at = Some(now());
                item.status = "preparing".to_string();
            })
            .await;
            let result = async {
                let agent = agent.ok_or_else(|| {
                    "Installation or launch configuration changed; refresh and retry".to_string()
                })?;
                if expected_selection
                    .as_ref()
                    .is_some_and(|key| selection_keys.get(&id) != Some(key))
                    || expected_target
                        .as_ref()
                        .is_some_and(|key| key != &update_target_key(&agent))
                {
                    return Err(
                        "Installation or launch configuration changed; refresh and retry"
                            .to_string(),
                    );
                }
                self.update_item(&id, |item| {
                    item.before = agent
                        .native_update
                        .as_ref()
                        .map(|update| update.version.clone())
                        .or_else(|| agent.version.clone());
                    item.status = "installing".to_string();
                })
                .await;
                let before = agent
                    .native_update
                    .as_ref()
                    .map(|update| update.version.clone())
                    .or_else(|| agent.version.clone());
                if agent.native_update.is_none()
                    && (agent.manager.is_none()
                        || agent.package.is_none()
                        || agent.latest.is_none())
                {
                    return Err(agent
                        .update_error
                        .clone()
                        .unwrap_or_else(|| "Verified updater unavailable".to_string()));
                }
                if agent
                    .native_update
                    .as_ref()
                    .is_some_and(|update| update.target == "wecode")
                    && agent.latest.is_none()
                {
                    return Err(agent
                        .update_error
                        .clone()
                        .unwrap_or_else(|| "WeCode latest version unavailable".to_string()));
                }
                if agent.latest.as_ref().is_some_and(|latest| {
                    Some(latest) == before.as_ref()
                        || before
                            .as_ref()
                            .is_some_and(|before| is_newer_stable_version(before, latest))
                }) {
                    self.update_item(&id, |item| {
                        item.status = "up_to_date".to_string();
                        item.after = before.clone();
                    })
                    .await;
                    return Ok(());
                }
                execute_agent_update(&agent).await?;
                self.update_item(&id, |item| item.status = "verifying".to_string())
                    .await;
                self.refresh().await.map_err(|error| error.to_string())?;
                let after_agent = self
                    .get_inventory()
                    .await
                    .agents
                    .into_iter()
                    .find(|row| row.id == id)
                    .ok_or_else(|| "Updated installation was not rediscovered".to_string())?;
                let after = after_agent
                    .native_update
                    .as_ref()
                    .map(|update| update.version.clone())
                    .or_else(|| after_agent.version.clone());
                let ready =
                    after_agent.runnable || after_agent.acp_status.as_deref() == Some("verified");
                if !ready
                    || after.is_none()
                    || before
                        .as_ref()
                        .zip(after.as_ref())
                        .is_some_and(|(before, after)| is_newer_stable_version(before, after))
                {
                    return Err(
                        "Updated installation failed version/runtime verification".to_string()
                    );
                }
                self.update_item(&id, |item| {
                    item.after = after.clone();
                    item.status = if item.before == item.after {
                        "up_to_date".to_string()
                    } else {
                        "updated".to_string()
                    };
                })
                .await;
                Ok::<(), String>(())
            }
            .await;
            if let Err(error) = result {
                let error = redact_sensitive_text(&error);
                self.update_item(&id, |item| {
                    item.status = "failed".to_string();
                    item.error = Some(error);
                })
                .await;
            }
            self.update_item(&id, |item| item.finished_at = Some(now()))
                .await;
        }
        {
            let mut state = self.updates.write().await;
            if let Some(mut batch) = state.active_batch.take() {
                for item in &mut batch.items {
                    if !update_terminal(&item.status) {
                        item.status = "cancelled".to_string();
                        item.finished_at = Some(now());
                    }
                }
                batch.status = "completed".to_string();
                batch.finished_at = Some(now());
                state.last_completed_batch = Some(batch);
            }
        }
        let _ = self.persist_updates().await;
    }

    async fn update_item(&self, id: &str, update: impl FnOnce(&mut AgentUpdateItem)) {
        {
            let mut state = self.updates.write().await;
            if let Some(item) = state
                .active_batch
                .as_mut()
                .and_then(|batch| batch.items.iter_mut().find(|item| item.agent_id == id))
            {
                update(item);
            }
        }
        let _ = self.persist_updates().await;
    }
}

fn update_terminal(status: &str) -> bool {
    !matches!(status, "queued" | "preparing" | "installing" | "verifying")
}
fn update_target_key(agent: &LocalAgent) -> String {
    sha256(
        &serde_json::to_string(&json!([
            agent.installation_id,
            agent.source,
            agent.prefix,
            agent.package,
            agent.native_update,
            agent.cli
        ]))
        .unwrap_or_default(),
    )
}

async fn run_file(
    command: &str,
    args: &[String],
    timeout_ms: u64,
    env: Option<&HashMap<String, String>>,
) -> Result<String, String> {
    let merged = merge_child_env(&std::env::vars().collect(), env);
    let output = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        Command::new(command)
            .args(args)
            .env_clear()
            .envs(merged)
            .output(),
    )
    .await
    .map_err(|_| format!("Process timed out after {timeout_ms}ms"))?
    .map_err(|error| redact_sensitive_text(&error.to_string()))?;
    if !output.status.success() {
        return Err(redact_sensitive_text(&String::from_utf8_lossy(
            if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            },
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn executable(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn discovery_dirs() -> Vec<PathBuf> {
    let mut dirs = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let home = home();
    dirs.extend([
        home.join("bin"),
        home.join(".local/bin"),
        home.join(".npm-global/bin"),
        home.join(".npm/bin"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".asdf/shims"),
        home.join(".local/share/mise/shims"),
        home.join(".wecode-cli/bin"),
        home.join(".cargo/bin"),
        home.join(".grok/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    let mut seen = HashSet::new();
    dirs.into_iter()
        .filter(|dir| seen.insert(dir.clone()))
        .collect()
}

async fn find_executable(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path)
            .await
            .then(|| path.to_string_lossy().to_string());
    }
    for dir in discovery_dirs() {
        let candidate = dir.join(name);
        if executable(&candidate).await {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn extract_version(raw: &str) -> Option<String> {
    Regex::new(r"\d+\.\d+\.\d+(?:[-+][\w.-]+)?")
        .ok()?
        .find(raw)
        .map(|value| value.as_str().to_string())
}

fn kimi_acp_args_from_help(help: &str) -> Option<Vec<String>> {
    for line in crate::buddy::shell_path::strip_terminal_decorations(help).lines() {
        let line = line.trim_start_matches([' ', '│', '|']);
        if Regex::new(r"^acp(?:\s|\[|$)").unwrap().is_match(line) {
            return Some(vec!["acp".to_string()]);
        }
        if Regex::new(r"^(?:-\w,\s*)?--acp(?:\s|[=,\[]|$)")
            .unwrap()
            .is_match(line)
        {
            return Some(vec!["--acp".to_string()]);
        }
    }
    None
}

fn native_update_command_from_help(help: &str) -> Option<String> {
    let mut commands = false;
    for line in crate::buddy::shell_path::strip_terminal_decorations(help).lines() {
        if line == "Commands:" {
            commands = true;
            continue;
        }
        if !commands {
            continue;
        }
        if !line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            break;
        }
        let Some(captures) = Regex::new(
            r"(?i)^\s+(update|upgrade)(?:\|(update|upgrade))?(?:\s+\[[^\]]+\])*\s{2,}(.+)$",
        )
        .unwrap()
        .captures(line) else {
            continue;
        };
        if Regex::new(r"(?i)\b(latest|version|binary|self|updates)\b")
            .unwrap()
            .is_match(&captures[3])
        {
            return Some(captures[1].to_string());
        }
    }
    None
}

async fn detect_native_update(
    id: &str,
    cli: &Launcher,
    native: bool,
    real_path: &str,
) -> Option<NativeAgentUpdate> {
    if !native || cli.command.is_empty() {
        return None;
    }
    let wecode = Path::new(&cli.command)
        .file_name()
        .map(|name| name.to_string_lossy())
        == Some("wecode".into());
    if !wecode
        && !id.starts_with("wecode")
        && detect_source(None, real_path, Some(&cli.command)).await.0 != "unknown"
    {
        return None;
    }
    let args = cli.args.clone().unwrap_or_default();
    let prefix = if wecode
        && args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "codex" | "opencode"))
    {
        vec![args[0].clone()]
    } else {
        Vec::new()
    };
    let help_args = if wecode {
        vec!["help".to_string()]
    } else {
        [prefix.clone(), vec!["--help".to_string()]].concat()
    };
    let help = run_file(&cli.command, &help_args, 5000, Some(&cli.env))
        .await
        .ok()?;
    let update = native_update_command_from_help(&help)?;
    if wecode && !help.contains("--cli-only") {
        return None;
    }
    let version_args = if wecode {
        vec!["version".to_string()]
    } else {
        [prefix.clone(), vec!["--version".to_string()]].concat()
    };
    let raw = run_file(&cli.command, &version_args, 3000, Some(&cli.env))
        .await
        .ok()?;
    let version = extract_version(&raw).unwrap_or(raw);
    Some(NativeAgentUpdate {
        command: cli.command.clone(),
        args: if wecode {
            vec![update, "--cli-only".to_string()]
        } else {
            [prefix, vec![update]].concat()
        },
        version_args,
        version,
        target: if wecode {
            "wecode".to_string()
        } else {
            "agent".to_string()
        },
        shared_key: if wecode {
            Some(format!(
                "{}:{}",
                real_path,
                cli.env
                    .get("WECODE_INSTALL_BRANCH")
                    .cloned()
                    .or_else(|| std::env::var("WECODE_INSTALL_BRANCH").ok())
                    .unwrap_or_else(|| "master".to_string())
            ))
        } else {
            None
        },
    })
}

async fn detect_source(
    npm: Option<&str>,
    real_path: &str,
    _command: Option<&str>,
) -> (String, Option<String>, Option<String>, Option<String>) {
    if let Ok(captures) = Regex::new(r"^(.*)/(?:Cellar|Caskroom)/([^/]+)/") {
        if let Some(captures) = captures.captures(real_path) {
            return (
                "brew".to_string(),
                None,
                Some(
                    PathBuf::from(&captures[1])
                        .join("bin/brew")
                        .to_string_lossy()
                        .to_string(),
                ),
                Some(captures[2].to_string()),
            );
        }
    }
    if let Some(npm) = npm {
        if let Some(index) = real_path.find(&format!("/lib/node_modules/{npm}")) {
            let prefix = real_path[..index].to_string();
            return (
                "npm".to_string(),
                Some(prefix.clone()),
                Some(
                    PathBuf::from(prefix)
                        .join("bin/npm")
                        .to_string_lossy()
                        .to_string(),
                ),
                Some(npm.to_string()),
            );
        }
    }
    let source = if real_path.contains("/.bun/") {
        "bun"
    } else if real_path.contains("/pnpm/") {
        "pnpm"
    } else if real_path.contains("/.local/share/uv/tools/") {
        "uv"
    } else if real_path.contains("/pipx/venvs/") {
        "pipx"
    } else {
        "unknown"
    };
    let manager = match source {
        "bun" | "pnpm" | "uv" | "pipx" => find_executable(source).await,
        _ => None,
    };
    let package = if matches!(source, "uv" | "pipx") {
        Some("kimi-cli".to_string())
    } else {
        npm.map(String::from)
    };
    (source.to_string(), None, manager, package)
}

fn parse_cursor_models(output: &str) -> Vec<AcpModelInfo> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for line in crate::buddy::shell_path::strip_terminal_decorations(output).lines() {
        let Some(captures) = Regex::new(r"^\s*([\w][\w./:+-]*)\s+-\s+(.+?)\s*$")
            .unwrap()
            .captures(line)
        else {
            continue;
        };
        let id = captures[1].to_string();
        let name = Regex::new(r"\s+\((?:current|default)(?:,\s*(?:current|default))*\)$")
            .unwrap()
            .replace_all(&captures[2], "")
            .trim()
            .to_string();
        if !name.is_empty() && seen.insert(id.clone()) {
            models.push(AcpModelInfo { id, name });
        }
    }
    models
}

fn parse_agy_models(output: &str) -> Vec<AcpModelInfo> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    for line in crate::buddy::shell_path::strip_terminal_decorations(output).lines() {
        let mut parts = line.split('\t');
        let id = parts.next().unwrap_or("").trim().to_string();
        let name = parts.collect::<Vec<_>>().join(" ").trim().to_string();
        if !name.is_empty()
            && Regex::new(r"^[a-zA-Z0-9][\w./:+-]*$")
                .unwrap()
                .is_match(&id)
            && !matches!(id.to_lowercase().as_str(), "id" | "model_id")
            && seen.insert(id.clone())
        {
            models.push(AcpModelInfo { id, name });
        }
    }
    models
}

fn parse_kimi_models(output: &str) -> Result<Vec<AcpModelInfo>, String> {
    let value: Value = serde_json::from_str(output)
        .map_err(|_| "Kimi model list returned invalid JSON".to_string())?;
    let models = value
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| "Kimi model list returned an unsupported data format".to_string())?;
    Ok(models
        .iter()
        .filter(|(id, _)| !id.trim().is_empty())
        .map(|(id, model)| AcpModelInfo {
            id: id.clone(),
            name: model
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(id)
                .to_string(),
        })
        .collect())
}

pub fn parse_acp_auth_failure(error: &str) -> Option<HashMap<String, String>> {
    if !Regex::new(r"(?i)(authentication(?:\s+token)?|token\s+has\s+expired|not\s+logged\s+in|please\s+(?:run|login|sign\s*in)|(?:^|[^\w])login(?:[^\w]|$)|sign[\s-]?in\s+required|unauthorized)").unwrap().is_match(error) { return None; }
    let command = Regex::new(r"(?i)\b((?:wecode|claude|codex|agy|antigravity|opencode|kimi|dsh|gemini|codebuddy|cbc|cursor-agent|qwen|copilot|goose|droid|qoder|qodercn|qodercli|qoderclicn|cline|omp|pi|hermes|mimo|mimocode|mcode|crush)(?:\s+[a-z0-9_-]+)?\s+login)\b").unwrap()
        .captures(error).and_then(|captures| captures.get(1)).map(|value| value.as_str().trim().to_string());
    Some(
        command
            .map(|command| [("command".to_string(), command)].into_iter().collect())
            .unwrap_or_default(),
    )
}

async fn latest_agent_version(agent: &LocalAgent) -> Result<Option<String>, String> {
    if let Some(update) = &agent.native_update {
        if update.target == "wecode" {
            return latest_wecode_version(
                &update.command,
                agent.cli.as_ref().map(|launcher| &launcher.env),
            )
            .await
            .map(Some);
        }
    }
    match agent.source.as_str() {
        "brew" => {
            let manager = agent
                .manager
                .as_ref()
                .ok_or_else(|| "Homebrew manager unavailable".to_string())?;
            let package = agent
                .package
                .as_ref()
                .ok_or_else(|| "Homebrew package unavailable".to_string())?;
            let raw = run_file(
                manager,
                &["info".to_string(), "--json=v2".to_string(), package.clone()],
                30_000,
                None,
            )
            .await?;
            let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            Ok(value
                .pointer("/formulae/0/versions/stable")
                .or_else(|| value.pointer("/casks/0/version"))
                .and_then(Value::as_str)
                .map(String::from))
        }
        "npm" | "pnpm" | "bun" | "self" => {
            let package = agent
                .package
                .as_ref()
                .ok_or_else(|| "npm package unavailable".to_string())?;
            let response = reqwest::Client::new()
                .get(format!(
                    "https://registry.npmjs.org/{}/latest",
                    urlencoding(package)
                ))
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("Version registry returned {}", response.status()));
            }
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            Ok(value
                .get("version")
                .and_then(Value::as_str)
                .map(String::from))
        }
        "uv" | "pipx" => {
            let package = agent
                .package
                .as_ref()
                .ok_or_else(|| "Python package unavailable".to_string())?;
            let response = reqwest::Client::new()
                .get(format!("https://pypi.org/pypi/{package}/json"))
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|error| error.to_string())?;
            if !response.status().is_success() {
                return Err(format!("Version registry returned {}", response.status()));
            }
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            Ok(value
                .pointer("/info/version")
                .and_then(Value::as_str)
                .map(String::from))
        }
        _ => Ok(None),
    }
}

fn urlencoding(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

async fn latest_wecode_version(
    command: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<String, String> {
    let entry = tokio::fs::canonicalize(command)
        .await
        .map_err(|error| error.to_string())?;
    if tokio::fs::metadata(&entry)
        .await
        .map_err(|error| error.to_string())?
        .len()
        > 8192
    {
        return Err("Unsupported WeCode launcher".to_string());
    }
    let shim = tokio::fs::read_to_string(&entry)
        .await
        .map_err(|error| error.to_string())?;
    let relative = if shim.contains("$SCRIPT_DIR/../wecode-cli/bin/cli.js") {
        "../wecode-cli/bin/cli.js"
    } else {
        "cli.js"
    };
    let bundle = tokio::fs::canonicalize(entry.parent().unwrap_or(Path::new(".")).join(relative))
        .await
        .map_err(|error| error.to_string())?;
    if tokio::fs::metadata(&bundle)
        .await
        .map_err(|error| error.to_string())?
        .len()
        > 4 * 1024 * 1024
    {
        return Err("Unsupported WeCode bundle size".to_string());
    }
    let source = tokio::fs::read_to_string(bundle)
        .await
        .map_err(|error| error.to_string())?;
    if !Regex::new(r"/\*\s*WeCoder CLI v\d+\.\d+\.\d+\s*\|")
        .unwrap()
        .is_match(&source)
    {
        return Err("Unrecognized WeCode bundle".to_string());
    }
    let endpoint = Regex::new(r#"\b([\w$]+)\s*=\s*(\"https://git\.intra\.weibo\.com/api/v4/[^\"\n]+/repository/files/dist%2Fcli\.js/raw\")"#).unwrap().captures(&source).ok_or_else(|| "WeCode update source unavailable".to_string())?;
    let variable = regex::escape(&endpoint[1]);
    let request = Regex::new(&format!(
        r#"fetch\(\s*`\$\{{{variable}\}}\?ref=[\s\S]{{0,500}}?"PRIVATE-TOKEN"\s*:\s*([\w$]+)"#
    ))
    .unwrap()
    .captures(&source);
    let token_name = request
        .and_then(|captures| captures.get(1).map(|value| value.as_str().to_string()))
        .ok_or_else(|| "WeCode update authentication unavailable".to_string())?;
    let token_pattern = Regex::new(&format!(
        r#"\b{}\s*=\s*(\"(?:[^\"\\\\]|\\\\.)*\")"#,
        regex::escape(&token_name)
    ))
    .unwrap();
    let token = token_pattern
        .captures(&source)
        .and_then(|captures| captures.get(1))
        .ok_or_else(|| "WeCode update authentication unavailable".to_string())?;
    let credential: String = serde_json::from_str(token.as_str())
        .map_err(|_| "Unsupported WeCode update authentication".to_string())?;
    let endpoint_url: String =
        serde_json::from_str(&endpoint[2]).map_err(|error| error.to_string())?;
    let branch = env
        .and_then(|env| env.get("WECODE_INSTALL_BRANCH"))
        .cloned()
        .or_else(|| std::env::var("WECODE_INSTALL_BRANCH").ok())
        .unwrap_or_else(|| "master".to_string());
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?
        .get(format!("{endpoint_url}?ref={branch}"))
        .header("PRIVATE-TOKEN", credential)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|_| "Cannot reach WeCode update source".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "WeCode update source returned {}",
            response.status()
        ));
    }
    let content = response.text().await.map_err(|error| error.to_string())?;
    if content.len() > 4 * 1024 * 1024 {
        return Err("Invalid WeCode update version".to_string());
    }
    Regex::new(r"/\*\s*WeCoder CLI v(\d+\.\d+\.\d+)\s*\|")
        .unwrap()
        .captures(&content)
        .map(|captures| captures[1].to_string())
        .ok_or_else(|| "Invalid WeCode update version".to_string())
}

async fn execute_agent_update(agent: &LocalAgent) -> Result<(), String> {
    let latest = agent.latest.as_ref();
    let args = if let Some(update) = &agent.native_update {
        update.args.clone()
    } else {
        match agent.source.as_str() {
            "brew" => [
                "upgrade".to_string(),
                if agent.real_path.contains("/Caskroom/") {
                    "--cask".to_string()
                } else {
                    "".to_string()
                },
                agent.package.clone().unwrap_or_default(),
            ]
            .into_iter()
            .filter(|arg| !arg.is_empty())
            .collect(),
            "self" => vec!["update".to_string()],
            "uv" => vec![
                "tool".to_string(),
                "upgrade".to_string(),
                agent.package.clone().unwrap_or_default(),
            ],
            "pipx" => vec![
                "upgrade".to_string(),
                agent.package.clone().unwrap_or_default(),
            ],
            "pnpm" | "bun" => vec![
                "add".to_string(),
                "-g".to_string(),
                format!(
                    "{}@{}",
                    agent.package.clone().unwrap_or_default(),
                    latest.cloned().unwrap_or_default()
                ),
            ],
            _ => vec![
                "install".to_string(),
                "-g".to_string(),
                "--prefix".to_string(),
                agent.prefix.clone().unwrap_or_default(),
                format!(
                    "{}@{}",
                    agent.package.clone().unwrap_or_default(),
                    latest.cloned().unwrap_or_default()
                ),
            ],
        }
    };
    let command = agent
        .native_update
        .as_ref()
        .map(|update| update.command.clone())
        .or_else(|| agent.manager.clone())
        .ok_or_else(|| "No newer stable version available".to_string())?;
    let env = agent
        .native_update
        .as_ref()
        .and_then(|_| agent.cli.as_ref())
        .map(|launcher| launcher.env.clone());
    let merged = merge_child_env(&std::env::vars().collect(), env.as_ref());
    let child = Command::new(&command)
        .args(&args)
        .env_clear()
        .envs(merged)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    let output = tokio::time::timeout(Duration::from_secs(600), child.wait_with_output())
        .await
        .map_err(|_| "Update timed out".to_string())?
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(
            redact_sensitive_text(&String::from_utf8_lossy(&output.stderr))
                .chars()
                .take(2000)
                .collect(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_only_stable_versions() {
        assert!(is_newer_stable_version("2.0.19", "2.0.5"));
        assert!(!is_newer_stable_version("2.0.5", "2.0.19"));
        assert!(!is_newer_stable_version("2.0.19-beta", "2.0.5"));
    }

    #[test]
    fn detects_acp_auth_failure_and_login_command() {
        let result = parse_acp_auth_failure("Authentication required; run codex login").unwrap();
        assert_eq!(
            result.get("command").map(String::as_str),
            Some("codex login")
        );
        assert!(parse_acp_auth_failure("session not found").is_none());
    }

    #[test]
    fn parses_cursor_and_kimi_model_catalogs() {
        let cursor = parse_cursor_models("model-a - Model A (current)\nmodel-b - Model B\n");
        assert_eq!(cursor.len(), 2);
        assert_eq!(cursor[0].name, "Model A");
        let kimi =
            parse_kimi_models(r#"{"models":{"kimi-k2":{"display_name":"Kimi K2"}}}"#).unwrap();
        assert_eq!(kimi[0].id, "kimi-k2");
        assert_eq!(kimi[0].name, "Kimi K2");
    }

    #[test]
    fn native_update_help_skips_unrelated_commands() {
        let help =
            "Commands:\n  run       Run the agent\n  upgrade   Upgrade to the latest binary\n";
        assert_eq!(
            native_update_command_from_help(help).as_deref(),
            Some("upgrade")
        );
    }

    #[test]
    fn protocol_readiness_is_scoped_to_selected_protocol() {
        let agent = LocalAgent {
            runnable: true,
            acp_status: Some("failed".to_string()),
            ..Default::default()
        };
        assert!(agent_protocol_ready(&agent, "cli"));
        assert!(!agent_protocol_ready(&agent, "acp"));
    }
}
