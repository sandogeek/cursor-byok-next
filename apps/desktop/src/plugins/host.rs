//! Orchestrates plugin capabilities: resources and live model catalogs.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde::{Deserialize as _, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::calls::CallRecorder;
use crate::time::now_ms;
use crate::{
    model::ModelInvocation,
    provider::{ModelEvent, ProviderStream},
};

use super::{
    Error, Result,
    catalog::{
        PluginCatalog, PluginEntry,
        cache::{CatalogCache, CatalogRefresh, LiveCatalogEntry, LiveCatalogKey, ProviderCatalog},
    },
    descriptor::{
        FormFieldKind, ModelSource, PluginDescriptor, PluginModelDescriptor,
        PluginProviderDescriptor, PluginResourceDescriptor, PluginResourceView, ResourceDefinition,
        ResourcePresentation, ValidatedProvider, parse_model_id,
    },
    invocation::WorkerStreamItem,
    origin,
    state::{
        PluginStateStore, ResourceDraft, ResourcePatch, ResourceRecord, StoredModel,
        data::PluginDataStore,
    },
    worker::{PluginWorker, WorkerRequest},
};

#[derive(Clone)]
pub struct PluginHost {
    inner: Arc<HostInner>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PluginHost").finish_non_exhaustive()
    }
}

struct HostInner {
    catalog: PluginCatalog,
    state: PluginStateStore,
    entries: RwLock<Option<Arc<[Arc<PluginEntry>]>>>,
    workers: Mutex<HashMap<String, Arc<PluginWorker>>>,
    catalogs: CatalogCache,
    /// Serializes the mutations that read the catalog, call into the plugin and
    /// then write plugin state. The worker round-trip runs while this is held on
    /// purpose: two interleaved form submits could otherwise write resources
    /// derived from the same stale snapshot.
    operations: Mutex<()>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormSubmitResponse {
    /// How many resources the submit created.
    pub added: usize,
    /// How many existing resources it replaced, matched by `key`.
    pub updated: usize,
    /// Why the follow-up model sync failed, when it did. The resources were
    /// still saved; this only means their catalog is stale.
    pub model_sync_error: Option<String>,
}

/// What the Plugins screen renders. `runtime_ready` is false when `QuickJS` is
/// unavailable, in which case `plugins` came from manifests alone.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSnapshot {
    pub runtime_ready: bool,
    pub runtime_error: Option<String>,
    pub plugins: Vec<PluginDescriptor>,
}

impl PluginHost {
    /// Opens the app's managed plugin host under the app data directory.
    ///
    /// # Errors
    ///
    /// Fails when the data directory cannot be resolved or the plugin data
    /// store cannot be created.
    pub fn managed() -> Result<Self> {
        let data_dir = crate::storage::data_dir()?;
        Ok(Self::new(
            PluginCatalog::managed(),
            PluginDataStore::at(data_dir.join("plugins/data"))?,
        ))
    }

    fn new(catalog: PluginCatalog, data: PluginDataStore) -> Self {
        Self {
            inner: Arc::new(HostInner {
                catalog,
                state: PluginStateStore::new(data),
                entries: RwLock::new(None),
                workers: Mutex::new(HashMap::new()),
                catalogs: CatalogCache::default(),
                operations: Mutex::new(()),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Result<Self> {
        Ok(Self::new(
            PluginCatalog::for_test(root),
            PluginDataStore::at(root.join("data"))?,
        ))
    }

    /// Everything the Plugins screen renders: the `QuickJS` runtime state plus each
    /// plugin's providers, resources, and cached catalogs.
    ///
    /// Never fails: a failed VM initialization is reported as `runtime_ready: false`
    /// with the manifests it could still read, so the UI can show plugin cards
    /// if VM initialization fails.
    pub async fn snapshot(&self) -> PluginSnapshot {
        let entries = self.entries().await;
        let mut plugins = Vec::with_capacity(entries.len());
        for entry in entries.iter() {
            plugins.push(self.descriptor(entry).await);
        }
        if plugins.is_empty() {
            return PluginSnapshot {
                runtime_ready: false,
                runtime_error: Some(super::text("plugin.error.vm_failed")),
                plugins: Self::manifest_plugins(),
            };
        }
        PluginSnapshot {
            runtime_ready: true,
            runtime_error: None,
            plugins,
        }
    }

    /// Runs one plugin add-method form and persists the resources it produces.
    ///
    /// # Errors
    ///
    /// Fails when the runtime, plugin, resource type or method is unknown; a
    /// required field is empty; a URL field is not a usable origin; the plugin
    /// returns no resources; or the resources cannot be saved.
    pub async fn form_submit(
        &self,
        plugin_id: &str,
        resource_type: &str,
        method_id: &str,
        values: serde_json::Value,
    ) -> Result<FormSubmitResponse> {
        let _operation = self.inner.operations.lock().await;
        let entry = self.find_entry(plugin_id).await?;
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
        let serde_json::Value::Object(form) = values else {
            return Err(Error::Config("form values must be an object".into()));
        };
        let mut submitted = serde_json::Map::new();
        let mut submit_origins = HashSet::new();
        for field in &method.fields {
            let raw = form
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
            if field.field_type == FormFieldKind::Url && !raw.is_empty() {
                submit_origins.insert(origin::origin_from_url(raw)?);
            }
            submitted.insert(field.id.clone(), serde_json::Value::String(raw.to_owned()));
        }
        let value = self
            .worker(&entry)
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
        if entry.manifest.resource_origin_policy().is_allowed() {
            for draft in &mut resources {
                let origin = origin::origin_from_private_data(&draft.private_data)?;
                origin::rewrite_private_origin(&mut draft.private_data, &origin);
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
            .sync_provider_models_for_resource(&entry, resource_type, &changed)
            .await;
        Ok(FormSubmitResponse {
            added: outcome.added,
            updated: outcome.updated,
            model_sync_error,
        })
    }

    /// Removes one stored resource and drops its cached catalog.
    ///
    /// # Errors
    ///
    /// Fails when the runtime, plugin, resource type or resource is unknown, or
    /// the store cannot be written.
    pub async fn delete_resource(
        &self,
        plugin_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let entry = self.find_entry(plugin_id).await?;
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

    /// Re-asks one provider for its model list and updates the cache.
    ///
    /// A refresh the provider rejects is not an error for the caller: the cached
    /// models stay usable and the failure shows up as `catalogError` in the next
    /// [`PluginHost::snapshot`], next to the button that asked for it.
    ///
    /// # Errors
    ///
    /// Fails when the runtime is missing or the plugin or provider is unknown.
    pub async fn refresh_catalog(&self, plugin_id: &str, provider_id: &str) -> Result<()> {
        let _operation = self.inner.operations.lock().await;
        let entry = self.find_entry(plugin_id).await?;
        let provider = find_provider(&entry, provider_id)?;
        self.provider_models(&entry, provider, CatalogRefresh::Forced, None)
            .await;
        Ok(())
    }

    /// Every model of every configured provider, in plugin-declared order.
    /// Best-effort: an unreadable runtime or a failing catalog yields the models
    /// that could still be loaded, because Cursor's model picker must not fail
    /// to open.
    pub async fn configured_models(&self) -> Vec<PluginModelDescriptor> {
        let entries = self.entries().await;
        let mut models = Vec::new();
        for entry in entries.iter() {
            for provider in &entry.providers {
                if !self.provider_configured(entry, provider).await {
                    continue;
                }
                let stored = self
                    .provider_models(entry, provider, CatalogRefresh::Cached, None)
                    .await
                    .models;
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

    /// Resolves one `plugin:` model id to a descriptor, or `None` when the id is
    /// not a plugin id or its provider is not configured.
    ///
    /// # Errors
    ///
    /// Fails when the runtime, plugin or provider is unknown.
    pub async fn model(&self, model_id: &str) -> Result<Option<PluginModelDescriptor>> {
        let Some(id) = parse_model_id(model_id) else {
            return Ok(None);
        };
        let entry = self.find_entry(id.plugin).await?;
        let provider = find_provider(&entry, id.provider)?;
        if !self.provider_configured(&entry, provider).await {
            return Ok(None);
        }
        let stored = self
            .provider_models(&entry, provider, CatalogRefresh::Cached, None)
            .await
            .models;
        Ok(stored
            .into_iter()
            .find(|model| model.id() == id.model)
            .map(|model| {
                PluginModelDescriptor::new(id.plugin, &entry.manifest.name, provider, &model)
            }))
    }

    /// Turns one `plugin:` model into a provider stream the local loop can serve.
    ///
    /// The stream emits the plugin's events in order and finishes with the
    /// plugin's terminal result; a plugin that stops without one is an error
    /// rather than a silent end, because the loop would otherwise record a
    /// truncated turn as complete.
    pub fn stream_model(
        &self,
        mut invocation: ModelInvocation,
        cancellation: CancellationToken,
        recorder: Option<CallRecorder>,
    ) -> ProviderStream {
        let host = self.clone();
        Box::pin(async_stream::try_stream! {
            let parsed = parse_model_id(&invocation.request.model.model_id).ok_or_else(|| {
                Error::Provider(format!(
                    "invalid plugin model ID: {}",
                    invocation.request.model.model_id
                ))
            })?;
            let plugin_id = parsed.plugin.to_owned();
            let provider_id = parsed.provider.to_owned();
            let upstream_id = parsed.model.to_owned();
                let entry = host.find_entry(&plugin_id).await?;
            let provider = find_provider(&entry, &provider_id)?;
            let stored = host
                .provider_models(&entry, provider, CatalogRefresh::Cached, None)
                .await
                .models;
            let stored = stored
                .into_iter()
                .find(|model| model.id() == upstream_id)
                .ok_or_else(|| {
                    Error::NotFound(format!(
                        "plugin model {}",
                        invocation.request.model.model_id
                    ))
                })?;
            let resource = host
                .invocation_resource(&plugin_id, provider, &upstream_id)
                .await?;
            let mut model_snapshot = stored.snapshot();
            if matches!(provider.models, ModelSource::Live { .. })
                && let Some(upstream) = live_upstream_id(&upstream_id)
                && let Some(object) = model_snapshot.as_object_mut()
            {
                object.insert("id".into(), serde_json::Value::String(upstream.to_owned()));
            }
            let params = serde_json::json!({
                "providerId": provider.id(),
                "model": model_snapshot,
                "resource": resource.as_ref().map(|(resource_type, record)| record.snapshot(resource_type)),
            });
            let worker = host.worker(&entry).await;
            let origins = Self::resource_origins(&entry, resource.as_ref().map(|(_, record)| record));
            if invocation.request.model.max_output_tokens.is_none() {
                invocation.request.model.max_output_tokens = stored.max_output_tokens;
            }
            let invocation = Arc::new(invocation);
            let mut items = worker
                .invoke_streaming(WorkerRequest {
                    method: "provider.invoke",
                    params,
                    cancellation,
                    recorder,
                    allowed_origins: origins,
                    model: Some(Arc::clone(&invocation)),
                })
                .await?;
            yield ModelEvent::Start {
                model_call_id: invocation.call_id.clone(),
            };
            while let Some(item) = items.recv().await {
                match item {
                    WorkerStreamItem::Event(event) => yield event,
                    WorkerStreamItem::Result(result) => {
                        let value = result?;
                        let bound_resource = resource.as_ref().map(|(kind, record)| (*kind, record));
                        if host
                            .apply_invocation_result(&plugin_id, bound_resource, &value)
                            .await?
                        {
                            return;
                        }
                    }
                }
            }
            Err(Error::Provider(format!(
                "plugin '{plugin_id}' worker stopped mid-stream"
            )))?;
        })
    }

    /// Resolves which stored resource the invocation binds to, if the provider
    /// advertises one. The type is borrowed from the provider declaration.
    async fn invocation_resource<'a>(
        &self,
        plugin_id: &str,
        provider: &'a ValidatedProvider,
        upstream_id: &str,
    ) -> Result<Option<(&'a str, ResourceRecord)>> {
        let Some(resource_type) = provider.bound_resource_type() else {
            return Ok(None);
        };
        if !matches!(provider.models, ModelSource::Live { .. }) {
            let record = self.select_resource(plugin_id, resource_type).await?;
            return Ok(Some((resource_type, record)));
        }
        let resource_id = live_resource_id(upstream_id).ok_or_else(|| {
            Error::Provider(format!("invalid live plugin model ID: {upstream_id}"))
        })?;
        let record = self
            .find_record(plugin_id, resource_type, resource_id)
            .await?;
        Ok(Some((resource_type, record)))
    }

    /// Applies the plugin's terminal result. Returns `Ok(true)` when the call
    /// reported `completed`, and an error for the two error statuses.
    async fn apply_invocation_result(
        &self,
        plugin_id: &str,
        resource: Option<(&str, &ResourceRecord)>,
        value: &serde_json::Value,
    ) -> Result<bool> {
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let patch = value
            .get("patch")
            .filter(|patch| !patch.is_null())
            .map(ResourcePatch::deserialize)
            .transpose()?;
        if let (Some(patch), Some((resource_type, record))) = (patch, resource) {
            if let Err(error) = self
                .inner
                .state
                .apply_patch(plugin_id, resource_type, record.id.as_str(), patch)
                .await
            {
                tracing::warn!(plugin = %plugin_id, %error, "failed to apply plugin resource patch");
            } else {
                self.invalidate_live_catalogs(plugin_id, &HashSet::from([record.id.clone()]))
                    .await;
            }
        }
        match status {
            "completed" => Ok(true),
            "resource-error" | "request-error" => Err(Error::Provider(
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("plugin provider call failed")
                    .to_owned(),
            )),
            status => Err(Error::Protocol(format!(
                "unknown plugin provider result: {status}"
            ))),
        }
    }

    fn manifest_plugins() -> Vec<PluginDescriptor> {
        PluginCatalog::manifests()
            .into_iter()
            .map(|loaded| PluginDescriptor {
                id: loaded.manifest.id,
                name: loaded.manifest.name,
                version: loaded.manifest.version,
                author: loaded.manifest.author,
                icon: loaded.icon,
                runtime_ready: false,
                runtime_error: Some(super::text("plugin.error.vm_failed")),
                providers: Vec::new(),
                resources: Vec::new(),
            })
            .collect()
    }

    async fn descriptor(&self, entry: &PluginEntry) -> PluginDescriptor {
        let plugin_id = &entry.manifest.id;
        let mut providers = Vec::new();
        for provider in &entry.providers {
            let ProviderCatalog {
                models: stored,
                error: catalog_error,
            } = self
                .provider_models(entry, provider, CatalogRefresh::Cached, None)
                .await;
            let configured = self.provider_configured(entry, provider).await;
            let definition = &provider.definition;
            providers.push(PluginProviderDescriptor {
                id: definition.id.clone(),
                plugin_id: plugin_id.clone(),
                display_name: definition.display_name.clone(),
                description: definition.description.clone(),
                provider_type: definition.provider_type.clone(),
                resource_type: definition.resource_type.clone(),
                has_models: definition.has_models,
                catalog: definition.catalog,
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
                    self.present_resources(entry, definition, &records).await,
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
            .flat_map(|record| Self::resource_origins(entry, Some(record)))
            .collect();
        let presented = self
            .worker(entry)
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
            Ok(views) => {
                tracing::warn!(
                    plugin = %entry.manifest.id,
                    resource_type = %definition.resource_type,
                    expected = records.len(),
                    presented = views.len(),
                    "plugin returned a resource presentation of the wrong length"
                );
                Self::fallback_views(records)
            }
            Err(error) => {
                tracing::warn!(
                    plugin = %entry.manifest.id,
                    resource_type = %definition.resource_type,
                    %error,
                    "plugin could not present its resources"
                );
                Self::fallback_views(records)
            }
        }
    }

    /// Renders every record with no plugin-supplied presentation, so a failed
    /// or malformed `resource.present` never hides resources the user stored.
    fn fallback_views(records: &[ResourceRecord]) -> Vec<PluginResourceView> {
        records
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
            .collect()
    }

    async fn provider_configured(&self, entry: &PluginEntry, provider: &ValidatedProvider) -> bool {
        let Some(resource_type) = provider.bound_resource_type() else {
            return true;
        };
        match self
            .inner
            .state
            .resources(&entry.manifest.id, resource_type)
            .await
        {
            Ok(records) => !records.is_empty(),
            // A provider whose resources cannot be read is not usable; the
            // failure is already reported on the resource descriptor.
            Err(error) => {
                tracing::warn!(
                    plugin = %entry.manifest.id,
                    %error,
                    "could not read plugin resources to decide if the provider is configured"
                );
                false
            }
        }
    }

    async fn sync_provider_models_for_resource(
        &self,
        entry: &PluginEntry,
        resource_type: &str,
        changed: &HashSet<String>,
    ) -> Option<String> {
        let mut errors = Vec::new();
        for provider in &entry.providers {
            if provider.bound_resource_type() != Some(resource_type) {
                continue;
            }
            let catalog = self
                .provider_models(entry, provider, CatalogRefresh::Forced, Some(changed))
                .await;
            errors.extend(catalog.error);
        }
        (!errors.is_empty()).then(|| errors.join("; "))
    }

    async fn provider_models(
        &self,
        entry: &PluginEntry,
        provider: &ValidatedProvider,
        refresh: CatalogRefresh,
        only: Option<&HashSet<String>>,
    ) -> ProviderCatalog {
        let ModelSource::Live { resource_type } = &provider.models else {
            return ProviderCatalog::default();
        };
        let records = self
            .inner
            .state
            .resources(&entry.manifest.id, resource_type)
            .await;
        let Ok(records) = records else {
            // Reported on the resource descriptor instead.
            return ProviderCatalog::default();
        };
        let mut models = Vec::new();
        let mut errors = Vec::new();
        for record in records {
            if only.is_some_and(|ids| !ids.contains(&record.id)) {
                continue;
            }
            let catalog = self
                .live_models_for_resource(entry, provider, &record, refresh)
                .await;
            models.extend(catalog.models);
            if let Some(error) = catalog.error {
                errors.push(format!("{}: {error}", record.key));
            }
        }
        ProviderCatalog {
            models,
            error: (!errors.is_empty()).then(|| errors.join("; ")),
        }
    }

    async fn live_models_for_resource(
        &self,
        entry: &PluginEntry,
        provider: &ValidatedProvider,
        record: &ResourceRecord,
        refresh: CatalogRefresh,
    ) -> LiveCatalogEntry {
        let key = LiveCatalogKey {
            plugin: entry.manifest.id.clone(),
            provider: provider.id().to_owned(),
            resource: record.id.clone(),
        };
        if let Some(cached) = self.inner.catalogs.get(&key, refresh).await {
            return cached;
        }
        let result = self
            .list_models(entry, provider, Some(record))
            .await
            .map_err(|error| error.to_string());
        self.inner.catalogs.store(key, result, &record.id).await
    }

    async fn list_models(
        &self,
        entry: &PluginEntry,
        provider: &ValidatedProvider,
        resource: Option<&ResourceRecord>,
    ) -> Result<Vec<StoredModel>> {
        // A resource record is only meaningful with the type it was stored
        // under, so a provider that declares no type gets no snapshot.
        let snapshot = match (provider.bound_resource_type(), resource) {
            (Some(resource_type), Some(record)) => Some(record.snapshot(resource_type)),
            _ => None,
        };
        let value = self
            .worker(entry)
            .await
            .invoke(
                "models.list",
                serde_json::json!({ "providerId": provider.id(), "resource": snapshot }),
                Self::resource_origins(entry, resource),
            )
            .await?;
        let definitions = value
            .as_array()
            .ok_or_else(|| Error::Protocol("plugin models.list must return an array".into()))?;
        let mut models = Vec::with_capacity(definitions.len());
        let mut seen = HashSet::new();
        for definition in definitions {
            let model = StoredModel::from_definition(definition)?;
            if seen.insert(model.id().to_owned()) {
                models.push(model);
            }
        }
        Ok(models)
    }

    fn resource_origins(entry: &PluginEntry, record: Option<&ResourceRecord>) -> HashSet<String> {
        if !entry.manifest.resource_origin_policy().is_allowed() {
            return HashSet::new();
        }
        record
            .and_then(|record| origin::origin_from_private_data(&record.private_data).ok())
            .into_iter()
            .collect()
    }

    async fn invalidate_live_catalogs(&self, plugin_id: &str, resource_ids: &HashSet<String>) {
        self.inner
            .catalogs
            .invalidate(plugin_id, resource_ids)
            .await;
    }

    async fn entries(&self) -> Arc<[Arc<PluginEntry>]> {
        if let Some(entries) = self.inner.entries.read().await.as_ref() {
            return Arc::clone(entries);
        }
        let loaded: Arc<[Arc<PluginEntry>]> = self
            .inner
            .catalog
            .entries()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(Arc::new)
            .collect();
        if !loaded.is_empty() {
            *self.inner.entries.write().await = Some(Arc::clone(&loaded));
        }
        loaded
    }

    async fn find_entry(&self, plugin_id: &str) -> Result<Arc<PluginEntry>> {
        self.entries()
            .await
            .iter()
            .find(|entry| entry.manifest.id == plugin_id)
            .map(Arc::clone)
            .ok_or_else(|| Error::NotFound(format!("plugin {plugin_id}")))
    }

    async fn worker(&self, entry: &PluginEntry) -> Arc<PluginWorker> {
        let mut workers = self.inner.workers.lock().await;
        Arc::clone(workers.entry(entry.manifest.id.clone()).or_insert_with(|| {
            Arc::new(PluginWorker::new(
                entry.manifest.id.clone(),
                entry.manifest.resource_origin_policy(),
                entry.manifest.network_hosts(),
                entry.entry.clone(),
                self.inner.catalog.loader(),
            ))
        }))
    }

    #[cfg(test)]
    async fn last_invoke_frame(&self, plugin_id: &str) -> Vec<u8> {
        self.inner
            .workers
            .lock()
            .await
            .get(plugin_id)
            .map_or_else(Vec::new, |worker| worker.last_invoke_frame())
    }

    #[cfg(test)]
    async fn sdk_projection_count(&self, plugin_id: &str) -> usize {
        self.inner
            .workers
            .lock()
            .await
            .get(plugin_id)
            .map_or(0, |worker| worker.sdk_projection_count())
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

fn find_provider<'a>(entry: &'a PluginEntry, provider_id: &str) -> Result<&'a ValidatedProvider> {
    entry
        .providers
        .iter()
        .find(|provider| provider.id() == provider_id)
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

fn live_resource_id(stored_id: &str) -> Option<&str> {
    stored_id
        .split_once('/')
        .map(|(resource_id, _)| resource_id)
}

fn live_upstream_id(stored_id: &str) -> Option<&str> {
    stored_id.split_once('/').map(|(_, upstream)| upstream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelRequest, ModelSpec, PromptSpec};
    use futures_util::StreamExt;
    use std::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        sync::Notify,
    };

    const MAGPIE: &str = super::super::builtin::MAGPIE_PLUGIN_ID;
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
            let offline = Arc::clone(&offline);
            let calls = Arc::clone(&calls);
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
    async fn magpie_catalog_through_quickjs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let host = PluginHost::new(
            PluginCatalog::for_test(temp.path()),
            PluginDataStore::at(root.clone()).unwrap(),
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
                .catalogs
                .all()
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
        invocation_gateway_with_status(endpoint, events, 200).await
    }

    async fn invocation_gateway_with_status(
        endpoint: &'static str,
        events: &'static str,
        status: u16,
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
            let requests = Arc::clone(&requests);
            let received = Arc::clone(&received);
            let disconnected = Arc::clone(&disconnected);
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
                    let (status_line, extra_headers) = if status == 200 {
                        ("HTTP/1.1 200 OK", "Content-Type: text/event-stream\r\n")
                    } else {
                        (
                            "HTTP/1.1 401 Unauthorized",
                            "Content-Type: text/plain\r\nContent-Length: 12\r\n",
                        )
                    };
                    socket
                        .write_all(
                            format!("{status_line}\r\n{extra_headers}Connection: close\r\n\r\n")
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                    if socket.write_all(events.as_bytes()).await.is_err() {
                        disconnected.notify_waiters();
                        continue;
                    }
                    if status != 200 {
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

    async fn configured_invocation_host(
        root: &std::path::Path,
        origin: String,
    ) -> (PluginHost, String) {
        std::fs::create_dir_all(root).unwrap();
        let host = PluginHost::new(
            PluginCatalog::for_test(root),
            PluginDataStore::at(root.join("data")).unwrap(),
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

    fn invocation(model_id: &str, parts: Vec<crate::model::ContentPart>) -> ModelInvocation {
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

    fn pdf_part() -> crate::model::ContentPart {
        crate::model::ContentPart::File {
            mime_type: "application/pdf".into(),
            filename: Some("report.pdf".into()),
            data: b"%PDF-1".to_vec(),
        }
    }

    /// Drains a provider stream so every event and the terminal result is produced.
    async fn drain(mut stream: crate::provider::ProviderStream) {
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    }

    #[tokio::test]
    async fn magpie_invocation_serializes_files_per_protocol() {
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
        let mut file_invocation = invocation(&responses_host.1, vec![pdf_part()]);
        file_invocation.request.model.max_output_tokens = Some(8192);
        drain(
            responses_host
                .0
                .stream_model(file_invocation, CancellationToken::new(), None),
        )
        .await;
        let requests = responses.1.lock().await;
        assert_eq!(requests[0]["max_output_tokens"], 8192);
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
        drain(chat_host.0.stream_model(
            invocation(&chat_host.1, vec![pdf_part()]),
            CancellationToken::new(),
            None,
        ))
        .await;
        let requests = chat.1.lock().await;
        assert_eq!(requests[0]["messages"][0]["content"][0]["type"], "file");
        assert_eq!(
            requests[0]["messages"][0]["content"][0]["file"]["filename"],
            "report.pdf"
        );
        drop(requests);
        chat.4.abort();
    }

    #[tokio::test]
    async fn magpie_invocation_propagates_cancellation() {
        let temp = tempfile::tempdir().unwrap();
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
            invocation(&messages_host.1, vec![file]),
            cancellation.clone(),
            None,
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

    #[tokio::test]
    async fn magpie_invocation_records_the_completed_call() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::calls::CallStore::at(temp.path().join("calls.db"));
        store
            .set_detailed_logging(crate::calls::PayloadStorage::Detailed)
            .await
            .unwrap();
        let responses = invocation_gateway(
            "/v1/responses",
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ),
        )
        .await;
        let host = configured_invocation_host(&temp.path().join("responses"), responses.0).await;
        let recorder = crate::calls::CallRecorder::start(
            store.clone(),
            crate::calls::NewLlmCall {
                call_id: "call".into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: host.1.clone(),
                provider_type: "plugin".into(),
                provider_url: "plugin://dev.cursorbyoknext.plugins.magpie/magpie".into(),
                request_type: "plugin".into(),
                request_url: "plugin://dev.cursorbyoknext.plugins.magpie/magpie".into(),
                model_id: "model".into(),
                display_name: "Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: true,
            },
        )
        .await;
        let mut stream = host.0.stream_model(
            invocation(&host.1, Vec::new()),
            CancellationToken::new(),
            Some(recorder.clone()),
        );
        while let Some(event) = stream.next().await {
            recorder.event(&event.unwrap()).await;
        }
        if !recorder.is_finished() {
            recorder
                .completed(crate::provider::FinishReason::Stop)
                .await;
        }
        let call = store.llm_call("call").await.unwrap().unwrap();
        assert_eq!(call.http_status, Some(200));
        assert!(call.response_bytes > 0);
        let request = store.llm_call_request("call").await.unwrap().unwrap();
        let mut transcript = request.headers.to_string();
        transcript.push_str(&request.body.to_string());
        assert!(!transcript.contains(FAKE_KEY));
        assert!(!transcript.contains("Bearer"));
        responses.4.abort();
    }

    #[tokio::test]
    async fn magpie_invocation_records_cancellation() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::calls::CallStore::at(temp.path().join("calls.db"));
        let messages = invocation_gateway("/v1/messages", "").await;
        let host = configured_invocation_host(&temp.path().join("messages"), messages.0).await;
        let recorder = crate::calls::CallRecorder::start(
            store.clone(),
            crate::calls::NewLlmCall {
                call_id: "cancelled-call".into(),
                run_id: "run".into(),
                conversation_id: "conversation".into(),
                provider_call_index: 0,
                model_hash: host.1.clone(),
                provider_type: "plugin".into(),
                provider_url: "plugin://dev.cursorbyoknext.plugins.magpie/magpie".into(),
                request_type: "plugin".into(),
                request_url: "plugin://dev.cursorbyoknext.plugins.magpie/magpie".into(),
                model_id: "model".into(),
                display_name: "Model".into(),
                reasoning_effort: None,
                fast: false,
                message_count: 1,
                tool_count: 0,
                detailed: false,
            },
        )
        .await;
        let cancellation = CancellationToken::new();
        let mut stream = host.0.stream_model(
            invocation(&host.1, Vec::new()),
            cancellation.clone(),
            Some(recorder.clone()),
        );
        assert!(matches!(
            stream.next().await,
            Some(Ok(ModelEvent::Start { .. }))
        ));
        tokio::time::timeout(Duration::from_secs(3), messages.2.notified())
            .await
            .expect("the invocation must reach the test gateway");
        cancellation.cancel();
        while stream.next().await.is_some() {}
        recorder.cancelled().await;
        let call = store.llm_call("cancelled-call").await.unwrap().unwrap();
        assert_eq!(call.status, crate::calls::CallStatus::Cancelled);
        messages.4.abort();
    }

    #[tokio::test]
    async fn magpie_invocation_marks_resource_invalid_on_http_401() {
        let temp = tempfile::tempdir().unwrap();
        let gateway =
            invocation_gateway_with_status("/v1/chat/completions", "unauthorized\n", 401).await;
        let host = configured_invocation_host(temp.path(), gateway.0).await;
        let mut stream = host.0.stream_model(
            invocation(&host.1, Vec::new()),
            CancellationToken::new(),
            None,
        );
        let mut error = None;
        while let Some(event) = stream.next().await {
            if let Err(failed) = event {
                error = Some(failed.to_string());
            }
        }
        assert!(
            error
                .as_deref()
                .is_some_and(|message| message.contains("HTTP 401")),
            "{error:?}"
        );
        let snapshot = host.0.snapshot().await;
        match &snapshot.plugins[0].resources[0].resources[0].state {
            crate::plugins::ResourceState::Invalid { message } => {
                assert_eq!(
                    message.as_deref(),
                    Some("Magpie authorization failed; check the API key")
                );
            }
            other => panic!("expected invalid resource, got {other:?}"),
        }
        gateway.4.abort();
    }

    fn invoke_frame_params(frame: &[u8]) -> serde_json::Value {
        let value: serde_json::Value = serde_json::from_slice(frame.trim_ascii()).unwrap();
        assert_eq!(value["method"], "provider.invoke");
        value["params"].clone()
    }

    fn assert_invoke_frame_has_no_body(params: &serde_json::Value) {
        assert!(params.get("request").is_none());
        assert!(params.get("messages").is_none());
        assert!(params.get("tools").is_none());
        assert!(params.get("instructions").is_none());
        let encoded = serde_json::to_string(params).unwrap();
        assert!(!encoded.contains("dataBase64"));
        assert!(!encoded.contains("attachment"));
        assert!(!encoded.contains("hello"));
    }

    #[tokio::test]
    async fn magpie_invoke_frame_does_not_include_or_grow_with_the_model_body() {
        let temp = tempfile::tempdir().unwrap();
        let gateway = invocation_gateway(
            "/v1/responses",
            concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            ),
        )
        .await;
        let host = configured_invocation_host(&temp.path().join("frames"), gateway.0).await;
        let mut short = host.0.stream_model(
            invocation(
                &host.1,
                vec![crate::model::ContentPart::Text {
                    text: "hello".into(),
                }],
            ),
            CancellationToken::new(),
            None,
        );
        assert!(matches!(
            short.next().await,
            Some(Ok(ModelEvent::Start { .. }))
        ));
        let short_frame = host.0.last_invoke_frame(MAGPIE).await;
        let short_params = invoke_frame_params(&short_frame);
        assert_invoke_frame_has_no_body(&short_params);
        drop(short);

        let mut long_history = Vec::new();
        for index in 0..40 {
            long_history.push(crate::model::ProjectedMessage {
                message_id: format!("m{index:02}"),
                role: crate::model::Role::User,
                content: crate::model::ProjectedContent::Parts(vec![
                    crate::model::ContentPart::Text {
                        text: format!("turn-{index}-{}", "x".repeat(256)),
                    },
                ]),
            });
        }
        long_history.push(crate::model::ProjectedMessage {
            message_id: "file".into(),
            role: crate::model::Role::User,
            content: crate::model::ProjectedContent::Parts(vec![crate::model::ContentPart::File {
                mime_type: "application/pdf".into(),
                filename: Some("blob.pdf".into()),
                data: vec![0_u8; 1024 * 1024],
            }]),
        });
        let mut long_invocation = invocation(&host.1, Vec::new());
        long_invocation.request.history = long_history;
        let mut long = host
            .0
            .stream_model(long_invocation, CancellationToken::new(), None);
        assert!(matches!(
            long.next().await,
            Some(Ok(ModelEvent::Start { .. }))
        ));
        let long_frame = host.0.last_invoke_frame(MAGPIE).await;
        let long_params = invoke_frame_params(&long_frame);
        assert_invoke_frame_has_no_body(&long_params);
        assert_eq!(short_frame.len(), long_frame.len());
        assert_eq!(short_params, long_params);
        assert_eq!(host.0.sdk_projection_count(MAGPIE).await, 0);
        drop(long);
        gateway.4.abort();
    }

    #[tokio::test]
    async fn binds_live_models_to_resource_ids() {
        // The binding happens inside the cache, so its effect is observed through
        // the same read the picker makes.
        let cache = super::super::catalog::cache::CatalogCache::default();
        let key = super::super::catalog::cache::LiveCatalogKey {
            plugin: "dev.cursorbyoknext.plugins.magpie".into(),
            provider: "magpie".into(),
            resource: "resource-1".into(),
        };
        let entry = cache
            .store(
                key,
                Ok(vec![StoredModel::for_test("gpt-5", "GPT")]),
                "resource-1",
            )
            .await;
        assert_eq!(entry.models[0].id(), "resource-1/gpt-5");
        assert_eq!(
            crate::plugins::parse_model_id(&format!(
                "plugin:dev.cursorbyoknext.plugins.magpie/magpie/{}",
                entry.models[0].id()
            )),
            Some(crate::plugins::ModelId {
                plugin: "dev.cursorbyoknext.plugins.magpie",
                provider: "magpie",
                model: "resource-1/gpt-5",
            })
        );
    }
    #[tokio::test]
    async fn collector_failures_are_visible_as_safe_vm_failed_snapshots() {
        for source in [
            "throw new Error('secret private-source-path')",
            "await Promise.reject(new Error('secret private-source-path'))",
            "await import('./missing.js')",
        ] {
            let modules = super::super::vm::loader::Modules(Box::leak(
                vec![("sdk/collect.js", source)].into_boxed_slice(),
            ));
            let root = tempfile::tempdir().unwrap();
            let host = PluginHost::new(
                PluginCatalog::with_modules(modules),
                PluginDataStore::at(root.path().join("data")).unwrap(),
            );
            let snapshot = host.snapshot().await;
            assert!(!snapshot.runtime_ready);
            assert_eq!(
                snapshot.runtime_error,
                Some(super::super::text("plugin.error.vm_failed"))
            );
            assert_eq!(snapshot.plugins.len(), 1);
            assert!(!snapshot.plugins[0].runtime_ready);
            assert!(
                !serde_json::to_string(&snapshot)
                    .unwrap()
                    .contains("private-source-path")
            );
        }
    }
}
