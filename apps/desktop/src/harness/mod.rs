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

use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc};

use parking_lot::{Mutex as SyncMutex, RwLock};
use serde::Serialize;
use tokio::sync::Mutex;

use self::{
    account::AccountStore,
    ca::CaManager,
    proxy::{ProxyRuntime, Routing},
    settings::SettingsStore,
    state::{HarnessState, HarnessStateStore},
};

pub use self::account::{is_local_cursor_authorization, local_token};
pub use self::process::terminate_cursor as quit_cursor;
pub use self::proxy::{is_cursor_host, Routing as ProxyRouting, UPSTREAM_URL_HEADER};

/// Errors from copying Cursor's files and from running the proxy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Process(String),
    #[error("{0}")]
    Database(String),
    #[error("{0}")]
    Proxy(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessStatus {
    pub platform: String,
    pub ca: CaState,
    pub integration: IntegrationState,
    pub settings_applied: bool,
    pub proxy_url: Option<String>,
    pub proxy_port: Option<u16>,
    pub ca_install_command: Option<String>,
}

/// Where takeover writes, and which port it prefers. Tests build one of these
/// pointing at temporary files.
#[derive(Clone, Debug)]
pub struct HarnessPaths {
    pub settings: SettingsStore,
    pub account: AccountStore,
    pub ca: CaManager,
    pub state: HarnessStateStore,
    /// The port Cursor is told to use, when a backend is set.
    pub proxy_port: u16,
}

impl HarnessPaths {
    /// Cursor's real files, and this app's own data directory.
    pub fn managed(proxy_port: u16) -> Result<Self> {
        Ok(Self {
            settings: SettingsStore::managed()?,
            account: AccountStore::managed()?,
            ca: CaManager::managed()?,
            state: HarnessStateStore::managed()?,
            proxy_port,
        })
    }
}

/// The takeover lifecycle. One per app, driven by the settings UI.
pub struct CursorHarness {
    paths: Arc<HarnessPaths>,
    ca_initialization: SyncMutex<()>,
    /// Set once the local server listens; until then the proxy forwards
    /// everything to Cursor untouched.
    backend: RwLock<Option<SocketAddr>>,
    proxy: Mutex<ProxyRuntime>,
    /// Whether the user wants takeover, and the port last bound. Mirrors
    /// `harness.json` so the proxy can be restarted on the same port.
    intent: RwLock<HarnessState>,
}

impl CursorHarness {
    /// Restores the previous run's intent and port, if any.
    pub fn new(paths: HarnessPaths) -> Self {
        let intent = paths.state.load();
        Self {
            paths: Arc::new(paths),
            ca_initialization: SyncMutex::new(()),
            backend: RwLock::new(None),
            proxy: Mutex::new(ProxyRuntime::default()),
            intent: RwLock::new(intent),
        }
    }

    /// The address the local service listens on. Until it is set, takeover
    /// still works but routes nothing locally.
    pub fn set_backend_addr(&self, addr: SocketAddr) {
        *self.backend.write() = Some(addr);
    }

    pub async fn proxy_port(&self) -> Option<u16> {
        self.proxy.lock().await.port()
    }

    /// Drops the proxy a previous, crashed run left in `settings.json`.
    /// Returns `true` when it did.
    pub fn cleanup_stale_settings(&self) -> Result<bool> {
        self.paths.settings.clear_stale_managed_settings()
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
    pub async fn status(&self) -> Result<HarnessStatus> {
        self.restore().await;
        self.report().await
    }

    /// Turns takeover on or off. Enabling fails before touching Cursor when the
    /// CA is not trusted.
    pub async fn set_enabled(&self, enabled: bool) -> Result<HarnessStatus> {
        if enabled {
            self.intent.write().enabled = true;
            if let Err(error) = self.enable().await {
                self.intent.write().enabled = false;
                return Err(error);
            }
        } else {
            self.disable().await?;
        }
        self.report().await
    }

    /// Puts a wanted takeover back after a crash or a failed start. Silent:
    /// `report` is what shows the outcome.
    async fn restore(&self) {
        let Ok(ca) = self.paths.ca.state() else {
            return;
        };
        if !self.intent.read().enabled || !matches!(ca, CaState::Ready) {
            return;
        }
        let healed = self.proxy.lock().await;
        let settings_applied = healed
            .url()
            .as_deref()
            .map(|url| self.paths.settings.settings_match(url))
            .transpose()
            .ok()
            .flatten()
            .unwrap_or(false);
        let healthy = healed.running() && settings_applied;
        drop(healed);
        if healthy {
            return;
        }
        if let Err(error) = self.enable().await {
            tracing::warn!(%error, "could not restore Cursor takeover");
        }
    }

    /// The state as it is, without changing anything.
    async fn report(&self) -> Result<HarnessStatus> {
        let ca = self.paths.ca.state()?;
        let proxy = self.proxy.lock().await;
        let proxy_url = proxy.url();
        let settings_applied = proxy_url
            .as_deref()
            .map(|url| self.paths.settings.settings_match(url))
            .transpose()?
            .unwrap_or(false);
        let integration = match (proxy.running(), settings_applied) {
            (false, false) => IntegrationState::Disabled,
            (true, true) => IntegrationState::Enabled,
            _ => IntegrationState::Degraded,
        };
        Ok(HarnessStatus {
            platform: std::env::consts::OS.to_owned(),
            ca,
            integration,
            settings_applied,
            proxy_port: proxy.port(),
            proxy_url,
            ca_install_command: self.paths.ca.install_command(),
        })
    }

    async fn enable(&self) -> Result<()> {
        if !matches!(self.paths.ca.state()?, CaState::Ready) {
            return Err(Error::Config(
                "trust the local CA before enabling Cursor takeover".into(),
            ));
        }
        let mut proxy = self.proxy.lock().await;
        let settings_applied = proxy
            .url()
            .as_deref()
            .map(|url| self.paths.settings.settings_match(url))
            .transpose()?
            .unwrap_or(false);
        if !settings_applied {
            // Optional: it only makes the fresh setting take effect sooner.
            match process::terminate_cursor().await {
                Ok(true) => tracing::info!("quit Cursor so the proxy setting applies"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not quit Cursor before applying proxy settings")
                }
            }
        }
        if proxy.running() {
            if let Some(url) = proxy.url() {
                drop(proxy);
                self.apply(&url).await?;
            }
            return Ok(());
        }
        // Reuse the port Cursor's settings were already pointed at, so a
        // restart never invalidates them.
        let preferred = {
            let intent = self.intent.read();
            if intent.proxy_port != 0 {
                intent.proxy_port
            } else {
                self.paths.proxy_port
            }
        };
        let ca = self.paths.ca.load()?;
        let routing = Routing {
            backend: *self.backend.read(),
        };
        let (url, actual_port) = proxy.start(routing, ca, preferred).await?;
        if actual_port != preferred {
            tracing::warn!(
                requested = preferred,
                actual_port,
                "the preferred proxy port was taken"
            );
        }
        // The proxy is running now; release the lock before writing files.
        drop(proxy);
        if let Err(error) = self.apply(&url).await {
            self.proxy.lock().await.stop().await;
            return Err(error);
        }
        let mut intent = self.intent.write();
        intent.enabled = true;
        intent.proxy_port = actual_port;
        self.paths.state.save(*intent)
    }

    /// Clears the settings first: stopping the proxy while Cursor still points
    /// at it would leave Cursor talking to a closed port.
    async fn disable(&self) -> Result<()> {
        self.paths.settings.clear_proxy_settings()?;
        let mut proxy = self.proxy.lock().await;
        let port = proxy.port().unwrap_or(self.intent.read().proxy_port);
        proxy.stop().await;
        let mut intent = self.intent.write();
        intent.enabled = false;
        intent.proxy_port = port;
        self.paths.state.save(*intent)
    }

    async fn apply(&self, proxy_url: &str) -> Result<()> {
        self.paths.account.inject_if_missing().await?;
        self.paths.settings.write_proxy_settings(proxy_url)
    }
}

/// The app's own directory: `~/.cursor-byok-nsh`, beside the data file.
fn data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
    let dir = home.join(".cursor-byok-nsh");
    // The app may start twice at once: whoever wins the race creates it.
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn harness(dir: &tempfile::TempDir) -> CursorHarness {
        CursorHarness::new(HarnessPaths {
            settings: SettingsStore::at(dir.path().join("settings.json")),
            account: AccountStore::at(dir.path().join("state.vscdb")),
            ca: CaManager::at(dir.path().join("ca")),
            state: HarnessStateStore::at(dir.path().join("harness.json")),
            proxy_port: 0,
        })
    }

    #[tokio::test]
    async fn status_reports_disabled_before_anything_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let status = harness(&dir).status().await.unwrap();

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
        assert!(!harness.paths.state.load().enabled);
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

        harness.disable().await.unwrap();

        assert!(!harness
            .paths
            .settings
            .settings_match("http://127.0.0.1:41000")
            .unwrap());
        assert!(!harness.paths.state.load().enabled);
    }

    #[tokio::test]
    async fn stale_cleanup_reports_whether_it_removed_anything() {
        let dir = tempfile::tempdir().unwrap();
        let harness = harness(&dir);

        assert!(!harness.cleanup_stale_settings().unwrap());
        harness
            .paths
            .settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        assert!(harness.cleanup_stale_settings().unwrap());
    }
}