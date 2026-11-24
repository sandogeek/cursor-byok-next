//! App settings that belong to the desktop shell rather than Cursor takeover.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Whether the app appears in the Dock. Only macOS has a Dock; elsewhere the
/// choice is remembered but has no effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DockIcon {
    #[default]
    Visible,
    Hidden,
}

impl DockIcon {
    /// Whether the icon is shown, the value the settings toggle binds to.
    #[must_use]
    pub fn is_visible(self) -> bool {
        matches!(self, Self::Visible)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct State {
    /// macOS only: whether the app has a Dock icon.
    #[serde(default)]
    dock_icon: DockIcon,
}

/// Desktop shell settings, persisted next to the takeover state.
#[derive(Debug)]
pub struct DesktopSettings {
    path: PathBuf,
    state: State,
}

impl DesktopSettings {
    /// Loads the shell settings from the app's data directory.
    ///
    /// # Errors
    ///
    /// Fails when `~/.cursor-byok-next` cannot be resolved.
    pub fn managed() -> Result<Self> {
        Ok(Self::at(crate::storage::data_dir()?.join("desktop.json")))
    }

    /// A settings store over an explicit file, for tests.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        let state = fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default();
        Self { path, state }
    }

    /// The saved Dock choice.
    #[must_use]
    pub fn dock_icon(&self) -> DockIcon {
        self.state.dock_icon
    }

    /// Writes `dock_icon` atomically and keeps it only once the write landed.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, the state cannot be
    /// serialized, or the file cannot be written.
    pub fn save(&mut self, dock_icon: DockIcon) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let state = State { dock_icon };
        let mut data = serde_json::to_vec_pretty(&state)?;
        data.push(b'\n');
        crate::atomic::write(&self.path, &data)?;
        self.state = state;
        Ok(())
    }
}

/// Applies a Dock choice on the platform's main thread.
///
/// # Errors
///
/// Fails on macOS when `AppKit` refuses the change or the main thread is
/// unavailable; other platforms accept every choice.
pub async fn set_dock_icon(dock_icon: DockIcon) -> Result<()> {
    platform::set_dock_icon(dock_icon).await
}

#[cfg(target_os = "macos")]
mod platform {
    use nsh_core::element::run_on_main_thread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    use super::DockIcon;
    use crate::error::{Error, Result};

    pub async fn set_dock_icon(dock_icon: DockIcon) -> Result<()> {
        run_on_main_thread(move || {
            let main_thread = MainThreadMarker::new()
                .ok_or_else(|| Error::Runtime("AppKit must run on the main thread".into()))?;
            let application = NSApplication::sharedApplication(main_thread);
            let policy = match dock_icon {
                DockIcon::Visible => NSApplicationActivationPolicy::Regular,
                DockIcon::Hidden => NSApplicationActivationPolicy::Accessory,
            };
            if application.setActivationPolicy(policy) {
                Ok(())
            } else {
                Err(Error::Runtime(
                    "macOS could not change the Dock icon visibility".into(),
                ))
            }
        })
        .await
        .map_err(|error| Error::Runtime(error.to_string()))?
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::DockIcon;
    use crate::error::Result;

    pub async fn set_dock_icon(_dock_icon: DockIcon) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(dir: &tempfile::TempDir) -> DesktopSettings {
        DesktopSettings::at(dir.path().join("desktop.json"))
    }

    #[test]
    fn the_dock_icon_is_visible_by_default() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(settings(&dir).dock_icon(), DockIcon::Visible);
    }

    #[test]
    fn a_saved_dock_choice_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let mut store = DesktopSettings::at(path.clone());

        store.save(DockIcon::Hidden).unwrap();

        assert_eq!(DesktopSettings::at(path).dock_icon(), DockIcon::Hidden);
        assert_eq!(store.dock_icon(), DockIcon::Hidden);
    }

    #[test]
    fn an_unreadable_dock_setting_uses_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(&path, "not json").unwrap();

        assert_eq!(DesktopSettings::at(path).dock_icon(), DockIcon::Visible);
    }

    #[test]
    fn a_file_without_the_field_reads_as_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(&path, "{}").unwrap();

        assert_eq!(DesktopSettings::at(path).dock_icon(), DockIcon::Visible);
    }

    #[test]
    fn the_policy_reports_the_visibility_the_toggle_binds_to() {
        assert!(DockIcon::Visible.is_visible());
        assert!(!DockIcon::Hidden.is_visible());
    }
}
