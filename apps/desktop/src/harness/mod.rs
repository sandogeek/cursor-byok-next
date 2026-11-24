//! Makes Cursor route its own traffic to this app.
//!
//! Takeover needs three things at once, and each failure has a different
//! symptom:
//!
//! | What | Who writes it | Without it |
//! |---|---|---|
//! | Cursor points at the local proxy | [`SettingsStore`] | Cursor talks to `api2.cursor.sh` directly |
//! | Cursor trusts the certificates the proxy signs | [`CaManager`] | every `*.cursor.sh` request fails TLS |
//! | Cursor believes it is signed in | [`AccountStore`] | sign-in and quota UI breaks |
//!
//! `process` only makes a fresh proxy setting apply sooner; it is optional.
//! This module is the state machine over the three, and every entry point is
//! safe to call twice.

mod account;
mod ca;
mod process;
mod proxy;
mod settings;
mod state;

#[cfg(target_os = "windows")]
mod windows;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use parking_lot::{Mutex as SyncMutex, RwLock};
use serde::Serialize;
use tokio::sync::Mutex;

use self::{
    account::AccountStore,
    ca::CaManager,
    proxy::{ProxyRuntime, Routing},
    settings::{ManagedSettings, SettingsStore},
    state::{HarnessState, HarnessStateStore, PortChoice},
};

pub use self::account::request_uses_local_cursor_token;
pub use self::process::terminate_cursor as quit_cursor;
pub use self::proxy::{Routing as ProxyRouting, UPSTREAM_URL_HEADER, is_cursor_host};

/// Errors from copying Cursor's files and from running the proxy.
///
/// `Config` and `Process` carry their own description because they report
/// violated invariants of this app ("the CA is not trusted", "Cursor is not
/// running") rather than a wrapped foreign error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Process(String),
    #[error("{0}")]
    Proxy(String),
    #[error("cannot read Cursor's account database: {source}")]
    Database {
        #[source]
        source: sqlx::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl From<sqlx::Error> for Error {
    fn from(source: sqlx::Error) -> Self {
        Self::Database { source }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Whether the system trusts the CA this app generated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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

/// How much of takeover is actually working right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Disabled,
    Enabled,
    /// The proxy and the settings disagree: one of them is stale.
    Degraded,
}

/// What a settings dialog shows. `proxyUrl` is present only while the proxy
/// runs, so the UI can tell "off" from "starting" from "failed".
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessStatus {
    pub platform: String,
    pub ca: CaState,
    pub integration: IntegrationState,
    pub settings_applied: bool,
    pub proxy_url: Option<String>,
    pub proxy_port: Option<u16>,
    /// The port the user chose in system settings. `0` asks the OS to pick;
    /// absent means no choice was saved, so the last bound port is reused.
    pub preferred_proxy_port: Option<u16>,
    /// Where this app keeps `harness.json`, the CA and its plugins.
    pub data_dir: String,
    pub ca_install_command: Option<String>,
}

/// Where takeover writes. Tests build one of these pointing at temporary files.
#[derive(Debug)]
pub struct HarnessPaths {
    pub settings: SettingsStore,
    pub account: AccountStore,
    pub ca: CaManager,
    pub state: HarnessStateStore,
    /// The app's own directory, shown in system settings.
    pub data_dir: PathBuf,
}

impl HarnessPaths {
    /// Cursor's real files, and this app's own data directory.
    ///
    /// # Errors
    ///
    /// Fails when the home directory cannot be resolved or `~/.cursor-byok-next`
    /// cannot be created.
    pub fn managed() -> Result<Self> {
        let data_dir = crate::storage::data_dir()?;
        Ok(Self {
            settings: SettingsStore::managed()?,
            account: AccountStore::managed()?,
            ca: CaManager::at(data_dir.join("ca")),
            state: HarnessStateStore::at(data_dir.join("harness.json")),
            data_dir,
        })
    }
}

/// The takeover lifecycle. One per app, driven by the settings UI.
pub struct CursorHarness {
    paths: Arc<HarnessPaths>,
    ca_initialization: SyncMutex<()>,
    /// Serializes enable, disable and restoration across the proxy lock's
    /// intentional await gaps. Cursor's files are one shared state machine.
    operation: Mutex<()>,
    routing: Routing,
    proxy: Mutex<ProxyRuntime>,
    /// Whether the user wants takeover, and the port last bound. Mirrors
    /// `harness.json` so the proxy can be restarted on the same port.
    intent: RwLock<HarnessState>,
}

impl std::fmt::Debug for CursorHarness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CursorHarness")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

/// The port takeover falls back to on platforms where it has never bound one.
/// `0` lets the OS pick; the bound port is remembered from then on.
#[cfg(target_os = "windows")]
const DEFAULT_PROXY_PORT: u16 = 39831;
#[cfg(not(target_os = "windows"))]
const DEFAULT_PROXY_PORT: u16 = 0;

impl CursorHarness {
    /// Restores the previous run's intent and port, if any.
    pub fn new(paths: HarnessPaths) -> Self {
        let intent = paths.state.load();
        Self {
            paths: Arc::new(paths),
            ca_initialization: SyncMutex::new(()),
            operation: Mutex::new(()),
            routing: Routing::default(),
            proxy: Mutex::new(ProxyRuntime::default()),
            intent: RwLock::new(intent),
        }
    }

    /// Connects a loopback local service. If takeover is already running, this
    /// also applies its account and settings before local routing begins.
    pub async fn set_backend_addr(&self, addr: SocketAddr) -> Result<()> {
        if !addr.ip().is_loopback() {
            return Err(Error::Config(
                "the Cursor backend must be on loopback".into(),
            ));
        }
        let _operation = self.operation.lock().await;
        let url = self.proxy.lock().await.url();
        self.routing.set_backend_addr(addr);
        // Starting local routing while takeover runs applies the account and
        // settings a fresh enable would, so the two paths cannot drift.
        match url {
            Some(url) => self.apply(&url).await,
            None => Ok(()),
        }
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.proxy.lock().await.port()
    }

    /// Drops the proxy a previous, crashed run left in `settings.json`.
    /// Returns `true` when it did. A running harness is never stale.
    pub async fn cleanup_stale_settings(&self) -> Result<bool> {
        let _operation = self.operation.lock().await;
        if self.proxy.lock().await.running() {
            return Ok(false);
        }
        let Some(url) = self.saved_proxy_url() else {
            return Ok(false);
        };
        self.paths.settings.clear_proxy_settings(&url)
    }

    /// Generates the CA when missing. Blocking work: run it from a task.
    pub fn initialize_ca(&self) -> Result<()> {
        let _guard = self.ca_initialization.lock();
        self.paths.ca.initialize()
    }

    /// The current state, restoring a wanted takeover whose proxy is gone.
    ///
    /// A failed restore is logged, not returned: the screen still needs to show
    /// `degraded` and the reason the proxy is missing, rather than an error
    /// page.
    pub async fn status(&self) -> HarnessStatus {
        let _operation = self.operation.lock().await;
        self.restore().await;
        self.report().await
    }

    /// Turns takeover on or off. Enabling fails before touching Cursor when the
    /// CA is not trusted. The persisted intent is committed before files change.
    ///
    /// The flag stays a `bool` because it is the `#[class]` boundary the TSX
    /// takeover switch calls; an enum would change the generated binding.
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "the bool is the TSX takeover switch's argument at the native boundary"
    )]
    pub async fn set_enabled(&self, enabled: bool) -> Result<HarnessStatus> {
        let _operation = self.operation.lock().await;
        self.update_intent(|intent| intent.enabled = enabled)?;

        if enabled {
            self.enable().await?;
        } else {
            self.disable().await?;
        }
        Ok(self.report().await)
    }

    /// Remembers the port the user asked for; `Some(0)` lets the OS pick and
    /// `None` clears the choice. Takes effect the next time the proxy starts.
    ///
    /// # Errors
    ///
    /// Fails when `harness.json` cannot be written; the in-memory intent then
    /// keeps its previous value.
    pub async fn set_preferred_port(&self, port: Option<u16>) -> Result<HarnessStatus> {
        let _operation = self.operation.lock().await;
        self.update_intent(|intent| intent.preferred_port = PortChoice::from_boundary(port))?;
        Ok(self.report().await)
    }

    /// The one read-modify-write of the intent. Callers hold `operation`.
    fn update_intent(&self, change: impl FnOnce(&mut HarnessState)) -> Result<()> {
        let mut intent = *self.intent.read();
        change(&mut intent);
        self.paths.state.save(intent)?;
        *self.intent.write() = intent;
        Ok(())
    }

    /// Restores a wanted takeover without propagating recovery errors to the
    /// status screen. `report` exposes the resulting degraded state instead.
    async fn restore(&self) {
        if !self.intent.read().enabled || self.ca_state().await != CaState::Ready {
            return;
        }
        let proxy_url = {
            let proxy = self.proxy.lock().await;
            proxy.running().then(|| proxy.url()).flatten()
        };
        let healthy = match proxy_url {
            Some(url) => self
                .managed_settings(Some(url))
                .await
                .is_ok_and(ManagedSettings::is_applied),
            None => false,
        };
        if !healthy && let Err(error) = self.enable().await {
            tracing::warn!(%error, "could not restore Cursor takeover");
        }
    }

    /// Probes independently, so one unreadable file cannot hide the rest of
    /// the integration state. An inconsistent or unreadable state is degraded.
    async fn report(&self) -> HarnessStatus {
        let ca = self.ca_state().await;
        // Snapshot the proxy under its lock, then drop it before touching the
        // filesystem: `managed_state` reads and parses `settings.json`.
        let (proxy_url, proxy_port, proxy_running) = {
            let proxy = self.proxy.lock().await;
            (proxy.url(), proxy.port(), proxy.running())
        };
        let settings_url = proxy_url.clone().or_else(|| self.saved_proxy_url());
        let settings = self
            .managed_settings(settings_url)
            .await
            .inspect_err(|error| tracing::warn!(%error, "could not read Cursor settings"));
        let settings_applied = settings.as_ref().is_ok_and(|state| state.is_applied());
        let integration = match (proxy_running, settings_applied, settings) {
            (false, false, Ok(ManagedSettings::NotConfigured | ManagedSettings::Missing)) => {
                IntegrationState::Disabled
            }
            (true, true, _) => IntegrationState::Enabled,
            _ => IntegrationState::Degraded,
        };
        HarnessStatus {
            platform: std::env::consts::OS.to_owned(),
            ca,
            integration,
            settings_applied,
            proxy_port,
            proxy_url,
            preferred_proxy_port: self.intent.read().preferred_port.to_boundary(),
            data_dir: self.paths.data_dir.display().to_string(),
            ca_install_command: self.paths.ca.install_command(),
        }
    }

    /// Reads the CA state. It touches the filesystem and, on macOS, runs
    /// `security`, so it must not block the async runtime.
    async fn ca_state(&self) -> CaState {
        let paths = Arc::clone(&self.paths);
        let inspected = tokio::task::spawn_blocking(move || paths.ca.state()).await;
        match inspected {
            Ok(Ok(state)) => state,
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not inspect the local CA");
                CaState::Invalid
            }
            Err(error) => {
                tracing::warn!(%error, "the CA inspection task failed");
                CaState::Invalid
            }
        }
    }

    /// Reads the managed state of Cursor's `settings.json` off the async
    /// thread; the file is a synchronous read and JSONC parse.
    async fn managed_settings(&self, proxy_url: Option<String>) -> Result<ManagedSettings> {
        let paths = Arc::clone(&self.paths);
        tokio::task::spawn_blocking(move || paths.settings.managed_state(proxy_url.as_deref()))
            .await
            .map_err(|error| {
                Error::Process(format!("the settings inspection task failed: {error}"))
            })?
    }

    fn saved_proxy_url(&self) -> Option<String> {
        self.intent
            .read()
            .proxy_port
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    async fn enable(&self) -> Result<()> {
        if !matches!(self.paths.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "trust the local CA before enabling Cursor takeover".into(),
            ));
        }
        // Read the running URL and release the proxy lock before any `.await`:
        // terminating Cursor takes a full process scan and would otherwise
        // block `status()` and `proxy_port()` behind it.
        let running_url = self.proxy.lock().await.url();
        let settings_applied = match running_url.clone() {
            Some(url) => self.managed_settings(Some(url)).await?.is_applied(),
            None => false,
        };
        if !settings_applied {
            // Optional: it only makes the fresh setting take effect sooner.
            match process::terminate_cursor().await {
                Ok(true) => tracing::info!("quit Cursor so the proxy setting applies"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not quit Cursor before applying proxy settings");
                }
            }
        }
        if let Some(url) = running_url {
            return self.apply(&url).await;
        }
        let mut proxy = self.proxy.lock().await;
        let Some(url) = proxy.url() else {
            // Reuse the port the user chose, else the one Cursor's settings were
            // already pointed at, so a restart never invalidates them. A saved `0`
            // is the user asking for a fresh OS pick, so it is not a fallback.
            let intent = *self.intent.read();
            let preferred = intent
                .preferred_port
                .request(intent.proxy_port, DEFAULT_PROXY_PORT);
            let ca = self.paths.ca.load()?;
            let started = proxy.start(self.routing.clone(), ca, preferred).await?;
            if started.port != preferred {
                tracing::warn!(
                    requested = preferred,
                    actual_port = started.port,
                    "the preferred proxy port was taken"
                );
            }
            let url = started.url.clone();
            let port = started.port;
            // The proxy is running now; release the lock before writing files.
            drop(proxy);

            // Cursor's files and the remembered port must both name the port that
            // is actually open, or the next start leaves Cursor on a dead port. A
            // failure in either write undoes the proxy that is already listening.
            let applied = self
                .apply(&url)
                .await
                .and_then(|()| self.update_intent(|intent| intent.proxy_port = Some(port)));
            if let Err(error) = applied {
                // A rollback that also fails is worth a log line: Cursor's settings
                // may still name the port that is about to close.
                if let Err(cleanup) = self.paths.settings.clear_proxy_settings(&url) {
                    tracing::warn!(%cleanup, "could not roll back Cursor proxy settings");
                }
                self.proxy.lock().await.stop().await;
                return Err(error);
            }
            return Ok(());
        };
        // Another task started the proxy while this one waited for the lock.
        drop(proxy);
        self.apply(&url).await
    }

    async fn disable(&self) -> Result<()> {
        let proxy_url = self
            .proxy
            .lock()
            .await
            .url()
            .or_else(|| self.saved_proxy_url());
        self.clear_settings_then_stop(proxy_url).await
    }

    /// Clears Cursor's matching setting first: stopping the proxy while Cursor
    /// still points at it would leave Cursor talking to a closed port.
    async fn clear_settings_then_stop(&self, proxy_url: Option<String>) -> Result<()> {
        if let Some(url) = proxy_url {
            self.paths.settings.clear_proxy_settings(&url)?;
        }
        self.proxy.lock().await.stop().await;
        Ok(())
    }

    /// Points Cursor at a running proxy: the local account when this app serves
    /// the backend, then the proxy setting.
    async fn apply(&self, proxy_url: &str) -> Result<()> {
        if self.routing.has_backend() {
            self.paths.account.inject_if_missing().await?;
        }
        self.paths.settings.write_proxy_settings(proxy_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn harness(dir: &tempfile::TempDir) -> CursorHarness {
        CursorHarness::new(HarnessPaths {
            settings: SettingsStore::at(dir.path().join("settings.json")),
            account: AccountStore::at(dir.path().join("state.vscdb")),
            ca: CaManager::at(dir.path().join("ca")),
            state: HarnessStateStore::at(dir.path().join("harness.json")),
            data_dir: dir.path().to_path_buf(),
        })
    }

    #[tokio::test]
    async fn status_reports_disabled_before_anything_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let status = harness(&dir).status().await;

        assert_eq!(status.ca, CaState::Missing);
        assert_eq!(status.integration, IntegrationState::Disabled);
        assert!(!status.settings_applied);
        assert_eq!(status.proxy_url, None);
    }

    #[tokio::test]
    async fn enabling_without_a_trusted_ca_fails_before_touching_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);
        harness.initialize_ca().unwrap();

        assert!(harness.set_enabled(true).await.is_err());
        assert!(!dir.path().join("settings.json").exists());
        assert!(harness.paths.state.load().enabled);
    }

    #[tokio::test]
    async fn disable_clears_settings_and_persists_the_intent() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);
        harness
            .paths
            .settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        *harness.intent.write() = HarnessState {
            enabled: true,
            proxy_port: Some(41000),
            preferred_port: PortChoice::Unset,
        };

        harness.disable().await.unwrap();

        assert!(
            !harness
                .paths
                .settings
                .managed_state(Some("http://127.0.0.1:41000"))
                .unwrap()
                .is_applied()
        );
        assert!(!harness.paths.state.load().enabled);
    }

    #[tokio::test]
    async fn stale_cleanup_reports_whether_it_removed_anything() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);

        assert!(!harness.cleanup_stale_settings().await.unwrap());
        harness
            .paths
            .settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        harness
            .paths
            .state
            .save(HarnessState {
                enabled: false,
                proxy_port: Some(41000),
                preferred_port: PortChoice::Unset,
            })
            .unwrap();
        *harness.intent.write() = harness.paths.state.load();
        assert!(harness.cleanup_stale_settings().await.unwrap());
    }

    #[tokio::test]
    async fn a_stale_proxy_settings_file_is_degraded() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);
        harness
            .paths
            .settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        *harness.intent.write() = HarnessState {
            enabled: false,
            proxy_port: Some(41000),
            preferred_port: PortChoice::Unset,
        };

        let status = harness.status().await;
        assert_eq!(status.integration, IntegrationState::Degraded);
        assert!(status.settings_applied);
    }

    #[tokio::test]
    async fn a_chosen_port_is_remembered_for_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);

        let status = harness.set_preferred_port(Some(41000)).await.unwrap();

        assert_eq!(status.preferred_proxy_port, Some(41000));
        assert_eq!(
            harness.paths.state.load().preferred_port,
            PortChoice::Fixed(41000)
        );
        assert_eq!(harness.paths.state.load().proxy_port, None);
    }

    #[tokio::test]
    async fn a_saved_zero_asks_for_a_fresh_os_pick_even_after_a_bind() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);
        *harness.intent.write() = HarnessState {
            enabled: false,
            proxy_port: Some(41000),
            preferred_port: PortChoice::Unset,
        };

        let status = harness.set_preferred_port(Some(0)).await.unwrap();

        assert_eq!(status.preferred_proxy_port, Some(0));
        let intent = *harness.intent.read();
        assert_eq!(
            intent
                .preferred_port
                .request(intent.proxy_port, DEFAULT_PROXY_PORT),
            0
        );
    }

    #[test]
    fn the_port_to_bind_prefers_the_choice_over_the_last_bind() {
        let chosen = PortChoice::Fixed(42000).request(Some(41000), DEFAULT_PROXY_PORT);
        assert_eq!(chosen, 42000);

        let never_chosen = PortChoice::Unset.request(Some(41000), DEFAULT_PROXY_PORT);
        assert_eq!(never_chosen, 41000);

        let first_run = PortChoice::Unset.request(None, DEFAULT_PROXY_PORT);
        assert_eq!(first_run, DEFAULT_PROXY_PORT);
    }

    #[tokio::test]
    async fn unreadable_settings_still_return_a_degraded_status() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);
        fs::write(dir.path().join("settings.json"), "{ not JSONC").unwrap();

        let status = harness.status().await;
        assert_eq!(status.integration, IntegrationState::Degraded);
        assert!(!status.settings_applied);
    }
}
