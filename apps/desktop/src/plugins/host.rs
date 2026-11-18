//! Orchestrates plugin capabilities: resources and live model catalogs.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use super::{
    catalog::{PluginCatalog, PluginEntry},
    data::PluginDataStore,
    descriptor::{
        PluginDescriptor, PluginModelDescriptor, PluginProviderDescriptor, PluginResourceDescriptor,
        PluginResourceView, ProviderDefinition, ResourceDefinition, ResourcePresentation,
        FORM_ADD_METHOD, FORM_FIELD_URL, LIVE_CATALOG,
    },
    origin,
    state::{PluginStateStore, ResourceDraft, ResourceRecord, StoredModel},
    worker::PluginWorker,
    Error, Result,
};

const LIVE_CATALOG_TTL: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<HostInner>,
}

struct HostInner {
    catalog: PluginCatalog,
    state: PluginStateStore,
    entries: RwLock<Option<Vec<PluginEntry>>>,
    workers: Mutex<HashMap<String, Arc<PluginWorker>>>,
    live_catalogs: Mutex<HashMap<LiveCatalogKey, LiveCatalogEntry>>,
    executable: RwLock<Option<std::path::PathBuf>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LiveCatalogKey {
    plugin_id: String,
    provider_id: String,
    resource_id: String,
}

#[derive(Clone, Debug)]
struct LiveCatalogEntry {
    models: Vec<StoredModel>,
    fetched_at: Instant,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormSubmitResponse {
    pub added: usize,
    pub updated: usize,
    pub model_sync_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSnapshot {
    pub runtime_ready: bool,
    pub runtime_error: Option<String>,
    pub plugins: Vec<PluginDescriptor>,
}

impl PluginHost {
    pub fn managed() -> Result<Self> {
        Ok(Self {
            inner: Arc::new(HostInner {
                catalog: PluginCatalog::managed()?,
                state: PluginStateStore::new(PluginDataStore::managed()?),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                live_catalogs: Mutex::new(HashMap::new()),
                executable: RwLock::new(find_bun()),
            }),
        })
    }

    pub async fn snapshot(&self) -> PluginSnapshot {
        let executable = self.inner.executable.read().await.clone();
        let Some(executable) = executable else {
            return PluginSnapshot {
                runtime_ready: false,
                runtime_error: Some("bun is not on PATH".into()),
                plugins: self.manifest_plugins(),
            };
        };
        let mut plugins = Vec::new();
        for entry in self.entries(&executable).await {
            plugins.push(self.descriptor(&entry, &executable).await);
        }
        PluginSnapshot {
            runtime_ready: true,
            runtime_error: None,
            plugins,
        }
    }

    pub async fn form_submit(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
        values: serde_json::Value,
    ) -> Result<FormSubmitResponse> {
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let resource = find_resource(&entry, resource_type)?;
        let method = resource
            .add
            .iter()
            .find(|method| method.id == method_id)
            .ok_or_else(|| {
                Error::Config(format!(
                    "plugin '{plugin_id}' does not define form method '{method_id}'"
                ))
            })?;
        if method.method_type != FORM_ADD_METHOD {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' add method '{method_id}' is not a form method"
            )));
        }
        let values = values
            .as_object()
            .cloned()
            .ok_or_else(|| Error::Config("form values must be an object".into()))?;
        let mut submitted = serde_json::Map::new();
        let mut submit_origins = HashSet::new();
        for field in &method.fields {
            let raw = values
                .get(&field.id)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim();
            if field.required && raw.is_empty() {
                return Err(Error::Config(format!(
                    "form field '{}' is required",
                    field.id
                )));
            }
            if field.field_type == FORM_FIELD_URL && !raw.is_empty() {
                submit_origins.insert(origin::origin_from_url(raw)?);
            }
            submitted.insert(field.id.clone(), serde_json::Value::String(raw.to_owned()));
        }
        let value = self
            .worker(&entry, &executable)
            .await
            .invoke(
                "form.submit",
                serde_json::json!({
                    "resourceType": resource_type,
                    "methodId": method.id,
                    "values": submitted,
                }),
                submit_origins,
            )
            .await?;
        let mut resources: Vec<ResourceDraft> = serde_json::from_value(value)?;
        if resources.is_empty() {
            return Err(Error::Protocol("form submit produced no resources".into()));
        }
        if entry.manifest.permissions.resource_origin {
            for draft in &mut resources {
                let origin = origin::origin_from_private_data(&draft.private_data)?;
                origin::rewrite_private_origin(&mut draft.private_data, origin);
            }
        }
        let outcome = self
            .inner
            .state
            .upsert_resources(plugin_id, resource_type, resources)
            .await?;
        self.invalidate_live_catalogs(plugin_id, None).await;
        let model_sync_error = self
            .sync_provider_models_for_resource(&entry, &executable, resource_type)
            .await;
        Ok(FormSubmitResponse {
            added: outcome.added,
            updated: outcome.updated,
            model_sync_error,
        })
    }

    pub async fn delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        find_resource(&entry, resource_type)?;
        self.inner
            .state
            .remove_resource(plugin_id, resource_type, resource_id)
            .await?;
        self.invalidate_live_catalogs(plugin_id, Some(resource_id))
            .await;
        Ok(())
    }

    pub async fn refresh_catalog(&self, plugin_id: &str, provider_id: &str) -> Result<usize> {
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?.clone();
        self.refresh_live_provider_models(&entry, &executable, &provider)
            .await
    }

    fn manifest_plugins(&self) -> Vec<PluginDescriptor> {
        self.inner
            .catalog
            .manifests()
            .into_iter()
            .map(|(manifest, icon)| PluginDescriptor {
                id: manifest.id,
                name: manifest.name,
                version: manifest.version,
                author: manifest.author,
                icon,
                runtime_ready: false,
                runtime_error: Some("bun is not on PATH".into()),
                providers: Vec::new(),
                resources: Vec::new(),
            })
            .collect()
    }

    async fn descriptor(&self, entry: &PluginEntry, executable: &Path) -> PluginDescriptor {
        let plugin_id = &entry.manifest.id;
        let mut providers = Vec::new();
        for provider in &entry.definition.providers {
            let (stored, catalog_error) = self
                .provider_models(entry, executable, provider, false)
                .await;
            let configured = self.provider_configured(entry, provider).await;
            providers.push(PluginProviderDescriptor {
                id: provider.id.clone(),
                plugin_id: plugin_id.clone(),
                display_name: provider.display_name.clone(),
                description: provider.description.clone(),
                provider_type: provider.provider_type.clone(),
                resource_type: provider.resource_type.clone(),
                has_models: provider.has_models,
                catalog: provider.catalog.clone(),
                configured,
                catalog_error,
                models: stored
                    .iter()
                    .map(|model| {
                        PluginModelDescriptor::new(
                            plugin_id,
                            &entry.manifest.name,
                            provider,
                            model,
                        )
                    })
                    .collect(),
            });
        }
        let mut resources = Vec::new();
        for definition in &entry.definition.resources {
            let records = self
                .inner
                .state
                .resources(plugin_id, &definition.resource_type)
                .await
                .unwrap_or_default();
            let views = self
                .present_resources(entry, executable, definition, &records)
                .await;
            resources.push(PluginResourceDescriptor {
                resource_type: definition.resource_type.clone(),
                display_name: definition.display_name.clone(),
                add: definition.add.clone(),
                can_refresh: definition.can_refresh,
                can_remove: definition.can_remove,
                resources: views,
            });
        }
        PluginDescriptor {
            id: plugin_id.clone(),
            name: entry.manifest.name.clone(),
            version: entry.manifest.version.clone(),
            author: entry.manifest.author.clone(),
            icon: entry.icon.clone(),
            runtime_ready: true,
            runtime_error: None,
            providers,
            resources,
        }
    }

    async fn present_resources(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        definition: &ResourceDefinition,
        records: &[ResourceRecord],
    ) -> Vec<PluginResourceView> {
        if records.is_empty() {
            return Vec::new();
        }
        let snapshots = records
            .iter()
            .map(|record| record.snapshot(&definition.resource_type))
            .collect::<Vec<_>>();
        let origins = records
            .iter()
            .flat_map(|record| self.resource_origins(entry, Some(record)))
            .collect();
        let presented = self
            .worker(entry, executable)
            .await
            .invoke(
                "resource.present",
                serde_json::json!({
                    "resourceType": definition.resource_type,
                    "resources": snapshots,
                }),
                origins,
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<Vec<ResourcePresentation>>(value).map_err(Error::from)
            });
        match presented {
            Ok(views) if views.len() == records.len() => records
                .iter()
                .zip(views)
                .map(|(record, view)| PluginResourceView::from_record(record, view))
                .collect(),
            Ok(_) | Err(_) => records
                .iter()
                .map(|record| {
                    PluginResourceView::from_record(
                        record,
                        ResourcePresentation {
                            display_name: record.key.clone(),
                            description: serde_json::Value::Null,
                            metrics: Vec::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    async fn provider_configured(
        &self,
        entry: &PluginEntry,
        provider: &ProviderDefinition,
    ) -> bool {
        let Some(resource_type) = &provider.resource_type else {
            return true;
        };
        !self
            .inner
            .state
            .resources(&entry.manifest.id, resource_type)
            .await
            .unwrap_or_default()
            .is_empty()
    }

    async fn sync_provider_models_for_resource(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        resource_type: &str,
    ) -> Option<String> {
        let mut errors = Vec::new();
        for provider in entry.definition.providers.clone() {
            if provider.resource_type.as_deref() != Some(resource_type) || !provider.has_models {
                continue;
            }
            if let Err(error) = self
                .refresh_live_provider_models(entry, executable, &provider)
                .await
            {
                errors.push(format!("{}: {error}", provider.id));
            }
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    async fn refresh_live_provider_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
    ) -> Result<usize> {
        let resource_type = provider.resource_type.as_deref().ok_or_else(|| {
            Error::Config(format!(
                "plugin provider '{}' live catalog requires a resource type",
                provider.id
            ))
        })?;
        let records = self
            .inner
            .state
            .resources(&entry.manifest.id, resource_type)
            .await?;
        if records.is_empty() {
            return Err(Error::Provider(format!(
                "plugin '{}' has no '{resource_type}' resource; add one first",
                entry.manifest.id
            )));
        }
        let mut count = 0;
        let mut errors = Vec::new();
        for record in records {
            match self
                .live_models_for_resource(entry, executable, provider, &record, true)
                .await
            {
                Ok(models) => count += models.len(),
                Err(error) => errors.push(format!("{}: {error}", record.id)),
            }
        }
        if count == 0 && !errors.is_empty() {
            return Err(Error::Provider(errors.join("; ")));
        }
        Ok(count)
    }

    async fn provider_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
        force: bool,
    ) -> (Vec<StoredModel>, Option<String>) {
        if !provider.has_models || provider.catalog != LIVE_CATALOG {
            return (Vec::new(), None);
        }
        let Some(resource_type) = provider.resource_type.as_deref() else {
            return (Vec::new(), None);
        };
        let records = self
            .inner
            .state
            .resources(&entry.manifest.id, resource_type)
            .await
            .unwrap_or_default();
        let mut models = Vec::new();
        let mut errors = Vec::new();
        for record in records {
            match self
                .live_models_for_resource(entry, executable, provider, &record, force)
                .await
            {
                Ok(resource_models) => models.extend(resource_models),
                Err(error) => errors.push(format!("{}: {error}", record.id)),
            }
        }
        (
            models,
            (!errors.is_empty()).then(|| errors.join("; ")),
        )
    }

    async fn live_models_for_resource(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
        record: &ResourceRecord,
        force: bool,
    ) -> Result<Vec<StoredModel>> {
        let key = LiveCatalogKey {
            plugin_id: entry.manifest.id.clone(),
            provider_id: provider.id.clone(),
            resource_id: record.id.clone(),
        };
        if !force {
            let catalogs = self.inner.live_catalogs.lock().await;
            if let Some(cached) = catalogs.get(&key)
                && cached.fetched_at.elapsed() < LIVE_CATALOG_TTL
            {
                return match &cached.error {
                    Some(error) if cached.models.is_empty() => {
                        Err(Error::Provider(error.clone()))
                    }
                    _ => Ok(cached.models.clone()),
                };
            }
        }
        match self
            .list_models(entry, executable, provider, Some(record))
            .await
        {
            Ok(models) => {
                let stored = models
                    .into_iter()
                    .map(|model| bind_live_model(model, &record.id))
                    .collect::<Vec<_>>();
                let mut catalogs = self.inner.live_catalogs.lock().await;
                catalogs.insert(
                    key,
                    LiveCatalogEntry {
                        models: stored.clone(),
                        fetched_at: Instant::now(),
                        error: None,
                    },
                );
                Ok(stored)
            }
            Err(error) => {
                let mut catalogs = self.inner.live_catalogs.lock().await;
                if let Some(cached) = catalogs.get_mut(&key) {
                    cached.fetched_at = Instant::now();
                    cached.error = Some(error.to_string());
                    return Ok(cached.models.clone());
                }
                catalogs.insert(
                    key,
                    LiveCatalogEntry {
                        models: Vec::new(),
                        fetched_at: Instant::now(),
                        error: Some(error.to_string()),
                    },
                );
                Err(error)
            }
        }
    }

    async fn list_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
        resource: Option<&ResourceRecord>,
    ) -> Result<Vec<StoredModel>> {
        let snapshot = resource
            .map(|record| record.snapshot(provider.resource_type.as_deref().unwrap_or_default()));
        let value = self
            .worker(entry, executable)
            .await
            .invoke(
                "models.list",
                serde_json::json!({ "providerId": provider.id, "resource": snapshot }),
                self.resource_origins(entry, resource),
            )
            .await?;
        let definitions = value
            .as_array()
            .ok_or_else(|| Error::Protocol("plugin models.list must return an array".into()))?;
        let mut models = Vec::with_capacity(definitions.len());
        let mut seen = HashSet::new();
        for definition in definitions {
            let model = StoredModel::from_definition(definition)?;
            if seen.insert(model.id.clone()) {
                models.push(model);
            }
        }
        Ok(models)
    }

    fn resource_origins(
        &self,
        entry: &PluginEntry,
        record: Option<&ResourceRecord>,
    ) -> HashSet<String> {
        if !entry.manifest.permissions.resource_origin {
            return HashSet::new();
        }
        record
            .and_then(|record| origin::origin_from_private_data(&record.private_data).ok())
            .into_iter()
            .collect()
    }

    async fn invalidate_live_catalogs(&self, plugin_id: &str, resource_id: Option<&str>) {
        self.inner.live_catalogs.lock().await.retain(|key, _| {
            key.plugin_id != plugin_id
                || resource_id.is_some_and(|resource_id| key.resource_id != resource_id)
        });
    }

    async fn executable(&self) -> Result<std::path::PathBuf> {
        self.inner
            .executable
            .read()
            .await
            .clone()
            .ok_or_else(|| Error::Runtime("bun is not on PATH".into()))
    }

    async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        if let Some(entries) = self.inner.entries.read().await.as_ref() {
            return entries.clone();
        }
        let loaded = self.inner.catalog.entries(executable).await;
        *self.inner.entries.write().await = Some(loaded.clone());
        loaded
    }

    async fn find_entry(&self, executable: &Path, plugin_id: &str) -> Result<PluginEntry> {
        self.entries(executable)
            .await
            .into_iter()
            .find(|entry| entry.manifest.id == plugin_id)
            .ok_or_else(|| Error::NotFound(format!("plugin {plugin_id}")))
    }

    async fn worker(&self, entry: &PluginEntry, executable: &Path) -> Arc<PluginWorker> {
        let mut workers = self.inner.workers.lock().await;
        workers
            .entry(entry.manifest.id.clone())
            .or_insert_with(|| {
                Arc::new(PluginWorker::new(
                    entry,
                    executable.to_path_buf(),
                    self.inner.catalog.loader().clone(),
                ))
            })
            .clone()
    }
}

fn find_provider<'a>(entry: &'a PluginEntry, provider_id: &str) -> Result<&'a ProviderDefinition> {
    entry
        .definition
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "plugin '{}' provider {provider_id}",
                entry.manifest.id
            ))
        })
}

fn find_resource<'a>(
    entry: &'a PluginEntry,
    resource_type: &str,
) -> Result<&'a ResourceDefinition> {
    entry
        .definition
        .resources
        .iter()
        .find(|resource| resource.resource_type == resource_type)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "plugin '{}' resource type {resource_type}",
                entry.manifest.id
            ))
        })
}

fn bind_live_model(mut model: StoredModel, resource_id: &str) -> StoredModel {
    model.id = format!("{resource_id}/{}", model.id);
    model
}

fn find_bun() -> Option<std::path::PathBuf> {
    which("bun")
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = directory.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_live_models_to_resource_ids() {
        let model = bind_live_model(
            StoredModel {
                id: "gpt-5".into(),
                display_name: "GPT".into(),
                description: None,
                max_output_tokens: None,
                images: false,
                private_data: serde_json::Value::Null,
            },
            "resource-1",
        );
        assert_eq!(model.id, "resource-1/gpt-5");
        assert_eq!(
            crate::plugins::parse_model_id(&format!(
                "plugin:dev.cursorbyoknext.plugins.magpie/magpie/{}",
                model.id
            )),
            Some((
                "dev.cursorbyoknext.plugins.magpie",
                "magpie",
                "resource-1/gpt-5"
            ))
        );
    }
}
