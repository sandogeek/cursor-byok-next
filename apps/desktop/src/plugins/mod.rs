//! Owns discovery and Bun workers for trusted built-in plugins.

mod builtin;
mod catalog;
mod data;
mod definition;
mod descriptor;
mod error;
mod host;
mod manifest;
mod origin;
mod protocol;
mod state;
mod worker;

pub use descriptor::{
    parse_model_id, AddMethodDefinition, FormFieldDefinition, PluginDescriptor,
    PluginModelDescriptor, PluginProviderDescriptor, PluginResourceDescriptor, PluginResourceView,
    ADAPTER_ID_PREFIX,
};
pub use error::{Error, Result};
pub use host::{FormSubmitResponse, PluginHost, PluginSnapshot};
pub use state::ResourceState;

#[cfg(windows)]
fn detach_console(command: &mut tokio::process::Command) {
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn detach_console(_command: &mut tokio::process::Command) {}
