//! The TTL cache in front of a provider's live model list.
//!
//! A plugin's `models.list` is a network round-trip through its worker, and
//! Cursor asks for the catalog on every picker open. This cache is the only
//! place that decides whether a read may be served from the last answer,
//! records a failed attempt so it is not retried in a tight loop, and drops
//! entries when the resource they were listed for changes.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

use super::state::StoredModel;

/// How long a provider's model list may be reused. Long enough that opening the
/// picker repeatedly costs one round-trip, short enough that a model the
/// provider just published shows up without a manual refresh.
const LIVE_CATALOG_TTL: Duration = Duration::from_secs(15);

/// A provider's model list plus the error that left it incomplete, if any.
#[derive(Debug, Default)]
pub(super) struct ProviderCatalog {
    pub(super) models: Vec<StoredModel>,
    pub(super) error: Option<String>,
}

/// Whether a catalog read may reuse the TTL cache or must re-ask the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CatalogRefresh {
    Cached,
    Forced,
}

/// The identity of one cached list: the same resource under the same provider is
/// reused, anything else is a separate answer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct LiveCatalogKey {
    pub(super) plugin: String,
    pub(super) provider: String,
    pub(super) resource: String,
}

#[derive(Clone, Debug)]
pub(super) struct LiveCatalogEntry {
    pub(super) models: Vec<StoredModel>,
    last_attempt: Instant,
    pub(super) error: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct CatalogCache {
    entries: Mutex<HashMap<LiveCatalogKey, LiveCatalogEntry>>,
}

impl CatalogCache {
    /// The cached answer for `key`, when `refresh` allows reusing it and it is
    /// younger than [`LIVE_CATALOG_TTL`]. A stale or forced read returns `None`.
    pub(super) async fn get(
        &self,
        key: &LiveCatalogKey,
        refresh: CatalogRefresh,
    ) -> Option<LiveCatalogEntry> {
        if refresh == CatalogRefresh::Forced {
            return None;
        }
        let entries = self.entries.lock().await;
        entries
            .get(key)
            .filter(|entry| entry.last_attempt.elapsed() < LIVE_CATALOG_TTL)
            .cloned()
    }

    /// Records one attempt, successful or not, and returns what a reader should
    /// see. A failure keeps the previously listed models and only attaches the
    /// new error, so a provider that just went offline still shows its last
    /// known catalog.
    pub(super) async fn store(
        &self,
        key: LiveCatalogKey,
        result: Result<Vec<StoredModel>, String>,
        resource_id: &str,
    ) -> LiveCatalogEntry {
        let mut entries = self.entries.lock().await;
        let entry = entries.entry(key).or_insert_with(|| LiveCatalogEntry {
            models: Vec::new(),
            last_attempt: Instant::now(),
            error: None,
        });
        entry.last_attempt = Instant::now();
        match result {
            Ok(models) => {
                entry.models = models
                    .into_iter()
                    .map(|model| bind_live_model(model, resource_id))
                    .collect();
                entry.error = None;
            }
            Err(error) => entry.error = Some(error),
        }
        entry.clone()
    }

    /// Every cached entry, for tests that assert on what a read recorded.
    #[cfg(test)]
    pub(super) async fn all(&self) -> HashMap<LiveCatalogKey, LiveCatalogEntry> {
        self.entries.lock().await.clone()
    }

    /// Drops every entry listed for one plugin's changed resources, so the next
    /// read asks the plugin again instead of serving a pre-change list.
    pub(super) async fn invalidate(&self, plugin_id: &str, resource_ids: &HashSet<String>) {
        self.entries
            .lock()
            .await
            .retain(|key, _| key.plugin != plugin_id || !resource_ids.contains(&key.resource));
    }
}

/// Prefixes a listed model's id with the resource it came from, so two resources
/// of the same provider cannot advertise colliding ids.
fn bind_live_model(mut model: StoredModel, resource_id: &str) -> StoredModel {
    model.id = format!("{resource_id}/{}", model.id);
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(resource: &str) -> LiveCatalogKey {
        LiveCatalogKey {
            plugin: "plugin".into(),
            provider: "provider".into(),
            resource: resource.into(),
        }
    }

    fn model(id: &str) -> StoredModel {
        StoredModel {
            id: id.into(),
            display_name: id.into(),
            description: None,
            max_output_tokens: None,
            images: false,
            private_data: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn a_failed_attempt_keeps_the_previous_models() {
        let cache = CatalogCache::default();
        cache.store(key("one"), Ok(vec![model("m1")]), "one").await;
        let after_failure = cache.store(key("one"), Err("HTTP 503".into()), "one").await;
        assert_eq!(after_failure.models[0].id, "one/m1");
        assert_eq!(after_failure.error.as_deref(), Some("HTTP 503"));
    }

    #[tokio::test]
    async fn invalidating_one_resource_keeps_its_siblings() {
        let cache = CatalogCache::default();
        for resource in ["one", "two"] {
            cache
                .store(key(resource), Ok(vec![model("m")]), resource)
                .await;
        }
        cache
            .invalidate("plugin", &HashSet::from(["one".to_owned()]))
            .await;
        assert!(
            cache
                .get(&key("one"), CatalogRefresh::Cached)
                .await
                .is_none()
        );
        assert!(
            cache
                .get(&key("two"), CatalogRefresh::Cached)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_forced_refresh_ignores_a_fresh_entry() {
        let cache = CatalogCache::default();
        cache.store(key("one"), Ok(vec![model("m")]), "one").await;
        assert!(
            cache
                .get(&key("one"), CatalogRefresh::Forced)
                .await
                .is_none()
        );
    }
}
