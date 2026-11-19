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
    Error, Result,
    catalog::{PluginCatalog, PluginEntry},
    data::PluginDataStore,
    descriptor::{
        FORM_ADD_METHOD, FORM_FIELD_URL, LIVE_CATALOG, PluginDescriptor, PluginModelDescriptor,
        PluginProviderDescriptor, PluginResourceDescriptor, PluginResourceView, ProviderDefinition,
        ResourceDefinition, ResourcePresentation,
    },
    origin,
    state::{PluginStateStore, ResourceDraft, ResourceRecord, StoredModel},
    worker::PluginWorker,
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
                runtime_error: Some("bun is not on PATH".into()),
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
                runtime_error: Some("无法加载内置 Magpie 插件，请检查 Bun 是否可用后重试。".into()),
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
            .map_err(|_| Error::Storage("无法保存网关，请检查插件数据目录。".into()))?;
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
                _ => Error::Storage("无法移除网关，请检查插件数据目录。".into()),
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
            .ok_or_else(|| Error::Runtime("bun is not on PATH".into()))
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
    "无法读取已保存的网关，请检查插件数据文件后重试。".into()
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
