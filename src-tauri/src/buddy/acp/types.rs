//! ACP (Agent Client Protocol) wire types, port of
//! `src/main/buddy/acp/types.ts` from the Electron edition.
//!
//! These types are the wire protocol spoken with external agent processes —
//! field names must match the protocol byte-for-byte.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// Protocol/transport failure with a fully formatted message (mirrors the
    /// `Error` instances the TS edition rejects with).
    #[error("{0}")]
    Message(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// JSON-RPC 2.0 request id: `string | number` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

impl JsonRpcId {
    pub fn from_value(value: &Value) -> Option<JsonRpcId> {
        match value {
            Value::Number(n) => n
                .as_i64()
                .map(JsonRpcId::Number)
                .or_else(|| n.as_u64().map(|u| JsonRpcId::Number(u as i64))),
            Value::String(s) => Some(JsonRpcId::String(s.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A decoded incoming JSON-RPC message, classified with the same guard chain
/// as `client.ts` `handleMessage` (response first, then notification, then
/// request — the guards are mutually exclusive on well-formed input).
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcMessage {
    Request {
        id: JsonRpcId,
        method: String,
        params: Option<Value>,
    },
    Response {
        id: JsonRpcId,
        result: Option<Value>,
        error: Option<JsonRpcError>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}

impl JsonRpcMessage {
    /// Port of `isJsonRpcResponse` / `isJsonRpcNotification` /
    /// `isJsonRpcRequest`. Returns `None` for non-conforming messages.
    pub fn parse(value: &Value) -> Option<JsonRpcMessage> {
        let obj = value.as_object()?;
        let id = obj.get("id").and_then(JsonRpcId::from_value);
        let method = obj.get("method").and_then(Value::as_str);

        // isJsonRpcResponse: 'id' in msg && ('result' in msg || 'error' in msg)
        if let Some(id) = id.clone() {
            if obj.contains_key("result") || obj.contains_key("error") {
                let error = obj
                    .get("error")
                    .and_then(|e| serde_json::from_value::<JsonRpcError>(e.clone()).ok());
                return Some(JsonRpcMessage::Response {
                    id,
                    result: obj.get("result").cloned(),
                    error,
                });
            }
        }

        // isJsonRpcNotification: 'method' in msg && !('id' in msg)
        if let Some(method) = method {
            if !obj.contains_key("id") {
                return Some(JsonRpcMessage::Notification {
                    method: method.to_string(),
                    params: obj.get("params").cloned(),
                });
            }
            // isJsonRpcRequest: 'method' in msg && 'id' in msg
            if let Some(id) = id {
                return Some(JsonRpcMessage::Request {
                    id,
                    method: method.to_string(),
                    params: obj.get("params").cloned(),
                });
            }
        }

        None
    }
}

/// Build an outgoing JSON-RPC request object; `params` is omitted when `None`
/// (JSON.stringify drops `undefined`, so the key must not appear).
pub fn json_rpc_request(id: &JsonRpcId, method: &str, params: Option<Value>) -> Value {
    let mut map = Map::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert(
        "id".to_string(),
        serde_json::to_value(id).unwrap_or(Value::Null),
    );
    map.insert("method".to_string(), Value::String(method.to_string()));
    if let Some(params) = params {
        map.insert("params".to_string(), params);
    }
    Value::Object(map)
}

pub fn json_rpc_result(id: &JsonRpcId, result: Value) -> Value {
    let mut map = Map::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert(
        "id".to_string(),
        serde_json::to_value(id).unwrap_or(Value::Null),
    );
    map.insert("result".to_string(), result);
    Value::Object(map)
}

pub fn json_rpc_error(id: &JsonRpcId, code: i64, message: &str) -> Value {
    let mut error = Map::new();
    error.insert("code".to_string(), Value::Number(code.into()));
    error.insert("message".to_string(), Value::String(message.to_string()));
    let mut map = Map::new();
    map.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    map.insert(
        "id".to_string(),
        serde_json::to_value(id).unwrap_or(Value::Null),
    );
    map.insert("error".to_string(), Value::Object(error));
    Value::Object(map)
}

/// Port of `formatAcpRpcError` in client.ts: `ACP Error {code}: {message}`
/// plus the data payload (raw when a string, JSON otherwise), truncated to
/// 2000 characters.
pub fn format_acp_rpc_error(error: &JsonRpcError) -> String {
    let data_text = match &error.data {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => format!(" {s}"),
        Some(other) => format!(" {}", serde_json::to_string(other).unwrap_or_default()),
    };
    let text = format!("ACP Error {}: {}{}", error.code, error.message, data_text);
    text.chars().take(2000).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpClientCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<AcpFsCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpFsCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpInitializeParams {
    /// `number | string` on the wire.
    pub protocol_version: Value,
    pub client_info: AcpClientInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<AcpClientCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<AcpClientCapabilities>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpAgentInfo {
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpAgentCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpInitializeResult {
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub protocol_version: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<AcpAgentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_info: Option<AcpAgentInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<AcpAgentCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Content block for prompt turns (TS discriminated union on `type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AcpContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename = "resource")]
    Resource {
        resource: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        uri: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpNewSessionParams {
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpSessionMode {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpSessionModes {
    pub current_mode_id: String,
    #[serde(default)]
    pub available_modes: Vec<AcpSessionMode>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpSessionModel {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpSessionModels {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<Vec<AcpSessionModel>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpNewSessionResult {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<AcpSessionModes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<AcpSessionModels>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpLoadSessionParams {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpLoadSessionResult {
    pub session_id: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcpToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// `prompt` accepts a plain string or pre-built content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AcpPromptInput {
    Text(String),
    Blocks(Vec<AcpContentBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpPromptParams {
    pub session_id: String,
    pub prompt: AcpPromptInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AcpToolDefinition>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Map<String, Value>>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

/// Streaming events emitted by the agent during a prompt (TS union on
/// `type`). This is the typed projection of the notifications the client
/// fans out to prompt handlers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AcpStreamNotification {
    #[serde(rename = "content_delta")]
    ContentDelta { text: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        input: Map<String, Value>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        result: Value,
        #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "session_break")]
    SessionBreak {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_incoming_messages_like_ts_guards() {
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}});
        assert_eq!(
            JsonRpcMessage::parse(&response),
            Some(JsonRpcMessage::Response {
                id: JsonRpcId::Number(1),
                result: Some(json!({"ok": true})),
                error: None,
            })
        );

        let error_response =
            json!({"jsonrpc": "2.0", "id": "abc", "error": {"code": -32600, "message": "bad"}});
        assert_eq!(
            JsonRpcMessage::parse(&error_response),
            Some(JsonRpcMessage::Response {
                id: JsonRpcId::String("abc".to_string()),
                result: None,
                error: Some(JsonRpcError {
                    code: -32600,
                    message: "bad".to_string(),
                    data: None,
                }),
            })
        );

        let request = json!({"jsonrpc": "2.0", "id": "perm_req_1", "method": "session/request_permission", "params": {}});
        assert_eq!(
            JsonRpcMessage::parse(&request),
            Some(JsonRpcMessage::Request {
                id: JsonRpcId::String("perm_req_1".to_string()),
                method: "session/request_permission".to_string(),
                params: Some(json!({})),
            })
        );

        let notification = json!({"jsonrpc": "2.0", "method": "session/update", "params": {}});
        assert_eq!(
            JsonRpcMessage::parse(&notification),
            Some(JsonRpcMessage::Notification {
                method: "session/update".to_string(),
                params: Some(json!({})),
            })
        );

        assert_eq!(JsonRpcMessage::parse(&json!(null)), None);
        assert_eq!(JsonRpcMessage::parse(&json!({"jsonrpc": "2.0"})), None);
        // A string id is not a response id for our pending map but the guard
        // chain treats id+result as a response regardless of id type.
        let weird = json!({"id": 1, "method": "m", "result": 1});
        assert!(matches!(
            JsonRpcMessage::parse(&weird),
            Some(JsonRpcMessage::Response { .. })
        ));
    }

    #[test]
    fn request_builder_omits_params_when_absent() {
        let msg = json_rpc_request(&JsonRpcId::Number(7), "session/cancel", None);
        assert_eq!(
            msg,
            json!({"jsonrpc": "2.0", "id": 7, "method": "session/cancel"})
        );
        let msg = json_rpc_request(
            &JsonRpcId::String("s".to_string()),
            "m",
            Some(json!({"a": 1})),
        );
        assert_eq!(
            msg,
            json!({"jsonrpc": "2.0", "id": "s", "method": "m", "params": {"a": 1}})
        );
    }

    #[test]
    fn formats_rpc_errors_like_ts() {
        let plain = JsonRpcError {
            code: -32600,
            message: "Invalid Request".to_string(),
            data: None,
        };
        assert_eq!(
            format_acp_rpc_error(&plain),
            "ACP Error -32600: Invalid Request"
        );

        let with_string_data = JsonRpcError {
            code: -32603,
            message: "Internal error".to_string(),
            data: Some(json!("超出上下文")),
        };
        assert_eq!(
            format_acp_rpc_error(&with_string_data),
            "ACP Error -32603: Internal error 超出上下文"
        );

        let with_object_data = JsonRpcError {
            code: 1,
            message: "m".to_string(),
            data: Some(json!({"k": "v"})),
        };
        assert_eq!(
            format_acp_rpc_error(&with_object_data),
            "ACP Error 1: m {\"k\":\"v\"}"
        );

        let long = JsonRpcError {
            code: 1,
            message: "x".repeat(5000),
            data: None,
        };
        assert_eq!(format_acp_rpc_error(&long).chars().count(), 2000);
    }

    #[test]
    fn content_blocks_round_trip_wire_format() {
        let block = AcpContentBlock::Text {
            text: "hello".to_string(),
            extra: Map::new(),
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"type": "text", "text": "hello"})
        );
        let parsed: AcpContentBlock = serde_json::from_value(
            json!({"type": "image", "data": "b64", "mimeType": "image/png", "custom": 1}),
        )
        .unwrap();
        match parsed {
            AcpContentBlock::Image {
                data,
                mime_type,
                extra,
            } => {
                assert_eq!(data, "b64");
                assert_eq!(mime_type, "image/png");
                assert_eq!(extra.get("custom"), Some(&json!(1)));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        let link: AcpContentBlock =
            serde_json::from_value(json!({"type": "resource_link", "uri": "file:///x"})).unwrap();
        assert!(matches!(link, AcpContentBlock::ResourceLink { .. }));
    }

    #[test]
    fn prompt_input_serializes_string_or_blocks() {
        let params = AcpPromptParams {
            session_id: "sess_1".to_string(),
            prompt: AcpPromptInput::Text("hi".to_string()),
            tools: None,
            meta: None,
        };
        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value, json!({"sessionId": "sess_1", "prompt": "hi"}));
    }

    #[test]
    fn stream_notification_uses_snake_case_tags() {
        let n = AcpStreamNotification::ToolResult {
            id: "1".to_string(),
            result: json!(null),
            is_error: Some(true),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"type": "tool_result", "id": "1", "result": null, "isError": true})
        );
        let parsed: AcpStreamNotification =
            serde_json::from_value(json!({"type": "session_break", "reason": "done"})).unwrap();
        assert_eq!(
            parsed,
            AcpStreamNotification::SessionBreak {
                reason: Some("done".to_string())
            }
        );
    }

    #[test]
    fn initialize_result_keeps_unknown_fields() {
        let parsed: AcpInitializeResult = serde_json::from_value(json!({
            "protocolVersion": "1.0",
            "agentInfo": {"name": "test-agent", "version": "0.1.0"},
            "capabilities": {"streaming": true, "tools": true},
            "futureField": {"nested": true}
        }))
        .unwrap();
        assert_eq!(parsed.protocol_version, json!("1.0"));
        assert_eq!(
            parsed.agent_info.as_ref().map(|a| a.name.as_str()),
            Some("test-agent")
        );
        assert_eq!(
            parsed.capabilities.as_ref().and_then(|c| c.streaming),
            Some(true)
        );
        assert_eq!(
            parsed.extra.get("futureField"),
            Some(&json!({"nested": true}))
        );
    }
}
