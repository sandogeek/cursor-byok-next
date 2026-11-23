//! Owns discovery and Bun workers for trusted built-in plugins.

mod adapters;
mod builtin;
mod catalog;
mod data;
mod definition;
mod descriptor;
mod host;
mod manifest;
mod origin;
mod protocol;
mod state;
mod wire;
mod worker;

pub use crate::{Error, Result};
pub use descriptor::{
    ADAPTER_ID_PREFIX, AddMethodDefinition, FormFieldDefinition, ModelId, PluginDescriptor,
    PluginModelDescriptor, PluginProviderDescriptor, PluginResourceDescriptor, PluginResourceView,
    parse_model_id,
};
pub use host::{FormSubmitResponse, PluginHost, PluginSnapshot};
pub use state::ResourceState;

/// A user-facing message from the app's `locales/`: the plugin layer's errors
/// are written once, where the app shows them. Returns `key` itself when no
/// engine runs (a crate test), so `t!` never needs a UI to exist.
pub(crate) fn text(key: &str) -> String {
    if nsh_core::element::try_global::<nsh_core::i18n::Translations>().is_none() {
        return key.to_owned();
    }
    nsh_core::t!(key)
}

#[cfg(windows)]
fn detach_console(command: &mut tokio::process::Command) {
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn detach_console(_command: &mut tokio::process::Command) {}
