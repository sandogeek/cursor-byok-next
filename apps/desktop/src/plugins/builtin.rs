//! Pre-installs bundled built-in plugins into the user's installed directory.

use std::path::Path;

use super::{definition::write_if_changed, Result};

const MAGPIE: &[(&str, &str)] = &[
    (
        "plugin.json",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/plugin.json"
        )),
    ),
    (
        "main.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/main.ts"
        )),
    ),
    (
        "provider.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/provider.ts"
        )),
    ),
    (
        "catalog.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/catalog.ts"
        )),
    ),
    (
        "resources.ts",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/resources.ts"
        )),
    ),
    (
        "assets/magpie.svg",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/build-in/magpie/assets/magpie.svg"
        )),
    ),
];

const PLUGINS: &[(&str, &[(&str, &str)])] = &[("magpie", MAGPIE)];

pub(super) fn install(installed: &Path) -> Result<()> {
    for (name, files) in PLUGINS {
        let directory = installed.join(name);
        if disk_version(&directory) == Some(embedded_version(files)?) {
            continue;
        }
        write_plugin(&directory, files)?;
    }
    Ok(())
}

fn embedded_version(files: &[(&str, &str)]) -> Result<String> {
    let manifest = files
        .iter()
        .find(|(name, _)| *name == "plugin.json")
        .map(|(_, content)| *content)
        .expect("built-in plugin bundles plugin.json");
    let value: serde_json::Value = serde_json::from_str(manifest)?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| super::Error::Config("built-in plugin manifest requires version".into()))
}

fn disk_version(directory: &Path) -> Option<String> {
    let manifest = std::fs::read_to_string(directory.join("plugin.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    Some(value.get("version")?.as_str()?.to_owned())
}

fn write_plugin(directory: &Path, files: &[(&str, &str)]) -> Result<()> {
    for (relative, content) in files {
        let path = directory.join(relative);
        let parent = path.parent().expect("plugin file path has a parent");
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        write_if_changed(&path, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    prune_unknown_files(directory, directory, files)?;
    Ok(())
}

fn prune_unknown_files(root: &Path, directory: &Path, files: &[(&str, &str)]) -> Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            prune_unknown_files(root, &path, files)?;
            if std::fs::read_dir(&path)?.next().is_none() {
                std::fs::remove_dir(&path)?;
            }
            continue;
        }
        let known = files
            .iter()
            .any(|(relative, _)| root.join(relative) == path);
        if !known {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_version_gated() {
        let root = tempfile::tempdir().unwrap();
        install(root.path()).unwrap();
        assert!(root.path().join("magpie/plugin.json").is_file());
        assert!(root.path().join("magpie/assets/magpie.svg").is_file());
        std::fs::write(root.path().join("magpie/main.ts"), "edited").unwrap();
        install(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("magpie/main.ts")).unwrap(),
            "edited"
        );
    }
}
