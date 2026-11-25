//! Evaluates a descriptor in a fresh VM, never sharing a worker namespace.
use super::loader::Modules;
use crate::plugins::{Error, Result, descriptor::PluginModuleDefinition};
use std::sync::{Arc, atomic::AtomicBool, mpsc};
pub(in crate::plugins) async fn collect(
    modules: Modules,
    entry: String,
) -> Result<PluginModuleDefinition> {
    tokio::task::spawn_blocking(move || {
        let (_sender, receiver) = mpsc::channel();
        let (output, mut frames) = tokio::sync::mpsc::channel(1);
        super::runtime::run(
            modules,
            &entry,
            "sdk/collect.js",
            &receiver,
            &output,
            &Arc::new(AtomicBool::new(false)),
            None,
        )?;
        let frame = frames
            .try_recv()
            .map_err(|_| Error::Runtime("plugin descriptor unavailable".into()))?;
        serde_json::from_slice(&frame).map_err(Error::from)
    })
    .await
    .map_err(|_| Error::Runtime("plugin collector failed".into()))?
}
