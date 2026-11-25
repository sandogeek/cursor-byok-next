//! Compiled, embedded modules for the only trusted plugin, Magpie.
include!(concat!(env!("OUT_DIR"), "/plugin_modules.rs"));
pub(crate) const MAGPIE_PLUGIN_ID: &str = "dev.cursorbyoknext.plugins.magpie";
pub(super) const MANIFEST: &str = include_str!("../../plugins/build-in/magpie/plugin.json");
pub(super) const ICON: &[u8] = include_bytes!("../../plugins/build-in/magpie/assets/magpie.svg");
