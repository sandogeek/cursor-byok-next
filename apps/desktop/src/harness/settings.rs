//! Points Cursor's own HTTP stack at the local proxy.
//!
//! Cursor reads its proxy from `settings.json`; these five keys are the whole
//! contract, and `settings_match` is the only proof that a freshly written
//! proxy is actually in effect.

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde_json::Value;

use super::{Error, Result};

const NO_PROXY_KEY: &str = "http.noProxy";
const KEYS: [&str; 5] = [
    "http.proxy",
    "http.proxyKerberosServicePrincipal",
    "http.proxySupport",
    "cursor.general.disableHttp2",
    "http.experimental.systemCertificatesV2",
];

/// Cursor's user settings file, edited in place.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn managed() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| Error::Config("cannot resolve the user home directory".into()))?;
        let path = match std::env::consts::OS {
            "macos" => home.join("Library/Application Support/Cursor/User/settings.json"),
            "windows" => std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("AppData/Roaming"))
                .join("Cursor/User/settings.json"),
            "linux" => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"))
                .join("Cursor/User/settings.json"),
            platform => {
                return Err(Error::Config(format!(
                    "Cursor settings are unsupported on {platform}"
                )))
            }
        };
        Ok(Self { path })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Writes every managed key, and drops `http.noProxy` so no exclusion can
    /// bypass the proxy.
    pub fn write_proxy_settings(&self, proxy_url: &str) -> Result<()> {
        let mut settings = self.read()?;
        settings.remove(NO_PROXY_KEY);
        settings.insert(KEYS[0].into(), Value::String(proxy_url.into()));
        settings.insert(KEYS[1].into(), Value::String(proxy_url.into()));
        settings.insert(KEYS[2].into(), Value::String("on".into()));
        settings.insert(KEYS[3].into(), Value::Bool(true));
        settings.insert(KEYS[4].into(), Value::Bool(true));
        self.write(&settings)
    }

    pub fn clear_proxy_settings(&self) -> Result<()> {
        let mut settings = self.read()?;
        let before = settings.len();
        for key in KEYS {
            settings.remove(key);
        }
        if settings.len() != before {
            self.write(&settings)?;
        }
        Ok(())
    }

    /// Whether all five managed keys hold exactly what takeover writes.
    pub fn settings_match(&self, proxy_url: &str) -> Result<bool> {
        let settings = self.read()?;
        Ok(
            settings.get(KEYS[0]) == Some(&Value::String(proxy_url.into()))
                && settings.get(KEYS[1]) == Some(&Value::String(proxy_url.into()))
                && settings.get(KEYS[2]) == Some(&Value::String("on".into()))
                && settings.get(KEYS[3]) == Some(&Value::Bool(true))
                && settings.get(KEYS[4]) == Some(&Value::Bool(true)),
        )
    }

    /// Clears a proxy this app left behind when it was killed before
    /// disabling. Only a full managed signature pointing at loopback counts:
    /// a proxy configured by the user or by IT is never touched.
    pub fn clear_stale_managed_settings(&self) -> Result<bool> {
        let settings = self.read()?;
        let managed_signature = settings.get(KEYS[2]) == Some(&Value::String("on".into()))
            && settings.get(KEYS[3]) == Some(&Value::Bool(true))
            && settings.get(KEYS[4]) == Some(&Value::Bool(true));
        let loopback = settings
            .get(KEYS[0])
            .and_then(Value::as_str)
            .and_then(loopback_host)
            .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1"));
        if managed_signature && loopback {
            self.clear_proxy_settings()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Cursor's file is JSONC (`json5`), and a missing file is an empty one.
    fn read(&self) -> Result<BTreeMap<String, Value>> {
        let data = match fs::read_to_string(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new())
            }
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
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(settings)?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, [data.as_slice(), b"\n"].concat())?;
        fs::rename(temp, &self.path)?;
        Ok(())
    }
}

/// The host of a proxy URL such as `http://127.0.0.1:41000`.
fn loopback_host(value: &str) -> Option<String> {
    let authority = value.split_once("://").map_or(value, |(_, rest)| rest);
    let authority = authority.split('/').next().unwrap_or_default();
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => host,
        _ => authority,
    };
    let host = host.rsplit('@').next().unwrap_or(host).trim_matches(['[', ']']);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
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

        settings.write_proxy_settings("http://127.0.0.1:41000").unwrap();

        assert!(settings.settings_match("http://127.0.0.1:41000").unwrap());
        let written = settings.read().unwrap();
        assert_eq!(written.get("editor.fontSize"), Some(&Value::from(14)));
        assert_eq!(written.get(NO_PROXY_KEY), None);
    }

    #[test]
    fn clearing_keeps_unmanaged_keys() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);
        settings.write_proxy_settings("http://127.0.0.1:41000").unwrap();
        let before = settings.read().unwrap().len();

        settings.clear_proxy_settings().unwrap();

        let after = settings.read().unwrap();
        assert_eq!(after.len(), before - KEYS.len());
        assert!(!settings.settings_match("http://127.0.0.1:41000").unwrap());
    }

    #[test]
    fn stale_cleanup_only_removes_a_loopback_managed_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let settings = store(&dir);

        write_raw(
            &settings,
            r#"{ "http.proxy": "http://127.0.0.1:41000", "http.proxySupport": "on",
                 "cursor.general.disableHttp2": true,
                 "http.experimental.systemCertificatesV2": true }"#,
        );
        assert!(settings.clear_stale_managed_settings().unwrap());
        assert!(settings.read().unwrap().is_empty());

        write_raw(
            &settings,
            r#"{ "http.proxy": "http://corp-proxy.example:3128", "http.proxySupport": "on",
                 "cursor.general.disableHttp2": true,
                 "http.experimental.systemCertificatesV2": true }"#,
        );
        assert!(!settings.clear_stale_managed_settings().unwrap());
        assert_eq!(settings.read().unwrap().len(), 4);

        write_raw(&settings, r#"{ "http.proxy": "http://127.0.0.1:41000" }"#);
        assert!(!settings.clear_stale_managed_settings().unwrap());
        assert_eq!(settings.read().unwrap().len(), 1);
    }
}