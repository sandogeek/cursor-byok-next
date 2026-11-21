//! Orchestrates plugin capabilities: resources and live model catalogs.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::{
    model::ModelInvocation,
    provider::{ModelEvent, ProviderStream},
};

use super::{
    Error, Result,
    catalog::{PluginCatalog, PluginEntry},
    data::PluginDataStore,
    descriptor::{
        FORM_ADD_METHOD, FORM_FIELD_URL, LIVE_CATALOG, PluginDescriptor, PluginModelDescriptor,
        PluginProviderDescriptor, PluginResourceDescriptor, PluginResourceView, ProviderDefinition,
        ResourceDefinition, ResourcePresentation, parse_model_id,
    },
    origin,
    state::{PluginStateStore, ResourceDraft, ResourcePatch, ResourceRecord, StoredModel, now_ms},
    wire,
    worker::{PluginWorker, WorkerStreamItem},
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
    operations: Mutex<()>,
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
    last_attempt: Instant,
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
        Ok(Self::new(
            PluginCatalog::managed()?,
            PluginDataStore::managed()?,
        ))
    }

    fn new(catalog: PluginCatalog, data: PluginDataStore) -> Self {
        Self {
            inner: Arc::new(HostInner {
                catalog,
                state: PluginStateStore::new(data),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                live_catalogs: Mutex::new(HashMap::new()),
                operations: Mutex::new(()),
                executable: RwLock::new(find_bun()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &Path) -> Result<Self> {
        Ok(Self::new(
            PluginCatalog::for_test(root)?,
            PluginDataStore::for_test(root.join("data"))?,
        ))
    }

    pub async fn snapshot(&self) -> PluginSnapshot {
        let _operation = self.inner.operations.lock().await;
        let executable = {
            let mut executable = self.inner.executable.write().await;
            if self.inner.entries.read().await.is_none() {
                *executable = find_bun();
            }
            executable.clone()
        };
        let Some(executable) = executable else {
            return PluginSnapshot {
                runtime_ready: false,
                runtime_error: Some(super::text("plugin.error.bun_missing")),
                plugins: self.manifest_plugins(),
            };
        };
        let entries = self.entries(&executable).await;
        let mut plugins = Vec::new();
        for entry in entries {
            plugins.push(self.descriptor(&entry, &executable).await);
        }
        if plugins.is_empty() {
            return PluginSnapshot {
                runtime_ready: true,
                runtime_error: Some(super::text("plugin.error.builtin_unavailable")),
                plugins,
            };
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
        let _operation = self.inner.operations.lock().await;
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
            .await
            .map_err(|_| Error::Storage(super::text("plugin.error.storage_save_failed")))?;
        let changed: HashSet<_> = outcome.changed_ids.into_iter().collect();
        self.invalidate_live_catalogs(plugin_id, &changed).await;
        let model_sync_error = self
            .sync_provider_models_for_resource(&entry, &executable, resource_type, &changed)
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
        let _operation = self.inner.operations.lock().await;
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        find_resource(&entry, resource_type)?;
        self.inner
            .state
            .remove_resource(plugin_id, resource_type, resource_id)
            .await
            .map_err(|error| match error {
                Error::NotFound(_) => error,
                _ => Error::Storage(super::text("plugin.error.storage_remove_failed")),
            })?;
        self.invalidate_live_catalogs(plugin_id, &HashSet::from([resource_id.to_owned()]))
            .await;
        Ok(())
    }

    pub async fn refresh_catalog(&self, plugin_id: &str, provider_id: &str) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?;
        self.provider_models(&entry, &executable, provider, true, None)
            .await;
        Ok(())
    }

    pub async fn configured_models(&self) -> Vec<PluginModelDescriptor> {
        let Ok(executable) = self.executable().await else {
            return Vec::new();
        };
        let mut models = Vec::new();
        for entry in self.entries(&executable).await {
            for provider in &entry.definition.providers {
                if !self.provider_configured(&entry, provider).await {
                    continue;
                }
                let (stored, _) = self
                    .provider_models(&entry, &executable, provider, false, None)
                    .await;
                models.extend(stored.into_iter().map(|model| {
                    PluginModelDescriptor::new(
                        &entry.manifest.id,
                        &entry.manifest.name,
                        provider,
                        &model,
                    )
                }));
            }
        }
        models
    }

    pub async fn model(&self, model_id: &str) -> Result<Option<PluginModelDescriptor>> {
        let Some((plugin_id, provider_id, stored_id)) = parse_model_id(model_id) else {
            return Ok(None);
        };
        let executable = self.executable().await?;
        let entry = self.find_entry(&executable, plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?;
        if !self.provider_configured(&entry, provider).await {
            return Ok(None);
        }
        let (stored, _) = self
            .provider_models(&entry, &executable, provider, false, None)
            .await;
        Ok(stored
            .into_iter()
            .find(|model| model.id == stored_id)
            .map(|model| {
                PluginModelDescriptor::new(plugin_id, &entry.manifest.name, provider, &model)
            }))
    }

    pub fn stream_model(
        &self,
        invocation: ModelInvocation,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        let host = self.clone();
        Box::pin(async_stream::try_stream! {
            let model_id = invocation.request.model.model_id.clone();
            let (plugin_id, provider_id, upstream_id) = parse_model_id(&model_id)
                .map(|(plugin, provider, model)| (plugin.to_owned(), provider.to_owned(), model.to_owned()))
                .ok_or_else(|| Error::Provider(format!("invalid plugin model ID: {model_id}")))?;
            let executable = host.executable().await?;
            let entry = host.find_entry(&executable, &plugin_id).await?;
            let provider = find_provider(&entry, &provider_id)?.clone();
            let (stored, _) = host
                .provider_models(&entry, &executable, &provider, false, None)
                .await;
            let stored = stored
                .into_iter()
                .find(|model| model.id == upstream_id)
                .ok_or_else(|| Error::NotFound(format!("plugin model {model_id}")))?;
            let resource = match &provider.resource_type {
                Some(resource_type) if provider.catalog == LIVE_CATALOG => {
                    let resource_id = live_resource_id(&upstream_id).ok_or_else(|| {
                        Error::Provider(format!("invalid live plugin model ID: {model_id}"))
                    })?;
                    Some((
                        resource_type.clone(),
                        host.find_record(&plugin_id, resource_type, resource_id)
                            .await?,
                    ))
                }
                Some(resource_type) => Some((
                    resource_type.clone(),
                    host.select_resource(&plugin_id, resource_type).await?,
                )),
                None => None,
            };
            let mut model_snapshot = stored.snapshot();
            if provider.catalog == LIVE_CATALOG
                && let Some(upstream) = live_upstream_id(&upstream_id)
                && let Some(object) = model_snapshot.as_object_mut()
            {
                object.insert("id".into(), serde_json::Value::String(upstream.to_owned()));
            }
            let request = wire::llm_request(&invocation)?;
            let params = serde_json::json!({
                "providerId": provider_id,
                "model": model_snapshot,
                "resource": resource.as_ref().map(|(resource_type, record)| record.snapshot(resource_type)),
                "request": request,
            });
            let worker = host.worker(&entry, &executable).await;
            let origins = host.resource_origins(&entry, resource.as_ref().map(|(_, record)| record));
            let mut items = worker
                .invoke_streaming_with_origins(
                    "provider.invoke",
                    params,
                    cancellation.clone(),
                    origins,
                )
                .await?;
            yield ModelEvent::Start {
                model_call_id: invocation.call_id.clone(),
            };
            while let Some(item) = items.recv().await {
                match item {
                    WorkerStreamItem::Event(event) => {
                        yield wire::model_event(&event)?;
                    }
                    WorkerStreamItem::Result(result) => {
                        let value = result?;
                        let status = value
                            .get("status")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        let patch = value
                            .get("patch")
                            .filter(|patch| !patch.is_null())
                            .map(|patch| serde_json::from_value::<ResourcePatch>(patch.clone()))
                            .transpose()?;
                        if let (Some(patch), Some((resource_type, record))) =
                            (patch, resource.as_ref())
                        {
                            if let Err(error) = host
                                .inner
                                .state
                                .apply_patch(&plugin_id, resource_type, &record.id, patch)
                                .await
                            {
                                tracing::warn!(plugin = %plugin_id, %error, "failed to apply plugin resource patch");
                            } else {
                                host.invalidate_live_catalogs(
                                    &plugin_id,
                                    &HashSet::from([record.id.clone()]),
                                )
                                .await;
                            }
                        }
                        match status {
                            "completed" => return,
                            "resource-error" | "request-error" => {
                                let message = value
                                    .get("message")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or("plugin provider call failed");
                                Err(Error::Provider(message.to_owned()))?;
                            }
                            status => {
                                Err(Error::Protocol(format!(
                                    "unknown plugin provider result: {status}"
                                )))?;
                            }
                        }
                    }
                }
            }
            Err(Error::Provider(format!(
                "plugin '{plugin_id}' worker stopped mid-stream"
            )))?;
        })
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
                runtime_error: Some(super::text("plugin.error.bun_missing")),
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
                .provider_models(entry, executable, provider, false, None)
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
                        PluginModelDescriptor::new(plugin_id, &entry.manifest.name, provider, model)
                    })
                    .collect(),
            });
        }
        let mut resources = Vec::new();
        for definition in &entry.definition.resources {
            let (views, storage_error) = match self
                .inner
                .state
                .resources(plugin_id, &definition.resource_type)
                .await
            {
                Ok(records) => (
                    self.present_resources(entry, executable, definition, &records)
                        .await,
                    None,
                ),
                Err(error) => (Vec::new(), Some(resource_storage_error(plugin_id, &error))),
            };
            resources.push(PluginResourceDescriptor {
                resource_type: definition.resource_type.clone(),
                display_name: definition.display_name.clone(),
                add: definition.add.clone(),
                can_refresh: definition.can_refresh,
                can_remove: definition.can_remove,
                storage_error,
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
        changed: &HashSet<String>,
    ) -> Option<String> {
        let mut errors = Vec::new();
        for provider in &entry.definition.providers {
            if provider.resource_type.as_deref() != Some(resource_type) {
                continue;
            }
            let (_, error) = self
                .provider_models(entry, executable, provider, true, Some(changed))
                .await;
            errors.extend(error);
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    async fn provider_models(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
        force: bool,
        only: Option<&HashSet<String>>,
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
            .await;
        let records = match records {
            Ok(records) => records,
            Err(_) => return (Vec::new(), None), // Reported on the resource descriptor.
        };
        let mut models = Vec::new();
        let mut errors = Vec::new();
        for record in records {
            if only.is_some_and(|ids| !ids.contains(&record.id)) {
                continue;
            }
            let catalog = self
                .live_models_for_resource(entry, executable, provider, &record, force)
                .await;
            models.extend(catalog.models);
            if let Some(error) = catalog.error {
                errors.push(format!("{}：{error}", record.key));
            }
        }
        (models, (!errors.is_empty()).then(|| errors.join("; ")))
    }

    async fn live_models_for_resource(
        &self,
        entry: &PluginEntry,
        executable: &Path,
        provider: &ProviderDefinition,
        record: &ResourceRecord,
        force: bool,
    ) -> LiveCatalogEntry {
        let key = LiveCatalogKey {
            plugin_id: entry.manifest.id.clone(),
            provider_id: provider.id.clone(),
            resource_id: record.id.clone(),
        };
        if !force {
            let catalogs = self.inner.live_catalogs.lock().await;
            if let Some(cached) = catalogs.get(&key)
                && cached.last_attempt.elapsed() < LIVE_CATALOG_TTL
            {
                return cached.clone();
            }
        }
        let result = self
            .list_models(entry, executable, provider, Some(record))
            .await;
        let mut catalogs = self.inner.live_catalogs.lock().await;
        let cached = catalogs.entry(key).or_insert_with(|| LiveCatalogEntry {
            models: Vec::new(),
            last_attempt: Instant::now(),
            error: None,
        });
        cached.last_attempt = Instant::now();
        match result {
            Ok(models) => {
                cached.models = models
                    .into_iter()
                    .map(|model| bind_live_model(model, &record.id))
                    .collect();
                cached.error = None;
            }
            Err(error) => cached.error = Some(error.to_string()),
        }
        cached.clone()
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

    async fn invalidate_live_catalogs(&self, plugin_id: &str, resource_ids: &HashSet<String>) {
        self.inner.live_catalogs.lock().await.retain(|key, _| {
            key.plugin_id != plugin_id || !resource_ids.contains(&key.resource_id)
        });
    }

    async fn executable(&self) -> Result<std::path::PathBuf> {
        self.inner
            .executable
            .read()
            .await
            .clone()
            .ok_or_else(|| Error::Runtime(super::text("plugin.error.bun_missing")))
    }

    async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        if let Some(entries) = self.inner.entries.read().await.as_ref() {
            return entries.clone();
        }
        let loaded = self.inner.catalog.entries(executable).await;
        if !loaded.is_empty() {
            *self.inner.entries.write().await = Some(loaded.clone());
        }
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

    async fn select_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
    ) -> Result<ResourceRecord> {
        let records = self.inner.state.resources(plugin_id, resource_type).await?;
        if records.is_empty() {
            return Err(Error::Provider(format!(
                "plugin '{plugin_id}' has no '{resource_type}' resource; add one first"
            )));
        }
        let now = now_ms();
        records
            .iter()
            .find(|record| record.state.is_ready(now))
            .or_else(|| records.first())
            .cloned()
            .ok_or_else(|| Error::Provider("no plugin resource is available".into()))
    }

    async fn find_record(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<ResourceRecord> {
        self.inner
            .state
            .resources(plugin_id, resource_type)
            .await?
            .into_iter()
            .find(|record| record.id == resource_id)
            .ok_or_else(|| Error::NotFound(format!("plugin resource {resource_id}")))
    }
}

fn resource_storage_error(plugin_id: &str, error: &Error) -> String {
    tracing::warn!(
        plugin = plugin_id,
        kind = match error {
            Error::Json(_) => "invalid JSON",
            _ => "read failed",
        },
        "plugin resource storage unavailable"
    );
    super::text("plugin.error.storage_unreadable")
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

fn live_resource_id(stored_id: &str) -> Option<&str> {
    stored_id
        .split_once('/')
        .map(|(resource_id, _)| resource_id)
}

fn live_upstream_id(stored_id: &str) -> Option<&str> {
    stored_id.split_once('/').map(|(_, upstream)| upstream)
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
    use crate::model::{ModelRequest, ModelSpec, PromptSpec};
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        sync::Notify,
    };

    const MAGPIE: &str = "dev.cursorbyoknext.plugins.magpie";
    const FAKE_KEY: &str = "integration-test-placeholder-key";

    async fn fake_gateway() -> (
        String,
        Arc<AtomicBool>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let offline = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn({
            let offline = offline.clone();
            let calls = calls.clone();
            async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let mut socket = BufReader::new(socket);
                    let mut request = String::new();
                    while socket.read_line(&mut request).await.unwrap() > 0 {
                        if request.ends_with("\r\n\r\n") {
                            break;
                        }
                    }
                    assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
                    assert!(request.contains(&format!("Bearer {FAKE_KEY}")));
                    calls.fetch_add(1, Ordering::Relaxed);
                    let (status, body) = if offline.load(Ordering::Relaxed) {
                        (
                            "503 Service Unavailable",
                            format!("untrusted response containing {FAKE_KEY}"),
                        )
                    } else {
                        ("200 OK", r#"{"data":[{"id":"openai/gpt-5","magpie_label":"GPT-5","max_output_tokens":8192,"native_endpoints":["/v1/responses"]},{"id":"openai/gpt-5","display_name":"duplicate"},{"id":"media","kind":"image"},{"id":"video","kind":"video"}]}"#.into())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .get_mut()
                        .write_all(response.as_bytes())
                        .await
                        .unwrap();
                }
            }
        });
        (origin, offline, calls, task)
    }

    #[tokio::test]
    async fn magpie_catalog_through_real_bun() {
        assert!(
            find_bun().is_some(),
            "install Bun on PATH to run the Magpie integration test"
        );
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let host = PluginHost::new(
            PluginCatalog::for_test(temp.path()).unwrap(),
            PluginDataStore::for_test(root.clone()).unwrap(),
        );
        let initial = host.snapshot().await;
        assert!(
            initial.runtime_ready && initial.plugins.iter().any(|plugin| plugin.id == MAGPIE),
            "{initial:?}"
        );
        let (origin, offline, calls, server) = fake_gateway().await;
        let (second, _, second_calls, second_server) = fake_gateway().await;
        let submit = |origin: String| {
            host.form_submit(
                MAGPIE,
                "gateway",
                "connect",
                serde_json::json!({
                    "base-url": format!("{origin}/ignored"), "api-key": FAKE_KEY,
                }),
            )
        };
        assert_eq!(submit(origin.clone()).await.unwrap().added, 1);
        let snapshot = host.snapshot().await;
        let model = &snapshot.plugins[0].providers[0].models[0];
        assert_eq!(model.display_name, "GPT-5");
        assert_eq!(model.max_output_tokens, Some(8192));
        assert_eq!(
            host.inner
                .live_catalogs
                .lock()
                .await
                .values()
                .next()
                .unwrap()
                .models[0]
                .private_data["nativeEndpoints"][0],
            "/v1/responses"
        );
        assert!(model.id.ends_with("/openai/gpt-5"));
        assert_eq!(snapshot.plugins[0].providers[0].models.len(), 1);
        assert_eq!(
            snapshot.plugins[0].resources[0].resources[0].display_name,
            origin
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        offline.store(true, Ordering::Relaxed);
        host.refresh_catalog(MAGPIE, "magpie").await.unwrap();
        let stale = host.snapshot().await;
        let provider = &stale.plugins[0].providers[0];
        assert_eq!(provider.models[0].id, model.id);
        assert!(
            provider
                .catalog_error
                .as_deref()
                .is_some_and(|error| error.contains("503"))
        );
        assert!(!serde_json::to_string(&stale).unwrap().contains(FAKE_KEY));
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        assert_eq!(submit(second).await.unwrap().added, 1);
        let merged = host.snapshot().await;
        assert_eq!(merged.plugins[0].providers[0].models.len(), 2);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(second_calls.load(Ordering::Relaxed), 1);
        let gateway = &merged.plugins[0].resources[0].resources[0];
        host.delete_resource(MAGPIE, "gateway", &gateway.id)
            .await
            .unwrap();
        let removed = host.snapshot().await;
        assert_eq!(removed.plugins[0].providers[0].models.len(), 1);
        assert!(removed.plugins[0].providers[0].catalog_error.is_none());

        std::fs::write(
            root.join(MAGPIE).join("resources-gateway.json"),
            b"invalid JSON",
        )
        .unwrap();
        let degraded = host.snapshot().await;
        assert!(degraded.plugins[0].resources[0].storage_error.is_some());
        server.abort();
        second_server.abort();
    }

    async fn invocation_gateway(
        endpoint: &'static str,
        events: &'static str,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
        Arc<Notify>,
        Arc<Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let received = Arc::new(Notify::new());
        let disconnected = Arc::new(Notify::new());
        let task = tokio::spawn({
            let requests = requests.clone();
            let received = received.clone();
            let disconnected = disconnected.clone();
            async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut header = vec![0_u8; 16 * 1024];
                    let read = socket.read(&mut header).await.unwrap();
                    let text = String::from_utf8_lossy(&header[..read]);
                    if text.starts_with("GET /v1/models ") {
                        let body = format!(
                            r#"{{"data":[{{"id":"model","display_name":"Model","native_endpoints":["{endpoint}"]}}]}}"#
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        continue;
                    }
                    assert!(text.starts_with(&format!("POST {endpoint} ")), "{text}");
                    let body_start = text.find("\r\n\r\n").unwrap() + 4;
                    let body: serde_json::Value =
                        serde_json::from_str(&text[body_start..]).unwrap();
                    requests.lock().await.push(body);
                    received.notify_waiters();
                    socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    if socket.write_all(events.as_bytes()).await.is_err() {
                        disconnected.notify_waiters();
                        continue;
                    }
                    let mut buffer = [0_u8; 1024];
                    while socket.read(&mut buffer).await.unwrap_or(0) > 0 {}
                    disconnected.notify_waiters();
                }
            }
        });
        (origin, requests, received, disconnected, task)
    }

    async fn configured_invocation_host(root: &Path, origin: String) -> (PluginHost, String) {
        std::fs::create_dir_all(root).unwrap();
        let host = PluginHost::new(
            PluginCatalog::for_test(root).unwrap(),
            PluginDataStore::for_test(root.join("data")).unwrap(),
        );
        host.form_submit(
            MAGPIE,
            "gateway",
            "connect",
            serde_json::json!({"base-url": origin, "api-key": FAKE_KEY}),
        )
        .await
        .unwrap();
        let model_id = host.configured_models().await[0].id.clone();
        (host, model_id)
    }

    fn invocation(model_id: String, parts: Vec<crate::model::ContentPart>) -> ModelInvocation {
        ModelInvocation {
            call_id: "call".into(),
            run_id: "run".into(),
            conversation_id: "conversation".into(),
            provider_call_index: 0,
            request: ModelRequest {
                prompt: PromptSpec {
                    instructions: String::new(),
                    tools: Vec::new(),
                },
                model: ModelSpec::new(model_id),
                history: vec![crate::model::ProjectedMessage {
                    message_id: "message".into(),
                    role: crate::model::Role::User,
                    content: crate::model::ProjectedContent::Parts(parts),
                }],
            },
        }
    }

    #[tokio::test]
    async fn magpie_invocation_serializes_files_and_propagates_cancellation() {
        assert!(
            find_bun().is_some(),
            "install Bun on PATH to run the Magpie integration test"
        );
        let temp = tempfile::tempdir().unwrap();
        let responses = invocation_gateway(
            "/v1/responses",
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ),
        )
        .await;
        let responses_host =
            configured_invocation_host(&temp.path().join("responses"), responses.0).await;
        let file = crate::model::ContentPart::File {
            mime_type: "application/pdf".into(),
            filename: Some("report.pdf".into()),
            data: b"%PDF-1".to_vec(),
        };
        let mut stream = responses_host.0.stream_model(
            invocation(responses_host.1, vec![file]),
            CancellationToken::new(),
        );
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let requests = responses.1.lock().await;
        assert_eq!(requests[0]["input"][0]["content"][0]["type"], "input_file");
        assert_eq!(
            requests[0]["input"][0]["content"][0]["filename"],
            "report.pdf"
        );
        assert_eq!(
            requests[0]["input"][0]["content"][0]["file_data"],
            "data:application/pdf;base64,JVBERi0x"
        );
        drop(requests);
        responses.4.abort();

        let chat = invocation_gateway("/v1/chat/completions", "data: [DONE]\n\n").await;
        let chat_host = configured_invocation_host(&temp.path().join("chat"), chat.0).await;
        let file = crate::model::ContentPart::File {
            mime_type: "application/pdf".into(),
            filename: Some("report.pdf".into()),
            data: b"%PDF-1".to_vec(),
        };
        let mut stream = chat_host.0.stream_model(
            invocation(chat_host.1, vec![file]),
            CancellationToken::new(),
        );
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
        let requests = chat.1.lock().await;
        assert_eq!(requests[0]["messages"][0]["content"][0]["type"], "file");
        assert_eq!(
            requests[0]["messages"][0]["content"][0]["file"]["filename"],
            "report.pdf"
        );
        drop(requests);
        chat.4.abort();

        let messages = invocation_gateway("/v1/messages", "").await;
        let messages_host =
            configured_invocation_host(&temp.path().join("messages"), messages.0).await;
        let file = crate::model::ContentPart::File {
            mime_type: "text/plain".into(),
            filename: Some("notes.txt".into()),
            data: b"attachment text".to_vec(),
        };
        let cancellation = CancellationToken::new();
        let mut stream = messages_host.0.stream_model(
            invocation(messages_host.1, vec![file]),
            cancellation.clone(),
        );
        assert!(matches!(
            stream.next().await,
            Some(Ok(ModelEvent::Start { .. }))
        ));
        tokio::time::timeout(Duration::from_secs(3), messages.2.notified())
            .await
            .expect("the invocation must reach the test gateway");
        let requests = messages.1.lock().await;
        assert_eq!(requests[0]["messages"][0]["content"][0]["type"], "document");
        assert_eq!(
            requests[0]["messages"][0]["content"][0]["source"]["type"],
            "plain_text"
        );
        assert_eq!(
            requests[0]["messages"][0]["content"][0]["source"]["data"],
            "attachment text"
        );
        assert_eq!(
            requests[0]["messages"][0]["content"][0]["title"],
            "notes.txt"
        );
        drop(requests);
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(3), messages.3.notified())
            .await
            .expect("cancelling an invocation must close its upstream HTTP stream");
        messages.4.abort();
    }

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
