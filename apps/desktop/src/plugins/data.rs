//! Stores plugin-owned JSON with private permissions and atomic replacement.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use super::{Error, Result};

#[derive(Clone)]
pub struct PluginDataStore {
    root: PathBuf,
    locks: Arc<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl PluginDataStore {
    pub fn managed() -> Result<Self> {
        Self::new(crate::harness::data_dir()?.join("plugins/data"))
    }

    #[cfg(test)]
    pub(super) fn for_test(root: PathBuf) -> Result<Self> {
        Self::new(root)
    }

    fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        set_directory_permissions(&root)?;
        Ok(Self {
            root,
            locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn read(&self, plugin_id: &str, key: &str) -> Result<serde_json::Value> {
        let path = self.path(plugin_id, key)?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(serde_json::Value::Null)
            }
            Err(error) => Err(Error::Config(format!(
                "plugin data read failed at {}: {error}",
                path.display()
            ))),
        }
    }

    pub async fn update(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let path = self.path(plugin_id, key)?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        self.write_locked(&path, key, value).await
    }

    async fn write_locked(&self, path: &Path, key: &str, value: &serde_json::Value) -> Result<()> {
        let directory = path
            .parent()
            .expect("plugin data path has a parent")
            .to_owned();
        let temporary = directory.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
        let target = path.to_owned();
        let bytes = serde_json::to_vec_pretty(value)?;
        tokio::task::spawn_blocking(move || write_once(&directory, &temporary, &target, &bytes))
            .await
            .map_err(|error| Error::Config(format!("plugin data write task failed: {error}")))??;
        Ok(())
    }

    #[cfg(test)]
    pub async fn clear(&self, plugin_id: &str) -> Result<()> {
        validate_component(plugin_id, "plugin id")?;
        let lock = self.lock(plugin_id);
        let _guard = lock.lock().await;
        let path = self.root.join(plugin_id);
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::Config(format!(
                "plugin data cleanup failed at {}: {error}",
                path.display()
            ))),
        }
    }

    fn path(&self, plugin_id: &str, key: &str) -> Result<PathBuf> {
        validate_component(plugin_id, "plugin id")?;
        validate_component(key, "plugin data key")?;
        Ok(self.root.join(plugin_id).join(format!("{key}.json")))
    }

    fn lock(&self, plugin_id: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .entry(plugin_id.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
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
    let result = written.and_then(|()| std::fs::rename(temporary, target));
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.map_err(Into::into)
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(Error::Config(format!("invalid {label}: {value}")));
    }
    Ok(())
}

fn set_directory_permissions(path: &Path) -> Result<()> {
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
        let store = PluginDataStore::new(root.path().join("data")).unwrap();
        store
            .update(
                "com.example",
                "state",
                &serde_json::json!({"token":"secret"}),
            )
            .await
            .unwrap();
        assert_eq!(
            store.read("com.example", "state").await.unwrap()["token"],
            "secret"
        );
        store.clear("com.example").await.unwrap();
        assert!(store.read("com.example", "state").await.unwrap().is_null());
    }
}
