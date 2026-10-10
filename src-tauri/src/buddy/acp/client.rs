//! ACP client, port of `src/main/buddy/acp/client.ts` from the Electron
//! edition: initialize handshake, session lifecycle, streaming prompt turns,
//! host-request handling (permission auto-approve, `buddy_propose_break`),
//! timeouts and cancellation.
//!
//! Structural adaptations from TS:
//! - The `onMessage`/`onClose`/`onError` subscriptions collapse into a single
//!   dispatch task draining the transport's [`AcpTransportEvent`] stream.
//! - Pending requests are `oneshot` channels keyed by JSON-RPC id instead of
//!   a `Map` of resolve/reject closures; per-request timeouts use
//!   `tokio::time::timeout` instead of `setTimeout`.

use crate::buddy::acp::transport::AcpTransport;
use crate::buddy::acp::transport::AcpTransportEvent;
use crate::buddy::acp::types::{
    format_acp_rpc_error, json_rpc_error, json_rpc_request, json_rpc_result, AcpClientCapabilities,
    AcpClientInfo, AcpError, AcpFsCapabilities, AcpInitializeParams, AcpInitializeResult,
    AcpLoadSessionParams, AcpLoadSessionResult, AcpNewSessionParams, AcpNewSessionResult,
    AcpPromptInput, AcpPromptParams, AcpPromptResult, JsonRpcId, JsonRpcMessage,
};
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Future returned by the host tool executor (`onExecuteTool` in TS).
pub type ExecuteToolFuture = Pin<Box<dyn Future<Output = Result<Value, AcpError>> + Send>>;
pub type ExecuteToolFn = Arc<dyn Fn(String, Map<String, Value>) -> ExecuteToolFuture + Send + Sync>;

/// Callbacks for a streaming prompt turn, mirroring the TS
/// `AcpPromptHandlers`. All fields optional, like the TS version.
#[derive(Clone, Default)]
pub struct AcpPromptHandlers {
    pub on_content_delta: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_thinking_delta: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_tool_call: Option<Arc<dyn Fn(String, String, Map<String, Value>) + Send + Sync>>,
    pub on_tool_result: Option<Arc<dyn Fn(String, Value, bool) + Send + Sync>>,
    pub on_break: Option<Arc<dyn Fn(Option<String>) + Send + Sync>>,
    /// Host-provided tool executor for `tools/call` requests.
    pub on_execute_tool: Option<ExecuteToolFn>,
}

#[derive(Clone, Default)]
pub struct AcpClientOptions {
    pub timeout_ms: Option<u64>,
    pub client_info: Option<AcpClientInfo>,
}

const DEFAULT_TIMEOUT_MS: u64 = 60000;

struct AcpClientInner {
    transport: Arc<dyn AcpTransport>,
    pending: Mutex<HashMap<JsonRpcId, oneshot::Sender<Result<Value, AcpError>>>>,
    next_id: AtomicU64,
    handlers: Mutex<Option<AcpPromptHandlers>>,
    options: AcpClientOptions,
}

impl AcpClientInner {
    fn reject_all_pending(&self, error: AcpError) {
        let pending = std::mem::take(&mut *self.pending.lock());
        for (_, sender) in pending {
            let _ = sender.send(Err(clone_error(&error)));
        }
    }

    fn handler_snapshot(&self) -> Option<AcpPromptHandlers> {
        self.handlers.lock().clone()
    }
}

/// AcpError is not Clone (io/json variants); pending rejection only ever
/// carries `Message`, so rebuild the message text.
fn clone_error(error: &AcpError) -> AcpError {
    AcpError::Message(error.to_string())
}

#[derive(Clone)]
pub struct AcpClient {
    inner: Arc<AcpClientInner>,
    transport: Arc<dyn AcpTransport>,
    dispatch_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AcpClient {
    /// Subscribe to the transport and spawn the dispatch task. Subscribe
    /// before starting the transport so no event is missed.
    pub fn new(transport: Arc<dyn AcpTransport>, options: AcpClientOptions) -> AcpClient {
        let inner = Arc::new(AcpClientInner {
            transport: transport.clone(),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            handlers: Mutex::new(None),
            options,
        });
        let rx = transport.subscribe();
        let dispatch_inner = inner.clone();
        let dispatch_task = tokio::spawn(dispatch_loop(dispatch_inner, rx));
        AcpClient {
            inner,
            transport,
            dispatch_task: Arc::new(Mutex::new(Some(dispatch_task))),
        }
    }

    /// Raw JSON-RPC request; resolves with the `result` value. Port of the
    /// TS `request<T>` (registration before send, send failure rejects,
    /// timeout removes the pending entry).
    pub async fn request_value(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, AcpError> {
        let id = JsonRpcId::Number(self.inner.next_id.fetch_add(1, Ordering::SeqCst) as i64);
        let request = json_rpc_request(&id, method, params);

        let effective_timeout = timeout_ms
            .or(self.inner.options.timeout_ms)
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id.clone(), tx);
        if let Err(error) = self.transport.send(request).await {
            self.inner.pending.lock().remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(std::time::Duration::from_millis(effective_timeout), rx).await {
            Ok(Ok(result)) => result,
            // Sender dropped without answering (client closed race).
            Ok(Err(_)) => Err(AcpError::Message("ACP Client closed".to_string())),
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                Err(AcpError::Message(format!(
                    "ACP Request timeout after {effective_timeout}ms for method {method}"
                )))
            }
        }
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_ms: Option<u64>,
    ) -> Result<T, AcpError> {
        let value = self.request_value(method, params, timeout_ms).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Handshake with the ACP agent. `custom_params` keys override the
    /// defaults (TS `...customParams` spread).
    pub async fn initialize(
        &self,
        custom_params: Option<Map<String, Value>>,
    ) -> Result<AcpInitializeResult, AcpError> {
        let client_capabilities = AcpClientCapabilities {
            fs: Some(AcpFsCapabilities {
                read_text_file: Some(true),
                write_text_file: Some(true),
            }),
            terminal: Some(true),
            extra: Map::new(),
        };
        let params = AcpInitializeParams {
            protocol_version: json!(1),
            client_info: self
                .inner
                .options
                .client_info
                .clone()
                .unwrap_or(AcpClientInfo {
                    name: "buddy".to_string(),
                    version: "1.0.0".to_string(),
                }),
            client_capabilities: Some(client_capabilities.clone()),
            capabilities: Some(client_capabilities),
            meta: None,
        };
        let mut map = match serde_json::to_value(params)? {
            Value::Object(map) => map,
            _ => unreachable!("struct serializes to object"),
        };
        if let Some(custom) = custom_params {
            for (key, value) in custom {
                map.insert(key, value);
            }
        }
        self.request("initialize", Some(Value::Object(map)), None)
            .await
    }

    /// Create a new session; `mcpServers` defaults to `[]` on the wire.
    pub async fn new_session(
        &self,
        params: &AcpNewSessionParams,
    ) -> Result<AcpNewSessionResult, AcpError> {
        let mut map = to_object(params)?;
        map.entry("mcpServers".to_string())
            .or_insert_with(|| json!([]));
        self.request("session/new", Some(Value::Object(map)), None)
            .await
    }

    /// Load an existing session; `mcpServers` defaults to `[]` on the wire.
    pub async fn load_session(
        &self,
        params: &AcpLoadSessionParams,
    ) -> Result<AcpLoadSessionResult, AcpError> {
        let mut map = to_object(params)?;
        map.entry("mcpServers".to_string())
            .or_insert_with(|| json!([]));
        self.request("session/load", Some(Value::Object(map)), None)
            .await
    }

    /// Set session mode (e.g. bypassPermissions, acceptEdits).
    pub async fn set_session_mode(
        &self,
        session_id: &str,
        mode_id: &str,
    ) -> Result<Value, AcpError> {
        self.request_value(
            "session/set_mode",
            Some(json!({"sessionId": session_id, "modeId": mode_id})),
            None,
        )
        .await
    }

    /// Legacy ACP model selector (session/set_model). Newer agents expose the
    /// same choice through session/set_config_option instead.
    pub async fn set_session_model(
        &self,
        session_id: &str,
        model_id: &str,
    ) -> Result<Value, AcpError> {
        self.request_value(
            "session/set_model",
            Some(json!({"sessionId": session_id, "modelId": model_id})),
            None,
        )
        .await
    }

    /// Set a session config option (model, mode, thought level, …).
    pub async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: Value,
    ) -> Result<Value, AcpError> {
        self.request_value(
            "session/set_config_option",
            Some(json!({"sessionId": session_id, "configId": config_id, "value": value})),
            None,
        )
        .await
    }

    /// Ask the agent to drop an active session if it advertises session close.
    pub async fn close_session(&self, session_id: &str) -> Result<Value, AcpError> {
        self.request_value(
            "session/close",
            Some(json!({"sessionId": session_id})),
            None,
        )
        .await
    }

    /// Send a prompt, providing the `buddy_propose_break` tool and streaming
    /// notifications to `handlers` for the duration of the turn.
    pub async fn prompt(
        &self,
        params: &AcpPromptParams,
        handlers: Option<AcpPromptHandlers>,
    ) -> Result<AcpPromptResult, AcpError> {
        *self.inner.handlers.lock() = handlers;

        let result = self.prompt_request(params).await;

        *self.inner.handlers.lock() = None;
        let value = result?;
        Ok(serde_json::from_value(value)?)
    }

    async fn prompt_request(&self, params: &AcpPromptParams) -> Result<Value, AcpError> {
        let prompt_blocks = match &params.prompt {
            AcpPromptInput::Blocks(blocks) => serde_json::to_value(blocks)?,
            AcpPromptInput::Text(text) => json!([{"type": "text", "text": text}]),
        };

        let break_tool = json!({
            "name": "buddy_propose_break",
            "description": "Call this tool when you believe the task is complete, verified, or ready for reviewer break confirmation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "reason": {"type": "string", "description": "Detailed reason why you propose breaking/completing the task"}
                },
                "required": ["reason"]
            }
        });

        let mut tools: Vec<Value> = match &params.tools {
            Some(tools) => serde_json::to_value(tools)?,
            None => json!([]),
        }
        .as_array()
        .cloned()
        .unwrap_or_default();
        tools.push(break_tool);

        let mut payload = Map::new();
        payload.insert("sessionId".to_string(), json!(params.session_id));
        payload.insert("prompt".to_string(), prompt_blocks);
        payload.insert("tools".to_string(), Value::Array(tools));
        if let Some(meta) = &params.meta {
            payload.insert("_meta".to_string(), Value::Object(meta.clone()));
        }

        self.request_value("session/prompt", Some(Value::Object(payload)), None)
            .await
    }

    /// Cancel an ongoing session prompt.
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        self.request_value(
            "session/cancel",
            Some(json!({"sessionId": session_id})),
            None,
        )
        .await?;
        Ok(())
    }

    /// Clean up client resources and close the underlying transport.
    pub async fn close(&self) {
        if let Some(task) = self.dispatch_task.lock().take() {
            task.abort();
        }
        self.inner
            .reject_all_pending(AcpError::Message("ACP Client closed".to_string()));
        self.transport.close().await;
    }
}

fn to_object<T: serde::Serialize>(value: &T) -> Result<Map<String, Value>, AcpError> {
    match serde_json::to_value(value)? {
        Value::Object(map) => Ok(map),
        _ => unreachable!("struct serializes to object"),
    }
}

async fn dispatch_loop(
    inner: Arc<AcpClientInner>,
    mut rx: tokio::sync::broadcast::Receiver<AcpTransportEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(AcpTransportEvent::Message(message)) => handle_message(&inner, message),
            Ok(AcpTransportEvent::Closed { code, signal }) => {
                let stderr = inner.transport.stderr_text().trim().to_string();
                let exit_desc = match (code, signal) {
                    (Some(code), _) => format!("code {code}"),
                    (None, Some(signal)) => format!("signal {signal}"),
                    (None, None) => "unknown reason".to_string(),
                };
                let reason = if stderr.is_empty() {
                    format!("Process exited unexpectedly with {exit_desc}")
                } else {
                    format!("Process exited with {exit_desc}: {stderr}")
                };
                inner.reject_all_pending(AcpError::Message(reason));
                break;
            }
            Ok(AcpTransportEvent::Error(error)) => {
                inner.reject_all_pending(AcpError::Message(error));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                inner.reject_all_pending(AcpError::Message("ACP Client closed".to_string()));
                break;
            }
        }
    }
}

fn handle_message(inner: &Arc<AcpClientInner>, message: Value) {
    match JsonRpcMessage::parse(&message) {
        Some(JsonRpcMessage::Response { id, result, error }) => {
            let pending = inner.pending.lock().remove(&id);
            if let Some(sender) = pending {
                let outcome = match error {
                    Some(error) => Err(AcpError::Message(format_acp_rpc_error(&error))),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
        }
        Some(JsonRpcMessage::Notification { method, params }) => {
            handle_notification(inner, &method, params.as_ref());
        }
        Some(JsonRpcMessage::Request { id, method, params }) => {
            let inner = inner.clone();
            tokio::spawn(async move {
                handle_host_request(&inner, id, &method, params).await;
            });
        }
        None => {}
    }
}

fn handle_notification(inner: &AcpClientInner, method: &str, params: Option<&Value>) {
    let Some(handlers) = inner.handler_snapshot() else {
        return;
    };
    let data = params.and_then(Value::as_object);

    match method {
        "session/contentDelta" | "content_delta" => {
            if let Some(text) = data.and_then(|d| d.get("text")).and_then(Value::as_str) {
                if let Some(cb) = &handlers.on_content_delta {
                    cb(text.to_string());
                }
            }
        }
        "session/thinkingDelta" | "thinking_delta" => {
            if let Some(thinking) = data.and_then(|d| d.get("thinking")).and_then(Value::as_str) {
                if let Some(cb) = &handlers.on_thinking_delta {
                    cb(thinking.to_string());
                }
            }
        }
        "session/toolCall" | "tool_call" => {
            let id = data.and_then(|d| d.get("id")).and_then(Value::as_str);
            let name = data.and_then(|d| d.get("name")).and_then(Value::as_str);
            if let (Some(id), Some(name)) = (id, name) {
                let input = data
                    .and_then(|d| d.get("input"))
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if let Some(cb) = &handlers.on_tool_call {
                    cb(id.to_string(), name.to_string(), input);
                }
            }
        }
        "session/toolResult" | "tool_result" => {
            if let Some(id) = data.and_then(|d| d.get("id")).and_then(Value::as_str) {
                let result = data
                    .and_then(|d| d.get("result"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let is_error = js_truthy(data.and_then(|d| d.get("isError")));
                if let Some(cb) = &handlers.on_tool_result {
                    cb(id.to_string(), result, is_error);
                }
            }
        }
        "session/break" => {
            let reason = data
                .and_then(|d| d.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(cb) = &handlers.on_break {
                cb(reason);
            }
        }
        "session/update" => {
            let update = data.map(|d| match d.get("update") {
                Some(update) if !update.is_null() => update.clone(),
                _ => Value::Object(d.clone()),
            });
            if let Some(update) = update {
                if let Some(update_obj) = update.as_object() {
                    dispatch_session_update(&handlers, update_obj);
                }
            }
            // Generic fallback for custom delta format (runs in addition to
            // the sessionUpdate handling, as in the TS edition).
            if let Some(delta) = data.and_then(|d| d.get("delta")).and_then(Value::as_object) {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    if let Some(cb) = &handlers.on_content_delta {
                        cb(text.to_string());
                    }
                }
                if let Some(thinking) = delta.get("thinking").and_then(Value::as_str) {
                    if let Some(cb) = &handlers.on_thinking_delta {
                        cb(thinking.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

fn dispatch_session_update(handlers: &AcpPromptHandlers, update: &Map<String, Value>) {
    let session_update = update.get("sessionUpdate").and_then(Value::as_str);
    match session_update {
        Some("agent_message_chunk") | Some("content_chunk") => {
            let content_text = update
                .get("content")
                .and_then(Value::as_object)
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str);
            if let Some(text) = content_text {
                if let Some(cb) = &handlers.on_content_delta {
                    cb(text.to_string());
                }
            } else if let Some(text) = update.get("text").and_then(Value::as_str) {
                if let Some(cb) = &handlers.on_content_delta {
                    cb(text.to_string());
                }
            }
        }
        Some("agent_thought_chunk") => {
            let content_text = update
                .get("content")
                .and_then(Value::as_object)
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str);
            if let Some(text) = content_text {
                if let Some(cb) = &handlers.on_thinking_delta {
                    cb(text.to_string());
                }
            } else if let Some(thinking) = update.get("thinking").and_then(Value::as_str) {
                if let Some(cb) = &handlers.on_thinking_delta {
                    cb(thinking.to_string());
                }
            }
        }
        Some("tool_call") => {
            let tool_call_id = pick(update, &["toolCallId", "id"])
                .map(js_string)
                .unwrap_or_default();
            let name = pick(update, &["name", "title"])
                .map(js_string)
                .unwrap_or_else(|| "tool".to_string());
            let input = pick(update, &["rawInput", "input"])
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(cb) = &handlers.on_tool_call {
                cb(tool_call_id, name, input);
            }
        }
        Some("tool_call_update") => {
            let tool_call_id = pick(update, &["toolCallId", "id"])
                .map(js_string)
                .unwrap_or_default();
            let status = update.get("status").and_then(Value::as_str);
            if status == Some("completed") || status == Some("error") {
                let content = update.get("content").cloned().unwrap_or(Value::Null);
                let is_error = status == Some("error");
                if let Some(cb) = &handlers.on_tool_result {
                    cb(tool_call_id, content, is_error);
                }
            }
        }
        _ => {}
    }
}

/// First non-null value among `keys` (JS `a ?? b ?? c` with `??` treating
/// null and undefined the same).
fn pick<'a>(obj: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| obj.get(*key).filter(|value| !value.is_null()))
}

/// JS `String(value)` for the shapes ACP ids/names take in practice.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS truthiness (`Boolean(value)`): null/false/0/"" are falsy.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

async fn handle_host_request(
    inner: &AcpClientInner,
    id: JsonRpcId,
    method: &str,
    params: Option<Value>,
) {
    let params = params
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if method == "session/request_permission" || method == "session/requestPermission" {
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let pick_kind = |kind: &str| {
            options
                .iter()
                .find(|opt| opt.get("kind").and_then(Value::as_str) == Some(kind))
        };
        let allowed = pick_kind("allow_always")
            .or_else(|| pick_kind("allow_once"))
            .or_else(|| options.first());
        let result = match allowed.and_then(|opt| opt.get("optionId")) {
            Some(option_id) => json!({
                "outcome": {"outcome": "selected", "optionId": option_id}
            }),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        };
        send_result(inner, &id, result).await;
        return;
    }

    if method == "tools/call" || method == "session/callTool" {
        let tool_name = params
            .get("name")
            .filter(|v| !v.is_null())
            .map(js_string)
            .unwrap_or_default();
        let tool_input = params
            .get("input")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        if tool_name == "buddy_propose_break" {
            let reason = tool_input
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(handlers) = inner.handler_snapshot() {
                if let Some(cb) = &handlers.on_break {
                    cb(reason);
                }
            }
            send_result(
                inner,
                &id,
                json!({"status": "success", "message": "Break proposal recorded by Buddy"}),
            )
            .await;
            return;
        }

        let executor = inner
            .handler_snapshot()
            .and_then(|handlers| handlers.on_execute_tool.clone());
        if let Some(executor) = executor {
            match executor(tool_name.clone(), tool_input).await {
                Ok(result) => send_result(inner, &id, json!({"result": result})).await,
                Err(error) => send_error(inner, &id, -32000, &error.to_string()).await,
            }
            return;
        }

        send_error(inner, &id, -32601, &format!("Tool not found: {tool_name}")).await;
        return;
    }

    send_error(
        inner,
        &id,
        -32601,
        &format!("Method not supported: {method}"),
    )
    .await;
}

async fn send_result(inner: &AcpClientInner, id: &JsonRpcId, result: Value) {
    if let Err(error) = inner.transport.send(json_rpc_result(id, result)).await {
        eprintln!("acp: failed to send response for {id:?}: {error}");
    }
}

async fn send_error(inner: &AcpClientInner, id: &JsonRpcId, code: i64, message: &str) {
    if let Err(error) = inner
        .transport
        .send(json_rpc_error(id, code, message))
        .await
    {
        eprintln!("acp: failed to send error response for {id:?}: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buddy::acp::transport::AcpTransportEvent;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::sync::broadcast;

    /// Port of the MockAcpTransport in tests/unit/main/buddy-acp.test.ts.
    struct MockTransport {
        sent: Mutex<Vec<Value>>,
        events: broadcast::Sender<AcpTransportEvent>,
        closed: AtomicBool,
        stderr: Mutex<String>,
    }

    impl MockTransport {
        fn new() -> Arc<MockTransport> {
            let (events, _) = broadcast::channel(1024);
            Arc::new(MockTransport {
                sent: Mutex::new(Vec::new()),
                events,
                closed: AtomicBool::new(false),
                stderr: Mutex::new(String::new()),
            })
        }

        fn simulate_message(&self, msg: Value) {
            let _ = self.events.send(AcpTransportEvent::Message(msg));
        }

        fn simulate_close(&self, code: Option<i32>, signal: Option<&str>, stderr: &str) {
            *self.stderr.lock() = stderr.to_string();
            self.closed.store(true, Ordering::SeqCst);
            let _ = self.events.send(AcpTransportEvent::Closed {
                code,
                signal: signal.map(str::to_string),
            });
        }

        fn simulate_error(&self, message: &str) {
            let _ = self
                .events
                .send(AcpTransportEvent::Error(message.to_string()));
        }

        fn sent(&self) -> Vec<Value> {
            self.sent.lock().clone()
        }
    }

    #[async_trait]
    impl AcpTransport for MockTransport {
        async fn send(&self, message: Value) -> Result<(), AcpError> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(AcpError::Message("Transport is closed".to_string()));
            }
            self.sent.lock().push(message);
            Ok(())
        }

        fn subscribe(&self) -> broadcast::Receiver<AcpTransportEvent> {
            self.events.subscribe()
        }

        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }

        fn stderr_text(&self) -> String {
            self.stderr.lock().clone()
        }
    }

    async fn wait_for_sent(mock: &MockTransport, n: usize) -> Vec<Value> {
        for _ in 0..400 {
            if mock.sent.lock().len() >= n {
                return mock.sent();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "timed out waiting for {n} sent messages, got {:#?}",
            mock.sent()
        );
    }

    #[tokio::test]
    async fn performs_initialize_handshake_with_capabilities() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let client_task = {
            let client = client.clone();
            tokio::spawn(async move { client.initialize(None).await })
        };

        let sent = wait_for_sent(&transport, 1).await;
        let req = &sent[0];
        assert_eq!(req["method"], json!("initialize"));
        assert_eq!(req["jsonrpc"], json!("2.0"));
        assert_eq!(req["params"]["protocolVersion"], json!(1));

        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": req["id"],
            "result": {
                "protocolVersion": "1.0",
                "agentInfo": {"name": "test-agent", "version": "0.1.0"},
                "capabilities": {"streaming": true, "tools": true}
            }
        }));

        let result = client_task.await.unwrap().unwrap();
        assert_eq!(result.agent_info.unwrap().name, "test-agent");
        assert_eq!(result.capabilities.unwrap().streaming, Some(true));
        client.close().await;
    }

    #[tokio::test]
    async fn creates_and_resumes_sessions_with_mcp_servers_default() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let new_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .new_session(&AcpNewSessionParams {
                        cwd: "/test/repo".to_string(),
                        ..Default::default()
                    })
                    .await
            })
        };
        let sent = wait_for_sent(&transport, 1).await;
        assert_eq!(sent[0]["method"], json!("session/new"));
        assert_eq!(sent[0]["params"]["mcpServers"], json!([]));
        transport.simulate_message(json!({
            "jsonrpc": "2.0", "id": sent[0]["id"], "result": {"sessionId": "sess_12345"}
        }));
        let new_result = new_task.await.unwrap().unwrap();
        assert_eq!(new_result.session_id, "sess_12345");

        let load_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .load_session(&AcpLoadSessionParams {
                        session_id: "sess_12345".to_string(),
                        cwd: "/test/repo".to_string(),
                        ..Default::default()
                    })
                    .await
            })
        };
        let sent = wait_for_sent(&transport, 2).await;
        assert_eq!(sent[1]["method"], json!("session/load"));
        assert_eq!(sent[1]["params"]["mcpServers"], json!([]));
        transport.simulate_message(json!({
            "jsonrpc": "2.0", "id": sent[1]["id"], "result": {"sessionId": "sess_12345"}
        }));
        let load_result = load_task.await.unwrap().unwrap();
        assert_eq!(load_result.session_id, "sess_12345");
        client.close().await;
    }

    #[tokio::test]
    async fn streams_content_and_thinking_deltas_including_session_update() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let content_deltas = Arc::new(Mutex::new(Vec::new()));
        let thinking_deltas = Arc::new(Mutex::new(Vec::new()));
        let handlers = AcpPromptHandlers {
            on_content_delta: Some(Arc::new({
                let deltas = content_deltas.clone();
                move |text: String| deltas.lock().push(text)
            })),
            on_thinking_delta: Some(Arc::new({
                let deltas = thinking_deltas.clone();
                move |thinking: String| deltas.lock().push(thinking)
            })),
            ..Default::default()
        };

        let prompt_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .prompt(
                        &AcpPromptParams {
                            session_id: "sess_1".to_string(),
                            prompt: AcpPromptInput::Text("Hello agent".to_string()),
                            tools: None,
                            meta: None,
                        },
                        Some(handlers),
                    )
                    .await
            })
        };

        let sent = wait_for_sent(&transport, 1).await;
        let prompt_req = &sent[0];
        assert_eq!(prompt_req["method"], json!("session/prompt"));
        assert_eq!(
            prompt_req["params"]["prompt"],
            json!([{"type": "text", "text": "Hello agent"}])
        );
        // The dual-break tool is always appended.
        let tools = prompt_req["params"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("buddy_propose_break"));

        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "method": "session/thinkingDelta",
            "params": {"thinking": "Let me think..."}
        }));
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": "More thinking..."}
                }
            }
        }));
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "Hello "}
                }
            }
        }));
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "method": "session/contentDelta",
            "params": {"text": "world!"}
        }));
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": prompt_req["id"],
            "result": {"status": "completed"}
        }));

        let result = prompt_task.await.unwrap().unwrap();
        assert_eq!(result.status.as_deref(), Some("completed"));
        assert_eq!(
            *thinking_deltas.lock(),
            vec![
                "Let me think...".to_string(),
                "More thinking...".to_string()
            ]
        );
        assert_eq!(
            *content_deltas.lock(),
            vec!["Hello ".to_string(), "world!".to_string()]
        );
        client.close().await;
    }

    #[tokio::test]
    async fn auto_approves_permission_requests_from_agent() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let prompt_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .prompt(
                        &AcpPromptParams {
                            session_id: "sess_1".to_string(),
                            prompt: AcpPromptInput::Text("Run code".to_string()),
                            tools: None,
                            meta: None,
                        },
                        None,
                    )
                    .await
            })
        };
        let sent = wait_for_sent(&transport, 1).await;
        let prompt_id = sent[0]["id"].clone();

        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": "perm_req_1",
            "method": "session/request_permission",
            "params": {
                "sessionId": "sess_1",
                "toolCall": {"toolCallId": "call_1", "title": "Write file"},
                "options": [
                    {"optionId": "opt_reject", "name": "Reject", "kind": "reject_once"},
                    {"optionId": "opt_allow", "name": "Allow always", "kind": "allow_always"}
                ]
            }
        }));

        let sent = wait_for_sent(&transport, 2).await;
        let perm_res = sent
            .iter()
            .find(|m| m["id"] == json!("perm_req_1"))
            .expect("permission response");
        assert_eq!(
            perm_res["result"],
            json!({"outcome": {"outcome": "selected", "optionId": "opt_allow"}})
        );

        transport.simulate_message(json!({
            "jsonrpc": "2.0", "id": prompt_id, "result": {"status": "completed"}
        }));
        prompt_task.await.unwrap().unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn handles_buddy_propose_break_tool_call() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let break_reason = Arc::new(Mutex::new(None::<String>));
        let handlers = AcpPromptHandlers {
            on_break: Some(Arc::new({
                let slot = break_reason.clone();
                move |reason: Option<String>| *slot.lock() = reason
            })),
            ..Default::default()
        };

        let prompt_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .prompt(
                        &AcpPromptParams {
                            session_id: "sess_1".to_string(),
                            prompt: AcpPromptInput::Text("Check if done".to_string()),
                            tools: None,
                            meta: None,
                        },
                        Some(handlers),
                    )
                    .await
            })
        };
        let sent = wait_for_sent(&transport, 1).await;
        let prompt_id = sent[0]["id"].clone();

        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": "agent_call_1",
            "method": "tools/call",
            "params": {
                "name": "buddy_propose_break",
                "input": {"reason": "All unit tests pass and feature is verified."}
            }
        }));

        let sent = wait_for_sent(&transport, 2).await;
        let host_res = sent
            .iter()
            .find(|m| m["id"] == json!("agent_call_1"))
            .expect("tool call response");
        assert_eq!(
            host_res["result"],
            json!({"status": "success", "message": "Break proposal recorded by Buddy"})
        );
        assert_eq!(
            break_reason.lock().as_deref(),
            Some("All unit tests pass and feature is verified.")
        );

        transport.simulate_message(json!({
            "jsonrpc": "2.0", "id": prompt_id, "result": {"status": "completed"}
        }));
        let result = prompt_task.await.unwrap().unwrap();
        assert_eq!(result.status.as_deref(), Some("completed"));
        client.close().await;
    }

    #[tokio::test]
    async fn rejects_on_json_rpc_error_response() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let req_task = {
            let client = client.clone();
            tokio::spawn(async move { client.request_value("some/method", None, None).await })
        };
        let sent = wait_for_sent(&transport, 1).await;
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": sent[0]["id"],
            "error": {"code": -32600, "message": "Invalid Request"}
        }));

        let err = req_task.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "ACP Error -32600: Invalid Request");
        client.close().await;
    }

    #[tokio::test]
    async fn includes_error_data_in_wrapped_message() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let req_task = {
            let client = client.clone();
            tokio::spawn(async move { client.request_value("session/prompt", None, None).await })
        };
        let sent = wait_for_sent(&transport, 1).await;
        transport.simulate_message(json!({
            "jsonrpc": "2.0",
            "id": sent[0]["id"],
            "error": {"code": -32603, "message": "Internal error", "data": "超出上下文"}
        }));

        let err = req_task.await.unwrap().unwrap_err();
        assert_eq!(
            err.to_string(),
            "ACP Error -32603: Internal error 超出上下文"
        );
        client.close().await;
    }

    #[tokio::test]
    async fn rejects_pending_when_transport_closes_with_stderr() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let init_task = {
            let client = client.clone();
            tokio::spawn(async move { client.initialize(None).await })
        };
        wait_for_sent(&transport, 1).await;
        transport.simulate_close(Some(2), None, "Error: unexpected argument \"acp\"");

        let err = init_task.await.unwrap().unwrap_err();
        assert_eq!(
            err.to_string(),
            "Process exited with code 2: Error: unexpected argument \"acp\""
        );
        client.close().await;
    }

    #[tokio::test]
    async fn rejects_pending_when_transport_emits_error() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let init_task = {
            let client = client.clone();
            tokio::spawn(async move { client.initialize(None).await })
        };
        wait_for_sent(&transport, 1).await;
        transport.simulate_error("spawn ENOENT");

        let err = init_task.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "spawn ENOENT");
        client.close().await;
    }

    #[tokio::test]
    async fn sends_set_model_and_set_config_option() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let model_task = {
            let client = client.clone();
            tokio::spawn(async move { client.set_session_model("sess_1", "opus-4.6").await })
        };
        let sent = wait_for_sent(&transport, 1).await;
        assert_eq!(sent[0]["method"], json!("session/set_model"));
        assert_eq!(
            sent[0]["params"],
            json!({"sessionId": "sess_1", "modelId": "opus-4.6"})
        );
        transport.simulate_message(json!({"jsonrpc": "2.0", "id": sent[0]["id"], "result": {}}));
        model_task.await.unwrap().unwrap();

        let config_task = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .set_session_config_option("sess_1", "model", json!("gpt-5.6"))
                    .await
            })
        };
        let sent = wait_for_sent(&transport, 2).await;
        assert_eq!(sent[1]["method"], json!("session/set_config_option"));
        assert_eq!(
            sent[1]["params"],
            json!({"sessionId": "sess_1", "configId": "model", "value": "gpt-5.6"})
        );
        transport.simulate_message(json!({"jsonrpc": "2.0", "id": sent[1]["id"], "result": {}}));
        config_task.await.unwrap().unwrap();
        client.close().await;
    }

    #[tokio::test]
    async fn request_times_out_with_ts_message() {
        let transport = MockTransport::new();
        let client = AcpClient::new(
            transport.clone(),
            AcpClientOptions {
                timeout_ms: Some(50),
                client_info: None,
            },
        );

        let err = client
            .request_value("some/method", None, None)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ACP Request timeout after 50ms for method some/method"
        );
        client.close().await;
    }

    #[tokio::test]
    async fn close_rejects_pending_and_closes_transport() {
        let transport = MockTransport::new();
        let client = AcpClient::new(transport.clone(), AcpClientOptions::default());

        let req_task = {
            let client = client.clone();
            tokio::spawn(async move { client.request_value("some/method", None, None).await })
        };
        wait_for_sent(&transport, 1).await;
        client.close().await;

        let err = req_task.await.unwrap().unwrap_err();
        assert_eq!(err.to_string(), "ACP Client closed");
        assert!(transport.closed.load(Ordering::SeqCst));
    }
}
