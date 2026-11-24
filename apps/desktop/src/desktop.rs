//! App settings that belong to the desktop shell rather than Cursor takeover.

use std::{fs, path::PathBuf};

use nsh_core::element::PropEnum;
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

/// The appearance the app shows in, the same choice `setColorScheme` applies
/// to the screens.
///
/// NSH paints the app's own views in it; the title bar macOS draws above
/// them is `AppKit`'s and follows the window's appearance, which NSH does not
/// set. The desktop shell sets that here as well, so a forced dark app has a
/// dark title bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PropEnum)]
pub enum AppAppearance {
    /// Follow the system (default).
    #[default]
    System,
    Light,
    Dark,
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
    ///
    /// A missing or unparsable file falls back to the defaults: shell appearance
    /// is cosmetic, so a corrupt `desktop.json` must never block the app from
    /// opening. The next [`save`](Self::save) replaces the file whole.
    ///
    /// Only a missing file is silent; any other failure is logged with the path
    /// and the reason, so a reset appearance is diagnosable.
    #[must_use]
    pub fn at(path: PathBuf) -> Self {
        let state = match fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<State>(&data) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        path = %path.display(),
                        "desktop.json is not valid JSON; using the default appearance"
                    );
                    State::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "cannot read desktop.json; using the default appearance"
                );
                State::default()
            }
        };
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
#[cfg(target_os = "macos")]
pub async fn set_dock_icon(dock_icon: DockIcon) -> Result<()> {
    platform::set_dock_icon(dock_icon).await
}

/// Applies a Dock choice. Every platform without `AppKit` accepts all of them,
/// so this only exists for the one async signature the TSX side calls.
///
/// # Errors
///
/// Never fails on these platforms.
#[cfg(not(target_os = "macos"))]
#[expect(
    clippy::unused_async,
    reason = "the TSX binding has one signature on every platform; only macOS needs the main-thread round trip"
)]
pub async fn set_dock_icon(dock_icon: DockIcon) -> Result<()> {
    platform::set_dock_icon(dock_icon)
}

/// Applies the app's appearance to the native window chrome.
///
/// The screens take `setColorScheme` from the TSX side; this covers what NSH
/// does not: the title bar the platform draws, which follows its own
/// appearance rather than the views'. Other platforms accept every choice:
/// Windows paints its caption buttons from the app's scheme already, and
/// GTK's header follows the environment.
///
/// # Errors
///
/// Fails on macOS when the main thread is unavailable. Changing the
/// appearance itself cannot fail.
#[cfg(target_os = "macos")]
pub async fn set_app_appearance(appearance: AppAppearance) -> Result<()> {
    platform::set_app_appearance(appearance).await
}

/// Applies the app's appearance to the native window chrome. The platform
/// already follows the app's scheme, so this only exists for the async
/// signature the TSX side calls.
///
/// # Errors
///
/// Never fails on these platforms.
#[cfg(not(target_os = "macos"))]
#[expect(
    clippy::unused_async,
    reason = "the TSX binding has one signature on every platform; only macOS needs the main-thread round trip"
)]
pub async fn set_app_appearance(appearance: AppAppearance) -> Result<()> {
    platform::set_app_appearance(appearance)
}

#[cfg(target_os = "macos")]
mod platform {
    use nsh_core::element::run_on_main_thread;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
        NSApplicationActivationPolicy,
    };

    use super::{AppAppearance, DockIcon};
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

    pub async fn set_app_appearance(appearance: AppAppearance) -> Result<()> {
        run_on_main_thread(move || {
            let main_thread = MainThreadMarker::new()
                .ok_or_else(|| Error::Runtime("AppKit must run on the main thread".into()))?;
            let name = match appearance {
                AppAppearance::System => None,
                // SAFETY: the appearance names are AppKit's immutable
                // constants.
                AppAppearance::Light => Some(unsafe { NSAppearanceNameAqua }),
                // SAFETY: as above.
                AppAppearance::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
            };
            let appearance = name.and_then(NSAppearance::appearanceNamed);
            // The application's appearance is what its windows inherit: a
            // window without one of its own, the main window among them,
            // draws its title bar in it.
            NSApplication::sharedApplication(main_thread).setAppearance(appearance.as_deref());
            Ok(())
        })
        .await
        .map_err(|error| Error::Runtime(error.to_string()))?
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{AppAppearance, DockIcon};
    use crate::error::Result;

    pub(super) fn set_dock_icon(_dock_icon: DockIcon) -> Result<()> {
        Ok(())
    }

    pub(super) fn set_app_appearance(_appearance: AppAppearance) -> Result<()> {
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
