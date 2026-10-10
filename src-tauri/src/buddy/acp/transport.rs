//! JSON-RPC stdio transport for ACP agents, port of
//! `src/main/buddy/acp/transport.ts` from the Electron edition.
//!
//! Spawns the agent with piped stdio (tokio::process), frames messages as
//! newline-delimited JSON, and surfaces events through a broadcast channel.
//! The TS edition's `onMessage`/`onClose`/`onError` handler sets collapse
//! into a single [`AcpTransportEvent`] stream; subscribers should call
//! [`AcpTransport::subscribe`] before `start()` so no event is lost.

use crate::buddy::acp::types::AcpError;
use crate::buddy::launchers::split_command;
use crate::buddy::shell_path::merge_child_env;
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

/// Events delivered to transport subscribers, replacing the TS
/// `onMessage`/`onClose`/`onError` handler triplet.
#[derive(Debug, Clone)]
pub enum AcpTransportEvent {
    /// A parsed JSON-RPC message line from the agent's stdout.
    Message(Value),
    /// The child process exited (`close` in Node). `signal` is the Node-style
    /// signal name when the child died from one.
    Closed {
        code: Option<i32>,
        signal: Option<String>,
    },
    /// Spawn/pipe failure (`error` in Node). Carries the error message.
    Error(String),
}

/// Transport abstraction consumed by [`crate::buddy::acp::client::AcpClient`];
/// mirrors the TS `AcpTransport` interface so tests can substitute a mock.
#[async_trait]
pub trait AcpTransport: Send + Sync {
    /// Serialize `message` as one JSON line and write it to the agent.
    async fn send(&self, message: Value) -> Result<(), AcpError>;
    /// Subscribe to the transport event stream. Call before `start()`.
    fn subscribe(&self) -> broadcast::Receiver<AcpTransportEvent>;
    /// Terminate the child (SIGTERM, SIGKILL after 1s if still alive).
    async fn close(&self);
    /// Buffered stderr tail (last 50 chunks joined), TS `getStderr()`.
    fn stderr_text(&self) -> String;
}

#[derive(Clone, Default)]
pub struct StdioTransportOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub on_stderr: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

const STDERR_CHUNK_CAP: usize = 50;
const EVENT_CHANNEL_CAP: usize = 1024;

pub struct AcpStdioTransport {
    options: StdioTransportOptions,
    events: broadcast::Sender<AcpTransportEvent>,
    stdin: Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>,
    stderr_chunks: Arc<Mutex<VecDeque<String>>>,
    closed: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    child_pid: Arc<AtomicU32>,
    started: AtomicBool,
}

impl AcpStdioTransport {
    pub fn new(options: StdioTransportOptions) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        AcpStdioTransport {
            options,
            events,
            stdin: Arc::new(tokio::sync::Mutex::new(None)),
            stderr_chunks: Arc::new(Mutex::new(VecDeque::new())),
            closed: Arc::new(AtomicBool::new(false)),
            exited: Arc::new(AtomicBool::new(false)),
            child_pid: Arc::new(AtomicU32::new(0)),
            started: AtomicBool::new(false),
        }
    }

    /// Spawn the subprocess and attach stdio handlers. Like the TS `start()`:
    /// repeated calls are no-ops, and spawn failure surfaces through the
    /// `Error`/`Closed` events rather than throwing.
    pub async fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }

        let tokens = split_command(&self.options.command);
        let cmd = tokens.first().cloned().unwrap_or_default();
        let prefix_args = &tokens[tokens.len().min(1)..];

        let mut command = tokio::process::Command::new(&cmd);
        command
            .args(prefix_args)
            .args(&self.options.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &self.options.cwd {
            command.current_dir(cwd);
        }
        // TS: `env: mergeChildEnv(process.env, this.options.env)`.
        let merged_env = merge_child_env(&std::env::vars().collect(), self.options.env.as_ref());
        command.env_clear().envs(merged_env);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Node reports spawn failure asynchronously as an 'error'
                // event ("spawn <cmd> ENOENT") followed by 'close'.
                self.closed.store(true, Ordering::SeqCst);
                self.exited.store(true, Ordering::SeqCst);
                let _ = self
                    .events
                    .send(AcpTransportEvent::Error(spawn_error_message(&cmd, &error)));
                let _ = self.events.send(AcpTransportEvent::Closed {
                    code: None,
                    signal: None,
                });
                return;
            }
        };

        if let Some(pid) = child.id() {
            self.child_pid.store(pid, Ordering::SeqCst);
        }
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let stdin = child.stdin.take().expect("stdin piped");
        *self.stdin.lock().await = Some(stdin);

        // Stdout reader: newline-delimited JSON; non-JSON lines (startup
        // banners, debug logs) are ignored, matching the TS readline handler.
        {
            let events = self.events.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<Value>(trimmed) {
                                Ok(parsed) => {
                                    // Send only fails when no subscribers are
                                    // listening; nothing to do then.
                                    let _ = events.send(AcpTransportEvent::Message(parsed));
                                }
                                Err(_) => continue,
                            }
                        }
                        Ok(None) => break,
                        Err(error) => {
                            let _ = events.send(AcpTransportEvent::Error(format!(
                                "stdout read error: {error}"
                            )));
                            break;
                        }
                    }
                }
            });
        }

        // Stderr reader: keep the last 50 chunks and forward to on_stderr.
        {
            let stderr_chunks = self.stderr_chunks.clone();
            let on_stderr = self.options.on_stderr.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::with_capacity(8192);
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let text = String::from_utf8_lossy(&buf).into_owned();
                            {
                                let mut chunks = stderr_chunks.lock();
                                chunks.push_back(text.clone());
                                while chunks.len() > STDERR_CHUNK_CAP {
                                    chunks.pop_front();
                                }
                            }
                            if let Some(cb) = &on_stderr {
                                cb(text);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Exit watcher: Node 'close' event equivalent.
        {
            let events = self.events.clone();
            let closed = self.closed.clone();
            let exited = self.exited.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                closed.store(true, Ordering::SeqCst);
                exited.store(true, Ordering::SeqCst);
                let (code, signal) = match status {
                    Ok(status) => exit_code_and_signal(&status),
                    Err(_) => (None, None),
                };
                let _ = events.send(AcpTransportEvent::Closed { code, signal });
            });
        }
    }
}

/// Node-style error text for a failed spawn (`spawn <cmd> ENOENT`).
fn spawn_error_message(cmd: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("spawn {cmd} ENOENT"),
        std::io::ErrorKind::PermissionDenied => format!("spawn {cmd} EACCES"),
        _ => format!("spawn {cmd} failed: {error}"),
    }
}

#[cfg(unix)]
fn exit_code_and_signal(status: &std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;
    (status.code(), status.signal().map(signal_name))
}

#[cfg(not(unix))]
fn exit_code_and_signal(status: &std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    (status.code(), None)
}

/// Node-style signal names, same table as launchers.rs.
#[cfg(unix)]
fn signal_name(signal: i32) -> String {
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

fn send_signal(pid: u32, signal: &str) {
    let _ = std::process::Command::new("kill")
        .args([signal, &pid.to_string()])
        .status();
}

#[async_trait]
impl AcpTransport for AcpStdioTransport {
    async fn send(&self, message: Value) -> Result<(), AcpError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(AcpError::Message("Transport is closed".to_string()));
        }
        let mut guard = self.stdin.lock().await;
        let Some(stdin) = guard.as_mut() else {
            return Err(AcpError::Message("Transport is closed".to_string()));
        };
        let mut payload = serde_json::to_string(&message)?;
        payload.push('\n');
        stdin.write_all(payload.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<AcpTransportEvent> {
        self.events.subscribe()
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // Drop stdin so the child sees EOF on its input.
        *self.stdin.lock().await = None;

        let pid = self.child_pid.load(Ordering::SeqCst);
        if pid != 0 && !self.exited.load(Ordering::SeqCst) {
            send_signal(pid, "-TERM");
            // Give it 1 second to exit cleanly, then SIGKILL (TS setTimeout).
            let exited = self.exited.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if !exited.load(Ordering::SeqCst) {
                    send_signal(pid, "-KILL");
                }
            });
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr_chunks.lock().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::acp::client::{AcpClient, AcpClientOptions, AcpPromptHandlers};
    use crate::buddy::acp::types::{AcpPromptInput, AcpPromptParams};
    use serde_json::json;
    use std::time::Duration;

    /// Doubles as the mock agent process: when spawned as a child test binary
    /// with `BUDDY_ACP_MOCK_AGENT=1` it answers JSON-RPC lines on stdin. Run
    /// normally (env unset) it returns immediately so the suite stays green.
    #[test]
    fn mock_agent_child_main() {
        if std::env::var("BUDDY_ACP_MOCK_AGENT").is_err() {
            return;
        }
        use std::io::{BufRead, Write};
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        // Non-JSON banner: the transport must ignore it.
        println!("mock-agent startup banner");
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            match msg.get("method").and_then(Value::as_str) {
                Some("test/ping") => {
                    let response = json!({"jsonrpc": "2.0", "id": id, "result": {"ping": "pong"}});
                    writeln!(stdout, "{response}").unwrap();
                    stdout.flush().unwrap();
                }
                Some("session/prompt") => {
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {"update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "chunk-1 "}}}
                    });
                    writeln!(stdout, "{notification}").unwrap();
                    let response =
                        json!({"jsonrpc": "2.0", "id": id, "result": {"status": "completed"}});
                    writeln!(stdout, "{response}").unwrap();
                    stdout.flush().unwrap();
                }
                Some("test/exit") => {
                    eprintln!("fatal error: unknown flag");
                    let _ = std::io::stderr().flush();
                    std::process::exit(2);
                }
                _ => {
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "Method not found"}
                    });
                    writeln!(stdout, "{response}").unwrap();
                    stdout.flush().unwrap();
                }
            }
        }
    }

    fn mock_agent_options() -> StdioTransportOptions {
        let exe = std::env::current_exe().unwrap();
        let mut env = HashMap::new();
        env.insert("BUDDY_ACP_MOCK_AGENT".to_string(), "1".to_string());
        StdioTransportOptions {
            command: exe.to_string_lossy().into_owned(),
            args: vec!["mock_agent_child_main".to_string()],
            env: Some(env),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn communicates_over_stdio_with_subprocess() {
        let transport = Arc::new(AcpStdioTransport::new(mock_agent_options()));
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());
        transport.start().await;

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            client.request_value("test/ping", None, None),
        )
        .await
        .expect("ping timed out")
        .unwrap();
        assert_eq!(result, json!({"ping": "pong"}));

        client.close().await;
    }

    #[tokio::test]
    async fn delivers_streaming_notifications_over_stdio() {
        let transport = Arc::new(AcpStdioTransport::new(mock_agent_options()));
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());
        transport.start().await;

        let deltas = Arc::new(Mutex::new(Vec::new()));
        let handlers = AcpPromptHandlers {
            on_content_delta: Some(Arc::new({
                let deltas = deltas.clone();
                move |text: String| deltas.lock().push(text)
            })),
            ..Default::default()
        };

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            client.prompt(
                &AcpPromptParams {
                    session_id: "sess_1".to_string(),
                    prompt: AcpPromptInput::Text("hi".to_string()),
                    tools: None,
                    meta: None,
                },
                Some(handlers),
            ),
        )
        .await
        .expect("prompt timed out")
        .unwrap();
        assert_eq!(result.status.as_deref(), Some("completed"));
        assert_eq!(*deltas.lock(), vec!["chunk-1 ".to_string()]);

        client.close().await;
    }

    #[tokio::test]
    async fn buffers_stderr_and_reports_exit_code() {
        let stderr_seen = Arc::new(Mutex::new(String::new()));
        let transport = AcpStdioTransport::new(StdioTransportOptions {
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "echo 'fatal error: unknown flag' >&2; exit 1".to_string(),
            ],
            on_stderr: Some(Arc::new({
                let seen = stderr_seen.clone();
                move |chunk: String| seen.lock().push_str(&chunk)
            })),
            ..Default::default()
        });
        let mut rx = transport.subscribe();
        transport.start().await;

        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(AcpTransportEvent::Closed { code, signal }) = rx.recv().await {
                    break (code, signal);
                }
            }
        })
        .await
        .expect("close event timed out");
        assert_eq!(closed, (Some(1), None));
        assert!(transport
            .stderr_text()
            .contains("fatal error: unknown flag"));
        assert!(stderr_seen.lock().contains("fatal error: unknown flag"));
    }

    #[tokio::test]
    async fn close_terminates_child_and_rejects_sends() {
        let transport = AcpStdioTransport::new(StdioTransportOptions {
            // `sleep` ignores stdin EOF, so only close()'s SIGTERM ends it.
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "sleep 60".to_string()],
            ..Default::default()
        });
        let mut rx = transport.subscribe();
        transport.start().await;
        transport.close().await;

        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(AcpTransportEvent::Closed { code, signal }) = rx.recv().await {
                    break (code, signal);
                }
            }
        })
        .await
        .expect("close event timed out");
        assert_eq!(closed.1, Some("SIGTERM".to_string()));

        let err = transport
            .send(json!({"jsonrpc": "2.0", "id": 1, "method": "x"}))
            .await;
        assert!(matches!(err, Err(AcpError::Message(m)) if m == "Transport is closed"));
    }

    #[tokio::test]
    async fn spawn_failure_emits_error_and_close_events() {
        let transport = AcpStdioTransport::new(StdioTransportOptions {
            command: "non-existent-binary-buddy-test-xyz-987".to_string(),
            ..Default::default()
        });
        let mut rx = transport.subscribe();
        transport.start().await;

        let mut saw_error = None;
        let mut saw_closed = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Ok(AcpTransportEvent::Error(e)) => saw_error = Some(e),
                    Ok(AcpTransportEvent::Closed { .. }) => {
                        saw_closed = true;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("events timed out");
        assert_eq!(
            saw_error.as_deref(),
            Some("spawn non-existent-binary-buddy-test-xyz-987 ENOENT")
        );
        assert!(saw_closed);
    }
}
