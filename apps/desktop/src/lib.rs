//! The app's Rust code: what `App.tsx` imports from `cursor-byok-next/native`. Their
//! TypeScript declarations are generated from these types into
//! `.nsh/types/cursor-byok-next/native.d.ts`.
//!
//! `harness/` is the port of cursor-byok's `local_app`: it is what makes Cursor
//! willing to talk to this app (proxy, CA, local account). The screens that
//! drive it come next; this file is the typed boundary they will call.

mod atomic;

pub mod backend;
pub mod calls;
pub mod desktop;
pub mod error;
pub mod harness;
pub mod loop_service;
pub mod model;
pub mod plugins;
pub mod protocol;
pub mod provider;
mod saturating;
mod storage;
mod time;

pub use error::{Error, Result};

use std::sync::{Arc, OnceLock};

#[expect(
    unused_imports,
    reason = "re-exported to the TSX boundary and macro output"
)]
use nsh_core::{
    JsonValue,
    element::{JsHost, JsIn, JsOwned, JsValue, PropError, TsTypes},
    i18n::Translations,
    prelude::*,
};
use parking_lot::Mutex;
use tokio::sync::watch;

use self::backend::Backend;
use self::calls::{CallStatus, CallStore, PayloadStorage};
pub use self::desktop::AppAppearance;
use self::harness::{
    CaState as HarnessCa, CursorHarness, Error as HarnessError, HarnessPaths, HarnessStatus,
    IntegrationState as HarnessIntegration,
};
use self::plugins::{
    FormSubmitResponse as HostFormSubmit, PluginDescriptor as HostPlugin, PluginHost,
    PluginModelDescriptor as HostModel, PluginProviderDescriptor as HostProvider,
    PluginResourceDescriptor as HostResource, PluginResourceView as HostResourceView,
    PluginSnapshot as HostSnapshot, ResourceState as HostResourceState,
};

/// The crate's UI set (`nsh.json` `"rust"`).
#[native_module("cursor-byok-next/native")]
fn native(registry: &mut Registry) {
    registry
        .class::<Harness>()
        .class::<Plugins>()
        .class::<Calls>()
        .class::<Desktop>();
}

/// Whether macOS shows the app's Dock icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, JsObject)]
pub struct DesktopState {
    /// The saved choice; this setting has no effect on other platforms.
    pub show_dock_icon: bool,
}

impl From<desktop::DockIcon> for DesktopState {
    fn from(value: desktop::DockIcon) -> Self {
        Self {
            show_dock_icon: value.is_visible(),
        }
    }
}

/// Desktop shell settings. macOS restores and applies the Dock choice at startup.
#[class]
pub struct Desktop {
    settings: parking_lot::Mutex<desktop::DesktopSettings>,
    /// The appearance last handed to `AppKit`; `None` before any was. Asking
    /// for the same one again then costs no trip to the main thread.
    applied: parking_lot::Mutex<Option<AppAppearance>>,
}

impl std::fmt::Debug for Desktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Locks are deliberately not taken: a Debug call must not block on or
        // re-enter the settings the UI is using.
        f.debug_struct("Desktop").finish_non_exhaustive()
    }
}

#[class]
impl Desktop {
    /// Restores the desktop shell's saved settings.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            settings: parking_lot::Mutex::new(desktop::DesktopSettings::managed()?),
            applied: parking_lot::Mutex::new(None),
        })
    }

    /// Reads the saved shell settings without changing `AppKit`.
    ///
    /// Async because the generated TSX binding groups every member of a class
    /// the same way; there is nothing to await.
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "the #[class] binding is async-only; a sync member would change the generated TSX type"
    )]
    pub async fn snapshot(this: Instance<Self>) -> DesktopState {
        this.with(|desktop| desktop.settings.lock().dock_icon())
            .into()
    }

    /// Applies the restored Dock setting after the native window is mounted.
    pub async fn restore(this: Instance<Self>) {
        let dock_icon = this.with(|desktop| desktop.settings.lock().dock_icon());
        if dock_icon == desktop::DockIcon::Hidden
            && let Err(error) = desktop::set_dock_icon(dock_icon).await
        {
            tracing::warn!(%error, "could not restore the Dock icon setting");
        }
    }

    /// Shows or hides the Dock icon and remembers the choice across launches.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the native boundary takes the switch's boolean directly"
    )]
    pub async fn set_dock_icon_visible(
        this: Instance<Self>,
        visible: bool,
    ) -> Result<DesktopState, Error> {
        let requested = if visible {
            desktop::DockIcon::Visible
        } else {
            desktop::DockIcon::Hidden
        };
        let previous = this.with(|desktop| desktop.settings.lock().dock_icon());
        desktop::set_dock_icon(requested).await?;
        let saved = this.with_mut(|desktop| desktop.settings.lock().save(requested));
        if let Err(error) = saved {
            // `AppKit` already accepted the change; put the previous state back
            // rather than leaving the window out of sync with the file.
            if let Err(rollback) = desktop::set_dock_icon(previous).await {
                tracing::error!(%rollback, "could not roll back the Dock icon setting");
            }
            return Err(error);
        }
        Ok(requested.into())
    }

    /// Applies the app's appearance to the title bar the platform draws.
    ///
    /// The screens call `setColorScheme` themselves, which NSH applies to its
    /// views; the title bar above them is the platform's and follows this.
    /// The choice itself lives in TSX storage, so nothing is saved here; a
    /// repeat of the choice already applied changes nothing.
    pub async fn set_appearance(
        this: Instance<Self>,
        appearance: AppAppearance,
    ) -> Result<(), Error> {
        if this.with(|desktop| *desktop.applied.lock()) == Some(appearance) {
            return Ok(());
        }
        desktop::set_app_appearance(appearance).await?;
        this.with(|desktop| *desktop.applied.lock() = Some(appearance));
        Ok(())
    }
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
            preferred_proxy_port: value.preferred_proxy_port,
            data_dir: value.data_dir,
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
    /// The port the user chose in system settings. `0` asks the OS to pick;
    /// `null` means no choice was saved, so the last bound port is reused.
    pub preferred_proxy_port: Option<u16>,
    /// Where this app keeps its own files (`harness.json`, `ca/`, plugins).
    pub data_dir: String,
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

impl std::fmt::Debug for Harness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `CursorHarness` already redacts its own state; the backend address is a
        // small non-secret value and is what makes this object identifiable.
        f.debug_struct("Harness")
            .field("inner", &self.inner)
            .field("backend", &self.backend.borrow().clone())
            .finish()
    }
}

// A class: `new Harness()`, `await harness.status()`, `await harness.setEnabled(true)`.
#[class]
impl Harness {
    /// Restores the proxy port and the on/off intent of the previous run.
    pub fn new() -> Result<Self, HarnessError> {
        let paths = HarnessPaths::managed()?;
        Ok(Self {
            inner: Arc::new(CursorHarness::new(paths)),
            backend: backend_addr(),
        })
    }

    /// The current state. Also heals a takeover whose proxy went away.
    pub async fn status(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        let (harness, mut backend) =
            this.with(|harness| (Arc::clone(&harness.inner), harness.backend.clone()));
        attach_backend(&harness, &mut backend).await?;
        Ok(harness.status().await.into())
    }

    /// Turns takeover on or off. Enabling fails, without touching Cursor, when
    /// the CA is not trusted yet; check `ca` in the status first.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the switch maps 1:1 to a TSX boolean; an enum would only move the translation to the screen"
    )]
    pub async fn set_enabled(
        this: Instance<Self>,
        enabled: bool,
    ) -> Result<HarnessState, HarnessError> {
        let (harness, mut backend) =
            this.with(|harness| (Arc::clone(&harness.inner), harness.backend.clone()));
        attach_backend(&harness, &mut backend).await?;
        harness.set_takeover(enabled.into()).await.map(Into::into)
    }

    /// Connects the local service after it starts listening. Existing takeover
    /// starts routing locally and receives its local account at the same time.
    pub async fn set_backend_addr(this: Instance<Self>, addr: String) -> Result<(), HarnessError> {
        let addr = addr.parse().map_err(|error| {
            HarnessError::Config(format!("invalid local backend address: {error}"))
        })?;
        this.with(|harness| Arc::clone(&harness.inner))
            .set_backend_addr(addr)
            .await
    }

    /// Generates the CA when it is missing. Trusting it is the user's step:
    /// copy `caInstallCommand` into a terminal.
    pub async fn initialize_ca(this: Instance<Self>) -> Result<HarnessState, HarnessError> {
        let harness = this.with(|harness| Arc::clone(&harness.inner));
        let worker = Arc::clone(&harness);
        blocking(move || worker.initialize_ca())
            .await
            .map_err(|error| {
                HarnessError::Process(format!("CA initialization task failed: {error}"))
            })??;
        Ok(harness.status().await.into())
    }

    /// Remembers the port takeover should bind; `0` lets the OS pick and `null`
    /// clears the choice. Takes effect the next time takeover starts.
    pub async fn set_preferred_port(
        this: Instance<Self>,
        port: Option<u16>,
    ) -> Result<HarnessState, HarnessError> {
        this.with(|harness| Arc::clone(&harness.inner))
            .set_preferred_port(port)
            .await
            .map(Into::into)
    }

    /// Drops the proxy a crashed run left in Cursor's settings. `true` when it
    /// removed something.
    pub async fn cleanup_stale_settings(this: Instance<Self>) -> Result<bool, HarnessError> {
        this.with(|harness| Arc::clone(&harness.inner))
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
            catalog: value.catalog.to_string(),
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
            field_type: value.field_type.as_str().to_owned(),
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
            added: saturating::u32_from_usize(value.added),
            updated: saturating::u32_from_usize(value.updated),
            model_sync_error: value.model_sync_error,
        }
    }
}

/// A plugin runtime snapshot the settings page shows.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct PluginSnapshot {
    /// Whether the isolated plugin VM initialized successfully.
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

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `PluginHost` holds worker processes and their mutable state; naming the
        // object is enough to identify it in a log without walking that state.
        f.debug_struct("Plugins").finish_non_exhaustive()
    }
}

#[class]
impl Plugins {
    /// Restores embedded plugins and initializes their isolated VMs.
    pub fn new() -> Result<Self, plugins::Error> {
        let inner = shared_plugin_host()?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// The current plugin snapshot, including Magpie models when a gateway exists.
    pub async fn snapshot(this: Instance<Self>) -> Result<PluginSnapshot, plugins::Error> {
        let inner = this.with(|plugins| Arc::clone(&plugins.inner));
        start_backend((*inner).clone());
        Ok(inner.snapshot().await.into())
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
        this.with(|plugins| Arc::clone(&plugins.inner))
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
        let host = this.with(|plugins| Arc::clone(&plugins.inner));
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
        let host = this.with(|plugins| Arc::clone(&plugins.inner));
        host.refresh_catalog(&plugin_id, &provider_id).await?;
        Ok(host.snapshot().await.into())
    }
}

/// Whether a recorded call is still running or has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PropEnum)]
pub enum CallStatusView {
    /// Accepted, waiting for the provider's first byte.
    Running,
    /// The provider finished the stream normally.
    Completed,
    /// The provider or the request failed.
    Error,
    /// The client dropped the stream before it finished.
    Cancelled,
}

impl From<CallStatus> for CallStatusView {
    fn from(value: CallStatus) -> Self {
        match value {
            CallStatus::Running => Self::Running,
            CallStatus::Completed => Self::Completed,
            CallStatus::Error => Self::Error,
            CallStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// One recorded Magpie model call.
#[derive(Debug, Clone, PartialEq, JsObject)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "these are independent SQLite flags mirrored to TSX, not a mode switch"
)]
pub struct CallRecord {
    /// Invocation id from Cursor.
    pub call_id: String,
    /// Owning local run id.
    pub run_id: String,
    /// Cursor conversation id.
    pub conversation_id: String,
    /// Provider call index inside the run.
    pub provider_call_index: i64,
    /// Stable plugin model id.
    pub model_hash: Option<String>,
    /// Provider family, such as `"plugin"`.
    pub provider_type: String,
    /// Plugin URL used for attribution.
    pub provider_url: String,
    /// Request family, such as `"plugin"`.
    pub request_type: String,
    /// Plugin URL used for the request.
    pub request_url: String,
    /// Upstream model id.
    pub model_id: String,
    /// Name shown in the picker.
    pub display_name: String,
    /// Effective reasoning effort.
    pub reasoning_effort: Option<String>,
    /// Whether the call asked for fast latency.
    pub fast: bool,
    /// Where the call is in its lifecycle.
    pub status: CallStatusView,
    /// Provider finish reason, when completed.
    pub finish_reason: Option<String>,
    /// When the call row was created.
    pub created_at_ms: i64,
    /// When the request started.
    pub request_started_at_ms: Option<i64>,
    /// When response headers arrived.
    pub response_headers_at_ms: Option<i64>,
    /// When the first response event arrived.
    pub first_event_at_ms: Option<i64>,
    /// When the first non-empty text arrived.
    pub first_text_at_ms: Option<i64>,
    /// When the first valid response event arrived.
    pub first_valid_response_at_ms: Option<i64>,
    /// When the call reached a terminal status.
    pub finished_at_ms: Option<i64>,
    /// Queue time before the request started.
    pub queue_ms: Option<i64>,
    /// Time to the upstream HTTP response headers.
    pub response_headers_ms: Option<i64>,
    /// Time to first text.
    pub ttft_ms: Option<i64>,
    /// Time to first valid response.
    pub ttfr_ms: Option<i64>,
    /// Total duration.
    pub duration_ms: Option<i64>,
    /// Input tokens.
    pub input_tokens: Option<i64>,
    /// Output tokens.
    pub output_tokens: Option<i64>,
    /// Total tokens.
    pub total_tokens: Option<i64>,
    /// Cache-read tokens.
    pub cache_read_tokens: Option<i64>,
    /// Cache-write tokens.
    pub cache_write_tokens: Option<i64>,
    /// Reasoning tokens.
    pub reasoning_tokens: Option<i64>,
    /// Message count in the projected history.
    pub message_count: i64,
    /// Tool count in the projected prompt.
    pub tool_count: i64,
    /// Request body bytes.
    pub request_bytes: Option<i64>,
    /// Response bytes, including non-detailed metrics.
    pub response_bytes: i64,
    /// Raw network response chunk count.
    pub response_chunk_count: i64,
    /// HTTP status of the first plugin network request.
    pub http_status: Option<i64>,
    /// Error kind, when failed.
    pub error_kind: Option<String>,
    /// Error message, when failed.
    pub error_message: Option<String>,
    /// Whether this call asked to store request and response payloads.
    pub detailed: bool,
    /// Whether the stored request body was replaced because it exceeded the limit.
    pub request_truncated: bool,
    /// Whether later response chunks were omitted after reaching the storage limit.
    pub response_truncated: bool,
}

/// Counts of stored call records.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct CallStorageInfo {
    /// Number of call summaries.
    pub call_count: i64,
    /// Number of stored request payloads.
    pub request_count: i64,
    /// Number of stored response chunks.
    pub chunk_count: i64,
}

/// Call list, detailed-mode setting, and storage counts.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct CallsSnapshot {
    /// Newest calls first.
    pub calls: Vec<CallRecord>,
    /// Whether new calls store request and response payloads.
    pub detailed: bool,
    /// Storage counts.
    pub storage: CallStorageInfo,
    /// Whether a recent read or write failed.
    pub degraded: bool,
}

/// Stored request headers and body for a detailed call.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct CallRequestRecord {
    /// JSON text of the redacted headers.
    pub headers_json: String,
    /// JSON text of the request body.
    pub body_json: String,
    /// Request body bytes.
    pub byte_count: i64,
}

/// One stored response chunk.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct CallChunkRecord {
    /// Sequence number.
    pub seq: i64,
    /// Offset from request start.
    pub received_offset_ms: i64,
    /// Raw chunk text.
    pub data: String,
    /// Chunk bytes.
    pub byte_count: i64,
}

/// Full call, plus optional request and response payloads.
#[derive(Debug, Clone, PartialEq, JsObject)]
pub struct CallDetail {
    /// Call summary.
    pub call: CallRecord,
    /// Request payload, present only for detailed calls.
    pub request: Option<CallRequestRecord>,
    /// Response chunks in sequence.
    pub chunks: Vec<CallChunkRecord>,
}

impl From<calls::LlmCallSummary> for CallRecord {
    fn from(value: calls::LlmCallSummary) -> Self {
        Self {
            call_id: value.call_id,
            run_id: value.run_id,
            conversation_id: value.conversation_id,
            provider_call_index: value.provider_call_index,
            model_hash: value.model_hash,
            provider_type: value.provider_type,
            provider_url: value.provider_url,
            request_type: value.request_type,
            request_url: value.request_url,
            model_id: value.model_id,
            display_name: value.display_name,
            reasoning_effort: value.reasoning_effort,
            fast: value.fast,
            status: value.status.into(),
            finish_reason: value.finish_reason,
            created_at_ms: value.created_at_ms,
            request_started_at_ms: value.request_started_at_ms,
            response_headers_at_ms: value.response_headers_at_ms,
            first_event_at_ms: value.first_event_at_ms,
            first_text_at_ms: value.first_text_at_ms,
            first_valid_response_at_ms: value.first_valid_response_at_ms,
            finished_at_ms: value.finished_at_ms,
            queue_ms: value.queue_ms,
            response_headers_ms: value.response_headers_ms,
            ttft_ms: value.ttft_ms,
            ttfr_ms: value.ttfr_ms,
            duration_ms: value.duration_ms,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            total_tokens: value.total_tokens,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            reasoning_tokens: value.reasoning_tokens,
            message_count: value.message_count,
            tool_count: value.tool_count,
            request_bytes: value.request_bytes,
            response_bytes: value.response_bytes,
            response_chunk_count: value.response_chunk_count,
            http_status: value.http_status,
            error_kind: value.error_kind,
            error_message: value.error_message,
            detailed: value.detailed,
            request_truncated: value.request_truncated,
            response_truncated: value.response_truncated,
        }
    }
}

impl From<calls::CallStorage> for CallStorageInfo {
    fn from(value: calls::CallStorage) -> Self {
        Self {
            call_count: value.call_count,
            request_count: value.request_count,
            chunk_count: value.chunk_count,
        }
    }
}

impl From<calls::CallSnapshot> for CallsSnapshot {
    fn from(value: calls::CallSnapshot) -> Self {
        Self {
            calls: value.calls.into_iter().map(Into::into).collect(),
            detailed: value.detailed,
            storage: value.storage.into(),
            degraded: value.degraded,
        }
    }
}

impl From<calls::LlmCallRequest> for CallRequestRecord {
    fn from(value: calls::LlmCallRequest) -> Self {
        Self {
            headers_json: value.headers.to_string(),
            body_json: value.body.to_string(),
            byte_count: value.byte_count,
        }
    }
}

impl From<calls::LlmCallResponseChunk> for CallChunkRecord {
    fn from(value: calls::LlmCallResponseChunk) -> Self {
        Self {
            seq: value.seq,
            received_offset_ms: value.received_offset_ms,
            data: value.data,
            byte_count: value.byte_count,
        }
    }
}

/// Magpie model-call records.
///
/// Create one and keep it for the app's lifetime: it shares the store with the
/// local loop.
#[class]
pub struct Calls {
    store: CallStore,
}

impl std::fmt::Debug for Calls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `CallStore` owns the SQLite pool; naming the wrapper keeps its Debug
        // independent of database connection state.
        f.debug_struct("Calls").field("store", &self.store).finish()
    }
}

#[class]
impl Calls {
    /// Opens the shared call store used by the local loop.
    pub fn new() -> Self {
        Self::default()
    }

    /// Newest calls, the detailed-mode setting, and storage counts.
    pub async fn snapshot(this: Instance<Self>) -> CallsSnapshot {
        this.with(|calls| calls.store.clone())
            .snapshot(calls::DEFAULT_LIST_LIMIT)
            .await
            .into()
    }

    /// Full call, or `null` after the record was deleted.
    pub async fn detail(
        this: Instance<Self>,
        call_id: String,
    ) -> Result<Option<CallDetail>, Error> {
        let store = this.with(|calls| calls.store.clone());
        let Some(call) = store.llm_call(&call_id).await? else {
            return Ok(None);
        };
        let request = store.llm_call_request(&call_id).await?;
        let chunks = store.llm_call_chunks(&call_id).await?;
        Ok(Some(CallDetail {
            call: call.into(),
            request: request.map(Into::into),
            chunks: chunks.into_iter().map(Into::into).collect(),
        }))
    }

    /// Turns detailed recording on or off for later calls.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the toggle maps 1:1 to a TSX boolean, so the boundary takes the same bool the UI binds to"
    )]
    pub async fn set_detailed(
        this: Instance<Self>,
        detailed: bool,
    ) -> Result<CallsSnapshot, Error> {
        let store = this.with(|calls| calls.store.clone());
        let storage = if detailed {
            PayloadStorage::Detailed
        } else {
            PayloadStorage::Metrics
        };
        store.set_detailed_logging(storage).await?;
        Ok(store.snapshot(calls::DEFAULT_LIST_LIMIT).await.into())
    }

    /// Deletes stored request and response payloads, keeping summaries.
    pub async fn clear_details(this: Instance<Self>) -> Result<CallsSnapshot, Error> {
        this.with(|calls| calls.store.clone())
            .clear_details()
            .await
            .map(Into::into)
    }

    /// Deletes every recorded call.
    pub async fn clear_all(this: Instance<Self>) -> Result<CallsSnapshot, Error> {
        this.with(|calls| calls.store.clone())
            .clear_all()
            .await
            .map(Into::into)
    }
}

impl Default for Calls {
    /// The store is process-wide, so `Default` and `new` are the same operation.
    fn default() -> Self {
        Self {
            store: shared_call_store(),
        }
    }
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
    state: Mutex<BackendState>,
    /// Kept alive for as long as the process runs: the backend owns the axum
    /// task Cursor talks to, so dropping it would close the port.
    backend: OnceLock<Backend>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendState {
    Idle,
    Starting,
    Running,
}

fn backend_watch() -> &'static Arc<BackendWatch> {
    BACKEND.get_or_init(|| {
        let (sender, _) = watch::channel(None);
        Arc::new(BackendWatch {
            sender,
            state: Mutex::new(BackendState::Idle),
            backend: OnceLock::new(),
        })
    })
}

fn backend_state(watch: &BackendWatch) -> parking_lot::MutexGuard<'_, BackendState> {
    watch.state.lock()
}

fn backend_addr() -> watch::Receiver<Option<String>> {
    backend_watch().sender.subscribe()
}

fn start_backend(plugins: PluginHost) {
    start_backend_on(
        Arc::clone(backend_watch()),
        Backend::start(plugins, shared_call_store()),
    );
}

fn start_backend_on(
    watch: Arc<BackendWatch>,
    start: impl std::future::Future<Output = Result<Backend>> + Send + 'static,
) {
    // Synchronous members run on the engine thread; an async member retries later.
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("the local loop backend waits for a runtime");
        return;
    };
    {
        let mut state = backend_state(&watch);
        if *state != BackendState::Idle {
            return;
        }
        *state = BackendState::Starting;
    }
    let starting = runtime.spawn(start);
    runtime.spawn(async move {
        match starting.await {
            Ok(Ok(backend)) => {
                let addr = backend.addr().to_string();
                *backend_state(&watch) = BackendState::Running;
                watch.sender.send_replace(Some(addr));
                if watch.backend.set(backend).is_err() {
                    tracing::error!("the local loop backend started twice");
                }
            }
            Ok(Err(error)) => {
                *backend_state(&watch) = BackendState::Idle;
                watch.sender.send_replace(None);
                tracing::error!(%error, "cannot start the local loop backend");
            }
            Err(error) => {
                *backend_state(&watch) = BackendState::Idle;
                watch.sender.send_replace(None);
                tracing::error!(%error, "local loop backend startup task failed");
            }
        }
    });
}

async fn attach_backend(
    harness: &CursorHarness,
    backend: &mut watch::Receiver<Option<String>>,
) -> std::result::Result<(), HarnessError> {
    if backend.borrow().is_none()
        && let Ok(host) = shared_plugin_host()
    {
        start_backend(host);
    }
    if backend.borrow().is_none() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), backend.changed()).await;
    }
    // Bind the address before the branch so the `watch::Ref` is released here
    // and does not stay borrowed across `set_backend_addr`'s await.
    let addr = backend.borrow().clone();
    if let Some(addr) = addr {
        harness
            .set_backend_addr(addr.parse().map_err(|error| {
                HarnessError::Config(format!("invalid local backend address: {error}"))
            })?)
            .await?;
    }
    Ok(())
}

static PLUGIN_HOST: OnceLock<PluginHost> = OnceLock::new();
static CALL_STORE: OnceLock<CallStore> = OnceLock::new();
static BACKEND: OnceLock<Arc<BackendWatch>> = OnceLock::new();

fn shared_call_store() -> CallStore {
    if let Some(store) = CALL_STORE.get() {
        return store.clone();
    }
    let store = CallStore::at(crate::storage::data_dir().map_or_else(
        |error| {
            tracing::error!(%error, "cannot open LLM call records");
            std::env::temp_dir().join("cursor-byok-next-calls.db")
        },
        |dir| dir.join("calls.db"),
    ));
    CALL_STORE.get_or_init(|| store.clone()).clone()
}

#[cfg(test)]
mod backend_lifecycle_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use parking_lot::Mutex;
    use tokio::sync::{Notify, watch};

    use super::{BackendState, backend_state, start_backend_on};
    use crate::{Result, backend::Backend};

    fn isolated_watch() -> Arc<super::BackendWatch> {
        let (sender, _) = watch::channel(None);
        Arc::new(super::BackendWatch {
            sender,
            state: Mutex::new(BackendState::Idle),
            backend: std::sync::OnceLock::new(),
        })
    }

    async fn wait_for_state(watch: &super::BackendWatch, expected: BackendState) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if *backend_state(watch) == expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("backend did not reach {expected:?}"));
    }

    async fn wait_for_attempts(attempts: &AtomicUsize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while attempts.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backend startup future was not polled");
    }

    #[test]
    fn startup_outside_a_runtime_stays_idle() {
        let watch = isolated_watch();
        let started = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::clone(&started);
        start_backend_on(Arc::clone(&watch), async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<Backend>>().await
        });
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert_eq!(*backend_state(&watch), BackendState::Idle);
    }

    #[tokio::test]
    async fn concurrent_calls_start_only_once() {
        let watch = isolated_watch();
        let release = Arc::new(Notify::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let release = Arc::clone(&release);
            let attempts = Arc::clone(&attempts);
            start_backend_on(Arc::clone(&watch), async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                std::future::pending::<Result<Backend>>().await
            });
        }
        wait_for_state(&watch, BackendState::Starting).await;
        wait_for_attempts(&attempts).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(*watch.sender.borrow(), None);
    }

    #[tokio::test]
    async fn startup_failure_wakes_waiters_and_allows_a_retry() {
        let watch = isolated_watch();
        let mut receiver = watch.sender.subscribe();
        start_backend_on(Arc::clone(&watch), async {
            Err(crate::Error::Protocol("injected startup failure".into()))
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), receiver.changed())
            .await
            .expect("startup failure must wake the status waiter")
            .unwrap();
        assert_eq!(*receiver.borrow(), None);
        assert_eq!(*backend_state(&watch), BackendState::Idle);

        let attempts = Arc::new(AtomicUsize::new(0));
        let retried = Arc::clone(&attempts);
        start_backend_on(Arc::clone(&watch), async move {
            retried.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Result<Backend>>().await
        });
        wait_for_state(&watch, BackendState::Starting).await;
        wait_for_attempts(&attempts).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn startup_panic_also_allows_a_retry() {
        let watch = isolated_watch();
        start_backend_on(Arc::clone(&watch), async {
            panic!("injected startup panic");
        });
        wait_for_state(&watch, BackendState::Idle).await;
        start_backend_on(
            Arc::clone(&watch),
            std::future::pending::<Result<Backend>>(),
        );
        wait_for_state(&watch, BackendState::Starting).await;
    }
}
