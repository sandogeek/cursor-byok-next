//! Atomic file replacement, so a crash cannot leave a half-written file.
//!
//! Cursor, the harness and the plugin store all rewrite files while another
//! process may be reading them, so every write goes through a temporary file in
//! the same directory and a rename.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Replaces `path` with `data`, keeping the file's existing permissions.
///
/// # Errors
///
/// Fails when the parent directory cannot be created, the temporary file cannot
/// be written and flushed, or the rename fails.
pub(crate) fn write(path: &Path, data: &[u8]) -> io::Result<()> {
    write_inner(path, data, None)
}

/// Replaces `path` with `data` and sets `mode` on Unix; other platforms keep
/// whatever the filesystem gives the file.
///
/// # Errors
///
/// Same as [`write`], plus a failure to apply `mode`.
pub(crate) fn write_with_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    write_inner(path, data, Some(mode))
}

fn write_inner(path: &Path, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    write_file(&temporary, data, mode)?;
    replace(&temporary, path, mode)
}

fn write_file(path: &Path, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    fs::write(path, data)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

fn replace(temporary: &Path, path: &Path, mode: Option<u32>) -> io::Result<()> {
    // Windows refuses to rename over an existing file.
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(temporary, path) {
        // The temporary file is disposable; a failed cleanup must not mask the
        // error the caller needs.
        if let Err(cleanup) = fs::remove_file(temporary) {
            tracing::debug!(%cleanup, "could not remove an atomic-write temporary file");
        }
        return Err(error);
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// A sibling of `path`, so the rename stays on one filesystem. The random
/// suffix keeps two writers of the same file from sharing a temporary.
fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_an_existing_file_and_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("state.json");
        write(&path, b"first").unwrap();
        write(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        let left_over = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name() != "state.json")
            .count();
        assert_eq!(left_over, 0, "temporary files must not survive a write");
    }

    #[cfg(unix)]
    #[test]
    fn applies_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("key.pem");
        write_with_mode(&path, b"secret", 0o600).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
