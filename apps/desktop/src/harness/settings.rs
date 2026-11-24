//! Points Cursor's own HTTP stack at the local proxy.
//!
//! Cursor reads its proxy from `settings.json`; these five keys are the whole
//! contract, and `managed_state` is the only proof that a freshly written proxy
//! is actually in effect.

use std::{collections::BTreeMap, fs, io, path::PathBuf};

use serde_json::Value;

use super::{Error, Result};

const NO_PROXY_KEY: &str = "http.noProxy";
/// The exact values takeover writes. A proxy is "managed" only when all of them
/// still hold, which is what keeps this app from deleting someone else's proxy.
const PROXY_KEY: &str = "http.proxy";
const KERBEROS_KEY: &str = "http.proxyKerberosServicePrincipal";
const SUPPORT_KEY: &str = "http.proxySupport";
const DISABLE_HTTP2_KEY: &str = "cursor.general.disableHttp2";
const SYSTEM_CERTS_KEY: &str = "http.experimental.systemCertificatesV2";
/// Every key takeover writes; clearing removes exactly these.
const MANAGED_KEYS: [&str; 5] = [
    PROXY_KEY,
    KERBEROS_KEY,
    SUPPORT_KEY,
    DISABLE_HTTP2_KEY,
    SYSTEM_CERTS_KEY,
];
const PROXY_SUPPORT_ON: &str = "on";

/// Cursor's user settings file, edited in place.
#[derive(Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

/// Whether `settings.json` carries the exact proxy takeover writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedSettings {
    /// No proxy URL is known, so there is nothing to compare against.
    NotConfigured,
    /// The managed keys are absent or were changed by someone else.
    Missing,
    /// Every managed key still holds what takeover wrote.
    Applied,
}

impl ManagedSettings {
    /// Whether the file holds the managed signature.
    #[must_use]
    pub fn is_applied(self) -> bool {
        self == Self::Applied
    }
}

impl SettingsStore {
    /// Cursor's real settings file.
    ///
    /// # Errors
    ///
    /// Fails when the home directory cannot be resolved or the platform is one
    /// this app does not support.
    pub fn managed() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
        let path = match std::env::consts::OS {
            "macos" => home.join("Library/Application Support/Cursor/User/settings.json"),
            "windows" => std::env::var_os("APPDATA")
                .map_or_else(|| home.join("AppData/Roaming"), PathBuf::from)
                .join("Cursor/User/settings.json"),
            "linux" => std::env::var_os("XDG_CONFIG_HOME")
                .map_or_else(|| home.join(".config"), PathBuf::from)
                .join("Cursor/User/settings.json"),
            platform => {
                return Err(Error::Config(format!(
                    "Cursor settings are unsupported on {platform}"
                )));
            }
        };
        Ok(Self { path })
    }

    /// A store over an explicit file, for tests.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Writes every managed key, and drops `http.noProxy` so no exclusion can
    /// bypass the proxy.
    ///
    /// # Errors
    ///
    /// Fails when the existing file cannot be parsed or the new one written.
    pub fn write_proxy_settings(&self, proxy_url: &str) -> Result<()> {
        let mut settings = self.read()?;
        settings.remove(NO_PROXY_KEY);
        let url = Value::String(proxy_url.to_owned());
        settings.insert(PROXY_KEY.to_owned(), url.clone());
        settings.insert(KERBEROS_KEY.to_owned(), url);
        settings.insert(
            SUPPORT_KEY.to_owned(),
            Value::String(PROXY_SUPPORT_ON.to_owned()),
        );
        settings.insert(DISABLE_HTTP2_KEY.to_owned(), Value::Bool(true));
        settings.insert(SYSTEM_CERTS_KEY.to_owned(), Value::Bool(true));
        self.write(&settings)
    }

    /// Removes the managed keys, but only when all of them still match
    /// `proxy_url`. Returns whether anything was removed. A loopback proxy
    /// belonging to another tool is not a managed signature, so it survives.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be parsed or rewritten.
    pub fn clear_proxy_settings(&self, proxy_url: &str) -> Result<bool> {
        let mut settings = self.read()?;
        if !matches_managed_proxy(&settings, proxy_url) {
            return Ok(false);
        }
        for key in MANAGED_KEYS {
            settings.remove(key);
        }
        self.write(&settings)?;
        Ok(true)
    }

    /// Whether the file currently holds the managed signature for `proxy_url`.
    /// Reads settings even when no proxy URL is known, so corruption is still
    /// visible to the status screen rather than looking like "disabled".
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be parsed.
    pub fn managed_state(&self, proxy_url: Option<&str>) -> Result<ManagedSettings> {
        let settings = self.read()?;
        let Some(url) = proxy_url else {
            return Ok(ManagedSettings::NotConfigured);
        };
        Ok(if matches_managed_proxy(&settings, url) {
            ManagedSettings::Applied
        } else {
            ManagedSettings::Missing
        })
    }

    /// Cursor's file is JSONC (`json5`), and a missing file is an empty one.
    fn read(&self) -> Result<BTreeMap<String, Value>> {
        let data = match fs::read_to_string(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(error) => return Err(error.into()),
        };
        if data.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        json5::from_str(&data)
            .map_err(|error| Error::Config(format!("parse Cursor settings JSONC: {error}")))
    }

    /// Cursor may be running: replace atomically, never truncate in place.
    fn write(&self, settings: &BTreeMap<String, Value>) -> Result<()> {
        let mut data = serde_json::to_vec_pretty(settings)?;
        data.push(b'\n');
        crate::atomic::write(&self.path, &data)?;
        Ok(())
    }
}

/// Whether `settings` hold the exact managed signature for `proxy_url`.
fn matches_managed_proxy(settings: &BTreeMap<String, Value>, proxy_url: &str) -> bool {
    let loopback = loopback_host(proxy_url)
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
    loopback
        && settings.get(PROXY_KEY).and_then(Value::as_str) == Some(proxy_url)
        && settings.get(KERBEROS_KEY).and_then(Value::as_str) == Some(proxy_url)
        && settings.get(SUPPORT_KEY).and_then(Value::as_str) == Some(PROXY_SUPPORT_ON)
        && settings.get(DISABLE_HTTP2_KEY).and_then(Value::as_bool) == Some(true)
        && settings.get(SYSTEM_CERTS_KEY).and_then(Value::as_bool) == Some(true)
}

/// The host of a proxy URL such as `http://127.0.0.1:41000`, lowercased.
fn loopback_host(value: &str) -> Option<String> {
    let value = value.trim();
    // Accept a bare `host:port` too; the scheme is irrelevant to the host check.
    let candidate = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    match url::Url::parse(&candidate).ok()?.host()? {
        url::Host::Domain(domain) => Some(domain.to_ascii_lowercase()),
        url::Host::Ipv4(address) => Some(address.to_string()),
        url::Host::Ipv6(address) => Some(address.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> SettingsStore {
        SettingsStore::at(dir.path().join("settings.json"))
    }

    fn write_raw(settings: &SettingsStore, text: &str) {
        std::fs::write(&settings.path, text).unwrap();
    }

    #[test]
    fn writes_every_managed_key_and_removes_no_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);
        write_raw(
            &settings,
            r#"{ "editor.fontSize": 14, "http.noProxy": "*.cursor.sh" }"#,
        );

        settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();

        assert!(
            settings
                .managed_state(Some("http://127.0.0.1:41000"))
                .unwrap()
                .is_applied()
        );
        let written = settings.read().unwrap();
        assert_eq!(written.get("editor.fontSize"), Some(&Value::from(14)));
        assert_eq!(written.get(NO_PROXY_KEY), None);
    }

    #[test]
    fn clearing_keeps_unmanaged_keys() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);
        settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        let before = settings.read().unwrap().len();

        settings
            .clear_proxy_settings("http://127.0.0.1:41000")
            .unwrap();

        let after = settings.read().unwrap();
        assert_eq!(after.len(), before - MANAGED_KEYS.len());
        assert!(
            !settings
                .managed_state(Some("http://127.0.0.1:41000"))
                .unwrap()
                .is_applied()
        );
    }

    #[test]
    fn stale_cleanup_only_removes_a_loopback_managed_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);

        write_raw(
            &settings,
            r#"{ "http.proxy": "http://127.0.0.1:41000",
                 "http.proxyKerberosServicePrincipal": "http://127.0.0.1:41000",
                 "http.proxySupport": "on",
                 "cursor.general.disableHttp2": true,
                 "http.experimental.systemCertificatesV2": true }"#,
        );
        assert!(
            settings
                .clear_proxy_settings("http://127.0.0.1:41000")
                .unwrap()
        );
        assert!(settings.read().unwrap().is_empty());

        write_raw(
            &settings,
            r#"{ "http.proxy": "http://corp-proxy.example:3128", "http.proxySupport": "on",
                 "cursor.general.disableHttp2": true,
                 "http.experimental.systemCertificatesV2": true }"#,
        );
        assert!(
            !settings
                .clear_proxy_settings("http://127.0.0.1:41000")
                .unwrap()
        );
        assert_eq!(settings.read().unwrap().len(), 4);

        write_raw(&settings, r#"{ "http.proxy": "http://127.0.0.1:41000" }"#);
        assert!(
            !settings
                .clear_proxy_settings("http://127.0.0.1:41000")
                .unwrap()
        );
        settings
            .write_proxy_settings("http://127.0.0.1:41000")
            .unwrap();
        assert!(
            !settings
                .clear_proxy_settings("http://127.0.0.1:41001")
                .unwrap()
        );
        assert!(
            settings
                .managed_state(Some("http://127.0.0.1:41000"))
                .unwrap()
                .is_applied()
        );
    }
}
