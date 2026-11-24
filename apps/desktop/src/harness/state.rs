//! Remembers the user's takeover intent across app restarts.
//!
//! Cursor keeps pointing at a dead proxy if this app is killed while takeover
//! is on, so the intent and the port have to outlive the process: the next
//! start can restore takeover on the same port instead of leaving Cursor broken
//! until someone toggles it.

use std::{fs, num::NonZeroU16, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::Result;

/// The port the user asked takeover to bind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortChoice {
    /// No choice was saved, so the port bound last is reused.
    #[default]
    Unset,
    /// The user asked for a fresh OS-assigned port, even if a fixed one worked.
    OsAssigned,
    /// A specific port the user typed. `0` cannot be stored here: it is the
    /// "let the OS pick" sentinel, so [`PortChoice::OsAssigned`] is the only
    /// state that means it.
    Fixed(NonZeroU16),
}

impl PortChoice {
    /// The choice `port` expresses at the TSX boundary: `null` is no choice and
    /// `0` asks the OS to pick.
    #[must_use]
    pub fn from_boundary(port: Option<u16>) -> Self {
        match port {
            None => Self::Unset,
            Some(port) => NonZeroU16::new(port).map_or(Self::OsAssigned, Self::Fixed),
        }
    }

    /// The `null | number` the TSX boundary and `harness.json` both use.
    #[must_use]
    pub fn as_boundary(self) -> Option<u16> {
        match self {
            Self::Unset => None,
            Self::OsAssigned => Some(0),
            Self::Fixed(port) => Some(port.get()),
        }
    }

    /// The port to request from the OS, given the one bound last.
    #[must_use]
    pub fn request(self, last_bound: Option<u16>, fallback: u16) -> u16 {
        match self {
            Self::Fixed(port) => port.get(),
            // An explicit "let the OS pick" overrides the remembered port.
            Self::OsAssigned => 0,
            Self::Unset => last_bound.unwrap_or(fallback),
        }
    }
}

impl Serialize for PortChoice {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.as_boundary().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PortChoice {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Ok(Self::from_boundary(Option::<u16>::deserialize(
            deserializer,
        )?))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessState {
    /// Whether the user turned takeover on.
    pub enabled: bool,
    /// The port the proxy bound last, so a restart can try it again.
    #[serde(default)]
    pub proxy_port: Option<u16>,
    /// What the user asked for, kept apart from `proxy_port` so a choice
    /// survives a run that could not bind it.
    #[serde(default)]
    pub preferred_port: PortChoice,
}

#[derive(Debug)]
pub struct HarnessStateStore {
    path: PathBuf,
}

impl HarnessStateStore {
    /// A store over an explicit file, for tests.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// A missing or unreadable file means "off": never take over on the
    /// strength of a file we cannot read.
    ///
    /// A missing file is silent, because it is the first run. Any other failure
    /// is logged with the path so a takeover that silently came back off is
    /// diagnosable.
    #[must_use]
    pub fn load(&self) -> HarnessState {
        let data = match fs::read_to_string(&self.path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return HarnessState::default();
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %self.path.display(),
                    "cannot read harness.json; takeover stays off"
                );
                return HarnessState::default();
            }
        };
        match serde_json::from_str(&data) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %self.path.display(),
                    "harness.json is not valid JSON; takeover stays off"
                );
                HarnessState::default()
            }
        }
    }

    /// Writes the intent atomically, so a crash cannot leave half a file.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, the state cannot be
    /// serialized, or the file cannot be written.
    pub fn save(&self, state: HarnessState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut data = serde_json::to_vec_pretty(&state)?;
        data.push(b'\n');
        crate::atomic::write(&self.path, &data)?;
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
            proxy_port: Some(41000),
            preferred_port: PortChoice::Fixed(NonZeroU16::new(41000).unwrap()),
        };

        store.save(state).unwrap();

        assert_eq!(store.load(), state);
    }

    #[test]
    fn a_state_without_a_choice_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        let state = HarnessState {
            enabled: false,
            proxy_port: Some(41000),
            preferred_port: PortChoice::Unset,
        };

        store.save(state).unwrap();

        assert_eq!(store.load(), state);
        let raw = std::fs::read_to_string(&store.path).unwrap();
        // An explicit null, not an omitted key: the choice is visible in the file.
        assert!(raw.contains("\"preferredPort\": null"), "{raw}");
    }

    #[test]
    fn a_file_without_a_choice_field_reads_as_no_choice() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        std::fs::write(&store.path, r#"{"enabled": true, "proxyPort": 41000}"#).unwrap();

        assert_eq!(
            store.load(),
            HarnessState {
                enabled: true,
                proxy_port: Some(41000),
                preferred_port: PortChoice::Unset,
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

    #[test]
    fn the_choice_and_the_last_bound_port_do_not_overlap() {
        let os_pick = PortChoice::from_boundary(Some(0));
        let fixed = PortChoice::from_boundary(Some(42000));
        let unset = PortChoice::from_boundary(None);

        assert_eq!(os_pick.as_boundary(), Some(0));
        assert_eq!(fixed.as_boundary(), Some(42000));
        assert_eq!(unset.as_boundary(), None);

        assert_eq!(os_pick.request(Some(41000), 39831), 0);
        assert_eq!(fixed.request(Some(41000), 39831), 42000);
        assert_eq!(unset.request(Some(41000), 39831), 41000);
        assert_eq!(unset.request(None, 39831), 39831);
    }

    #[test]
    fn every_constructible_port_choice_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = HarnessStateStore::at(dir.path().join("harness.json"));
        let choices = [
            PortChoice::Unset,
            PortChoice::OsAssigned,
            PortChoice::Fixed(NonZeroU16::new(41000).unwrap()),
        ];

        for choice in choices {
            let state = HarnessState {
                enabled: true,
                proxy_port: Some(41000),
                preferred_port: choice,
            };
            store.save(state).unwrap();
            assert_eq!(store.load().preferred_port, choice, "{choice:?}");
        }
    }

    #[test]
    fn zero_is_only_ever_the_os_assigned_choice() {
        assert_eq!(
            PortChoice::from_boundary(Some(0)),
            PortChoice::OsAssigned,
            "a boundary 0 maps to the OS pick, so `Fixed(0)` is unrepresentable"
        );
        assert_eq!(
            PortChoice::from_boundary(Some(1)),
            PortChoice::from_boundary(Some(1))
        );
        assert_ne!(PortChoice::from_boundary(Some(1)), PortChoice::OsAssigned);
    }
}
