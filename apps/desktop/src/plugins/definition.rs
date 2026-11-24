//! Evaluates TypeScript plugin definitions through a Bun subprocess.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tokio::io::AsyncReadExt;

use super::{Error, Result, descriptor::PluginModuleDefinition};

const DEFINITION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const OUTPUT_PREFIX: &str = "CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:";

#[derive(Clone, Debug)]
pub struct PluginDefinitionLoader {
    workspace: Arc<RuntimeDirectory>,
    /// `workspace/sdk`: the materialized SDK the Bun processes run from.
    sdk_dir: PathBuf,
}

impl PluginDefinitionLoader {
    fn sdk_file(&self, name: &str) -> PathBuf {
        self.sdk_dir.join(name)
    }
}

#[derive(Debug)]
struct RuntimeDirectory(PathBuf);

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl PluginDefinitionLoader {
    pub(super) fn in_temporary_workspace(parent: &Path) -> Result<Self> {
        let path = parent
            .canonicalize()?
            .join(format!("cursor-byok-next-plugin-{}", uuid::Uuid::new_v4()));
        // The workspace holds plugin code another user must not read or
        // replace, so it is created 0o700 where the platform has modes.
        #[cfg(unix)]
        let directory = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        #[cfg(not(unix))]
        let directory = std::fs::DirBuilder::new();
        // Create a new isolated copy; never reuse code under a discoverable install path.
        directory.create(&path)?;
        let workspace = Arc::new(RuntimeDirectory(path));
        let sdk_dir = workspace.0.join("sdk");
        std::fs::create_dir_all(&sdk_dir)?;
        let modules = [
            ("bunfig.toml", include_str!("sdk/bunfig.toml")),
            ("tsconfig.json", include_str!("sdk/tsconfig.json")),
            ("collect.ts", include_str!("sdk/collect.ts")),
            ("worker.ts", include_str!("sdk/worker.ts")),
            ("plugin.ts", include_str!("sdk/plugin.ts")),
            ("provider.ts", include_str!("sdk/provider.ts")),
            ("model.ts", include_str!("sdk/model.ts")),
            ("resource.ts", include_str!("sdk/resource.ts")),
        ];
        for (name, content) in modules {
            std::fs::write(sdk_dir.join(name), content)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sdk_dir, std::fs::Permissions::from_mode(0o700))?;
            for (name, _) in modules {
                std::fs::set_permissions(
                    sdk_dir.join(name),
                    std::fs::Permissions::from_mode(0o600),
                )?;
            }
        }
        Ok(Self { workspace, sdk_dir })
    }

    pub(super) fn workspace(&self) -> &Path {
        &self.workspace.0
    }

    /// The `worker.ts` a [`PluginWorker`](super::worker::PluginWorker) runs.
    pub fn worker_path(&self) -> PathBuf {
        self.sdk_file("worker.ts")
    }

    pub(super) fn bun_command(&self, executable: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(executable);
        super::detach_console(&mut command);
        let mut config = OsString::from("--config=");
        config.push(self.sdk_file("bunfig.toml"));
        let mut tsconfig = OsString::from("--tsconfig-override=");
        tsconfig.push(self.sdk_file("tsconfig.json"));
        command
            .args(["run", "--no-install", "--no-env-file"])
            .arg(config)
            .arg(tsconfig)
            .current_dir(&self.sdk_dir)
            .env("BUN_INSTALL_CACHE_DIR", self.sdk_dir.join("cache"))
            .kill_on_drop(true);
        command
    }

    /// Evaluates a plugin entry through Bun and returns its declared providers and
    /// resources.
    ///
    /// # Errors
    ///
    /// Fails when Bun cannot be spawned, the evaluation exceeds
    /// [`DEFINITION_TIMEOUT`], the plugin exits non-zero, or its output is not
    /// a valid descriptor.
    pub async fn load(&self, executable: &Path, entry: &Path) -> Result<PluginModuleDefinition> {
        let entry_url = file_url(entry)?;
        let mut child = self
            .bun_command(executable)
            .arg(self.sdk_file("collect.ts"))
            .arg(entry_url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Config("cannot capture plugin definition output".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Config("cannot capture plugin definition error output".into()))?;
        let (stdout, stderr, status) = tokio::time::timeout(DEFINITION_TIMEOUT, async move {
            let (stdout, stderr, status) = tokio::join!(
                read_limited(stdout, MAX_OUTPUT_BYTES),
                read_limited(stderr, MAX_OUTPUT_BYTES),
                child.wait()
            );
            Ok::<_, Error>((stdout?, stderr?, status?))
        })
        .await
        .map_err(|_| Error::Config("plugin definition evaluation timed out".into()))??;
        if !status.success() {
            return Err(Error::Config(format!(
                "plugin definition evaluation failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            )));
        }
        parse_definition_output(stdout)
    }
}

pub(super) fn file_url(path: &Path) -> Result<url::Url> {
    url::Url::from_file_path(path).map_err(|()| {
        Error::Config(format!(
            "plugin entry path is not a valid file URL: {}",
            path.display()
        ))
    })
}

/// Reads at most `limit` bytes, reporting an overrun instead of truncating.
async fn read_limited(reader: impl tokio::io::AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let cap = u64::try_from(limit).expect("the output cap fits in u64");
    let mut bytes = Vec::new();
    // Read one byte past the cap so an overrun is distinguishable from an
    // exactly-at-cap output.
    reader.take(cap + 1).read_to_end(&mut bytes).await?;
    if bytes.len() > limit {
        return Err(Error::Config(
            "plugin definition output is larger than allowed".into(),
        ));
    }
    Ok(bytes)
}

fn parse_definition_output(output: Vec<u8>) -> Result<PluginModuleDefinition> {
    let output = String::from_utf8(output)
        .map_err(|error| Error::context("plugin definition output is not UTF-8", error))?;
    let json = output
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(OUTPUT_PREFIX))
        .ok_or_else(|| Error::Config("plugin definition did not produce a descriptor".into()))?;
    Ok(serde_json::from_str(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_form_and_live_descriptor_fields() {
        let output = br#"CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:{"providers":[{"id":"gateway","displayName":"Gateway","description":null,"providerType":"openai","resourceType":"gateway","hasModels":true,"catalog":"live"}],"resources":[{"type":"gateway","displayName":"Gateways","add":[{"type":"form","id":"connect","displayName":"Connect","description":null,"fields":[{"id":"base-url","label":"URL","type":"url","required":true,"placeholder":null}]}],"canRefresh":false,"canRemove":false}]}"#;
        let descriptor = parse_definition_output(output.to_vec()).unwrap();
        assert_eq!(
            descriptor.providers[0].catalog,
            crate::plugins::descriptor::CatalogMode::Live
        );
        assert_eq!(
            descriptor.resources[0].add[0].method_type,
            crate::plugins::descriptor::AddMethodKind::Form
        );
        assert_eq!(descriptor.resources[0].add[0].fields[0].id, "base-url");
    }
}
