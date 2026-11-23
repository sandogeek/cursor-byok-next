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
const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024;
const OUTPUT_PREFIX: &str = "CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:";

#[derive(Clone)]
pub struct PluginDefinitionLoader {
    workspace: Arc<RuntimeDirectory>,
    sdk_dir: PathBuf,
    bunfig: PathBuf,
    tsconfig: PathBuf,
    collector: PathBuf,
    worker: PathBuf,
}

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
        let mut directory = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        // Create a new isolated copy; never reuse code under a discoverable install path.
        directory.create(&path)?;
        let workspace = Arc::new(RuntimeDirectory(path));
        let sdk_dir = workspace.0.join("sdk");
        std::fs::create_dir_all(&sdk_dir)?;
        let bunfig = sdk_dir.join("bunfig.toml");
        let tsconfig = sdk_dir.join("tsconfig.json");
        let collector = sdk_dir.join("collect.ts");
        let worker = sdk_dir.join("worker.ts");
        let modules = [
            (&bunfig, include_str!("sdk/bunfig.toml")),
            (&tsconfig, include_str!("sdk/tsconfig.json")),
            (&collector, include_str!("sdk/collect.ts")),
            (&worker, include_str!("sdk/worker.ts")),
            (&sdk_dir.join("plugin.ts"), include_str!("sdk/plugin.ts")),
            (
                &sdk_dir.join("provider.ts"),
                include_str!("sdk/provider.ts"),
            ),
            (&sdk_dir.join("model.ts"), include_str!("sdk/model.ts")),
            (
                &sdk_dir.join("resource.ts"),
                include_str!("sdk/resource.ts"),
            ),
        ];
        for (path, content) in &modules {
            std::fs::write(path, content)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sdk_dir, std::fs::Permissions::from_mode(0o700))?;
            for (path, _) in &modules {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
        }
        Ok(Self {
            workspace,
            sdk_dir,
            bunfig,
            tsconfig,
            collector,
            worker,
        })
    }

    pub(super) fn workspace(&self) -> &Path {
        &self.workspace.0
    }

    pub fn worker_path(&self) -> &Path {
        &self.worker
    }

    pub(super) fn bun_command(&self, executable: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(executable);
        super::detach_console(&mut command);
        let mut config = OsString::from("--config=");
        config.push(&self.bunfig);
        let mut tsconfig = OsString::from("--tsconfig-override=");
        tsconfig.push(&self.tsconfig);
        command
            .args(["run", "--no-install", "--no-env-file"])
            .arg(config)
            .arg(tsconfig)
            .current_dir(&self.sdk_dir)
            .env("BUN_INSTALL_CACHE_DIR", self.sdk_dir.join("cache"))
            .kill_on_drop(true);
        command
    }

    pub async fn load(&self, executable: &Path, entry: &Path) -> Result<PluginModuleDefinition> {
        let entry_url = file_url(entry)?;
        let mut child = self
            .bun_command(executable)
            .arg(&self.collector)
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
            let (stdout, stderr, status) =
                tokio::join!(read_limited(stdout), read_limited(stderr), child.wait());
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
        parse_definition_output(&stdout)
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

async fn read_limited(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(Error::Config(
            "plugin definition output is larger than allowed".into(),
        ));
    }
    Ok(bytes)
}

fn parse_definition_output(output: &[u8]) -> Result<PluginModuleDefinition> {
    let output = String::from_utf8(output.to_vec()).map_err(|error| {
        Error::Config(format!("plugin definition output is not UTF-8: {error}"))
    })?;
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
        let output = br#"CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:{"providers":[{"id":"gateway","displayName":"Gateway","description":null,"providerType":"openai","resourceType":"gateway","hasModels":true,"catalog":"live"}],"resources":[{"type":"gateway","displayName":"Gateways","add":[{"type":"form","id":"connect","displayName":"Connect","description":null,"fields":[{"id":"base-url","label":"URL","type":"url","required":true,"placeholder":null}]}],"import":null,"actions":[],"canRefresh":false,"canRemove":false}]}"#;
        let descriptor = parse_definition_output(output).unwrap();
        assert_eq!(descriptor.providers[0].catalog, "live");
        assert_eq!(descriptor.resources[0].add[0].method_type, "form");
        assert_eq!(descriptor.resources[0].add[0].fields[0].id, "base-url");
    }
}
