//! Owns core-side persistence of plugin resources.

pub(super) mod data;

use serde::{Deserialize, Serialize};

use self::data::PluginDataStore;
use super::{Error, Result};
use crate::time::now_ms;

/// What every `Debug` in this module prints instead of stored credentials.
const REDACTED: &str = "[REDACTED]";

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

#[derive(Clone, Deserialize, Serialize)]
pub struct ResourceRecord {
    pub id: String,
    pub key: String,
    /// Credentials the plugin calls upstream with; never rendered by `Debug`.
    pub private_data: serde_json::Value,
    pub state: ResourceState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl std::fmt::Debug for ResourceRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceRecord")
            .field("id", &self.id)
            .field("key", &self.key)
            .field("private_data", &REDACTED)
            .field("state", &self.state)
            .field("created_at_ms", &self.created_at_ms)
            .field("updated_at_ms", &self.updated_at_ms)
            .finish()
    }
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

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceDraft {
    pub key: String,
    /// Credentials the plugin calls upstream with; never rendered by `Debug`.
    pub private_data: serde_json::Value,
    /// The state the plugin reports with a new resource; ready when absent.
    #[serde(default)]
    pub state: Option<ResourceState>,
}

impl std::fmt::Debug for ResourceDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceDraft")
            .field("key", &self.key)
            .field("private_data", &REDACTED)
            .field("state", &self.state)
            .finish()
    }
}

/// A partial update a plugin sends after a call: only the fields it names are
/// replaced, so an omitted `state` keeps the stored one.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcePatch {
    #[serde(default)]
    pub private_data: Option<serde_json::Value>,
    #[serde(default)]
    pub state: Option<ResourceState>,
}

impl std::fmt::Debug for ResourcePatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let private_data = if self.private_data.is_some() {
            REDACTED
        } else {
            "<unchanged>"
        };
        f.debug_struct("ResourcePatch")
            .field("private_data", &private_data)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct StoredModel {
    id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub images: bool,
    /// Credentials this model calls upstream with; never rendered by `Debug`.
    #[serde(default)]
    pub private_data: serde_json::Value,
}

impl std::fmt::Debug for StoredModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredModel")
            .field("id", &self.id)
            .field("display_name", &self.display_name)
            .field("description", &self.description)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("images", &self.images)
            .field("private_data", &REDACTED)
            .finish()
    }
}

impl StoredModel {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn with_resource_prefix(mut self, resource_id: &str) -> Self {
        self.id = format!("{resource_id}/{}", self.id);
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(super) fn for_test(id: &str, display_name: &str) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            description: None,
            max_output_tokens: None,
            images: false,
            private_data: serde_json::Value::Null,
        }
    }

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
    /// The whole list is one [`PluginDataStore::mutate`] transaction, so two
    /// concurrent drafts cannot overwrite each other with an older snapshot, and
    /// a cancelled caller cannot leave a half-applied update.
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
        let now = now_ms();
        for draft in &drafts {
            if draft.key.trim().is_empty() {
                return Err(Error::Protocol("plugin resource draft requires key".into()));
            }
        }
        self.mutate_resources(plugin_id, resource_type, move |mut records| {
            let mut outcome = UpsertOutcome {
                added: 0,
                updated: 0,
                changed_ids: Vec::new(),
            };
            for draft in drafts {
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
            Ok((records, outcome))
        })
        .await
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
        let resource_id = resource_id.to_owned();
        let now = now_ms();
        self.mutate_resources(plugin_id, resource_type, move |mut records| {
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
            record.updated_at_ms = now;
            Ok((records, ()))
        })
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
        let resource_id = resource_id.to_owned();
        self.mutate_resources(plugin_id, resource_type, move |mut records| {
            let index = records
                .iter()
                .position(|record| record.id == resource_id)
                .ok_or_else(|| Error::NotFound(format!("plugin resource {resource_id}")))?;
            let removed = records.remove(index);
            Ok((records, removed))
        })
        .await
    }

    /// Runs one read-modify-write of the whole resource list under the store's
    /// per-key lock. Every entry point above goes through here, so a UI edit and
    /// a concurrent provider-result patch cannot interleave.
    async fn mutate_resources<T, F>(
        &self,
        plugin_id: &str,
        resource_type: &str,
        change: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Vec<ResourceRecord>) -> Result<(Vec<ResourceRecord>, T)> + Send + 'static,
    {
        self.data
            .mutate(plugin_id, &resource_key(resource_type), move |existing| {
                let records: Vec<ResourceRecord> = existing
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let (records, output) = change(records)?;
                Ok((serde_json::to_value(records)?, output))
            })
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

    #[tokio::test]
    async fn concurrent_resource_updates_keep_both_changes() {
        let (_root, store) = store();
        let outcome = store
            .upsert_resources(
                "dev.example",
                "account",
                vec![
                    ResourceDraft {
                        key: "acct-1".into(),
                        private_data: serde_json::json!({"token":"one"}),
                        state: None,
                    },
                    ResourceDraft {
                        key: "acct-2".into(),
                        private_data: serde_json::json!({"token":"two"}),
                        state: None,
                    },
                ],
            )
            .await
            .unwrap();
        let mut ids = outcome.changed_ids;
        let first = ids.remove(0);
        let second = ids.remove(0);
        let (left, right) = tokio::join!(
            store.apply_patch(
                "dev.example",
                "account",
                &first,
                ResourcePatch {
                    private_data: Some(serde_json::json!({"token":"one-updated"})),
                    state: None,
                },
            ),
            store.apply_patch(
                "dev.example",
                "account",
                &second,
                ResourcePatch {
                    private_data: Some(serde_json::json!({"token":"two-updated"})),
                    state: None,
                },
            ),
        );
        left.unwrap();
        right.unwrap();
        let records = store.resources("dev.example", "account").await.unwrap();
        let tokens: Vec<&str> = records
            .iter()
            .map(|record| record.private_data["token"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(tokens, vec!["one-updated", "two-updated"]);
    }

    #[test]
    fn debug_never_prints_private_data() {
        let secret = "sentinel-secret-value";
        let record = ResourceRecord {
            id: "r1".into(),
            key: "acct".into(),
            private_data: serde_json::json!({ "token": secret }),
            state: ResourceState::Ready,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let draft = ResourceDraft {
            key: "acct".into(),
            private_data: serde_json::json!({ "token": secret }),
            state: None,
        };
        let patch = ResourcePatch {
            private_data: Some(serde_json::json!({ "token": secret })),
            state: None,
        };
        let mut model = StoredModel::for_test("m1", "Model");
        model.private_data = serde_json::json!({ "token": secret });
        for rendered in [
            format!("{record:?}"),
            format!("{draft:?}"),
            format!("{patch:?}"),
            format!("{model:?}"),
            // Nested debug output must not bypass the redaction either.
            format!("{:?}", vec![record.clone()]),
            format!("{:?}", Some(record)),
        ] {
            assert!(
                !rendered.contains(secret),
                "Debug leaked a credential: {rendered}"
            );
            assert!(
                rendered.contains("REDACTED"),
                "Debug should say so: {rendered}"
            );
        }
    }
}
