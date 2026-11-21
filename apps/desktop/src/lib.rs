//! The app's Rust code: what `App.tsx` imports from `cursor-byok-next/native`. Their
//! TypeScript declarations are generated from these types into
//! `.nsh/types/cursor-byok-next/native.d.ts`.
//!
//! `harness/` is the port of cursor-byok's `local_app`: it is what makes Cursor
//! willing to talk to this app (proxy, CA, local account). The screens that
//! drive it come next; this file is the typed boundary they will call.

pub mod backend;
pub mod error;
pub mod harness;
pub mod loop_service;
pub mod model;
pub mod plugins;
pub mod protocol;
pub mod provider;

pub use error::{Error, Result};

use std::sync::{Arc, Mutex, OnceLock};

use nsh_base::theme;
#[allow(unused_imports)]
use nsh_core::{
    JsonValue,
    element::{JsHost, JsIn, JsOwned, JsValue, PropError, TsTypes},
    i18n::Translations,
    prelude::*,
};
use tokio::sync::watch;

use self::backend::Backend;
use self::harness::{
    CaState as HarnessCa, CursorHarness, Error as HarnessError, HarnessPaths, HarnessStatus,
    IntegrationState as HarnessIntegration,
};
use self::plugins::{
    FormSubmitResponse as HostFormSubmit, PluginHost, PluginSnapshot as HostSnapshot,
    PluginDescriptor as HostPlugin, PluginModelDescriptor as HostModel,
    PluginProviderDescriptor as HostProvider, PluginResourceDescriptor as HostResource,
    PluginResourceView as HostResourceView, ResourceState as HostResourceState,
};

/// The proxy port Cursor is pointed at when this app has never bound one.
/// `0` lets the OS pick, and the chosen port is remembered from then on.
const DEFAULT_PROXY_PORT: u16 = 0;

/// The crate's UI set (`nsh.json` `"rust"`).
#[native_module("cursor-byok-next/native")]
fn native(registry: &mut Registry) {
    registry
        .register::<Badge>()
        .module::<Format>()
        .class::<Counter>()
        .class::<Harness>()
        .class::<Plugins>();
}

/// Whether the system trusts the local CA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum CaState {
    /// No CA yet.
    Missing,
    /// Generated, but not installed into the system's trust store.
    Untrusted,
    /// Trusted: takeover may start.
    Ready,
    /// The pair is incomplete, mismatched or expired.
    Invalid,
}

impl From<HarnessCa> for CaState {
    fn from(value: HarnessCa) -> Self {
        match value {
            HarnessCa::Missing => Self::Missing,
            HarnessCa::Untrusted => Self::Untrusted,
            HarnessCa::Ready => Self::Ready,
            HarnessCa::Invalid => Self::Invalid,
        }
    }
}

/// How much of takeover is working right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum IntegrationState {
    Disabled,
    Enabled,
    /// The proxy and Cursor's settings disagree: one of them is stale.
    Degraded,
}

impl From<HarnessIntegration> for IntegrationState {
    fn from(value: HarnessIntegration) -> Self {
        match value {
            HarnessIntegration::Disabled => Self::Disabled,
            HarnessIntegration::Enabled => Self::Enabled,
            HarnessIntegration::Degraded => Self::Degraded,
        }
    }
}

impl From<HarnessStatus> for HarnessState {
    fn from(value: HarnessStatus) -> Self {
        Self {
            platform: value.platform,
            ca: value.ca.into(),
            integration: value.integration.into(),
            settings_applied: value.settings_applied,
            proxy_url: value.proxy_url,
            proxy_port: value.proxy_port,
            ca_install_command: value.ca_install_command,
        }
    }
}

/// The takeover state a screen shows.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct HarnessState {
    /// The OS, as Rust names it (`"macos"`, `"windows"`, `"linux"`).
    pub platform: String,
    /// Whether the system trusts the local CA.
    pub ca: CaState,
    /// Whether Cursor is actually routed through this app.
    pub integration: IntegrationState,
    /// Whether Cursor's settings hold exactly this app's proxy.
    pub settings_applied: bool,
    /// The proxy Cursor should use, present only while it runs.
    pub proxy_url: Option<String>,
    /// The port the proxy bound.
    pub proxy_port: Option<u16>,
    /// The command that trusts the CA; the app cannot run it (it needs sudo).
    pub ca_install_command: Option<String>,
}

/// Cursor takeover: points Cursor at the local proxy, signs the certificates it
/// accepts, and signs it in to a local account.
///
/// Create one and keep it for the app's lifetime: it owns the proxy task.
#[class]
pub struct Harness {
    inner: Arc<CursorHarness>,
    backend: watch::Receiver<Option<String>>,
}

// A class: `new Harness()`, `await harness.status()`, `await harness.setEnabled(true)`.
#[class]
impl Harness {
    /// Restores the proxy port and the on/off intent of the previous run.
    pub fn new() -> Result<Self, HarnessError> {
        let paths = HarnessPaths::managed(DEFAULT_PROXY_PORT)?;
        if let Ok(host) = shared_plugin_host() {
            start_backend(host);
        }
        Ok(Self {
            inner: Arc::new(CursorHarness::new(paths)),
            backend: backend_addr(),
        })
    }

    /// The current state. Also heals a takeover whose proxy went away.
    pub async fn status(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        let (harness, mut backend) = this.with(|harness| (harness.inner.clone(), harness.backend.clone()));
        attach_backend(&harness, &mut backend).await?;
        Ok(harness.status().await.into())
    }

    /// Turns takeover on or off. Enabling fails, without touching Cursor, when
    /// the CA is not trusted yet; check `ca` in the status first.
    pub async fn set_enabled(
        this: Instance<Self>,
        enabled: bool,
    ) -> Result<HarnessState, HarnessError> {
        let (harness, mut backend) = this.with(|harness| (harness.inner.clone(), harness.backend.clone()));
        attach_backend(&harness, &mut backend).await?;
        harness.set_enabled(enabled).await.map(Into::into)
    }

    /// Connects the local service after it starts listening. Existing takeover
    /// starts routing locally and receives its local account at the same time.
    pub async fn set_backend_addr(this: Instance<Self>, addr: String) -> Result<(), HarnessError> {
        let addr = addr.parse().map_err(|error| {
            HarnessError::Config(format!("invalid local backend address: {error}"))
        })?;
        this.with(|harness| harness.inner.clone())
            .set_backend_addr(addr)
            .await
    }

    /// Generates the CA when it is missing. Trusting it is the user's step:
    /// copy `caInstallCommand` into a terminal.
    pub async fn initialize_ca(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        let harness = this.with(|harness| harness.inner.clone());
        let worker = harness.clone();
        blocking(move || worker.initialize_ca())
            .await
            .map_err(|error| {
                HarnessError::Process(format!("CA initialization task failed: {error}"))
            })??;
        Ok(harness.status().await.into())
    }

    /// Drops the proxy a crashed run left in Cursor's settings. `true` when it
    /// removed something.
    pub async fn cleanup_stale_settings(this: Instance<Self>) -> Result<bool, HarnessError> {
        this.with(|harness| harness.inner.clone())
            .cleanup_stale_settings()
            .await
    }

    /// Quits Cursor so a freshly written proxy setting applies at once. `false`
    /// when Cursor was not running.
    pub async fn quit_cursor() -> Result<bool, HarnessError> {
        harness::quit_cursor().await
    }
}

/// Text a plugin localizes: one string for every language, or the string per
/// language tag (`{ "en-US": "Gateways", "zh-CN": "网关" }`). The screen picks
/// the tag the app shows; Rust leaves the map as the plugin wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalizedText(pub JsonValue);

impl From<JsonValue> for LocalizedText {
    fn from(value: JsonValue) -> Self {
        Self(value)
    }
}

impl JsValue for LocalizedText {
    fn ts_type(types: &mut TsTypes) -> String {
        types.declare("LocalizedText", |_| {
            "/** A string, or that string per language tag (`{ \"en-US\": \"Gateways\", \"zh-CN\": \"网关\" }`). */\nexport type LocalizedText = string | { [locale: string]: string };".to_owned()
        })
    }

    fn from_js(value: JsIn<'_>) -> Result<Self, PropError> {
        JsonValue::from_js(value).map(Self)
    }

    fn to_js<'a>(&self, host: &'a dyn JsHost) -> Result<JsOwned<'a>, PropError> {
        self.0.to_js(host)
    }
}

fn resource_status(state: &HostResourceState) -> ResourceStatus {
    match state {
        HostResourceState::Ready => ResourceStatus::Ready,
        HostResourceState::Cooling { .. } => ResourceStatus::Cooling,
        HostResourceState::Invalid { .. } => ResourceStatus::Invalid,
    }
}

fn resource_message(state: &HostResourceState) -> Option<String> {
    match state {
        HostResourceState::Ready => None,
        HostResourceState::Cooling { message, .. } | HostResourceState::Invalid { message } => {
            message.clone()
        }
    }
}

impl From<HostSnapshot> for PluginSnapshot {
    fn from(value: HostSnapshot) -> Self {
        Self {
            runtime_ready: value.runtime_ready,
            runtime_error: value.runtime_error,
            plugins: value.plugins.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<HostPlugin> for PluginInfo {
    fn from(value: HostPlugin) -> Self {
        Self {
            id: value.id,
            name: value.name,
            version: value.version,
            author: value.author,
            icon: value.icon,
            runtime_ready: value.runtime_ready,
            runtime_error: value.runtime_error,
            providers: value.providers.into_iter().map(Into::into).collect(),
            resources: value.resources.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<HostProvider> for PluginProvider {
    fn from(value: HostProvider) -> Self {
        Self {
            id: value.id,
            plugin_id: value.plugin_id,
            display_name: LocalizedText(value.display_name),
            description: LocalizedText(value.description),
            provider_type: value.provider_type,
            resource_type: value.resource_type,
            catalog: value.catalog,
            configured: value.configured,
            catalog_error: value.catalog_error,
            models: value.models.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<HostModel> for PluginModel {
    fn from(value: HostModel) -> Self {
        Self {
            id: value.id,
            plugin_id: value.plugin_id,
            plugin_name: value.plugin_name,
            provider_id: value.provider_id,
            model_id: value.model_id,
            display_name: value.display_name,
            description: value.description,
            max_output_tokens: value.max_output_tokens,
        }
    }
}

impl From<HostResource> for PluginResource {
    fn from(value: HostResource) -> Self {
        Self {
            resource_type: value.resource_type,
            display_name: LocalizedText(value.display_name),
            storage_error: value.storage_error,
            add: value.add.into_iter().map(Into::into).collect(),
            resources: value.resources.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<plugins::AddMethodDefinition> for PluginAddMethod {
    fn from(value: plugins::AddMethodDefinition) -> Self {
        Self {
            id: value.id,
            display_name: LocalizedText(value.display_name),
            fields: value.fields.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<plugins::FormFieldDefinition> for PluginFormField {
    fn from(value: plugins::FormFieldDefinition) -> Self {
        Self {
            id: value.id,
            label: LocalizedText(value.label),
            field_type: value.field_type,
            required: value.required,
            placeholder: LocalizedText(value.placeholder),
        }
    }
}

impl From<HostResourceView> for PluginResourceView {
    fn from(value: HostResourceView) -> Self {
        Self {
            id: value.id,
            status: resource_status(&value.state),
            message: resource_message(&value.state),
            display_name: value.display_name,
            description: LocalizedText(value.description),
        }
    }
}

impl From<HostFormSubmit> for PluginFormResult {
    fn from(value: HostFormSubmit) -> Self {
        Self {
            added: value.added as u32,
            updated: value.updated as u32,
            model_sync_error: value.model_sync_error,
        }
    }
}

/// A plugin runtime snapshot the settings page shows.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginSnapshot {
    /// Whether a Bun executable was found; a plugin may still fail to load.
    pub runtime_ready: bool,
    /// Why the runtime is unavailable, when it is.
    pub runtime_error: Option<String>,
    /// Installed plugins, including Magpie.
    pub plugins: Vec<PluginInfo>,
}

/// One plugin as the settings page shows it.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginInfo {
    /// Stable plugin id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Plugin version.
    pub version: String,
    /// Optional author.
    pub author: Option<String>,
    /// Data URL of the plugin icon.
    pub icon: String,
    /// Whether this plugin's worker can run.
    pub runtime_ready: bool,
    /// Why this plugin cannot run, when it cannot.
    pub runtime_error: Option<String>,
    /// Providers the plugin exposes.
    pub providers: Vec<PluginProvider>,
    /// Resource types the plugin exposes.
    pub resources: Vec<PluginResource>,
}

/// A provider that can list models.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginProvider {
    /// Provider id inside the plugin.
    pub id: String,
    /// Owning plugin id.
    pub plugin_id: String,
    /// Localized display name.
    pub display_name: LocalizedText,
    /// Localized description.
    pub description: LocalizedText,
    /// Product identity, such as `"magpie"`.
    pub provider_type: String,
    /// Resource type this provider consumes, when any.
    pub resource_type: Option<String>,
    /// `"live"` or `"snapshot"`.
    pub catalog: String,
    /// Whether a resource is present so models can be listed.
    pub configured: bool,
    /// Last catalog refresh error, if any.
    pub catalog_error: Option<String>,
    /// Models currently known for this provider.
    pub models: Vec<PluginModel>,
}

/// One model from a plugin catalog.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginModel {
    /// Stable plugin model id.
    pub id: String,
    /// Owning plugin id.
    pub plugin_id: String,
    /// Plugin display name.
    pub plugin_name: String,
    /// Provider id.
    pub provider_id: String,
    /// Upstream or live-bound model id.
    pub model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Optional max output tokens.
    pub max_output_tokens: Option<u64>,
}

/// A resource type a plugin can persist.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginResource {
    /// Resource type, such as `"gateway"`.
    pub resource_type: String,
    /// Localized type name.
    pub display_name: LocalizedText,
    /// Form methods that add a resource.
    pub add: Vec<PluginAddMethod>,
    /// Why saved resources could not be read, when storage is degraded.
    pub storage_error: Option<String>,
    /// Saved resources of this type.
    pub resources: Vec<PluginResourceView>,
}

/// A form that creates a plugin resource.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginAddMethod {
    /// Method id.
    pub id: String,
    /// Localized method name.
    pub display_name: LocalizedText,
    /// Fields the host draws.
    pub fields: Vec<PluginFormField>,
}

/// One field of a plugin form.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginFormField {
    /// Field id submitted to the plugin.
    pub id: String,
    /// Localized label.
    pub label: LocalizedText,
    /// `"url"`, `"secret"` or `"text"`.
    pub field_type: String,
    /// Whether the field is required.
    pub required: bool,
    /// Localized placeholder.
    pub placeholder: LocalizedText,
}

/// Whether a saved resource is usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum ResourceStatus {
    Ready,
    Cooling,
    Invalid,
}

/// A saved resource as the UI may show it. Credentials never appear here.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginResourceView {
    /// Host-assigned resource id.
    pub id: String,
    /// Current usability.
    pub status: ResourceStatus,
    /// Optional status message.
    pub message: Option<String>,
    /// User-facing name, such as a gateway origin.
    pub display_name: String,
    /// Optional description.
    pub description: LocalizedText,
}

/// Result of submitting a plugin form.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginFormResult {
    /// Newly created resources.
    pub added: u32,
    /// Updated existing resources.
    pub updated: u32,
    /// Catalog refresh error after the resource was saved, if any.
    pub model_sync_error: Option<String>,
}

/// One form field value.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginFormValue {
    /// Field id.
    pub id: String,
    /// Submitted text.
    pub value: String,
}

/// Plugin catalog and Magpie gateway configuration.
///
/// Create one and keep it for the app's lifetime: it owns plugin workers.
#[class]
pub struct Plugins {
    inner: Arc<PluginHost>,
}

#[class]
impl Plugins {
    /// Restores installed plugins and looks for Bun on PATH.
    pub fn new() -> Result<Self, plugins::Error> {
        let inner = shared_plugin_host()?;
        start_backend(inner.clone());
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The current plugin snapshot, including Magpie models when a gateway exists.
    pub async fn snapshot(this: Instance<Self>) -> Result<PluginSnapshot, plugins::Error> {
        Ok(this
            .with(|plugins| plugins.inner.clone())
            .snapshot()
            .await
            .into())
    }

    /// Submits a plugin form. Magpie uses this to save a gateway.
    pub async fn submit_form(
        this: Instance<Self>,
        plugin_id: String,
        resource_type: String,
        method_id: String,
        values: Vec<PluginFormValue>,
    ) -> Result<PluginFormResult, plugins::Error> {
        let mut object = serde_json::Map::new();
        for value in values {
            object.insert(value.id, serde_json::Value::String(value.value));
        }
        this.with(|plugins| plugins.inner.clone())
            .form_submit(
                &plugin_id,
                &resource_type,
                &method_id,
                serde_json::Value::Object(object),
            )
            .await
            .map(Into::into)
    }

    /// Removes a saved plugin resource.
    pub async fn delete_resource(
        this: Instance<Self>,
        plugin_id: String,
        resource_type: String,
        resource_id: String,
    ) -> Result<PluginSnapshot, plugins::Error> {
        let host = this.with(|plugins| plugins.inner.clone());
        host.delete_resource(&plugin_id, &resource_type, &resource_id)
            .await?;
        Ok(host.snapshot().await.into())
    }

    /// Refreshes a live catalog without changing resources.
    pub async fn refresh_catalog(
        this: Instance<Self>,
        plugin_id: String,
        provider_id: String,
    ) -> Result<PluginSnapshot, plugins::Error> {
        let host = this.with(|plugins| plugins.inner.clone());
        host.refresh_catalog(&plugin_id, &provider_id).await?;
        Ok(host.snapshot().await.into())
    }
}

// A component: `<Badge label="…" onPress={…} />`. The function's doc comment
// is the component's in TS, the fields' its props'.
/// The props of [`Badge`].
#[derive(Debug, Clone, PartialEq, Props)]
pub struct BadgeProps {
    /// The text.
    pub label: Prop<SharedString>,
}

/// The events of a [`Badge`].
#[derive(Debug, Clone, PartialEq, Event)]
pub enum BadgeEvent {
    /// The badge was pressed.
    Press,
}

/// A pill with a label; pressing it reports `onPress`.
#[component]
pub fn Badge(props: BadgeProps, cx: Scope<BadgeEvent>) -> impl IntoElement {
    view()
        .px_4()
        .py_2()
        .rounded_full()
        .bg(move || theme().primary)
        .active(|s| s.bg(move || theme().primary.opacity(0.9)))
        .on_press(move || cx.emit(BadgeEvent::Press))
        .child(text(props.label).text_color(move || theme().primary_foreground))
}

fn shared_plugin_host() -> std::result::Result<PluginHost, plugins::Error> {
    if let Some(host) = PLUGIN_HOST.get() {
        return Ok(host.clone());
    }
    let host = PluginHost::managed()?;
    Ok(PLUGIN_HOST.get_or_init(|| host.clone()).clone())
}

struct BackendWatch {
    sender: watch::Sender<Option<String>>,
    started: Mutex<bool>,
}

fn backend_watch() -> &'static BackendWatch {
    BACKEND.get_or_init(|| {
        let (sender, _) = watch::channel(None);
        BackendWatch {
            sender,
            started: Mutex::new(false),
        }
    })
}

fn backend_addr() -> watch::Receiver<Option<String>> {
    backend_watch().sender.subscribe()
}

fn start_backend(plugins: PluginHost) {
    let watch = backend_watch();
    let mut started = watch.started.lock().expect("backend start lock");
    if *started {
        return;
    }
    *started = true;
    let sender = watch.sender.clone();
    tokio::spawn(async move {
        match Backend::start(plugins).await {
            Ok(backend) => {
                let addr = backend.addr().to_string();
                let _ = sender.send(Some(addr));
                std::mem::forget(backend);
            }
            Err(error) => {
                tracing::error!(%error, "cannot start the local loop backend");
            }
        }
    });
}

async fn attach_backend(
    harness: &CursorHarness,
    backend: &mut watch::Receiver<Option<String>>,
) -> std::result::Result<(), HarnessError> {
    if backend.borrow().is_none() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), backend.changed()).await;
    }
    if let Some(addr) = backend.borrow().clone() {
        harness
            .set_backend_addr(addr.parse().map_err(|error| {
                HarnessError::Config(format!("invalid local backend address: {error}"))
            })?)
            .await?;
    }
    Ok(())
}

static PLUGIN_HOST: OnceLock<PluginHost> = OnceLock::new();
static BACKEND: OnceLock<BackendWatch> = OnceLock::new();

/// Text helpers.
#[module]
pub struct Format;

// A module: `Format.times(3)`. The struct's doc comment is the module's in
// TS (`#[module]` on it and on this block).
#[module]
impl Format {
    /// "once", "twice", "3 times".
    pub fn times(count: u32) -> String {
        match count {
            1 => "once".to_owned(),
            2 => "twice".to_owned(),
            n => format!("{n} times"),
        }
    }
}

/// A count kept in Rust.
#[derive(Default)]
#[class]
pub struct Counter {
    count: u32,
}

// A class: `new Counter()`, `counter.increment()`, `counter.count`.
#[class]
impl Counter {
    /// A counter at 0.
    pub fn new() -> Self {
        Counter { count: 0 }
    }

    /// Adds one; the new count.
    pub fn increment(&mut self) -> u32 {
        self.count += 1;
        self.count
    }

    /// The count.
    #[getter]
    pub fn count(&self) -> u32 {
        self.count
    }
}
