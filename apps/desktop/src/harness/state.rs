//! Remembers the user's takeover intent across app restarts.
//!
//! Cursor keeps pointing at a dead proxy if this app is killed while takeover
//! is on, so the intent and the port have to outlive the process: the next
//! start can restore takeover on the same port instead of leaving Cursor broken
//! until someone toggles it.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::Result;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessState {
    /// Whether the user turned takeover on.
    pub enabled: bool,
    /// The port the proxy bound last, so a restart can try it again.
    pub proxy_port: u16,
    /// The port the user chose in system settings; `0` lets the OS pick.
    /// Kept apart from `proxy_port` so a chosen port survives a run that
    /// could not bind it.
    #[serde(default)]
    pub preferred_port: u16,
}

#[derive(Debug)]
pub struct HarnessStateStore {
    path: PathBuf,
}

impl HarnessStateStore {
    pub fn managed() -> Result<Self> {
        Ok(Self {
            path: super::data_dir()?.join("harness.json"),
        })
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// A missing or unreadable file means "off": never take over on the
    /// strength of a file we cannot read.
    pub fn load(&self) -> HarnessState {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, state: HarnessState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(&state)?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, [data.as_slice(), b"\n"].concat())?;
        fs::rename(temp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_reads_as_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));

        assert_eq!(store.load(), HarnessState::default());
    }

    #[test]
    fn a_saved_state_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        let state = HarnessState {
            enabled: true,
            proxy_port: 41000,
            preferred_port: 41000,
        };

        store.save(state).unwrap();

        assert_eq!(store.load(), state);
    }

    #[test]
    fn a_state_from_before_preferred_ports_reads_the_bound_port_as_no_choice() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        std::fs::write(
            &store.path,
            r#"{"enabled": true, "proxyPort": 41000}"#,
        )
        .unwrap();

        assert_eq!(
            store.load(),
            HarnessState {
                enabled: true,
                proxy_port: 41000,
                preferred_port: 0,
            }
        );
    }

    #[test]
    fn a_corrupt_file_reads_as_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        std::fs::write(&store.path, "{ not json").unwrap();

        assert_eq!(store.load(), HarnessState::default());
    }
}
