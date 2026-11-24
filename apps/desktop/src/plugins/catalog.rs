//! Loads only the app's embedded Magpie plugin.

use std::{
    fs,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};

use super::{
    Error, Result,
    definition::PluginDefinitionLoader,
    descriptor::{CatalogMode, PluginModuleDefinition, ValidatedProvider},
    manifest::{PluginManifest, validate_id},
};

const MANIFEST_FILE_NAME: &str = "plugin.json";
const MAX_ICON_BYTES: u64 = 1024 * 1024;
/// The running app's version: the same string `nsh.json` publishes, because the
/// crate version tracks the release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct PluginCatalog {
    directory: PathBuf,
    definition_loader: PluginDefinitionLoader,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginEntry {
    pub entry: PathBuf,
    pub manifest: PluginManifest,
    /// The declaration as the plugin produced it; resources are read from here.
    pub definition: PluginModuleDefinition,
    /// The same providers, with their model source resolved. The host reads
    /// these instead of the raw `hasModels` / `catalog` / `resourceType` trio.
    pub providers: Vec<ValidatedProvider>,
    pub icon: String,
}

impl PluginCatalog {
    /// The app's own plugin catalog: a private workspace under the temp dir
    /// that holds the embedded Magpie.
    ///
    /// # Errors
    ///
    /// Fails when the workspace or its SDK files cannot be created.
    pub fn managed() -> Result<Self> {
        Self::in_temporary_workspace(&std::env::temp_dir())
    }

    #[cfg(test)]
    pub(super) fn for_test(root: &Path) -> Result<Self> {
        Self::in_temporary_workspace(root)
    }

    fn in_temporary_workspace(parent: &Path) -> Result<Self> {
        let definition_loader = PluginDefinitionLoader::in_temporary_workspace(parent)?;
        let directory = super::builtin::materialize_magpie(definition_loader.workspace())?;
        Ok(Self {
            directory,
            definition_loader,
        })
    }

    pub(crate) fn loader(&self) -> &PluginDefinitionLoader {
        &self.definition_loader
    }

    /// Loads the bundled plugin's entry module. A load failure is logged and
    /// reported as an empty list, because the UI shows manifests instead.
    pub(crate) async fn entries(&self, executable: &Path) -> Vec<PluginEntry> {
        match load_plugin(self.directory.clone(), &self.definition_loader, executable).await {
            Ok(entry) => vec![entry],
            Err(error) => {
                tracing::warn!(%error, "failed to load bundled Magpie");
                Vec::new()
            }
        }
    }

    /// Reads the embedded manifest and icon without a worker, for the UI's
    /// state before Bun is available. Both reads are filesystem work, so they
    /// run off the async runtime.
    pub(crate) async fn manifests(&self) -> Vec<LoadedManifest> {
        let directory = self.directory.clone();
        let loaded = tokio::task::spawn_blocking(move || load_manifest(&directory))
            .await
            .map_err(|error| Error::context("plugin manifest task failed".to_owned(), error))
            .and_then(std::convert::identity);
        match loaded {
            Ok(manifest) => vec![manifest],
            Err(error) => {
                tracing::warn!(%error, "failed to load the bundled Magpie manifest");
                Vec::new()
            }
        }
    }
}

/// A manifest that passed validation, plus the icon the UI shows before a
/// worker is ready.
#[derive(Clone, Debug)]
pub(crate) struct LoadedManifest {
    pub manifest: PluginManifest,
    /// `data:` URL of the manifest's icon.
    pub icon: String,
}

fn load_manifest(directory: &Path) -> Result<LoadedManifest> {
    let manifest: PluginManifest =
        serde_json::from_slice(&fs::read(directory.join(MANIFEST_FILE_NAME))?)?;
    manifest.validate(directory)?;
    require_app_version(&manifest, APP_VERSION)?;
    let icon = icon_data_url(directory, &manifest.icon)?;
    Ok(LoadedManifest { manifest, icon })
}

fn require_app_version(manifest: &PluginManifest, app_version: &str) -> Result<()> {
    let Some(minimum) = &manifest.min_app_version else {
        return Ok(());
    };
    if super::manifest::version_at_least(app_version, minimum) {
        return Ok(());
    }
    Err(Error::Config(format!(
        "plugin '{}' requires app version {minimum} or newer (current {app_version})",
        manifest.id
    )))
}

async fn load_plugin(
    directory: PathBuf,
    loader: &PluginDefinitionLoader,
    executable: &Path,
) -> Result<PluginEntry> {
    // Reading the manifest and the icon touches the filesystem; keep it off the
    // async runtime.
    let manifest_directory = directory.clone();
    let LoadedManifest { manifest, icon } =
        tokio::task::spawn_blocking(move || load_manifest(&manifest_directory))
            .await
            .map_err(|error| Error::context("plugin manifest task failed".to_owned(), error))??;
    let entry = directory.join(&manifest.entry).canonicalize()?;
    let definition = loader.load(executable, &entry).await?;
    let providers = validate_definition(&manifest.id, &definition)?;
    Ok(PluginEntry {
        entry,
        manifest,
        definition,
        providers,
        icon,
    })
}

/// Accepts a bare string or a locale map. `label` only runs on failure, so
/// the success path allocates nothing for diagnostics.
fn validate_localized_text(
    value: &serde_json::Value,
    label: impl FnOnce() -> String,
) -> Result<()> {
    match value {
        serde_json::Value::String(text) if !text.trim().is_empty() => Ok(()),
        serde_json::Value::Object(map)
            if !map.is_empty()
                && map
                    .values()
                    .all(|entry| entry.as_str().is_some_and(|text| !text.trim().is_empty())) =>
        {
            Ok(())
        }
        _ => Err(Error::Config(format!(
            "{} must be a non-empty string or a locale map of non-empty strings",
            label()
        ))),
    }
}

pub(super) fn validate_definition(
    plugin_id: &str,
    definition: &PluginModuleDefinition,
) -> Result<Vec<ValidatedProvider>> {
    if definition.providers.is_empty() {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' must define at least one provider"
        )));
    }
    let mut provider_ids = std::collections::HashSet::new();
    let mut validated = Vec::with_capacity(definition.providers.len());
    for provider in &definition.providers {
        validate_id(&provider.id, "plugin provider id")?;
        validate_localized_text(&provider.display_name, || {
            format!(
                "plugin '{plugin_id}' provider '{}' displayName",
                provider.id
            )
        })?;
        if provider.provider_type.trim().is_empty() {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' provider '{}' requires providerType",
                provider.id
            )));
        }
        if provider.catalog == CatalogMode::Live && !provider.has_models {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' provider '{}' live catalog requires models",
                provider.id
            )));
        }
        if provider.catalog == CatalogMode::Live && provider.resource_type.is_none() {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' provider '{}' live catalog requires a resource type",
                provider.id
            )));
        }
        if !provider_ids.insert(provider.id.clone()) {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' contains duplicate provider '{}'",
                provider.id
            )));
        }
        if let Some(resource_type) = &provider.resource_type
            && !definition
                .resources
                .iter()
                .any(|resource| &resource.resource_type == resource_type)
        {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' provider '{}' consumes undeclared resource '{resource_type}'",
                provider.id
            )));
        }
        // The checks above are the ones a user can act on; this one re-states
        // them as a type, so the host cannot construct an invalid provider.
        validated.push(
            provider
                .clone()
                .validated()
                .map_err(|error| Error::Config(format!("plugin '{plugin_id}' {error}")))?,
        );
    }
    let mut resource_types = std::collections::HashSet::new();
    for resource in &definition.resources {
        validate_id(&resource.resource_type, "plugin resource type")?;
        validate_localized_text(&resource.display_name, || {
            format!(
                "plugin '{plugin_id}' resource '{}' displayName",
                resource.resource_type
            )
        })?;
        if !resource_types.insert(resource.resource_type.clone()) {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' contains duplicate resource type '{}'",
                resource.resource_type
            )));
        }
        for method in &resource.add {
            validate_id(&method.id, "plugin add method id")?;
            validate_form_fields(plugin_id, &method.id, &method.fields)?;
        }
    }
    Ok(validated)
}

fn validate_form_fields(
    plugin_id: &str,
    method_id: &str,
    fields: &[super::descriptor::FormFieldDefinition],
) -> Result<()> {
    if fields.is_empty() {
        return Err(Error::Config(format!(
            "plugin '{plugin_id}' form method '{method_id}' must declare at least one field"
        )));
    }
    let mut field_ids = std::collections::HashSet::new();
    for field in fields {
        validate_id(&field.id, "plugin form field id")?;
        if !field_ids.insert(field.id.clone()) {
            return Err(Error::Config(format!(
                "plugin '{plugin_id}' form method '{method_id}' contains duplicate field '{}'",
                field.id
            )));
        }
        validate_localized_text(&field.label, || {
            format!(
                "plugin '{plugin_id}' form method '{method_id}' field '{}' label",
                field.id
            )
        })?;
    }
    Ok(())
}

fn icon_data_url(directory: &Path, relative: &str) -> Result<String> {
    let root = directory.canonicalize()?;
    let path = directory.join(relative).canonicalize()?;
    if !path.starts_with(&root) {
        return Err(Error::Config(format!(
            "plugin icon escapes its directory: {relative}"
        )));
    }
    if fs::metadata(&path)?.len() > MAX_ICON_BYTES {
        return Err(Error::Config(format!(
            "plugin icon exceeds {MAX_ICON_BYTES} bytes: {relative}"
        )));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = match extension.as_str() {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => {
            return Err(Error::Config(format!(
                "unsupported plugin icon: {relative}"
            )));
        }
    };
    Ok(format!(
        "data:{mime};base64,{}",
        STANDARD.encode(fs::read(path)?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::descriptor::{
        AddMethodDefinition, FormFieldDefinition, ProviderDefinition, ResourceDefinition,
    };
    use crate::plugins::descriptor::{AddMethodKind, FormFieldKind};

    #[tokio::test]
    async fn embedded_magpie_ignores_installed_manifests_and_source() {
        let root = tempfile::tempdir().unwrap();
        let installed = root.path().join("plugins/installed");
        fs::create_dir_all(&installed).unwrap();
        let shadow = super::super::builtin::materialize_magpie(&installed).unwrap();
        fs::write(shadow.join("main.ts"), "throw new Error('untrusted')").unwrap();
        let arbitrary = installed.join("arbitrary");
        fs::rename(&shadow, &arbitrary).unwrap();
        let impostor = super::super::builtin::materialize_magpie(&installed).unwrap();
        fs::write(impostor.join("main.ts"), "throw new Error('same version')").unwrap();
        let manifest_path = arbitrary.join("plugin.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["id"] = serde_json::json!("dev.example.arbitrary");
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let catalog = PluginCatalog::for_test(root.path()).unwrap();
        let manifests = catalog.manifests().await;
        assert_eq!(manifests.len(), 1);
        let LoadedManifest {
            manifest: magpie,
            icon,
        } = &manifests[0];
        assert_eq!(magpie.id, super::super::builtin::MAGPIE_PLUGIN_ID);
        assert!(magpie.permissions.resource_origin);
        assert_eq!(magpie.permissions.network, Vec::<String>::new());
        assert!(icon.starts_with("data:image/svg+xml;base64,"));
        assert_ne!(catalog.directory, impostor);
        assert_eq!(
            fs::read_to_string(catalog.directory.join("main.ts")).unwrap(),
            include_str!("../../plugins/build-in/magpie/main.ts")
        );
        assert_eq!(
            fs::read_to_string(impostor.join("main.ts")).unwrap(),
            "throw new Error('same version')"
        );
    }

    #[tokio::test]
    async fn bun_collects_embedded_magpie() {
        match std::process::Command::new("bun").arg("--version").output() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            result => assert!(result.unwrap().status.success()),
        }
        let root = tempfile::Builder::new()
            .prefix("magpie smoke ")
            .tempdir()
            .unwrap();
        let catalog = PluginCatalog::for_test(root.path()).unwrap();
        let entry = load_plugin(
            catalog.directory.clone(),
            catalog.loader(),
            Path::new("bun"),
        )
        .await
        .unwrap();
        assert_eq!(entry.definition.providers[0].id, "magpie");
        assert_eq!(entry.definition.resources[0].resource_type, "gateway");
    }

    #[test]
    fn accepts_form_add_methods_and_live_catalogs() {
        let definition = PluginModuleDefinition {
            providers: vec![ProviderDefinition {
                id: "gateway".into(),
                display_name: serde_json::json!("Gateway"),
                description: serde_json::Value::Null,
                provider_type: "openai".into(),
                resource_type: Some("gateway".into()),
                has_models: true,
                catalog: CatalogMode::Live,
            }],
            resources: vec![ResourceDefinition {
                resource_type: "gateway".into(),
                display_name: serde_json::json!("Gateways"),
                add: vec![AddMethodDefinition {
                    method_type: AddMethodKind::Form,
                    id: "connect".into(),
                    display_name: serde_json::json!("Connect"),
                    description: serde_json::Value::Null,
                    callback: None,
                    fields: vec![FormFieldDefinition {
                        id: "base-url".into(),
                        label: serde_json::json!("URL"),
                        field_type: FormFieldKind::Url,
                        required: true,
                        placeholder: serde_json::json!("http://127.0.0.1:3425"),
                    }],
                }],
                can_refresh: false,
                can_remove: false,
            }],
        };
        validate_definition("dev.example", &definition).unwrap();
    }
}
