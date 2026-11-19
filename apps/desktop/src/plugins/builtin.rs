//! Materializes embedded Magpie in the private runtime workspace.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::Result;

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

pub(super) fn materialize_magpie(workspace: &Path) -> Result<PathBuf> {
    let directory = workspace.join("magpie");
    fs::create_dir(&directory)?;
    fs::create_dir(directory.join("assets"))?;
    for (relative, content) in MAGPIE {
        fs::write(directory.join(relative), content)?;
    }
    Ok(directory)
}
