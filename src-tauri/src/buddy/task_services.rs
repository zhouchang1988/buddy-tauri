//! Task-scoped background service management, port of the Electron edition's
//! `src/main/buddy/task-services.ts`.
//!
//! The Electron edition ships two dependency-free Node scripts
//! (`service-supervisor.cjs` / `service-client.cjs`) and spawns them with
//! `ELECTRON_RUN_AS_NODE=1 process.execPath`. A Tauri binary is not a Node
//! runtime, so this port rewrites both in Rust and exposes them as hidden
//! subcommands of the app binary itself:
//!
//! - `buddy __service_supervisor` — reads a JSON config from stdin, spawns the
//!   service in its own process group, and serves an authenticated Unix-socket
//!   control channel (`/status`, `/stop`). Only this supervisor signals the
//!   process group it created (never a saved PID).
//! - `buddy __service_client` — the short-lived CLI actor shells invoke via
//!   `$BUDDY_SERVICE_CLI` (a generated wrapper script that execs the app
//!   binary with this subcommand).
//!
//! The manager below mirrors `TaskServiceManager`: per-task service records
//! under `dataRoot/runtime/services/`, lease-tracked per-run broker servers,
//! cleanup on task completion/cancellation, and startup recovery.

use crate::buddy::store::{BuddyStore, EventInput, StoreError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};

// ---------------------------------------------------------------------------
// Wire types + read-time validation (zod schema equivalents from schemas.ts)
// ---------------------------------------------------------------------------

fn service_name_valid(name: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-zA-Z0-9._-]{1,64}$").unwrap())
        .is_match(name)
}

/// A persisted `<name>.service.json` record, port of `taskServiceSchema`
/// (discriminated union on `owner`).
#[derive(Debug, Clone)]
pub enum TaskServiceRecord {
    Buddy {
        id: String,
        name: String,
        task_id: String,
        workspace_key: String,
        created_at: String,
        keep_reason: Option<String>,
        command: Vec<String>,
        cwd: String,
        token: String,
        socket: String,
        status_path: String,
        log_path: String,
    },
    External {
        id: String,
        name: String,
        task_id: String,
        workspace_key: String,
        created_at: String,
        keep_reason: Option<String>,
        pid: u32,
    },
}

impl TaskServiceRecord {
    pub fn name(&self) -> &str {
        match self {
            TaskServiceRecord::Buddy { name, .. } => name,
            TaskServiceRecord::External { name, .. } => name,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            TaskServiceRecord::Buddy { id, .. } => id,
            TaskServiceRecord::External { id, .. } => id,
        }
    }

    pub fn keep_reason(&self) -> Option<&str> {
        match self {
            TaskServiceRecord::Buddy { keep_reason, .. } => keep_reason.as_deref(),
            TaskServiceRecord::External { keep_reason, .. } => keep_reason.as_deref(),
        }
    }

    pub fn owner(&self) -> &'static str {
        match self {
            TaskServiceRecord::Buddy { .. } => "buddy",
            TaskServiceRecord::External { .. } => "external",
        }
    }

    pub fn with_keep_reason(&self, reason: Option<String>) -> Self {
        match self.clone() {
            TaskServiceRecord::Buddy {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                command,
                cwd,
                token,
                socket,
                status_path,
                log_path,
                ..
            } => TaskServiceRecord::Buddy {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason: reason,
                command,
                cwd,
                token,
                socket,
                status_path,
                log_path,
            },
            TaskServiceRecord::External {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                pid,
                ..
            } => TaskServiceRecord::External {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason: reason,
                pid,
            },
        }
    }

    /// Serialize in the key order the Electron edition's spreads produce
    /// (`{...base, owner, ...}`).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let (id, name, task_id, workspace_key, created_at, keep_reason) = match self {
            TaskServiceRecord::Buddy {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason,
                ..
            }
            | TaskServiceRecord::External {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason,
                ..
            } => (id, name, task_id, workspace_key, created_at, keep_reason),
        };
        map.insert("id".to_string(), Value::from(id.clone()));
        map.insert("name".to_string(), Value::from(name.clone()));
        map.insert("task_id".to_string(), Value::from(task_id.clone()));
        map.insert(
            "workspace_key".to_string(),
            Value::from(workspace_key.clone()),
        );
        map.insert("created_at".to_string(), Value::from(created_at.clone()));
        if let Some(reason) = keep_reason {
            map.insert("keep_reason".to_string(), Value::from(reason.clone()));
        }
        match self {
            TaskServiceRecord::Buddy {
                command,
                cwd,
                token,
                socket,
                status_path,
                log_path,
                ..
            } => {
                map.insert("owner".to_string(), Value::from("buddy"));
                map.insert(
                    "command".to_string(),
                    Value::Array(command.iter().cloned().map(Value::from).collect()),
                );
                map.insert("cwd".to_string(), Value::from(cwd.clone()));
                map.insert("token".to_string(), Value::from(token.clone()));
                map.insert("socket".to_string(), Value::from(socket.clone()));
                map.insert("status_path".to_string(), Value::from(status_path.clone()));
                map.insert("log_path".to_string(), Value::from(log_path.clone()));
            }
            TaskServiceRecord::External { pid, .. } => {
                map.insert("owner".to_string(), Value::from("external"));
                map.insert("pid".to_string(), Value::from(*pid));
            }
        }
        Value::Object(map)
    }
}

fn json_string(map: &Map<String, Value>, key: &str) -> Result<String, String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("service record field `{key}` must be a string"))
}

/// `taskServiceSchema.parse` (read-time validation, never on write).
pub fn parse_task_service_record(input: &Value) -> Result<TaskServiceRecord, String> {
    let map = input
        .as_object()
        .ok_or_else(|| "service record must be an object".to_string())?;
    let id = json_string(map, "id")?;
    if uuid::Uuid::parse_str(&id).is_err() {
        return Err("service record field `id` must be a uuid".to_string());
    }
    let name = json_string(map, "name")?;
    if !service_name_valid(&name) {
        return Err(format!("invalid service name `{name}`"));
    }
    let task_id = json_string(map, "task_id")?;
    let workspace_key = json_string(map, "workspace_key")?;
    let created_at = json_string(map, "created_at")?;
    let keep_reason = match map.get("keep_reason") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let reason = value
                .as_str()
                .ok_or_else(|| "`keep_reason` must be a string".to_string())?;
            if reason.is_empty() {
                return Err("`keep_reason` must not be empty".to_string());
            }
            Some(reason.to_string())
        }
    };
    match map.get("owner").and_then(Value::as_str) {
        Some("buddy") => {
            let command = map
                .get("command")
                .and_then(Value::as_array)
                .ok_or_else(|| "`command` must be an array".to_string())?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "`command` entries must be strings".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if command.is_empty() {
                return Err("`command` must not be empty".to_string());
            }
            let token = json_string(map, "token")?;
            if token.len() < 32 {
                return Err("`token` must be at least 32 characters".to_string());
            }
            Ok(TaskServiceRecord::Buddy {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason,
                command,
                cwd: json_string(map, "cwd")?,
                token,
                socket: json_string(map, "socket")?,
                status_path: json_string(map, "status_path")?,
                log_path: json_string(map, "log_path")?,
            })
        }
        Some("external") => {
            let pid = map
                .get("pid")
                .and_then(Value::as_u64)
                .filter(|pid| *pid > 0)
                .ok_or_else(|| "`pid` must be a positive integer".to_string())?;
            Ok(TaskServiceRecord::External {
                id,
                name,
                task_id,
                workspace_key,
                created_at,
                keep_reason,
                pid: pid as u32,
            })
        }
        _ => Err("service record `owner` must be `buddy` or `external`".to_string()),
    }
}

pub const SERVICE_TERMINAL_STATUSES: [&str; 3] = ["stopped", "exited", "failed"];

fn terminal(status: &str) -> bool {
    SERVICE_TERMINAL_STATUSES.contains(&status)
}

const SERVICE_STATUS_VALUES: [&str; 6] = [
    "starting",
    "running",
    "stopped",
    "exited",
    "failed",
    "cleanup_failed",
];

/// `serviceStatusSchema.parse`: validates and returns the original object.
pub fn parse_service_status(input: &Value) -> Result<Map<String, Value>, String> {
    let map = input
        .as_object()
        .ok_or_else(|| "service status must be an object".to_string())?;
    let status = map
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| "service status field `status` must be a string".to_string())?;
    if !SERVICE_STATUS_VALUES.contains(&status) {
        return Err(format!("unknown service status `{status}`"));
    }
    Ok(map.clone())
}

/// A validated `serviceRequestSchema` payload from an actor shell.
#[derive(Debug, Clone, Default)]
pub struct ServiceRequest {
    pub action: String,
    pub name: Option<String>,
    pub command: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub keep_reason: Option<String>,
    pub pid: Option<u32>,
}

pub fn parse_service_request(input: &Value) -> Result<ServiceRequest, String> {
    let map = input
        .as_object()
        .ok_or_else(|| "service request must be an object".to_string())?;
    let action = map
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "`action` is required".to_string())?;
    if !["start", "list", "stop", "keep", "external"].contains(&action) {
        return Err(format!("unknown service action `{action}`"));
    }
    let name = match map.get("name") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let name = value
                .as_str()
                .ok_or_else(|| "`name` must be a string".to_string())?;
            if !service_name_valid(name) {
                return Err(format!("invalid service name `{name}`"));
            }
            Some(name.to_string())
        }
    };
    let command = match map.get("command") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let items = value
                .as_array()
                .ok_or_else(|| "`command` must be an array".to_string())?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "`command` entries must be strings".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            if items.is_empty() {
                return Err("`command` must not be empty".to_string());
            }
            Some(items)
        }
    };
    let cwd = match map.get("cwd") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| "`cwd` must be a string".to_string())?
                .to_string(),
        ),
    };
    let keep_reason = match map.get("keepReason") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let reason = value
                .as_str()
                .ok_or_else(|| "`keepReason` must be a string".to_string())?
                .trim()
                .to_string();
            if reason.is_empty() {
                return Err("`keepReason` must not be empty".to_string());
            }
            Some(reason)
        }
    };
    let pid = match map.get("pid") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|pid| *pid > 0)
                .ok_or_else(|| "`pid` must be a positive integer".to_string())?
                as u32,
        ),
    };
    Ok(ServiceRequest {
        action: action.to_string(),
        name,
        command,
        cwd,
        keep_reason,
        pid,
    })
}

/// `serviceTaskManifestSchema` (`task.json` in a service directory).
pub struct ServiceTaskManifest {
    pub task_id: String,
    pub workspace_key: String,
}

pub fn parse_service_task_manifest(input: &Value) -> Result<ServiceTaskManifest, String> {
    let map = input
        .as_object()
        .ok_or_else(|| "service task manifest must be an object".to_string())?;
    Ok(ServiceTaskManifest {
        task_id: json_string(map, "task_id")?,
        workspace_key: json_string(map, "workspace_key")?,
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn sha24(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())[..24].to_string()
}

fn random_token() -> String {
    // 32 random bytes hex-encoded (64 chars), like Node's
    // `randomBytes(32).toString('hex')`.
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

/// TS `quote`: wrap in single quotes with `'\''` escaping.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn utc_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn atomic_json_sync(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        uuid::Uuid::new_v4().simple().to_string()
    ));
    write_file_mode(&tmp, serde_json::to_string_pretty(value).unwrap().as_bytes(), 0o600)
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename to {}: {e}", path.display()))
}

async fn atomic_json(path: &Path, value: &Value) -> Result<(), String> {
    let path = path.to_path_buf();
    let value = value.clone();
    tokio::task::spawn_blocking(move || atomic_json_sync(&path, &value))
        .await
        .map_err(|e| e.to_string())?
}

fn write_file_mode(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?
        .write_all(contents)
}

fn open_append_mode(path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode)
        .open(path)
}

/// `mkdtemp(join(tmpdir(), prefix))` equivalent (mode 0700).
fn create_temp_dir(prefix: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    for _ in 0..16 {
        let candidate = std::env::temp_dir().join(format!(
            "{prefix}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        match std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&candidate)
        {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("mkdtemp {prefix}: {error}")),
        }
    }
    Err(format!("mkdtemp {prefix}: could not allocate a unique directory"))
}

fn is_not_found(error: &StoreError) -> bool {
    match error {
        StoreError::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP/1.x over Unix sockets (no external HTTP dependency)
// ---------------------------------------------------------------------------

/// Async POST with a bearer token against a Unix-socket server; resolves the
/// parsed JSON body or an error carrying the server's `error` field (port of
/// the manager's `socketRequest`).
async fn socket_request(
    socket_path: &str,
    token: &str,
    path: &str,
    body: Option<&Value>,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    let work = async {
        let mut stream = tokio::net::UnixStream::connect(socket_path)
            .await
            .map_err(|e| e.to_string())?;
        let payload = body.map(|b| serde_json::to_string(b).unwrap()).unwrap_or_default();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nauthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "malformed supervisor response".to_string())?;
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let value: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or(format!("Supervisor HTTP {status}")));
        }
        Ok(value)
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err("Service supervisor did not respond".to_string()),
    }
}

/// Read one HTTP request (headers + Content-Length body) from a stream.
async fn read_http_request<S>(stream: &mut S, max_body: usize) -> Result<(String, String, HashMap<String, String>, Vec<u8>), String>
where
    S: AsyncReadExt + Unpin,
{
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&raw, b"\r\n\r\n") {
            break pos;
        }
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before headers".to_string());
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > 65536 + 4096 {
            return Err("request headers too large".to_string());
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    if content_length > max_body {
        return Err("Service request is too large".to_string());
    }
    let mut body = raw[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_length);
    Ok((method, path, headers, body))
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn write_http_response<S>(stream: &mut S, status: u16, body: &str) -> Result<(), String>
where
    S: AsyncWriteExt + Unpin,
{
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream.shutdown().await.map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Service supervisor (Rust rewrite of service-supervisor.cjs)
// ---------------------------------------------------------------------------

/// Config the supervisor reads as JSON from stdin.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorConfig {
    pub socket: String,
    pub token: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub log_path: String,
    pub status_path: String,
}

/// The supervisor's live status, serialized to `status_path` atomically.
#[derive(Debug, Clone, Default)]
struct SupervisorState {
    status: String,
    supervisor_pid: Option<u32>,
    pid: Option<u32>,
    pid_present: bool,
    exit_code: Option<i32>,
    exit_code_present: bool,
    signal: Option<String>,
    signal_present: bool,
    reason: Option<String>,
    error: Option<String>,
    ended_at: Option<String>,
}

impl SupervisorState {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("status".to_string(), Value::from(self.status.clone()));
        if let Some(pid) = self.supervisor_pid {
            map.insert("supervisor_pid".to_string(), Value::from(pid));
        }
        if self.pid_present {
            map.insert(
                "pid".to_string(),
                self.pid.map(Value::from).unwrap_or(Value::Null),
            );
        }
        if self.exit_code_present {
            map.insert(
                "exit_code".to_string(),
                self.exit_code.map(Value::from).unwrap_or(Value::Null),
            );
        }
        if self.signal_present {
            map.insert(
                "signal".to_string(),
                self.signal
                    .clone()
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
        }
        if let Some(reason) = &self.reason {
            map.insert("reason".to_string(), Value::from(reason.clone()));
        }
        if let Some(error) = &self.error {
            map.insert("error".to_string(), Value::from(error.clone()));
        }
        if let Some(ended_at) = &self.ended_at {
            map.insert("ended_at".to_string(), Value::from(ended_at.clone()));
        }
        Value::Object(map)
    }
}

/// Is any process in the child's process group still alive
/// (`process.kill(-pid, 0)` in Node)?
fn process_group_alive(pid: Option<u32>) -> bool {
    let Some(pid) = pid else { return false };
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn signal_process_group(pid: Option<u32>, signal: &str) {
    let Some(pid) = pid else { return };
    let _ = std::process::Command::new("kill")
        .args(["-s", signal, "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

struct SupervisorShared {
    state: StdMutex<SupervisorState>,
    stop_lock: Mutex<()>,
    child_pid: StdMutex<Option<u32>>,
    status_path: String,
    quit: Notify,
}

impl SupervisorShared {
    fn save(&self) -> Result<(), String> {
        let snapshot = self.state.lock().unwrap().to_json();
        atomic_json_sync(Path::new(&self.status_path), &snapshot)
    }

    /// Port of the cjs `stop(reason)`: SIGTERM the owned process group,
    /// escalate to SIGKILL, verify the group exited, then persist the
    /// terminal status. Concurrent callers are serialized.
    async fn stop(&self, reason: &str) -> Result<Value, String> {
        let _guard = self.stop_lock.lock().await;
        let pid = *self.child_pid.lock().unwrap();
        {
            let current = self.state.lock().unwrap();
            if terminal(&current.status) && !process_group_alive(pid) {
                return Ok(current.to_json());
            }
        }
        signal_process_group(pid, "SIGTERM");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        while process_group_alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        if process_group_alive(pid) {
            signal_process_group(pid, "SIGKILL");
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            while process_group_alive(pid) && std::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
        if process_group_alive(pid) {
            let message = "Service process group did not exit".to_string();
            {
                let mut state = self.state.lock().unwrap();
                state.status = "cleanup_failed".to_string();
                state.error = Some(message.clone());
            }
            let _ = self.save();
            return Err(message);
        }
        {
            let mut state = self.state.lock().unwrap();
            state.status = if reason == "exited" {
                "exited".to_string()
            } else {
                "stopped".to_string()
            };
            state.reason = Some(reason.to_string());
            state.ended_at = Some(utc_now());
        }
        self.save()?;
        Ok(self.state.lock().unwrap().to_json())
    }
}

/// Run a service supervisor until the service exits or a stop request or
/// signal arrives. Returns when the control channel has been torn down.
pub async fn run_service_supervisor(config: SupervisorConfig) -> Result<(), String> {
    if config.command.is_empty() {
        return Err("supervisor command is empty".to_string());
    }
    if let Some(parent) = Path::new(&config.status_path).parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| e.to_string())?;
    }
    let shared = Arc::new(SupervisorShared {
        state: StdMutex::new(SupervisorState {
            status: "starting".to_string(),
            supervisor_pid: Some(std::process::id()),
            pid: None,
            pid_present: true,
            ..Default::default()
        }),
        stop_lock: Mutex::new(()),
        child_pid: StdMutex::new(None),
        status_path: config.status_path.clone(),
        quit: Notify::new(),
    });
    shared.save()?;

    let listener = tokio::net::UnixListener::bind(&config.socket)
        .map_err(|e| format!("bind {}: {e}", config.socket))?;
    let socket_dir = Path::new(&config.socket)
        .parent()
        .map(Path::to_path_buf);

    // Spawn the service in its own process group so the supervisor — and only
    // the supervisor — can signal every process it forked.
    let log = open_append_mode(Path::new(&config.log_path), 0o600).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .current_dir(&config.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    // The service inherits the actor run env minus Buddy's service-channel
    // variables (cjs: delete every BUDDY_SERVICE_* key).
    let env: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.starts_with("BUDDY_SERVICE_"))
        .collect();
    command.env_clear().envs(env);
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            {
                let mut state = shared.state.lock().unwrap();
                state.status = "failed".to_string();
                state.error = Some(error.to_string());
            }
            let _ = shared.save();
            if let Some(dir) = &socket_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(format!("spawn {}: {error}", config.command[0]));
        }
    };
    let pid = child.id();
    *shared.child_pid.lock().unwrap() = pid;
    {
        let mut state = shared.state.lock().unwrap();
        state.status = "running".to_string();
        state.pid = pid;
    }
    shared.save()?;

    // Child exit watcher: an exited leader must not leave children running in
    // its owned group (cjs `child.once('exit', ...)` → `stop('exited')`).
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            {
                let mut state = shared.state.lock().unwrap();
                match &status {
                    Ok(status) => {
                        use std::os::unix::process::ExitStatusExt;
                        state.exit_code = status.code();
                        state.exit_code_present = true;
                        state.signal = status.signal().map(supervisor_signal_name);
                        state.signal_present = true;
                    }
                    Err(_) => {
                        state.exit_code_present = true;
                        state.signal_present = true;
                    }
                }
            }
            let _ = shared.stop("exited").await;
            shared.quit.notify_one();
        });
    }

    // SIGTERM/SIGINT/SIGHUP: stop the owned group, then exit (cjs
    // `process.on(signal, ...)`).
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut terminate = signal(SignalKind::terminate()).ok();
            let mut interrupt = signal(SignalKind::interrupt()).ok();
            let mut hangup = signal(SignalKind::hangup()).ok();
            let name = tokio::select! {
                _ = async { if let Some(s) = terminate.as_mut() { s.recv().await } else { std::future::pending().await } } => "SIGTERM",
                _ = async { if let Some(s) = interrupt.as_mut() { s.recv().await } else { std::future::pending().await } } => "SIGINT",
                _ = async { if let Some(s) = hangup.as_mut() { s.recv().await } else { std::future::pending().await } } => "SIGHUP",
            };
            let _ = shared.stop(name).await;
            shared.quit.notify_one();
        });
    }

    // Control channel: /status and /stop behind the bearer token.
    loop {
        let (mut stream, _) = tokio::select! {
            _ = shared.quit.notified() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(_) => continue,
            },
        };
        let shared = shared.clone();
        let token = config.token.clone();
        tokio::spawn(async move {
            let Ok((method, path, headers, _body)) = read_http_request(&mut stream, 65536).await
            else {
                return;
            };
            if headers
                .get("authorization")
                .map(|value| value == &format!("Bearer {token}"))
                .unwrap_or(false)
                == false
            {
                let _ = write_http_response(&mut stream, 403, "").await;
                return;
            }
            if path == "/stop" && method == "POST" {
                match shared.stop("requested").await {
                    Ok(state) => {
                        let body = serde_json::to_string(&state).unwrap();
                        let _ = write_http_response(&mut stream, 200, &body).await;
                        shared.quit.notify_one();
                    }
                    Err(error) => {
                        let body = serde_json::to_string(&serde_json::json!({ "error": error }))
                            .unwrap();
                        let _ = write_http_response(&mut stream, 500, &body).await;
                    }
                }
            } else if path == "/status" {
                let body = serde_json::to_string(&shared.state.lock().unwrap().to_json()).unwrap();
                let _ = write_http_response(&mut stream, 200, &body).await;
            } else {
                let _ = write_http_response(&mut stream, 404, "").await;
            }
        });
    }

    if let Some(dir) = &socket_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
    Ok(())
}

fn supervisor_signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        other => return other.to_string(),
    }
    .to_string()
}

/// Entry point for the hidden `__service_supervisor` subcommand.
pub fn service_supervisor_main() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("failed to read supervisor config from stdin");
        return 1;
    }
    let config: SupervisorConfig = match serde_json::from_str(&input) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("invalid supervisor config: {error}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to start runtime: {error}");
            return 1;
        }
    };
    match runtime.block_on(run_service_supervisor(config)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Service client (Rust rewrite of service-client.cjs)
// ---------------------------------------------------------------------------

fn client_request(
    socket_path: &str,
    token: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket_path).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(15)))
        .ok();
    let payload = body.map(|b| serde_json::to_string(b).unwrap()).unwrap_or_default();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nauthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
            "Buddy service request timed out".to_string()
        } else {
            e.to_string()
        }
    })?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed Buddy service response".to_string())?;
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let value: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if status != 200 {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(format!("HTTP {status}")));
    }
    Ok(value)
}

/// Entry point for the hidden `__service_client` subcommand. Prints the JSON
/// response on stdout (actors read it), errors on stderr with exit code 1.
pub fn service_client_main(args: Vec<String>) -> i32 {
    match service_client_run(&args) {
        Ok(value) => {
            println!("{}", serde_json::to_string(&value).unwrap());
            0
        }
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

fn service_client_run(args: &[String]) -> Result<Value, String> {
    service_client_run_with(args, None, None)
}

/// `service_client_run` with an explicit channel override — tests call the
/// client in-process (the generated `$BUDDY_SERVICE_CLI` wrapper execs the
/// app binary, which unit-test binaries are not).
pub(crate) fn service_client_run_with(
    args: &[String],
    socket_override: Option<&str>,
    token_override: Option<&str>,
) -> Result<Value, String> {
    let action = args.first().map(String::as_str).unwrap_or("");
    let name = args.get(1).map(String::as_str).unwrap_or("");
    let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();

    // This explicit stop command also works after a retained task has been
    // deleted: `stop-owned <control.json>`.
    if action == "stop-owned" {
        let text = std::fs::read_to_string(name)
            .map_err(|e| format!("read control record {name}: {e}"))?;
        let record: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if record.get("owner").and_then(Value::as_str) != Some("buddy") {
            return Err("Not a Buddy-owned service".to_string());
        }
        let socket = record
            .get("socket")
            .and_then(Value::as_str)
            .ok_or_else(|| "control record has no socket".to_string())?;
        let token = record
            .get("token")
            .and_then(Value::as_str)
            .ok_or_else(|| "control record has no token".to_string())?;
        return client_request(socket, token, "/stop", None);
    }

    let socket = socket_override
        .map(str::to_string)
        .or_else(|| std::env::var("BUDDY_SERVICE_SOCKET").ok())
        .filter(|v| !v.is_empty());
    let token = token_override
        .map(str::to_string)
        .or_else(|| std::env::var("BUDDY_SERVICE_TOKEN").ok())
        .filter(|v| !v.is_empty());
    let (Some(socket), Some(token)) = (socket, token) else {
        return Err("No active Buddy service session".to_string());
    };

    let mut body = Map::new();
    body.insert("action".to_string(), Value::from(action));
    if !name.is_empty() {
        body.insert("name".to_string(), Value::from(name));
    }
    match action {
        "start" => {
            let separator = rest
                .iter()
                .position(|arg| *arg == "--")
                .ok_or_else(|| "Usage: start NAME [--keep REASON] -- COMMAND [ARGS...]".to_string())?;
            if separator + 1 >= rest.len() {
                return Err("Usage: start NAME [--keep REASON] -- COMMAND [ARGS...]".to_string());
            }
            let options = &rest[..separator];
            if !options.is_empty()
                && !(options.len() == 2 && options[0] == "--keep")
            {
                return Err("Expected --keep REASON or no options".to_string());
            }
            body.insert(
                "command".to_string(),
                Value::Array(rest[separator + 1..].iter().map(|s| Value::from(*s)).collect()),
            );
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            body.insert(
                "cwd".to_string(),
                Value::from(cwd.to_string_lossy().to_string()),
            );
            if options.len() == 2 {
                body.insert("keepReason".to_string(), Value::from(options[1]));
            }
        }
        "keep" => {
            body.insert("keepReason".to_string(), Value::from(rest.join(" ")));
        }
        "external" => {
            let pid: u64 = rest
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| "External PID is required".to_string())?;
            body.insert("pid".to_string(), Value::from(pid));
        }
        "list" | "stop" => {}
        _ => return Err("Use start, list, stop, keep or external".to_string()),
    }
    client_request(&socket, &token, "/", Some(&Value::Object(body)))
}

// ---------------------------------------------------------------------------
// Task service manager (port of TaskServiceManager)
// ---------------------------------------------------------------------------

/// Env vars + close handle for one actor run (TS `ServiceRun`).
pub struct ServiceRun {
    pub env: HashMap<String, String>,
    lease: Option<LeaseHandle>,
    leases: std::sync::Weak<StdMutex<HashMap<String, Vec<LeaseHandle>>>>,
}

impl ServiceRun {
    pub fn close(&mut self) {
        if let Some(lease) = self.lease.take() {
            lease.close();
            if let Some(leases) = self.leases.upgrade() {
                if let Some(list) = leases.lock().unwrap().get_mut(&lease.directory) {
                    list.retain(|entry| entry.id != lease.id);
                }
            }
        }
    }
}

impl Drop for ServiceRun {
    fn drop(&mut self) {
        self.close();
    }
}

struct LeaseInner {
    id: u64,
    directory: String,
    active: AtomicBool,
    shutdown: Notify,
    socket_dir: PathBuf,
}

#[derive(Clone)]
struct LeaseHandle {
    inner: Arc<LeaseInner>,
}

impl std::ops::Deref for LeaseHandle {
    type Target = LeaseInner;
    fn deref(&self) -> &LeaseInner {
        &self.inner
    }
}

impl LeaseHandle {
    fn close(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.shutdown.notify_one();
    }
}

pub type SupervisorSpawner =
    Arc<dyn Fn(&SupervisorConfig, &Path, &HashMap<String, String>) -> Result<(), String> + Send + Sync>;

pub struct ServiceTools {
    pub client: PathBuf,
}

/// A task-scoped broker. Actor shells never own the long-lived processes.
pub struct TaskServiceManager {
    store: Arc<BuddyStore>,
    locks: StdMutex<HashMap<String, Arc<Mutex<()>>>>,
    leases: Arc<StdMutex<HashMap<String, Vec<LeaseHandle>>>>,
    closed: StdMutex<HashSet<String>>,
    tools: StdMutex<Option<Arc<ServiceTools>>>,
    spawner: SupervisorSpawner,
    next_lease_id: StdMutex<u64>,
}

impl TaskServiceManager {
    pub fn new(store: Arc<BuddyStore>) -> Self {
        Self::with_spawner(store, Arc::new(spawn_supervisor_process))
    }

    pub fn with_spawner(store: Arc<BuddyStore>, spawner: SupervisorSpawner) -> Self {
        TaskServiceManager {
            store,
            locks: StdMutex::new(HashMap::new()),
            leases: Arc::new(StdMutex::new(HashMap::new())),
            closed: StdMutex::new(HashSet::new()),
            tools: StdMutex::new(None),
            spawner,
            next_lease_id: StdMutex::new(0),
        }
    }

    fn directory(&self, task_id: &str, workspace_key: &str) -> PathBuf {
        self.store
            .data_root
            .join("runtime")
            .join("services")
            .join(sha24(&format!("{workspace_key}\0{task_id}")))
    }

    async fn locked<T>(
        &self,
        key: &Path,
        fut: impl std::future::Future<Output = T>,
    ) -> T {
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(key.to_string_lossy().to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        fut.await
    }

    /// Materialize the client wrapper script next to the data root
    /// (`dataRoot/runtime/service-tools/client-<hash>.sh`). Content-hashed so
    /// upgrades rewrite it exactly once.
    fn tools(&self) -> Result<Arc<ServiceTools>, String> {
        if let Some(tools) = self.tools.lock().unwrap().as_ref() {
            return Ok(tools.clone());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let content = format!(
            "#!/bin/sh\n# Generated by Buddy: forwards to the embedded service client.\nexec \"{}\" __service_client \"$@\"\n",
            exe.to_string_lossy()
        );
        let root = self.store.data_root.join("runtime").join("service-tools");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|e| e.to_string())?;
        let client = root.join(format!("client-{}.sh", sha24(&content)));
        write_file_mode(&client, content.as_bytes(), 0o700).map_err(|e| e.to_string())?;
        let tools = Arc::new(ServiceTools { client });
        *self.tools.lock().unwrap() = Some(tools.clone());
        Ok(tools)
    }

    async fn records(&self, directory: &Path) -> Result<Vec<TaskServiceRecord>, String> {
        let mut entries = match tokio::fs::read_dir(directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".service.json") {
                continue;
            }
            let text = tokio::fs::read_to_string(entry.path())
                .await
                .map_err(|e| e.to_string())?;
            let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            records.push(parse_task_service_record(&value)?);
        }
        Ok(records)
    }

    fn record_path(directory: &Path, name: &str) -> PathBuf {
        directory.join(format!("{name}.service.json"))
    }

    async fn status(&self, record: &TaskServiceRecord) -> Result<Map<String, Value>, String> {
        if let TaskServiceRecord::External { pid, .. } = record {
            let mut map = Map::new();
            map.insert("status".to_string(), Value::from("external"));
            map.insert("pid".to_string(), Value::from(*pid));
            return Ok(map);
        }
        let TaskServiceRecord::Buddy {
            socket,
            token,
            status_path,
            name,
            ..
        } = record
        else {
            unreachable!()
        };
        match socket_request(socket, token, "/status", None, std::time::Duration::from_secs(5))
            .await
        {
            Ok(value) => parse_service_status(&value),
            Err(error) => {
                // A recorded PID is not authority to signal anything. Only a
                // live, authenticated supervisor can stop its own process
                // group.
                let saved = tokio::fs::read_to_string(status_path)
                    .await
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| parse_service_status(&value).ok());
                if let Some(saved) = saved {
                    if saved
                        .get("status")
                        .and_then(Value::as_str)
                        .map(terminal)
                        .unwrap_or(false)
                    {
                        return Ok(saved);
                    }
                }
                Err(format!("Cannot verify service {name}: {error}"))
            }
        }
    }

    async fn describe(
        &self,
        directory: &Path,
        record: &TaskServiceRecord,
    ) -> Result<Value, String> {
        let status = match self.status(record).await {
            Ok(status) => status,
            Err(error) => {
                let mut map = Map::new();
                map.insert("status".to_string(), Value::from("unreachable"));
                map.insert("error".to_string(), Value::from(error));
                map
            }
        };
        let mut map = Map::new();
        map.insert("name".to_string(), Value::from(record.name()));
        map.insert("owner".to_string(), Value::from(record.owner()));
        if let Some(reason) = record.keep_reason() {
            map.insert("keep_reason".to_string(), Value::from(reason));
        }
        for (key, value) in status {
            map.insert(key, value);
        }
        if let TaskServiceRecord::Buddy {
            command,
            cwd,
            log_path,
            id,
            ..
        } = record
        {
            let tools = self.tools()?;
            map.insert(
                "command".to_string(),
                Value::Array(command.iter().cloned().map(Value::from).collect()),
            );
            map.insert("cwd".to_string(), Value::from(cwd.clone()));
            map.insert("log_path".to_string(), Value::from(log_path.clone()));
            let control = directory.join(format!("{id}.control.json"));
            map.insert(
                "stop_command".to_string(),
                Value::from(format!(
                    "{} stop-owned {}",
                    shell_quote(&tools.client.to_string_lossy()),
                    shell_quote(&control.to_string_lossy())
                )),
            );
        }
        Ok(Value::Object(map))
    }

    async fn stop(&self, record: &TaskServiceRecord) -> Result<(), String> {
        let TaskServiceRecord::Buddy { socket, token, name, .. } = record else {
            return Err("Existing external services are never stopped by Buddy".to_string());
        };
        let status = self.status(record).await?;
        if status
            .get("status")
            .and_then(Value::as_str)
            .map(terminal)
            .unwrap_or(false)
        {
            return Ok(());
        }
        let result = socket_request(socket, token, "/stop", None, std::time::Duration::from_secs(5))
            .await
            .and_then(|value| parse_service_status(&value))?;
        if !result
            .get("status")
            .and_then(Value::as_str)
            .map(terminal)
            .unwrap_or(false)
        {
            return Err(format!("Service {name} did not stop"));
        }
        Ok(())
    }

    /// Open a per-run broker: actor shells reach it through the
    /// `BUDDY_SERVICE_*` env vars in the returned lease. Port of `openRun`.
    pub async fn open_run(
        self: &Arc<Self>,
        task_id: &str,
        workspace_key: &str,
        run_id: &str,
        env: &HashMap<String, String>,
    ) -> Result<ServiceRun, String> {
        let directory = self.directory(task_id, workspace_key);
        let directory_key = directory.to_string_lossy().to_string();

        // Validate the run is live and persist the task manifest before the
        // broker starts accepting requests (TS: the `locked` block in openRun).
        self.locked(&directory, async {
            let state = self
                .store
                .read_task_state(task_id, workspace_key)
                .await
                .map_err(|e| e.to_string())?;
            let active_run_id = state
                .active_run
                .as_ref()
                .and_then(|run| run.run_id.as_deref());
            if active_run_id != Some(run_id) {
                return Err("Actor run is no longer active".to_string());
            }
            atomic_json(
                &directory.join("task.json"),
                &serde_json::json!({ "task_id": task_id, "workspace_key": workspace_key }),
            )
            .await?;
            self.closed.lock().unwrap().remove(&directory_key);
            Ok(())
        })
        .await?;

        let socket_dir = create_temp_dir("br-")?;
        let socket = socket_dir.join("s");
        let token = random_token();
        let broker_token = token.clone();
        let listener = match tokio::net::UnixListener::bind(&socket) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&socket_dir);
                return Err(format!("bind {}: {error}", socket.display()));
            }
        };

        let lease_id = {
            let mut next = self.next_lease_id.lock().unwrap();
            *next += 1;
            *next
        };
        let lease = LeaseHandle {
            inner: Arc::new(LeaseInner {
                id: lease_id,
                directory: directory_key.clone(),
                active: AtomicBool::new(true),
                shutdown: Notify::new(),
                socket_dir: socket_dir.clone(),
            }),
        };

        {
            let manager = self.clone();
            let lease = lease.clone();
            let directory = directory.clone();
            let task_id = task_id.to_string();
            let workspace_key = workspace_key.to_string();
            let run_id = run_id.to_string();
            let run_env = env.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, _) = tokio::select! {
                        _ = lease.shutdown.notified() => break,
                        accepted = listener.accept() => match accepted {
                            Ok(pair) => pair,
                            Err(_) => continue,
                        },
                    };
                    if !lease.active.load(Ordering::SeqCst) {
                        break;
                    }
                    let manager = manager.clone();
                    let lease = lease.clone();
                    let directory = directory.clone();
                    let task_id = task_id.clone();
                    let workspace_key = workspace_key.clone();
                    let run_id = run_id.clone();
                    let run_env = run_env.clone();
                    let token = broker_token.clone();
                    tokio::spawn(async move {
                        manager
                            .handle_broker_connection(
                                stream, &lease, &directory, &task_id, &workspace_key, &run_id,
                                &token, &run_env,
                            )
                            .await;
                        let _ = stream;
                    });
                }
                let _ = std::fs::remove_dir_all(&lease.socket_dir);
            });
        }

        self.leases
            .lock()
            .unwrap()
            .entry(directory_key)
            .or_default()
            .push(lease.clone());

        let tools = self.tools()?;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut env_out = HashMap::new();
        env_out.insert(
            "BUDDY_SERVICE_SOCKET".to_string(),
            socket.to_string_lossy().to_string(),
        );
        env_out.insert("BUDDY_SERVICE_TOKEN".to_string(), token.clone());
        env_out.insert(
            "BUDDY_SERVICE_NODE".to_string(),
            exe.to_string_lossy().to_string(),
        );
        env_out.insert(
            "BUDDY_SERVICE_CLI".to_string(),
            tools.client.to_string_lossy().to_string(),
        );
        Ok(ServiceRun {
            env: env_out,
            lease: Some(lease),
            leases: Arc::downgrade(&self.leases),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_broker_connection(
        self: &Arc<Self>,
        mut stream: tokio::net::UnixStream,
        lease: &LeaseHandle,
        directory: &Path,
        task_id: &str,
        workspace_key: &str,
        run_id: &str,
        token: &str,
        run_env: &HashMap<String, String>,
    ) {
        let result: Result<Value, String> = async {
            let (_method, _path, headers, body) = read_http_request(&mut stream, 65536).await?;
            if headers
                .get("authorization")
                .map(|value| value != &format!("Bearer {token}"))
                .unwrap_or(true)
            {
                return Err("__forbidden__".to_string());
            }
            let input: Value =
                serde_json::from_slice(&body).map_err(|e| format!("invalid JSON: {e}"))?;
            let input = parse_service_request(&input)?;
            self.handle_service_action(
                lease, directory, task_id, workspace_key, run_id, &input, run_env,
            )
            .await
        }
        .await;
        match result {
            Ok(value) => {
                let body = serde_json::to_string(&value).unwrap();
                let _ = write_http_response(&mut stream, 200, &body).await;
            }
            Err(error) if error == "__forbidden__" => {
                let _ = write_http_response(&mut stream, 403, "").await;
            }
            Err(error) => {
                let body = serde_json::to_string(&serde_json::json!({ "error": error })).unwrap();
                let _ = write_http_response(&mut stream, 400, &body).await;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_service_action(
        self: &Arc<Self>,
        lease: &LeaseHandle,
        directory: &Path,
        task_id: &str,
        workspace_key: &str,
        run_id: &str,
        input: &ServiceRequest,
        run_env: &HashMap<String, String>,
    ) -> Result<Value, String> {
        let directory_key = directory.to_string_lossy().to_string();
        self.locked(directory, async {
            let state = self
                .store
                .read_task_state(task_id, workspace_key)
                .await
                .map_err(|e| e.to_string())?;
            let active_run_id = state
                .active_run
                .as_ref()
                .and_then(|run| run.run_id.as_deref());
            if !lease.active.load(Ordering::SeqCst)
                || self.closed.lock().unwrap().contains(&directory_key)
                || active_run_id != Some(run_id)
            {
                return Err("This actor run has ended".to_string());
            }
            let records = self.records(directory).await?;
            if input.action == "list" {
                let mut out = Vec::new();
                for record in &records {
                    out.push(self.describe(directory, record).await?);
                }
                return Ok(Value::Array(out));
            }
            let name = input
                .name
                .clone()
                .ok_or_else(|| "Service name is required".to_string())?;
            let existing = records.iter().find(|record| record.name() == name).cloned();
            if input.action == "stop" {
                let existing =
                    existing.ok_or_else(|| "Service not found".to_string())?;
                self.stop(&existing).await?;
                self.store
                    .append_task_event(
                        task_id,
                        workspace_key,
                        EventInput {
                            event_type: "service.stopped".to_string(),
                            run_id: Some(run_id.to_string()),
                            payload: to_payload(serde_json::json!({ "name": name })),
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                return self.describe(directory, &existing).await;
            }
            if input.action == "keep" {
                let (Some(existing), Some(reason)) = (existing.clone(), input.keep_reason.clone())
                else {
                    return Err(
                        "Service and explicit retention reason are required".to_string()
                    );
                };
                let kept = existing.with_keep_reason(Some(reason));
                atomic_json(
                    &Self::record_path(directory, &name),
                    &kept.to_json(),
                )
                .await?;
                return self.describe(directory, &kept).await;
            }
            if let Some(existing) = existing.clone() {
                let status = self.status(&existing).await?;
                let is_terminal = status
                    .get("status")
                    .and_then(Value::as_str)
                    .map(terminal)
                    .unwrap_or(false);
                if !is_terminal {
                    if input.action == "external" {
                        if let TaskServiceRecord::External { pid, .. } = existing {
                            if input.pid == Some(pid) {
                                return self.describe(directory, &existing).await;
                            }
                        }
                    }
                    let same_command = match (&existing, &input.command) {
                        (
                            TaskServiceRecord::Buddy { command, cwd, .. },
                            Some(new_command),
                        ) => {
                            let new_cwd = input.cwd.clone().unwrap_or_default();
                            let new_cwd = std::fs::canonicalize(&new_cwd)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or(new_cwd);
                            input.action == "start"
                                && command == new_command
                                && cwd == &new_cwd
                        }
                        _ => false,
                    };
                    if !same_command {
                        return Err(
                            "Service name already belongs to another command or external service"
                                .to_string(),
                        );
                    }
                    let reused = existing.with_keep_reason(input.keep_reason.clone());
                    if input.keep_reason.is_some() {
                        atomic_json(&Self::record_path(directory, &name), &reused.to_json()).await?;
                    }
                    let mut described = self.describe(directory, &reused).await?;
                    if let Some(map) = described.as_object_mut() {
                        map.insert("reused".to_string(), Value::from(true));
                    }
                    return Ok(described);
                }
            }

            let id = uuid::Uuid::new_v4().to_string();
            let created_at = utc_now();
            if input.action == "external" {
                let pid = input
                    .pid
                    .ok_or_else(|| "External PID is required".to_string())?;
                let record = TaskServiceRecord::External {
                    id,
                    name: name.clone(),
                    task_id: task_id.to_string(),
                    workspace_key: workspace_key.to_string(),
                    created_at,
                    keep_reason: input.keep_reason.clone(),
                    pid,
                };
                atomic_json(&Self::record_path(directory, &name), &record.to_json()).await?;
                return self.describe(directory, &record).await;
            }

            let (Some(command), Some(cwd)) = (input.command.clone(), input.cwd.clone()) else {
                return Err("Command and working directory are required".to_string());
            };
            let cwd = std::fs::canonicalize(&cwd)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(cwd);
            let supervisor_socket_dir = create_temp_dir("bs-")?;
            let record = TaskServiceRecord::Buddy {
                id: id.clone(),
                name: name.clone(),
                task_id: task_id.to_string(),
                workspace_key: workspace_key.to_string(),
                created_at,
                keep_reason: input.keep_reason.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                token: random_token(),
                socket: supervisor_socket_dir.join("s").to_string_lossy().to_string(),
                status_path: directory.join(format!("{id}.status.json")).to_string_lossy().to_string(),
                log_path: directory.join(format!("{id}.log")).to_string_lossy().to_string(),
            };
            // Persist ownership before spawn, so a Buddy crash cannot lose a
            // service.
            atomic_json(&Self::record_path(directory, &name), &record.to_json()).await?;
            atomic_json(
                &directory.join(format!("{id}.control.json")),
                &record.to_json(),
            )
            .await?;

            let TaskServiceRecord::Buddy {
                socket,
                token,
                status_path,
                log_path,
                ..
            } = &record
            else {
                unreachable!()
            };
            let config = SupervisorConfig {
                socket: socket.clone(),
                token: token.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                log_path: log_path.clone(),
                status_path: status_path.clone(),
            };
            let supervisor_log = directory.join(format!("{id}.supervisor.log"));
            if let Err(error) = (self.spawner)(&config, &supervisor_log, run_env) {
                let _ = atomic_json(
                    Path::new(status_path),
                    &serde_json::json!({ "status": "failed", "error": error }),
                )
                .await;
                return Err(error);
            }

            let until = std::time::Instant::now() + std::time::Duration::from_millis(7000);
            loop {
                let status = self.status(&record).await.ok();
                if let Some(status) = status {
                    let value = status.get("status").and_then(Value::as_str).unwrap_or("");
                    if value != "starting" {
                        if value != "running" {
                            return Err(format!(
                                "Service {name} failed to stay running; see {log_path}"
                            ));
                        }
                        let pid = status.get("pid").and_then(Value::as_u64);
                        self.store
                            .append_task_event(
                                task_id,
                                workspace_key,
                                EventInput {
                                    event_type: "service.started".to_string(),
                                    run_id: Some(run_id.to_string()),
                                    payload: to_payload(serde_json::json!({
                                        "name": name,
                                        "pid": pid,
                                        "log_path": log_path,
                                        "keep_reason": record.keep_reason(),
                                    })),
                                    ..Default::default()
                                },
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                        return self.describe(directory, &record).await;
                    }
                }
                if std::time::Instant::now() >= until {
                    // Do not discard the record on ambiguous startup; cleanup
                    // can retry it.
                    return Err(format!(
                        "Service startup was not confirmed; inspect {log_path} before retrying"
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
    }

    /// Stop every Buddy-owned, non-retained service of a task. Revokes live
    /// run leases first so a draining actor cannot start new services
    /// (TS `cleanupTask`). Returns per-service failure messages.
    pub async fn cleanup_task(
        self: &Arc<Self>,
        task_id: &str,
        workspace_key: &str,
    ) -> Result<Vec<String>, String> {
        let directory = self.directory(task_id, workspace_key);
        let directory_key = directory.to_string_lossy().to_string();
        // Revoke leases immediately, including an actor still draining output.
        if let Some(leases) = self.leases.lock().unwrap().get(&directory_key) {
            for lease in leases {
                lease.close();
            }
        }
        self.locked(&directory, async {
            self.closed.lock().unwrap().insert(directory_key.clone());
            let mut failures = Vec::new();
            let records = self.records(&directory).await?;
            for record in &records {
                if matches!(record, TaskServiceRecord::External { .. })
                    || record.keep_reason().is_some()
                {
                    continue;
                }
                if let Err(error) = self.stop(record).await {
                    failures.push(format!("{}: {error}", record.name()));
                }
            }
            Ok(failures)
        })
        .await
    }

    /// Startup recovery (TS `recover`): keep services of unfinished tasks;
    /// continue cleanup for terminal, deleted or cleanup-pending tasks.
    pub async fn recover(self: &Arc<Self>) -> Result<Vec<String>, String> {
        let root = self.store.data_root.join("runtime").join("services");
        let mut entries = match tokio::fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                atomic_json(
                    &self
                        .store
                        .data_root
                        .join("runtime")
                        .join("service-recovery-errors.json"),
                    &serde_json::json!([]),
                )
                .await?;
                return Ok(Vec::new());
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut errors: Vec<String> = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
            let directory = entry.file_name().to_string_lossy().to_string();
            let result: Result<(), String> = async {
                let text = tokio::fs::read_to_string(entry.path().join("task.json"))
                    .await
                    .map_err(|e| e.to_string())?;
                let manifest =
                    parse_service_task_manifest(&serde_json::from_str(&text).map_err(|e| e.to_string())?)?;
                let state = match self
                    .store
                    .read_task_state(&manifest.task_id, &manifest.workspace_key)
                    .await
                {
                    Ok(state) => Some(state),
                    Err(error) if is_not_found(&error) => None,
                    Err(error) => return Err(error.to_string()),
                };
                let terminal_or_pending = match &state {
                    None => true,
                    Some(state) => {
                        state.status == crate::buddy::types::TaskStatus::Done
                            || state.status == crate::buddy::types::TaskStatus::Cancelled
                            || state.service_cleanup_pending.unwrap_or(false)
                    }
                };
                if terminal_or_pending {
                    let failures = self
                        .cleanup_task(&manifest.task_id, &manifest.workspace_key)
                        .await?;
                    if !failures.is_empty() {
                        errors.extend(failures.iter().cloned());
                        if state.is_some() {
                            self.store
                                .update_task_state(&manifest.task_id, &manifest.workspace_key, |mut current| {
                                    current.status = crate::buddy::types::TaskStatus::Paused;
                                    current.active_run = None;
                                    current.service_cleanup_pending = Some(true);
                                    current
                                })
                                .await
                                .map_err(|e| e.to_string())?;
                            let mut meta = Map::new();
                            meta.insert(
                                "kind".to_string(),
                                Value::from("service_cleanup_failed"),
                            );
                            self.store
                                .append_transcript(
                                    &manifest.task_id,
                                    &manifest.workspace_key,
                                    "system",
                                    &format!(
                                        "后台服务恢复清理失败，已保留记录并暂停任务：{}",
                                        failures.join("; ")
                                    ),
                                    meta,
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                        return Ok(());
                    }
                    if state
                        .as_ref()
                        .and_then(|state| state.service_cleanup_pending)
                        .unwrap_or(false)
                    {
                        self.store
                            .update_task_state(&manifest.task_id, &manifest.workspace_key, |mut current| {
                                current.service_cleanup_pending = Some(false);
                                current
                            })
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                errors.push(format!("{directory}: {error}"));
            }
        }
        atomic_json(
            &self
                .store
                .data_root
                .join("runtime")
                .join("service-recovery-errors.json"),
            &serde_json::json!(errors),
        )
        .await?;
        Ok(errors)
    }
}

fn to_payload(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Default supervisor spawner: run the app binary's hidden
/// `__service_supervisor` subcommand detached (own process group, config on
/// stdin, output to the supervisor log), mirroring the Electron edition's
/// `spawn(process.execPath, [supervisor], { detached: true, ... })`.
fn spawn_supervisor_process(
    config: &SupervisorConfig,
    supervisor_log: &Path,
    env: &HashMap<String, String>,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = open_append_mode(supervisor_log, 0o600).map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__service_supervisor")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err))
        .envs(std::env::vars())
        .envs(env)
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn supervisor: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "supervisor stdin unavailable".to_string())?;
    stdin
        .write_all(serde_json::to_string(config).unwrap().as_bytes())
        .map_err(|e| format!("write supervisor config: {e}"))?;
    drop(stdin);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (port of tests/unit/main/buddy-task-services.test.ts)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::types::{ActiveRun, CreateTaskInput, TaskStatus};
    use tempfile::TempDir;

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    struct Fixture {
        _root: TempDir,
        store: Arc<BuddyStore>,
        manager: Arc<TaskServiceManager>,
        workspace: String,
        leases: Vec<ServiceRun>,
    }

    async fn fixture(task: &str) -> Fixture {
        let root = TempDir::new().unwrap();
        let store = Arc::new(BuddyStore::new(root.path().join("data")));
        let created = store
            .create_task(CreateTaskInput {
                task_id: task.to_string(),
                repo_root: Some(root.path().to_string_lossy().to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();
        // In-process supervisor spawner: the production spawner execs the app
        // binary's `__service_supervisor` subcommand, which unit-test
        // binaries cannot dispatch; the logic under test is identical.
        let manager = Arc::new(TaskServiceManager::with_spawner(
            store.clone(),
            Arc::new(|config, _log, _env| {
                let config = config.clone();
                tokio::spawn(async move {
                    if let Err(error) = run_service_supervisor(config).await {
                        eprintln!("test supervisor exited: {error}");
                    }
                });
                Ok(())
            }),
        ));
        Fixture {
            _root: root,
            store,
            manager,
            workspace: created.workspace_key,
            leases: Vec::new(),
        }
    }

    async fn open_run(f: &mut Fixture, run_id: &str, task: &str) {
        f.store
            .update_task_state(task, &f.workspace, |mut state| {
                state.status = TaskStatus::RunningCursor;
                state.active_run = Some(ActiveRun {
                    run_id: Some(run_id.to_string()),
                    actor: "cursor".to_string(),
                    started_at: utc_now(),
                    status: Some("running".to_string()),
                    session_id_before: None,
                    session_id_after: None,
                });
                state
            })
            .await
            .unwrap();
        let lease = f
            .manager
            .open_run(task, &f.workspace, run_id, &HashMap::new())
            .await
            .unwrap();
        f.leases.push(lease);
    }

    /// In-process equivalent of invoking `$BUDDY_SERVICE_CLI` from the actor
    /// shell: real HTTP over the run's broker socket. The blocking client
    /// runs on a blocking thread so the current-thread test runtime can keep
    /// driving the broker and supervisor tasks.
    async fn client(lease: &ServiceRun, args: &[&str]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let socket = lease.env.get("BUDDY_SERVICE_SOCKET").cloned();
        let token = lease.env.get("BUDDY_SERVICE_TOKEN").cloned();
        tokio::task::spawn_blocking(move || {
            service_client_run_with(&args, socket.as_deref(), token.as_deref())
        })
        .await
        .unwrap()
    }

    fn service_command(seconds: u32) -> Vec<&'static str> {
        let _ = seconds;
        vec!["sh", "-c", "exec sleep 60"]
    }

    fn json_pid(value: &Value) -> u32 {
        value.get("pid").and_then(Value::as_u64).unwrap() as u32
    }

    async fn registry_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            while let Some(entry) = entries.next_entry().await.unwrap() {
                let path = entry.path();
                if entry.file_type().await.unwrap().is_dir() {
                    stack.push(path);
                } else if entry.file_name().to_string_lossy().ends_with(".service.json") {
                    out.push(path);
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn isolates_tasks_even_when_their_services_use_the_same_name() {
        let mut f = fixture("demo").await;
        f.store
            .create_task(CreateTaskInput {
                task_id: "other".to_string(),
                repo_root: Some(f._root.path().to_string_lossy().to_string()),
                task_text: None,
                context_text: None,
                settings: None,
                execution_mode: None,
            })
            .await
            .unwrap();
        open_run(&mut f, "r1", "demo").await;
        open_run(&mut f, "other-run", "other").await;
        let mut command = service_command(60);
        let first = client(&f.leases[0], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        command = service_command(60);
        let other = client(&f.leases[1], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        assert_ne!(json_pid(&first), json_pid(&other));

        assert!(f.manager.cleanup_task("demo", &f.workspace).await.unwrap().is_empty());
        assert!(!alive(json_pid(&first)));
        assert!(alive(json_pid(&other)));
        assert!(f.manager.cleanup_task("other", &f.workspace).await.unwrap().is_empty());
        assert!(!alive(json_pid(&other)));
    }

    #[tokio::test]
    async fn retains_a_service_only_after_an_explicit_keep_request() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        let command = service_command(60);
        let worker = client(&f.leases[0], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        assert!(client(&f.leases[0], &["keep", "worker"]).await.is_err());
        client(&f.leases[0], &["keep", "worker", "User explicitly requested a persistent preview"]).await.unwrap();
        assert!(f.manager.cleanup_task("demo", &f.workspace).await.unwrap().is_empty());
        assert!(alive(json_pid(&worker)));
        // Fixture teardown: stop the retained service so no processes leak.
        let records = registry_files(&f.store.data_root).await;
        assert_eq!(records.len(), 1);
        let control = records[0].to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || {
            service_client_run_with(&["stop-owned".to_string(), control], None, None)
        })
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn reuses_services_across_runs_and_cleans_the_whole_process_group() {
        let mut f = fixture("demo").await;
        let child_pid_file = f._root.path().join("child.pid");
        let script = format!(
            "sleep 60 & echo $! > {}; wait",
            child_pid_file.to_string_lossy()
        );
        open_run(&mut f, "r1", "demo").await;
        let first = client(&f.leases[0], &["start", "worker", "--", "sh", "-c", &script]).await.unwrap();
        assert!(alive(json_pid(&first)));
        f.leases.pop().unwrap().close();

        open_run(&mut f, "r2", "demo").await;
        let second = client(&f.leases[0], &["start", "worker", "--", "sh", "-c", &script]).await.unwrap();
        assert_eq!(json_pid(&second), json_pid(&first));
        assert_eq!(second.get("reused"), Some(&Value::from(true)));

        let child: u32 = tokio::fs::read_to_string(&child_pid_file)
            .await
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(alive(child));
        assert!(f.manager.cleanup_task("demo", &f.workspace).await.unwrap().is_empty());
        assert!(!alive(json_pid(&first)));
        assert!(!alive(child));
        // A closed run cannot start new services through its broker.
        assert!(client(&f.leases[0], &["start", "late", "--", "sh", "-c", "exec sleep 60"]).await.is_err());
    }

    #[tokio::test]
    async fn preserves_retained_and_external_services_with_explicit_stop_after_deletion() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        let mut external = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let external_pid = external.id();
        let command = service_command(60);
        let kept = client(
            &f.leases[0],
            &[&["start", "keep", "--keep", "user asked to keep preview", "--"], command.as_slice()].concat(),
        )
        .await
        .unwrap();
        let command = service_command(60);
        let managed = client(&f.leases[0], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        client(&f.leases[0], &["external", "existing", &external_pid.to_string()]).await.unwrap();
        let stop_external = client(&f.leases[0], &["stop", "existing"]).await;
        assert!(stop_external.unwrap_err().contains("never stopped"));

        let service = crate::buddy::service::BuddyCoreService::new(
            crate::buddy::service::BuddyCoreServiceOptions {
                data_root: Some(f.store.data_root.clone()),
                locale: Some("en-US".to_string()),
                ..Default::default()
            },
        );
        service.delete_task("demo", Some(&f.workspace)).await.unwrap();
        assert!(!alive(json_pid(&managed)));
        assert!(alive(json_pid(&kept)));
        assert!(alive(external_pid));

        // The explicit stop command still works after the task is deleted.
        let records = registry_files(&f.store.data_root).await;
        let keep_record = records
            .iter()
            .find(|path| path.file_name().unwrap().to_string_lossy() == "keep.service.json")
            .unwrap()
            .to_string_lossy()
            .to_string();
        tokio::task::spawn_blocking(move || {
            service_client_run_with(&["stop-owned".to_string(), keep_record], None, None)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(!alive(json_pid(&kept)));
        let _ = external.kill();
        let _ = external.wait();
    }

    #[tokio::test]
    async fn recovers_ownership_after_restart_preserving_paused_and_cleaning_terminal() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        let command = service_command(60);
        let worker = client(&f.leases[0], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        f.leases.pop().unwrap().close();
        f.store
            .update_task_state("demo", &f.workspace, |mut state| {
                state.status = TaskStatus::Paused;
                state.active_run = None;
                state
            })
            .await
            .unwrap();

        // A rebuilt manager (app restart) keeps services of unfinished tasks.
        let restarted = Arc::new(TaskServiceManager::with_spawner(
            f.store.clone(),
            Arc::new(|_, _, _| Ok(())),
        ));
        assert!(restarted.recover().await.unwrap().is_empty());
        assert!(alive(json_pid(&worker)));

        f.store
            .update_task_state("demo", &f.workspace, |mut state| {
                state.status = TaskStatus::Done;
                state
            })
            .await
            .unwrap();
        assert!(restarted.recover().await.unwrap().is_empty());
        assert!(!alive(json_pid(&worker)));
    }

    #[tokio::test]
    async fn does_not_trust_stale_pids_when_the_supervisor_is_unreachable() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        let command = service_command(60);
        let worker = client(&f.leases[0], &[&["start", "worker", "--"], command.as_slice()].concat()).await.unwrap();
        client(&f.leases[0], &["stop", "worker"]).await.unwrap();
        assert!(!alive(json_pid(&worker)));

        // Point the saved status at THIS process: a recorded PID is not
        // authority to signal anything.
        let records = registry_files(&f.store.data_root).await;
        assert_eq!(records.len(), 1);
        let record_text = tokio::fs::read_to_string(&records[0]).await.unwrap();
        let record = parse_task_service_record(&serde_json::from_str(&record_text).unwrap()).unwrap();
        let TaskServiceRecord::Buddy { status_path, .. } = &record else {
            panic!("expected buddy record");
        };
        atomic_json_sync(
            Path::new(status_path),
            &serde_json::json!({
                "status": "running",
                "pid": std::process::id(),
                "supervisor_pid": std::process::id(),
            }),
        )
        .unwrap();
        let failures = f.manager.cleanup_task("demo", &f.workspace).await.unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("Cannot verify"));
        assert!(alive(std::process::id()));
    }

    #[tokio::test]
    async fn cleans_a_failed_start_without_losing_its_record() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        let error = client(&f.leases[0], &["start", "bad", "--", "/definitely/not/a/program"]).await
            .unwrap_err();
        assert!(error.contains("failed to stay running"), "{error}");
        assert!(f.manager.cleanup_task("demo", &f.workspace).await.unwrap().is_empty());
        assert_eq!(registry_files(&f.store.data_root).await.len(), 1);
    }

    #[tokio::test]
    async fn validates_client_requests_and_record_files() {
        let mut f = fixture("demo").await;
        open_run(&mut f, "r1", "demo").await;
        // Missing `--` separator.
        assert!(client(&f.leases[0], &["start", "worker", "sleep"]).await.is_err());
        // Unknown action.
        assert!(client(&f.leases[0], &["restart", "worker"]).await.is_err());
        // Malformed name rejected by the read-time schema.
        assert!(client(&f.leases[0], &["start", "bad name!", "--", "sleep", "1"]).await.is_err());
        // The broker rejects requests without the bearer token.
        let socket = f.leases[0].env.get("BUDDY_SERVICE_SOCKET").unwrap().clone();
        assert!(service_client_run_with(&["list".to_string()], Some(&socket), Some("wrong-token")).is_err());
    }
}
