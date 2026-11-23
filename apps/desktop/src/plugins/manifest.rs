//! Defines and validates the static filesystem plugin manifest.

use std::{collections::HashSet, path::Path};

use serde::Deserialize;

use super::{Error, Result};

pub const PLUGIN_API_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginManifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub min_app_version: Option<String>,
    pub icon: String,
    pub entry: String,
    #[serde(default)]
    pub permissions: PluginPermissions,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginPermissions {
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub resource_origin: bool,
}

impl PluginManifest {
    pub fn validate(&self, directory: &Path) -> Result<()> {
        if self.api_version != PLUGIN_API_VERSION {
            return Err(Error::Config(format!(
                "plugin '{}' uses unsupported API version {}",
                self.id, self.api_version
            )));
        }
        validate_id(&self.id, "plugin id")?;
        required(&self.name, "plugin name")?;
        Version::parse(&self.version)
            .ok_or_else(|| Error::Config(format!("invalid plugin version: {}", self.version)))?;
        if let Some(minimum) = &self.min_app_version {
            Version::parse(minimum)
                .ok_or_else(|| Error::Config(format!("invalid plugin minAppVersion: {minimum}")))?;
        }
        validate_entry_path(directory, &self.entry)?;
        validate_asset_path(directory, &self.icon)?;
        let mut hosts = HashSet::new();
        for host in &self.permissions.network {
            validate_network_host(host)?;
            if !hosts.insert(host.to_ascii_lowercase()) {
                return Err(Error::Config(format!(
                    "plugin '{}' contains duplicate network host '{host}'",
                    self.id
                )));
            }
        }
        Ok(())
    }
}

/// A `major.minor.patch` plugin version, ignoring pre-release and build tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Parses `major.minor.patch`, ignoring anything after `-` or `+`.
    pub(super) fn parse(value: &str) -> Option<Self> {
        let core = value.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let version = Self {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next()?.parse().ok()?,
        };
        parts.next().is_none().then_some(version)
    }
}

pub(super) fn version_at_least(actual: &str, minimum: &str) -> bool {
    match (Version::parse(actual), Version::parse(minimum)) {
        (Some(actual), Some(minimum)) => actual >= minimum,
        _ => false,
    }
}

/// Ids are `[a-z0-9]+` runs joined by `.`, `_` or `-`, e.g. `dev.example.app`.
pub(super) fn validate_id(value: &str, label: &str) -> Result<()> {
    if is_dotted_id(value) {
        Ok(())
    } else {
        Err(Error::Config(format!("invalid {label}: {value}")))
    }
}

fn is_dotted_id(value: &str) -> bool {
    !value.is_empty()
        && value.split(['.', '_', '-']).all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_network_host(value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('/')
        || value.contains(':')
        || value.starts_with('.')
        || value.ends_with('.')
    {
        return Err(Error::Config(format!(
            "invalid plugin network host: {value}"
        )));
    }
    let parsed = url::Url::parse(&format!("https://{value}")).map_err(|error| {
        Error::Config(format!("invalid plugin network host '{value}': {error}"))
    })?;
    if parsed.host_str() != Some(value) {
        return Err(Error::Config(format!(
            "invalid plugin network host: {value}"
        )));
    }
    Ok(())
}

fn required<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() {
        Err(Error::Config(format!("{label} is required")))
    } else {
        Ok(value)
    }
}

fn validate_entry_path(directory: &Path, value: &str) -> Result<()> {
    let path = Path::new(value);
    if !is_safe_relative_path(path) {
        return Err(Error::Config(format!("invalid plugin entry path: {value}")));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "js" | "mjs" | "ts" | "mts") {
        return Err(Error::Config(format!(
            "unsupported plugin entry format: {value}"
        )));
    }
    let entry = directory.join(path);
    if !entry.is_file() {
        return Err(Error::Config(format!(
            "plugin entry does not exist: {value}"
        )));
    }
    let root = directory.canonicalize()?;
    let entry = entry.canonicalize()?;
    if !entry.starts_with(&root) {
        return Err(Error::Config(format!(
            "plugin entry escapes its directory: {value}"
        )));
    }
    Ok(())
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn validate_asset_path(directory: &Path, value: &str) -> Result<()> {
    let path = Path::new(value);
    if !is_safe_relative_path(path) {
        return Err(Error::Config(format!("invalid plugin asset path: {value}")));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "svg" | "png" | "webp") {
        return Err(Error::Config(format!(
            "unsupported plugin icon format: {value}"
        )));
    }
    if !directory.join(path).is_file() {
        return Err(Error::Config(format!(
            "plugin icon does not exist: {value}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_urls_in_network_host_allowlist() {
        assert!(validate_network_host("https://example.com").is_err());
        assert!(validate_network_host("example.com:443").is_err());
        assert!(validate_network_host("example.com").is_ok());
    }

    #[test]
    fn resource_origin_defaults_to_false() {
        let permissions: PluginPermissions = serde_json::from_value(serde_json::json!({
            "network": ["example.com"]
        }))
        .unwrap();
        assert!(!permissions.resource_origin);
    }
}
