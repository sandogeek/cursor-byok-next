//! Loads only the app's embedded Magpie plugin.

pub(in crate::plugins) mod cache;

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::plugins::{
    Error, Result, builtin,
    descriptor::{CatalogMode, PluginModuleDefinition, ValidatedProvider},
    manifest::{self, PluginManifest, validate_id},
    vm::{self, loader::Modules},
};

/// The running app's version: the same string `nsh.json` publishes, because the
/// crate version tracks the release.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct PluginCatalog {
    modules: Modules,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginEntry {
    pub entry: String,
    pub manifest: PluginManifest,
    /// The declaration as the plugin produced it; resources are read from here.
    pub definition: PluginModuleDefinition,
    /// The same providers, with their model source resolved. The host reads
    /// these instead of the raw `hasModels` / `catalog` / `resourceType` trio.
    pub providers: Vec<ValidatedProvider>,
    pub icon: String,
}

impl PluginCatalog {
    /// Opens the immutable, embedded module catalog; no runtime files are written.
    pub fn managed() -> Self {
        Self {
            modules: Modules::embedded(),
        }
    }
    #[cfg(test)]
    pub(super) fn for_test(_root: &std::path::Path) -> Self {
        Self::managed()
    }
    #[cfg(test)]
    pub(super) fn with_modules(modules: Modules) -> Self {
        Self { modules }
    }
    pub(crate) fn loader(&self) -> Modules {
        self.modules
    }
    pub(crate) async fn entries(&self) -> Result<Vec<PluginEntry>> {
        let LoadedManifest { manifest, icon } = load_manifest()?;
        let definition = vm::collect(self.modules, manifest.entry.clone()).await?;
        let providers = validate_definition(&manifest.id, &definition)?;
        Ok(vec![PluginEntry {
            entry: manifest.entry.clone(),
            manifest,
            definition,
            providers,
            icon,
        }])
    }
    pub(crate) fn manifests() -> Vec<LoadedManifest> {
        load_manifest().into_iter().collect()
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

fn load_manifest() -> Result<LoadedManifest> {
    let manifest: PluginManifest = serde_json::from_str(builtin::MANIFEST)?;
    manifest.validate()?;
    require_app_version(&manifest, APP_VERSION)?;
    if Modules::embedded().source(&manifest.entry).is_none() {
        return Err(Error::Config("unknown embedded entry".into()));
    }
    let icon = format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(builtin::ICON)
    );
    Ok(LoadedManifest { manifest, icon })
}

fn require_app_version(manifest: &PluginManifest, app_version: &str) -> Result<()> {
    let Some(minimum) = &manifest.min_app_version else {
        return Ok(());
    };
    if manifest::version_at_least(app_version, minimum) {
        return Ok(());
    }
    Err(Error::Config(format!(
        "plugin '{}' requires app version {minimum} or newer (current {app_version})",
        manifest.id
    )))
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
    fields: &[crate::plugins::descriptor::FormFieldDefinition],
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
        let impostor = root.path().join("plugins/installed/magpie");
        std::fs::create_dir_all(&impostor).unwrap();
        std::fs::write(impostor.join("main.ts"), "throw new Error('untrusted')").unwrap();
        let catalog = PluginCatalog::for_test(root.path());
        let manifests = PluginCatalog::manifests();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].manifest.id, builtin::MAGPIE_PLUGIN_ID);
        assert_eq!(
            manifests[0].manifest.resource_origin_policy(),
            crate::plugins::manifest::ResourceOriginPolicy::Allowed
        );
        assert_eq!(manifests[0].manifest.network_hosts(), Vec::<String>::new());
        assert!(manifests[0].icon.starts_with("data:image/svg+xml;base64,"));
        let entries = catalog.entries().await.unwrap();
        assert_eq!(entries[0].definition.providers[0].id, "magpie");
        assert_eq!(entries[0].definition.resources[0].resource_type, "gateway");
        assert_eq!(
            std::fs::read_to_string(impostor.join("main.ts")).unwrap(),
            "throw new Error('untrusted')"
        );
        assert_eq!(
            std::fs::read_dir(root.path()).unwrap().count(),
            1,
            "collector must not materialize a runtime workspace"
        );
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
    #[test]
    fn collector_works_in_a_process_with_no_runtime_on_path_or_home() {
        let home = tempfile::tempdir().unwrap();
        for test in [
            "plugins::catalog::tests::embedded_magpie_ignores_installed_manifests_and_source",
            "plugins::host::tests::magpie_catalog_through_quickjs",
        ] {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test])
                .env("PATH", "")
                .env("HOME", home.path())
                .status()
                .unwrap();
            assert!(status.success(), "no-runtime child failed: {test}");
        }
        assert!(!home.path().join(".bun").exists());
    }
}
