//! Owns core-side persistence of plugin resources.

use serde::{Deserialize, Serialize};

use super::{Error, Result, data::PluginDataStore};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResourceState {
    Ready,
    Cooling {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Invalid {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl ResourceState {
    pub fn is_ready(&self, now_ms: i64) -> bool {
        match self {
            Self::Ready => true,
            Self::Cooling { retry_at_ms, .. } => retry_at_ms.is_some_and(|at| at <= now_ms),
            Self::Invalid { .. } => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ResourceRecord {
    pub id: String,
    pub key: String,
    pub private_data: serde_json::Value,
    pub state: ResourceState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl ResourceRecord {
    pub fn snapshot(&self, resource_type: &str) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "type": resource_type,
            "key": self.key,
            "privateData": self.private_data,
            "state": state_json(&self.state),
        })
    }
}

fn state_json(state: &ResourceState) -> serde_json::Value {
    match state {
        ResourceState::Ready => serde_json::json!({ "status": "ready" }),
        ResourceState::Cooling {
            retry_at_ms,
            message,
        } => serde_json::json!({
            "status": "cooling",
            "retryAtMs": retry_at_ms,
            "message": message,
        }),
        ResourceState::Invalid { message } => serde_json::json!({
            "status": "invalid",
            "message": message,
        }),
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDraft {
    pub key: String,
    pub private_data: serde_json::Value,
    #[serde(default)]
    pub state: Option<ResourceStateInput>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePatch {
    #[serde(default)]
    pub private_data: Option<serde_json::Value>,
    #[serde(default)]
    pub state: Option<ResourceStateInput>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ResourceStateInput {
    Ready,
    Cooling {
        #[serde(default, rename = "retryAtMs")]
        retry_at_ms: Option<i64>,
        #[serde(default)]
        message: Option<String>,
    },
    Invalid {
        #[serde(default)]
        message: Option<String>,
    },
}

impl From<ResourceStateInput> for ResourceState {
    fn from(input: ResourceStateInput) -> Self {
        match input {
            ResourceStateInput::Ready => Self::Ready,
            ResourceStateInput::Cooling {
                retry_at_ms,
                message,
            } => Self::Cooling {
                retry_at_ms,
                message,
            },
            ResourceStateInput::Invalid { message } => Self::Invalid { message },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredModel {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub private_data: serde_json::Value,
}

impl StoredModel {
    pub fn from_definition(value: &serde_json::Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| Error::Protocol("plugin model definition must be an object".into()))?;
        let id = object
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| Error::Protocol("plugin model definition requires id".into()))?;
        let display_name = object
            .get("displayName")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                Error::Protocol("plugin model definition requires displayName".into())
            })?;
        let capabilities = object
            .get("capabilities")
            .and_then(|value| value.as_object());
        let images = capabilities
            .and_then(|value| value.get("images"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        Ok(Self {
            id: id.to_owned(),
            display_name: display_name.to_owned(),
            description: object
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            max_output_tokens: object
                .get("maxOutputTokens")
                .and_then(serde_json::Value::as_u64),
            images,
            private_data: object
                .get("privateData")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        })
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "displayName": self.display_name,
            "description": self.description,
            "maxOutputTokens": self.max_output_tokens,
            "capabilities": { "images": self.images },
            "privateData": self.private_data,
        })
    }
}

#[derive(Clone)]
pub struct PluginStateStore {
    data: PluginDataStore,
}

pub struct UpsertOutcome {
    pub added: usize,
    pub updated: usize,
    pub changed_ids: Vec<String>,
}

impl PluginStateStore {
    pub fn new(data: PluginDataStore) -> Self {
        Self { data }
    }

    pub async fn resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<Vec<ResourceRecord>> {
        let value = self
            .data
            .read(plugin_id, &resource_key(resource_type))
            .await?;
        if value.is_null() {
            return Ok(Vec::new());
        }
        Ok(serde_json::from_value(value)?)
    }

    pub async fn upsert_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        drafts: Vec<ResourceDraft>,
    ) -> Result<UpsertOutcome> {
        let mut records = self.resources(plugin_id, resource_type).await?;
        let now = now_ms();
        let mut outcome = UpsertOutcome {
            added: 0,
            updated: 0,
            changed_ids: Vec::new(),
        };
        for draft in drafts {
            if draft.key.trim().is_empty() {
                return Err(Error::Protocol("plugin resource draft requires key".into()));
            }
            let state = draft
                .state
                .map_or(ResourceState::Ready, ResourceState::from);
            match records.iter_mut().find(|record| record.key == draft.key) {
                Some(existing) => {
                    existing.private_data = draft.private_data;
                    existing.state = state;
                    existing.updated_at_ms = now;
                    outcome.changed_ids.push(existing.id.clone());
                    outcome.updated += 1;
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    outcome.changed_ids.push(id.clone());
                    records.push(ResourceRecord {
                        id,
                        key: draft.key,
                        private_data: draft.private_data,
                        state,
                        created_at_ms: now,
                        updated_at_ms: now,
                    });
                    outcome.added += 1;
                }
            }
        }
        self.save_resources(plugin_id, resource_type, &records)
            .await?;
        Ok(outcome)
    }

    pub async fn apply_patch(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
        patch: ResourcePatch,
    ) -> Result<()> {
        let mut records = self.resources(plugin_id, resource_type).await?;
        let record = records
            .iter_mut()
            .find(|record| record.id == resource_id)
            .ok_or_else(|| Error::NotFound(format!("plugin resource {resource_id}")))?;
        if let Some(private_data) = patch.private_data {
            record.private_data = private_data;
        }
        if let Some(state) = patch.state {
            record.state = state.into();
        }
        record.updated_at_ms = now_ms();
        self.save_resources(plugin_id, resource_type, &records)
            .await
    }

    pub async fn remove_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ResourceRecord> {
        let mut records = self.resources(plugin_id, resource_type).await?;
        let index = records
            .iter()
            .position(|record| record.id == resource_id)
            .ok_or_else(|| Error::NotFound(format!("plugin resource {resource_id}")))?;
        let removed = records.remove(index);
        self.save_resources(plugin_id, resource_type, &records)
            .await?;
        Ok(removed)
    }

    async fn save_resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
        records: &[ResourceRecord],
    ) -> Result<()> {
        self.data
            .update(
                plugin_id,
                &resource_key(resource_type),
                &serde_json::to_value(records)?,
            )
            .await
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

fn resource_key(resource_type: &str) -> String {
    format!("resources-{resource_type}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PluginStateStore) {
        let root = tempfile::tempdir().unwrap();
        let data = PluginDataStore::for_test(root.path().join("data")).unwrap();
        (root, PluginStateStore::new(data))
    }

    #[tokio::test]
    async fn upserts_resources_by_key() {
        let (_root, store) = store();
        let outcome = store
            .upsert_resources(
                "dev.example",
                "account",
                vec![ResourceDraft {
                    key: "acct-1".into(),
                    private_data: serde_json::json!({"token":"one"}),
                    state: None,
                }],
            )
            .await
            .unwrap();
        assert_eq!(outcome.added, 1);
        let outcome = store
            .upsert_resources(
                "dev.example",
                "account",
                vec![ResourceDraft {
                    key: "acct-1".into(),
                    private_data: serde_json::json!({"token":"two"}),
                    state: None,
                }],
            )
            .await
            .unwrap();
        assert_eq!(outcome.updated, 1);
        let records = store.resources("dev.example", "account").await.unwrap();
        assert_eq!(records[0].private_data["token"], "two");
        assert!(!serde_json::to_string(&records).unwrap().contains("display"));
    }
}
