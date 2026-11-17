use crate::buddy::store::atomic_write_json;
use crate::buddy::types::AcpModelList;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub const MODEL_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
pub const MODEL_CACHE_ERROR_TTL_MS: u64 = 45 * 1000;

#[derive(Debug, Clone)]
pub struct ModelCacheMemEntry {
    pub list: AcpModelList,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiskEntry {
    models: Vec<crate::buddy::types::AcpModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_model_id: Option<String>,
    fetched_at: String,
}

pub fn model_catalog_cache_key(
    protocol: &str,
    actor: &str,
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    cwd: Option<&str>,
) -> String {
    let env = env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    let payload = serde_json::to_string(&json!([
        protocol,
        actor,
        command,
        args,
        env,
        cwd.unwrap_or("")
    ]))
    .unwrap_or_default();
    format!("{:x}", Sha256::digest(payload.as_bytes()))
}

pub struct ModelCatalogDiskCache {
    path: PathBuf,
    entries: Arc<Mutex<Option<HashMap<String, DiskEntry>>>>,
}

impl ModelCatalogDiskCache {
    pub fn new(data_root: &std::path::Path) -> Self {
        Self {
            path: data_root.join("global").join("agent-model-cache.json"),
            entries: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn ensure_loaded(&self) {
        let mut entries = self.entries.lock().await;
        if entries.is_some() {
            return;
        }
        let mut loaded = HashMap::new();
        if let Ok(text) = tokio::fs::read_to_string(&self.path).await {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if value
                    .get("schemaVersion")
                    .and_then(serde_json::Value::as_u64)
                    == Some(2)
                {
                    if let Some(raw) = value.get("entries").and_then(serde_json::Value::as_object) {
                        for (key, value) in raw {
                            if key.len() == 64 && key.chars().all(|ch| ch.is_ascii_hexdigit()) {
                                if let Ok(entry) =
                                    serde_json::from_value::<DiskEntry>(value.clone())
                                {
                                    loaded.insert(key.clone(), entry);
                                }
                            }
                        }
                    }
                } else {
                    let _ = tokio::fs::remove_file(&self.path).await;
                }
            } else {
                let _ = tokio::fs::remove_file(&self.path).await;
            }
        }
        *entries = Some(loaded);
    }

    pub async fn get(&self, key: &str) -> Option<ModelCacheMemEntry> {
        self.ensure_loaded().await;
        let entry = self.entries.lock().await.as_ref()?.get(key)?.clone();
        let fetched_at = chrono::DateTime::parse_from_rfc3339(&entry.fetched_at)
            .ok()?
            .timestamp_millis()
            .max(0) as u64;
        Some(ModelCacheMemEntry {
            list: AcpModelList {
                models: entry.models,
                current_model_id: entry.current_model_id,
                error: None,
            },
            expires_at: fetched_at + MODEL_CACHE_TTL_MS,
        })
    }

    pub async fn set(&self, key: &str, list: &AcpModelList) -> Result<(), String> {
        if key.len() != 64 || !key.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Ok(());
        }
        self.ensure_loaded().await;
        let snapshot = {
            let mut entries = self.entries.lock().await;
            let entries = entries.get_or_insert_with(HashMap::new);
            entries.insert(
                key.to_string(),
                DiskEntry {
                    models: list.models.clone(),
                    current_model_id: list.current_model_id.clone(),
                    fetched_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                },
            );
            json!({ "schemaVersion": 2, "entries": entries })
        };
        atomic_write_json(&self.path, &snapshot)
            .await
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_sorted_digest_not_plaintext_credentials() {
        let env = HashMap::from([("TOKEN".to_string(), "secret-value".to_string())]);
        let key = model_catalog_cache_key("cli", "kimi", "kimi", &[], &env, None);
        assert_eq!(key.len(), 64);
        assert!(!key.contains("secret-value"));
        assert!(!key.contains("TOKEN"));
    }

    #[tokio::test]
    async fn disk_cache_round_trip_uses_schema_v2() {
        let temp = tempfile::tempdir().unwrap();
        let cache = ModelCatalogDiskCache::new(temp.path());
        let list = AcpModelList {
            models: vec![crate::buddy::types::AcpModelInfo {
                id: "m1".to_string(),
                name: "Model 1".to_string(),
            }],
            current_model_id: None,
            error: None,
        };
        cache.set(&"a".repeat(64), &list).await.unwrap();
        let loaded = cache.get(&"a".repeat(64)).await.unwrap();
        assert_eq!(loaded.list.models[0].id, "m1");
        let raw = tokio::fs::read_to_string(temp.path().join("global/agent-model-cache.json"))
            .await
            .unwrap();
        assert!(raw.contains("schemaVersion"));
    }
}
