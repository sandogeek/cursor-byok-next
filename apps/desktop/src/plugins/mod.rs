//! Owns embedded modules and isolated VMs for trusted built-in plugins.

mod adapters;
mod builtin;
mod catalog;
mod descriptor;
mod host;
mod host_calls;
mod invocation;
mod manifest;
mod origin;
mod state;
mod vm;
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
