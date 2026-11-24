//! Stores plugin-owned JSON with private permissions and atomic replacement.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::{Error, Result};

/// Rejects ids and keys that could escape their plugin directory.
const MAX_COMPONENT_BYTES: usize = 128;

#[derive(Clone, Debug)]
pub struct PluginDataStore {
    root: PathBuf,
    locks: Arc<Mutex<HashMap<LockKey, Arc<AsyncMutex<()>>>>>,
}

/// One lock per stored key, so unrelated keys of the same plugin are independent
/// while a read-modify-write of one key is serialized as a unit.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LockKey {
    plugin_id: String,
    key: String,
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
        let lock = self.key_lock(plugin_id, key);
        let _guard = lock.lock().await;
        read_json(&path).await
    }

    /// Replaces the JSON stored under `key`.
    ///
    /// # Errors
    ///
    /// Fails when the id or key is illegal, the value cannot be serialized, or
    /// the atomic replacement fails.
    #[cfg(test)]
    pub async fn update(
        &self,
        plugin_id: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<()> {
        let value = value.clone();
        self.mutate(plugin_id, key, move |_existing| Ok((value, ())))
            .await
    }

    /// Reads, changes and writes one key as a single critical section.
    ///
    /// `change` receives the current value (or `None`) and returns the value to
    /// store plus whatever the caller needs. The write runs on the blocking pool
    /// while owning the key's lock, so a dropped caller future cannot release the
    /// lock before the write completes: a later value always lands after an
    /// earlier one.
    ///
    /// # Errors
    ///
    /// Fails when the id or key is illegal, the stored value is not decodable,
    /// `change` fails, or the atomic replacement fails.
    pub async fn mutate<T, F>(&self, plugin_id: &str, key: &str, change: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Option<serde_json::Value>) -> Result<(serde_json::Value, T)> + Send + 'static,
    {
        let path = self.path(plugin_id, key)?;
        let directory = path
            .parent()
            .ok_or_else(|| Error::Config("plugin data path has no parent".into()))?
            .to_owned();
        let temporary = directory.join(format!(".{key}.{}.tmp", uuid::Uuid::new_v4()));
        let lock = self.key_lock(plugin_id, key);
        let guard = lock.lock_owned().await;
        let existing = read_json(&path).await?;
        let (value, output) = change(existing)?;
        let bytes = serde_json::to_vec_pretty(&value)?;
        // The owned guard moves into the blocking task, so the serialization
        // outlives a cancelled caller future instead of ending at its drop.
        tokio::task::spawn_blocking(move || {
            let _guard: OwnedMutexGuard<()> = guard;
            write_once(&directory, &temporary, &path, &bytes)
        })
        .await
        .map_err(|error| Error::context("plugin data write task failed".to_owned(), error))??;
        Ok(output)
    }

    #[cfg(test)]
    pub async fn clear(&self, plugin_id: &str) -> Result<()> {
        validate_component(plugin_id, "plugin id")?;
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

    fn key_lock(&self, plugin_id: &str, key: &str) -> Arc<AsyncMutex<()>> {
        let lock_key = LockKey {
            plugin_id: plugin_id.to_owned(),
            key: key.to_owned(),
        };
        let mut locks = self.locks.lock();
        Arc::clone(
            locks
                .entry(lock_key)
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

async fn read_json(path: &Path) -> Result<Option<serde_json::Value>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::context(
            format!("plugin data read failed at {}", path.display()),
            source,
        )),
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

/// Whether `value` is one safe path component: non-empty, bounded, and made only
/// of safe characters.
fn is_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_COMPONENT_BYTES
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_component(value: &str, label: &str) -> Result<()> {
    if !is_component(value) {
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

    #[test]
    fn rejects_relative_path_components() {
        assert!(!is_component("."));
        assert!(!is_component(".."));
        assert!(!is_component(""));
        assert!(!is_component("a/b"));
        assert!(!is_component("C:evil"));
        assert!(!is_component(&"x".repeat(MAX_COMPONENT_BYTES + 1)));
        assert!(is_component("resources-account"));
        assert!(is_component("state.v2"));
    }

    #[tokio::test]
    async fn a_write_lands_after_a_caller_that_stopped_waiting() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::at(root.path().join("data")).unwrap();
        // `timeout` drops the update future as soon as the deadline passes; on
        // first poll the blocking write is already queued and holds the key lock.
        let first = tokio::time::timeout(
            std::time::Duration::from_nanos(1),
            store.update("com.example", "state", &serde_json::json!({ "n": 1 })),
        )
        .await;
        assert!(first.is_err(), "the 1ns timeout should cancel the caller");
        store
            .update("com.example", "state", &serde_json::json!({ "n": 2 }))
            .await
            .unwrap();
        assert_eq!(
            store.read("com.example", "state").await.unwrap().unwrap()["n"],
            2,
            "the cancelled write must not land after the newer one"
        );
    }

    #[tokio::test]
    async fn mutate_sees_the_current_value_and_returns_its_output() {
        let root = tempfile::tempdir().unwrap();
        let store = PluginDataStore::at(root.path().join("data")).unwrap();
        let first = store
            .mutate("com.example", "counter", |existing| {
                assert!(existing.is_none());
                Ok((serde_json::json!({ "n": 1 }), "first"))
            })
            .await
            .unwrap();
        assert_eq!(first, "first");
        store
            .mutate("com.example", "counter", |existing| {
                let current = existing
                    .as_ref()
                    .and_then(|value| value.get("n"))
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                Ok((serde_json::json!({ "n": current + 1 }), ()))
            })
            .await
            .unwrap();
        assert_eq!(
            store.read("com.example", "counter").await.unwrap().unwrap()["n"],
            2
        );
    }
}
