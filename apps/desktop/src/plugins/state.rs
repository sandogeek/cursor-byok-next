//! Owns core-side persistence of plugin resources.

use serde::{Deserialize, Serialize};

use super::{Error, Result, data::PluginDataStore};
use crate::time::now_ms;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ResourceState {
    Ready,
    /// Retryable failure. `retry_at_ms` is the point the resource becomes
    /// usable again; without one the resource stays out of rotation until the
    /// user acts on it.
    Cooling {
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "retryAtMs")]
        retry_at_ms: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Needs user action (a bad key, a removed project). Never retried
    /// automatically.
    Invalid {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl ResourceState {
    /// Whether the resource may serve a call now. A cooling resource without a
    /// retry time stays out until something re-states it.
    #[must_use]
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
    /// The record as a plugin sees it: identity, private data, and state. The
    /// private data is included because plugins need the credentials to call
    /// upstream; nothing here is shown to the user.
    #[must_use]
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
    /// The state the plugin reports with a new resource; ready when absent.
    #[serde(default)]
    pub state: Option<ResourceState>,
}

/// A partial update a plugin sends after a call: only the fields it names are
/// replaced, so an omitted `state` keeps the stored one.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePatch {
    #[serde(default)]
    pub private_data: Option<serde_json::Value>,
    #[serde(default)]
    pub state: Option<ResourceState>,
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
    /// Reads one `models.list` entry.
    ///
    /// # Errors
    ///
    /// Fails when the entry is not an object, or `id` or `displayName` is
    /// missing or blank.
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

    /// The model as a plugin sees it, in the same shape `models.list` returns so a
    /// plugin can round-trip what it stored.
    #[must_use]
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

#[derive(Clone, Debug)]
pub struct PluginStateStore {
    data: PluginDataStore,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpsertOutcome {
    pub added: usize,
    pub updated: usize,
    pub changed_ids: Vec<String>,
}

impl PluginStateStore {
    /// Wraps the data store this state persists through.
    pub fn new(data: PluginDataStore) -> Self {
        Self { data }
    }

    /// Every stored resource of `resource_type`, in insertion order. A missing
    /// file is an empty list, not an error.
    ///
    /// # Errors
    ///
    /// Fails when the stored JSON cannot be read or decoded.
    pub async fn resources(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<Vec<ResourceRecord>> {
        let Some(value) = self
            .data
            .read(plugin_id, &resource_key(resource_type))
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_value(value)?)
    }

    /// Applies a plugin's drafts, matching them to stored resources by `key`.
    ///
    /// # Errors
    ///
    /// Fails when a draft has a blank key, or the stored list cannot be read or
    /// written.
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
            let state = draft.state.unwrap_or(ResourceState::Ready);
            if let Some(existing) = records.iter_mut().find(|record| record.key == draft.key) {
                existing.private_data = draft.private_data;
                existing.state = state;
                existing.updated_at_ms = now;
                outcome.changed_ids.push(existing.id.clone());
                outcome.updated += 1;
            } else {
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
        self.save_resources(plugin_id, resource_type, &records)
            .await?;
        Ok(outcome)
    }

    /// Applies a partial update to one resource.
    ///
    /// # Errors
    ///
    /// Fails when the resource is unknown, or the stored list cannot be read or
    /// written.
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
            record.state = state;
        }
        record.updated_at_ms = now_ms();
        self.save_resources(plugin_id, resource_type, &records)
            .await
    }

    /// Deletes one resource and returns the record that was removed.
    ///
    /// # Errors
    ///
    /// Fails when the resource is unknown, or the stored list cannot be read or
    /// written.
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

fn resource_key(resource_type: &str) -> String {
    format!("resources-{resource_type}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PluginStateStore) {
        let root = tempfile::tempdir().unwrap();
        let data = PluginDataStore::at(root.path().join("data")).unwrap();
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
