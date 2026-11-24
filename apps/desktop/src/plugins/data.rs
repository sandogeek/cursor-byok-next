//! Stores plugin-owned JSON with private permissions and atomic replacement.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use super::{Error, Result};

/// Rejects ids and keys that could escape their plugin directory.
const MAX_COMPONENT_BYTES: usize = 128;

#[derive(Clone, Debug)]
pub struct PluginDataStore {
    root: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl PluginDataStore {
    /// Opens the store rooted at `root`, creating it with private permissions.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the permissions cannot be
    /// tightened to owner-only.
    pub fn at(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        set_directory_permissions(&root)?;
        Ok(Self {
            root,
            locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Reads the JSON stored under `key`, or `None` when nothing was stored yet.
    ///
    /// # Errors
    ///
    /// Fails when the id or key is illegal, the file cannot be read, or its
    /// contents are not JSON.
    pub async fn read(&self, plugin_id: &str, key: &str) -> Result<Option<serde_json::Value>> {
        let path = self.path(plugin_id, key)?;
        let lock = self.plugin_lock(plugin_id);
        let _guard = lock.lock().await;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::context(
                format!("plugin data read failed at {}", path.display()),
                source,
            )),
        }
    }

    /// Replaces the JSON stored under `key`.
    ///
    /// # Errors
    ///
    /// Fails when the id or key is illegal, the value cannot be serialized, or
    /// the atomic replacement fails.
    pub async fn update(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let path = self.path(plugin_id, key)?;
        let lock = self.plugin_lock(plugin_id);
        let _guard = lock.lock().await;
        self.write_locked(&path, key, value).await
    }

    async fn write_locked(&self, path: &Path, key: &str, value: &serde_json::Value) -> Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| Error::Config("plugin data path has no parent".into()))?
            .to_owned();
        let temporary = directory.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
        let target = path.to_owned();
        let bytes = serde_json::to_vec_pretty(value)?;
        tokio::task::spawn_blocking(move || write_once(&directory, &temporary, &target, &bytes))
            .await
            .map_err(|error| Error::context("plugin data write task failed".to_owned(), error))??;
        Ok(())
    }

    #[cfg(test)]
    pub async fn clear(&self, plugin_id: &str) -> Result<()> {
        validate_component(plugin_id, "plugin id")?;
        let lock = self.plugin_lock(plugin_id);
        let _guard = lock.lock().await;
        let path = self.root.join(plugin_id);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(Error::context(
                format!("plugin data cleanup failed at {}", path.display()),
                source,
            )),
        }
    }

    fn path(&self, plugin_id: &str, key: &str) -> Result<PathBuf> {
        validate_component(plugin_id, "plugin id")?;
        validate_component(key, "plugin data key")?;
        Ok(self.root.join(plugin_id).join(format!("{key}.json")))
    }

    fn plugin_lock(&self, plugin_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.locks.lock();
        Arc::clone(
            locks
                .entry(plugin_id.to_owned())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

fn write_once(
    directory: &Path,
    temporary: &Path,
    target: &Path,
    bytes: &[u8],
) -> Result<(), Error> {
    use std::io::Write;
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        directories.mode(0o700);
        options.mode(0o600);
    }
    directories.create(directory)?;
    set_directory_permissions(directory)?;
    let mut file = options.open(temporary)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| std::fs::rename(temporary, target)) {
        // The temporary file is disposable; a failed cleanup must not mask the
        // write error the caller needs.
        if let Err(cleanup) = std::fs::remove_file(temporary) {
            tracing::debug!(%cleanup, "could not remove a plugin data temporary file");
        }
        return Err(error.into());
    }
    Ok(())
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_COMPONENT_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::Config(format!("invalid {label}: {value}")));
    }
    Ok(())
}

#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "the caller treats a permission failure the same on every platform, but only Unix has a way to fail here"
    )
)]
fn set_directory_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_reads_and_removes_json() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::at(root.path().join("data")).unwrap();
        store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"secret"}),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .read("com.example", "state")
                .await
                .unwrap()
                .expect("state was written")["token"],
            "secret"
        );
        store.clear("com.example").await.unwrap();
        assert!(store.read("com.example", "state").await.unwrap().is_none());
    }
}
