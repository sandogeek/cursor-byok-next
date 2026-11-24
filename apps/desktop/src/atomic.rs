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
    write_inner(path, data, ModeSource::Preserve)
}

/// Replaces `path` with `data` and sets `mode` on Unix; other platforms keep
/// whatever the filesystem gives the file.
///
/// # Errors
///
/// Same as [`write`], plus a failure to apply `mode`.
pub(crate) fn write_with_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    write_inner(path, data, ModeSource::Explicit(mode))
}

/// Where the replacement's Unix mode comes from.
#[derive(Clone, Copy)]
enum ModeSource {
    /// Reuse the mode of the file being replaced; without one, the process umask
    /// decides (the same answer `fs::write` gives a new file).
    Preserve,
    /// Apply this mode, regardless of any previous file.
    Explicit(u32),
}

fn write_inner(path: &Path, data: &[u8], mode: ModeSource) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    // Every step after the temporary exists is covered by the guard, so a failed
    // write, chmod or rename cannot leave a stray temporary behind.
    let guard = TemporaryFile { path: &temporary };
    let mode = match mode {
        ModeSource::Preserve => existing_mode(path)?,
        ModeSource::Explicit(mode) => Some(mode),
    };
    write_file(&temporary, data, mode)?;
    // The rename carries the temporary's mode onto `path`, so the mode is
    // applied in exactly one place.
    let replaced = replace(&temporary, path);
    if replaced.is_ok() {
        // The temporary's path now belongs to `path`; forget the guard.
        std::mem::forget(guard);
    }
    replaced
}

/// The Unix mode of `path` if it already exists, else `None`.
#[cfg(unix)]
fn existing_mode(path: &Path) -> io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions().mode() & 0o7777)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Windows keeps ACLs, which this module deliberately does not rewrite; a
/// replacing rename keeps the destination's ACL when one already exists.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Unix path can fail, and `write_inner` asks both platforms the same `io::Result` question"
)]
fn existing_mode(_path: &Path) -> io::Result<Option<u32>> {
    Ok(None)
}

fn write_file(path: &Path, data: &[u8], mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        // `create_new` plus an initial mode means the file never exists with the
        // umask default, so a 0600 replacement cannot be read mid-write.
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options.open(path)?;
        file.write_all(data)?;
        file.sync_all()
    }
    #[cfg(not(unix))]
    {
        use std::io::Write as _;
        // Windows keeps ACLs; a new file inherits the parent directory's, which is
        // the same answer `fs::write` gives, so there is no mode to apply here.
        let _ = mode;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(data)?;
        file.sync_all()
    }
}

fn replace(temporary: &Path, path: &Path) -> io::Result<()> {
    // `fs::rename` overwrites an existing file on every supported platform,
    // including Windows (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`). Deleting
    // the destination first would leave a window where neither file exists.
    fs::rename(temporary, path)
}

/// A sibling of `path`, so the rename stays on one filesystem. The random
/// suffix keeps two writers of the same file from sharing a temporary.
fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
}

/// Removes the temporary file unless the write completed and renamed it away.
struct TemporaryFile<'a> {
    path: &'a Path,
}

impl Drop for TemporaryFile<'_> {
    fn drop(&mut self) {
        // The temporary file is disposable; a failed cleanup must not mask the
        // error the caller needs.
        if let Err(cleanup) = fs::remove_file(self.path) {
            tracing::debug!(%cleanup, "could not remove an atomic-write temporary file");
        }
    }
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

    #[cfg(unix)]
    #[test]
    fn an_unlabelled_write_preserves_the_existing_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        write_with_mode(&path, b"first", 0o600).unwrap();
        write(&path, b"second").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a rewrite must not widen 0600 to the umask");
        assert_eq!(fs::read(&path).unwrap(), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        write(&path, b"first").unwrap();
        // A directory at the temporary's target name is impossible to predict, so
        // provoke a failure by making the parent a file instead.
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        assert!(write(&blocked.join("state.json"), b"data").is_err());
        let left_over = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(left_over, 0, "a failed write must clean its temporary");
    }
}
