//! ACP model discovery and selection, port of `src/main/buddy/acp/models.ts`.
//!
//! Agents advertise models either on the legacy `session/new` `models` field
//! or as a `configOptions` entry whose category/id is `model`. Applying a
//! choice prefers the channel the session actually advertised.

use crate::buddy::acp::client::{AcpClient, AcpClientOptions};
use crate::buddy::acp::transport::{AcpStdioTransport, AcpTransport, StdioTransportOptions};
use crate::buddy::acp::types::AcpError;
use crate::buddy::types::{AcpModelInfo, AcpModelList};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const MODEL_CATEGORY: &str = "model";
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpModelSource {
    Models,
    ConfigOptions,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedAcpModels {
    pub models: Vec<AcpModelInfo>,
    pub current_model_id: Option<String>,
    pub model_config_id: Option<String>,
    pub source: AcpModelSource,
}

/// Host calls used by [`apply_acp_session_model`]. `AcpClient` implements this;
/// tests substitute a recorder.
#[async_trait]
pub trait AcpModelClient: Send + Sync {
    async fn set_session_model(&self, session_id: &str, model_id: &str) -> Result<Value, AcpError>;
    async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: Value,
    ) -> Result<Value, AcpError>;
}

#[async_trait]
impl AcpModelClient for AcpClient {
    async fn set_session_model(&self, session_id: &str, model_id: &str) -> Result<Value, AcpError> {
        AcpClient::set_session_model(self, session_id, model_id).await
    }

    async fn set_session_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: Value,
    ) -> Result<Value, AcpError> {
        AcpClient::set_session_config_option(self, session_id, config_id, value).await
    }
}

pub struct ProbeAcpModelsOptions {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

fn config_value_id(value: &Value) -> Option<String> {
    if let Some(direct) = text_value(value) {
        return Some(direct);
    }
    let Value::Object(record) = value else {
        return None;
    };
    record
        .get("value")
        .and_then(text_value)
        .or_else(|| record.get("id").and_then(text_value))
}

fn option_id(option: &Value) -> Option<String> {
    let Value::Object(record) = option else {
        return None;
    };
    record
        .get("value")
        .and_then(text_value)
        .or_else(|| record.get("modelId").and_then(text_value))
        .or_else(|| record.get("id").and_then(text_value))
}

fn option_name(option: &Value, fallback: &str) -> String {
    let Value::Object(record) = option else {
        return fallback.to_string();
    };
    record
        .get("name")
        .and_then(text_value)
        .or_else(|| record.get("title").and_then(text_value))
        .unwrap_or_else(|| fallback.to_string())
}

fn is_model_config_option(option: &serde_json::Map<String, Value>) -> bool {
    if option
        .get("category")
        .and_then(text_value)
        .is_some_and(|category| category.eq_ignore_ascii_case(MODEL_CATEGORY))
    {
        return true;
    }
    let id = option
        .get("configId")
        .and_then(text_value)
        .or_else(|| option.get("id").and_then(text_value));
    if id.is_some_and(|id| id.eq_ignore_ascii_case(MODEL_CATEGORY)) {
        return true;
    }
    let name = option.get("name").and_then(text_value);
    option.get("type").and_then(text_value).as_deref() == Some("select")
        && name.is_some_and(|name| name.eq_ignore_ascii_case(MODEL_CATEGORY))
}

fn unique_models(models: Vec<AcpModelInfo>) -> Vec<AcpModelInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for model in models {
        if model.id.is_empty() || !seen.insert(model.id.clone()) {
            continue;
        }
        result.push(model);
    }
    result
}

pub fn extract_acp_session_models(session: &Value) -> ExtractedAcpModels {
    let Value::Object(record) = session else {
        return ExtractedAcpModels {
            models: Vec::new(),
            current_model_id: None,
            model_config_id: None,
            source: AcpModelSource::None,
        };
    };

    if let Some(Value::Object(models_record)) = record.get("models") {
        let available = models_record
            .get("availableModels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let models = unique_models(
            available
                .iter()
                .filter_map(|item| {
                    let Value::Object(info) = item else {
                        return None;
                    };
                    let id = info
                        .get("modelId")
                        .and_then(text_value)
                        .or_else(|| info.get("id").and_then(text_value))
                        .or_else(|| info.get("value").and_then(text_value))?;
                    let name = info
                        .get("name")
                        .and_then(text_value)
                        .unwrap_or_else(|| id.clone());
                    Some(AcpModelInfo { id, name })
                })
                .collect(),
        );
        if !models.is_empty() {
            return ExtractedAcpModels {
                models,
                current_model_id: models_record.get("currentModelId").and_then(text_value),
                model_config_id: None,
                source: AcpModelSource::Models,
            };
        }
    }

    let config_options = record
        .get("configOptions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for raw in config_options {
        let Value::Object(option) = raw else {
            continue;
        };
        if !is_model_config_option(&option) {
            continue;
        }
        let choices = option
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let models = unique_models(
            choices
                .iter()
                .filter_map(|item| {
                    let id = option_id(item)?;
                    Some(AcpModelInfo {
                        name: option_name(item, &id),
                        id,
                    })
                })
                .collect(),
        );
        if models.is_empty() {
            continue;
        }
        return ExtractedAcpModels {
            models,
            current_model_id: option.get("currentValue").and_then(config_value_id),
            model_config_id: Some(
                option
                    .get("configId")
                    .and_then(text_value)
                    .or_else(|| option.get("id").and_then(text_value))
                    .unwrap_or_else(|| MODEL_CATEGORY.to_string()),
            ),
            source: AcpModelSource::ConfigOptions,
        };
    }

    ExtractedAcpModels {
        models: Vec::new(),
        current_model_id: None,
        model_config_id: None,
        source: AcpModelSource::None,
    }
}

pub async fn apply_acp_session_model(
    client: &impl AcpModelClient,
    session_id: &str,
    model_id: &str,
    session: &Value,
) -> Result<(), AcpError> {
    let trimmed = model_id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let extracted = extract_acp_session_models(session);
    if extracted.current_model_id.as_deref() == Some(trimmed) {
        return Ok(());
    }

    if extracted.source == AcpModelSource::ConfigOptions {
        if let Some(config_id) = extracted.model_config_id.as_deref() {
            return set_config_option(client, session_id, config_id, trimmed).await;
        }
    }

    if extracted.source == AcpModelSource::Models {
        client.set_session_model(session_id, trimmed).await?;
        return Ok(());
    }

    if client.set_session_model(session_id, trimmed).await.is_ok() {
        return Ok(());
    }
    let config_id = extracted
        .model_config_id
        .as_deref()
        .unwrap_or(MODEL_CATEGORY);
    set_config_option(client, session_id, config_id, trimmed).await
}

async fn set_config_option(
    client: &impl AcpModelClient,
    session_id: &str,
    config_id: &str,
    model_id: &str,
) -> Result<(), AcpError> {
    if client
        .set_session_config_option(session_id, config_id, json!(model_id))
        .await
        .is_ok()
    {
        return Ok(());
    }
    client
        .set_session_config_option(
            session_id,
            config_id,
            json!({"type": "id", "value": model_id}),
        )
        .await?;
    Ok(())
}

/// Spawn an ACP agent, open one session, and return the models it advertises.
pub async fn probe_acp_models(options: ProbeAcpModelsOptions) -> Result<AcpModelList, AcpError> {
    let cwd = options
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let transport = Arc::new(AcpStdioTransport::new(StdioTransportOptions {
        command: options.command,
        args: options.args,
        cwd: Some(cwd.clone()),
        env: options.env,
        on_stderr: None,
    }));
    // Subscribe before start so the initialize response is not missed.
    let client = AcpClient::new(
        transport.clone() as Arc<dyn AcpTransport>,
        AcpClientOptions {
            timeout_ms: Some(options.timeout_ms.unwrap_or(DEFAULT_PROBE_TIMEOUT_MS)),
            client_info: None,
        },
    );
    transport.start().await;
    let outcome = async {
        client.initialize(None).await?;
        let created = client
            .request_value(
                "session/new",
                Some(json!({"cwd": cwd, "mcpServers": []})),
                None,
            )
            .await?;
        let extracted = extract_acp_session_models(&created);
        Ok(AcpModelList {
            models: extracted.models,
            current_model_id: extracted.current_model_id,
        })
    }
    .await;
    client.close().await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockModelClient {
        model_calls: Mutex<Vec<(String, String)>>,
        config_calls: Mutex<Vec<(String, String, Value)>>,
        fail_model: bool,
        fail_string_config: bool,
    }

    impl MockModelClient {
        fn new() -> Self {
            Self {
                model_calls: Mutex::new(Vec::new()),
                config_calls: Mutex::new(Vec::new()),
                fail_model: false,
                fail_string_config: false,
            }
        }
    }

    #[async_trait]
    impl AcpModelClient for MockModelClient {
        async fn set_session_model(
            &self,
            session_id: &str,
            model_id: &str,
        ) -> Result<Value, AcpError> {
            self.model_calls
                .lock()
                .unwrap()
                .push((session_id.to_string(), model_id.to_string()));
            if self.fail_model {
                Err(AcpError::Message("no set_model".into()))
            } else {
                Ok(json!({}))
            }
        }

        async fn set_session_config_option(
            &self,
            session_id: &str,
            config_id: &str,
            value: Value,
        ) -> Result<Value, AcpError> {
            self.config_calls.lock().unwrap().push((
                session_id.to_string(),
                config_id.to_string(),
                value.clone(),
            ));
            if self.fail_string_config && value.is_string() {
                Err(AcpError::Message("string rejected".into()))
            } else {
                Ok(json!({}))
            }
        }
    }

    #[test]
    fn reads_the_legacy_session_models_field() {
        let extracted = extract_acp_session_models(&json!({
            "sessionId": "sess_1",
            "models": {
                "currentModelId": "sonnet-4.5",
                "availableModels": [
                    {"modelId": "opus-4.6", "name": "Opus 4.6"},
                    {"id": "sonnet-4.5", "name": "Sonnet 4.5"}
                ]
            }
        }));
        assert_eq!(extracted.source, AcpModelSource::Models);
        assert_eq!(extracted.current_model_id.as_deref(), Some("sonnet-4.5"));
        assert_eq!(extracted.model_config_id, None);
        assert_eq!(
            extracted.models,
            vec![
                AcpModelInfo {
                    id: "opus-4.6".into(),
                    name: "Opus 4.6".into()
                },
                AcpModelInfo {
                    id: "sonnet-4.5".into(),
                    name: "Sonnet 4.5".into()
                },
            ]
        );
    }

    #[test]
    fn reads_a_config_option_whose_category_is_model() {
        let extracted = extract_acp_session_models(&json!({
            "sessionId": "sess_1",
            "configOptions": [
                {
                    "configId": "mode",
                    "category": "mode",
                    "type": "select",
                    "currentValue": "code",
                    "options": [{"value": "ask", "name": "Ask"}]
                },
                {
                    "id": "model",
                    "category": "model",
                    "type": "select",
                    "currentValue": {"type": "id", "value": "gpt-5.6"},
                    "options": [
                        {"value": "gpt-5.6", "name": "GPT-5.6"},
                        {"value": "gpt-5.5", "name": "GPT-5.5"}
                    ]
                }
            ]
        }));
        assert_eq!(extracted.source, AcpModelSource::ConfigOptions);
        assert_eq!(extracted.current_model_id.as_deref(), Some("gpt-5.6"));
        assert_eq!(extracted.model_config_id.as_deref(), Some("model"));
        assert_eq!(
            extracted.models,
            vec![
                AcpModelInfo {
                    id: "gpt-5.6".into(),
                    name: "GPT-5.6".into()
                },
                AcpModelInfo {
                    id: "gpt-5.5".into(),
                    name: "GPT-5.5".into()
                },
            ]
        );
    }

    #[test]
    fn returns_an_empty_list_when_the_session_has_no_model_selector() {
        let extracted = extract_acp_session_models(&json!({"sessionId": "sess_1"}));
        assert_eq!(extracted.source, AcpModelSource::None);
        assert!(extracted.models.is_empty());
        assert_eq!(extracted.current_model_id, None);
    }

    #[tokio::test]
    async fn uses_set_model_for_the_legacy_models_field() {
        let client = MockModelClient::new();
        apply_acp_session_model(
            &client,
            "sess_1",
            "opus-4.6",
            &json!({
                "models": {
                    "currentModelId": "sonnet-4.5",
                    "availableModels": [
                        {"modelId": "opus-4.6", "name": "Opus"},
                        {"modelId": "sonnet-4.5", "name": "Sonnet"}
                    ]
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            client.model_calls.lock().unwrap().as_slice(),
            [("sess_1".into(), "opus-4.6".into())]
        );
        assert!(client.config_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn uses_set_config_option_for_a_model_config_option() {
        let client = MockModelClient::new();
        apply_acp_session_model(
            &client,
            "sess_1",
            "gpt-5.6",
            &json!({
                "configOptions": [{
                    "configId": "model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "gpt-5.5",
                    "options": [{"value": "gpt-5.6", "name": "GPT-5.6"}]
                }]
            }),
        )
        .await
        .unwrap();
        assert!(client.model_calls.lock().unwrap().is_empty());
        assert_eq!(
            client.config_calls.lock().unwrap().as_slice(),
            [("sess_1".into(), "model".into(), json!("gpt-5.6"))]
        );
    }

    #[tokio::test]
    async fn skips_the_round_trip_when_the_current_model_already_matches() {
        let client = MockModelClient::new();
        apply_acp_session_model(
            &client,
            "sess_1",
            "sonnet-4.5",
            &json!({
                "models": {
                    "currentModelId": "sonnet-4.5",
                    "availableModels": [{"modelId": "sonnet-4.5", "name": "Sonnet"}]
                }
            }),
        )
        .await
        .unwrap();
        assert!(client.model_calls.lock().unwrap().is_empty());
        assert!(client.config_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn falls_back_to_config_option_object_when_string_value_is_rejected() {
        let mut client = MockModelClient::new();
        client.fail_model = true;
        client.fail_string_config = true;
        apply_acp_session_model(
            &client,
            "sess_1",
            "gpt-5.6",
            &json!({"sessionId": "sess_1"}),
        )
        .await
        .unwrap();
        let calls = client.config_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, json!("gpt-5.6"));
        assert_eq!(calls[1].2, json!({"type": "id", "value": "gpt-5.6"}));
    }
}
