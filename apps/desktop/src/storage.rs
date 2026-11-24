//! The app's own data directory: `~/.cursor-byok-next`.
//!
//! Every store (`harness.json`, `ca/`, `desktop.json`, `calls.db`, `plugins/`,
//! `rules/`) lives under this one path, so it is resolved here rather than in
//! any single owner. Takeover, the local loop backend, the plugin host and the
//! desktop settings all need it; none of them should have to depend on the
//! others to find it.
//!
//! Failures are [`std::io::Error`] because that is what both owners already
//! carry: an unresolvable home directory is an environment failure of the same
//! shape as an uncreatable directory.

use std::{fs, io, path::PathBuf, sync::OnceLock};

const DIRECTORY_NAME: &str = ".cursor-byok-next";

/// The app's directory, created on first use, then reused for the process
/// lifetime so every store lands on the same path.
pub(crate) fn data_dir() -> io::Result<PathBuf> {
    static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
    if let Some(dir) = DATA_DIR.get() {
        return Ok(dir.clone());
    }
    let dir = resolve()?;
    Ok(DATA_DIR.get_or_init(|| dir).clone())
}

/// A subdirectory of [`data_dir`], created on demand.
pub(crate) fn data_subdir(name: &str) -> io::Result<PathBuf> {
    let dir = data_dir()?.join(name);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn resolve() -> io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot resolve the user home directory",
        )
    })?;
    let dir = home.join(DIRECTORY_NAME);
    // The app may start twice at once: whoever wins the race creates it.
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}
