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

/// Whether the app opens its main window on launch or starts hidden in the
/// system tray.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LaunchMode {
    #[default]
    Normal,
    Silent,
}

impl LaunchMode {
    /// Whether the app starts hidden in the tray, the value the settings toggle
    /// binds to.
    #[must_use]
    pub fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }
}

/// Whether the app currently has an open window (visible on screen or
/// minimized to the Dock).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PropEnum)]
pub enum WindowPresence {
    /// At least one window is open (or minimized in the Dock).
    #[default]
    Open,
    /// Every window has been closed (hidden to the menu bar tray).
    Closed,
}

impl WindowPresence {
    /// Initial window presence at launch: silent launch keeps every window
    /// hidden in the tray, while normal launch shows the main window.
    #[must_use]
    pub fn at_launch(launch: LaunchMode) -> Self {
        match launch {
            LaunchMode::Normal => Self::Open,
            LaunchMode::Silent => Self::Closed,
        }
    }

    /// Resolves the macOS Dock icon policy from the user's saved preference
    /// and whether a window is currently open.
    ///
    /// Closing the last window hides the app into the menu bar tray, so the
    /// Dock icon hides even when the preference is [`DockIcon::Visible`];
    /// reopening a window restores it when the preference allows.
    #[must_use]
    pub fn dock_icon(self, preference: DockIcon) -> DockIcon {
        match self {
            Self::Open => preference,
            Self::Closed => DockIcon::Hidden,
        }
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

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct State {
    /// macOS only: whether the app has a Dock icon.
    #[serde(default)]
    dock_icon: DockIcon,
    /// Whether the app starts hidden in the system tray without showing the
    /// main window.
    #[serde(default)]
    silent_start: bool,
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

    /// Whether the app starts hidden in the tray without showing the main
    /// window.
    #[must_use]
    pub fn launch_mode(&self) -> LaunchMode {
        if self.state.silent_start {
            LaunchMode::Silent
        } else {
            LaunchMode::Normal
        }
    }

    /// Writes `dock_icon` atomically while preserving the remaining settings,
    /// keeping the new value only once the write landed.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, the state cannot be
    /// serialized, or the file cannot be written.
    pub fn set_dock_icon(&mut self, dock_icon: DockIcon) -> Result<()> {
        let mut next = self.state;
        next.dock_icon = dock_icon;
        self.save_state(next)
    }

    /// Writes `launch_mode` atomically while preserving the remaining
    /// settings, keeping the new value only once the write landed.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created, the state cannot be
    /// serialized, or the file cannot be written.
    pub fn set_launch_mode(&mut self, launch_mode: LaunchMode) -> Result<()> {
        let mut next = self.state;
        next.silent_start = launch_mode.is_silent();
        self.save_state(next)
    }

    fn save_state(&mut self, state: State) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
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

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the macOS path can fail, and the exported function has one signature on every platform"
    )]
    pub(super) fn set_dock_icon(_dock_icon: DockIcon) -> Result<()> {
        Ok(())
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the macOS path can fail, and the exported function has one signature on every platform"
    )]
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
    fn the_dock_icon_is_visible_and_silent_start_is_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = settings(&dir);

        assert_eq!(store.dock_icon(), DockIcon::Visible);
        assert_eq!(store.launch_mode(), LaunchMode::Normal);
    }

    #[test]
    fn saving_one_desktop_setting_preserves_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let mut store = DesktopSettings::at(path.clone());

        store.set_dock_icon(DockIcon::Hidden).unwrap();
        store.set_launch_mode(LaunchMode::Silent).unwrap();

        let reloaded = DesktopSettings::at(path.clone());
        assert_eq!(reloaded.dock_icon(), DockIcon::Hidden);
        assert_eq!(reloaded.launch_mode(), LaunchMode::Silent);
        assert_eq!(store.dock_icon(), DockIcon::Hidden);
        assert_eq!(store.launch_mode(), LaunchMode::Silent);

        store.set_dock_icon(DockIcon::Visible).unwrap();
        let reloaded = DesktopSettings::at(path);
        assert_eq!(reloaded.dock_icon(), DockIcon::Visible);
        assert_eq!(reloaded.launch_mode(), LaunchMode::Silent);
    }

    #[test]
    fn an_unreadable_desktop_setting_uses_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(&path, "not json").unwrap();
        let store = DesktopSettings::at(path);

        assert_eq!(store.dock_icon(), DockIcon::Visible);
        assert_eq!(store.launch_mode(), LaunchMode::Normal);
    }

    #[test]
    fn a_file_without_fields_reads_as_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        fs::write(&path, "{}").unwrap();
        let store = DesktopSettings::at(path);

        assert_eq!(store.dock_icon(), DockIcon::Visible);
        assert_eq!(store.launch_mode(), LaunchMode::Normal);
    }

    #[test]
    fn initial_window_presence_matches_launch_mode() {
        assert_eq!(
            WindowPresence::at_launch(LaunchMode::Normal),
            WindowPresence::Open
        );
        assert_eq!(
            WindowPresence::at_launch(LaunchMode::Silent),
            WindowPresence::Closed
        );
    }

    #[test]
    fn the_policy_reports_the_visibility_the_toggle_binds_to() {
        assert!(DockIcon::Visible.is_visible());
        assert!(!DockIcon::Hidden.is_visible());
    }

    #[test]
    fn closing_the_last_window_hides_the_dock_icon_without_losing_the_preference() {
        assert_eq!(
            WindowPresence::Open.dock_icon(DockIcon::Visible),
            DockIcon::Visible
        );
        assert_eq!(
            WindowPresence::Open.dock_icon(DockIcon::Hidden),
            DockIcon::Hidden
        );
        assert_eq!(
            WindowPresence::Closed.dock_icon(DockIcon::Visible),
            DockIcon::Hidden
        );
        assert_eq!(
            WindowPresence::Closed.dock_icon(DockIcon::Hidden),
            DockIcon::Hidden
        );
    }
}
